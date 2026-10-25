//! The recordings of `--record-dir`: the list per channel, deleting, and playback in the TUI.
//!
//! The list is built at startup by reading the channel subdirectories and their `.wav` files;
//! the recorder adds every file it closes, so the directory is not read again. Channels are the
//! frequencies of the subdirectory names (`145.550-Name`, in the airband the channel number), so
//! the subdirectories of a renamed channel are one channel. Deleting accepts only files of the
//! list (identifiers, never paths from outside), never the file being recorded, and removes a
//! channel subdirectory that became empty.
//!
//! Playback (TUI): a thread of its own decodes the μ-law file, resamples it to the audio output
//! rate and feeds a ring buffer that the mixer plays instead of the scanner audio while
//! `active` is set; scanning and recording go on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::NaiveDateTime;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};

use crate::demod::Resampler;
use crate::web::MULAW_RATE;

/// Files listed per channel (the newest); older ones stay only on the disk.
pub const LIST_MAX: usize = 500;
/// Header length of the files written by the recorder (`recorder::header`).
const HEADER_LEN: usize = 58;

/// One recording.
#[derive(Clone, Debug)]
pub struct RecFile {
    pub id: u64,
    pub path: PathBuf,
    /// Start time from the file name (`YYYYMMDD-HHMMSS.wav`).
    pub at: Option<NaiveDateTime>,
    /// Length from the WAV header of an 8 kHz μ-law file, `None` for other files.
    pub seconds: Option<f64>,
    pub bytes: u64,
}

/// The recordings of one channel (one or more subdirectories with the same frequency).
#[derive(Clone, Debug)]
pub struct RecChannel {
    pub id: u64,
    /// Frequency from the subdirectory name; `None` when it does not start with one.
    pub freq: Option<u64>,
    /// Airband 25 kHz channel number in the subdirectory name.
    pub raster25: bool,
    /// The newest subdirectory name (the name part after `-` is shown for unnamed channels).
    pub dir_name: String,
    /// The newest first.
    pub files: Vec<RecFile>,
}

impl RecChannel {
    pub fn last(&self) -> Option<NaiveDateTime> {
        self.files.iter().filter_map(|f| f.at).max()
    }

    pub fn total_seconds(&self) -> f64 {
        self.files.iter().filter_map(|f| f.seconds).sum()
    }

    /// The name part of the subdirectory (`_` as spaces), if any.
    pub fn dir_label(&self) -> Option<String> {
        let rest = match self.freq {
            Some(_) => self.dir_name.split_once('-').map(|(_, n)| n)?,
            None => self.dir_name.as_str(),
        };
        (!rest.is_empty()).then(|| rest.replace('_', " "))
    }
}

/// All recordings of `--record-dir`.
pub struct Library {
    root: PathBuf,
    channels: Vec<RecChannel>,
    next_id: u64,
    /// The file being recorded (cannot be deleted).
    pub writing: Option<PathBuf>,
}

pub type SharedLibrary = Arc<Mutex<Library>>;

/// Frequency of a subdirectory name: the number before the first `-` (an airband value with
/// three decimal places is a channel number).
fn dir_freq(name: &str) -> Option<(u64, bool)> {
    let text = name.split('-').next()?;
    let mhz: f64 = text.parse().ok()?;
    if !mhz.is_finite() || mhz <= 0.0 {
        return None;
    }
    crate::airband::from_text(text, (mhz * 1e6).round() as u64).ok()
}

/// Start time from a file name `YYYYMMDD-HHMMSS[-N].wav`.
fn file_time(path: &Path) -> Option<NaiveDateTime> {
    let stem = path.file_stem()?.to_str()?;
    NaiveDateTime::parse_from_str(stem.get(..15)?, "%Y%m%d-%H%M%S").ok()
}

/// Length of an 8 kHz mono μ-law WAV file as written by the recorder.
fn wav_seconds(path: &Path) -> Option<f64> {
    use std::io::Read;
    let mut h = [0u8; HEADER_LEN];
    std::fs::File::open(path).ok()?.read_exact(&mut h).ok()?;
    let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
    let ours = &h[0..4] == b"RIFF"
        && &h[8..12] == b"WAVE"
        && u16_at(20) == 7
        && u16_at(22) == 1
        && &h[50..54] == b"data";
    ours.then(|| u32_at(54) as f64 / u32_at(24).max(1) as f64)
}

fn is_wav(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
}

impl Library {
    /// Reads the channel subdirectories of `root` and their `.wav` files.
    pub fn scan(root: &Path) -> Self {
        let mut lib = Library {
            root: root.to_path_buf(),
            channels: Vec::new(),
            next_id: 1,
            writing: None,
        };
        let Ok(dirs) = std::fs::read_dir(root) else {
            return lib;
        };
        let mut paths: Vec<PathBuf> = dirs
            .flatten()
            .map(|d| d.path())
            .filter(|p| p.is_dir())
            .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
            .map(|f| f.path())
            .filter(|p| p.is_file() && is_wav(p))
            .collect();
        // Oldest first, so the newest subdirectory name of a channel wins.
        paths.sort_by_key(|p| file_time(p));
        for p in paths {
            lib.add(&p);
        }
        lib
    }

    /// Adds a file (a closed recording); ignored outside the channel subdirectories.
    pub fn add(&mut self, path: &Path) {
        let Some(dir) = path.parent() else { return };
        if dir.parent() != Some(self.root.as_path()) && !same_dir(dir.parent(), &self.root) {
            return;
        }
        let Some(dir_name) = dir.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let parsed = dir_freq(dir_name);
        let file = RecFile {
            id: self.next_id,
            path: path.to_path_buf(),
            at: file_time(path),
            seconds: wav_seconds(path),
            bytes: std::fs::metadata(path).map_or(0, |m| m.len()),
        };
        self.next_id += 1;
        let pos = self.channels.iter().position(|c| match (parsed, c.freq) {
            (Some((f, _)), Some(cf)) => f == cf,
            (None, None) => c.dir_name == dir_name,
            _ => false,
        });
        let ch = match pos {
            Some(i) => &mut self.channels[i],
            None => {
                self.channels.push(RecChannel {
                    id: self.next_id,
                    freq: parsed.map(|p| p.0),
                    raster25: parsed.is_some_and(|p| p.1),
                    dir_name: dir_name.to_string(),
                    files: Vec::new(),
                });
                self.next_id += 1;
                let last = self.channels.len() - 1;
                &mut self.channels[last]
            }
        };
        if file.at >= ch.last() {
            ch.dir_name = dir_name.to_string();
        }
        ch.files.push(file);
        ch.files
            .sort_by(|a, b| b.at.cmp(&a.at).then(b.id.cmp(&a.id)));
    }

    /// Channels, the most recent recording first.
    pub fn channels(&self) -> Vec<&RecChannel> {
        let mut v: Vec<&RecChannel> = self.channels.iter().collect();
        v.sort_by(|a, b| b.last().cmp(&a.last()).then(b.id.cmp(&a.id)));
        v
    }

    pub fn channel(&self, id: u64) -> Option<&RecChannel> {
        self.channels.iter().find(|c| c.id == id)
    }

    /// The listed files of a channel: at most `LIST_MAX`, the newest first.
    pub fn files(&self, channel: u64) -> &[RecFile] {
        self.channel(channel)
            .map_or(&[][..], |c| &c.files[..c.files.len().min(LIST_MAX)])
    }

    pub fn file(&self, id: u64) -> Option<&RecFile> {
        self.channels
            .iter()
            .flat_map(|c| &c.files)
            .find(|f| f.id == id)
    }

    /// Deletes a listed file (not the one being recorded) and its subdirectory when it became
    /// empty. Returns the path of the deleted file.
    pub fn delete(&mut self, id: u64) -> Result<PathBuf, String> {
        let Some((ci, fi)) = self
            .channels
            .iter()
            .enumerate()
            .find_map(|(ci, c)| c.files.iter().position(|f| f.id == id).map(|fi| (ci, fi)))
        else {
            return Err("no such recording".into());
        };
        let path = self.channels[ci].files[fi].path.clone();
        if self.writing.as_deref() == Some(path.as_path()) {
            return Err("this file is being recorded".into());
        }
        if !is_wav(&path) || !path.starts_with(&self.root) {
            return Err("not a recording".into());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot delete {}: {e}", path.display())),
        }
        // `remove_dir` removes only an empty directory.
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        let ch = &mut self.channels[ci];
        ch.files.remove(fi);
        if ch.files.is_empty() {
            self.channels.remove(ci);
        }
        Ok(path)
    }
}

/// Whether `a` is the directory `b` (also when one of them is written differently).
fn same_dir(a: Option<&Path>, b: &Path) -> bool {
    match (a.and_then(|a| a.canonicalize().ok()), b.canonicalize().ok()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// G.711 μ-law decoding to a sample in ±1 (the inverse of `web::mulaw`).
pub fn ulaw_decode(u: u8) -> f32 {
    let u = !u;
    let exponent = (u >> 4) & 7;
    let mantissa = (u & 0x0F) as i32;
    let mag = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    (if u & 0x80 != 0 { -mag } else { mag }) as f32 / 32768.0
}

/// Offset and length of the `data` chunk of a WAV file.
fn data_chunk(b: &[u8]) -> Option<(usize, usize)> {
    if b.get(0..4)? != b"RIFF" || b.get(8..12)? != b"WAVE" {
        return None;
    }
    let mut at = 12;
    while at + 8 <= b.len() {
        let id = &b[at..at + 4];
        let len = u32::from_le_bytes(b[at + 4..at + 8].try_into().ok()?) as usize;
        if id == b"data" {
            return Some((at + 8, len.min(b.len() - at - 8)));
        }
        at += 8 + len + (len & 1);
    }
    None
}

enum PlayCmd {
    Play { id: u64, path: PathBuf },
    Stop,
}

/// Playback of recordings on the local audio output (TUI).
pub struct Player {
    tx: Sender<PlayCmd>,
    /// The file playing (its id), 0 when none.
    playing: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl Player {
    /// Starts the player thread: `ring` goes to the mixer, which plays it instead of the scanner
    /// audio while `active` is set; `rate` is the audio output rate.
    pub fn start(
        mut ring: rtrb::Producer<f32>,
        active: Arc<AtomicBool>,
        rate: f64,
    ) -> Result<Self, String> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let playing = Arc::new(AtomicU64::new(0));
        let now = playing.clone();
        let thread = std::thread::Builder::new()
            .name("rfscan-player".into())
            .spawn(move || {
                let mut next = None;
                loop {
                    let cmd = match next.take() {
                        Some(c) => c,
                        None => match rx.recv() {
                            Ok(c) => c,
                            Err(_) => return,
                        },
                    };
                    let PlayCmd::Play { id, path } = cmd else {
                        continue;
                    };
                    now.store(id, Ordering::Relaxed);
                    active.store(true, Ordering::Relaxed);
                    next = play(&path, &mut ring, &rx, rate);
                    active.store(false, Ordering::Relaxed);
                    now.store(0, Ordering::Relaxed);
                    if matches!(next, Some(PlayCmd::Stop)) {
                        next = None;
                    }
                }
            })
            .map_err(|e| format!("cannot start the player thread: {e}"))?;
        Ok(Player {
            tx,
            playing,
            thread: Some(thread),
        })
    }

    pub fn play(&self, id: u64, path: PathBuf) {
        let _ = self.tx.send(PlayCmd::Play { id, path });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(PlayCmd::Stop);
    }

    /// The id of the file playing, if any.
    pub fn playing(&self) -> Option<u64> {
        Some(self.playing.load(Ordering::Relaxed)).filter(|&id| id != 0)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let (tx, _) = crossbeam_channel::unbounded();
        // Closing the command channel ends the thread.
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Plays one file into the ring at the pace the mixer takes it. Returns a command that came in
/// meanwhile (a new file or Stop), if any.
fn play(
    path: &Path,
    ring: &mut rtrb::Producer<f32>,
    rx: &Receiver<PlayCmd>,
    rate: f64,
) -> Option<PlayCmd> {
    let bytes = std::fs::read(path).ok()?;
    let (start, len) = data_chunk(&bytes)?;
    let mut rs = Resampler::new(MULAW_RATE, rate);
    let mut pending: Vec<f32> = Vec::with_capacity(16);
    for &u in &bytes[start..start + len] {
        pending.clear();
        rs.push(ulaw_decode(u), |s| pending.push(s));
        for &s in &pending {
            while ring.push(s).is_err() {
                match rx.try_recv() {
                    Ok(c) => return Some(c),
                    Err(TryRecvError::Disconnected) => return Some(PlayCmd::Stop),
                    Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        }
    }
    // The end of the file: wait until the mixer has played the ring.
    let capacity = ring.buffer().capacity();
    while ring.slots() < capacity {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(c) => return Some(c),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, samples: usize) {
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((50 + samples) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&18u32.to_le_bytes());
        for v in [7u16, 1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&8000u32.to_le_bytes());
        b.extend_from_slice(&8000u32.to_le_bytes());
        for v in [1u16, 8, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(b"fact");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&(samples as u32).to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(samples as u32).to_le_bytes());
        b.resize(b.len() + samples, 0xFF);
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn ulaw_round_trip() {
        for x in [-0.9f32, -0.3, -0.01, 0.0, 0.02, 0.5, 0.95] {
            let y = ulaw_decode(crate::web::mulaw(x));
            assert!((y - x).abs() <= 0.03 * x.abs() + 0.002, "{x} -> {y}");
        }
    }

    /// Channels from the subdirectory names (a renamed channel is one channel), the newest
    /// first; deleting removes the empty subdirectory; the file being recorded stays.
    #[test]
    fn list_and_delete() {
        let root = std::env::temp_dir().join(format!("rfscan-recordings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "145.550-Old_name",
            "145.550-New_name",
            "118.010-TWR",
            "misc",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        write_wav(&root.join("145.550-Old_name/20261020-100000.wav"), 8_000);
        write_wav(&root.join("145.550-New_name/20261025-120000.wav"), 4_000);
        write_wav(&root.join("118.010-TWR/20261024-090000.wav"), 16_000);
        write_wav(&root.join("misc/20261023-090000.wav"), 800);
        std::fs::write(root.join("misc/notes.txt"), "x").unwrap();
        let mut lib = Library::scan(&root);
        let chans = lib.channels();
        assert_eq!(chans.len(), 3);
        assert_eq!(chans[0].freq, Some(145_550_000));
        assert_eq!(chans[0].files.len(), 2);
        assert_eq!(chans[0].total_seconds(), 1.5);
        assert_eq!(chans[0].dir_label().as_deref(), Some("New name"));
        assert_eq!(chans[1].freq, Some(118_008_333));
        assert_eq!(chans[2].freq, None);
        assert_eq!(chans[2].dir_label().as_deref(), Some("misc"));
        let (old, new) = (chans[0].files[1].id, chans[0].files[0].id);
        let ch = chans[0].id;
        lib.writing = Some(lib.file(new).unwrap().path.clone());
        assert!(lib.delete(new).is_err());
        lib.delete(old).unwrap();
        assert!(!root.join("145.550-Old_name").exists());
        assert_eq!(lib.files(ch).len(), 1);
        // A new recording joins its channel.
        write_wav(&root.join("118.010-TWR/20261025-130000.wav"), 800);
        lib.add(&root.join("118.010-TWR/20261025-130000.wav"));
        assert_eq!(lib.channels()[0].freq, Some(118_008_333));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn wav_data_chunk() {
        let root = std::env::temp_dir().join(format!("rfscan-play-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("x.wav");
        write_wav(&p, 10);
        let b = std::fs::read(&p).unwrap();
        assert_eq!(data_chunk(&b), Some((58, 10)));
        assert_eq!(wav_seconds(&p), Some(10.0 / 8000.0));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
