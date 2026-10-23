//! Retuning benchmark: passes through the range windows and step time measurement.
//!
//! The stream is activated once. In each window: retune, discard `settle` samples, collect
//! `dwell` samples (without processing). The loop does not allocate memory.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::device::{IqBuffer, ReadOutcome, SdrError, SdrSource};
use crate::plan::Window;

/// Timeout of a single stream read.
const READ_TIMEOUT_US: i64 = 100_000;
/// This many consecutive timeouts without any samples are treated as a stream failure.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 20;
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub struct BenchConfig {
    pub settle_samples: usize,
    pub dwell_samples: usize,
    pub duration: Duration,
    /// Actual sample rate (Hz).
    pub sample_rate: f64,
    /// Bandwidth of the measured channels of each window (Hz), in `windows` order.
    pub window_hz: Vec<f64>,
}

/// Time statistics (seconds).
#[derive(Clone, Copy)]
struct Stat {
    sum: f64,
    min: f64,
    max: f64,
    n: u64,
}

impl Stat {
    const fn new() -> Self {
        Stat {
            sum: 0.0,
            min: f64::INFINITY,
            max: 0.0,
            n: 0,
        }
    }

    fn add(&mut self, d: Duration) {
        let s = d.as_secs_f64();
        self.sum += s;
        self.min = self.min.min(s);
        self.max = self.max.max(s);
        self.n += 1;
    }

    fn avg_ms(&self) -> f64 {
        1e3 * self.sum / self.n.max(1) as f64
    }

    fn min_ms(&self) -> f64 {
        if self.n == 0 { 0.0 } else { 1e3 * self.min }
    }

    fn max_ms(&self) -> f64 {
        1e3 * self.max
    }
}

#[derive(Default)]
pub struct StreamCounters {
    pub overflows: u64,
    pub timeouts: u64,
}

/// Reads and discards `count` samples.
pub fn consume(
    src: &mut impl SdrSource,
    buf: &mut IqBuffer,
    count: usize,
    counters: &mut StreamCounters,
) -> Result<(), SdrError> {
    let mut remaining = count;
    let mut consecutive_timeouts = 0;
    while remaining > 0 {
        match src.read(buf, remaining, READ_TIMEOUT_US)? {
            ReadOutcome::Samples(n @ 1..) => {
                remaining = remaining.saturating_sub(n);
                consecutive_timeouts = 0;
            }
            ReadOutcome::Overflow => counters.overflows += 1,
            // An empty read counts as a timeout, so the loop does not spin without progress.
            ReadOutcome::Timeout | ReadOutcome::Samples(0) => {
                counters.timeouts += 1;
                consecutive_timeouts += 1;
                if consecutive_timeouts >= MAX_CONSECUTIVE_TIMEOUTS {
                    return Err(SdrError::NoSamples {
                        timeouts: consecutive_timeouts,
                    });
                }
            }
        }
    }
    Ok(())
}

pub fn run(
    src: &mut impl SdrSource,
    windows: &[Window],
    cfg: &BenchConfig,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    anyhow::ensure!(!windows.is_empty(), "no windows to tune");
    let (format, _) = src.stream_format();
    let mut buf = IqBuffer::new(format, src.buffer_len());
    // Bandwidth of the measured channels in one full window cycle.
    let cycle_hz: f64 = cfg.window_hz.iter().sum();

    src.start()?;

    let mut tune = Stat::new();
    let mut read = Stat::new();
    let mut step = Stat::new();
    let mut counters = StreamCounters::default();
    let mut steps: u64 = 0;
    let mut period_steps: u64 = 0;
    let mut covered_hz = 0.0;
    let mut period_hz = 0.0;
    let mut period_tune = Stat::new();
    let mut period_step = Stat::new();

    let start = Instant::now();
    let mut period_start = start;
    let mut i = 0;
    loop {
        let w = windows[i];
        let w_hz = cfg.window_hz[i];
        i = (i + 1) % windows.len();

        let t0 = Instant::now();
        src.tune(w.center as f64)?;
        let t1 = Instant::now();
        consume(src, &mut buf, cfg.settle_samples, &mut counters)?;
        consume(src, &mut buf, cfg.dwell_samples, &mut counters)?;
        let t2 = Instant::now();

        tune.add(t1 - t0);
        read.add(t2 - t1);
        step.add(t2 - t0);
        period_tune.add(t1 - t0);
        period_step.add(t2 - t0);
        steps += 1;
        period_steps += 1;
        covered_hz += w_hz;
        period_hz += w_hz;

        let period = t2 - period_start;
        if period >= REPORT_INTERVAL {
            let secs = period.as_secs_f64();
            let wps = period_steps as f64 / secs;
            writeln!(
                out,
                "{:6.1} s  windows/s {:7.1}  {:7.1} MHz/s  step avg {:6.3} ms (max {:6.3})  retune avg {:6.3} ms  overflows {}  timeouts {}",
                (t2 - start).as_secs_f64(),
                wps,
                period_hz / 1e6 / secs,
                period_step.avg_ms(),
                period_step.max_ms(),
                period_tune.avg_ms(),
                counters.overflows,
                counters.timeouts,
            )?;
            period_start = t2;
            period_steps = 0;
            period_hz = 0.0;
            period_tune = Stat::new();
            period_step = Stat::new();
        }

        if t2 - start >= cfg.duration && i == 0 {
            break;
        }
    }

    let total = start.elapsed().as_secs_f64();
    let wps = steps as f64 / total;
    let samples_ms = 1e3 * (cfg.settle_samples + cfg.dwell_samples) as f64 / cfg.sample_rate;
    writeln!(
        out,
        "\nResult ({total:.1} s, {steps} steps, {} full window cycles):",
        steps / windows.len() as u64
    )?;
    writeln!(
        out,
        "  retune (setFrequency): avg {:.3} ms, min {:.3} ms, max {:.3} ms",
        tune.avg_ms(),
        tune.min_ms(),
        tune.max_ms()
    )?;
    writeln!(
        out,
        "  settle+dwell read: avg {:.3} ms, max {:.3} ms (duration of these samples: {samples_ms:.3} ms)",
        read.avg_ms(),
        read.max_ms()
    )?;
    writeln!(
        out,
        "  step (retune + read): avg {:.3} ms, min {:.3} ms, max {:.3} ms",
        step.avg_ms(),
        step.min_ms(),
        step.max_ms()
    )?;
    writeln!(out, "  windows/s: {wps:.1}")?;
    writeln!(
        out,
        "  effective speed: {:.1} MHz/s (measured channels: {:.4} MHz per window cycle)",
        covered_hz / 1e6 / total,
        cycle_hz / 1e6
    )?;
    writeln!(
        out,
        "  overflows: {}, timeouts: {}",
        counters.overflows, counters.timeouts
    )?;
    if read.avg_ms() < samples_ms * 0.9 {
        writeln!(
            out,
            "  WARNING: the read is shorter than the duration of the samples, so some samples come from buffers\n  filled before the retune. This measurement does not check whether samples after --settle are already from the new frequency."
        )?;
    }
    Ok(())
}
