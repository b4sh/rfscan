//! Contract between the scanner core and a user interface.
//!
//! The scanner sends `UiEvent`s through a bounded channel with `try_send` (it never waits; when
//! the interface is slow, events are dropped) and polls `Command`s with `try_recv` once per loop
//! iteration. All events are `Copy` and fixed-size, so sending them does not allocate in the hot
//! loop. Channel names are looked up by the interface from the plan channel index.

use std::time::{Duration, Instant};

use crate::plan::Modulation;

/// Number of mini-spectrum buckets per window.
pub const SPECTRUM_BINS: usize = 96;

/// Scanner state shown in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanState {
    Scanning,
    Locked,
    Hang,
    Paused,
}

/// Periodic scanner status.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: ScanState,
    /// Index of the current window in scan order (pass A, then B).
    pub window: usize,
    /// Windows measured since the previous status (they end at `window`); 0 when stopped.
    pub visited: usize,
    pub cycles: u64,
    pub windows_per_s: f64,
    pub mhz_per_s: f64,
    pub overflows: u64,
    pub timeouts: u64,
    /// Hits rejected by `--confirm`.
    pub rejected: u64,
    /// Entry threshold (changes live with `+`/`-`) and hysteresis.
    pub squelch_snr: f32,
    pub hysteresis: f32,
}

/// Mini-spectrum of the last measured window: per bucket the highest channel SNR (dB) and flags.
#[derive(Clone, Copy, Debug)]
pub struct Spectrum {
    pub window: usize,
    pub first_hz: u64,
    pub last_hz: u64,
    pub lo_hz: u64,
    /// Number of used buckets (fewer than `SPECTRUM_BINS` when the window has fewer channels).
    pub bins: usize,
    pub snr_db: [f32; SPECTRUM_BINS],
    /// Bucket contains an excluded channel.
    pub excluded: [bool; SPECTRUM_BINS],
    pub noise_db: f32,
    /// Strongest non-excluded channel in the window: frequency and SNR.
    pub strongest: Option<(u64, f32)>,
}

/// The channel the scanner stopped on (Locked or Hang), with live measurements.
#[derive(Clone, Copy, Debug)]
pub struct LockView {
    /// Plan channel index (for the name lookup).
    pub channel: usize,
    pub freq: u64,
    pub mode: Modulation,
    /// Demodulator filter width (Hz).
    pub bw: f64,
    pub rssi_db: f32,
    pub noise_db: f32,
    pub peak_rssi_db: f32,
    pub peak_snr_db: f32,
    pub start: Instant,
    /// In Hang: when the signal disappeared and the pause length.
    pub hang: Option<(Instant, Duration)>,
    /// Current audio AGC gain (dB), when known.
    pub agc_db: Option<f32>,
}

/// One finished activity for the detection history.
#[derive(Clone, Copy, Debug)]
pub struct HistoryItem {
    pub at: chrono::DateTime<chrono::Local>,
    pub channel: usize,
    pub freq: u64,
    pub mode: Modulation,
    pub peak_snr_db: f32,
    pub peak_rssi_db: f32,
    pub duration: Duration,
}

// Events are sent by value through a bounded channel: a fixed-size `Copy` enum keeps the hot
// loop free of allocation (boxing the spectrum would allocate on every send). The size
// difference is irrelevant at a few events per 100 ms.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug)]
pub enum UiEvent {
    Status(Status),
    Spectrum(Spectrum),
    Lock(LockView),
    /// Back to scanning (the lock view disappears).
    Unlock,
    History(HistoryItem),
    /// The exclusion list changed (the interface re-reads it from the shared list).
    ExclusionsChanged,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Quit,
    TogglePause,
    /// Skip the current signal once and resume scanning (not saved).
    Skip,
    /// Exclude a frequency with an optional note (from the note popup after `x`/`X` on the main
    /// screen or `x` on the activity screen). `channel` is the plan channel when known; `wide`
    /// also excludes its adjacent channels (`--exclude-wide`). `minutes`: a timed exclusion,
    /// not saved; `None` saves it to the file. If the scanner is on this frequency, it leaves it
    /// at once.
    Exclude {
        freq: u64,
        channel: Option<usize>,
        wide: bool,
        note: Option<String>,
        minutes: Option<u32>,
    },
    /// Undo the last exclusion added in this session.
    UndoExclusion,
    /// Remove an entry from the exclusion list (exclusions screen): a timed one, or one from the
    /// file (saved).
    RemoveExclusion(u64),
    /// Make a timed exclusion permanent (saved to the file with its note).
    KeepExclusion(u64),
    /// Change the note of an exclusion (`None` removes it): saved for a file entry, in memory for
    /// a timed one.
    SetExclusionNote {
        freq: u64,
        note: Option<String>,
    },
    /// A channel was switched to or from `digi` mode in the channel file (`[l]` on the activity
    /// screen): the nearest plan channel changes its mode at once.
    SetDigi {
        freq: u64,
        digi: bool,
    },
    /// A recording was deleted (from the TUI or the web GUI): the scanner logs it.
    RecordingDeleted(std::path::PathBuf),
    /// All the recordings (`dir` = `--record-dir`) or those of one channel (its subdirectory)
    /// were deleted, except `skipped` (the file being recorded): the scanner logs it.
    RecordingsDeleted {
        dir: std::path::PathBuf,
        files: usize,
        skipped: usize,
    },
    /// Change the entry threshold by this many dB.
    Squelch(f32),
}
