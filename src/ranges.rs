//! Range file: the ranges of range mode with their step, bandwidth, mode, name and `off`.
//!
//! Line format: `START-STOP [STEP] [am|fm|nfm] [bw=BW] [off] ["name"]`, e.g.
//! `144.000-146.000 12.5k fm bw=12.5k "2m"`. START and STOP are in MHz (or with a `k`, `M`, `G`
//! suffix), the other fields are optional, in any order before the name, with the suffixes of
//! the command line. `digi` is not a range mode here: a `digi` channel inside a range is set in
//! the channel file. `#` outside quotes starts a comment. A line that cannot be parsed gives a
//! warning with the line number and is skipped.
//!
//! The program writes the file only when the user switches a range on or off (`set_off`) or
//! names it (`set_name`, `append` for a range given on the command line): the line changes only
//! in that field, every other line stays (also lines the program does not understand). The write
//! is atomic (temporary file + rename).

use std::path::{Path, PathBuf};

use crate::channel_db::{quoted, split_comment, tokenize, write_atomic};
use crate::cli::{RangeArg, parse_bw, parse_hz, parse_step};
use crate::plan::Modulation;

/// Range file name in the user's configuration directory.
pub const FILE_NAME: &str = "rfscan-ranges.txt";

pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join(FILE_NAME))
}

/// One range of the file.
#[derive(Clone, Debug, PartialEq)]
pub struct RangeEntry {
    pub start: u64,
    pub stop: u64,
    pub step: Option<u64>,
    pub mode: Option<Modulation>,
    pub bw: Option<u64>,
    /// Switched off in the scan list.
    pub off: bool,
    pub name: Option<String>,
    /// Line number in the file (for messages).
    pub line: usize,
}

impl RangeEntry {
    /// The range as a `--scan-range` argument: the missing fields are filled the same way.
    pub fn arg(&self) -> RangeArg {
        RangeArg {
            start: self.start,
            stop: self.stop,
            step: self.step,
            mode: self.mode,
            bw: self.bw,
        }
    }
}

/// A frequency in MHz, or with a `k`, `M` or `G` suffix.
fn parse_mhz(s: &str) -> Result<u64, String> {
    if s.ends_with(|c: char| c.is_ascii_alphabetic()) {
        return parse_hz(s);
    }
    let mhz: f64 = s
        .parse()
        .map_err(|_| format!("'{s}': expected a frequency in MHz (e.g. 144.000)"))?;
    let hz = (mhz * 1e6).round();
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!("'{s}': frequency must be positive"));
    }
    Ok(hz as u64)
}

fn parse_line(line: &str, number: usize) -> Result<Option<RangeEntry>, String> {
    let (words, name) = tokenize(line)?;
    let Some((&range, opts)) = words.split_first() else {
        if name.is_some() {
            return Err("missing START-STOP before the name".into());
        }
        return Ok(None);
    };
    let Some((a, b)) = range.split_once('-') else {
        return Err(format!(
            "'{range}': expected START-STOP in MHz (e.g. 144.000-146.000)"
        ));
    };
    let (start, stop) = (parse_mhz(a)?, parse_mhz(b)?);
    if start >= stop {
        return Err(format!("'{range}': range start must be lower than its end"));
    }
    let mut e = RangeEntry {
        start,
        stop,
        step: None,
        mode: None,
        bw: None,
        off: false,
        name,
        line: number,
    };
    for &w in opts {
        if let Some(v) = w.strip_prefix("bw=") {
            if e.bw.replace(parse_bw(v)?).is_some() {
                return Err("bw= given twice".into());
            }
        } else if w == "off" {
            if e.off {
                return Err("off given twice".into());
            }
            e.off = true;
        } else if w.eq_ignore_ascii_case("digi") {
            return Err("digi is not a range mode (mark digi channels in the channel file)".into());
        } else if let Some(m) = Modulation::parse(w) {
            if e.mode.replace(m).is_some() {
                return Err("mode given twice".into());
            }
        } else if w.starts_with(|c: char| c.is_ascii_digit()) {
            if e.step.replace(parse_step(w)?).is_some() {
                return Err("step given twice".into());
            }
        } else {
            return Err(format!(
                "'{w}': unknown field (allowed: STEP, am, fm, nfm, bw=..., off, \"name\")"
            ));
        }
    }
    Ok(Some(e))
}

/// Parses the file content. Returns the ranges and warnings (with line numbers).
pub fn parse_file(text: &str) -> (Vec<RangeEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, line) in text.lines().enumerate() {
        match parse_line(line, n + 1) {
            Ok(Some(e)) => entries.push(e),
            Ok(None) => {}
            Err(e) => warnings.push(format!("line {}: {e}", n + 1)),
        }
    }
    (entries, warnings)
}

/// Loads the range file. A missing file is an empty list.
pub fn load(path: &Path) -> Result<(Vec<RangeEntry>, Vec<String>), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_file(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), Vec::new())),
        Err(e) => Err(format!("cannot read range file {}: {e}", path.display())),
    }
}

/// `12.5k`, `8.33k`, `25k`: a step or bandwidth as written to the file.
pub fn khz_text(hz: f64) -> String {
    let s = format!("{:.2}", hz / 1e3);
    format!("{}k", s.trim_end_matches('0').trim_end_matches('.'))
}

/// The line of a new range (`append`): `START-STOP STEP MODE bw=BW [off] ["name"]`.
pub fn entry_text(
    start: u64,
    stop: u64,
    step_hz: f64,
    mode: Modulation,
    bw_hz: f64,
    off: bool,
    name: Option<&str>,
) -> Result<String, String> {
    let mode = match mode {
        Modulation::Nfm => "fm",
        Modulation::Am => "am",
        Modulation::Digi => {
            return Err("a digi range cannot be written to the range file".into());
        }
    };
    let mut s = format!(
        "{}-{} {} {mode} bw={}",
        crate::activity::format_mhz(start),
        crate::activity::format_mhz(stop),
        khz_text(step_hz),
        khz_text(bw_hz)
    );
    if off {
        s.push_str(" off");
    }
    if let Some(n) = name {
        s.push(' ');
        s.push_str(&quoted(n));
    }
    Ok(s)
}

/// Switches the ranges with the given start and stop on or off (`off`), as one write. A range
/// that is no longer in the file is an error, and nothing is written.
pub fn set_off(path: &Path, ranges: &[(u64, u64)], off: bool) -> Result<(), String> {
    rewrite(path, ranges, |words, _| {
        words.retain(|w| w != "off");
        if off {
            words.push("off".into());
        }
    })
}

/// Names the range with the given start and stop (`None` removes the name).
pub fn set_name(path: &Path, range: (u64, u64), name: Option<&str>) -> Result<(), String> {
    rewrite(path, &[range], |_, n| *n = name.map(str::to_string))
}

/// Appends a line to the file (created when missing). Returns its line number.
pub fn append(path: &Path, line: &str) -> Result<usize, String> {
    let mut text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read range file {}: {e}", path.display())),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
    write_atomic(path, &text)?;
    Ok(text.lines().count())
}

/// Changes the lines of the given ranges: `change` gets the words before the name and the name.
fn rewrite(
    path: &Path,
    ranges: &[(u64, u64)],
    change: impl Fn(&mut Vec<String>, &mut Option<String>),
) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read range file {}: {e}", path.display()))?;
    let (entries, _) = parse_file(&text);
    let mut lines = Vec::with_capacity(ranges.len());
    for &(start, stop) in ranges {
        let e = entries
            .iter()
            .find(|e| e.start == start && e.stop == stop)
            .ok_or_else(|| {
                format!(
                    "{}: the range {}-{} MHz is no longer in the file",
                    path.display(),
                    crate::activity::format_mhz(start),
                    crate::activity::format_mhz(stop)
                )
            })?;
        lines.push(e.line);
    }
    let mut out = String::with_capacity(text.len() + 16 * ranges.len());
    for (n, line) in text.split_inclusive('\n').enumerate() {
        if !lines.contains(&(n + 1)) {
            out.push_str(line);
            continue;
        }
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let bom = n == 0 && body.starts_with('\u{feff}');
        let body = body.trim_start_matches('\u{feff}');
        let (content, comment) = split_comment(body);
        let (words, name) = tokenize(content)?;
        let mut words: Vec<String> = words.into_iter().map(str::to_string).collect();
        let mut name = name;
        change(&mut words, &mut name);
        if bom {
            out.push('\u{feff}');
        }
        out.push_str(&words.join(" "));
        if let Some(n) = &name {
            out.push(' ');
            out.push_str(&quoted(n));
        }
        if let Some(c) = comment {
            out.push_str("  ");
            out.push_str(c.trim_end());
        }
        out.push_str(if ending.is_empty() { "\n" } else { ending });
    }
    write_atomic(path, &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, text: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rfscan-ranges-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    /// Optional fields in any order, BOM, non-ASCII names, comments.
    #[test]
    fn parses_optional_fields_in_any_order() {
        let text = "\u{feff}# ranges\n144.000-146.000 12.5k fm bw=12.5k \"2m\"\n\n118-137 off am 8.33k \"Lotnisko Wrocław # nie komentarz\" # comment\n430M-440M\n433.000-434.000 bw=25k off 25k\n";
        let (e, w) = parse_file(text);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(e.len(), 4);
        assert_eq!(
            e[0],
            RangeEntry {
                start: 144_000_000,
                stop: 146_000_000,
                step: Some(12_500),
                mode: Some(Modulation::Nfm),
                bw: Some(12_500),
                off: false,
                name: Some("2m".into()),
                line: 2,
            }
        );
        assert_eq!((e[1].start, e[1].stop), (118_000_000, 137_000_000));
        assert!(e[1].off && e[1].mode == Some(Modulation::Am) && e[1].step == Some(8_330));
        assert_eq!(
            e[1].name.as_deref(),
            Some("Lotnisko Wrocław # nie komentarz")
        );
        assert_eq!(
            (e[2].start, e[2].step, e[2].mode, e[2].bw, e[2].name.clone()),
            (430_000_000, None, None, None, None)
        );
        assert_eq!(e[2].line, 5);
        assert!(e[3].off && e[3].bw == Some(25_000) && e[3].step == Some(25_000));
        // The command line fields are filled the same way.
        assert_eq!(e[2].arg().start, 430_000_000);
    }

    /// Bad lines, `digi` included, give a warning with the line number and are skipped.
    #[test]
    fn bad_lines_are_skipped_with_the_line_number() {
        let text = "144-146 digi\n146-144\n144-146 12.5\n144-146 fm am\n144-146 xyz\n\"no range\"\n144 fm\n145-146 \"ok\"\n144-145 off off\n";
        let (e, w) = parse_file(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].name.as_deref(), Some("ok"));
        let lines: Vec<_> = w.iter().map(|s| s.split(':').next().unwrap()).collect();
        assert_eq!(
            lines,
            [
                "line 1", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7", "line 9"
            ]
        );
        assert!(w[0].contains("digi is not a range mode"), "{}", w[0]);
        assert!(w[2].contains("did you mean 12.5k?"), "{}", w[2]);
    }

    #[test]
    fn missing_file_is_empty() {
        let p = std::env::temp_dir()
            .join("rfscan-no-such-dir-xyz")
            .join(FILE_NAME);
        assert_eq!(load(&p).unwrap(), (Vec::new(), Vec::new()));
    }

    /// `off` and the name are written and removed, every other field and line stays.
    #[test]
    fn switch_and_name_keep_everything_else() {
        let (dir, path) = temp_file(
            "rw",
            "\u{feff}144.000-146.000 12.5k fm \"2m\"  # mine\r\nnot a range\r\n118-137 am 8.33k\r\n# end",
        );
        set_off(
            &path,
            &[(144_000_000, 146_000_000), (118_000_000, 137_000_000)],
            true,
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "\u{feff}144.000-146.000 12.5k fm off \"2m\"  # mine\r\nnot a range\r\n118-137 am 8.33k off\r\n# end"
        );
        set_name(&path, (118_000_000, 137_000_000), Some("Air \"band\"")).unwrap();
        set_off(&path, &[(144_000_000, 146_000_000)], false).unwrap();
        set_name(&path, (144_000_000, 146_000_000), None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "\u{feff}144.000-146.000 12.5k fm  # mine\r\nnot a range\r\n118-137 am 8.33k off \"Air \\\"band\\\"\"\r\n# end"
        );
        let (e, w) = parse_file(&text);
        assert!(w.len() == 1 && w[0].starts_with("line 2:"), "{w:?}");
        assert_eq!(e[1].name.as_deref(), Some("Air \"band\""));
        // A range no longer in the file: an error, nothing written.
        assert!(set_off(&path, &[(430_000_000, 440_000_000)], true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(!dir.join(format!("{FILE_NAME}.tmp")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Naming a command line range appends it with all its fields; the line parses back.
    #[test]
    fn append_writes_all_fields() {
        let (dir, path) = temp_file("app", "144-146 # no newline");
        let line = entry_text(
            430_000_000,
            440_000_000,
            25_000.0,
            Modulation::Nfm,
            12_500.0,
            true,
            Some("70cm"),
        )
        .unwrap();
        assert_eq!(line, "430.000-440.000 25k fm bw=12.5k off \"70cm\"");
        assert_eq!(append(&path, &line).unwrap(), 2);
        let air = entry_text(
            118_000_000,
            137_000_000,
            25_000.0 / 3.0,
            Modulation::Am,
            25_000.0 / 3.0,
            false,
            None,
        )
        .unwrap();
        assert_eq!(air, "118.000-137.000 8.33k am bw=8.33k");
        append(&path, &air).unwrap();
        let (e, w) = load(&path).unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(e.len(), 3);
        assert!(e[1].off && e[1].name.as_deref() == Some("70cm") && e[1].step == Some(25_000));
        assert_eq!(e[2].step, Some(8_330));
        assert!(
            entry_text(
                430_000_000,
                440_000_000,
                12_500.0,
                Modulation::Digi,
                12_500.0,
                false,
                None
            )
            .is_err()
        );
        // A missing file is created.
        let new = dir.join("new.txt");
        assert_eq!(append(&new, &line).unwrap(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
