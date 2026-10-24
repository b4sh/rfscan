//! Excluded channel list: file loading, `--exclude`, channel mask.
//!
//! File: one entry per line, `#` starts a comment, empty lines are ignored, frequencies in MHz.
//! An entry is a single channel frequency (`433.920`); ranges are not supported. A missing file
//! is an empty list. A line that cannot be parsed (including a range) produces a warning with
//! the line number, and the rest of the file is loaded.
//!
//! Changes from the TUI: adding an entry appends a line and flushes it at once, so a
//! crash loses nothing; removing an entry rewrites the file atomically (temporary file + rename),
//! keeping comments and lines the program does not understand. Timed exclusions (`x` with a
//! duration) are kept in memory only and end by themselves.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::plan::ScanPlan;

/// Exclusion file name in the user's configuration directory.
const FILE_NAME: &str = "rfscan-exclude.txt";

/// Durations offered for a timed exclusion (minutes).
pub const TIMED_MINUTES: [u32; 3] = [10, 30, 60];

/// One entry of the exclusion list as shown: `until` is set for a timed exclusion.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub freq: u64,
    pub note: Option<String>,
    pub until: Option<Instant>,
}

impl Item {
    /// Whole minutes left of a timed exclusion, rounded up.
    pub fn minutes_left(&self, now: Instant) -> Option<u64> {
        self.until
            .map(|t| t.saturating_duration_since(now).as_secs().div_ceil(60))
    }
}

/// The exclusion list (timed entries first, then the file), shared between the scanner and the
/// interfaces.
pub type SharedList = std::sync::Arc<std::sync::Mutex<Vec<Item>>>;

/// A timed exclusion: not saved, it ends at `until`.
#[derive(Clone, Debug)]
pub struct Timed {
    pub freq: u64,
    pub note: Option<String>,
    pub until: Instant,
}

#[derive(Debug, Default)]
pub struct Exclusions {
    /// Excluded frequencies from the file (Hz), in file order.
    pub entries: Vec<u64>,
    /// Notes (the comment on the entry's line), parallel to `entries`.
    pub notes: Vec<Option<String>>,
    /// Temporary exclusions from `--exclude` (Hz), never saved.
    pub temp: Vec<u64>,
    /// Timed exclusions added during the run, never saved.
    pub timed: Vec<Timed>,
}

/// Default exclusion file path (user's configuration directory).
pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join(FILE_NAME))
}

/// Parses one entry (without a comment): channel frequency in MHz, e.g. `433.920`.
pub fn parse_entry(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.contains('-') {
        return Err(format!(
            "'{s}': ranges are not supported, give single channels (e.g. 433.920)"
        ));
    }
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}': expected a channel frequency in MHz (e.g. 433.920)"))?;
    let hz = (v * 1e6).round();
    if !v.is_finite() || hz <= 0.0 {
        return Err(format!("'{s}': frequency must be positive"));
    }
    // An airband channel number (three decimal places) is converted to the actual frequency.
    crate::airband::from_text(s, hz as u64).map(|(f, _)| f)
}

/// Parses the file content. Returns the entries and warnings for lines that cannot be parsed.
#[cfg(test)]
fn parse_file(text: &str) -> (Vec<u64>, Vec<String>) {
    let (items, warnings) = parse_items(text);
    (items.into_iter().map(|(f, _)| f).collect(), warnings)
}

/// Parses the file content into entries with their notes (the comment on the entry's line).
fn parse_items(text: &str) -> (Vec<(u64, Option<String>)>, Vec<String>) {
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    // A BOM from Windows editors is not an error.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, line) in text.lines().enumerate() {
        let (content, comment) = match line.split_once('#') {
            Some((c, note)) => (c.trim(), Some(note.trim())),
            None => (line.trim(), None),
        };
        if content.is_empty() {
            continue;
        }
        match parse_entry(content) {
            Ok(e) => items.push((e, comment.filter(|c| !c.is_empty()).map(str::to_string))),
            Err(e) => warnings.push(format!("line {}: {e}", n + 1)),
        }
    }
    (items, warnings)
}

impl Exclusions {
    /// Loads the file. A missing file is an empty list (no warnings).
    pub fn load(path: &Path) -> Result<(Self, Vec<String>), String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let (items, warnings) = parse_items(&text);
                let (entries, notes) = items.into_iter().unzip();
                Ok((
                    Exclusions {
                        entries,
                        notes,
                        temp: Vec::new(),
                        timed: Vec::new(),
                    },
                    warnings,
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok((Exclusions::default(), Vec::new()))
            }
            Err(e) => Err(format!(
                "cannot read exclusion file {}: {e}",
                path.display()
            )),
        }
    }

    /// Adds temporary exclusions from `--exclude F,...` (MHz).
    pub fn add_cli(&mut self, list: &str) -> Result<(), String> {
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            self.temp
                .push(parse_entry(item).map_err(|e| format!("--exclude {e}"))?);
        }
        Ok(())
    }

    /// Plan channel mask (channel index -> excluded). An entry covers channels whose center lies
    /// within half the channel spacing (range step or bw of a listed channel). Built once, at
    /// startup.
    pub fn mask(&self, plan: &ScanPlan) -> Vec<bool> {
        plan.channels
            .iter()
            .map(|c| {
                self.entries
                    .iter()
                    .chain(&self.temp)
                    .chain(self.timed.iter().map(|t| &t.freq))
                    .any(|&f| (c.freq as f64 - f as f64).abs() <= c.spacing / 2.0)
            })
            .collect()
    }

    /// Adds `freqs` to the file (appended and flushed at once) and to the list, with an optional
    /// note (written as the line's comment).
    /// Airband frequencies are written as channel numbers (`raster25`: the 25 kHz raster ones).
    pub fn add(
        &mut self,
        path: &Path,
        freqs: &[u64],
        note: Option<&str>,
        raster25: &[u64],
    ) -> Result<(), String> {
        append(path, freqs, note, raster25)?;
        self.entries.extend_from_slice(freqs);
        self.notes
            .extend(freqs.iter().map(|_| note.map(str::to_string)));
        // A permanent entry replaces a timed one.
        self.timed.retain(|t| !freqs.contains(&t.freq));
        Ok(())
    }

    /// Adds timed exclusions ending after `minutes` (not saved). A frequency that is already
    /// timed gets the new end and note.
    pub fn add_timed(&mut self, freqs: &[u64], note: Option<&str>, minutes: u32, now: Instant) {
        let until = now + Duration::from_secs(u64::from(minutes) * 60);
        self.timed.retain(|t| !freqs.contains(&t.freq));
        self.timed.extend(freqs.iter().map(|&freq| Timed {
            freq,
            note: note.map(str::to_string),
            until,
        }));
    }

    /// Changes the note of an exclusion (`None` removes it): a timed one in memory, a file entry
    /// on its first line in the file (atomic rewrite, the frequency text stays). A `#` in the
    /// note becomes a space, so the comment reads back the same.
    pub fn set_note(&mut self, path: &Path, freq: u64, note: Option<&str>) -> Result<(), String> {
        let note = note
            .map(|n| n.replace('#', " ").trim().to_string())
            .filter(|n| !n.is_empty());
        if let Some(t) = self.timed.iter_mut().find(|t| t.freq == freq) {
            t.note = note;
            return Ok(());
        }
        let Some(i) = self.entries.iter().position(|&f| f == freq) else {
            return Err(format!(
                "{} MHz is not on the exclusion list",
                crate::activity::format_mhz(freq)
            ));
        };
        set_note_line(path, freq, note.as_deref())?;
        if let Some(n) = self.notes.get_mut(i) {
            *n = note;
        }
        Ok(())
    }

    /// Removes timed exclusions; returns whether any was removed.
    pub fn remove_timed(&mut self, freqs: &[u64]) -> bool {
        let n = self.timed.len();
        self.timed.retain(|t| !freqs.contains(&t.freq));
        self.timed.len() != n
    }

    /// The earliest end of a timed exclusion.
    pub fn next_expiry(&self) -> Option<Instant> {
        self.timed.iter().map(|t| t.until).min()
    }

    /// Removes the timed exclusions that ended by `now` and returns their frequencies.
    pub fn expire(&mut self, now: Instant) -> Vec<u64> {
        let ended = self
            .timed
            .iter()
            .filter(|t| t.until <= now)
            .map(|t| t.freq)
            .collect();
        self.timed.retain(|t| t.until > now);
        ended
    }

    /// Removes `freqs` from the file (atomic rewrite) and from the list.
    pub fn remove(&mut self, path: &Path, freqs: &[u64]) -> Result<(), String> {
        remove_lines(path, freqs)?;
        let mut notes = std::mem::take(&mut self.notes).into_iter();
        let (entries, notes): (Vec<u64>, Vec<Option<String>>) = self
            .entries
            .iter()
            .map(|&f| (f, notes.next().flatten()))
            .filter(|(f, _)| !freqs.contains(f))
            .unzip();
        self.entries = entries;
        self.notes = notes;
        Ok(())
    }

    /// The list as shown: timed entries (soonest end first), then the file entries with their
    /// notes in file order.
    pub fn items(&self) -> Vec<Item> {
        let mut timed: Vec<&Timed> = self.timed.iter().collect();
        timed.sort_by_key(|t| t.until);
        let timed = timed.into_iter().map(|t| Item {
            freq: t.freq,
            note: t.note.clone(),
            until: Some(t.until),
        });
        let file = self
            .entries
            .iter()
            .zip(self.notes.iter().cloned().chain(std::iter::repeat(None)))
            .map(|(&freq, note)| Item {
                freq,
                note,
                until: None,
            });
        timed.chain(file).collect()
    }
}

/// Appends entries to the file, each with the note as its comment. A file that does not end with
/// a newline gets one first, so the new entry does not merge with the last line.
fn append(path: &Path, freqs: &[u64], note: Option<&str>, raster25: &[u64]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create directory {}: {e}", dir.display()))?;
    }
    let needs_newline = std::fs::read(path)
        .map(|b| b.last().is_some_and(|&c| c != b'\n'))
        .unwrap_or(false);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("cannot open exclusion file {}: {e}", path.display()))?;
    let mut text = String::new();
    if needs_newline {
        text.push('\n');
    }
    for &hz in freqs {
        let mhz = crate::airband::file_text(hz, raster25.contains(&hz));
        match note {
            Some(n) => text.push_str(&format!("{mhz:<22} # {n}\n")),
            None => text.push_str(&format!("{mhz}\n")),
        }
    }
    f.write_all(text.as_bytes())
        .and_then(|_| f.sync_data())
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Rewrites the file without the lines whose entry is in `freqs`. Comments, empty lines and
/// lines that cannot be parsed stay.
fn remove_lines(path: &Path, freqs: &[u64]) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read exclusion file {}: {e}", path.display()))?;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let content = line
            .trim_start_matches('\u{feff}')
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        let drop = !content.is_empty() && parse_entry(content).is_ok_and(|f| freqs.contains(&f));
        if !drop {
            out.push_str(line);
        }
    }
    write_atomic(path, &out)
}

/// Rewrites the comment of the first line whose entry is `freq` (`None`: no comment), keeping
/// its frequency text and the rest of the file.
fn set_note_line(path: &Path, freq: u64, note: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read exclusion file {}: {e}", path.display()))?;
    let mut out = String::with_capacity(text.len() + LINE_EXTRA);
    let mut done = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        let bom = if body.starts_with('\u{feff}') {
            "\u{feff}"
        } else {
            ""
        };
        let content = body[bom.len()..].split('#').next().unwrap_or("").trim();
        if !done && !content.is_empty() && parse_entry(content).is_ok_and(|f| f == freq) {
            done = true;
            out.push_str(bom);
            match note {
                Some(n) => out.push_str(&format!("{content:<22} # {n}")),
                None => out.push_str(content),
            }
            out.push_str(eol);
        } else {
            out.push_str(line);
        }
    }
    if !done {
        return Err(format!(
            "{} MHz is not in {}",
            crate::activity::format_mhz(freq),
            path.display()
        ));
    }
    write_atomic(path, &out)
}

/// Room for a longer comment when a line is rewritten.
const LINE_EXTRA: usize = 64;

/// Writes the whole file through a temporary file and a rename.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Modulation, RangeSpec, Step, ranges_plan};

    #[test]
    fn parse_file_with_comments_and_bad_lines() {
        let text = "\u{feff}# rfscan: exclusions\n433.920                # added with the x key\n\n433.920-434.100 # range\n433.00625\nabc\n";
        let (e, w) = parse_file(text);
        assert_eq!(e, vec![433_920_000, 433_006_250]);
        assert_eq!(w.len(), 2);
        assert!(w[0].starts_with("line 4:") && w[0].contains("ranges are not supported"));
        assert!(w[1].starts_with("line 6:"));
    }

    #[test]
    fn mask_single_channels() {
        let r = RangeSpec {
            start: 144_000_000,
            stop: 146_000_000,
            step: Step::from_hz(12_500),
            bw: 12_500.0,
            bw_explicit: false,
            mode: Modulation::Nfm,
        };
        let p = ranges_plan(&[r], 4_800_000, 1).unwrap();
        let mut ex = Exclusions::default();
        // 3 kHz from channel 145.000 covers only that channel.
        ex.add_cli("145.003, 145.1").unwrap();
        let m = ex.mask(&p);
        let idx = |f: u64| ((f - 144_000_000) / 12_500) as usize;
        assert!(m[idx(145_000_000)] && m[idx(145_100_000)]);
        assert!(!m[idx(144_987_500)] && !m[idx(145_012_500)]);
        assert_eq!(m.iter().filter(|&&x| x).count(), 2);
        assert!(ex.add_cli("145.0-145.1").is_err());
        assert!(ex.add_cli("xyz").is_err());

        // A timed exclusion masks its channel until it ends, and is listed first.
        let now = std::time::Instant::now();
        ex.entries.push(145_500_000);
        ex.add_timed(&[145_200_000], Some("beacon"), 10, now);
        assert!(ex.mask(&p)[idx(145_200_000)]);
        let items = ex.items();
        assert_eq!(items[0].freq, 145_200_000);
        assert_eq!(items[0].minutes_left(now), Some(10));
        assert_eq!(items[1].until, None);
        let later = now + Duration::from_secs(9 * 60);
        assert!(ex.expire(later).is_empty());
        assert_eq!(ex.expire(later + Duration::from_secs(60)), [145_200_000]);
        assert!(!ex.mask(&p)[idx(145_200_000)] && ex.next_expiry().is_none());
    }

    /// Adding appends (also after a last line without a newline), removing keeps comments and
    /// lines the program does not understand.
    #[test]
    fn add_and_remove_keep_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("rfscan-excl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "# my list\n144.000 # carrier\nbad line\n438.000").unwrap();
        let (mut ex, _) = Exclusions::load(&path).unwrap();
        ex.add(
            &path,
            &[145_500_000, 145_512_500],
            Some("interference"),
            &[],
        )
        .unwrap();
        ex.add(&path, &[146_000_000], None, &[]).unwrap();
        let (again, w) = Exclusions::load(&path).unwrap();
        assert_eq!(
            again.entries,
            vec![
                144_000_000,
                438_000_000,
                145_500_000,
                145_512_500,
                146_000_000
            ]
        );
        // Notes are the comments on the entry lines.
        assert_eq!(
            again.notes,
            vec![
                Some("carrier".to_string()),
                None,
                Some("interference".to_string()),
                Some("interference".to_string()),
                None
            ]
        );
        assert_eq!(ex.notes, again.notes);
        assert_eq!(w.len(), 1);
        ex.remove(&path, &[438_000_000, 145_500_000]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my list\n144.000 # carrier\nbad line\n"),
            "{text}"
        );
        assert!(
            !text.contains("438.000") && !text.contains("145.500 "),
            "{text}"
        );
        assert!(text.contains("145.5125"), "{text}");
        // A note edit rewrites only the comment of that line; `#` in the note becomes a space.
        ex.set_note(&path, 144_000_000, Some("receiver # carrier"))
            .unwrap();
        ex.set_note(&path, 146_000_000, Some("beacon")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my list\n144.000                # receiver   carrier\nbad line\n"),
            "{text}"
        );
        assert!(
            text.ends_with("146.000                # beacon\n"),
            "{text}"
        );
        assert_eq!(Exclusions::load(&path).unwrap().0.notes, ex.notes);
        ex.set_note(&path, 146_000_000, None).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("\n146.000\n")
        );
        assert!(ex.set_note(&path, 433_000_000, Some("x")).is_err());
        ex.set_note(&path, 144_000_000, Some("carrier")).unwrap();
        assert_eq!(ex.entries, vec![144_000_000, 145_512_500, 146_000_000]);
        assert_eq!(
            ex.notes,
            vec![
                Some("carrier".to_string()),
                Some("interference".to_string()),
                None
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_is_empty() {
        let (ex, w) = Exclusions::load(Path::new("does/not/exist/rfscan-exclude.txt")).unwrap();
        assert!(ex.entries.is_empty() && w.is_empty());
    }
}
