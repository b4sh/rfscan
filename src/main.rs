mod activity;
mod airband;
mod audio;
mod audio_dsp;
mod bench;
mod calibrate;
mod channel_db;
mod cli;
mod config;
mod demod;
mod detector;
mod device;
mod digi_tcp;
mod digi_voice;
mod dsd_fme;
mod events;
mod exclusions;
mod help;
mod log;
mod paths;
mod plan;
mod record;
mod recorder;
mod scanner;
mod ui;
mod web;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use num_complex::Complex;

use cli::Cli;
use device::soapy::{SoapySource, list_devices};
use device::{IqBuffer, ReadOutcome, SdrSource};
use log::{Level, Log};

/// Timeout of a single stream read.
const READ_TIMEOUT_US: i64 = 100_000;
/// Set by Ctrl+C: loops end normally and the activity log is saved.
static STOP: AtomicBool = AtomicBool::new(false);
/// IQ buffer capacity between the scanner and the DSP thread (seconds of samples).
const IQ_BUFFER_S: f64 = 0.25;
/// Measurement time for hit confirmation (`--confirm`): longer than a TDMA frame (DMR 60 ms), so
/// a bursty signal is not rejected.
const CONFIRM_S: f64 = 0.06;
/// Capacity of the scanner -> TUI event queue (events beyond it are dropped).
const UI_EVENT_QUEUE: usize = 1024;
/// Tolerance for matching a channel file name in the activity report.
const REPORT_NAME_TOLERANCE_HZ: u64 = 4_000;
/// How often to print the power.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Sums for the mean power and the DC component over one report period.
#[derive(Default)]
struct PowerAcc {
    power: f64,
    i: f64,
    q: f64,
    n: u64,
}

impl PowerAcc {
    fn add(&mut self, i: f32, q: f32) {
        let (i, q) = (i as f64, q as f64);
        self.power += i * i + q * q;
        self.i += i;
        self.q += q;
        self.n += 1;
    }

    /// Mean power in dBFS (full-scale complex signal = 0 dBFS).
    fn power_dbfs(&self) -> f64 {
        db(self.power / self.n.max(1) as f64)
    }

    /// DC component power in dBFS.
    fn dc_dbfs(&self) -> f64 {
        let n = self.n.max(1) as f64;
        let (i, q) = (self.i / n, self.q / n);
        db(i * i + q * q)
    }
}

fn db(p: f64) -> f64 {
    10.0 * p.max(1e-30).log10()
}

/// Normalizes `n` samples from the buffer to ±1 according to the full scale and adds them to the
/// sums.
fn accumulate(buf: &IqBuffer, n: usize, full_scale: f64, acc: &mut PowerAcc) {
    let k = (1.0 / full_scale) as f32;
    match buf {
        // Unsigned format: zero lies in the middle of the range (128 for 8 bits).
        // To be checked on a real signal (DC component in the report).
        IqBuffer::Cu8(b) => {
            for s in &b[..n] {
                acc.add((s.re as f32 - 128.0) * k, (s.im as f32 - 128.0) * k);
            }
        }
        IqBuffer::Cs8(b) => {
            for s in &b[..n] {
                acc.add(s.re as f32 * k, s.im as f32 * k);
            }
        }
        IqBuffer::Cs16(b) => {
            for s in &b[..n] {
                acc.add(s.re as f32 * k, s.im as f32 * k);
            }
        }
        IqBuffer::Cf32(b) => {
            for &Complex { re, im } in &b[..n] {
                acc.add(re * k, im * k);
            }
        }
    }
}

/// Default benchmark time when --duration is not given.
const BENCH_DEFAULT_DURATION: Duration = Duration::from_secs(10);
/// Typical maximum NFM channel width; above it only a warning.
const BW_WARN_HZ: f64 = 25_000.0;
/// Default AM channel width and grid step: 8.33 kHz, i.e. exactly 25/3 kHz (the airband
/// channel grid used in Europe, user decision).
const AM_DEFAULT_BW_HZ: f64 = 25_000.0 / 3.0;
const AM_DEFAULT_STEP_HZ: u64 = 8_333;
/// Default NFM grid step.
const NFM_DEFAULT_STEP_HZ: u64 = 12_500;
/// Range scanned when no mode is chosen (step and bw from --step/--bw, default 12.5 kHz).
const DEFAULT_RANGE: (u64, u64) = (144_000_000, 146_000_000);

/// Default width of a listed channel: 12.5 kHz for NFM and digi, 8.33 kHz for AM (user
/// decision).
fn default_channel_bw(mode: plan::Modulation) -> f64 {
    match mode {
        plan::Modulation::Nfm | plan::Modulation::Digi => 12_500.0,
        plan::Modulation::Am => AM_DEFAULT_BW_HZ,
    }
}

/// Default range channel width: NFM = step, AM 8.33 kHz, digi 12.5 kHz.
fn default_range_bw(mode: plan::Modulation, step_hz: f64) -> f64 {
    match mode {
        plan::Modulation::Nfm => step_hz,
        plan::Modulation::Am => AM_DEFAULT_BW_HZ,
        plan::Modulation::Digi => 12_500.0,
    }
}

/// Channel file, always loaded. Without a configuration directory and without
/// `--channels-file` the database is empty, unless the file is the only source of channels
/// (`required`).
fn load_channel_db(
    log: &mut Log,
    cli: &Cli,
    required: bool,
) -> anyhow::Result<Vec<channel_db::ChannelEntry>> {
    let path = match cli.channels_file.clone().or_else(channel_db::default_path) {
        Some(p) => p,
        None if required => {
            bail!(
                "cannot determine the user's home directory (for ~/.config/rfscan), use --channels-file"
            )
        }
        None => return Ok(Vec::new()),
    };
    let (entries, warnings) = channel_db::load(&path).map_err(anyhow::Error::msg)?;
    for w in &warnings {
        logwarn!(log, "WARNING: {}: {w}", path.display());
    }
    logln!(
        log,
        Level::Debug,
        "channel file: {} ({} entries)",
        path.display(),
        entries.len()
    );
    Ok(entries)
}

/// `--digi` or `--digi-tcp`: digi channels are scanned.
fn digi_on(cli: &Cli) -> bool {
    cli.digi || cli.digi_tcp.is_some()
}

/// An explicit `digi` mode on the command line without `--digi` or `--digi-tcp` is a
/// configuration error.
fn check_explicit_digi(cli: &Cli) -> anyhow::Result<()> {
    if digi_on(cli) {
        return Ok(());
    }
    let digi = Some(plan::Modulation::Digi);
    let in_ranges = cli.scan_range.iter().any(|r| r.mode == digi);
    let in_channels = cli
        .scan_channels
        .as_ref()
        .and_then(|l| l.as_ref())
        .is_some_and(|l| l.0.iter().any(|c| c.mode == digi));
    if cli.mode == digi || in_ranges || in_channels {
        bail!(
            "digi mode requires DSD-FME: add --digi (DSD-FME started by rfscan), or --digi-tcp 7355 and then run dsd-fme -i tcp:127.0.0.1:7355"
        );
    }
    Ok(())
}

/// Names, modes and `bw=` from the channel file for range channels: matching within
/// half a step, the file mode takes precedence over the range suffix, the file `bw=` changes
/// only the demodulator filter and only when the range has no `bw=` of its own. The detector
/// keeps the range `bw`. Returns warnings for entries off the range grid: the scanner listens on
/// the grid channel, not on the entry's frequency.
fn apply_channel_db(
    plan: &mut plan::ScanPlan,
    entries: &[channel_db::ChannelEntry],
    cli_bw: Option<u64>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for ch in &mut plan.channels {
        let Some(e) = channel_db::nearest(entries, ch.freq, ch.spacing / 2.0) else {
            continue;
        };
        // Above OFF_GRID of the step: not a rounding difference (e.g. 8.33 kHz channels).
        let off = e.freq as f64 - ch.freq as f64;
        if off.abs() > OFF_GRID * ch.spacing {
            warnings.push(format!(
                "WARNING: channel file: {:.6} MHz{} is {:+.1} kHz off the range grid, the scanner listens on the nearest channel {:.6} MHz (change the range start or step to include it)",
                e.freq as f64 / 1e6,
                e.name
                    .as_deref()
                    .map_or(String::new(), |n| format!(" \"{n}\"")),
                off / 1e3,
                ch.freq as f64 / 1e6,
            ));
        }
        ch.name = e.name.clone();
        ch.raster25 = e.raster25;
        let range_mode = ch.mode;
        if let Some(m) = e.mode {
            ch.mode = m;
        }
        if !ch.bw_explicit {
            if let Some(b) = e.bw {
                ch.demod_bw = b as f64;
            } else if ch.mode != range_mode && cli_bw.is_none() {
                ch.demod_bw = default_range_bw(ch.mode, ch.spacing);
            }
        }
    }
    warnings
}

/// A channel file entry farther than this fraction of the step from the range grid channel gets
/// a warning (`apply_channel_db`).
const OFF_GRID: f64 = 0.01;

/// Checks the channel bw: an error when wider than the window; a warning above 25 kHz.
fn check_bw(log: &mut Log, what: &str, bw: f64, width: u64) -> anyhow::Result<()> {
    if bw > width as f64 {
        bail!(
            "{what}: bw {} kHz is wider than the usable window band {} kHz",
            bw / 1e3,
            width as f64 / 1e3
        );
    }
    if bw > BW_WARN_HZ {
        logwarn!(
            log,
            "WARNING: {what}: bw {} kHz above 25 kHz is unusual for NFM (WFM is not supported)",
            bw / 1e3
        );
    }
    Ok(())
}

/// Channels of the --scan-channels mode: from the command line list or from the channel file.
fn scan_channels(
    log: &mut Log,
    cli: &Cli,
    list: Option<&cli::ChannelList>,
) -> anyhow::Result<Vec<plan::ChannelSpec>> {
    let default_mode = cli.mode.unwrap_or(plan::Modulation::Nfm);
    let entries = load_channel_db(log, cli, list.is_none())?;
    let mut specs = Vec::new();
    if let Some(list) = list {
        // Precedence: explicit channel fields, then the file entry, then --mode/--bw, then the
        // defaults.
        for c in &list.0 {
            let guess =
                c.bw.or(cli.bw)
                    .map_or(default_channel_bw(c.mode.unwrap_or(default_mode)), |b| {
                        b as f64
                    });
            let e = channel_db::nearest(&entries, c.freq, guess / 2.0);
            let mode = c.mode.or(e.and_then(|e| e.mode)).unwrap_or(default_mode);
            specs.push(plan::ChannelSpec {
                freq: c.freq,
                bw: c
                    .bw
                    .or(e.and_then(|e| e.bw))
                    .or(cli.bw)
                    .map_or(default_channel_bw(mode), |b| b as f64),
                mode,
                name: e.and_then(|e| e.name.clone()),
                raster25: c.raster25 || e.is_some_and(|e| e.raster25),
            });
        }
    } else {
        for e in entries {
            let mode = e.mode.unwrap_or(default_mode);
            specs.push(plan::ChannelSpec {
                freq: e.freq,
                bw: e
                    .bw
                    .or(cli.bw)
                    .map_or(default_channel_bw(mode), |b| b as f64),
                mode,
                raster25: e.raster25,
                name: e.name,
            });
        }
    }
    // Without --digi or --digi-tcp digi channels (from the file only: explicit digi is an
    // error) are skipped.
    if !digi_on(cli) {
        let before = specs.len();
        specs.retain(|s| s.mode != plan::Modulation::Digi);
        let skipped = before - specs.len();
        if skipped > 0 {
            logln!(
                log,
                Level::Debug,
                "skipped {skipped} digi channel(s) from the channel file (no --digi or --digi-tcp)"
            );
        }
    }
    if specs.is_empty() {
        bail!(
            "no channels to scan (the channel file is empty or has only digi channels without --digi or --digi-tcp; give a list: --scan-channels 145.5M,...)"
        );
    }
    Ok(specs)
}

/// Builds the plan from the chosen mode (ranges, channels or the default range), prints it and
/// checks every LO with the device.
fn prepare_plan(
    cli: &Cli,
    src: &mut SoapySource,
    rate: f64,
    log: &mut Log,
) -> anyhow::Result<plan::ScanPlan> {
    let width = (cli.usable * rate).floor() as u64;
    check_explicit_digi(cli)?;
    let default_mode = cli.mode.unwrap_or(plan::Modulation::Nfm);
    let plan = if let Some(list) = &cli.scan_channels {
        let specs = scan_channels(log, cli, list.as_ref())?;
        for c in &specs {
            check_bw(
                log,
                &format!("channel {:.6} MHz", c.freq as f64 / 1e6),
                c.bw,
                width,
            )?;
        }
        logln!(log, Level::Debug, "mode: channels");
        plan::channels_plan(&specs, width, cli.dc_guard).map_err(anyhow::Error::msg)?
    } else {
        let args: Vec<cli::RangeArg> = if cli.scan_range.is_empty() {
            logln!(
                log,
                Level::Debug,
                "mode: ranges (default range, neither --scan-range nor --scan-channels given)"
            );
            vec![cli::RangeArg {
                start: DEFAULT_RANGE.0,
                stop: DEFAULT_RANGE.1,
                step: None,
                mode: None,
                bw: None,
            }]
        } else {
            logln!(log, Level::Debug, "mode: ranges");
            cli.scan_range.clone()
        };
        let mut ranges = Vec::with_capacity(args.len());
        for a in &args {
            let mode = a.mode.unwrap_or(default_mode);
            // Step: from the range, then --step, then the modulation default.
            let step = plan::Step::from_hz(a.step.or(cli.step).unwrap_or(match mode {
                plan::Modulation::Nfm | plan::Modulation::Digi => NFM_DEFAULT_STEP_HZ,
                plan::Modulation::Am => AM_DEFAULT_STEP_HZ,
            }));
            let bw =
                a.bw.or(cli.bw)
                    .map_or(default_range_bw(mode, step.hz()), |b| b as f64);
            let what = format!("range {}-{} MHz", a.start as f64 / 1e6, a.stop as f64 / 1e6);
            check_bw(log, &what, bw, width)?;
            if bw < step.hz() - 0.5 {
                logwarn!(
                    log,
                    "WARNING: {what}: bw {} kHz is smaller than the step {:.4} kHz: signals between channels will not be measured",
                    bw / 1e3,
                    step.hz() / 1e3
                );
            }
            ranges.push(plan::RangeSpec {
                start: a.start,
                stop: a.stop,
                step,
                bw,
                bw_explicit: a.bw.is_some(),
                mode,
            });
        }
        for (i, a) in ranges.iter().enumerate() {
            for b in &ranges[i + 1..] {
                if a.start <= b.stop && b.start <= a.stop {
                    logwarn!(
                        log,
                        "WARNING: ranges {}-{} and {}-{} MHz overlap: shared frequencies will be scanned twice",
                        a.start as f64 / 1e6,
                        a.stop as f64 / 1e6,
                        b.start as f64 / 1e6,
                        b.stop as f64 / 1e6
                    );
                }
            }
        }
        for r in &ranges {
            logln!(
                log,
                Level::Debug,
                "  range {}-{} MHz, step {:.4} kHz, bw {:.4} kHz, {}",
                r.start as f64 / 1e6,
                r.stop as f64 / 1e6,
                r.step.hz() / 1e3,
                r.bw / 1e3,
                r.mode.name()
            );
        }
        let mut plan =
            plan::ranges_plan(&ranges, width, cli.dc_guard).map_err(anyhow::Error::msg)?;
        let entries = load_channel_db(log, cli, false)?;
        for w in apply_channel_db(&mut plan, &entries, cli.bw) {
            logwarn!(log, "{w}");
        }
        plan
    };

    logln!(
        log,
        Level::Debug,
        "channels: {}, sample rate: {} MSps, usable {:.0}% = {} MHz, DC zone: {} channel(s) on each side of the LO, passes: {}",
        plan.total_channels(),
        rate / 1e6,
        cli.usable * 100.0,
        width as f64 / 1e6,
        cli.dc_guard,
        plan.passes.len()
    );
    logln!(log, Level::Debug, "receiver: {}", src.config_summary());
    // Check every center frequency with the device before the measurement loop.
    // The console gets the number of windows per pass, the window list goes to the file only.
    logln!(
        log,
        Level::Debug,
        "{}",
        plan.passes
            .iter()
            .enumerate()
            .map(|(p, pass)| format!(
                "pass {}: {} window{}",
                if p == 0 { 'A' } else { 'B' },
                pass.len(),
                if pass.len() == 1 { "" } else { "s" }
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (p, pass) in plan.passes.iter().enumerate() {
        let name = if p == 0 { 'A' } else { 'B' };
        logfile!(log, Level::Debug, "pass {name}: {} windows", pass.len());
        for (i, w) in pass.iter().enumerate() {
            let actual = src.set_frequency(w.center as f64)?;
            let note = if (actual - w.center as f64).abs() >= 1.0 {
                format!(" (tuner set {:.6} MHz)", actual / 1e6)
            } else {
                String::new()
            };
            let guard = match w.guard {
                Some((g0, g1)) => format!(
                    ", DC zone {:.6}-{:.6} MHz",
                    plan.channel(g0) as f64 / 1e6,
                    plan.channel(g1) as f64 / 1e6
                ),
                None => String::new(),
            };
            logfile!(
                log,
                Level::Debug,
                "  window {i:2}: LO {:.6} MHz, channels {:.6}-{:.6} MHz ({}, measured {}){guard}{note}",
                w.center as f64 / 1e6,
                w.first_channel as f64 / 1e6,
                w.last_channel as f64 / 1e6,
                w.channels,
                w.measured
            );
        }
    }
    Ok(plan)
}

fn run_bench(cli: &Cli, src: &mut SoapySource, rate: f64, log: &mut Log) -> anyhow::Result<()> {
    let plan = prepare_plan(cli, src, rate, log)?;
    // Passes alternate: A, B, A, B...
    let windows: Vec<plan::Window> = plan.passes.concat();

    let cfg = bench::BenchConfig {
        settle_samples: (cli.settle * 1e-3 * rate).round() as usize,
        dwell_samples: ((cli.dwell * 1e-3 * rate).round() as usize).max(1),
        duration: cli
            .duration
            .map_or(BENCH_DEFAULT_DURATION, Duration::from_secs_f64),
        sample_rate: rate,
        window_hz: windows.iter().map(|w| plan.measured_hz(w)).collect(),
    };
    logln!(
        log,
        Level::Debug,
        "settle {} ms = {} samples, dwell {} ms = {} samples, read buffer {} samples, time {:.1} s",
        cli.settle,
        cfg.settle_samples,
        cli.dwell,
        cfg.dwell_samples,
        src.buffer_len(),
        cfg.duration.as_secs_f64()
    );
    let stdout = std::io::stdout();
    bench::run(src, &windows, &cfg, &mut stdout.lock())
}

/// `--fft-size`, or a bin not wider than 1/4 of the narrowest of: the range step and the channel
/// bw.
fn fft_size(cli: &Cli, plan: &plan::ScanPlan, rate: f64) -> usize {
    let resolution = plan
        .channels
        .iter()
        .map(|c| c.bw.min(c.spacing))
        .fold(f64::INFINITY, f64::min);
    match cli.fft_size {
        Some(n) => n,
        None => ((rate / (resolution / 4.0)).ceil() as usize).next_power_of_two(),
    }
}

/// A calibrated settle above this (ms) gives a hint about the stream buffers.
const LONG_SETTLE_HINT_MS: f64 = 30.0;

/// Stream read timeout of the recording.
const RECORD_READ_TIMEOUT_US: i64 = 100_000;
/// How often the recording prints its progress.
const RECORD_STATUS_INTERVAL: Duration = Duration::from_secs(10);

/// `--record-iq DIR`: records the raw IQ stream of the one channel of
/// `--scan-channels` to WAV files, tuned as for scanning it, until Ctrl+C or `--duration`.
fn run_record(
    cli: &Cli,
    src: &mut SoapySource,
    rate: f64,
    dir: &std::path::Path,
    log: &mut Log,
) -> anyhow::Result<()> {
    let plan = prepare_plan(cli, src, rate, log)?;
    let (Some(window), Some(channel)) = (
        plan.passes.first().and_then(|p| p.first()),
        plan.channels.first(),
    ) else {
        bail!("--record-iq: the plan contains no channel");
    };
    let lo = src.set_frequency(window.center as f64)?.round() as u64;
    let (format, _) = src.stream_format();
    logln!(
        log,
        Level::Info,
        "recording: channel {} MHz, LO {} MHz (the file's center frequency), {} MSps {}, {:.1} MB/s, to {}",
        activity::format_mhz(channel.freq),
        activity::format_mhz(lo),
        rate / 1e6,
        format.name(),
        rate * record::bytes_per_sample(format) as f64 / 1e6,
        dir.display()
    );
    let mut rec = record::Recorder::new(dir, lo, rate, format);
    let mut buf = IqBuffer::new(format, src.buffer_len());
    let len = src.buffer_len();
    let deadline = cli.duration.map(Duration::from_secs_f64);
    let mut overflows: u64 = 0;
    src.start()?;
    let start = Instant::now();
    let mut status_at = start + RECORD_STATUS_INTERVAL;
    let result = loop {
        let now = Instant::now();
        if STOP.load(Ordering::Relaxed) || deadline.is_some_and(|d| now - start >= d) {
            break Ok(());
        }
        match src.read(&mut buf, len, RECORD_READ_TIMEOUT_US)? {
            ReadOutcome::Samples(n) => match rec.write(&buf, n) {
                Ok(Some(path)) => logev!(log, Level::Info, now, "recording to {}", path.display()),
                Ok(None) => {}
                Err(e) => break Err(anyhow::anyhow!("--record-iq: {e}")),
            },
            ReadOutcome::Overflow => overflows += 1,
            ReadOutcome::Timeout => {}
        }
        if now >= status_at {
            logev!(
                log,
                Level::Debug,
                now,
                "recording {:.0} s  {:.1} MB  overflows {overflows}",
                (now - start).as_secs_f64(),
                rec.total_bytes as f64 / 1e6
            );
            status_at = now + RECORD_STATUS_INTERVAL;
        }
    };
    // The sizes and the stop time go into the last file also after an error.
    let finished = rec.finish();
    logln!(
        log,
        Level::Info,
        "recording finished: {:.1} s, {:.1} MB in {} file(s)",
        start.elapsed().as_secs_f64(),
        rec.total_bytes as f64 / 1e6,
        rec.files.len()
    );
    if overflows > 0 {
        logwarn!(
            log,
            "WARNING: {overflows} stream overflow(s): samples were lost and the recording has gaps (lower --sample-rate or use a faster disk)"
        );
    }
    result?;
    finished.map_err(anyhow::Error::msg)
}

/// `--calibrate`: measures --settle, then asks whether to save it to the configuration
/// file.
fn run_calibrate(cli: &Cli, src: &mut SoapySource, rate: f64, log: &mut Log) -> anyhow::Result<()> {
    let plan = prepare_plan(cli, src, rate, log)?;
    let setup = calibrate::Setup {
        fft_size: fft_size(cli, &plan, rate),
        dc_mask: cli.dc_mask,
        usable_hz: (cli.usable * rate).floor(),
        rate,
        full_scale: src.stream_format().1,
        dwell_ms: cli.dwell,
        budget: cli
            .duration
            .map_or(calibrate::DEFAULT_BUDGET, Duration::from_secs_f64),
    };
    println!(
        "\ncalibration of --settle: up to about {:.0} s with the antenna connected; Ctrl+C stops it without saving",
        setup.budget.as_secs_f64()
    );
    let stdout = std::io::stdout();
    let settle = calibrate::run(src, &plan, &setup, &mut stdout.lock())?;

    println!();
    let Some(ms) = settle else {
        println!(
            "settle: not measured (now {} ms), nothing to save",
            cli.settle
        );
        return Ok(());
    };
    println!("settle: {ms} ms (now {} ms)", cli.settle);
    if ms > LONG_SETTLE_HINT_MS && !src.stream_arg_keys().is_empty() {
        println!(
            "hint: a long settle often comes from large stream buffers of the driver; smaller ones may shorten it. Stream arguments of this driver (--stream-args, details in --list-devices): {}. See the README for settings found for some receivers, then run --calibrate again.",
            src.stream_arg_keys().join(", ")
        );
    }
    let values = [("settle", ms.to_string())];
    let path = match &cli.config {
        Some(p) => p.clone(),
        None => config::default_path().context("cannot determine the user's home directory")?,
    };
    print!("Save to {}? [y/n] ", path.display());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if STOP.load(Ordering::Relaxed) || !answer.trim().eq_ignore_ascii_case("y") {
        println!("nothing saved");
        return Ok(());
    }
    config::set_values(&path, cli.config.is_none(), &values).map_err(anyhow::Error::msg)?;
    println!("saved to {}", path.display());
    Ok(())
}

fn run_scan(cli: &Cli, src: &mut SoapySource, rate: f64, log: &mut Log) -> anyhow::Result<()> {
    // DSD-FME is found before anything else starts: without it --digi ends here.
    let dsd_exe = if cli.digi {
        Some(dsd_fme::locate(cli.dsd_fme.as_deref()).map_err(anyhow::Error::msg)?)
    } else {
        None
    };
    // --dsd-fme-logs is not created: a mistyped directory would scatter the logs.
    if let Some(dir) = &cli.dsd_fme_logs
        && !dir.is_dir()
    {
        bail!(
            "--dsd-fme-logs: the directory {} does not exist",
            dir.display()
        );
    }
    let plan = prepare_plan(cli, src, rate, log)?;
    let fft_size = fft_size(cli, &plan, rate);
    let dwell_samples = cli.dwell * 1e-3 * rate;
    let frames = ((dwell_samples / fft_size as f64).round() as usize).max(1);
    let det_cfg = detector::DetectorConfig {
        fft_size,
        frames,
        dc_mask: cli.dc_mask,
        sample_rate: rate,
        usable_hz: (cli.usable * rate).floor(),
    };
    let mut det = detector::Detector::new(&plan, &det_cfg).map_err(anyhow::Error::msg)?;

    let settle_samples = (cli.settle * 1e-3 * rate).round() as usize;
    logln!(
        log,
        Level::Debug,
        "FFT {fft_size} (bin {:.1} Hz), frames per window {frames} ({} samples = {:.3} ms), DC mask ±{} bin(s), settle {} ms = {settle_samples} samples",
        rate / fft_size as f64,
        det.samples_needed(),
        det.samples_needed() as f64 / rate * 1e3,
        cli.dc_mask,
        cli.settle
    );
    logln!(
        log,
        Level::Debug,
        "threshold: SNR >= {} dB{}",
        cli.squelch_snr,
        cli.squelch_abs
            .map_or(String::new(), |a| format!(", power >= {a} dBFS"))
    );

    // Exclusions: the file (persistent) and --exclude (temporary), the channel mask built once.
    let exclude_path = match &cli.exclude_file {
        Some(p) => p.clone(),
        None => exclusions::default_path().context(
            "cannot determine the user's home directory (for ~/.config/rfscan), use --exclude-file",
        )?,
    };
    let (mut excl, warnings) =
        exclusions::Exclusions::load(&exclude_path).map_err(anyhow::Error::msg)?;
    for w in &warnings {
        logwarn!(log, "WARNING: {}: {w}", exclude_path.display());
    }
    if let Some(list) = &cli.exclude {
        excl.add_cli(list).map_err(anyhow::Error::msg)?;
    }
    // Without --digi or --digi-tcp digi channels from the channel file are masked like exclusions
    //, temporarily and without writing to the exclusion file.
    let digi_masked: Vec<bool> = plan
        .channels
        .iter()
        .map(|ch| !digi_on(cli) && ch.mode == plan::Modulation::Digi)
        .collect();
    let skipped = digi_masked.iter().filter(|&&m| m).count();
    if skipped > 0 {
        logln!(
            log,
            Level::Debug,
            "skipped {skipped} digi channel(s) from the channel file (no --digi or --digi-tcp)"
        );
    }
    let (from_file, from_cli) = (excl.entries.len(), excl.temp.len());
    let mut exclusions = scanner::ExclusionCtl::new(
        excl,
        exclude_path.clone(),
        digi_masked,
        cli.exclude_wide,
        &plan,
    );
    logln!(
        log,
        Level::Debug,
        "exclusions: {} ({from_file} entries from the file, {from_cli} from --exclude), masked channels: {}",
        exclude_path.display(),
        exclusions.masked_channels()
    );
    logln!(
        log,
        Level::Debug,
        "hysteresis {} dB (leaving Locked at SNR < {} dB), pause {} s",
        cli.hysteresis,
        cli.squelch_snr - cli.hysteresis,
        cli.pause
    );

    let cfg = scanner::ScanConfig {
        settle_samples,
        squelch_snr: cli.squelch_snr,
        squelch_abs: cli.squelch_abs,
        hysteresis: cli.hysteresis,
        pause: Duration::from_secs_f64(cli.pause),
        duration: cli.duration.map(Duration::from_secs_f64),
        confirm: cli.confirm,
        confirm_blocks: ((CONFIRM_S * rate) / det.samples_needed() as f64)
            .ceil()
            .max(1.0) as usize,
        usable_hz: cli.usable * rate,
    };
    // Activity log.
    let (activity, activity_path) = open_activity(cli, log)?;

    // Audio output and the DSP thread (demodulation in the Locked state). With --local-audio off
    // nothing is played locally; the web GUI gets the mix either way.
    if !cli.local_audio && cli.audio_device.is_some() {
        logwarn!(
            log,
            "WARNING: --audio-device is ignored with --local-audio off"
        );
    }
    let audio::Opened {
        out,
        audio: audio_tx,
        voice: voice_tx,
        web: web_audio,
        rec: rec_tap,
    } = audio::AudioOut::open(
        cli.audio_device.as_deref(),
        cli.local_audio,
        cli.web_gui.is_some(),
        cli.record_dir.is_some(),
    )
    .map_err(anyhow::Error::msg)?;
    // Audio recording of the channels marked `rec` (--record-dir). The list is shared with the
    // interfaces, which switch it.
    let channels_path = cli.channels_file.clone().or_else(channel_db::default_path);
    let rec_list: recorder::RecList = Arc::new(Mutex::new(
        channels_path
            .as_deref()
            .and_then(|p| channel_db::load(p).ok())
            .map(|(entries, _)| entries.iter().filter(|e| e.rec).map(|e| e.freq).collect())
            .unwrap_or_default(),
    ));
    let rec = match (&cli.record_dir, rec_tap) {
        (Some(dir), Some(tap)) => {
            let r = recorder::start(
                recorder::Config {
                    dir: dir.clone(),
                    rate: out.rate,
                    channels_path: channels_path.clone(),
                    disk_limit: recorder::DISK_LIMIT,
                },
                tap,
                rec_list.clone(),
            )
            .map_err(anyhow::Error::msg)?;
            logln!(
                log,
                Level::Debug,
                "recording: {} ({} channel(s) marked rec)",
                dir.display(),
                rec_list.lock().map_or(0, |l| l.len())
            );
            Some(r)
        }
        _ => None,
    };
    // Digi output: listening from startup, an error (e.g. a busy port) ends the program
    //. With --digi on a port chosen by the system, for the DSD-FME started below.
    let digi_addr = match (cli.digi, cli.digi_tcp) {
        (true, _) => Some(std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
        (false, addr) => addr,
    };
    let (digi, digi_input) = match digi_addr {
        Some(addr) => {
            let (server, input) = digi_tcp::start(addr).map_err(anyhow::Error::msg)?;
            if !addr.ip().is_loopback() {
                logwarn!(
                    log,
                    "WARNING: --digi-tcp {addr}: a non-loopback address, the digi stream is reachable from the network"
                );
            }
            if cli.digi {
                logln!(
                    log,
                    Level::Debug,
                    "digi output: listening on {} (PCM s16le mono 48 kHz, gain {} dB) for DSD-FME",
                    server.addr,
                    cli.digi_gain
                );
            } else {
                logln!(
                    log,
                    Level::Debug,
                    "digi output: listening on {} (PCM s16le mono 48 kHz, gain {} dB); now run: dsd-fme -i tcp:{}",
                    server.addr,
                    cli.digi_gain,
                    server.addr
                );
            }
            (Some(server), Some(input))
        }
        None => (None, None),
    };
    // Decoded voice from DSD-FME over UDP.
    let voice_addr = if cli.digi {
        Some(cli::VoiceAddr {
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            rate: None,
        })
    } else {
        cli.digi_audio_udp
    };
    let voice = match voice_addr {
        Some(v) => {
            let r = digi_voice::start(v.addr, v.rate, out.rate, voice_tx)
                .map_err(anyhow::Error::msg)?;
            logln!(
                log,
                Level::Debug,
                "digi voice: receiving on UDP {} (s16le mono, sample rate {})",
                r.addr,
                v.rate
                    .map_or("estimated from the stream".to_string(), |r| format!(
                        "{r} Hz"
                    ))
            );
            Some(r)
        }
        None => None,
    };
    // DSD-FME started and supervised by rfscan (--digi).
    let dsd = match (dsd_exe, &digi, &voice) {
        (Some(exe), Some(server), Some(v)) => {
            let dir = match &cli.dsd_fme_logs {
                Some(dir) => dir.clone(),
                None => {
                    let dir = paths::config_dir().context(
                        "cannot determine the user's home directory (for ~/.config/rfscan)",
                    )?;
                    std::fs::create_dir_all(&dir)
                        .with_context(|| format!("cannot create {}", dir.display()))?;
                    dir
                }
            };
            let dsd_cfg = dsd_fme::DsdConfig {
                exe,
                args: cli
                    .dsd_fme_args
                    .split_whitespace()
                    .map(String::from)
                    .collect(),
                tcp: server.addr,
                udp: v.addr,
                log_path: dir.join("dsd-fme.log"),
                events_path: dir.join("dsd-fme-events.log"),
            };
            logln!(
                log,
                Level::Info,
                "DSD-FME: {} {} (console output: {})",
                dsd_cfg.exe.display(),
                dsd_cfg.command_args().join(" "),
                dsd_cfg.log_path.display()
            );
            Some(dsd_fme::start(dsd_cfg, server.event_sender.clone()).map_err(anyhow::Error::msg)?)
        }
        _ => None,
    };
    // Channel chains for every width a channel can get, also after its mode is changed from the
    // TUI (`[l]` with `[Tab] digi`): the plan widths, the widths without the channel file and
    // the digi width.
    let mut bandwidths: Vec<f64> = plan
        .channels
        .iter()
        .flat_map(|c| [c.demod_bw, c.base.1])
        .chain([plan::DIGI_BW_HZ])
        .collect();
    bandwidths.sort_by(f64::total_cmp);
    bandwidths.dedup();
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (report_tx, report_rx) = crossbeam_channel::bounded(64);
    let iq_capacity = ((IQ_BUFFER_S * rate) as usize).max(4 * det.samples_needed());
    let (iq_tx, iq_rx) = rtrb::RingBuffer::new(iq_capacity);
    let nr = Arc::new(std::sync::atomic::AtomicBool::new(cli.nr));
    let nr_strength = Arc::new(std::sync::atomic::AtomicU32::new(cli.nr_strength));
    let audio_cfg = audio_dsp::AudioDspConfig {
        band: cli.audio_band.0,
        agc: cli.agc,
        agc_target_db: cli.agc_target,
        agc_target_am_db: cli.agc_target_am,
        agc_max_gain_db: cli.agc_max_gain,
    };
    let worker = demod::spawn_worker(
        demod::WorkerConfig {
            sample_rate: rate,
            bandwidths,
            deemph_us: cli.deemph.0,
            audio: audio_cfg,
            output_rate: out.rate,
            output_channels: out.channels,
            // Tail: the time to confirm the signal loss plus a margin for the measurement block
            // length.
            tail_max_s: scanner::EXIT_HOLD.as_secs_f64() + 4.0 * det.samples_needed() as f64 / rate,
            digi_gain: 10f32.powf(cli.digi_gain / 20.0),
            nr: nr.clone(),
            nr_strength: nr_strength.clone(),
        },
        ctrl_rx,
        iq_rx,
        audio_tx,
        out.active.clone(),
        report_tx,
        digi_input,
    )
    .map_err(anyhow::Error::msg)?;
    logln!(
        log,
        Level::Debug,
        "audio: {} ({} Hz, {} channels), de-emphasis {}, noise reduction {}, filter {}, AGC {}",
        out.device,
        out.rate,
        out.channels,
        cli.deemph
            .0
            .map_or("off".to_string(), |us| format!("{us} µs")),
        if cli.nr {
            format!("rnnoise {}%", cli.nr_strength)
        } else {
            "off".to_string()
        },
        cli.audio_band
            .0
            .map_or("off".to_string(), |(lo, hi)| format!("{lo}-{hi} Hz")),
        if cli.agc {
            format!(
                "target NFM {} dBFS, AM {} dBFS, max {} dB",
                cli.agc_target, cli.agc_target_am, cli.agc_max_gain
            )
        } else {
            "off".to_string()
        }
    );

    // TUI unless --headless or the output is not a terminal; the web GUI
    // with --web-gui. Both read the same context and send the same commands.
    let tui = !cli.headless && std::io::stdout().is_terminal();
    let ctx = (tui || cli.web_gui.is_some()).then(|| {
        let db_entries = cli
            .channels_file
            .clone()
            .or_else(channel_db::default_path)
            .and_then(|p| channel_db::load(&p).ok())
            .map(|(entries, _)| entries)
            .unwrap_or_default();
        ui::UiContext {
            device: short_device_name(src.description()),
            sample_rate: rate,
            gain: cli
                .gain
                .as_ref()
                .map_or("driver defaults".to_string(), |k| {
                    k.0.iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(",")
                }),
            ranges: range_labels(cli, &plan),
            window_range: plan
                .passes
                .iter()
                .flatten()
                .map(|w| plan.channels[w.first_index as usize].range)
                .collect(),
            window_span: plan
                .passes
                .iter()
                .flatten()
                .map(|w| (w.first_channel, w.last_channel))
                .collect(),
            window_names: plan
                .passes
                .iter()
                .enumerate()
                .flat_map(|(p, pass)| {
                    (0..pass.len()).map(move |i| format!("{}{i}", if p == 0 { 'A' } else { 'B' }))
                })
                .collect(),
            names: plan.channels.iter().map(|c| c.name.clone()).collect(),
            channel_db: db_entries
                .iter()
                .filter_map(|e| e.name.clone().map(|n| (e.freq, n)))
                .collect(),
            digi_db: db_entries
                .iter()
                .filter(|e| e.mode == Some(plan::Modulation::Digi))
                .map(|e| e.freq)
                .collect(),
            // Airband channels on the 25 kHz raster (channel file and command line).
            raster25: plan
                .channels
                .iter()
                .filter(|c| c.raster25)
                .map(|c| c.freq)
                .chain(db_entries.iter().filter(|e| e.raster25).map(|e| e.freq))
                .collect(),
            channels_path: cli.channels_file.clone().or_else(channel_db::default_path),
            channel_freqs: plan.channels.iter().map(|c| c.freq).collect(),
            activity: activity.clone(),
            activity_path: activity_path.clone(),
            exclusions: exclusions.shared.clone(),
            exclusions_path: exclude_path.clone(),
            muted: out.muted.clone(),
            nr: nr.clone(),
            nr_strength: nr_strength.clone(),
            underruns: out.underruns.clone(),
            digi: digi.as_ref().map(|d| (d.addr, d.stats.clone())),
            dsd: dsd.as_ref().map(|d| d.state.clone()),
            dsd_console: dsd.as_ref().map(|d| d.console.clone()),
            voice: voice.as_ref().map(|v| v.stats.clone()),
            rec_list: rec_list.clone(),
            recorder: rec.as_ref().map(|r| r.shared.clone()),
        }
    });
    let mut ui_link = None;
    let mut ui_thread = None;
    let mut web_server = None;
    if let Some(ctx) = ctx {
        let (ev_tx, ev_rx) = crossbeam_channel::bounded(UI_EVENT_QUEUE);
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        // With both interfaces the scanner's events are copied to each of them by a thread of
        // its own, so the scanner loop stays unchanged.
        let (tui_rx, web_rx) = match (tui, cli.web_gui.is_some()) {
            (true, true) => {
                let (tui_tx, tui_rx) = crossbeam_channel::bounded(UI_EVENT_QUEUE);
                let (web_tx, web_rx) = crossbeam_channel::bounded(UI_EVENT_QUEUE);
                std::thread::Builder::new()
                    .name("rfscan-ui-events".into())
                    .spawn(move || {
                        for e in ev_rx {
                            let _ = tui_tx.try_send(e);
                            let _ = web_tx.try_send(e);
                        }
                    })
                    .context("cannot start the UI event thread")?;
                (Some(tui_rx), Some(web_rx))
            }
            (true, false) => (Some(ev_rx), None),
            _ => (None, Some(ev_rx)),
        };
        if let (Some(addr), Some(web_rx)) = (cli.web_gui, web_rx) {
            let server = web::start(
                web::WebConfig {
                    addr,
                    token: cli.web_token.clone(),
                    local_audio: cli.local_audio,
                    audio_rate: out.rate,
                    exclude_wide: cli.exclude_wide,
                },
                ctx.clone(),
                web_rx,
                cmd_tx.clone(),
                web_audio,
            )
            .map_err(anyhow::Error::msg)?;
            logln!(log, Level::Info, "web GUI: {}", server.url);
            if server.token_generated {
                logln!(
                    log,
                    Level::Info,
                    "web GUI: a random token for this run (set a fixed one with --web-token)"
                );
            }
            if !addr.ip().is_loopback() {
                logwarn!(
                    log,
                    "WARNING: --web-gui {addr}: reachable from the network; plain HTTP without advanced security, for a home network or a VPN only"
                );
            }
            web_server = Some(server);
        }
        if let Some(tui_rx) = tui_rx {
            let handle = std::thread::Builder::new()
                .name("rfscan-ui".into())
                .spawn(move || ui::run(ctx, tui_rx, cmd_tx))
                .context("cannot start the UI thread")?;
            // The TUI owns the terminal: the log goes only to the file from now on.
            log.set_console(false);
            ui_thread = Some(handle);
        }
        ui_link = Some(scanner::UiLink {
            events: ev_tx,
            commands: cmd_rx,
        });
    }

    let mut link = scanner::AudioLink::new(ctrl_tx, iq_tx, report_rx, out.underruns.clone());
    let result = scanner::run(
        src,
        &plan,
        &mut det,
        &cfg,
        scanner::Session {
            audio: Some(&mut link),
            activity: &activity,
            activity_path: &activity_path,
            stop: &STOP,
            digi: digi.as_ref(),
            exclusions: &mut exclusions,
            ui: ui_link,
            recorder: rec.as_ref().map(|r| &r.link),
        },
        log,
    );
    // The scanner dropped its UI link: the TUI sees the closed channel, restores the terminal
    // and ends.
    if let Some(h) = ui_thread {
        let ui_result = h.join();
        log.set_console(true);
        match ui_result {
            Ok(Err(e)) => logwarn!(log, "WARNING: TUI error: {e:#}"),
            Err(_) => logwarn!(log, "WARNING: the TUI thread panicked"),
            Ok(Ok(())) => {}
        }
    }
    // The web GUI also ends when the scanner closes its event channel.
    if let Some(w) = web_server {
        w.join();
    }
    // Closing the message channel ends the DSP thread.
    drop(link);
    let _ = worker.join();
    // The open recording is closed; its last messages go to the log.
    if let Some(r) = rec {
        for e in r.finish() {
            match e {
                recorder::Event::Info(m) => logln!(log, Level::Info, "{m}"),
                recorder::Event::Warning(m) => logwarn!(log, "WARNING: {m}"),
            }
        }
    }
    // DSD-FME first, while its TCP input is still open.
    drop(dsd);
    drop(voice);
    drop(digi);
    drop(out);
    result
}

/// Short device name for the TUI header, e.g. `airspy 2064c8703d20018`.
fn short_device_name(description: &str) -> String {
    let field = |key: &str| {
        description
            .split(',')
            .map(str::trim)
            .find_map(|kv| kv.strip_prefix(key).map(str::to_string))
    };
    match (field("driver="), field("serial=")) {
        (Some(d), Some(s)) => format!("{d} {s}"),
        (Some(d), None) => d,
        _ => description.to_string(),
    }
}

/// Labels of the scanned ranges (or the channel list) for the TUI progress view.
fn range_labels(cli: &Cli, plan: &plan::ScanPlan) -> Vec<ui::RangeLabel> {
    if cli.scan_channels.is_some() {
        let freqs = plan.channels.iter().map(|c| c.freq);
        return vec![ui::RangeLabel {
            span: format!("{} channels", plan.channels.len()),
            start: freqs.clone().min().unwrap_or(0),
            stop: freqs.max().unwrap_or(0),
        }];
    }
    let count = plan.channels.iter().map(|c| c.range + 1).max().unwrap_or(0);
    (0..count)
        .map(|r| {
            let mut chans = plan.channels.iter().filter(|c| c.range == r);
            let first = chans.next();
            let last = chans.next_back().or(first);
            match (first, last) {
                (Some(a), Some(b)) => ui::RangeLabel {
                    span: format!("{:.3}-{:.3} MHz", a.freq as f64 / 1e6, b.freq as f64 / 1e6),
                    start: a.freq,
                    stop: b.freq,
                },
                _ => ui::RangeLabel {
                    span: String::new(),
                    start: 0,
                    stop: 0,
                },
            }
        })
        .collect()
}

/// Activity log path from --activity-file or from the configuration directory.
fn activity_path(cli: &Cli) -> anyhow::Result<PathBuf> {
    match &cli.activity_file {
        Some(p) => Ok(p.clone()),
        None => activity::default_path().context(
            "cannot determine the user's home directory (for ~/.config/rfscan), use --activity-file",
        ),
    }
}

/// Loads the activity log (after --activity-reset starts from zero) and sets up saving on
/// panic.
fn open_activity(
    cli: &Cli,
    log: &mut Log,
) -> anyhow::Result<(Arc<Mutex<activity::ActivityLog>>, PathBuf)> {
    let path = activity_path(cli)?;
    if cli.activity_reset
        && let Some(bak) = activity::reset(&path).map_err(anyhow::Error::msg)?
    {
        logln!(
            log,
            Level::Debug,
            "activity log archived: {}",
            bak.display()
        );
    }
    let (activity_log, warnings) =
        activity::ActivityLog::load(&path).map_err(anyhow::Error::msg)?;
    for w in &warnings {
        logwarn!(log, "WARNING: {}: {w}", path.display());
    }
    logln!(
        log,
        Level::Debug,
        "activity log: {} ({} frequencies)",
        path.display(),
        activity_log.entries.len()
    );
    let activity_log = Arc::new(Mutex::new(activity_log));
    // Saving on panic (the hook runs also with panic = "abort", before aborting).
    let (hook_log, hook_path) = (activity_log.clone(), path.clone());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut l) = hook_log.try_lock() {
            let _ = l.save(&hook_path);
        }
        previous(info);
    }));
    Ok((activity_log, path))
}

/// --report: a table from the activity log, sorted by descending time.
fn report(cli: &Cli) -> anyhow::Result<()> {
    let path = activity_path(cli)?;
    let (log, warnings) = activity::ActivityLog::load(&path).map_err(anyhow::Error::msg)?;
    for w in &warnings {
        eprintln!("WARNING: {}: {w}", path.display());
    }
    // Names from the channel file (if any).
    let names = match cli.channels_file.clone().or_else(channel_db::default_path) {
        Some(p) => channel_db::load(&p).map(|(e, _)| e).unwrap_or_default(),
        None => Vec::new(),
    };
    let nearest = |hz: u64| {
        names
            .iter()
            .filter(|e| e.freq.abs_diff(hz) <= REPORT_NAME_TOLERANCE_HZ)
            .min_by_key(|e| e.freq.abs_diff(hz))
    };
    let name_of = |hz: u64| nearest(hz).and_then(|e| e.name.clone()).unwrap_or_default();
    // As on the activity screens: DIGI for a digi channel file entry, otherwise the mode of the
    // last activity (older log lines have none).
    let mode_of = |hz: u64, e: &activity::Entry| {
        if nearest(hz).is_some_and(|n| n.mode == Some(plan::Modulation::Digi)) {
            plan::Modulation::Digi.name()
        } else {
            e.mode.map_or("-", |m| m.name())
        }
    };
    let mut rows: Vec<_> = log.entries.iter().collect();
    rows.sort_by(|a, b| b.1.seconds.total_cmp(&a.1.seconds));
    println!(
        "activity log: {} ({} frequencies)",
        path.display(),
        rows.len()
    );
    println!(
        "{:>12}  {:<4}  {:<30}  {:>10}  {:>6}  last",
        "MHz", "mode", "name", "time [s]", "hits"
    );
    for (&hz, e) in rows {
        println!(
            "{:>12}  {:<4}  {:<30}  {:>10.1}  {:>6}  {}",
            // Airband: the channel number (25 kHz raster when the channel file says so).
            airband::display(
                hz,
                names
                    .iter()
                    .any(|n| n.raster25 && n.freq.abs_diff(hz) <= 100)
            ),
            mode_of(hz, e),
            name_of(hz),
            e.seconds,
            e.hits,
            e.last.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// Module directory of SoapySDR relative to its root (`SoapySDR::getRootPath`).
const SOAPY_MODULES: &str = "lib/SoapySDR/modules0.8";

/// The Windows package has `SoapySDR.dll` next to `rfscan.exe` and the driver modules
/// in `lib\SoapySDR\modules0.8` below it. SoapySDR would look for them relative to the directory
/// above its DLL (the `bin\` layout of an installation), so rfscan points it to its own
/// directory. Only when that module directory exists and `SOAPY_SDR_ROOT` is not set by the
/// user; a build that uses an installed SoapySDR is not affected.
fn use_packaged_soapy() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    else {
        return;
    };
    if dir.join(SOAPY_MODULES).is_dir() && std::env::var_os("SOAPY_SDR_ROOT").is_none() {
        // SAFETY: called first in `main`, before any other thread exists, so nothing reads or
        // writes the environment at the same time.
        unsafe { std::env::set_var("SOAPY_SDR_ROOT", &dir) };
    }
}

/// Printed after an error from a failed retune: it usually means the receiver stopped
/// responding, most often because of a too short settle or a too high sample rate.
const TUNE_HINT: &str =
    "The receiver stopped responding. Unplug it and plug it in again: it may stay stuck until then.
Then check --settle and --sample-rate for your receiver; see DOC.md, Receiver setup.";

/// Printed after the stream stopped delivering samples: an unplugged or hung receiver.
const NO_SAMPLES_HINT: &str = "The receiver stopped delivering samples. Unplug it and plug it in again: it may stay stuck until then.
Then check the USB connection, --settle and --sample-rate for your receiver; see DOC.md, Receiver setup.";

fn main() -> std::process::ExitCode {
    use_packaged_soapy();
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // The same output as returning the error from `main`, followed by the hint.
            eprintln!("Error: {e:?}");
            if let Some(hint) = receiver_hint(&e) {
                eprintln!("{hint}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

/// A hint for an error (or one of its causes) that points to the receiver settings.
fn receiver_hint(e: &anyhow::Error) -> Option<&'static str> {
    e.chain()
        .find_map(|c| match c.downcast_ref::<device::SdrError>()? {
            device::SdrError::Tune { .. } => Some(TUNE_HINT),
            device::SdrError::NoSamples { .. } => Some(NO_SAMPLES_HINT),
            _ => None,
        })
}

fn run() -> anyhow::Result<()> {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // -h and --help before parsing, so they work also with other options or a broken file.
    if let Some(long) = help::requested(&argv) {
        print!("{}", help::render(long, help::terminal_width()));
        return Ok(());
    }
    let (cli, config_note) = config::parse_args(argv).map_err(anyhow::Error::msg)?;
    if let Some(file) = &cli.gen_conf {
        let path = config::write(file.as_deref()).map_err(anyhow::Error::msg)?;
        println!("configuration file written: {}", path.display());
        return Ok(());
    }

    if cli.list_devices {
        let stdout = std::io::stdout();
        list_devices(&mut stdout.lock()).context("writing the device list")?;
        println!(
            "
Audio outputs (--audio-device):"
        );
        match audio::list_outputs() {
            Ok(list) => {
                for (name, is_default) in list {
                    println!("  {name}{}", if is_default { " (default)" } else { "" });
                }
            }
            Err(e) => println!("  error: {e}"),
        }
        return Ok(());
    }
    if cli.report {
        return report(&cli);
    }
    // --record-iq: checked before the device is opened.
    if let Some(dir) = &cli.record_iq {
        if !dir.is_dir() {
            bail!(
                "--record-iq: the directory {} does not exist",
                dir.display()
            );
        }
        if !matches!(&cli.scan_channels, Some(Some(list)) if list.0.len() == 1) {
            bail!(
                "--record-iq needs --scan-channels with exactly one frequency, e.g. --scan-channels 145.5M"
            );
        }
    }
    // --record-dir: rfscan never creates it; checked before the device is opened.
    if let Some(dir) = &cli.record_dir
        && !dir.is_dir()
    {
        bail!(
            "--record-dir: the directory {} does not exist (rfscan does not create it)",
            dir.display()
        );
    }
    ctrlc::set_handler(|| STOP.store(true, Ordering::Relaxed))
        .context("cannot set the Ctrl+C handler")?;

    // Log: the console, and with --log also a file (level --log-level).
    let mut log = match &cli.log {
        Some(path) => Log::open(path, cli.log_level).map_err(anyhow::Error::msg)?,
        None => Log::console(),
    };
    if let Some(path) = &cli.log {
        logln!(
            log,
            Level::Debug,
            "log: {} (level {:?})",
            path.display(),
            cli.log_level
        );
    }
    if let Some(note) = &config_note {
        logln!(log, Level::Debug, "{note}");
    }
    let stream_args = cli.stream_args.as_ref().map_or(&[][..], |k| &k.0[..]);
    let mut src = SoapySource::open(&cli.device, cli.channel, stream_args)?;
    logln!(log, Level::Debug, "device: {}", src.description());

    let (format, full_scale) = src.stream_format();
    if format.name() != src.native_format_name() {
        eprintln!(
            "WARNING: native format {} is not supported, using {} (conversion in SoapySDR costs CPU)",
            src.native_format_name(),
            format.name()
        );
    }

    let rate = match cli.sample_rate {
        Some(r) => r as f64,
        None => match src.sample_rates().last() {
            Some(&r) => r,
            None => bail!("the device did not report a list of sample rates, use --sample-rate"),
        },
    };
    let rate = src.set_sample_rate(rate)?;

    if let Some(cfg) = &cli.gain {
        src.configure(&cfg.0).context("--gain")?;
    }
    if let Some(ppm) = cli.ppm {
        let actual = src.set_frequency_correction(ppm).context("--ppm")?;
        logln!(log, Level::Debug, "frequency correction: {actual} ppm");
    }

    if cli.bench {
        return run_bench(&cli, &mut src, rate, &mut log);
    }
    if cli.calibrate {
        return run_calibrate(&cli, &mut src, rate, &mut log);
    }
    if let Some(dir) = &cli.record_iq {
        return run_record(&cli, &mut src, rate, dir, &mut log);
    }
    // Without --power: scanning (ranges, channels or the default range).
    let Some(freq) = cli.power else {
        let args: Vec<String> = std::env::args().skip(1).collect();
        logln!(
            log,
            Level::Info,
            "=== rfscan: session start ({}) ===",
            args.join(" ")
        );
        let result = run_scan(&cli, &mut src, rate, &mut log);
        match &result {
            Ok(()) => logln!(log, Level::Info, "=== rfscan: session end ==="),
            Err(e) => logln!(
                log,
                Level::Info,
                "=== rfscan: session ended with an error: {e:#} ==="
            ),
        }
        return result;
    };

    let actual_freq = src.set_frequency(freq as f64)?;

    let buf_len = src.buffer_len();
    println!(
        "frequency: {:.6} MHz (requested {:.6}), sample rate: {} MSps",
        actual_freq / 1e6,
        freq as f64 / 1e6,
        rate / 1e6
    );
    println!("receiver: {}", src.config_summary());
    println!(
        "format: {}, full scale {full_scale}, MTU {} samples, buffer {buf_len} samples",
        format.name(),
        src.mtu()
    );

    let mut buf = IqBuffer::new(format, buf_len);
    src.start()?;

    let start = Instant::now();
    let deadline = cli.duration.map(Duration::from_secs_f64);
    let mut acc = PowerAcc::default();
    let mut overflows = 0u64;
    let mut timeouts = 0u64;
    let mut period_start = Instant::now();
    let stdout = std::io::stdout();

    loop {
        match src.read(&mut buf, buf_len, READ_TIMEOUT_US)? {
            ReadOutcome::Samples(n) => accumulate(&buf, n, full_scale, &mut acc),
            ReadOutcome::Overflow => overflows += 1,
            ReadOutcome::Timeout => timeouts += 1,
        }

        let now = Instant::now();
        let period = now - period_start;
        if period >= REPORT_INTERVAL {
            let secs = period.as_secs_f64();
            let per_sec = acc.n as f64 / secs;
            let mut out = stdout.lock();
            writeln!(
                out,
                "{:7.1} s  {:.6} MHz  power {:6.1} dBFS  DC {:6.1} dBFS  samples {:.0}/s ({:.1}%)  overflows {overflows}  timeouts {timeouts}",
                (now - start).as_secs_f64(),
                actual_freq / 1e6,
                acc.power_dbfs(),
                acc.dc_dbfs(),
                per_sec,
                100.0 * per_sec / rate,
            )?;
            if overflows > 0 {
                writeln!(
                    out,
                    "WARNING: stream overflows (samples lost), consider a lower --sample-rate (available: {:?} Hz)",
                    src.sample_rates()
                )?;
            }
            acc = PowerAcc::default();
            overflows = 0;
            timeouts = 0;
            period_start = now;
        }

        if STOP.load(Ordering::Relaxed) || deadline.is_some_and(|d| now - start >= d) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plan::{Modulation, RangeSpec, Step};

    fn entry(freq: u64, mode: Option<Modulation>, bw: Option<u64>) -> channel_db::ChannelEntry {
        channel_db::ChannelEntry {
            freq,
            mode,
            bw,
            name: Some(format!("K{freq}")),
            rec: false,
            raster25: false,
            freq_text: String::new(),
            line: 1,
        }
    }

    /// A failed retune names the frequency; receiver errors get a hint, also below a context.
    #[test]
    fn tune_error() {
        let e = device::SdrError::Tune {
            hz: 433_200_000.0,
            message: "Other: setFrequency failed".into(),
        };
        assert_eq!(
            e.to_string(),
            "setFrequency 433.200 MHz failed: Other: setFrequency failed"
        );
        let e = anyhow::Error::from(e).context("scanning");
        assert_eq!(receiver_hint(&e), Some(TUNE_HINT));
        let e = anyhow::Error::from(device::SdrError::NoSamples { timeouts: 20 });
        assert_eq!(receiver_hint(&e), Some(NO_SAMPLES_HINT));
        let e = anyhow::Error::from(device::SdrError::Unsupported("x".into()));
        assert_eq!(receiver_hint(&e), None);
    }

    /// Channel file in a range: name and mode from the file, bw= from the file only for the
    /// demodulator and only without an explicit range bw=; the detector keeps the range bw.
    #[test]
    fn channel_file_applies_to_ranges() {
        let range = |bw_explicit| RangeSpec {
            start: 438_000_000,
            stop: 439_000_000,
            step: Step::from_hz(12_500),
            bw: 12_500.0,
            bw_explicit,
            mode: Modulation::Nfm,
        };
        let entries = [
            // 2 kHz off the grid: matching within half a step.
            entry(438_802_000, Some(Modulation::Digi), None),
            entry(438_500_000, Some(Modulation::Am), None),
            entry(438_250_000, None, Some(10_000)),
        ];
        let find = |p: &plan::ScanPlan, f: u64| p.channels.iter().find(|c| c.freq == f).cloned();

        let mut p = plan::ranges_plan(&[range(false)], 4_800_000, 1).unwrap();
        // Only the entry off the grid gets a warning.
        let warnings = apply_channel_db(&mut p, &entries, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("438.802000 MHz \"K438802000\" is +2.0 kHz off the range grid")
                && warnings[0].contains("nearest channel 438.800000 MHz"),
            "{warnings:?}"
        );
        let digi = find(&p, 438_800_000).unwrap();
        assert_eq!(digi.mode, Modulation::Digi);
        assert_eq!(digi.name.as_deref(), Some("K438802000"));
        assert_eq!(digi.demod_bw, 12_500.0);
        let am = find(&p, 438_500_000).unwrap();
        assert_eq!(
            (am.mode, am.demod_bw, am.bw),
            (Modulation::Am, AM_DEFAULT_BW_HZ, 12_500.0)
        );
        let narrow = find(&p, 438_250_000).unwrap();
        assert_eq!((narrow.mode, narrow.demod_bw), (Modulation::Nfm, 10_000.0));
        let unknown = find(&p, 438_262_500).unwrap();
        assert_eq!(
            (unknown.name.clone(), unknown.mode),
            (None, Modulation::Nfm)
        );

        // An explicit range bw= wins over the file bw=, the file mode still applies.
        let mut p = plan::ranges_plan(&[range(true)], 4_800_000, 1).unwrap();
        apply_channel_db(&mut p, &entries, None);
        assert_eq!(find(&p, 438_250_000).unwrap().demod_bw, 12_500.0);
        assert_eq!(find(&p, 438_500_000).unwrap().mode, Modulation::Am);
    }
}
