//! Audio recording of the channels marked `rec` in the channel file (`--record-dir`).
//!
//! A thread of its own drains the recording tap of the audio mixer (the mono mix, only the frames
//! with audio, so the silence of the Hang state is not there), encodes it like the web GUI audio
//! (band-pass, 8 kHz, G.711 μ-law) and writes one WAV file per activity into a subdirectory of
//! the channel, e.g. `145.550-Xyz_Cvz/20261025-143012.wav`. The scanner only says where an
//! activity begins and ends (`Link`, `try_send` of a `Copy` message); whether its channel is
//! recorded is read from the shared `rec` list on every tick, so switching it applies at once.
//! The scanner and DSP threads never wait for the disk.
//!
//! The WAV header sizes are updated every few seconds, so a file left by a crash still plays.
//! Over 90% disk usage the file is closed and recording pauses; it resumes by itself below 90%.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use crate::web::{MULAW_RATE, MulawEncoder};

/// How often the thread drains the tap and the messages.
const TICK: Duration = Duration::from_millis(20);
/// How often the WAV header sizes are written.
const HEADER_EVERY: Duration = Duration::from_secs(2);
/// How often the disk usage is checked while writing or paused.
const DISK_EVERY: Duration = Duration::from_secs(10);
/// Disk usage above which recording pauses.
pub const DISK_LIMIT: f64 = 0.9;
/// After an activity ends, the file is closed once no audio came for this long (the audio still
/// in the buffers belongs to it), at the latest after `CLOSE_MAX`.
const CLOSE_GAP: Duration = Duration::from_millis(200);
const CLOSE_MAX: Duration = Duration::from_millis(1500);
/// Matching tolerance of channel file entries (as on the activity screen).
const TOLERANCE_HZ: u64 = 4_000;
/// Messages queued from the scanner.
const QUEUE: usize = 64;
/// WAV header length: RIFF, `fmt ` (18 bytes, μ-law), `fact`, `data`.
const HEADER_LEN: u32 = 58;

/// Frequencies of the channel file entries marked `rec`, shared by the interfaces (which switch
/// them) and the recorder.
pub type RecList = Arc<Mutex<Vec<u64>>>;

/// Whether `freq` is a channel marked `rec`: an entry within the matching tolerance.
pub fn is_rec(list: &[u64], freq: u64) -> bool {
    list.iter().any(|&f| f.abs_diff(freq) <= TOLERANCE_HZ)
}

/// Recording state shown by the interfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecState {
    On = 0,
    /// The disk of `--record-dir` is over `DISK_LIMIT` full.
    Paused = 1,
}

impl RecState {
    pub fn load(a: &AtomicU8) -> Self {
        match a.load(Ordering::Relaxed) {
            0 => RecState::On,
            _ => RecState::Paused,
        }
    }
}

/// State shared with the interfaces.
pub struct Shared {
    pub state: AtomicU8,
    /// A file is being written (the `REC` marker).
    pub writing: AtomicBool,
}

/// A message from the scanner.
#[derive(Clone, Copy, Debug)]
enum Msg {
    /// An activity begins on the channel `freq` (`raster25`: an airband 25 kHz channel).
    Begin { freq: u64, raster25: bool },
    /// The activity ends.
    End,
}

/// A message for the log.
pub enum Event {
    Info(String),
    Warning(String),
}

/// The scanner's side: activity boundaries in, log messages out.
pub struct Link {
    tx: Sender<Msg>,
    pub events: Receiver<Event>,
}

impl Link {
    pub fn begin(&self, freq: u64, raster25: bool) {
        let _ = self.tx.try_send(Msg::Begin { freq, raster25 });
    }

    pub fn end(&self) {
        let _ = self.tx.try_send(Msg::End);
    }
}

pub struct Config {
    /// `--record-dir`: it must exist.
    pub dir: PathBuf,
    /// Sample rate of the tap (the audio output rate).
    pub rate: f64,
    /// The channel file (names for the subdirectories).
    pub channels_path: Option<PathBuf>,
    /// Disk usage above which recording pauses (`DISK_LIMIT`).
    pub disk_limit: f64,
    /// The list of recordings: every closed file is added, the open one cannot be deleted.
    pub library: Option<crate::recordings::SharedLibrary>,
}

/// The running recorder; dropping it closes the open file and ends the thread.
pub struct Recorder {
    pub link: Link,
    pub shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Closes the open file, ends the thread and returns the messages not read yet.
    pub fn finish(mut self) -> Vec<Event> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.link.events.try_iter().collect()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts the recording thread.
pub fn start(cfg: Config, tap: rtrb::Consumer<f32>, list: RecList) -> Result<Recorder, String> {
    let (tx, rx) = crossbeam_channel::bounded(QUEUE);
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let shared = Arc::new(Shared {
        state: AtomicU8::new(RecState::On as u8),
        writing: AtomicBool::new(false),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let mut worker = Worker {
        encoder: MulawEncoder::new(cfg.rate),
        cfg,
        tap,
        list,
        shared: shared.clone(),
        events: ev_tx,
        current: None,
        file: None,
        ending: None,
        last_audio: Instant::now(),
        failed: false,
        next_disk: Instant::now(),
        bytes: Vec::with_capacity(4096),
    };
    let flag = stop.clone();
    let thread = std::thread::Builder::new()
        .name("rfscan-recorder".into())
        .spawn(move || worker.run(&rx, &flag))
        .map_err(|e| format!("cannot start the recording thread: {e}"))?;
    Ok(Recorder {
        link: Link { tx, events: ev_rx },
        shared,
        stop,
        thread: Some(thread),
    })
}

struct Worker {
    cfg: Config,
    tap: rtrb::Consumer<f32>,
    list: RecList,
    shared: Arc<Shared>,
    events: Sender<Event>,
    encoder: MulawEncoder,
    /// The channel of the current activity.
    current: Option<(u64, bool)>,
    file: Option<Wav>,
    /// The activity ended while a file was open: when.
    ending: Option<Instant>,
    last_audio: Instant,
    /// Opening or writing failed in this activity: no new file until the next one.
    failed: bool,
    next_disk: Instant,
    /// Encoded bytes of one tick.
    bytes: Vec<u8>,
}

impl Worker {
    fn info(&self, m: String) {
        let _ = self.events.send(Event::Info(m));
    }

    fn warn(&self, m: String) {
        let _ = self.events.send(Event::Warning(m));
    }

    fn run(&mut self, rx: &Receiver<Msg>, stop: &AtomicBool) {
        loop {
            let now = Instant::now();
            loop {
                match rx.try_recv() {
                    Ok(Msg::Begin { freq, raster25 }) => {
                        self.close();
                        self.current = Some((freq, raster25));
                        self.failed = false;
                    }
                    Ok(Msg::End) => {
                        self.current = None;
                        if self.file.is_some() {
                            self.ending.get_or_insert(now);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.close();
                        return;
                    }
                }
            }
            if stop.load(Ordering::Relaxed) {
                self.drain(now, false);
                self.close();
                return;
            }
            let on = RecState::load(&self.shared.state) == RecState::On;
            let wanted = on
                && !self.failed
                && self
                    .current
                    .is_some_and(|(f, _)| self.list.lock().is_ok_and(|l| is_rec(&l, f)));
            // `rec` switched off (or the disk paused it) during the activity.
            if self.file.is_some() && self.ending.is_none() && !wanted {
                self.close();
            }
            self.drain(now, wanted);
            let ended = self.ending.is_some_and(|end| {
                now.saturating_duration_since(self.last_audio.max(end)) >= CLOSE_GAP
                    || now.saturating_duration_since(end) >= CLOSE_MAX
            });
            if ended {
                self.close();
            }
            if let Some(w) = &mut self.file
                && now.saturating_duration_since(w.header_at) >= HEADER_EVERY
            {
                w.header_at = now;
                if let Err(e) = w.update_header() {
                    let _ = self.events.send(Event::Warning(format!(
                        "recording {}: {e}",
                        w.path.display()
                    )));
                    self.file = None;
                    self.failed = true;
                    self.release();
                }
            }
            if now >= self.next_disk && (self.file.is_some() || !on) {
                self.next_disk = now + DISK_EVERY;
                self.check_disk(on);
            }
            self.shared
                .writing
                .store(self.file.is_some(), Ordering::Relaxed);
            std::thread::sleep(TICK);
        }
    }

    /// Drains the tap: into the open file, or into a new one when the channel is recorded;
    /// otherwise the samples are dropped.
    fn drain(&mut self, now: Instant, wanted: bool) {
        let n = self.tap.slots();
        if n == 0 {
            return;
        }
        self.last_audio = now;
        if self.file.is_none() && wanted && self.ending.is_none() {
            self.open();
        }
        let Ok(chunk) = self.tap.read_chunk(n) else {
            return;
        };
        if self.file.is_none() {
            chunk.commit_all();
            return;
        }
        self.bytes.clear();
        let (a, b) = chunk.as_slices();
        for &x in a.iter().chain(b) {
            self.encoder.push(x, |u| self.bytes.push(u));
        }
        chunk.commit_all();
        if let Some(w) = &mut self.file
            && let Err(e) = w.write(&self.bytes)
        {
            let _ = self.events.send(Event::Warning(format!(
                "recording {}: {e}",
                w.path.display()
            )));
            self.file = None;
            self.failed = true;
            self.release();
        }
    }

    /// Opens the file of the current activity (after checking the disk).
    fn open(&mut self) {
        let Some((freq, raster25)) = self.current else {
            return;
        };
        if !self.check_disk(true) {
            return;
        }
        let entry = self
            .cfg
            .channels_path
            .as_deref()
            .and_then(|p| crate::channel_db::load(p).ok())
            .and_then(|(entries, _)| {
                crate::channel_db::nearest(&entries, freq, TOLERANCE_HZ as f64).cloned()
            });
        let raster25 = raster25 || entry.as_ref().is_some_and(|e| e.raster25);
        let name = entry.and_then(|e| e.name);
        let dir = self.cfg.dir.join(dir_name(freq, raster25, name.as_deref()));
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        match std::fs::create_dir_all(&dir).and_then(|()| Wav::create_in(&dir, &stamp)) {
            Ok(w) => {
                self.info(format!("REC       {}", w.path.display()));
                self.encoder = MulawEncoder::new(self.cfg.rate);
                if let Some(lib) = &self.cfg.library
                    && let Ok(mut l) = lib.lock()
                {
                    l.writing = Some(w.path.clone());
                }
                self.file = Some(w);
            }
            Err(e) => {
                self.warn(format!("cannot record into {}: {e}", dir.display()));
                self.failed = true;
            }
        }
    }

    /// Closes the open file, if any.
    fn close(&mut self) {
        self.ending = None;
        if let Some(w) = self.file.take() {
            let path = w.path.clone();
            let seconds = w.data as f64 / MULAW_RATE;
            match w.finish() {
                Ok(()) => self.info(format!("REC END   {} ({seconds:.1} s)", path.display())),
                Err(e) => self.warn(format!("recording {}: {e}", path.display())),
            }
            if let Some(lib) = &self.cfg.library
                && let Ok(mut l) = lib.lock()
            {
                l.add(&path);
            }
        }
        self.release();
        self.shared.writing.store(false, Ordering::Relaxed);
    }

    /// No file is being written any more (also after a write error): it may be deleted.
    fn release(&self) {
        if let Some(lib) = &self.cfg.library
            && let Ok(mut l) = lib.lock()
        {
            l.writing = None;
        }
    }

    /// Checks the disk usage of `--record-dir`: over the limit recording pauses (the open file
    /// is closed), below it a paused recording resumes. Returns whether recording may go on.
    fn check_disk(&mut self, on: bool) -> bool {
        let usage = match fs4::statvfs(&self.cfg.dir) {
            Ok(s) if s.total_space() > 0 => {
                1.0 - s.available_space() as f64 / s.total_space() as f64
            }
            // The usage is unknown: recording goes on, writing reports its own errors.
            _ => return on,
        };
        let limit = self.cfg.disk_limit;
        if usage > limit {
            if on {
                self.close();
                self.shared
                    .state
                    .store(RecState::Paused as u8, Ordering::Relaxed);
                self.next_disk = Instant::now() + DISK_EVERY;
                self.warn(format!(
                    "the disk of --record-dir {} is {:.0}% full: recording paused until it is below {:.0}%",
                    self.cfg.dir.display(),
                    usage * 100.0,
                    limit * 100.0
                ));
            }
            false
        } else {
            if !on {
                self.shared
                    .state
                    .store(RecState::On as u8, Ordering::Relaxed);
                self.info(format!(
                    "recording resumed: the disk of --record-dir is {:.0}% full",
                    usage * 100.0
                ));
            }
            true
        }
    }
}

/// Subdirectory of a channel: the frequency as shown (airband: the channel number), then `-`
/// and the name made safe for file names: transliterated to ASCII, spaces as `_`, without the
/// characters Windows does not allow.
pub fn dir_name(freq: u64, raster25: bool, name: Option<&str>) -> String {
    let mut s = crate::airband::display(freq, raster25);
    let safe: String = name
        .map(|n| deunicode::deunicode_with_tofu(n, ""))
        .unwrap_or_default()
        .chars()
        .filter_map(|c| match c {
            c if c.is_whitespace() => Some('_'),
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    // Windows does not allow a name ending with a dot.
    let safe = safe.trim_matches(['_', '.']);
    if !safe.is_empty() {
        s.push('-');
        s.push_str(safe);
    }
    s
}

/// A WAV file being written: 8 kHz mono G.711 μ-law.
struct Wav {
    file: BufWriter<File>,
    path: PathBuf,
    /// Bytes (samples) in the data chunk.
    data: u32,
    header_at: Instant,
}

impl Wav {
    /// A new file `STAMP.wav` in `dir` (`STAMP-2.wav` etc. when it exists).
    fn create_in(dir: &Path, stamp: &str) -> std::io::Result<Wav> {
        let mut n = 1;
        loop {
            let path = if n == 1 {
                dir.join(format!("{stamp}.wav"))
            } else {
                dir.join(format!("{stamp}-{n}.wav"))
            };
            match File::create_new(&path) {
                Ok(f) => {
                    let mut w = Wav {
                        file: BufWriter::new(f),
                        path,
                        data: 0,
                        header_at: Instant::now(),
                    };
                    w.file.write_all(&header(0, false))?;
                    return Ok(w);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        // A WAV file holds at most 4 GB (about 6 days of audio here): the rest is dropped.
        let room = (u32::MAX - HEADER_LEN - 1 - self.data) as usize;
        let bytes = &bytes[..bytes.len().min(room)];
        self.file.write_all(bytes)?;
        self.data += bytes.len() as u32;
        Ok(())
    }

    /// Writes the current sizes into the header (the file plays after a crash).
    fn update_header(&mut self) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.data, false))?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.flush()
    }

    /// The final header with the pad byte of an odd data chunk.
    fn finish(mut self) -> std::io::Result<()> {
        let pad = self.data % 2 == 1;
        if pad {
            self.file.write_all(&[0])?;
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header(self.data, pad))?;
        self.file.flush()
    }
}

/// WAV header of `data` bytes of 8 kHz mono μ-law (format 7, with the `fact` chunk required for
/// a non-PCM format); `pad`: a pad byte follows the data.
fn header(data: u32, pad: bool) -> [u8; HEADER_LEN as usize] {
    let rate = MULAW_RATE as u32;
    let mut h = [0u8; HEADER_LEN as usize];
    let mut put = |at: usize, b: &[u8]| h[at..at + b.len()].copy_from_slice(b);
    put(0, b"RIFF");
    put(4, &(HEADER_LEN - 8 + data + u32::from(pad)).to_le_bytes());
    put(8, b"WAVE");
    put(12, b"fmt ");
    put(16, &18u32.to_le_bytes());
    put(20, &7u16.to_le_bytes()); // WAVE_FORMAT_MULAW
    put(22, &1u16.to_le_bytes()); // mono
    put(24, &rate.to_le_bytes());
    put(28, &rate.to_le_bytes()); // bytes per second
    put(32, &1u16.to_le_bytes()); // block align
    put(34, &8u16.to_le_bytes()); // bits per sample
    put(36, &0u16.to_le_bytes()); // no extra format bytes
    put(38, b"fact");
    put(42, &4u32.to_le_bytes());
    put(46, &data.to_le_bytes());
    put(50, b"data");
    put(54, &data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_names() {
        assert_eq!(dir_name(145_550_000, false, None), "145.550");
        assert_eq!(
            dir_name(145_550_000, false, Some("Xyz Cvz")),
            "145.550-Xyz_Cvz"
        );
        assert_eq!(
            dir_name(433_450_000, false, Some("Łódź: \"wieża\"/R1?")),
            "433.450-Lodz_wiezaR1"
        );
        assert_eq!(dir_name(433_450_000, false, Some(" ... ")), "433.450");
        // Airband: the 8.33 kHz channel number.
        assert_eq!(dir_name(118_008_333, false, Some("TWR")), "118.010-TWR");
    }

    #[test]
    fn rec_list_tolerance() {
        let list = [145_550_000];
        assert!(is_rec(&list, 145_550_000));
        assert!(is_rec(&list, 145_553_000));
        assert!(!is_rec(&list, 145_562_500));
    }

    /// The thread writes one file for an activity on a `rec` channel, in the channel's
    /// subdirectory, and nothing for another channel.
    #[test]
    fn records_one_activity() {
        let dir = std::env::temp_dir().join(format!("rfscan-recorder-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut tx, rx) = rtrb::RingBuffer::new(48_000);
        let r = start(
            Config {
                dir: dir.clone(),
                rate: 48_000.0,
                channels_path: None,
                // The test must not depend on the disk usage of this computer.
                disk_limit: 1.0,
                library: None,
            },
            rx,
            Arc::new(Mutex::new(vec![145_550_000])),
        )
        .unwrap();
        let feed = |tx: &mut rtrb::Producer<f32>| {
            // 0.5 s of a 1 kHz tone in 50 ms blocks.
            for k in 0..10 {
                for i in 0..2_400 {
                    let t = (k * 2_400 + i) as f32 / 48_000.0;
                    let _ = tx.push(0.3 * (std::f32::consts::TAU * 1_000.0 * t).sin());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        r.link.begin(145_550_000, false);
        feed(&mut tx);
        r.link.end();
        std::thread::sleep(Duration::from_millis(400));
        assert!(!r.shared.writing.load(Ordering::Relaxed));
        r.link.begin(145_562_500, false);
        feed(&mut tx);
        r.link.end();
        let events = r.finish();
        assert!(events.iter().all(|e| matches!(e, Event::Info(_))));
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["145.550"]);
        let files: Vec<_> = std::fs::read_dir(dir.join("145.550"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        // About 0.5 s at 8 kHz (one byte per sample) after the resampler.
        let len = std::fs::metadata(&files[0]).unwrap().len();
        assert!((58 + 3_900..=58 + 4_100).contains(&len), "{len}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file of a few samples: sizes in the header, the pad byte after an odd data chunk.
    #[test]
    fn wav_file() {
        let dir = std::env::temp_dir().join(format!("rfscan-recorder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = Wav::create_in(&dir, "20261025-143012").unwrap();
        w.write(&[0xFF; 5]).unwrap();
        w.update_header().unwrap();
        w.write(&[0x7F; 4]).unwrap();
        let path = w.path.clone();
        w.finish().unwrap();
        let second = Wav::create_in(&dir, "20261025-143012").unwrap();
        assert!(second.path.ends_with("20261025-143012-2.wav"));
        drop(second);
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 58 + 9 + 1);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 50 + 9 + 1);
        assert_eq!(u16::from_le_bytes(b[20..22].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8_000);
        assert_eq!(u32::from_le_bytes(b[46..50].try_into().unwrap()), 9);
        assert_eq!(&b[50..54], b"data");
        assert_eq!(u32::from_le_bytes(b[54..58].try_into().unwrap()), 9);
        assert_eq!(b[58 + 9], 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
