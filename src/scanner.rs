//! Scanner with a state machine, headless mode, with demodulation and audio in the
//! Locked state.
//!
//! ```text
//! Scanning --(signal >= threshold)--> Locked --(signal < threshold - hysteresis)--> Hang --(pause elapsed)--> Scanning
//!                                       ^                                            |
//!                                       +-------(signal >= threshold during pause)---+
//! ```
//!
//! - Scanning: plan windows in order (pass A, then B). In each window: retune, discard `settle`
//!   samples, measure. When a window has a non-excluded channel above the threshold, the
//!   scanner stops on the strongest one (Locked).
//! - Locked and Hang: the tuner stays at the LO where the signal was detected or confirmed, and
//!   measures that channel in consecutive sample blocks without retuning.
//! - Hang: the pause is counted from the drop below the threshold with hysteresis. After the
//!   pause scanning resumes from the next window.
//!
//! - Audio: in Locked the window samples go through a ring buffer to the DSP thread (`demod`),
//!   which extracts the channel digitally (NCO); Start/Stop messages carry the stream position.
//!   In Hang the audio is silent.
//! - Activity log: entering Locked from Scanning is one activity (`hits`), the demodulated audio
//!   time arrives in DSP thread reports. Saved every 30 s and at the end.
//!
//! Transitions are printed with a timestamp (to check `--pause` with a stopwatch). The loop does
//! not allocate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use num_complex::Complex;

use crate::activity::{ActivityLog, format_mhz};
use crate::bench::{StreamCounters, consume};
use crate::demod::{Ctrl, Report};
use crate::detector::Detector;
use crate::device::{IqBuffer, ReadOutcome, SdrError, SdrSource};
use crate::digi_tcp::{DigiEvent, DigiServer};
use crate::events::{
    Command, HistoryItem, LockView, SPECTRUM_BINS, ScanState, Spectrum, Status, UiEvent,
};
use crate::exclusions::Exclusions;
use crate::log::{Level, Log};
use crate::plan::{Modulation, ScanPlan};
use crate::{logev, logwarn};

const READ_TIMEOUT_US: i64 = 100_000;
const STATUS_INTERVAL: Duration = Duration::from_secs(30);
/// The first status line comes right after the start, once the speed can be measured.
const FIRST_STATUS: Duration = Duration::from_secs(1);
/// How often to print the channel measurement in the Locked state.
const LOCKED_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Locked -> Hang only after this long of continuous measurements below the threshold with
/// hysteresis. Single drops (measurement spread with a signal at the threshold) do not close the
/// channel, and the DSP thread cuts this audio segment (already noise) before the fade-out.
pub const EXIT_HOLD: Duration = Duration::from_millis(30);
/// Hang -> Locked only after this long of continuous measurements above the entry threshold.
/// Without it a single measurement spike with a signal at the threshold returned to Locked for
/// ~1 ms, restarted the pause and kept the scanner on the channel without audible audio.
pub const RETURN_HOLD: Duration = EXIT_HOLD;
/// How often to save the activity log if it changed.
const ACTIVITY_SAVE_INTERVAL: Duration = Duration::from_secs(30);
/// This many stream overflows within `OVERFLOW_PERIOD` mean the receiver keeps dropping
/// samples at this sample rate: warned once per run. A single overflow (e.g. at the start) is
/// not a reason to warn.
const OVERFLOW_WARN: u64 = 5;
const OVERFLOW_PERIOD: Duration = Duration::from_secs(60);
/// How often to send the status, the mini-spectrum and the live lock measurement to the UI.
const UI_STATUS_INTERVAL: Duration = Duration::from_millis(250);
const UI_LIVE_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct ScanConfig {
    pub settle_samples: usize,
    pub squelch_snr: f32,
    pub squelch_abs: Option<f32>,
    pub hysteresis: f32,
    pub pause: Duration,
    pub duration: Option<Duration>,
    /// Hit confirmation by a measurement at a different LO (`--confirm`).
    pub confirm: bool,
    /// Number of measurement blocks for confirmation (about 60 ms, to catch TDMA bursts).
    pub confirm_blocks: usize,
    /// Usable window band (Hz): determines the LO for confirmation.
    pub usable_hz: f64,
}

impl ScanConfig {
    /// Entry threshold (Scanning -> Locked, Hang -> Locked).
    fn above_entry(&self, power_db: f32, snr: f32) -> bool {
        snr >= self.squelch_snr && self.squelch_abs.is_none_or(|a| power_db >= a)
    }

    /// Exit threshold with hysteresis (Locked -> Hang).
    fn below_exit(&self, power_db: f32, snr: f32) -> bool {
        snr < self.squelch_snr - self.hysteresis
            || self
                .squelch_abs
                .is_some_and(|a| power_db < a - self.hysteresis)
    }
}

/// The channel the scanner stopped on.
#[derive(Clone, Copy)]
struct Lock {
    /// The window where the signal was detected (index in A-then-B order): after the pause
    /// scanning resumes from the next one.
    window: usize,
    /// Plan channel index.
    channel: usize,
    freq: u64,
    /// Channel offset from the tuner LO in the Locked state (window or confirmation LO).
    offset_hz: f64,
    mode: Modulation,
    /// Channel width for measurement and the demodulator filter width.
    bw: f64,
    demod_bw: f64,
    start: Instant,
    peak_rssi: f32,
    peak_snr: f32,
    /// Start of a continuous drop below the exit threshold: the instant and the audio stream
    /// position.
    below: Option<(Instant, u64)>,
    /// In Hang: start of a continuous signal above the entry threshold (return confirmation).
    above: Option<Instant>,
}

#[derive(Clone, Copy)]
enum State {
    Scanning,
    Locked(Lock),
    /// The signal disappeared at `since`.
    Hang(Lock, Instant),
}

/// Reads exactly `out.len()` samples from the stream, normalized to ±1.
pub fn read_iq(
    src: &mut impl SdrSource,
    buf: &mut IqBuffer,
    full_scale: f64,
    out: &mut [Complex<f32>],
    counters: &mut StreamCounters,
) -> anyhow::Result<()> {
    let mut filled = 0;
    let mut consecutive_timeouts = 0;
    while filled < out.len() {
        match src.read(buf, out.len() - filled, READ_TIMEOUT_US)? {
            ReadOutcome::Samples(n @ 1..) => {
                buf.to_cf32(n, full_scale, &mut out[filled..filled + n]);
                filled += n;
                consecutive_timeouts = 0;
            }
            ReadOutcome::Overflow => counters.overflows += 1,
            // An empty read counts as a timeout, so the loop does not spin without progress.
            ReadOutcome::Timeout | ReadOutcome::Samples(0) => {
                counters.timeouts += 1;
                consecutive_timeouts += 1;
                if consecutive_timeouts >= 20 {
                    return Err(SdrError::NoSamples {
                        timeouts: consecutive_timeouts,
                    }
                    .into());
                }
            }
        }
    }
    Ok(())
}

/// Link between the scanner and the DSP thread: Start/Stop messages and IQ samples through a
/// ring buffer. The scanner never waits: when the buffer is full, a sample block is skipped and
/// counted.
pub struct AudioLink {
    ctrl: Sender<Ctrl>,
    iq: rtrb::Producer<Complex<f32>>,
    reports: Receiver<Report>,
    underruns: Arc<AtomicU64>,
    pushed: u64,
    iq_dropped: u64,
    last: Option<Report>,
}

impl AudioLink {
    pub fn new(
        ctrl: Sender<Ctrl>,
        iq: rtrb::Producer<Complex<f32>>,
        reports: Receiver<Report>,
        underruns: Arc<AtomicU64>,
    ) -> Self {
        AudioLink {
            ctrl,
            iq,
            reports,
            underruns,
            pushed: 0,
            iq_dropped: 0,
            last: None,
        }
    }

    fn start(&mut self, lock: &Lock) {
        let _ = self.ctrl.send(Ctrl::Start {
            at: self.pushed,
            freq: lock.freq,
            offset_hz: lock.offset_hz,
            mode: lock.mode,
            bw: lock.demod_bw,
        });
    }

    /// Position in the sample stream (number of samples pushed).
    fn position(&self) -> u64 {
        self.pushed
    }

    /// End of an activity. Audio from position `tail_from` (the noise tail after the signal
    /// disappeared) is cut out.
    fn stop_with_tail(&mut self, tail_from: u64) {
        let _ = self.ctrl.send(Ctrl::Stop {
            at: self.pushed,
            tail_from: tail_from.min(self.pushed),
        });
    }

    fn stop(&mut self) {
        self.stop_with_tail(self.pushed);
    }

    fn push(&mut self, block: &[Complex<f32>]) {
        match self.iq.write_chunk(block.len()) {
            Ok(mut chunk) => {
                let (a, b) = chunk.as_mut_slices();
                let n = a.len();
                a.copy_from_slice(&block[..n]);
                b.copy_from_slice(&block[n..]);
                chunk.commit_all();
                self.pushed += block.len() as u64;
            }
            Err(_) => self.iq_dropped += block.len() as u64,
        }
    }

    /// Receives DSP thread reports and adds the audio time to the log.
    fn drain(&mut self, activity: &Mutex<ActivityLog>) {
        while let Ok(r) = self.reports.try_recv() {
            if let Ok(mut log) = activity.lock() {
                log.add_seconds(r.freq, r.audio_seconds);
            }
            self.last = Some(r);
        }
    }
}
/// Exclusions during the scan: the list and file (changed from the UI) and the channel mask.
pub struct ExclusionCtl {
    pub list: Exclusions,
    pub path: PathBuf,
    /// Channels masked like exclusions for another reason (`digi` without `--digi-tcp`).
    pub masked: Vec<bool>,
    /// `X`: number of adjacent channels excluded on each side (`--exclude-wide`).
    pub wide: u64,
    /// The file entries with their notes, shared with the UI (exclusions screen).
    pub shared: crate::exclusions::SharedList,
    mask: Vec<bool>,
    /// Batches added in this session, for undo.
    added: Vec<Vec<u64>>,
}

impl ExclusionCtl {
    pub fn new(
        list: Exclusions,
        path: PathBuf,
        masked: Vec<bool>,
        wide: u64,
        plan: &ScanPlan,
    ) -> Self {
        let mut ctl = ExclusionCtl {
            shared: Arc::new(Mutex::new(list.items())),
            list,
            path,
            masked,
            wide,
            mask: Vec::new(),
            added: Vec::new(),
        };
        ctl.rebuild(plan);
        ctl
    }

    /// Number of masked channels (exclusions and `masked`).
    pub fn masked_channels(&self) -> usize {
        self.mask.iter().filter(|&&m| m).count()
    }

    /// Rebuilds the mask after a change (only on user action, not in the hot loop).
    fn rebuild(&mut self, plan: &ScanPlan) {
        self.mask = self.list.mask(plan);
        for (m, &d) in self.mask.iter_mut().zip(&self.masked) {
            *m |= d;
        }
        if let Ok(mut s) = self.shared.lock() {
            *s = self.list.items();
        }
    }

    /// Adds exclusions: to the file, or for `minutes` in memory only.
    fn add(
        &mut self,
        plan: &ScanPlan,
        freqs: Vec<u64>,
        note: Option<&str>,
        minutes: Option<u32>,
    ) -> Result<(), String> {
        let new: Vec<u64> = freqs
            .into_iter()
            .filter(|f| !self.list.entries.contains(f))
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        if let Some(minutes) = minutes {
            self.list.add_timed(&new, note, minutes, Instant::now());
            self.added.push(new);
            self.rebuild(plan);
            return Ok(());
        }
        // Airband channels on the 25 kHz raster keep their 25 kHz channel number in the file.
        let raster25: Vec<u64> = new
            .iter()
            .copied()
            .filter(|&f| {
                plan.channels
                    .iter()
                    .any(|c| c.raster25 && c.freq.abs_diff(f) <= 100)
            })
            .collect();
        self.list.add(&self.path, &new, note, &raster25)?;
        self.added.push(new);
        self.rebuild(plan);
        Ok(())
    }

    /// Removes exclusions: timed ones from memory, file ones from the file.
    fn remove(&mut self, plan: &ScanPlan, freqs: &[u64]) -> Result<(), String> {
        self.list.remove_timed(freqs);
        let saved: Vec<u64> = freqs
            .iter()
            .copied()
            .filter(|f| self.list.entries.contains(f))
            .collect();
        let result = if saved.is_empty() {
            Ok(())
        } else {
            self.list.remove(&self.path, &saved)
        };
        self.rebuild(plan);
        result
    }

    /// Saves a timed exclusion to the file with its note. Returns false when `freq` is not timed.
    fn keep(&mut self, plan: &ScanPlan, freq: u64) -> Result<bool, String> {
        let Some(t) = self.list.timed.iter().find(|t| t.freq == freq) else {
            return Ok(false);
        };
        let note = t.note.clone();
        self.add(plan, vec![freq], note.as_deref(), None)?;
        // Undo applies to the batch the exclusion was added with, not to this save.
        self.added.pop();
        Ok(true)
    }

    /// Changes the note of an exclusion and updates the shared list.
    fn set_note(&mut self, plan: &ScanPlan, freq: u64, note: Option<&str>) -> Result<(), String> {
        let result = self.list.set_note(&self.path, freq, note);
        self.rebuild(plan);
        result
    }

    /// Ends the timed exclusions due by `now`; returns their frequencies (empty when none is
    /// due, without allocating).
    fn expire(&mut self, plan: &ScanPlan, now: Instant) -> Vec<u64> {
        if self.list.next_expiry().is_none_or(|t| t > now) {
            return Vec::new();
        }
        let ended = self.list.expire(now);
        self.rebuild(plan);
        ended
    }
}

/// Link to a user interface: events out, commands in.
pub struct UiLink {
    pub events: Sender<UiEvent>,
    pub commands: Receiver<Command>,
}

/// Scan session resources beyond the measurement itself: audio, activity log, stop flag.
pub struct Session<'a> {
    pub audio: Option<&'a mut AudioLink>,
    pub activity: &'a Mutex<ActivityLog>,
    pub activity_path: &'a Path,
    /// Set by Ctrl+C.
    pub stop: &'a AtomicBool,
    /// Digi output for DSD-FME (`--digi-tcp`).
    pub digi: Option<&'a DigiServer>,
    pub exclusions: &'a mut ExclusionCtl,
    /// User interface (TUI), `None` in headless mode.
    pub ui: Option<UiLink>,
    /// Audio recording (`--record-dir`): told where each activity begins and ends.
    pub recorder: Option<&'a crate::recorder::Link>,
}

/// Live view of a lock for the UI.
fn lock_view(
    lock: &Lock,
    rssi_db: f32,
    noise_db: f32,
    hang: Option<(Instant, Duration)>,
    audio: Option<&AudioLink>,
) -> LockView {
    LockView {
        channel: lock.channel,
        freq: lock.freq,
        mode: lock.mode,
        bw: lock.demod_bw,
        rssi_db,
        noise_db,
        peak_rssi_db: lock.peak_rssi,
        peak_snr_db: lock.peak_snr,
        start: lock.start,
        hang,
        agc_db: audio
            .and_then(|a| a.last)
            .filter(|_| lock.mode != Modulation::Digi)
            .map(|r| r.agc_gain_db),
    }
}

/// A finished activity for the detection history (ended at `end`).
fn history_item(lock: &Lock, end: Instant) -> HistoryItem {
    HistoryItem {
        at: chrono::Local::now(),
        channel: lock.channel,
        freq: lock.freq,
        mode: lock.mode,
        peak_snr_db: lock.peak_snr,
        peak_rssi_db: lock.peak_rssi,
        duration: end.saturating_duration_since(lock.start),
    }
}

/// Mini-spectrum of a window from its measurement (fixed arrays, no allocation).
fn spectrum(
    window: usize,
    lo_hz: u64,
    powers_db: &[f32],
    floors_db: &[f32],
    freqs: &[u64],
    indices: &[u32],
    mask: &[bool],
) -> Spectrum {
    let n = powers_db.len();
    let bins = n.clamp(1, SPECTRUM_BINS);
    let mut s = Spectrum {
        window,
        first_hz: freqs.first().copied().unwrap_or(lo_hz),
        last_hz: freqs.last().copied().unwrap_or(lo_hz),
        lo_hz,
        bins,
        snr_db: [f32::NEG_INFINITY; SPECTRUM_BINS],
        excluded: [false; SPECTRUM_BINS],
        noise_db: floors_db.first().copied().unwrap_or(f32::NAN),
        strongest: None,
    };
    for j in 0..n {
        let b = (j * bins / n.max(1)).min(bins - 1);
        let snr = powers_db[j] - floors_db[j];
        let excluded = mask[indices[j] as usize];
        s.excluded[b] |= excluded;
        if !excluded {
            s.snr_db[b] = s.snr_db[b].max(snr);
            if s.strongest.is_none_or(|(_, best)| snr > best) {
                s.strongest = Some((freqs[j], snr));
            }
        }
    }
    s
}

pub fn run(
    src: &mut impl SdrSource,
    plan: &ScanPlan,
    detector: &mut Detector,
    cfg: &ScanConfig,
    session: Session<'_>,
    log: &mut Log,
) -> anyhow::Result<()> {
    let Session {
        mut audio,
        activity,
        activity_path,
        stop,
        digi,
        exclusions,
        ui,
        recorder,
    } = session;
    // The entry threshold can change live from the UI.
    let mut cfg = cfg.clone();
    let send = |e: UiEvent| {
        if let Some(ui) = &ui {
            let _ = ui.events.try_send(e);
        }
    };
    let (format, full_scale) = src.stream_format();
    let mut buf = IqBuffer::new(format, src.buffer_len());
    let mut iq = vec![Complex::new(0.0f32, 0.0); detector.samples_needed()];
    let mut counters = StreamCounters::default();
    // Windows in scan order with a name (pass, window number), e.g. A0, B3, and the bandwidth of
    // the measured channels (for the scan speed), computed once at startup.
    let windows: Vec<(char, usize, &crate::plan::Window, f64)> = plan
        .passes
        .iter()
        .enumerate()
        .flat_map(|(p, pass)| {
            let name = if p == 0 { 'A' } else { 'B' };
            pass.iter()
                .enumerate()
                .map(move |(i, w)| (name, i, w, plan.measured_hz(w)))
        })
        .collect();
    anyhow::ensure!(!windows.is_empty(), "the plan contains no windows");

    src.start()?;
    let start = Instant::now();
    let mut status_at = start + FIRST_STATUS;
    let mut report_at = start;
    let mut state = State::Scanning;
    let mut pos = 0;
    let mut steps: u64 = 0;
    let mut cycles: u64 = 0;
    let mut covered_hz = 0.0;
    let mut scan_time = Duration::ZERO;
    let mut save_at = start + ACTIVITY_SAVE_INTERVAL;
    // Overflows counted at the start of the current period; `None` after the warning.
    let mut overflow_check: Option<(Instant, u64)> = Some((start + OVERFLOW_PERIOD, 0));
    let mut ui_status_at = start;
    let mut steps_at_status: u64 = 0;
    let mut ui_live_at = start;
    let mut rejected: u64 = 0;
    let mut paused = false;
    let mut quit = false;
    // Mode and demodulator width per channel, changed live by `Command::SetDigi`.
    let mut modes: Vec<(Modulation, f64)> =
        plan.channels.iter().map(|c| (c.mode, c.demod_bw)).collect();

    loop {
        // Commands from the UI: never blocking.
        while let Some(u) = &ui {
            let cmd = match u.commands.try_recv() {
                Ok(c) => c,
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                // The UI is gone (e.g. its thread failed): do not keep scanning without it.
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    quit = true;
                    break;
                }
            };
            let now = Instant::now();
            // The current lock, if any: commands that leave it go back to scanning from the
            // next window, with the audio stopped at once.
            let current = match state {
                State::Locked(l) | State::Hang(l, _) => Some(l),
                State::Scanning => None,
            };
            let mut leave = false;
            match cmd {
                Command::Quit => quit = true,
                Command::TogglePause => {
                    paused = !paused;
                    logev!(
                        log,
                        Level::Debug,
                        now,
                        "{}",
                        if paused { "PAUSED" } else { "RESUMED" }
                    );
                }
                Command::RecordingDeleted(path) => {
                    logev!(log, Level::Info, now, "REC DEL   {}", path.display());
                }
                Command::Squelch(delta) => {
                    cfg.squelch_snr = (cfg.squelch_snr + delta).max(0.0);
                    logev!(
                        log,
                        Level::Debug,
                        now,
                        "squelch   SNR >= {} dB",
                        cfg.squelch_snr
                    );
                }
                Command::Skip => leave = current.is_some(),
                Command::SetDigi { freq, digi: on } => {
                    let near = plan
                        .channels
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| (c.freq.abs_diff(freq) as f64) <= c.spacing / 2.0)
                        .min_by_key(|(_, c)| c.freq.abs_diff(freq));
                    if let Some((i, ch)) = near {
                        modes[i] = if on {
                            // The digi default width, unless set on the command line.
                            let bw = if ch.bw_explicit {
                                ch.demod_bw
                            } else {
                                crate::plan::DIGI_BW_HZ
                            };
                            (Modulation::Digi, bw)
                        } else if modes[i].0 == Modulation::Digi {
                            ch.base
                        } else {
                            modes[i]
                        };
                        // Without a digi output a digi channel is skipped.
                        exclusions.masked[i] = on && digi.is_none();
                        exclusions.rebuild(plan);
                        logev!(
                            log,
                            Level::Info,
                            now,
                            "MODE      {} MHz  {}{}",
                            format_mhz(ch.freq),
                            modes[i].0.name(),
                            if exclusions.masked[i] {
                                "  (skipped: no --digi or --digi-tcp)"
                            } else {
                                ""
                            }
                        );
                        // A lock on this channel ends; the next one uses the new mode.
                        leave = current.is_some_and(|l| l.channel == i);
                    }
                }
                Command::Exclude {
                    freq,
                    channel,
                    wide,
                    note,
                    minutes,
                } => {
                    // `X`: also the adjacent channels on the grid of the channel.
                    let spacing = channel
                        .and_then(|c| plan.channels.get(c))
                        .map_or(0.0, |c| c.spacing);
                    let n = if wide && spacing > 0.0 {
                        exclusions.wide as i64
                    } else {
                        0
                    };
                    let freqs: Vec<u64> = (-n..=n)
                        .map(|k| (freq as f64 + k as f64 * spacing).round() as u64)
                        .collect();
                    match exclusions.add(plan, freqs, note.as_deref(), minutes) {
                        Ok(()) => logev!(
                            log,
                            Level::Info,
                            now,
                            "EXCLUDED  {} MHz{}{}{}",
                            format_mhz(freq),
                            if n > 0 { " with adjacent channels" } else { "" },
                            minutes.map_or(String::new(), |m| format!(" for {m} min")),
                            note.as_deref()
                                .map_or(String::new(), |n| format!("  # {n}"))
                        ),
                        Err(e) => logwarn!(log, "WARNING: {e}"),
                    }
                    send(UiEvent::ExclusionsChanged);
                    // The current channel is now masked: also when it is one of the adjacent
                    // channels (half a step tolerance, as for the exclusion file).
                    leave = current.is_some_and(|l| exclusions.mask.get(l.channel) == Some(&true));
                }
                Command::UndoExclusion => {
                    if let Some(last) = exclusions.added.pop() {
                        match exclusions.remove(plan, &last) {
                            Ok(()) => logev!(
                                log,
                                Level::Info,
                                now,
                                "UNDONE    exclusion of {} frequency(ies)",
                                last.len()
                            ),
                            Err(e) => logwarn!(log, "WARNING: {e}"),
                        }
                        send(UiEvent::ExclusionsChanged);
                    }
                }
                Command::RemoveExclusion(f) => {
                    match exclusions.remove(plan, &[f]) {
                        Ok(()) => logev!(
                            log,
                            Level::Info,
                            now,
                            "REMOVED   exclusion {} MHz",
                            format_mhz(f)
                        ),
                        Err(e) => logwarn!(log, "WARNING: {e}"),
                    }
                    send(UiEvent::ExclusionsChanged);
                }
                Command::KeepExclusion(f) => {
                    match exclusions.keep(plan, f) {
                        Ok(true) => logev!(
                            log,
                            Level::Info,
                            now,
                            "KEPT      exclusion {} MHz (saved to the file)",
                            format_mhz(f)
                        ),
                        Ok(false) => {}
                        Err(e) => logwarn!(log, "WARNING: {e}"),
                    }
                    send(UiEvent::ExclusionsChanged);
                }
                Command::SetExclusionNote { freq, note } => {
                    match exclusions.set_note(plan, freq, note.as_deref()) {
                        Ok(()) => logev!(
                            log,
                            Level::Info,
                            now,
                            "NOTE      exclusion {} MHz{}",
                            format_mhz(freq),
                            note.as_deref()
                                .map_or(String::new(), |n| format!("  # {n}"))
                        ),
                        Err(e) => logwarn!(log, "WARNING: {e}"),
                    }
                    send(UiEvent::ExclusionsChanged);
                }
            }
            if let (true, Some(l)) = (leave, current) {
                let end = match state {
                    State::Hang(_, since) => since,
                    _ => now,
                };
                if let (State::Locked(_), Some(a)) = (state, audio.as_deref_mut()) {
                    a.stop();
                }
                if let Some(r) = recorder {
                    r.end();
                }
                send(UiEvent::History(history_item(&l, end)));
                send(UiEvent::Unlock);
                pos = (l.window + 1) % windows.len();
                if pos == 0 {
                    cycles += 1;
                }
                state = State::Scanning;
            }
        }
        if quit {
            break;
        }
        // Timed exclusions that ended: the channels are scanned again from the next window.
        let now = Instant::now();
        for f in exclusions.expire(plan, now) {
            logev!(
                log,
                Level::Info,
                now,
                "EXPIRED   exclusion {} MHz",
                format_mhz(f)
            );
            send(UiEvent::ExclusionsChanged);
        }

        state = match state {
            // Paused while scanning: keep reading the stream, do not move or measure.
            State::Scanning if paused => {
                read_iq(src, &mut buf, full_scale, &mut iq, &mut counters)?;
                State::Scanning
            }
            State::Scanning => {
                let t0 = Instant::now();
                let (_, _, w, w_hz) = windows[pos];
                src.tune(w.center as f64)?;
                consume(src, &mut buf, cfg.settle_samples, &mut counters)?;
                read_iq(src, &mut buf, full_scale, &mut iq, &mut counters)?;
                let r = detector.measure(pos, &iq);
                steps += 1;
                covered_hz += w_hz;
                let excluded = &exclusions.mask;
                if ui.is_some() && t0 >= ui_live_at {
                    send(UiEvent::Spectrum(spectrum(
                        pos,
                        w.center,
                        r.powers_db,
                        r.floors_db,
                        r.freqs,
                        r.indices,
                        excluded,
                    )));
                    ui_live_at = t0 + UI_LIVE_INTERVAL;
                }

                // The strongest non-excluded channel above the threshold that is a local maximum.
                let mut best: Option<(usize, f32)> = None;
                for (j, (&power_db, &index)) in r.powers_db.iter().zip(r.indices).enumerate() {
                    if excluded[index as usize]
                        || !cfg.above_entry(power_db, power_db - r.floors_db[j])
                        || !is_local_max(
                            r.powers_db,
                            r.freqs,
                            j,
                            plan.channels[index as usize].spacing,
                        )
                    {
                        continue;
                    }
                    if best.is_none_or(|(_, p)| power_db > p) {
                        best = Some((j, power_db));
                    }
                }
                // The candidate copied from the window measurement results.
                let candidate = best.map(|(j, power_db)| {
                    (r.freqs[j], r.indices[j] as usize, power_db, r.floors_db[j])
                });
                let next = match candidate {
                    Some((freq, index, power_db, floor_db)) => {
                        let ch = &plan.channels[index];
                        let confirmed = if cfg.confirm {
                            // Measure again at a different LO: a real signal
                            // stays, an LO-dependent artifact (image, receiver products) does not.
                            let offset = freq as f64 - w.center as f64;
                            let lo = (freq as f64 - confirm_offset(offset, cfg.usable_hz)).round()
                                as u64;
                            if src.set_frequency(lo as f64).is_ok() {
                                consume(src, &mut buf, cfg.settle_samples, &mut counters)?;
                                let off = freq as f64 - lo as f64;
                                let mut result = Confirm::No {
                                    lo,
                                    max_snr: f32::NEG_INFINITY,
                                };
                                for _ in 0..cfg.confirm_blocks {
                                    read_iq(src, &mut buf, full_scale, &mut iq, &mut counters)?;
                                    let (p, fl) = detector.measure_channel(&iq, off, ch.bw);
                                    if cfg.above_entry(p, p - fl) {
                                        result = Confirm::Yes {
                                            lo,
                                            power_db: p,
                                            floor_db: fl,
                                        };
                                        break;
                                    }
                                    if let Confirm::No { max_snr, .. } = &mut result {
                                        *max_snr = max_snr.max(p - fl);
                                    }
                                }
                                result
                            } else {
                                // Confirmation LO outside the device range: a hit without
                                // confirmation, at the window LO.
                                src.tune(w.center as f64)?;
                                consume(src, &mut buf, cfg.settle_samples, &mut counters)?;
                                Confirm::Yes {
                                    lo: w.center,
                                    power_db,
                                    floor_db,
                                }
                            }
                        } else {
                            Confirm::Yes {
                                lo: w.center,
                                power_db,
                                floor_db,
                            }
                        };
                        let now = Instant::now();
                        let (name, i, _, _) = windows[pos];
                        match confirmed {
                            Confirm::Yes {
                                lo,
                                power_db,
                                floor_db,
                            } => {
                                let snr = power_db - floor_db;
                                logev!(
                                    log,
                                    Level::Info,
                                    now,
                                    "LOCKED    {} MHz  {:4}  RSSI {power_db:6.1} dBFS  noise {:6.1} dBFS  SNR {snr:5.1} dB  (window {name}{i}{}){}{}{}{}",
                                    format_mhz(freq),
                                    modes[index].0.name(),
                                    floor_db,
                                    if lo != w.center { ", confirmed" } else { "" },
                                    ChTag(freq, ch.raster25),
                                    if ch.name.is_some() { "  \"" } else { "" },
                                    ch.name.as_deref().unwrap_or(""),
                                    if ch.name.is_some() { "\"" } else { "" },
                                );
                                report_at = now + LOCKED_REPORT_INTERVAL;
                                let lock = Lock {
                                    window: pos,
                                    channel: index,
                                    freq,
                                    offset_hz: freq as f64 - lo as f64,
                                    mode: modes[index].0,
                                    bw: ch.bw,
                                    demod_bw: modes[index].1,
                                    start: now,
                                    peak_rssi: power_db,
                                    peak_snr: snr,
                                    below: None,
                                    above: None,
                                };
                                if let Ok(mut log) = activity.lock() {
                                    log.hit(lock.freq, lock.mode);
                                }
                                if let Some(r) = recorder {
                                    r.begin(lock.freq, ch.raster25);
                                }
                                if lock.mode == Modulation::Digi {
                                    logev!(
                                        log,
                                        Level::Debug,
                                        now,
                                        "digi      raw discriminator to TCP, audio chain (audio_dsp) bypassed, only the decoded voice from DSD-FME is heard{}",
                                        DigiStatus(digi),
                                    );
                                }
                                if let Some(a) = audio.as_deref_mut() {
                                    a.start(&lock);
                                }
                                send(UiEvent::Lock(lock_view(
                                    &lock,
                                    power_db,
                                    floor_db,
                                    None,
                                    audio.as_deref(),
                                )));
                                State::Locked(lock)
                            }
                            Confirm::No { lo, max_snr } => {
                                rejected += 1;
                                logev!(
                                    log,
                                    Level::Debug,
                                    now,
                                    "rejected  {} MHz  SNR {:5.1} dB in window {name}{i}, at LO {} MHz max SNR {max_snr:5.1} dB (LO-dependent artifact)",
                                    format_mhz(freq),
                                    power_db - floor_db,
                                    format_mhz(lo),
                                );
                                pos = (pos + 1) % windows.len();
                                if pos == 0 {
                                    cycles += 1;
                                }
                                State::Scanning
                            }
                        }
                    }
                    None => {
                        pos = (pos + 1) % windows.len();
                        if pos == 0 {
                            cycles += 1;
                        }
                        State::Scanning
                    }
                };
                scan_time += t0.elapsed();
                next
            }

            State::Locked(mut lock) => {
                read_iq(src, &mut buf, full_scale, &mut iq, &mut counters)?;
                let block_start = audio.as_deref().map_or(0, AudioLink::position);
                if let Some(a) = audio.as_deref_mut() {
                    a.push(&iq);
                }
                let (power_db, floor_db) = detector.measure_channel(&iq, lock.offset_hz, lock.bw);
                let snr = power_db - floor_db;
                let now = Instant::now();
                if cfg.below_exit(power_db, snr) {
                    lock.below.get_or_insert((now, block_start));
                } else {
                    lock.below = None;
                    lock.peak_rssi = lock.peak_rssi.max(power_db);
                    lock.peak_snr = lock.peak_snr.max(snr);
                }
                match lock.below {
                    Some((since, tail_from)) if now - since >= EXIT_HOLD => {
                        logev!(
                            log,
                            Level::Info,
                            since,
                            "HANG      {} MHz  SNR {snr:5.1} dB  (signal {:.3} s, max RSSI {:.1} dBFS, max SNR {:.1} dB, pause {:.3} s){}",
                            format_mhz(lock.freq),
                            (since - lock.start).as_secs_f64(),
                            lock.peak_rssi,
                            lock.peak_snr,
                            cfg.pause.as_secs_f64(),
                            ChTag(lock.freq, plan.channels[lock.channel].raster25)
                        );
                        if let Some(a) = audio.as_deref_mut() {
                            a.stop_with_tail(tail_from);
                        }
                        lock.below = None;
                        send(UiEvent::Lock(lock_view(
                            &lock,
                            power_db,
                            floor_db,
                            Some((since, cfg.pause)),
                            audio.as_deref(),
                        )));
                        // The pause is counted from the signal loss.
                        State::Hang(lock, since)
                    }
                    _ => {
                        if ui.is_some() && now >= ui_live_at {
                            send(UiEvent::Lock(lock_view(
                                &lock,
                                power_db,
                                floor_db,
                                None,
                                audio.as_deref(),
                            )));
                            ui_live_at = now + UI_LIVE_INTERVAL;
                        }
                        if now >= report_at {
                            logev!(
                                log,
                                Level::Debug,
                                now,
                                "locked    {} MHz  RSSI {power_db:6.1} dBFS  noise {:6.1} dBFS  SNR {snr:5.1} dB{}",
                                format_mhz(lock.freq),
                                floor_db,
                                LockedStatus(audio.as_deref(), lock.mode, digi),
                            );
                            report_at = now + LOCKED_REPORT_INTERVAL;
                        }
                        State::Locked(lock)
                    }
                }
            }

            State::Hang(mut lock, since) => {
                read_iq(src, &mut buf, full_scale, &mut iq, &mut counters)?;
                let (power_db, floor_db) = detector.measure_channel(&iq, lock.offset_hz, lock.bw);
                let snr = power_db - floor_db;
                let now = Instant::now();
                if cfg.above_entry(power_db, snr) {
                    lock.above.get_or_insert(now);
                } else {
                    lock.above = None;
                }
                if lock.above.is_some_and(|t| now - t >= RETURN_HOLD) {
                    lock.above = None;
                    lock.peak_rssi = lock.peak_rssi.max(power_db);
                    lock.peak_snr = lock.peak_snr.max(snr);
                    logev!(
                        log,
                        Level::Info,
                        now,
                        "LOCKED    {} MHz  RSSI {power_db:6.1} dBFS  noise {:6.1} dBFS  SNR {snr:5.1} dB  (return after {:.3} s of pause){}",
                        format_mhz(lock.freq),
                        floor_db,
                        (now - since).as_secs_f64(),
                        ChTag(lock.freq, plan.channels[lock.channel].raster25)
                    );
                    report_at = now + LOCKED_REPORT_INTERVAL;
                    if let Some(a) = audio.as_deref_mut() {
                        a.start(&lock);
                    }
                    send(UiEvent::Lock(lock_view(
                        &lock,
                        power_db,
                        floor_db,
                        None,
                        audio.as_deref(),
                    )));
                    State::Locked(lock)
                } else if lock.above.is_none() && now - since >= cfg.pause && !paused {
                    if let Some(r) = recorder {
                        r.end();
                    }
                    send(UiEvent::History(history_item(&lock, since)));
                    send(UiEvent::Unlock);
                    pos = (lock.window + 1) % windows.len();
                    if pos == 0 {
                        cycles += 1;
                    }
                    let (name, i, _, _) = windows[pos];
                    logev!(
                        log,
                        Level::Debug,
                        now,
                        "SCANNING  (pause of {:.3} s elapsed, resuming from window {name}{i})",
                        (now - since).as_secs_f64()
                    );
                    State::Scanning
                } else {
                    if ui.is_some() && now >= ui_live_at {
                        send(UiEvent::Lock(lock_view(
                            &lock,
                            power_db,
                            floor_db,
                            Some((since, cfg.pause)),
                            audio.as_deref(),
                        )));
                        ui_live_at = now + UI_LIVE_INTERVAL;
                    }
                    State::Hang(lock, since)
                }
            }
        };

        let now = Instant::now();
        if ui.is_some() && now >= ui_status_at {
            let scan_secs = scan_time.as_secs_f64().max(1e-9);
            send(UiEvent::Status(Status {
                state: match state {
                    _ if paused => ScanState::Paused,
                    State::Scanning => ScanState::Scanning,
                    State::Locked(_) => ScanState::Locked,
                    State::Hang(..) => ScanState::Hang,
                },
                window: match state {
                    State::Locked(l) | State::Hang(l, _) => l.window,
                    State::Scanning => pos,
                },
                visited: match state {
                    State::Scanning => (steps - steps_at_status).min(windows.len() as u64) as usize,
                    _ => 0,
                },
                cycles,
                windows_per_s: steps as f64 / scan_secs,
                mhz_per_s: covered_hz / 1e6 / scan_secs,
                overflows: counters.overflows,
                timeouts: counters.timeouts,
                rejected,
                squelch_snr: cfg.squelch_snr,
                hysteresis: cfg.hysteresis,
            }));
            ui_status_at = now + UI_STATUS_INTERVAL;
            steps_at_status = steps;
        }
        if now >= status_at {
            let scan_secs = scan_time.as_secs_f64().max(1e-9);
            logev!(
                log,
                Level::Debug,
                now,
                "status    cycles {cycles}  windows/s {:.1}  {:.1} MHz/s (during {:.1} s of scanning)  overflows {}  timeouts {}",
                steps as f64 / scan_secs,
                covered_hz / 1e6 / scan_secs,
                scan_secs,
                counters.overflows,
                counters.timeouts
            );
            if digi.is_some() {
                logev!(log, Level::Debug, now, "digi     {}", DigiStatus(digi));
            }
            status_at = now + STATUS_INTERVAL;
        }
        if let Some(a) = audio.as_deref_mut() {
            a.drain(activity);
        }
        if let Some(d) = digi {
            while let Ok(e) = d.events.try_recv() {
                match e {
                    DigiEvent::Connected(peer) => {
                        logev!(log, Level::Debug, now, "digi      client connected: {peer}")
                    }
                    DigiEvent::Disconnected { peer, reason } => logev!(
                        log,
                        Level::Debug,
                        now,
                        "digi      client disconnected: {peer} ({reason}), still listening"
                    ),
                    DigiEvent::Dsd(m) => {
                        logev!(log, Level::Info, now, "digi      {m}")
                    }
                }
            }
        }
        if let Some(r) = recorder {
            while let Ok(e) = r.events.try_recv() {
                match e {
                    crate::recorder::Event::Info(m) => logev!(log, Level::Info, now, "{m}"),
                    crate::recorder::Event::Warning(m) => logwarn!(log, "WARNING: {m}"),
                }
            }
        }
        if now >= save_at {
            save_activity(activity, activity_path);
            save_at = now + ACTIVITY_SAVE_INTERVAL;
        }
        if let Some((at, before)) = overflow_check
            && now >= at
        {
            let dropped = counters.overflows - before;
            overflow_check = if dropped >= OVERFLOW_WARN {
                logwarn!(
                    log,
                    "WARNING: the receiver dropped samples {dropped} times in the last minute (stream overflows), so the scan misses parts of the band. Try a lower --sample-rate; see DOC.md, Receiver setup."
                );
                None
            } else {
                Some((now + OVERFLOW_PERIOD, counters.overflows))
            };
        }
        if stop.load(Ordering::Relaxed) || cfg.duration.is_some_and(|d| now - start >= d) {
            break;
        }
    }
    if let (State::Locked(_), Some(a)) = (state, audio.as_deref_mut()) {
        a.stop();
    }
    if let (State::Locked(_) | State::Hang(..), Some(r)) = (state, recorder) {
        r.end();
    }
    if let Some(a) = audio {
        // The last report arrives after Stop; a short moment to receive it.
        std::thread::sleep(Duration::from_millis(100));
        a.drain(activity);
        if a.iq_dropped > 0 {
            logwarn!(
                log,
                "WARNING: skipped {} IQ samples (the DSP thread could not keep up)",
                a.iq_dropped
            );
        }
    }
    save_activity(activity, activity_path);
    Ok(())
}

/// Result of confirming a hit at a different LO.
enum Confirm {
    Yes {
        lo: u64,
        power_db: f32,
        floor_db: f32,
    },
    No {
        lo: u64,
        max_snr: f32,
    },
}

/// Channel offset from the LO for confirmation (usable band `usable`): a channel far from the
/// LO moves to the other side of the LO, one close to the LO shifts by 1/4 of the band. The LO
/// moves by at least 1/4 of the band, and the channel stays at least 1/8 of the band away from
/// DC and fits in the band.
fn confirm_offset(offset: f64, usable: f64) -> f64 {
    if offset.abs() >= usable / 8.0 {
        -offset
    } else {
        offset + offset.signum() * usable / 4.0
    }
}

/// Whether channel `j` is not weaker than any direct grid neighbor (at most 1.5 spacings away),
/// including excluded ones. A channel weaker than its neighbor is leakage of a stronger signal
/// (FFT measurement spread, transmitter splatter), not a separate transmission: without this
/// rule a strong excluded transmitter stopped the scanner on the adjacent channel.
fn is_local_max(powers_db: &[f32], freqs: &[u64], j: usize, spacing: f64) -> bool {
    let near = |k: usize| (freqs[k] as f64 - freqs[j] as f64).abs() <= 1.5 * spacing;
    let not_weaker = |k: usize| !near(k) || powers_db[j] >= powers_db[k];
    (j == 0 || not_weaker(j - 1)) && (j + 1 >= powers_db.len() || not_weaker(j + 1))
}

/// Saves the activity log if it changed. A write error only produces a warning.
pub fn save_activity(activity: &Mutex<ActivityLog>, path: &Path) {
    if let Ok(mut log) = activity.lock()
        && log.is_dirty()
        && let Err(e) = log.save(path)
    {
        eprintln!("WARNING: {e}");
    }
}

/// Airband channel number for log lines (`  ch 118.035`), empty outside the airband.
struct ChTag(u64, bool);

impl std::fmt::Display for ChTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match crate::airband::channel_number(self.0, self.1) {
            Some(n) => write!(f, "  ch {n}"),
            None => Ok(()),
        }
    }
}

/// Digi output state: connection, address and statistics.
struct DigiStatus<'a>(Option<&'a DigiServer>);

impl std::fmt::Display for DigiStatus<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(d) = self.0 else {
            return write!(f, "  digi output off");
        };
        let s = &d.stats;
        write!(
            f,
            "  digi {}: {}  sent {} kB  dropped chunks {}  trimmed samples {}  connections {}",
            d.addr,
            if s.connected.load(Ordering::Relaxed) {
                "DSD-FME connected"
            } else {
                "waiting for DSD-FME"
            },
            s.bytes_sent.load(Ordering::Relaxed) / 1000,
            s.dropped_chunks.load(Ordering::Relaxed),
            s.trimmed_samples.load(Ordering::Relaxed),
            s.connections.load(Ordering::Relaxed)
        )
    }
}

/// Chain state for the `locked` line: audio (from the last DSP thread report) or the digi
/// output.
struct LockedStatus<'a>(Option<&'a AudioLink>, Modulation, Option<&'a DigiServer>);

impl std::fmt::Display for LockedStatus<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.1 == Modulation::Digi {
            DigiStatus(self.2).fmt(f)?;
            if let Some(r) = self.0.and_then(|a| a.last) {
                write!(f, "  send buffer full: dropped samples {}", r.digi_dropped)?;
            }
            return Ok(());
        }
        let Some(a) = self.0 else { return Ok(()) };
        let Some(r) = a.last else { return Ok(()) };
        write!(
            f,
            "  AGC {:+.1} dB  limiter {}  silence {:.1} s  audio underruns {}  dropped audio {}",
            r.agc_gain_db,
            r.limiter_hits,
            r.frozen_seconds,
            a.underruns.load(Ordering::Relaxed),
            r.audio_dropped
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{confirm_offset, is_local_max};

    #[test]
    fn confirm_moves_lo_and_keeps_channel_in_band() {
        let usable = 4_800_000.0;
        for offset in [
            -2_390_000.0,
            -1_000_000.0,
            -600_000.0,
            -18_750.0,
            18_750.0,
            556_250.0,
            2_390_000.0,
        ] {
            let new = confirm_offset(offset, usable);
            // LO shifted by at least 1/4 of the band, the channel in the band and away from DC.
            assert!(
                (new - offset).abs() >= usable / 4.0 - 1.0,
                "{offset} -> {new}"
            );
            assert!(new.abs() <= usable / 2.0 - 10_000.0, "{offset} -> {new}");
            assert!(new.abs() >= usable / 8.0 - 1.0, "{offset} -> {new}");
        }
    }

    #[test]
    fn spill_next_to_strong_channel_is_not_local_max() {
        let freqs = [438_112_500, 438_125_000, 438_137_500, 438_150_000];
        // A strong signal on 438.125 (e.g. excluded) and leakage on 438.1375.
        let powers = [-80.0, -37.0, -60.0, -86.0];
        assert!(is_local_max(&powers, &freqs, 1, 12_500.0));
        assert!(!is_local_max(&powers, &freqs, 2, 12_500.0));
        assert!(!is_local_max(&powers, &freqs, 0, 12_500.0));
    }

    #[test]
    fn distant_list_entries_are_not_neighbours() {
        // Channel mode: adjacent list positions far from each other are not compared.
        let freqs = [124_175_000, 145_500_000];
        let powers = [-70.0, -40.0];
        assert!(is_local_max(&powers, &freqs, 0, 8_333.3));
        assert!(is_local_max(&powers, &freqs, 1, 12_500.0));
    }
}
