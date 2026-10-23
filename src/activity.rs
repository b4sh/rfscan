//! Channel activity log: seconds of demodulated audio per frequency.
//!
//! File: one line per frequency, `MHz:secondss [hits=N] [last=ISO8601] [mode=nfm|am|digi]`,
//! sorted by ascending frequency. Only the first field is required; unknown fields are ignored, unparsable lines
//! produce a warning. The log accumulates across runs. Atomic write: temporary file and rename.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::plan::Modulation;

/// Log file name in the user's configuration directory.
const FILE_NAME: &str = "rfscan-activity.txt";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub seconds: f64,
    pub hits: u64,
    pub last: Option<String>,
    /// Mode of the last activity.
    pub mode: Option<Modulation>,
}

#[derive(Debug, Default)]
pub struct ActivityLog {
    pub entries: BTreeMap<u64, Entry>,
    /// Time in the current session (for the report and the future activity screen).
    pub session: BTreeMap<u64, f64>,
    dirty: bool,
}

pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join(FILE_NAME))
}

/// Frequency in MHz with the minimum number of decimal places, at least 3.
pub fn format_mhz(hz: u64) -> String {
    let mut s = format!("{}.{:06}", hz / 1_000_000, hz % 1_000_000);
    while s.ends_with('0') && s.len() - s.find('.').unwrap_or(0) > 4 {
        s.pop();
    }
    s
}

/// Local time in ISO 8601 format (without time zone), e.g. 2026-10-13T14:02:11.
pub fn local_now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn parse_line(line: &str) -> Result<(u64, Entry), String> {
    let mut fields = line.split_whitespace();
    let first = fields.next().ok_or("empty line")?;
    let (mhz, secs) = first
        .split_once(':')
        .ok_or_else(|| format!("'{first}': expected MHz:secondss"))?;
    let mhz: f64 = mhz
        .parse()
        .map_err(|_| format!("'{mhz}': invalid frequency"))?;
    let secs: f64 = secs
        .strip_suffix('s')
        .unwrap_or(secs)
        .parse()
        .map_err(|_| format!("'{secs}': invalid time"))?;
    if !(mhz.is_finite() && mhz > 0.0 && secs.is_finite() && secs >= 0.0) {
        return Err(format!("'{first}': values out of range"));
    }
    let mut e = Entry {
        seconds: secs,
        ..Entry::default()
    };
    for f in fields {
        if let Some(v) = f.strip_prefix("hits=") {
            e.hits = v.parse().unwrap_or(0);
        } else if let Some(v) = f.strip_prefix("last=") {
            e.last = Some(v.to_string());
        } else if let Some(v) = f.strip_prefix("mode=") {
            e.mode = Modulation::parse(v);
        }
        // Unknown fields are ignored.
    }
    Ok(((mhz * 1e6).round() as u64, e))
}

impl ActivityLog {
    /// Loads the log. A missing file is an empty log.
    pub fn load(path: &Path) -> Result<(Self, Vec<String>), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((ActivityLog::default(), Vec::new()));
            }
            Err(e) => {
                return Err(format!("cannot read activity log {}: {e}", path.display()));
            }
        };
        let mut log = ActivityLog::default();
        let mut warnings = Vec::new();
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match parse_line(line) {
                Ok((hz, e)) => {
                    let slot = log.entries.entry(hz).or_default();
                    slot.seconds += e.seconds;
                    slot.hits += e.hits;
                    if e.last > slot.last {
                        slot.last = e.last;
                        slot.mode = e.mode.or(slot.mode);
                    } else if slot.mode.is_none() {
                        slot.mode = e.mode;
                    }
                }
                Err(e) => warnings.push(format!("line {}: {e}", n + 1)),
            }
        }
        Ok((log, warnings))
    }

    /// New activity (entering Locked from Scanning) in the given mode.
    pub fn hit(&mut self, freq: u64, mode: Modulation) {
        let e = self.entries.entry(freq).or_default();
        e.hits += 1;
        e.last = Some(local_now());
        e.mode = Some(mode);
        self.session.entry(freq).or_insert(0.0);
        self.dirty = true;
    }

    /// Adds demodulated audio time.
    pub fn add_seconds(&mut self, freq: u64, seconds: f64) {
        if seconds <= 0.0 {
            return;
        }
        self.entries.entry(freq).or_default().seconds += seconds;
        *self.session.entry(freq).or_insert(0.0) += seconds;
        self.dirty = true;
    }

    /// Removes all entries (TUI activity screen, `[c] clear`); the next save writes an empty
    /// file.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.session.clear();
        self.dirty = true;
    }

    /// Removes one frequency (its total and session time).
    pub fn remove(&mut self, freq: u64) {
        self.entries.remove(&freq);
        self.session.remove(&freq);
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (&hz, e) in &self.entries {
            let _ = write!(s, "{}:{:.1}s", format_mhz(hz), e.seconds);
            if e.hits > 0 {
                let _ = write!(s, " hits={}", e.hits);
            }
            if let Some(last) = &e.last {
                let _ = write!(s, " last={last}");
            }
            if let Some(mode) = e.mode {
                let _ = write!(s, " mode={}", mode.name().to_ascii_lowercase());
            }
            s.push('\n');
        }
        s
    }

    /// Atomic write: a temporary file next to it and a rename.
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create directory {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("txt.tmp");
        std::fs::write(&tmp, self.to_text())
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("writing {}: {e}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

/// `--activity-reset`: moves the existing log to `.bak`.
pub fn reset(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let bak = path.with_extension("txt.bak");
    std::fs::rename(path, &bak).map_err(|e| format!("archiving {}: {e}", path.display()))?;
    Ok(Some(bak))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mhz_formatting() {
        assert_eq!(format_mhz(145_500_000), "145.500");
        assert_eq!(format_mhz(433_006_250), "433.00625");
        assert_eq!(format_mhz(118_008_333), "118.008333");
        assert_eq!(format_mhz(144_000_000), "144.000");
    }

    #[test]
    fn roundtrip_and_accumulation() {
        let text = "145.500:140.4s hits=7 last=2026-10-13T14:02:11\n433.00625:12.0s hits=2 last=2026-10-13T13:40:03 extra=1 mode=digi\nbad line\n118.008333:3.25s\n";
        let dir = std::env::temp_dir().join(format!("rfscan-test-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, text).unwrap();
        let (mut log, w) = ActivityLog::load(&path).unwrap();
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("line 3:"));
        assert_eq!(log.entries[&145_500_000].hits, 7);
        assert_eq!(log.entries[&145_500_000].mode, None);
        assert_eq!(log.entries[&433_006_250].mode, Some(Modulation::Digi));
        log.hit(145_500_000, Modulation::Nfm);
        log.add_seconds(145_500_000, 30.04);
        log.save(&path).unwrap();
        let (again, _) = ActivityLog::load(&path).unwrap();
        let e = &again.entries[&145_500_000];
        assert!((e.seconds - 170.4).abs() < 0.06, "{}", e.seconds);
        assert_eq!(e.hits, 8);
        assert_eq!(e.mode, Some(Modulation::Nfm));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("mode=nfm\n")
        );
        // Sorted by ascending frequency.
        let text = std::fs::read_to_string(&path).unwrap();
        let first: Vec<_> = text.lines().map(|l| l.split(':').next().unwrap()).collect();
        assert_eq!(first, ["118.008333", "145.500", "433.00625"]);
        // Clearing (TUI `[c] clear`) saves an empty file.
        let mut log = again;
        log.clear();
        assert!(log.is_dirty());
        log.save(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        assert!(ActivityLog::load(&path).unwrap().0.entries.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
