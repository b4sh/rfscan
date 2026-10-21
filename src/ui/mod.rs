//! TUI: a thin layer over the scanner core.
//!
//! Runs in its own thread. It receives `UiEvent`s from the scanner, sends `Command`s back and
//! redraws about every 50 ms. It contains no scanning logic. The layout adapts to the terminal
//! size on every frame (large or normal frequency font, history rows, statistics column).

mod bigtext;
mod view;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::activity::ActivityLog;
use crate::digi_tcp::DigiStats;
use crate::events::{Command, HistoryItem, LockView, Spectrum, Status, UiEvent};

/// Redraw and input poll period.
const FRAME: Duration = Duration::from_millis(50);
/// How many finished activities the history keeps.
const HISTORY_LEN: usize = 200;
/// After `q`, wait at most this long for the scanner to finish.
const QUIT_TIMEOUT: Duration = Duration::from_secs(3);
/// Squelch change per `+`/`-` press (dB).
const SQUELCH_STEP: f32 = 1.0;
/// Noise reduction strength step (percent, the `<`/`>` keys).
const NR_STRENGTH_STEP: i32 = 10;

/// One scanned range (or the channel list) for the progress view.
#[derive(Clone)]
pub struct RangeLabel {
    /// E.g. `430.000-440.000 MHz` or `12 channels`.
    pub span: String,
    /// First and last channel frequency of the range (Hz), for the position bar.
    pub start: u64,
    pub stop: u64,
}

/// Everything the TUI (and the web GUI) needs that does not change during the run, plus shared
/// state.
#[derive(Clone)]
pub struct UiContext {
    /// Short device name, e.g. `airspy 2064c8703d20018`.
    pub device: String,
    pub sample_rate: f64,
    /// Receiver configuration (`--gain`) or a note that the driver defaults are used.
    pub gain: String,
    pub ranges: Vec<RangeLabel>,
    /// Per window in scan order: the range index and the window name (e.g. `A3`).
    pub window_range: Vec<usize>,
    pub window_names: Vec<String>,
    /// Per window in scan order: its first and last channel frequency (Hz).
    pub window_span: Vec<(u64, u64)>,
    /// Per plan channel: the name from the channel file.
    pub names: Vec<Option<String>>,
    /// Channel file entries with names, for the activity and exclusions screens.
    pub channel_db: Vec<(u64, String)>,
    /// Channel file entries in `digi` mode (the initial state of `[Tab] digi` in the label
    /// popup).
    pub digi_db: Vec<u64>,
    /// Airband channels on the 25 kHz raster (actual frequencies), shown with their 25 kHz
    /// channel numbers.
    pub raster25: Vec<u64>,
    /// Channel file path (naming channels on the activity screen writes to it), and the plan
    /// channel frequencies (to update the names shown in the lock view and history).
    pub channels_path: Option<PathBuf>,
    pub channel_freqs: Vec<u64>,
    pub activity: Arc<Mutex<ActivityLog>>,
    pub activity_path: PathBuf,
    /// Exclusion file entries with their notes.
    pub exclusions: crate::exclusions::SharedList,
    pub exclusions_path: PathBuf,
    pub muted: Arc<AtomicBool>,
    /// Speech noise reduction on (`--nr`, the `n` key).
    pub nr: Arc<AtomicBool>,
    /// Noise reduction strength in percent (`--nr-strength`, the `<`/`>` keys).
    pub nr_strength: Arc<AtomicU32>,
    pub underruns: Arc<AtomicU64>,
    pub digi: Option<(SocketAddr, Arc<DigiStats>)>,
    /// DSD-FME started by rfscan (`--digi`): process state (`dsd_fme::DsdState`).
    pub dsd: Option<Arc<std::sync::atomic::AtomicU8>>,
    /// The last DSD-FME console lines, shown in a window on `digi` channels.
    pub dsd_console: Option<crate::dsd_fme::ConsoleLines>,
    /// Decoded voice from DSD-FME.
    pub voice: Option<Arc<crate::digi_voice::VoiceStats>>,
}

impl UiContext {
    /// Whether `hz` is an airband channel on the 25 kHz raster.
    pub(crate) fn is_raster25(&self, hz: u64) -> bool {
        self.raster25.iter().any(|&f| f.abs_diff(hz) <= 100)
    }

    /// Frequency as shown: the airband channel number, otherwise MHz.
    pub(crate) fn freq_label(&self, hz: u64) -> String {
        crate::airband::display(hz, self.is_raster25(hz))
    }
    /// Name from the channel file nearest to `hz` within 4 kHz (for frequencies outside the
    /// plan, e.g. on the activity screen).
    pub(crate) fn name_near(&self, hz: u64) -> Option<&str> {
        self.channel_db
            .iter()
            .filter(|(f, _)| f.abs_diff(hz) <= NAME_TOLERANCE_HZ)
            .min_by_key(|(f, _)| f.abs_diff(hz))
            .map(|(_, n)| n.as_str())
    }

    /// Whether the channel file entry nearest to `hz` is in `digi` mode.
    pub(crate) fn digi_near(&self, hz: u64) -> bool {
        let nearest = self
            .channel_db
            .iter()
            .map(|(f, _)| *f)
            .chain(self.digi_db.iter().copied())
            .filter(|f| f.abs_diff(hz) <= NAME_TOLERANCE_HZ)
            .min_by_key(|f| f.abs_diff(hz));
        nearest.is_some_and(|f| self.digi_db.contains(&f))
    }

    /// Names the channel nearest to `freq` in the channel file and sets its `digi` mode
    ///, then updates the names shown. Returns the frequency of the written entry.
    pub(crate) fn set_label(
        &mut self,
        freq: u64,
        name: Option<&str>,
        digi: bool,
    ) -> Result<u64, String> {
        let Some(path) = self.channels_path.clone() else {
            return Err("no channel file path (use --channels-file)".into());
        };
        let raster25 = self.is_raster25(freq);
        let written = crate::channel_db::set_entry(
            &path,
            freq,
            NAME_TOLERANCE_HZ as f64,
            name,
            digi,
            raster25,
        )?;
        self.set_name(written, name);
        self.digi_db.retain(|&f| f != written);
        if digi {
            self.digi_db.push(written);
        }
        Ok(written)
    }

    /// Updates the names shown after a channel was named (`None`: the name was removed).
    fn set_name(&mut self, freq: u64, name: Option<&str>) {
        let db = &mut self.channel_db;
        db.retain(|(f, _)| *f != freq);
        if let Some(name) = name {
            db.push((freq, name.to_string()));
        }
        for (n, &f) in self.names.iter_mut().zip(&self.channel_freqs) {
            if f.abs_diff(freq) <= NAME_TOLERANCE_HZ {
                *n = name.map(str::to_string);
            }
        }
    }

    /// Activity log rows sorted by descending total time.
    pub(crate) fn activity_rows(&self) -> Vec<ActivityRow> {
        let Ok(log) = self.activity.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<ActivityRow> = log
            .entries
            .iter()
            .map(|(&freq, e)| ActivityRow {
                freq,
                total: e.seconds,
                session: log.session.get(&freq).copied().unwrap_or(0.0),
                hits: e.hits,
                last: e.last.clone().unwrap_or_default(),
                mode: e.mode,
            })
            .collect();
        rows.sort_by(|a, b| b.total.total_cmp(&a.total));
        rows
    }

    /// Mode shown for an activity log row: `DIGI` when the channel file marks it digi (also
    /// after `[l]` with `[Tab]`), otherwise the mode of its last activity, if logged.
    pub(crate) fn activity_mode(&self, row: &ActivityRow) -> Option<&'static str> {
        if self.digi_near(row.freq) {
            Some(crate::plan::Modulation::Digi.name())
        } else {
            row.mode.map(|m| m.name())
        }
    }

    /// Clears the activity log and saves the empty file at once (no `.bak`).
    pub(crate) fn clear_activity(&self) -> Result<(), String> {
        match self.activity.lock() {
            Ok(mut log) => {
                log.clear();
                log.save(&self.activity_path)
            }
            Err(_) => Err("the activity log is not available".to_string()),
        }
    }

    /// Exclusion file entries with their notes.
    pub(crate) fn exclusion_list(&self) -> Vec<(u64, Option<String>)> {
        self.exclusions
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default()
    }
}

/// Matching tolerance of channel file names on the activity and exclusions screens.
const NAME_TOLERANCE_HZ: u64 = 4_000;
/// Longest text typed in a popup (channel name or exclusion note).
const LABEL_MAX: usize = 40;

/// What the text popup is for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// A channel name and the `digi` mode (activity screen, `[l] add label`, `[Tab]` switches
    /// `digi`), saved to the channel file; `was`: the `digi` mode when the popup opened.
    Label { digi: bool, was: bool },
    /// An optional note for an exclusion (`[x]`/`[X]`); `wide` also excludes the adjacent
    /// channels of `channel`.
    Exclude { channel: Option<usize>, wide: bool },
}

/// A text popup: the frequency, the typed text and an error, if the last save failed.
pub struct LabelEdit {
    kind: EditKind,
    freq: u64,
    text: String,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Main,
    Activity,
    Exclusions,
}

/// Linux thermal zone of the processor (millidegrees Celsius); missing on Windows and on
/// computers without a sensor, then no temperature is shown.
const CPU_TEMP_FILE: &str = "/sys/class/thermal/thermal_zone0/temp";

/// Program CPU usage (percent of one core), processor temperature (°C) and physical memory
/// (working set / RSS, bytes), measured once per second.
pub(crate) struct CpuMeter {
    last: Option<(Instant, Duration)>,
    pub(crate) percent: Option<f64>,
    pub(crate) temp: Option<f64>,
    pub(crate) ram: Option<usize>,
}

impl CpuMeter {
    pub(crate) fn new() -> Self {
        CpuMeter {
            last: None,
            percent: None,
            temp: None,
            ram: None,
        }
    }

    pub(crate) fn update(&mut self) {
        let now = Instant::now();
        let Ok(cpu) = cpu_time::ProcessTime::try_now().map(|t| t.as_duration()) else {
            return;
        };
        match self.last {
            Some((t, c)) if now - t >= Duration::from_secs(1) => {
                let wall = (now - t).as_secs_f64();
                self.percent = Some(100.0 * cpu.saturating_sub(c).as_secs_f64() / wall);
                self.last = Some((now, cpu));
            }
            Some(_) => return,
            None => self.last = Some((now, cpu)),
        }
        self.temp = std::fs::read_to_string(CPU_TEMP_FILE)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|m| m / 1000.0);
        self.ram = memory_stats::memory_stats().map(|m| m.physical_mem);
    }

    /// `9 %` or, with a temperature, `9 % / 48C`; `sep` and `unit` set the spacing for the
    /// one-line statistics (`9%/48C`).
    pub(crate) fn text(&self, unit: &str, sep: &str) -> String {
        match (self.percent, self.temp) {
            (None, _) => "-".to_string(),
            (Some(p), None) => format!("{p:.0}{unit}"),
            (Some(p), Some(t)) => format!("{p:.0}{unit}{sep}{t:.0}C"),
        }
    }
}

struct App {
    ctx: UiContext,
    status: Option<Status>,
    spectrum: Option<Spectrum>,
    lock: Option<LockView>,
    history: VecDeque<HistoryItem>,
    screen: Screen,
    activity_sel: usize,
    exclusions_sel: usize,
    cpu: CpuMeter,
    edit: Option<LabelEdit>,
    /// Confirmation of clearing the activity log (`[c] clear`), with an error if saving failed.
    confirm_clear: Option<Option<String>>,
}

/// Runs the TUI until the scanner finishes (the event channel closes).
pub fn run(
    ctx: UiContext,
    events: Receiver<UiEvent>,
    commands: Sender<Command>,
) -> anyhow::Result<()> {
    // `init` enters the alternate screen and raw mode and installs a panic hook that restores
    // the terminal.
    let mut terminal = ratatui::init();
    let mut app = App {
        ctx,
        status: None,
        spectrum: None,
        lock: None,
        history: VecDeque::new(),
        screen: Screen::Main,
        activity_sel: 0,
        exclusions_sel: 0,
        cpu: CpuMeter::new(),
        edit: None,
        confirm_clear: None,
    };
    let result = event_loop(&mut terminal, &mut app, &events, &commands);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    events: &Receiver<UiEvent>,
    commands: &Sender<Command>,
) -> anyhow::Result<()> {
    let mut quitting: Option<Instant> = None;
    loop {
        loop {
            match events.try_recv() {
                Ok(e) => app.apply(e),
                Err(TryRecvError::Empty) => break,
                // The scanner finished.
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if quitting.is_some_and(|t| t.elapsed() > QUIT_TIMEOUT) {
            return Ok(());
        }
        app.cpu.update();
        terminal.draw(|f| view::draw(f, app))?;
        if event::poll(FRAME)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            // The label popup takes all keys (also `q`), except Ctrl+C.
            if app.edit.is_some() && !ctrl_c {
                if let Some(cmd) = app.edit_key(key.code) {
                    let _ = commands.send(cmd);
                }
                continue;
            }
            if app.confirm_clear.is_some() && !ctrl_c {
                app.confirm_key(key.code);
                continue;
            }
            if ctrl_c || key.code == KeyCode::Char('q') {
                let _ = commands.send(Command::Quit);
                quitting.get_or_insert_with(Instant::now);
                continue;
            }
            if let Some(cmd) = app.key(key.code) {
                let _ = commands.send(cmd);
            }
        }
    }
}

impl App {
    fn change_nr_strength(&self, step: i32) {
        let _ = self
            .ctx
            .nr_strength
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |m| {
                Some((m as i32 + step).clamp(0, 100) as u32)
            });
    }

    fn apply(&mut self, e: UiEvent) {
        match e {
            UiEvent::Status(s) => self.status = Some(s),
            UiEvent::Spectrum(s) => self.spectrum = Some(s),
            UiEvent::Lock(l) => self.lock = Some(l),
            UiEvent::Unlock => self.lock = None,
            UiEvent::History(h) => {
                self.history.push_front(h);
                self.history.truncate(HISTORY_LEN);
            }
            UiEvent::ExclusionsChanged => {}
        }
    }

    /// Handles a key; returns a command for the scanner, if any.
    fn key(&mut self, code: KeyCode) -> Option<Command> {
        match self.screen {
            Screen::Main => match code {
                KeyCode::Char('p') => Some(Command::TogglePause),
                KeyCode::Char(' ') => Some(Command::Skip),
                KeyCode::Char(c @ ('x' | 'X')) => {
                    // The note popup; the exclusion is sent on Enter.
                    if let Some(l) = self.lock {
                        self.edit = Some(LabelEdit {
                            kind: EditKind::Exclude {
                                channel: Some(l.channel),
                                wide: c == 'X',
                            },
                            freq: l.freq,
                            text: String::new(),
                            error: None,
                        });
                    }
                    None
                }
                KeyCode::Char('u') => Some(Command::UndoExclusion),
                KeyCode::Char('+') | KeyCode::Char('=') => Some(Command::Squelch(SQUELCH_STEP)),
                KeyCode::Char('-') => Some(Command::Squelch(-SQUELCH_STEP)),
                KeyCode::Char('m') => {
                    self.ctx.muted.fetch_xor(true, Ordering::Relaxed);
                    None
                }
                KeyCode::Char('n') => {
                    self.ctx.nr.fetch_xor(true, Ordering::Relaxed);
                    None
                }
                KeyCode::Char('<') | KeyCode::Char(',') => {
                    self.change_nr_strength(-NR_STRENGTH_STEP);
                    None
                }
                KeyCode::Char('>') | KeyCode::Char('.') => {
                    self.change_nr_strength(NR_STRENGTH_STEP);
                    None
                }
                KeyCode::Char('a') => {
                    self.screen = Screen::Activity;
                    None
                }
                KeyCode::Char('e') => {
                    self.screen = Screen::Exclusions;
                    None
                }
                _ => None,
            },
            Screen::Activity => {
                let rows = self.activity_rows();
                match code {
                    KeyCode::Char('a') | KeyCode::Esc => self.screen = Screen::Main,
                    KeyCode::Char('x') => {
                        if let Some(r) = rows.get(self.activity_sel) {
                            self.edit = Some(LabelEdit {
                                kind: EditKind::Exclude {
                                    channel: None,
                                    wide: false,
                                },
                                freq: r.freq,
                                text: String::new(),
                                error: None,
                            });
                        }
                    }
                    KeyCode::Char('c') => self.confirm_clear = Some(None),
                    KeyCode::Char('l') => {
                        if let Some(r) = rows.get(self.activity_sel) {
                            let digi = self.ctx.digi_near(r.freq);
                            self.edit = Some(LabelEdit {
                                kind: EditKind::Label { digi, was: digi },
                                freq: r.freq,
                                text: self.ctx.name_near(r.freq).unwrap_or("").to_string(),
                                error: None,
                            });
                        }
                    }
                    _ => scroll(&mut self.activity_sel, rows.len(), code),
                }
                None
            }
            Screen::Exclusions => {
                let list = self.exclusion_list();
                match code {
                    KeyCode::Char('e') | KeyCode::Esc => self.screen = Screen::Main,
                    KeyCode::Char('d') => {
                        let cmd = list
                            .get(self.exclusions_sel)
                            .map(|&(f, _)| Command::RemoveExclusion(f));
                        self.exclusions_sel = self.exclusions_sel.min(list.len().saturating_sub(2));
                        return cmd;
                    }
                    _ => scroll(&mut self.exclusions_sel, list.len(), code),
                }
                None
            }
        }
    }

    /// A key in the clear confirmation: `y` clears the activity log and saves the empty file,
    /// `n` or Esc cancels.
    fn confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => match self.ctx.clear_activity() {
                Ok(()) => {
                    self.confirm_clear = None;
                    self.activity_sel = 0;
                }
                Err(e) => self.confirm_clear = Some(Some(e)),
            },
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.confirm_clear = None,
            _ => {}
        }
    }

    /// A key in the text popup: typing, Backspace, Enter saves (a label) or excludes (with the
    /// note, if any), Esc cancels. Returns a command for the scanner, if any.
    fn edit_key(&mut self, code: KeyCode) -> Option<Command> {
        let edit = self.edit.as_mut()?;
        match code {
            KeyCode::Esc => self.edit = None,
            KeyCode::Backspace => {
                edit.text.pop();
            }
            KeyCode::Tab => {
                if let EditKind::Label { digi, .. } = &mut edit.kind {
                    *digi = !*digi;
                }
            }
            KeyCode::Char(c) if edit.text.chars().count() < LABEL_MAX && !c.is_control() => {
                edit.text.push(c);
            }
            KeyCode::Enter => {
                let text = edit.text.trim().to_string();
                let freq = edit.freq;
                match edit.kind {
                    EditKind::Exclude { channel, wide } => {
                        self.edit = None;
                        return Some(Command::Exclude {
                            freq,
                            channel,
                            wide,
                            note: (!text.is_empty()).then_some(text),
                        });
                    }
                    EditKind::Label { digi, was } => {
                        if text.is_empty() && !digi {
                            edit.error =
                                Some("the name is empty (with [Tab] digi it may be)".into());
                            return None;
                        }
                        let name = (!text.is_empty()).then_some(text.as_str());
                        match self.ctx.set_label(freq, name, digi) {
                            Ok(written) => {
                                self.edit = None;
                                // The scanner changes the channel mode at once.
                                if digi != was {
                                    return Some(Command::SetDigi {
                                        freq: written,
                                        digi,
                                    });
                                }
                            }
                            Err(e) => edit.error = Some(e),
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn activity_rows(&self) -> Vec<ActivityRow> {
        self.ctx.activity_rows()
    }

    fn exclusion_list(&self) -> Vec<(u64, Option<String>)> {
        self.ctx.exclusion_list()
    }
}

pub(crate) struct ActivityRow {
    pub(crate) freq: u64,
    pub(crate) total: f64,
    pub(crate) session: f64,
    pub(crate) hits: u64,
    pub(crate) last: String,
    /// Mode of the last activity (older log lines have none).
    pub(crate) mode: Option<crate::plan::Modulation>,
}

/// Moves a list selection with the arrow, page and home/end keys.
fn scroll(sel: &mut usize, len: usize, code: KeyCode) {
    let last = len.saturating_sub(1);
    *sel = match code {
        KeyCode::Up => sel.saturating_sub(1),
        KeyCode::Down => (*sel + 1).min(last),
        KeyCode::PageUp => sel.saturating_sub(10),
        KeyCode::PageDown => (*sel + 10).min(last),
        KeyCode::Home => 0,
        KeyCode::End => last,
        _ => *sel,
    }
    .min(last);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{LockView, SPECTRUM_BINS, ScanState, Spectrum, Status};
    use crate::plan::Modulation;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let ctx = UiContext {
            device: "airspy 2064c8703d20018".into(),
            sample_rate: 6e6,
            gain: "agc=true,VGA=7".into(),
            ranges: vec![
                RangeLabel {
                    span: "118.000-136.675 MHz".into(),
                    start: 118_000_000,
                    stop: 136_675_000,
                },
                RangeLabel {
                    span: "430.000-440.000 MHz".into(),
                    start: 430_000_000,
                    stop: 440_000_000,
                },
            ],
            window_range: vec![0, 0, 0, 0, 1, 1, 1, 1, 1, 1],
            window_names: (0..10).map(|i| format!("A{i}")).collect(),
            window_span: (0..10u64)
                .map(|i| match i {
                    0..4 => (
                        118_000_000 + i * 4_700_000,
                        118_000_000 + i * 4_700_000 + 4_690_000,
                    ),
                    _ => {
                        let s = 430_000_000 + (i - 4) * 1_600_000;
                        (s, s + 1_590_000)
                    }
                })
                .collect(),
            names: vec![Some("AMATEUR CALLING FREQ".into()), None],
            channel_db: vec![(145_500_000, "AMATEUR CALLING FREQ".into())],
            digi_db: Vec::new(),
            raster25: Vec::new(),
            channels_path: None,
            channel_freqs: vec![145_500_000, 433_375_000],
            activity: Arc::new(Mutex::new(ActivityLog::default())),
            activity_path: PathBuf::from("rfscan-activity.txt"),
            exclusions: Arc::new(Mutex::new(vec![
                (120_000_000, Some("receiver carrier".into())),
                (136_975_000, None),
            ])),
            exclusions_path: PathBuf::from("rfscan-exclude.txt"),
            muted: Arc::new(AtomicBool::new(false)),
            nr: Arc::new(AtomicBool::new(true)),
            nr_strength: Arc::new(AtomicU32::new(70)),
            underruns: Arc::new(AtomicU64::new(0)),
            digi: None,
            dsd: None,
            dsd_console: None,
            voice: None,
        };
        let mut a = App {
            ctx,
            status: None,
            spectrum: None,
            lock: None,
            history: VecDeque::new(),
            screen: Screen::Main,
            activity_sel: 0,
            exclusions_sel: 0,
            cpu: CpuMeter {
                last: None,
                percent: Some(9.0),
                temp: None,
                ram: Some(48 << 20),
            },
            edit: None,
            confirm_clear: None,
        };
        a.apply(UiEvent::Status(Status {
            state: ScanState::Scanning,
            window: 6,
            visited: 0,
            cycles: 128,
            windows_per_s: 41.2,
            mhz_per_s: 203.4,
            overflows: 0,
            timeouts: 0,
            rejected: 4,
            squelch_snr: 10.0,
            hysteresis: 3.0,
        }));
        let mut snr = [0.0f32; SPECTRUM_BINS];
        snr[40] = 22.0;
        snr[10] = 8.0;
        a.apply(UiEvent::Spectrum(Spectrum {
            window: 6,
            first_hz: 432_806_250,
            last_hz: 434_393_750,
            lo_hz: 433_606_250,
            bins: SPECTRUM_BINS,
            snr_db: snr,
            excluded: [false; SPECTRUM_BINS],
            noise_db: -88.1,
            strongest: Some((433_612_500, 22.0)),
        }));
        for (ch, freq) in [(0usize, 145_500_000u64), (1, 433_375_000)] {
            a.apply(UiEvent::History(crate::events::HistoryItem {
                at: chrono::Local::now(),
                channel: ch,
                freq,
                mode: Modulation::Nfm,
                peak_snr_db: 35.3,
                peak_rssi_db: -53.4,
                duration: Duration::from_millis(4300),
            }));
        }
        a
    }

    fn lock() -> UiEvent {
        UiEvent::Lock(LockView {
            channel: 0,
            freq: 145_500_000,
            mode: Modulation::Nfm,
            bw: 12_500.0,
            rssi_db: -53.8,
            noise_db: -88.2,
            peak_rssi_db: -53.4,
            peak_snr_db: 35.3,
            start: Instant::now(),
            hang: None,
            agc_db: Some(5.6),
        })
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut t = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| view::draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders the main screens at the typical sizes (printed with `--nocapture` for review).
    #[test]
    fn layouts_fit() {
        let mut a = app();
        let scan = render(&a, 100, 30);
        println!("{scan}\n");
        assert!(scan.contains("SCANNING") && scan.contains("430.000-440.000 MHz"));
        let row = |s: &str| s.lines().position(|l| l.contains("detection history"));
        let scan_small = render(&a, 80, 24);
        // Pause: the message appears above SCANNING, which stays in place.
        let big_row = |s: &str| s.lines().position(|l| l.contains("██████ ██████ ██████"));
        let mut paused = app();
        if let Some(st) = paused.status.as_mut() {
            st.state = ScanState::Paused;
        }
        let p = render(&paused, 100, 30);
        println!("{p}\n");
        assert_eq!(big_row(&scan), big_row(&p));
        assert!(p.contains("PAUSED: press [p] to resume"));
        // Label popup on the activity screen.
        paused.screen = Screen::Activity;
        paused.edit = Some(LabelEdit {
            kind: EditKind::Label {
                digi: false,
                was: false,
            },
            freq: 145_500_000,
            text: "CALLING".into(),
            error: None,
        });
        paused.edit_key(KeyCode::Tab);
        let pop = render(&paused, 100, 30);
        println!("{pop}\n");
        assert!(pop.contains("label: 145.500 MHz") && pop.contains("> CALLING"));
        assert!(pop.contains("digi: [x]") && pop.contains("[Tab] digi"));
        paused.edit = None;
        paused.confirm_clear = Some(None);
        let confirm = render(&paused, 100, 30);
        println!("{confirm}\n");
        assert!(confirm.contains("Are you sure? [y]es / [n]o"));
        // Exclusion note popup and the list with notes.
        paused.confirm_clear = None;
        paused.screen = Screen::Exclusions;
        paused.edit = Some(LabelEdit {
            kind: EditKind::Exclude {
                channel: Some(0),
                wide: true,
            },
            freq: 145_500_000,
            text: "interference".into(),
            error: None,
        });
        let note = render(&paused, 100, 30);
        println!("{note}\n");
        assert!(note.contains("exclude with adjacent channels: 145.500 MHz"));
        assert!(note.contains("note: interference") && note.contains("receiver carrier"));
        // Enter sends the exclusion with the note; an empty note is no note.
        assert_eq!(
            paused.edit_key(KeyCode::Enter),
            Some(Command::Exclude {
                freq: 145_500_000,
                channel: Some(0),
                wide: true,
                note: Some("interference".into())
            })
        );
        assert!(paused.edit.is_none());
        paused.edit = Some(LabelEdit {
            kind: EditKind::Exclude {
                channel: None,
                wide: false,
            },
            freq: 433_375_000,
            text: "  ".into(),
            error: None,
        });
        assert_eq!(
            paused.edit_key(KeyCode::Enter),
            Some(Command::Exclude {
                freq: 433_375_000,
                channel: None,
                wide: false,
                note: None
            })
        );
        a.apply(lock());
        let big = render(&a, 100, 30);
        // Fixed slots: the large text starts on the same row on both screens, and the
        // measurement line does not move whether the channel has a name or not.
        let first_big = |s: &str| s.lines().skip(3).position(|l| l.contains("██"));
        assert_eq!(first_big(&scan), first_big(&big));
        let snr_row = |s: &str| {
            s.lines()
                .position(|l| l.contains("SNR:") && l.contains("RSSI:"))
        };
        let mut unnamed = app();
        if let UiEvent::Lock(mut l) = lock() {
            l.channel = 1;
            unnamed.apply(UiEvent::Lock(l));
        }
        let no_name = render(&unnamed, 100, 30);
        println!("{no_name}\n");
        assert_eq!(snr_row(&big), snr_row(&no_name));
        assert_eq!(first_big(&big), first_big(&no_name));
        assert_eq!(
            snr_row(&render(&a, 80, 24)),
            snr_row(&render(&unnamed, 80, 24))
        );
        // The history does not move between the scanning and the lock view.
        assert_eq!(row(&scan), row(&big));
        assert_eq!(row(&scan_small), row(&render(&a, 80, 24)));
        println!("{big}\n");
        assert!(big.contains("██████") && big.contains("AMATEUR CALLING FREQ"));
        let compact = render(&a, 80, 24);
        println!("{compact}\n");
        assert!(compact.contains("145.500 MHz") && !compact.contains("██   ██"));
        // The position bar takes the free width of a large window too.
        let wide140 = render(&a, 140, 30);
        println!("{wide140}\n");
        assert!(wide140.contains("pass: A   window: 6   cycle: 128 │"));
        // On a digi channel the DSD-FME console covers the history.
        let console: crate::dsd_fme::ConsoleLines = Arc::default();
        if let Ok(mut l) = console.lock() {
            for i in 0..20 {
                l.push_back(format!("line {i}"));
            }
            l.push_back(" SLOT 1 TGT=1234 SRC=567 Group Call".into());
        }
        a.ctx.dsd_console = Some(console);
        let analog = a.lock;
        a.lock = analog.map(|l| LockView {
            mode: crate::plan::Modulation::Digi,
            ..l
        });
        let digi = render(&a, 100, 30);
        println!("{digi}\n");
        assert!(digi.contains("DSD-FME console") && digi.contains("TGT=1234 SRC=567"));
        assert!(!digi.contains("detection history") && !digi.contains("line 0 "));
        a.lock = analog;
        a.ctx.dsd_console = None;
        // Fast scanning: every window measured since the last refresh is lit, no number.
        let exact = a.status;
        for visited in [3, 10] {
            a.status = exact.map(|s| Status { visited, ..s });
            let sweep = render(&a, 100, 30);
            println!("{}\n", sweep.lines().nth(1).unwrap_or(""));
            assert!(sweep.contains("pass: -   window: -   cycle: 128"));
        }
        a.status = exact;
        let small = render(&a, 70, 20);
        println!("{small}\n");
        assert!(small.contains("too small"));
        a.screen = Screen::Exclusions;
        let excl = render(&a, 80, 24);
        println!("{excl}");
        // Airband: the channel number (136.975 MHz is channel 136.980 on the 8.33 kHz raster).
        assert!(excl.contains("136.980"));
        // Lock view on an airband channel: the channel number as the headline, the actual
        // frequency below; a 25 kHz channel keeps its number.
        let mut air = app();
        if let UiEvent::Lock(mut l) = lock() {
            l.freq = 118_033_333;
            l.mode = Modulation::Am;
            l.bw = 25_000.0 / 3.0;
            air.apply(UiEvent::Lock(l));
        }
        let air_view = render(&air, 100, 30);
        println!("{air_view}\n");
        assert!(air_view.contains("frequency: 118.0333 MHz  (8.33 kHz channel)"));
        assert!(render(&air, 80, 24).contains("118.035 MHz"));
        air.ctx.raster25 = vec![121_500_000];
        if let UiEvent::Lock(mut l) = lock() {
            l.freq = 121_500_000;
            air.apply(UiEvent::Lock(l));
        }
        let guard = render(&air, 80, 24);
        assert!(guard.contains("121.500 MHz") && guard.contains("(25 kHz channel)"));
        // Activity screen: the mode of the last activity, DIGI for a digi channel file entry.
        let mut act = app();
        if let Ok(mut log) = act.ctx.activity.lock() {
            log.hit(145_500_000, Modulation::Nfm);
            log.add_seconds(145_500_000, 12.0);
            log.hit(438_800_000, Modulation::Nfm);
        }
        act.ctx.digi_db = vec![438_800_000];
        act.screen = Screen::Activity;
        let screen = render(&act, 80, 24);
        println!("{screen}\n");
        assert!(screen.contains("mode") && screen.contains("145.500  NFM"));
        assert!(screen.contains("438.800  DIGI"));
    }
}
