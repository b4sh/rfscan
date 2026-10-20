//! `-h` and `--help`, rendered from the clap option definitions.
//!
//! `-h` lists the most used options (those without `hide_short_help`) with their brief
//! description (the first paragraph of the doc comment) in two columns. `--help` lists all
//! options with the full description below each option. Options are indented by 2 spaces; clap's
//! own help keeps a column for short options, so it is not used.

use clap::{Arg, ArgAction, CommandFactory};

use crate::cli::Cli;

/// Indentation of the options.
const INDENT: usize = 2;
/// Indentation of the descriptions under the options in `--help`.
const LONG_INDENT: usize = 6;
/// Widest option column in `-h`; a longer option gets its description on the next line.
const MAX_NAME: usize = 28;
/// Width used when the terminal width is unknown, and the limits.
const DEFAULT_WIDTH: usize = 100;
const MIN_WIDTH: usize = 60;
const MAX_WIDTH: usize = 120;

/// Whether the command line asks for help: `Some(false)` for `-h`, `Some(true)` for `--help`.
/// A value of `--dsd-fme-args` (which may start with `-`) is not a request for help.
pub fn requested(argv: &[std::ffi::OsString]) -> Option<bool> {
    let mut prev = None;
    for a in argv.iter().skip(1) {
        let a = a.to_str();
        if prev != Some("--dsd-fme-args") {
            match a {
                Some("--help") => return Some(true),
                Some("-h") => return Some(false),
                _ => {}
            }
        }
        prev = a;
    }
    None
}

/// The help text: `-h` (`long` false) or `--help`.
pub fn render(long: bool, width: usize) -> String {
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let cmd = Cli::command();
    let shown: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && (long || !a.is_hide_short_help_set()))
        .collect();
    let name_w = shown
        .iter()
        .map(|a| option_text(a).len())
        .filter(|&n| n <= MAX_NAME)
        .max()
        .unwrap_or(MAX_NAME);

    let mut out = String::new();
    out.push_str(&format!(
        "{} {}: {}\n\nUsage: rfscan [OPTIONS]\n",
        cmd.get_name(),
        cmd.get_version().unwrap_or(""),
        cmd.get_about().map(|s| s.to_string()).unwrap_or_default()
    ));
    let mut heading = None;
    for a in shown {
        if a.get_help_heading() != heading {
            heading = a.get_help_heading();
            out.push_str(&format!("\n{}:\n", heading.unwrap_or("Options")));
        }
        let name = option_text(a);
        if long {
            out.push_str(&format!("{}{name}\n", " ".repeat(INDENT)));
            for line in wrap(&with_default(a, &details(a)), width - LONG_INDENT) {
                out.push_str(&format!("{}{line}\n", " ".repeat(LONG_INDENT)));
            }
        } else {
            let text = a.get_help().map(|s| s.to_string()).unwrap_or_default();
            let col = INDENT + name_w + 2;
            let lines = wrap(&text, width - col);
            let mut lines = lines.iter();
            if name.len() <= name_w {
                let first = lines.next().map_or("", String::as_str);
                out.push_str(&format!("{}{name:<name_w$}  {first}\n", " ".repeat(INDENT)));
            } else {
                out.push_str(&format!("{}{name}\n", " ".repeat(INDENT)));
            }
            for line in lines {
                out.push_str(&format!("{}{line}\n", " ".repeat(col)));
            }
        }
    }
    out.push_str(if long {
        "\n  -h: the most used options with brief descriptions, -V: version\n\n"
    } else {
        "\n  --help: all options with details, -V: version\n\n"
    });
    out
}

/// The terminal width, or a default when the output is not a terminal.
pub fn terminal_width() -> usize {
    ratatui::crossterm::terminal::size().map_or(DEFAULT_WIDTH, |(w, _)| w as usize)
}

/// The detailed description of an option (`--help`, the configuration file): without the brief
/// first paragraph when there is a longer one after it (the doc comment's first paragraph is the
/// `-h` text).
pub fn details(a: &Arg) -> String {
    let long = a.get_long_help().map(|s| s.to_string());
    match long.as_deref().and_then(|l| l.split_once("\n\n")) {
        Some((_, rest)) => rest.to_string(),
        None => long
            .or(a.get_help().map(|s| s.to_string()))
            .unwrap_or_default(),
    }
}

/// `--name <VALUE>`, `--name [<VALUE>]` for an optional value, `--name` for a switch.
fn option_text(a: &Arg) -> String {
    let long = a.get_long().unwrap_or_default();
    if matches!(a.get_action(), ArgAction::SetTrue | ArgAction::SetFalse) {
        return format!("--{long}");
    }
    let value = a
        .get_value_names()
        .and_then(|v| v.first())
        .map_or_else(|| a.get_id().as_str().to_uppercase(), |v| v.to_string());
    let optional = a.get_num_args().is_some_and(|n| n.min_values() == 0);
    if optional {
        format!("--{long} [<{value}>]")
    } else {
        format!("--{long} <{value}>")
    }
}

/// The description with the default value, if the option has a non-empty one.
fn with_default(a: &Arg, text: &str) -> String {
    if matches!(a.get_action(), ArgAction::SetTrue | ArgAction::SetFalse) {
        return text.to_string();
    }
    let defaults: Vec<String> = a
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    if defaults.is_empty() {
        text.to_string()
    } else {
        format!("{text} [default: {}]", defaults.join(","))
    }
}

/// Word wrapping to `width` columns; paragraphs (blank lines) stay separate.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split("\n\n") {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_long_help() {
        let short = render(false, 100);
        let long = render(true, 100);
        println!("{short}\n{long}");
        // Options indented by 2 spaces, no column for short options.
        assert!(short.contains("\n  --gain <"));
        assert!(long.contains("\n  --gain <"));
        // -h has fewer options and lines than --help, and no line is wider than the width.
        assert!(short.lines().count() < long.lines().count() / 2);
        assert!(!short.contains("--dc-mask") && long.contains("--dc-mask"));
        assert!(long.lines().all(|l| l.chars().count() <= 100));
        assert!(short.lines().all(|l| l.chars().count() <= 100));
        assert!(long.contains("[default: 10]"));
    }

    #[test]
    fn help_requests() {
        let args = |v: &[&str]| {
            v.iter()
                .map(Into::into)
                .collect::<Vec<std::ffi::OsString>>()
        };
        assert_eq!(requested(&args(&["rfscan", "-h"])), Some(false));
        assert_eq!(
            requested(&args(&["rfscan", "--gain", "x", "--help"])),
            Some(true)
        );
        assert_eq!(requested(&args(&["rfscan", "--dsd-fme-args", "-h"])), None);
        assert_eq!(requested(&args(&["rfscan"])), None);
    }
}
