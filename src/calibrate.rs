//! `--calibrate`: measures `--settle` on the user's receiver with the scan
//! settings in use, and suggests a value.
//!
//! The "ghost" test: the strongest constant signal in the scanned band is found, then
//! the receiver alternately sits on the window with the signal (LO A) and jumps to a window one
//! sample rate away (LO B). Samples still coming from LO A after the jump show the signal in B at
//! the same offset from the LO. The smallest `settle` after which that offset in B stays at its
//! own level (measured with a long settle) is the result, with a margin.

use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use num_complex::Complex;

use crate::bench::{StreamCounters, consume};
use crate::detector::{Detector, DetectorConfig};
use crate::device::{IqBuffer, SdrSource};
use crate::plan::ScanPlan;
use crate::scanner::read_iq;

/// Default total time of the calibration (without `--duration`).
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(240);
/// A constant signal for the ghost test needs at least this SNR (dB) in `STRONG_SHARE` of its
/// survey measurements, and a jump counts only when the signal had it just before the jump. A
/// broadcast FM station seen in a narrow channel varies with its modulation, so not every
/// measurement is strong.
const MIN_SIGNAL_SNR: f32 = 20.0;
const STRONG_SHARE: usize = 80;
/// The search for the constant signal: at least this many cycles over the plan and this long.
const SEARCH_CYCLES: usize = 5;
const SEARCH_TIME: Duration = Duration::from_secs(3);
/// Settle in the survey (ms): long, but the survey goes through the whole plan several times.
const SURVEY_SETTLE_MS: f64 = 150.0;
/// Long settle for the reference measurements (ms): longer than any device stream delay seen
/// (an RTL-SDR with its default stream buffers needed about 100 ms).
const LONG_SETTLE_MS: f64 = 400.0;
/// Time on LO A before each jump (ms, or twice the settle tested): the samples of A must be
/// fresh, which on an RTL-SDR takes about 100 ms after a retune.
const STAY_MS: f64 = 250.0;
/// Settle values tried (ms), ascending.
const SETTLE_STEPS_MS: &[f64] = &[
    0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 25.0, 30.0, 35.0,
    40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 120.0, 140.0, 160.0, 180.0, 200.0, 250.0, 300.0,
];
/// Jumps per settle value with the signal strong before the jump; the worst one counts. At most
/// `GHOST_TRIES` jumps are made to collect them.
const GHOST_REPS: usize = 12;
const GHOST_TRIES: usize = 4 * GHOST_REPS;
/// A ghost: the measured SNR at the signal's offset in window B this much above its own level.
/// Above the maximum of noise over `GHOST_REPS` measurements (3 dB was crossed by noise alone);
/// a real ghost of a whole block stale is as strong as the signal.
const GHOST_DB: f32 = 6.0;
/// The test stops after a clean value followed by this many clean ones; a ghost after a clean
/// value starts the count again. A clean value at the end of the list is accepted too.
const CLEAN_IN_A_ROW: usize = 3;
/// Scan settings for the calibration.
pub struct Setup {
    pub fft_size: usize,
    pub dc_mask: usize,
    pub usable_hz: f64,
    pub rate: f64,
    pub full_scale: f64,
    /// Current `--dwell` (ms): the measurement block is as long as in the scanner.
    pub dwell_ms: f64,
    pub budget: Duration,
}

struct Ctx<'a, S: SdrSource> {
    src: &'a mut S,
    buf: IqBuffer,
    counters: StreamCounters,
    setup: &'a Setup,
    start: Instant,
}

impl<S: SdrSource> Ctx<'_, S> {
    fn samples(&self, ms: f64) -> usize {
        (ms * 1e-3 * self.setup.rate).round() as usize
    }

    /// Tunes, discards `settle` samples and reads one block.
    fn block(&mut self, lo: u64, settle: usize, iq: &mut [Complex<f32>]) -> anyhow::Result<()> {
        self.src.tune(lo as f64)?;
        consume(self.src, &mut self.buf, settle, &mut self.counters)?;
        read_iq(
            self.src,
            &mut self.buf,
            self.setup.full_scale,
            iq,
            &mut self.counters,
        )
    }
}

/// Ends the calibration after Ctrl+C.
fn stopped() -> anyhow::Result<()> {
    anyhow::ensure!(
        !crate::STOP.load(Ordering::Relaxed),
        "calibration stopped (Ctrl+C), nothing saved"
    );
    Ok(())
}

/// Runs the measurement; progress goes to `out`. Returns the suggested `settle` (ms), `None`
/// when it cannot be measured (no strong constant signal in the band, ghosts even with the
/// longest value tried, out of time).
pub fn run<S: SdrSource>(
    src: &mut S,
    plan: &ScanPlan,
    setup: &Setup,
    out: &mut impl Write,
) -> anyhow::Result<Option<f64>> {
    let (format, _) = src.stream_format();
    let buf = IqBuffer::new(format, src.buffer_len());
    src.start()?;
    let mut ctx = Ctx {
        src,
        buf,
        counters: StreamCounters::default(),
        setup,
        start: Instant::now(),
    };

    writeln!(
        out,
        "\nlooking for a strong constant signal in the scanned band"
    )?;
    let strongest = strongest(&mut ctx, plan)?;
    writeln!(out, "\nghost test")?;
    let settle_ms = settle(&mut ctx, plan, strongest, out)?;
    if ctx.counters.overflows > 0 {
        writeln!(
            out,
            "WARNING: {} stream overflows during the calibration (samples lost)",
            ctx.counters.overflows
        )?;
    }
    Ok(settle_ms)
}

fn detector(plan: &ScanPlan, setup: &Setup, frames: usize) -> anyhow::Result<Detector> {
    let cfg = DetectorConfig {
        fft_size: setup.fft_size,
        frames,
        dc_mask: setup.dc_mask,
        sample_rate: setup.rate,
        usable_hz: setup.usable_hz,
    };
    Detector::new(plan, &cfg).map_err(anyhow::Error::msg)
}

/// Frames per window for `dwell_ms` (as in the scanner).
fn frames_for(setup: &Setup, dwell_ms: f64) -> usize {
    ((dwell_ms * 1e-3 * setup.rate / setup.fft_size as f64).round() as usize).max(1)
}

/// The strongest constant signal: (window index, channel frequency, channel bw, the SNR reached
/// in `STRONG_SHARE` percent of its measurements). Several cycles over the plan with a long
/// settle and the current dwell.
type Signal = (usize, u64, f64, f32);

fn strongest<S: SdrSource>(ctx: &mut Ctx<S>, plan: &ScanPlan) -> anyhow::Result<Option<Signal>> {
    let setup = ctx.setup;
    let mut det = detector(plan, setup, frames_for(setup, setup.dwell_ms))?;
    let mut iq = vec![Complex::new(0.0f32, 0.0); det.samples_needed()];
    let windows: Vec<_> = plan.passes.iter().flatten().copied().collect();
    let settle = ctx.samples(SURVEY_SETTLE_MS);
    // SNR of each measurement of each plan channel, and a window that measures it.
    let mut snrs: Vec<Vec<f32>> = vec![Vec::new(); plan.channels.len()];
    let mut window_of = vec![0; plan.channels.len()];
    let t0 = Instant::now();
    let mut cycles = 0;
    while cycles < SEARCH_CYCLES || t0.elapsed() < SEARCH_TIME {
        for (pos, w) in windows.iter().enumerate() {
            stopped()?;
            ctx.block(w.center, settle, &mut iq)?;
            let r = det.measure(pos, &iq);
            for (j, &index) in r.indices.iter().enumerate() {
                let snr = r.powers_db[j] - r.floors_db[j];
                snrs[index as usize].push(snr);
                window_of[index as usize] = pos;
            }
        }
        cycles += 1;
    }
    for v in &mut snrs {
        v.sort_by(f32::total_cmp);
    }
    // The SNR exceeded in STRONG_SHARE percent of the measurements (sorted ascending).
    let reached = |v: &Vec<f32>| v.get(v.len() * (100 - STRONG_SHARE) / 100).copied();
    Ok(snrs
        .iter()
        .enumerate()
        .filter_map(|(i, v)| reached(v).map(|snr| (i, snr)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, snr)| {
            (
                window_of[i],
                plan.channels[i].freq,
                plan.channels[i].bw,
                snr,
            )
        }))
}

fn settle<S: SdrSource>(
    ctx: &mut Ctx<S>,
    plan: &ScanPlan,
    found: Option<Signal>,
    out: &mut impl Write,
) -> anyhow::Result<Option<f64>> {
    let setup = ctx.setup;
    let frames = frames_for(setup, setup.dwell_ms);
    let mut det = detector(plan, setup, frames)?;
    let mut iq = vec![Complex::new(0.0f32, 0.0); det.samples_needed()];
    let Some((pos, freq, bw, snr)) = found.filter(|f| f.3 >= MIN_SIGNAL_SNR) else {
        let strongest = found.map_or(String::new(), |(_, f, _, snr)| {
            format!(" (the strongest: {:.6} MHz, {snr:.1} dB)", f as f64 / 1e6)
        });
        writeln!(
            out,
            "  no constant signal with SNR >= {MIN_SIGNAL_SNR} dB in the scanned band{strongest}: --settle cannot be measured. Give a range with a strong constant signal: a beacon or a repeater with a carrier, or, with the antenna disconnected, a carrier of the receiver itself (e.g. seen in SDR#)"
        )?;
        return Ok(None);
    };
    let lo_a = plan
        .passes
        .iter()
        .flatten()
        .nth(pos)
        .map_or(0, |w| w.center);
    let offset = freq as f64 - lo_a as f64;
    writeln!(
        out,
        "  signal: {:.6} MHz, SNR >= {snr:.1} dB in {STRONG_SHARE}% of the measurements (window LO {:.6} MHz)",
        freq as f64 / 1e6,
        lo_a as f64 / 1e6
    )?;

    // Window B one or two sample rates away, where the signal's offset is quiet.
    let long = ctx.samples(LONG_SETTLE_MS);
    let mut chosen = None;
    for k in [1.0, -1.0, 2.0, -2.0] {
        let lo_b = (lo_a as f64 + k * setup.rate).round();
        if lo_b <= 0.0 || ctx.src.set_frequency(lo_b).is_err() {
            continue;
        }
        let lo_b = lo_b as u64;
        let mut levels = Vec::with_capacity(GHOST_REPS);
        for _ in 0..GHOST_REPS {
            stopped()?;
            ctx.block(lo_b, long, &mut iq)?;
            let (p, floor) = det.measure_channel(&iq, offset, bw);
            levels.push(p - floor);
        }
        levels.sort_by(f32::total_cmp);
        let reference = levels[levels.len() / 2];
        // The quiet level must be well below the signal, otherwise a ghost would not show.
        if reference < snr - GHOST_DB - 10.0 {
            chosen = Some((lo_b, reference));
            break;
        }
    }
    ctx.src.set_frequency(lo_a as f64)?;
    let Some((lo_b, reference)) = chosen else {
        writeln!(
            out,
            "  no quiet window one or two sample rates away: --settle cannot be measured"
        )?;
        return Ok(None);
    };
    writeln!(
        out,
        "  jumps {:.6} -> {:.6} MHz, level at the signal's offset {reference:.1} dB",
        lo_a as f64 / 1e6,
        lo_b as f64 / 1e6
    )?;

    let mut clean = 0;
    let mut first_clean = None;
    for &ms in SETTLE_STEPS_MS {
        let settle = ctx.samples(ms);
        let stay = ctx.samples((2.0 * ms).max(STAY_MS));
        let mut ghost = f32::NEG_INFINITY;
        let mut valid = 0;
        for _ in 0..GHOST_TRIES {
            stopped()?;
            if ctx.start.elapsed() > setup.budget {
                writeln!(out, "  out of time (--duration) during the settle test")?;
                return Ok(None);
            }
            ctx.block(lo_a, stay, &mut iq)?;
            let (p, floor) = det.measure_channel(&iq, offset, bw);
            let strong = p - floor >= MIN_SIGNAL_SNR;
            ctx.block(lo_b, settle, &mut iq)?;
            if !strong {
                continue;
            }
            let (p, floor) = det.measure_channel(&iq, offset, bw);
            ghost = ghost.max(p - floor - reference);
            valid += 1;
            if valid == GHOST_REPS {
                break;
            }
        }
        if valid < GHOST_REPS {
            writeln!(
                out,
                "  settle {ms:5.1} ms: the signal was too often weak ({valid} of {GHOST_REPS} jumps): --settle cannot be measured"
            )?;
            return Ok(None);
        }
        let leak = ghost > GHOST_DB;
        writeln!(
            out,
            "  settle {ms:5.1} ms: {}",
            if leak {
                format!("ghost +{ghost:.1} dB")
            } else {
                "clean".to_string()
            }
        )?;
        if leak {
            clean = 0;
            first_clean = None;
        } else {
            first_clean.get_or_insert(ms);
            clean += 1;
            if clean > CLEAN_IN_A_ROW {
                break;
            }
        }
    }
    // Clean values in a row, or clean up to the end of the list.
    match first_clean {
        Some(ms) => Ok(Some(with_margin(ms))),
        _ => {
            writeln!(
                out,
                "  ghosts up to the longest value tried: --settle cannot be measured"
            )?;
            Ok(None)
        }
    }
}

/// The clean settle plus a margin: +25%, at least +2 ms, rounded up to whole ms.
fn with_margin(ms: f64) -> f64 {
    (ms * 1.25).max(ms + 2.0).ceil()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_rounds_up() {
        assert_eq!(with_margin(0.0), 2.0);
        assert_eq!(with_margin(12.0), 15.0);
        assert_eq!(with_margin(14.0), 18.0);
        assert_eq!(with_margin(5.0), 7.0);
    }
}
