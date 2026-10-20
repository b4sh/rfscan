//! Configuration file `rfscan.conf`. TOML whose keys are the long command line
//! options without `--` and whose values use the command line syntax; lists stand for options
//! given multiple times, `true`/`false` for switches. The file values are turned into command
//! line arguments and parsed by clap together with the real command line, so they go through the
//! same parsers, conflicts and requirements. Precedence: command line > file > built-in defaults;
//! an option given on the command line (or one that conflicts with it) is taken from the command
//! line only, also for lists.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgMatches, Command, CommandFactory, FromArgMatches};
use toml::Spanned;

use crate::cli::Cli;

/// File name in the configuration directory.
pub const FILE_NAME: &str = "rfscan.conf";

/// Options that never come from the file: one-time actions, test options, temporary settings
/// (`--exclude`: permanent exclusions belong in the exclusion file) and the options that choose
/// the file itself.
const NOT_IN_FILE: &[&str] = &[
    "list-devices",
    "exclude",
    "bench",
    "calibrate",
    "record-iq",
    "report",
    "activity-reset",
    "duration",
    "power",
    "gen-conf",
    "config",
    "no-config",
    "help",
    "version",
];

/// Width of the comment lines written by `--gen-conf`.
const WRAP: usize = 98;

/// The default file: `~/.config/rfscan/rfscan.conf`.
pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_dir().map(|d| d.join(FILE_NAME))
}

/// Parses the command line merged with the configuration file. Returns the options and a line
/// for the log about the file used. Errors of the command line itself end the program the way
/// clap does (`--help` and `--version` too).
pub fn parse_args(argv: Vec<OsString>) -> Result<(Cli, Option<String>), String> {
    let cli_m = command()
        .try_get_matches_from(&argv)
        .unwrap_or_else(|e| e.exit());
    let given = |id: &str| cli_m.value_source(id) == Some(ValueSource::CommandLine);
    // --gen-conf does not read the file, so a broken file can always be written anew.
    if given("gen_conf") {
        return Ok((from_matches(&cli_m)?, None));
    }
    if given("no_config") {
        let note = "configuration file: none (--no-config)".to_string();
        return Ok((from_matches(&cli_m)?, Some(note)));
    }
    let explicit = cli_m.get_one::<PathBuf>("config").cloned();
    let Some(path) = explicit.clone().or_else(default_path) else {
        return Ok((from_matches(&cli_m)?, None));
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // A missing default file is not an error, a missing --config file is.
        Err(e) if explicit.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((from_matches(&cli_m)?, None));
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let cli = merge(&argv, &cli_m, &text, &path.display().to_string())?;
    Ok((cli, Some(format!("configuration file: {}", path.display()))))
}

/// Writes the file with all options that may be in it, the defaults commented out (`--gen-conf`),
/// overwriting an existing one. Without `file` it writes the default file and creates the
/// configuration directory; with `file` it creates no directories.
pub fn write(file: Option<&Path>) -> Result<PathBuf, String> {
    let path = match file {
        Some(p) => {
            if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty())
                && !dir.is_dir()
            {
                return Err(format!("the directory {} does not exist", dir.display()));
            }
            p.to_path_buf()
        }
        None => {
            let p = default_path().ok_or("cannot determine the home directory")?;
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            p
        }
    };
    std::fs::write(&path, generate()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Sets `key = value` lines in the configuration file `path` (`--calibrate`): an active line of
/// the key gets the new value, otherwise a commented one (`# key = ...`, e.g. from `--gen-conf`)
/// is uncommented with it, otherwise the line is appended. All other lines stay unchanged. A
/// missing file is created; its directory only when `create_dir` (the default file). Atomic
/// write: a temporary file and a rename.
pub fn set_values(path: &Path, create_dir: bool, values: &[(&str, String)]) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for (key, value) in values {
        let line = format!("{key} = {value}");
        let active = lines.iter().position(|l| line_key(l) == Some(key));
        let commented = || {
            lines
                .iter()
                .position(|l| l.trim_start().strip_prefix('#').and_then(line_key) == Some(key))
        };
        match active.or_else(commented) {
            Some(i) => lines[i] = line,
            None => lines.push(line),
        }
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if create_dir {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        } else if !dir.is_dir() {
            return Err(format!("the directory {} does not exist", dir.display()));
        }
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(&tmp, out).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The key of a `key = value` line (a bare TOML key), or `None`.
fn line_key(line: &str) -> Option<&str> {
    let key = line.split_once('=')?.0.trim();
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    bare.then_some(key)
}

fn command() -> Command {
    let mut cmd = Cli::command();
    cmd.build();
    cmd
}

fn from_matches(m: &ArgMatches) -> Result<Cli, String> {
    Cli::from_arg_matches(m).map_err(|e| first_line(&e))
}

/// The first line of a clap error, without `error: ` (the rest is the usage and a hint).
fn first_line(e: &clap::Error) -> String {
    let text = e.to_string();
    let line = text.lines().next().unwrap_or("");
    line.strip_prefix("error: ").unwrap_or(line).to_string()
}

/// The command line `argv` (already parsed into `cli_m`) merged with the file `text`; `name`
/// names the file in error messages.
fn merge(argv: &[OsString], cli_m: &ArgMatches, text: &str, name: &str) -> Result<Cli, String> {
    let cmd = command();
    let file = file_args(&cmd, cli_m, text, name)?;
    let mut all: Vec<OsString> = vec![argv.first().cloned().unwrap_or_else(|| "rfscan".into())];
    all.extend(file);
    all.extend(argv.iter().skip(1).cloned());
    // The command line alone was valid, so an error here comes from the file values.
    let m = cmd
        .try_get_matches_from(all)
        .map_err(|e| format!("{name}: {}", first_line(&e)))?;
    from_matches(&m)
}

/// The file values as command line arguments (`--key=value`, `--switch`), without the options
/// given on the command line or conflicting with it.
fn file_args(
    cmd: &Command,
    cli_m: &ArgMatches,
    text: &str,
    name: &str,
) -> Result<Vec<OsString>, String> {
    let table: BTreeMap<Spanned<String>, Spanned<toml::Value>> =
        toml::from_str(text).map_err(|e| format!("{name}: {e}"))?;
    let mut entries: Vec<_> = table.into_iter().collect();
    entries.sort_by_key(|(k, _)| k.span().start);

    let given: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| cli_m.value_source(a.get_id().as_str()) == Some(ValueSource::CommandLine))
        .collect();
    let given_ids: HashSet<&str> = given.iter().map(|a| a.get_id().as_str()).collect();
    let blocked: HashSet<&str> = given
        .iter()
        .flat_map(|a| cmd.get_arg_conflicts_with(a))
        .map(|a| a.get_id().as_str())
        .chain(given_ids.iter().copied())
        .collect();

    let mut out = Vec::new();
    for (key, value) in &entries {
        let long = key.get_ref().as_str();
        let line = 1 + text[..key.span().start].matches('\n').count();
        let at = |msg: &str| format!("{name}:{line}: {long}: {msg}");
        let Some(arg) = cmd.get_arguments().find(|a| a.get_long() == Some(long)) else {
            return Err(at("unknown option"));
        };
        if NOT_IN_FILE.contains(&long) {
            return Err(at("allowed only on the command line"));
        }
        let id = arg.get_id().as_str();
        let conflicts = cmd.get_arg_conflicts_with(arg);
        if blocked.contains(id)
            || conflicts
                .iter()
                .any(|c| given_ids.contains(c.get_id().as_str()))
        {
            continue;
        }
        // The value alone through the option's parser, so the error points to this line.
        let check = |v: &str| {
            Command::new("rfscan")
                .no_binary_name(true)
                .arg(alone(arg))
                .try_get_matches_from([v])
                .map(|_| OsString::from(format!("--{long}={v}")))
                .map_err(|e| at(&without_name(&first_line(&e))))
        };
        let optional_value = takes_optional_value(arg);
        match (arg.get_action(), value.get_ref()) {
            (ArgAction::SetTrue, toml::Value::Boolean(b)) => {
                if *b {
                    out.push(format!("--{long}").into());
                }
            }
            (ArgAction::SetTrue, _) => return Err(at("expected true or false")),
            (ArgAction::Append, toml::Value::Array(items)) => {
                for item in items {
                    let v = scalar(item).ok_or_else(|| at("expected a list of values"))?;
                    out.push(check(&v)?);
                }
            }
            (_, toml::Value::Boolean(true)) if optional_value => {
                out.push(format!("--{long}").into());
            }
            (_, v) => {
                let v = scalar(v).ok_or_else(|| at("expected a value (text or number)"))?;
                out.push(check(&v)?);
            }
        }
    }
    Ok(out)
}

/// A value option whose value may be missing (`--scan-channels` without a list).
fn takes_optional_value(arg: &Arg) -> bool {
    !matches!(arg.get_action(), ArgAction::SetTrue)
        && arg.get_num_args().is_some_and(|n| n.min_values() == 0)
}

/// `invalid value 'x' for '<NAME>': ...` without ` for '<NAME>'`: the message already names the
/// key.
fn without_name(msg: &str) -> String {
    if let Some(i) = msg.find(" for '")
        && let Some(j) = msg[i..].find("': ")
    {
        return format!("{}{}", &msg[..i], &msg[i + j + 1..]);
    }
    msg.to_string()
}

/// The value parser of an option as a positional argument, without the option's relations to
/// other options (conflicts, requirements).
fn alone(arg: &Arg) -> Arg {
    let mut a = Arg::new(arg.get_id().clone())
        .value_parser(arg.get_value_parser().clone())
        .action(ArgAction::Set)
        .allow_hyphen_values(true);
    if let Some(names) = arg.get_value_names() {
        a = a.value_names(names.to_vec());
    }
    a
}

/// Text or a number as on the command line.
fn scalar(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// The content of a new configuration file: every option allowed in the file with its help as
/// a comment and its default commented out.
fn generate() -> String {
    let cmd = command();
    let mut s = format!(
        "# rfscan configuration file, written by `rfscan --gen-conf` (rfscan {}).
# Keys are the long command line options without \"--\", values use the command line syntax.
# Uncomment a line to set it; the commented values are the built-in defaults.
# Command line options take precedence over this file; --no-config ignores it.
# Windows paths in single quotes (no escapes): dsd-fme = 'C:\\Radio\\dsd-fme\\dsd-fme.exe'
# Options given multiple times are lists: scan-range = [\"144M-146M\", \"430M-440M:25k\"]
# Switches: true or false.
",
        env!("CARGO_PKG_VERSION")
    );
    let mut heading = None;
    for arg in cmd.get_arguments() {
        let Some(long) = arg.get_long() else {
            continue;
        };
        if NOT_IN_FILE.contains(&long) || arg.is_hide_set() {
            continue;
        }
        if arg.get_help_heading() != heading {
            heading = arg.get_help_heading();
            // Two empty lines before a section.
            s.push_str(&format!("\n\n# --- {} ---\n", heading.unwrap_or("Options")));
        }
        s.push('\n');
        wrap_comment(&mut s, &crate::help::details(arg));
        if takes_optional_value(arg) {
            s.push_str(&format!("# {long} = true\n"));
        }
        s.push_str(&format!("# {long} = {}\n", example(arg)));
    }
    s
}

/// The commented value: the default, `false` for a switch, `[]` for a list, otherwise a
/// placeholder.
fn example(arg: &Arg) -> String {
    if matches!(arg.get_action(), ArgAction::SetTrue) {
        return "false".into();
    }
    if let Some(d) = arg.get_default_values().first() {
        let d = d.to_string_lossy();
        return if d.parse::<f64>().is_ok_and(f64::is_finite) {
            d.into_owned()
        } else {
            toml::Value::String(d.into_owned()).to_string()
        };
    }
    if matches!(arg.get_action(), ArgAction::Append) {
        return "[]".into();
    }
    let value_name = arg
        .get_value_names()
        .and_then(|n| n.first())
        .map_or_else(|| arg.get_id().as_str().to_uppercase(), |n| n.to_string());
    format!("\"<{value_name}>\"")
}

/// Appends `text` as `# ` comment lines wrapped at `WRAP` columns.
fn wrap_comment(s: &mut String, text: &str) {
    for para in text.lines() {
        let mut line = String::from("#");
        for word in para.split_whitespace() {
            if line.len() > 1 && line.len() + 1 + word.len() > WRAP {
                s.push_str(&line);
                s.push('\n');
                line = String::from("#");
            }
            line.push(' ');
            line.push_str(word);
        }
        s.push_str(&line);
        s.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str], text: &str) -> Result<Cli, String> {
        let argv: Vec<OsString> = std::iter::once("rfscan")
            .chain(args.iter().copied())
            .map(OsString::from)
            .collect();
        let cli_m = command()
            .try_get_matches_from(&argv)
            .map_err(|e| first_line(&e))?;
        merge(&argv, &cli_m, text, "test.conf")
    }

    #[test]
    fn command_line_wins_over_file() {
        let file = "squelch-snr = 5\nheadless = true\ndsd-fme-args = \"-fa -1 1234\"\n\
                    scan-range = [\"144M-146M\", \"430M-440M:25k\"]\n";
        let c = run(&[], file).unwrap();
        assert_eq!(c.squelch_snr, 5.0);
        assert!(c.headless);
        assert_eq!(c.dsd_fme_args, "-fa -1 1234");
        assert_eq!(c.scan_range.len(), 2);
        // The command line replaces the value, also a whole list.
        let c = run(
            &["--squelch-snr", "7", "--scan-range", "118M-137M:am"],
            file,
        )
        .unwrap();
        assert_eq!(c.squelch_snr, 7.0);
        assert_eq!(c.scan_range.len(), 1);
        assert_eq!(c.scan_range[0].start, 118_000_000);
        // A conflicting command line option drops the file value: no conflict error.
        let c = run(&["--scan-channels", "145.5M"], file).unwrap();
        assert!(c.scan_range.is_empty() && c.scan_channels.is_some());
        let c = run(&["--digi-tcp", "7355"], "digi = true\n").unwrap();
        assert!(!c.digi);
        // Negative numbers and values starting with a hyphen.
        let c = run(&[], "agc-target = -9\nsquelch-abs = -70.5\n").unwrap();
        assert_eq!((c.agc_target, c.squelch_abs), (-9.0, Some(-70.5)));
        // A value that may be missing: true = without a value.
        let c = run(&[], "scan-channels = true\n").unwrap();
        assert_eq!(c.scan_channels, Some(None));
    }

    #[test]
    fn errors_name_file_line_and_key() {
        let e = |text: &str| run(&[], text).unwrap_err();
        let err = e("# comment\nsquelch-snr = 5\nfoo = 1\n");
        assert!(err.starts_with("test.conf:3: foo:"), "{err}");
        let err = e("\nconfirm = \"maybe\"\n");
        assert!(
            err.starts_with("test.conf:2: confirm:") && err.contains("maybe"),
            "{err}"
        );
        let err = e("duration = 10\n");
        assert!(err.contains("only on the command line"), "{err}");
        let err = e("headless = \"yes\"\n");
        assert!(err.contains("true or false"), "{err}");
        // A conflict inside the file.
        let err = e("digi = true\ndigi-tcp = 7355\n");
        assert!(err.starts_with("test.conf: "), "{err}");
        // TOML syntax.
        let err = e("squelch-snr = \n");
        assert!(err.starts_with("test.conf: "), "{err}");
    }

    #[test]
    fn generated_file_gives_the_defaults() {
        let text = generate();
        println!("{text}");
        // All commented out: as without a file.
        let base = format!("{:?}", run(&[], "").unwrap());
        assert_eq!(format!("{:?}", run(&[], &text).unwrap()), base);
        // Every default uncommented: the same options.
        let cmd = command();
        let longs: Vec<&str> = cmd.get_arguments().filter_map(|a| a.get_long()).collect();
        let uncommented: String = text
            .lines()
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| {
                l.split_once(" = ").is_some_and(|(k, v)| {
                    longs.contains(&k) && !v.starts_with("\"<") && v != "true"
                })
            })
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(uncommented.contains("squelch-snr = 10\n"), "{uncommented}");
        assert!(uncommented.contains("dsd-fme-args = \"-fa\"\n"));
        assert_eq!(format!("{:?}", run(&[], &uncommented).unwrap()), base);
        for skipped in NOT_IN_FILE {
            assert!(!text.contains(&format!("# {skipped} =")), "{skipped}");
        }
    }

    #[test]
    fn set_values_keeps_other_lines() {
        let dir = std::env::temp_dir().join(format!("rfscan-set-{}", std::process::id()));
        let file = dir.join("a.conf");
        // A missing directory is created only for the default file.
        let values = [("settle", "18".to_string()), ("dwell", "0.69".to_string())];
        assert!(set_values(&file, false, &values).is_err());
        set_values(&file, true, &values).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "settle = 18\ndwell = 0.69\n"
        );
        // From --gen-conf: the commented lines are uncommented in place.
        std::fs::write(&file, generate()).unwrap();
        set_values(&file, false, &values).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\nsettle = 18\n") && text.contains("\ndwell = 0.69\n"));
        assert!(!text.contains("# settle =") && !text.contains("# dwell ="));
        assert_eq!(text.lines().count(), generate().lines().count());
        // An active line wins over a commented one; other lines unchanged.
        let old = "# settle = 20\ngain = \"agc=true\"\nsettle = 30 # old\n";
        std::fs::write(&file, old).unwrap();
        set_values(&file, false, &values[..1]).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "# settle = 20\ngain = \"agc=true\"\nsettle = 18\n"
        );
        // The result loads.
        let c = run(&[], &std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(c.settle, 18.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_only_into_existing_directories() {
        let dir = std::env::temp_dir().join(format!("rfscan-conf-{}", std::process::id()));
        let missing = dir.join("missing").join("x.conf");
        let err = write(Some(&missing)).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.conf");
        std::fs::write(&file, "old").unwrap();
        assert_eq!(write(Some(&file)).unwrap(), file);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .starts_with("# rfscan")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
