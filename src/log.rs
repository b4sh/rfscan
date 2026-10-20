//! Program output: console and an optional log file with levels.
//!
//! The console gets everything except the details written only to the file (`logfile!`). The
//! file (`--log PATH`, appended) gets lines with a level not higher than `--log-level`: `info` is
//! detected transmissions (LOCKED, returns, HANG) plus session start and end, `debug` is
//! everything. Each file line starts with the local date and time. Scanner events (`logev!`)
//! start with the local time on the console too.
//!
//! On a terminal the console lines are colored by their keyword; the file and
//! redirected output get plain text.
//!
//! Formatting goes to reusable buffers, so logging does not allocate after the first lines.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Instant;

/// Log line level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Detected transmissions (and session start/end).
    Info,
    /// Everything else: plan, warnings, status, per-second measurements.
    Debug,
}

impl Level {
    pub fn parse(s: &str) -> Result<Level, String> {
        match s.to_ascii_lowercase().as_str() {
            "info" => Ok(Level::Info),
            "debug" => Ok(Level::Debug),
            _ => Err(format!("'{s}': expected info or debug")),
        }
    }
}

/// Where a line goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Stdout,
    Stderr,
    /// The file only (details that would flood the console).
    File,
}

pub struct Log {
    file: Option<File>,
    level: Level,
    buf: String,
    /// The colored console line.
    paint: String,
    /// Whether lines go to the console (off while the TUI owns the terminal).
    console: bool,
    /// Colors on stdout and stderr: only when they are terminals.
    color_out: bool,
    color_err: bool,
}

impl Log {
    /// Console only.
    pub fn console() -> Self {
        Log {
            file: None,
            level: Level::Info,
            buf: String::new(),
            paint: String::new(),
            console: true,
            color_out: color_on(std::io::stdout().is_terminal()),
            color_err: color_on(std::io::stderr().is_terminal()),
        }
    }

    /// Turns console output on or off (the file is written either way).
    pub fn set_console(&mut self, on: bool) {
        self.console = on;
    }

    /// Console and file `path` (appended) with level `level`.
    pub fn open(path: &Path, level: Level) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("cannot open log file {}: {e}", path.display()))?;
        Ok(Log {
            file: Some(file),
            level,
            buf: String::with_capacity(256),
            ..Log::console()
        })
    }

    /// A line to the console (stdout or stderr) and, if the level allows, to the file.
    pub fn line(&mut self, level: Level, to_stderr: bool, args: std::fmt::Arguments) {
        let target = if to_stderr {
            Target::Stderr
        } else {
            Target::Stdout
        };
        self.write(level, target, None, args);
    }

    /// A line to the file only (with the level check), e.g. the window list of the plan.
    pub fn file_line(&mut self, level: Level, args: std::fmt::Arguments) {
        self.write(level, Target::File, None, args);
    }

    /// A scanner event that happened at `at`: on the console with the local time, in the file
    /// with the date and time of `at`.
    pub fn event(&mut self, level: Level, at: Instant, args: std::fmt::Arguments) {
        self.write(level, Target::Stdout, Some(at), args);
    }

    fn write(
        &mut self,
        level: Level,
        target: Target,
        at: Option<Instant>,
        args: std::fmt::Arguments,
    ) {
        self.buf.clear();
        let _ = self.buf.write_fmt(args);
        if at.is_some() {
            // Events: the keyword in a column of a fixed width, so what follows always starts in
            // the same place.
            self.paint.clear();
            let (word, rest) = self.buf.split_once(' ').unwrap_or((&self.buf, ""));
            let _ = write!(self.paint, "{word:<KEYWORD_W$} {}", rest.trim_start());
            std::mem::swap(&mut self.buf, &mut self.paint);
        }
        let time = at.map(local_time);
        // A console write error (e.g. a closed pipe) does not stop the scanner.
        if self.console && target != Target::File {
            let color = match target {
                Target::Stderr => self.color_err,
                _ => self.color_out,
            };
            self.paint.clear();
            if let Some(t) = &time {
                let _ = write!(
                    self.paint,
                    "{}  ",
                    Painted(color, DIM, &t.format("%H:%M:%S"))
                );
            }
            if color {
                paint_line(&mut self.paint, &self.buf, at.is_some());
            } else {
                self.paint.push_str(&self.buf);
            }
            let line = &self.paint;
            if target == Target::Stderr {
                let _ = writeln!(std::io::stderr().lock(), "{line}");
            } else {
                let _ = writeln!(std::io::stdout().lock(), "{line}");
            }
        }
        if level <= self.level
            && let Some(f) = &mut self.file
        {
            let now = time.unwrap_or_else(chrono::Local::now);
            let _ = writeln!(
                f,
                "{}  {}",
                now.format("%Y-%m-%d %H:%M:%S%.3f"),
                self.buf.trim_start()
            );
        }
    }
}

/// Local time of `at` (an instant in the past or now).
fn local_time(at: Instant) -> chrono::DateTime<chrono::Local> {
    let ago = Instant::now().saturating_duration_since(at);
    chrono::Local::now() - chrono::Duration::from_std(ago).unwrap_or_default()
}

/// Colors on a terminal. On Windows the console must be switched to ANSI escape codes; if that
/// fails, no colors.
fn color_on(terminal: bool) -> bool {
    #[cfg(windows)]
    {
        terminal && ratatui::crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        terminal
    }
}

/// Width of the keyword column of event lines (`recording` is the longest keyword).
const KEYWORD_W: usize = 9;

// ANSI colors (SGR codes).
const DIM: &str = "90";
const GREEN: &str = "1;32";
const BRIGHT_GREEN: &str = "1;92";
const LIGHT_GREEN: &str = "92";
const YELLOW: &str = "33";
const BOLD_YELLOW: &str = "1;33";
const RED: &str = "1;31";
const CYAN: &str = "36";
const BLUE: &str = "94";
const PINK: &str = "95";
const BOLD: &str = "1";

/// A text in a color, or plain when colors are off.
struct Painted<'a, T: std::fmt::Display>(bool, &'a str, &'a T);

impl<T: std::fmt::Display> std::fmt::Display for Painted<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 {
            write!(f, "\x1b[{}m{}\x1b[0m", self.1, self.2)
        } else {
            write!(f, "{}", self.2)
        }
    }
}

fn push(out: &mut String, color: &str, text: &str) {
    let _ = write!(out, "{}", Painted(true, color, &text));
}

/// Colors a console line by its keyword: scanner events (`event`) by their first word, other
/// lines by a warning or error prefix, the session markers, and `label: value` lines with a dim
/// label.
fn paint_line(out: &mut String, line: &str, event: bool) {
    if event {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "LOCKED" => {
                push(out, GREEN, word);
                // The frequency bold, the name from the channel file (in quotes) light green.
                let rest_trim = rest.trim_start();
                let pad = &rest[..rest.len() - rest_trim.len()];
                out.push(' ');
                out.push_str(pad);
                let (freq, tail) = match rest_trim.find(" MHz") {
                    Some(i) => rest_trim.split_at(i + 4),
                    None => ("", rest_trim),
                };
                push(out, BRIGHT_GREEN, freq);
                match tail.find("  \"") {
                    Some(i) => {
                        out.push_str(&tail[..i + 2]);
                        push(out, LIGHT_GREEN, &tail[i + 2..]);
                    }
                    None => out.push_str(tail),
                }
                return;
            }
            "HANG" => push(out, BOLD_YELLOW, word),
            "SCANNING" | "PAUSED" | "RESUMED" | "squelch" => push(out, CYAN, word),
            "EXCLUDED" | "REMOVED" | "UNDONE" | "MODE" => push(out, BLUE, word),
            "digi" => push(out, PINK, word),
            "recording" => push(out, RED, word),
            "rejected" | "status" => {
                push(out, DIM, line);
                return;
            }
            _ => out.push_str(word),
        }
        out.push(' ');
        out.push_str(rest);
        return;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("WARNING") {
        push(out, YELLOW, line);
    } else if trimmed.starts_with("ERROR") || trimmed.starts_with("Error") {
        push(out, RED, line);
    } else if trimmed.starts_with("===") {
        push(out, BOLD, line);
    } else if let Some(i) = label_end(line) {
        push(out, DIM, &line[..i]);
        out.push_str(&line[i..]);
    } else {
        out.push_str(line);
    }
}

/// End of a `label:` at the start of a line (words of letters and spaces before the colon).
fn label_end(line: &str) -> Option<usize> {
    let i = line.find(':')?;
    let label = line[..i].trim_start();
    (!label.is_empty() && label.chars().all(|c| c.is_ascii_alphabetic() || c == ' '))
        .then_some(i + 1)
}

/// `logln!(log, Level::Info, "...", args)`: a line to stdout and to the log file.
#[macro_export]
macro_rules! logln {
    ($log:expr, $level:expr, $($arg:tt)*) => {
        $log.line($level, false, format_args!($($arg)*))
    };
}

/// `logwarn!(log, "...", args)`: a warning to stderr, at debug level in the file.
#[macro_export]
macro_rules! logwarn {
    ($log:expr, $($arg:tt)*) => {
        $log.line($crate::log::Level::Debug, true, format_args!($($arg)*))
    };
}

/// `logev!(log, Level::Info, instant, "...", args)`: a scanner event at `instant`, with the
/// local time on the console.
#[macro_export]
macro_rules! logev {
    ($log:expr, $level:expr, $at:expr, $($arg:tt)*) => {
        $log.event($level, $at, format_args!($($arg)*))
    };
}

/// `logfile!(log, Level::Debug, "...", args)`: a line to the log file only.
#[macro_export]
macro_rules! logfile {
    ($log:expr, $level:expr, $($arg:tt)*) => {
        $log.file_line($level, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(level: Level) -> String {
        let dir = std::env::temp_dir().join(format!("rfscan-log-{}-{level:?}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rfscan.log");
        let _ = std::fs::remove_file(&path);
        {
            let mut log = Log::open(&path, level).unwrap();
            crate::logev!(log, Level::Info, Instant::now(), "LOCKED  transmission");
            crate::logln!(log, Level::Debug, "[status] detail");
            crate::logwarn!(log, "WARNING: warning");
            crate::logfile!(log, Level::Debug, "  window  0: file only");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        text
    }

    #[test]
    fn info_keeps_only_transmissions() {
        let t = written(Level::Info);
        assert_eq!(t.lines().count(), 1, "{t}");
        // Date and time first, then the content without leading spaces (no second time).
        assert!(
            t.contains(".") && t.contains("  LOCKED    transmission"),
            "{t}"
        );
        assert!(t.as_bytes()[4] == b'-' && t.as_bytes()[10] == b' ');
    }

    #[test]
    fn debug_keeps_everything() {
        let t = written(Level::Debug);
        assert_eq!(t.lines().count(), 4, "{t}");
        assert!(t.contains("[status] detail") && t.contains("WARNING: warning"));
        assert!(t.contains("window  0: file only"));
    }

    #[test]
    fn colors() {
        let paint = |line: &str, event: bool| {
            let mut s = String::new();
            paint_line(&mut s, line, event);
            s
        };
        let locked = paint(
            "LOCKED    145.525000 MHz  NFM   SNR  26.8 dB  (window A1)  \"CALLING\"",
            true,
        );
        assert!(locked.starts_with("\x1b[1;32mLOCKED\x1b[0m"));
        assert!(locked.contains("\x1b[1;92m145.525000 MHz\x1b[0m"));
        assert!(locked.contains("\x1b[92m\"CALLING\"\x1b[0m"));
        assert!(paint("rejected  145.6 MHz", true).starts_with("\x1b[90m"));
        assert!(paint("HANG      145.2 MHz", true).starts_with("\x1b[1;33mHANG"));
        assert_eq!(paint("mode: ranges", false), "\x1b[90mmode:\x1b[0m ranges");
        assert!(paint("WARNING: x", false).starts_with("\x1b[33m"));
        // A line without a label stays plain.
        assert_eq!(paint("  plain 1.2: x", false), "  plain 1.2: x");
    }
}
