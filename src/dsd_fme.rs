//! DSD-FME started and supervised by rfscan (`--digi`).
//!
//! rfscan finds the program (`--dsd-fme` or `PATH`), starts it in the background without a
//! console window with the input, output and event log set by rfscan, and restarts it when it
//! exits during scanning, at most `MAX_RESTARTS` times per `RESTART_WINDOW`; after that the
//! digital mode stays off. DSD-FME's console output goes to a log file. When rfscan exits the
//! process is killed; DSD-FME also exits by itself when the TCP connection closes.

use std::collections::VecDeque;
use std::fs::File;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::digi_tcp::DigiEvent;

const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Pause before a restart.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// How often the supervisor checks the process and the stop flag.
const POLL: Duration = Duration::from_millis(200);

/// Process state for the TUI and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DsdState {
    Running = 0,
    Restarting = 1,
    /// Too many restarts: the digital mode is off.
    Failed = 2,
}

impl DsdState {
    pub fn load(a: &AtomicU8) -> Self {
        match a.load(Ordering::Relaxed) {
            0 => DsdState::Running,
            1 => DsdState::Restarting,
            _ => DsdState::Failed,
        }
    }
}

#[cfg(windows)]
const EXE: &str = "dsd-fme.exe";
#[cfg(not(windows))]
const EXE: &str = "dsd-fme";

/// The DSD-FME program: `explicit` (`--dsd-fme`), otherwise `dsd-fme` in `PATH`.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(format!("--dsd-fme {}: no such file", p.display()))
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join(EXE))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            format!(
                "--digi: {EXE} not found in PATH; give its location with --dsd-fme PATH (or add its directory to PATH)"
            )
        })
}

/// Start parameters.
pub struct DsdConfig {
    pub exe: PathBuf,
    /// Decoder arguments (`--dsd-fme-args`), without input, output and event log.
    pub args: Vec<String>,
    /// rfscan's discriminator TCP server and voice UDP port.
    pub tcp: SocketAddr,
    pub udp: SocketAddr,
    pub log_path: PathBuf,
    pub events_path: PathBuf,
}

impl DsdConfig {
    /// The full argument list.
    pub fn command_args(&self) -> Vec<String> {
        let mut a = self.args.clone();
        a.extend([
            "-i".into(),
            format!("tcp:{}:{}", self.tcp.ip(), self.tcp.port()),
            "-o".into(),
            format!("udp:{}:{}", self.udp.ip(), self.udp.port()),
            "-J".into(),
            self.events_path.display().to_string(),
        ]);
        a
    }

    fn spawn(&self, log: &File) -> Result<Child, String> {
        let out = log.try_clone().map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&self.exe);
        cmd.args(self.command_args())
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            /// `CREATE_NO_WINDOW`: no console window for the background process.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.exe.display()))
    }
}

pub struct DsdFme {
    pub state: Arc<AtomicU8>,
    /// The last console lines (for the TUI).
    pub console: ConsoleLines,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    console_handle: Option<JoinHandle<()>>,
}

impl Drop for DsdFme {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.console_handle.take() {
            let _ = h.join();
        }
    }
}

/// Lines of the DSD-FME console kept for the TUI window.
const CONSOLE_LINES: usize = 64;
/// How often the console log is read.
const CONSOLE_POLL: Duration = Duration::from_millis(100);

/// The last lines of the DSD-FME console (its log file), for the TUI window shown on `digi`
/// channels. Shown as they are; only ANSI escape sequences (colors) and other control
/// characters are removed.
pub type ConsoleLines = Arc<std::sync::Mutex<VecDeque<String>>>;

/// Removes ANSI escape sequences (`ESC [ ... letter`) and control characters.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // CSI: parameters up to the final letter; other escapes: one character.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
        } else if ch == '\t' {
            out.push(' ');
        } else if !ch.is_control() {
            out.push(ch);
        }
    }
    out.trim_end().to_string()
}

/// Follows the console log from its current end in a thread until `stop`.
fn follow_console(path: PathBuf, lines: ConsoleLines, stop: Arc<AtomicBool>) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = File::open(&path) else {
        return;
    };
    let _ = file.seek(SeekFrom::End(0));
    let mut buf = vec![0u8; 16_384];
    let mut partial: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match file.read(&mut buf) {
            Ok(0) | Err(_) => std::thread::sleep(CONSOLE_POLL),
            Ok(n) => {
                partial.extend_from_slice(&buf[..n]);
                // Complete lines end with \n (or \r, used for status updates in place).
                while let Some(i) = partial.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let raw: Vec<u8> = partial.drain(..=i).collect();
                    let line = clean(&String::from_utf8_lossy(&raw));
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(mut l) = lines.lock() {
                        if l.len() == CONSOLE_LINES {
                            l.pop_front();
                        }
                        l.push_back(line);
                    }
                }
            }
        }
    }
}
/// Starts DSD-FME (an error of the first start is returned at once) and the supervisor thread.
pub fn start(cfg: DsdConfig, events: Sender<DigiEvent>) -> Result<DsdFme, String> {
    let log = File::create(&cfg.log_path)
        .map_err(|e| format!("cannot create {}: {e}", cfg.log_path.display()))?;
    let console: ConsoleLines = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let console_handle = {
        let (path, lines, stop) = (cfg.log_path.clone(), console.clone(), stop.clone());
        std::thread::Builder::new()
            .name("rfscan-dsd-console".into())
            .spawn(move || follow_console(path, lines, stop))
            .map_err(|e| format!("cannot start the DSD-FME console reader: {e}"))?
    };
    let child = match cfg.spawn(&log) {
        Ok(c) => c,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            let _ = console_handle.join();
            return Err(e);
        }
    };
    let state = Arc::new(AtomicU8::new(DsdState::Running as u8));
    let handle = {
        let (state, stop) = (state.clone(), stop.clone());
        std::thread::Builder::new()
            .name("rfscan-dsd-fme".into())
            .spawn(move || supervise(cfg, log, child, &state, &stop, &events))
            .map_err(|e| format!("cannot start the DSD-FME supervisor: {e}"))?
    };
    Ok(DsdFme {
        state,
        console,
        stop,
        handle: Some(handle),
        console_handle: Some(console_handle),
    })
}

/// Sleeps `d` in short steps; `false` when the stop flag was set.
fn wait(d: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(POLL);
    }
    !stop.load(Ordering::Relaxed)
}

fn supervise(
    cfg: DsdConfig,
    log: File,
    mut child: Child,
    state: &AtomicU8,
    stop: &AtomicBool,
    events: &Sender<DigiEvent>,
) {
    let say = |m: String| {
        let _ = events.try_send(DigiEvent::Dsd(m));
    };
    let mut restarts: VecDeque<Instant> = VecDeque::new();
    loop {
        // Wait for the process to exit or for the stop flag.
        let status = loop {
            if stop.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            match child.try_wait() {
                Ok(Some(s)) => break s.to_string(),
                Ok(None) => std::thread::sleep(POLL),
                Err(e) => break e.to_string(),
            }
        };
        // Start again, unless it already exited too often.
        loop {
            let now = Instant::now();
            restarts.retain(|&t| now - t < RESTART_WINDOW);
            if restarts.len() >= MAX_RESTARTS {
                state.store(DsdState::Failed as u8, Ordering::Relaxed);
                say(format!(
                    "DSD-FME exited ({status}); {MAX_RESTARTS} restarts within {} s, digital mode off (see {})",
                    RESTART_WINDOW.as_secs(),
                    cfg.log_path.display()
                ));
                return;
            }
            state.store(DsdState::Restarting as u8, Ordering::Relaxed);
            say(format!(
                "DSD-FME exited ({status}), restarting in {} s",
                RESTART_DELAY.as_secs()
            ));
            if !wait(RESTART_DELAY, stop) {
                return;
            }
            restarts.push_back(Instant::now());
            match cfg.spawn(&log) {
                Ok(c) => {
                    child = c;
                    state.store(DsdState::Running as u8, Ordering::Relaxed);
                    say("DSD-FME restarted".into());
                    break;
                }
                Err(e) => say(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_set_input_output_and_events() {
        let cfg = DsdConfig {
            exe: PathBuf::from("dsd-fme"),
            args: vec!["-fa".into()],
            tcp: "127.0.0.1:50001".parse().unwrap(),
            udp: "127.0.0.1:50002".parse().unwrap(),
            log_path: PathBuf::from("dsd-fme.log"),
            events_path: PathBuf::from("events.log"),
        };
        assert_eq!(
            cfg.command_args(),
            [
                "-fa",
                "-i",
                "tcp:127.0.0.1:50001",
                "-o",
                "udp:127.0.0.1:50002",
                "-J",
                "events.log"
            ]
        );
    }

    #[test]
    fn console_lines_lose_only_colors() {
        assert_eq!(
            clean("\u{1b}[32m SLOT 1 TGT=1234 SRC=567 Group \u{1b}[0m\r"),
            " SLOT 1 TGT=1234 SRC=567 Group"
        );
        assert_eq!(clean("a\tb"), "a b");
    }

    #[test]
    fn missing_program_is_an_error() {
        let err = locate(Some(Path::new("no-such-dir/dsd-fme.exe")))
            .err()
            .unwrap();
        assert!(err.contains("no such file"), "{err}");
    }
}
