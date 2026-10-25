//! Channel file: frequencies with names, mode and bandwidth.
//!
//! Line format: `FREQUENCY_MHz [am|nfm|fm|digi] [rec] [bw=WIDTH] ["name"]`. Mode, `rec` and
//! `bw=` in any order before the name. `#` outside quotes starts a comment. `\"` and `\\` are allowed in the
//! name. A line that cannot be parsed produces a warning with the line number. Duplicate
//! frequency: the first entry wins, the second produces a warning.
//!
//! The program writes the file only when the user names a channel or marks it `digi` in the TUI
//! (`set_entry`) or switches its audio recording (`set_rec`): the matching entry gets the new
//! name and mode or `rec` (everything else stays), otherwise a new line is appended. The write is atomic (temporary file + rename) and keeps all other lines unchanged.

use std::path::{Path, PathBuf};

use crate::plan::Modulation;

/// Channel file name in the user's configuration directory.
const FILE_NAME: &str = "rfscan-channels.txt";

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelEntry {
    pub freq: u64,
    /// Mode from the file (including `digi`).
    pub mode: Option<Modulation>,
    pub bw: Option<u64>,
    pub name: Option<String>,
    /// `rec`: the channel's audio is recorded (`--record-dir`).
    pub rec: bool,
    /// Airband: the entry is written with its 25 kHz channel number (25 kHz raster).
    pub raster25: bool,
    /// The frequency as written in the file (kept when the program renames the entry).
    pub freq_text: String,
    /// Line number in the file (for messages).
    pub line: usize,
}

pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join(FILE_NAME))
}

/// Splits a line into words and a quoted name, cutting off a comment outside quotes.
fn tokenize(line: &str) -> Result<(Vec<&str>, Option<String>), String> {
    let mut words = Vec::new();
    let mut name = None;
    let mut rest = line;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            break;
        }
        if name.is_some() {
            return Err(format!(
                "unexpected text after the name: '{}'",
                rest.trim_end()
            ));
        }
        if let Some(body) = rest.strip_prefix('"') {
            let mut s = String::new();
            let mut chars = body.char_indices();
            let mut end = None;
            while let Some((i, ch)) = chars.next() {
                match ch {
                    '\\' => match chars.next() {
                        Some((_, e @ ('"' | '\\'))) => s.push(e),
                        _ => return Err("only \\\" and \\\\ are allowed in the name".into()),
                    },
                    '"' => {
                        end = Some(i);
                        break;
                    }
                    c => s.push(c),
                }
            }
            let Some(end) = end else {
                return Err("missing closing quote in the name".into());
            };
            name = Some(s);
            rest = &body[end + 1..];
            continue;
        }
        let len = rest
            .find(|c: char| c.is_whitespace() || c == '#' || c == '"')
            .unwrap_or(rest.len());
        words.push(&rest[..len]);
        rest = &rest[len..];
    }
    Ok((words, name))
}

fn parse_line(line: &str, number: usize) -> Result<Option<ChannelEntry>, String> {
    let (words, name) = tokenize(line)?;
    let Some((&freq, opts)) = words.split_first() else {
        if name.is_some() {
            return Err("missing frequency before the name".into());
        }
        return Ok(None);
    };
    let mhz: f64 = freq
        .parse()
        .map_err(|_| format!("'{freq}': expected a frequency in MHz (e.g. 145.500)"))?;
    let hz = (mhz * 1e6).round();
    if !mhz.is_finite() || hz <= 0.0 {
        return Err(format!("'{freq}': frequency must be positive"));
    }
    // Airband channel numbers (3 decimal places) are converted to the actual frequency.
    let (hz, raster25) = crate::airband::from_text(freq, hz as u64)?;
    let mut entry = ChannelEntry {
        freq: hz,
        mode: None,
        bw: None,
        name,
        rec: false,
        raster25,
        freq_text: freq.to_string(),
        line: number,
    };
    for &w in opts {
        if let Some(v) = w.strip_prefix("bw=") {
            if entry.bw.is_some() {
                return Err("bw= given twice".into());
            }
            entry.bw = Some(crate::cli::parse_bw(v)?);
        } else if w == "rec" {
            if entry.rec {
                return Err("rec given twice".into());
            }
            entry.rec = true;
        } else if let Some(m) = Modulation::parse(w) {
            if entry.mode.is_some() {
                return Err("mode given twice".into());
            }
            entry.mode = Some(m);
        } else {
            return Err(format!(
                "'{w}': unknown field (allowed: am, nfm, fm, digi, rec, bw=..., \"name\")"
            ));
        }
    }
    Ok(Some(entry))
}

/// Parses the file content. Returns the entries and warnings.
pub fn parse_file(text: &str) -> (Vec<ChannelEntry>, Vec<String>) {
    let mut entries: Vec<ChannelEntry> = Vec::new();
    let mut warnings = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, line) in text.lines().enumerate() {
        let number = n + 1;
        match parse_line(line, number) {
            Ok(Some(e)) => {
                if let Some(first) = entries.iter().find(|x| x.freq == e.freq) {
                    warnings.push(format!(
                        "line {number}: {:.6} MHz already defined in line {}, skipping",
                        e.freq as f64 / 1e6,
                        first.line
                    ));
                } else {
                    entries.push(e);
                }
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!("line {number}: {e}")),
        }
    }
    (entries, warnings)
}

/// Entry nearest to `freq`, at most `tolerance` Hz away (half the range step or half
/// the `bw` of a listed channel). Only while building the plan.
pub fn nearest(entries: &[ChannelEntry], freq: u64, tolerance: f64) -> Option<&ChannelEntry> {
    entries
        .iter()
        .filter(|e| e.freq.abs_diff(freq) as f64 <= tolerance)
        .min_by_key(|e| e.freq.abs_diff(freq))
}

/// Splits a line into its content and a comment (`#` outside quotes), if any.
fn split_comment(line: &str) -> (&str, Option<&str>) {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return (&line[..i], Some(&line[i..])),
            _ => {}
        }
    }
    (line, None)
}

/// A name in quotes, with `"` and `\` escaped.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The line of an entry (optional comment).
fn entry_line(e: &ChannelEntry, comment: Option<&str>) -> String {
    let mut s = e.freq_text.clone();
    if let Some(m) = e.mode {
        s.push(' ');
        s.push_str(&m.name().to_ascii_lowercase());
    }
    if e.rec {
        s.push_str(" rec");
    }
    if let Some(bw) = e.bw {
        s.push_str(&format!(" bw={bw}"));
    }
    if let Some(name) = &e.name {
        s.push(' ');
        s.push_str(&quoted(name));
    }
    if let Some(c) = comment {
        s.push_str("  ");
        s.push_str(c.trim_end());
    }
    s
}

/// Gives the channel at `freq` the name `name` (`None`: no name) and the `digi` mode or not in
/// the file `path`: the entry nearest to `freq` within `tolerance` Hz is changed
/// (its frequency text, `bw=`, `rec` and a mode other than `digi` are kept), otherwise a new
/// entry is appended, written as an airband channel number where it applies (`raster25`: a 25 kHz
/// channel). Returns the frequency of the entry that was written.
pub fn set_entry(
    path: &Path,
    freq: u64,
    tolerance: f64,
    name: Option<&str>,
    digi: bool,
    raster25: bool,
) -> Result<u64, String> {
    rewrite(path, freq, tolerance, raster25, |e| {
        e.name = name.map(str::to_string);
        e.mode = match (digi, e.mode) {
            (true, _) => Some(Modulation::Digi),
            (false, Some(Modulation::Digi)) => None,
            (false, m) => m,
        };
    })
}

/// Switches the audio recording (`rec`) of the channel at `freq` in the file `path`, like
/// `set_entry` (everything else in the entry stays). Returns the frequency of the entry.
pub fn set_rec(
    path: &Path,
    freq: u64,
    tolerance: f64,
    rec: bool,
    raster25: bool,
) -> Result<u64, String> {
    rewrite(path, freq, tolerance, raster25, |e| e.rec = rec)
}

/// Changes the entry nearest to `freq` within `tolerance` Hz with `change`, or appends a new
/// entry changed the same way; all other lines stay. Atomic write (temporary file + rename).
fn rewrite(
    path: &Path,
    freq: u64,
    tolerance: f64,
    raster25: bool,
    change: impl Fn(&mut ChannelEntry),
) -> Result<u64, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!("cannot read channel file {}: {e}", path.display()));
        }
    };
    let (entries, _) = parse_file(&text);
    let target = nearest(&entries, freq, tolerance).cloned();
    let mut out = String::with_capacity(text.len() + 64);
    let written = match target {
        Some(e) => {
            for (n, line) in text.split_inclusive('\n').enumerate() {
                if n + 1 == e.line {
                    let body = line.trim_end_matches(['\n', '\r']);
                    let ending = &line[body.len()..];
                    let (_, comment) = split_comment(body.trim_start_matches('\u{feff}'));
                    if n == 0 && body.starts_with('\u{feff}') {
                        out.push('\u{feff}');
                    }
                    let mut new = e.clone();
                    change(&mut new);
                    out.push_str(&entry_line(&new, comment));
                    out.push_str(if ending.is_empty() { "\n" } else { ending });
                } else {
                    out.push_str(line);
                }
            }
            e.freq
        }
        None => {
            out.push_str(&text);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            let mut e = ChannelEntry {
                freq,
                mode: None,
                bw: None,
                name: None,
                rec: false,
                raster25,
                freq_text: crate::airband::file_text(freq, raster25),
                line: 0,
            };
            change(&mut e);
            out.push_str(&entry_line(&e, None));
            out.push('\n');
            freq
        }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create directory {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, out).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(written)
}

/// Loads the channel file. A missing file is an empty list.
pub fn load(path: &Path) -> Result<(Vec<ChannelEntry>, Vec<String>), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_file(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(format!("cannot read channel file {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        // The last name uses Polish diacritics on purpose: names must support UTF-8.
        let text = "\u{feff}# rfscan: channels\n144.500 \"AMATEUR CALLING FREQ\"\n118.100 am bw=10k \"AIRPORT TOWER\"\n145.600 fm \"REPEATER R0\"\n438.800 digi \"DMR LOCAL\"\n433.00625\n145.500 \"Zażółć \\\"gęślą\\\" # not a comment\" # comment\n";
        let (e, w) = parse_file(text);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(e.len(), 6);
        assert_eq!(e[0].name.as_deref(), Some("AMATEUR CALLING FREQ"));
        assert_eq!(e[0].mode, None);
        assert_eq!(e[1].freq, 118_100_000);
        assert_eq!(e[1].mode, Some(Modulation::Am));
        assert_eq!(e[1].bw, Some(10_000));
        assert_eq!(e[2].mode, Some(Modulation::Nfm));
        assert_eq!(e[3].mode, Some(Modulation::Digi));
        assert_eq!(e[4].freq, 433_006_250);
        assert_eq!(e[4].name, None);
        assert_eq!(
            e[5].name.as_deref(),
            Some("Zażółć \"gęślą\" # not a comment")
        );
    }

    /// Naming keeps the mode, `bw=`, comments and other lines; a new frequency is appended;
    /// `digi` is set or removed.
    #[test]
    fn set_entry_renames_or_appends() {
        let dir = std::env::temp_dir().join(format!("rfscan-cdb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(
            &path,
            "# my channels\n438.6125 digi \"DMR\"  # repeater\n118.100 am bw=10000\nbad line",
        )
        .unwrap();
        assert_eq!(
            set_entry(
                &path,
                438_612_500,
                6_250.0,
                Some("DMR \"Repeater\""),
                true,
                false
            )
            .unwrap(),
            438_612_500
        );
        set_entry(&path, 118_100_000, 4_000.0, Some("TOWER"), false, false).unwrap();
        set_entry(&path, 145_500_000, 6_250.0, Some("CALLING"), false, false).unwrap();
        set_entry(&path, 438_800_000, 6_250.0, None, true, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# my channels\n438.6125 digi \"DMR \\\"Repeater\\\"\"  # repeater\n118.100 am bw=10000 \"TOWER\"\nbad line\n145.500 \"CALLING\"\n438.800 digi\n"
        );
        // Switching digi off removes only the mode; a missing name removes the name.
        set_entry(&path, 438_800_000, 6_250.0, Some("DMR"), false, false).unwrap();
        set_entry(&path, 118_100_000, 4_000.0, None, true, false).unwrap();
        let text2 = std::fs::read_to_string(&path).unwrap();
        assert!(text2.ends_with("\n438.800 \"DMR\"\n"), "{text2}");
        assert!(text2.contains("\n118.100 digi bw=10000\n"), "{text2}");
        // `rec` is switched without touching the rest and kept when the entry is renamed.
        set_rec(&path, 438_612_500, 6_250.0, true, false).unwrap();
        set_rec(&path, 433_450_000, 6_250.0, true, false).unwrap();
        set_entry(&path, 438_612_500, 6_250.0, Some("DMR"), true, false).unwrap();
        let text3 = std::fs::read_to_string(&path).unwrap();
        assert!(
            text3.contains("\n438.6125 digi rec \"DMR\"  # repeater\n"),
            "{text3}"
        );
        assert!(text3.ends_with("\n433.450 rec\n"), "{text3}");
        set_rec(&path, 433_450_000, 6_250.0, false, false).unwrap();
        let (e3, _) = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert!(e3.iter().any(|e| e.freq == 438_612_500 && e.rec));
        assert!(e3.iter().any(|e| e.freq == 433_450_000 && !e.rec));
        let (e, _) = parse_file(&text);
        assert_eq!(e[0].name.as_deref(), Some("DMR \"Repeater\""));
        assert_eq!(e[0].mode, Some(Modulation::Digi));
        assert_eq!(e[1].bw, Some(10_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_lines_and_duplicates_give_warnings() {
        let text = "145.500 \"A\"\nxyz\n145.500 \"B\"\n145.6 am fm\n145.7 \"no end\n145.8 bw=abc\n145.9 \"A\" am\n";
        let (e, w) = parse_file(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name.as_deref(), Some("A"));
        let lines: Vec<_> = w
            .iter()
            .map(|s| s.split(':').next().unwrap_or(""))
            .collect();
        assert_eq!(
            lines,
            ["line 2", "line 3", "line 4", "line 5", "line 6", "line 7"]
        );
    }
}
