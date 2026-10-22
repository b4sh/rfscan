//! Web GUI: an HTTP server with a page for phones and desktop browsers.
//!
//! Runs in its own thread with its own single-threaded tokio runtime, so it never slows down the
//! scanner. It is another consumer of the core events (`events.rs`): a pump task drains them
//! every 50 ms, turns them into JSON messages and broadcasts them to every connected browser
//! over a WebSocket; commands from the browsers go back to the scanner like the TUI keys. The
//! mixed scanner audio (after the 300-3000 Hz band-pass filter) is resampled to 8 kHz and sent
//! continuously as μ-law (G.711), also silence. Plain HTTP only: rfscan is meant for a home
//! network or a VPN, a token protects a non-loopback address.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::audio_dsp::Sos;
use crate::demod::Resampler;
use crate::dsd_fme::DsdState;
use crate::events::{Command, HistoryItem, LockView, ScanState, Spectrum, Status, UiEvent};
use crate::ui::{CpuMeter, UiContext};

/// The page: HTML, CSS and JavaScript in one file, bundled in the program.
const INDEX_HTML: &str = include_str!("index.html");
/// Icons made from the logo `docs/rfscan-logo.png`: the browser tab, iOS, Android (also
/// maskable: the logo within the safe zone on its background color). Served without the token:
/// they are public.
const ICONS: &[(&str, &[u8])] = &[
    ("favicon-32.png", include_bytes!("icons/favicon-32.png")),
    (
        "apple-touch-icon.png",
        include_bytes!("icons/apple-touch-icon.png"),
    ),
    ("icon-192.png", include_bytes!("icons/icon-192.png")),
    ("icon-512.png", include_bytes!("icons/icon-512.png")),
    (
        "icon-maskable-512.png",
        include_bytes!("icons/icon-maskable-512.png"),
    ),
];
/// Background of the dark theme: the browser and system bars of the web app.
const THEME_COLOR: &str = "#1a1a1a";
/// How often the pump drains the scanner events.
const PUMP_PERIOD: Duration = Duration::from_millis(50);
/// How often the audio tap is drained and encoded.
const AUDIO_PERIOD: Duration = Duration::from_millis(20);
/// Browser audio: 8 kHz μ-law in 20 ms frames.
const AUDIO_RATE: f64 = 8_000.0;
const AUDIO_FRAME: usize = 160;
/// Band of the browser audio.
const AUDIO_BAND: (f64, f64) = (300.0, 3000.0);
/// Anti-alias filter before 8 kHz: flat up to 3.4 kHz, at least 60 dB from 3.9 kHz (the
/// band-pass filter alone gives only 25 dB at 4 kHz, and the aliases sound metallic).
const ANTI_ALIAS: (f64, f64, f64) = (3400.0, 3900.0, 60.0);
/// Messages queued per browser; a slower browser skips the oldest ones.
const BROADCAST_QUEUE: usize = 256;
/// How many finished activities the history keeps (as in the TUI).
const HISTORY_LEN: usize = 200;
/// Longest channel name or exclusion note (as in the TUI).
const TEXT_MAX: usize = 40;
/// Noise reduction strength step (percent, as the TUI keys).
const NR_STEP: i32 = 10;
/// Largest squelch change per command (dB).
const SQUELCH_MAX_STEP: f32 = 10.0;
/// After the scanner finishes, wait at most this long for the server to close.
const JOIN_TIMEOUT: Duration = Duration::from_secs(3);

pub struct WebConfig {
    pub addr: SocketAddr,
    /// `--web-token`; generated when missing on a non-loopback address.
    pub token: Option<String>,
    /// `--local-audio`: without a local output the mute button is hidden.
    pub local_audio: bool,
    /// Sample rate of the audio tap.
    pub audio_rate: f64,
    /// `--exclude-wide`: adjacent channels excluded on each side by the wide exclusion.
    pub exclude_wide: u64,
}

/// The running server: the link to print and the thread to wait for.
pub struct WebServer {
    pub url: String,
    pub token_generated: bool,
    thread: Option<JoinHandle<()>>,
}

impl WebServer {
    /// Waits for the server thread (it ends when the scanner closes its event channel).
    pub fn join(mut self) {
        let Some(t) = self.thread.take() else {
            return;
        };
        let start = Instant::now();
        while !t.is_finished() && start.elapsed() < JOIN_TIMEOUT {
            std::thread::sleep(Duration::from_millis(20));
        }
        if t.is_finished() {
            let _ = t.join();
        }
    }
}

/// A message for the browsers.
#[derive(Clone)]
enum Out {
    Text(Utf8Bytes),
    Audio(Bytes),
}

/// The last state, sent to a browser when it connects.
struct Snapshot {
    hello: String,
    status: Option<Status>,
    lock: Option<LockView>,
    spectrum: Option<String>,
    history: VecDeque<HistoryItem>,
}

/// State shared by the pump and the browser connections.
struct Hub {
    ctx: Mutex<UiContext>,
    commands: Sender<Command>,
    token: Option<String>,
    snap: Mutex<Snapshot>,
    cpu: Mutex<CpuMeter>,
    tx: broadcast::Sender<Out>,
}

/// Starts the server: binds the address at once (a busy port is an error at startup), then
/// serves in its own thread until the scanner closes `events`.
pub fn start(
    cfg: WebConfig,
    ctx: UiContext,
    events: Receiver<UiEvent>,
    commands: Sender<Command>,
    audio: Option<rtrb::Consumer<f32>>,
) -> Result<WebServer, String> {
    let listener = std::net::TcpListener::bind(cfg.addr)
        .map_err(|e| format!("--web-gui {}: {e}", cfg.addr))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("--web-gui {}: {e}", cfg.addr))?;
    let bound = listener
        .local_addr()
        .map_err(|e| format!("--web-gui {}: {e}", cfg.addr))?;
    let token_generated = cfg.token.is_none() && !bound.ip().is_loopback();
    let token = cfg
        .token
        .clone()
        .or_else(|| token_generated.then(random_token));
    let url = page_url(bound, token.as_deref());
    let (tx, _) = broadcast::channel(BROADCAST_QUEUE);
    let hello = hello_json(&ctx, &cfg).to_string();
    let hub = Arc::new(Hub {
        ctx: Mutex::new(ctx),
        commands,
        token,
        snap: Mutex::new(Snapshot {
            hello,
            status: None,
            lock: None,
            spectrum: None,
            history: VecDeque::new(),
        }),
        cpu: Mutex::new(CpuMeter::new()),
        tx,
    });
    let audio_rate = cfg.audio_rate;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| format!("web GUI runtime: {e}"))?;
    let thread = std::thread::Builder::new()
        .name("rfscan-web".into())
        .spawn(move || {
            rt.block_on(serve(hub, listener, events, audio, audio_rate));
        })
        .map_err(|e| format!("cannot start the web GUI thread: {e}"))?;
    Ok(WebServer {
        url,
        token_generated,
        thread: Some(thread),
    })
}

async fn serve(
    hub: Arc<Hub>,
    listener: std::net::TcpListener,
    events: Receiver<UiEvent>,
    audio: Option<rtrb::Consumer<f32>>,
    audio_rate: f64,
) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("WARNING: web GUI: {e}");
            return;
        }
    };
    tokio::spawn(pump(hub.clone(), events, stop_tx));
    if let Some(rx) = audio {
        tokio::spawn(audio_task(hub.clone(), rx, audio_rate, stop_rx.clone()));
    }
    let app = Router::new()
        .route("/", get(index))
        .route("/manifest.json", get(manifest))
        .route("/icons/{name}", get(icon))
        .route("/ws", get(ws))
        .with_state((hub, stop_rx.clone()));
    let mut stop = stop_rx;
    let shutdown = async move {
        let _ = stop.wait_for(|s| *s).await;
    };
    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("WARNING: web GUI: {e}");
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    t: Option<String>,
}

impl Hub {
    fn authorized(&self, q: &TokenQuery) -> bool {
        match &self.token {
            None => true,
            Some(t) => q.t.as_deref() == Some(t.as_str()),
        }
    }

    /// `?t=TOKEN` for links of the page, or nothing without a token.
    fn token_query(&self) -> String {
        self.token
            .as_deref()
            .map_or_else(String::new, |t| format!("?t={t}"))
    }

    fn send(&self, v: Value) {
        let _ = self.tx.send(Out::Text(v.to_string().into()));
    }
}

type AppState = (Arc<Hub>, watch::Receiver<bool>);

async fn index(State((hub, _)): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !hub.authorized(&q) {
        return (
            StatusCode::FORBIDDEN,
            "rfscan: missing or wrong token; open the link printed by rfscan at startup",
        )
            .into_response();
    }
    // The manifest link carries the token, like every other request of the page.
    let page = INDEX_HTML.replace("{MANIFEST}", &format!("manifest.json{}", hub.token_query()));
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        page,
    )
        .into_response()
}

/// Web app manifest: a shortcut on the home screen opens the page as an app, without
/// the address bar where the browser allows it (Android only over HTTPS or on localhost). The
/// start address has the token, so the shortcut needs a fixed `--web-token`.
async fn manifest(State((hub, _)): State<AppState>, Query(q): Query<TokenQuery>) -> Response {
    if !hub.authorized(&q) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let body = json!({
        "name": "rfscan",
        "short_name": "rfscan",
        "description": "Radio scanner",
        "start_url": format!("/{}", hub.token_query()),
        "scope": "/",
        "display": "standalone",
        "background_color": THEME_COLOR,
        "theme_color": THEME_COLOR,
        "icons": [
            { "src": "/icons/icon-192.png", "sizes": "192x192", "type": "image/png", "purpose": "any" },
            { "src": "/icons/icon-512.png", "sizes": "512x512", "type": "image/png", "purpose": "any" },
            { "src": "/icons/icon-maskable-512.png", "sizes": "512x512", "type": "image/png", "purpose": "maskable" },
        ],
    });
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

async fn icon(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match ICONS.iter().find(|(n, _)| *n == name) {
        Some((_, data)) => (
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "max-age=86400"),
            ],
            *data,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws(
    State((hub, stop)): State<AppState>,
    Query(q): Query<TokenQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !hub.authorized(&q) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade.on_upgrade(move |socket| client(socket, hub, stop))
}

/// One browser: the snapshot first, then the broadcast messages; its commands go to the
/// scanner. Audio frames only after the browser asked for them.
async fn client(mut socket: WebSocket, hub: Arc<Hub>, mut stop: watch::Receiver<bool>) {
    let mut rx = hub.tx.subscribe();
    for msg in snapshot_messages(&hub) {
        if socket.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }
    let mut audio = false;
    loop {
        tokio::select! {
            m = socket.recv() => match m {
                Some(Ok(Message::Text(t))) => {
                    match serde_json::from_str::<ClientMsg>(t.as_str()) {
                        Ok(ClientMsg::Audio { on }) => audio = on,
                        Ok(cmd) => {
                            if let Some(reply) = handle(&hub, cmd)
                                && socket.send(Message::Text(reply.to_string().into())).await.is_err()
                            {
                                return;
                            }
                        }
                        Err(_) => {}
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
            o = rx.recv() => {
                let msg = match o {
                    Ok(Out::Text(t)) => Message::Text(t),
                    Ok(Out::Audio(b)) if audio => Message::Binary(b),
                    Ok(Out::Audio(_)) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if socket.send(msg).await.is_err() {
                    return;
                }
            }
            _ = stop.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                return;
            }
        }
    }
}

/// Commands from a browser.
#[derive(Deserialize)]
#[serde(tag = "c", rename_all = "snake_case")]
enum ClientMsg {
    Pause,
    Skip,
    Undo,
    Squelch {
        d: f32,
    },
    Nr,
    NrStep {
        d: i32,
    },
    Mute,
    Audio {
        on: bool,
    },
    Exclude {
        freq: u64,
        channel: Option<usize>,
        wide: bool,
        note: Option<String>,
    },
    Label {
        freq: u64,
        name: String,
        digi: bool,
    },
    ClearActivity,
    RemoveExclusion {
        freq: u64,
    },
    Activity,
}

/// Typed text: trimmed, without control characters, at most `TEXT_MAX` characters.
fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(TEXT_MAX)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Handles a command; returns a reply for this browser only, if any.
fn handle(hub: &Hub, cmd: ClientMsg) -> Option<Value> {
    let send = |c: Command| {
        let _ = hub.commands.send(c);
    };
    let Ok(mut ctx) = hub.ctx.lock() else {
        return Some(error_json("the web GUI state is not available"));
    };
    match cmd {
        ClientMsg::Pause => send(Command::TogglePause),
        ClientMsg::Skip => send(Command::Skip),
        ClientMsg::Undo => send(Command::UndoExclusion),
        ClientMsg::Squelch { d } if d.is_finite() => send(Command::Squelch(
            d.clamp(-SQUELCH_MAX_STEP, SQUELCH_MAX_STEP),
        )),
        ClientMsg::Squelch { .. } => {}
        ClientMsg::Nr => {
            ctx.nr.fetch_xor(true, Ordering::Relaxed);
            drop(ctx);
            resend_status(hub);
        }
        ClientMsg::NrStep { d } => {
            let step = d.signum() * NR_STEP;
            let _ = ctx
                .nr_strength
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |m| {
                    Some((m as i32 + step).clamp(0, 100) as u32)
                });
            drop(ctx);
            resend_status(hub);
        }
        ClientMsg::Mute => {
            ctx.muted.fetch_xor(true, Ordering::Relaxed);
            drop(ctx);
            resend_status(hub);
        }
        ClientMsg::Audio { .. } => {}
        ClientMsg::Exclude {
            freq,
            channel,
            wide,
            note,
        } => {
            // A plan channel index only when it is this frequency's channel.
            let channel = channel.filter(|&c| ctx.channel_freqs.get(c) == Some(&freq));
            let note = note.map(|n| clean_text(&n)).filter(|n| !n.is_empty());
            send(Command::Exclude {
                freq,
                channel,
                wide,
                note,
            });
        }
        ClientMsg::Label { freq, name, digi } => {
            let name = clean_text(&name);
            if name.is_empty() && !digi {
                return Some(error_json(
                    "the name is empty (with the digital mode it may be)",
                ));
            }
            let was = ctx.digi_near(freq);
            match ctx.set_label(freq, (!name.is_empty()).then_some(name.as_str()), digi) {
                Ok(written) => {
                    if digi != was {
                        send(Command::SetDigi {
                            freq: written,
                            digi,
                        });
                    }
                    let activity = activity_json(&ctx);
                    drop(ctx);
                    // The names in the history and the lock view change at once.
                    resend_history(hub);
                    return Some(activity);
                }
                Err(e) => return Some(error_json(&e)),
            }
        }
        ClientMsg::ClearActivity => {
            if let Err(e) = ctx.clear_activity() {
                return Some(error_json(&e));
            }
            return Some(activity_json(&ctx));
        }
        ClientMsg::RemoveExclusion { freq } => send(Command::RemoveExclusion(freq)),
        ClientMsg::Activity => return Some(activity_json(&ctx)),
    }
    None
}

fn error_json(msg: &str) -> Value {
    json!({ "t": "error", "msg": msg })
}

/// Drains the scanner events, keeps the snapshot and broadcasts the changes. Ends (and stops
/// the server) when the scanner closes the channel.
async fn pump(hub: Arc<Hub>, events: Receiver<UiEvent>, stop: watch::Sender<bool>) {
    let mut tick = tokio::time::interval(PUMP_PERIOD);
    loop {
        tick.tick().await;
        loop {
            match events.try_recv() {
                Ok(e) => apply(&hub, e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = stop.send(true);
                    return;
                }
            }
        }
    }
}

fn apply(hub: &Hub, e: UiEvent) {
    let Ok(ctx) = hub.ctx.lock() else {
        return;
    };
    let Ok(mut snap) = hub.snap.lock() else {
        return;
    };
    match e {
        UiEvent::Status(s) => {
            snap.status = Some(s);
            if let Ok(mut cpu) = hub.cpu.lock() {
                cpu.update();
            }
            hub.send(status_json(&ctx, &s, hub));
        }
        UiEvent::Spectrum(s) => {
            let v = spectrum_json(&ctx, &s).to_string();
            snap.spectrum = Some(v.clone());
            let _ = hub.tx.send(Out::Text(v.into()));
        }
        UiEvent::Lock(l) => {
            snap.lock = Some(l);
            hub.send(lock_json(&ctx, &l));
        }
        UiEvent::Unlock => {
            snap.lock = None;
            hub.send(json!({ "t": "unlock" }));
        }
        UiEvent::History(h) => {
            snap.history.push_front(h);
            snap.history.truncate(HISTORY_LEN);
            hub.send(json!({ "t": "history_add", "item": history_item_json(&ctx, &h) }));
        }
        UiEvent::ExclusionsChanged => hub.send(exclusions_json(&ctx)),
    }
}

/// Sends the status again at once (after a change of the mute or the noise reduction).
fn resend_status(hub: &Hub) {
    let (Ok(ctx), Ok(snap)) = (hub.ctx.lock(), hub.snap.lock()) else {
        return;
    };
    if let Some(s) = snap.status {
        hub.send(status_json(&ctx, &s, hub));
    }
}

/// Sends the whole history and the lock view again (after a channel was named).
fn resend_history(hub: &Hub) {
    let (Ok(ctx), Ok(snap)) = (hub.ctx.lock(), hub.snap.lock()) else {
        return;
    };
    hub.send(history_json(&ctx, &snap.history));
    if let Some(l) = snap.lock {
        hub.send(lock_json(&ctx, &l));
    }
}

/// Messages for a browser that just connected.
fn snapshot_messages(hub: &Hub) -> Vec<String> {
    let (Ok(ctx), Ok(snap)) = (hub.ctx.lock(), hub.snap.lock()) else {
        return Vec::new();
    };
    let mut out = vec![snap.hello.clone()];
    if let Some(s) = snap.status {
        out.push(status_json(&ctx, &s, hub).to_string());
    }
    out.extend(snap.spectrum.clone());
    out.push(match snap.lock {
        Some(l) => lock_json(&ctx, &l).to_string(),
        None => json!({ "t": "unlock" }).to_string(),
    });
    out.push(history_json(&ctx, &snap.history).to_string());
    out.push(exclusions_json(&ctx).to_string());
    out
}

/// What does not change during the run.
fn hello_json(ctx: &UiContext, cfg: &WebConfig) -> Value {
    json!({
        "t": "hello",
        "version": env!("CARGO_PKG_VERSION"),
        "source": env!("CARGO_PKG_REPOSITORY"),
        "device": ctx.device,
        "rate": format!("{} MSps", ctx.sample_rate / 1e6),
        "gain": ctx.gain,
        "ranges": ctx.ranges.len(),
        "local_audio": cfg.local_audio,
        "digi": ctx.digi.is_some(),
        "exclude_wide": cfg.exclude_wide,
    })
}

fn round1(x: f32) -> f64 {
    (x as f64 * 10.0).round() / 10.0
}

/// Digi output state: `off`, `waiting`, `connected`, `voice`, `restarting`, `failed` (as in the
/// TUI statistics).
fn digi_state(ctx: &UiContext) -> &'static str {
    match ctx.dsd.as_deref().map(DsdState::load) {
        Some(DsdState::Failed) => return "failed",
        Some(DsdState::Restarting) => return "restarting",
        _ => {}
    }
    if ctx.voice.as_ref().is_some_and(|v| v.playing()) {
        return "voice";
    }
    match &ctx.digi {
        None => "off",
        Some((_, st)) if st.connected.load(Ordering::Relaxed) => "connected",
        Some(_) => "waiting",
    }
}

fn status_json(ctx: &UiContext, s: &Status, hub: &Hub) -> Value {
    let w = s.window.min(ctx.window_range.len().saturating_sub(1));
    let r = ctx.window_range.get(w).copied().unwrap_or(0);
    let range = ctx.ranges.get(r);
    // The position bar: the current window, or every window of this range measured since the
    // previous status (fast scanning), as in the TUI.
    let first = (w + 1).saturating_sub(s.visited.max(1));
    let span = (first..=w)
        .filter(|&i| ctx.window_range.get(i) == Some(&r))
        .filter_map(|i| ctx.window_span.get(i).copied())
        .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)));
    let pos = match (range, span) {
        (Some(rg), Some((a, b))) if rg.stop > rg.start => {
            let len = (rg.stop - rg.start) as f64;
            let x0 = a.saturating_sub(rg.start) as f64 / len;
            let x1 = b.saturating_sub(rg.start).min(rg.stop - rg.start) as f64 / len;
            json!([x0, (x1 - x0).max(0.01)])
        }
        _ => json!([0.0, 1.0]),
    };
    let name = ctx.window_names.get(w).map_or("", String::as_str);
    // The window being measured when the status was sent, also during fast scanning (unlike the
    // TUI, which shows `-` then): the position bar shows all the windows measured meanwhile.
    let (pass, window) = if name.is_empty() {
        ("-".to_string(), "-".to_string())
    } else {
        (name[..1].to_string(), name[1..].to_string())
    };
    let (cpu, temp, ram) = hub
        .cpu
        .lock()
        .map_or((None, None, None), |c| (c.percent, c.temp, c.ram));
    let state = match s.state {
        ScanState::Scanning => "scanning",
        ScanState::Locked => "locked",
        ScanState::Hang => "hang",
        ScanState::Paused => "paused",
    };
    json!({
        "t": "status",
        "state": state,
        "range": r + 1,
        "span": range.map_or("", |r| r.span.as_str()),
        "pos": pos,
        "pass": pass,
        "window": window,
        "cycles": s.cycles,
        "mhz_s": s.mhz_per_s.round(),
        "w_s": s.windows_per_s.round(),
        "ovf": s.overflows,
        "timeouts": s.timeouts,
        "rejected": s.rejected,
        "squelch": round1(s.squelch_snr),
        "hyst": round1(s.hysteresis),
        "muted": ctx.muted.load(Ordering::Relaxed),
        "nr": ctx.nr.load(Ordering::Relaxed),
        "nr_strength": ctx.nr_strength.load(Ordering::Relaxed),
        "underruns": ctx.underruns.load(Ordering::Relaxed),
        "cpu": cpu.map(|p| p.round()),
        "temp": temp.map(|t| t.round()),
        "ram_mb": ram.map(|b| b >> 20),
        "exclusions": ctx.exclusions.lock().map_or(0, |l| l.len()),
        "digi": digi_state(ctx),
    })
}

fn spectrum_json(ctx: &UiContext, s: &Spectrum) -> Value {
    let n = s.bins.min(s.snr_db.len());
    let snr: Vec<Value> = s.snr_db[..n]
        .iter()
        .map(|&x| {
            if x.is_finite() {
                json!(round1(x))
            } else {
                Value::Null
            }
        })
        .collect();
    let ex: Vec<u8> = s.excluded[..n].iter().map(|&e| u8::from(e)).collect();
    json!({
        "t": "spectrum",
        "snr": snr,
        "ex": ex,
        "first": ctx.freq_label(s.first_hz),
        "last": ctx.freq_label(s.last_hz),
        "lo": crate::activity::format_mhz(s.lo_hz),
        "strongest": s.strongest.map(|(f, snr)| json!({
            "label": ctx.freq_label(f),
            "snr": round1(snr),
        })),
    })
}

/// The `DIGI` line: the DSD-FME state and the decoded voice (as in the TUI).
fn digi_text(ctx: &UiContext) -> String {
    let Some((addr, st)) = &ctx.digi else {
        return "DIGI · no digital output".to_string();
    };
    let dsd = ctx.dsd.as_deref().map(DsdState::load);
    let mut t = match dsd {
        Some(_) => "DIGI · DSD-FME".to_string(),
        None => format!("DIGI · DSD-FME {addr}"),
    };
    t.push_str(match dsd {
        Some(DsdState::Failed) => " off (too many restarts)",
        Some(DsdState::Restarting) => " restarting",
        _ if st.connected.load(Ordering::Relaxed) => " connected",
        _ => " waiting",
    });
    if let Some(v) = &ctx.voice {
        if v.playing() {
            let rate = v.rate.load(Ordering::Relaxed);
            let stereo = if v.channels.load(Ordering::Relaxed) == 2 {
                " stereo"
            } else {
                ""
            };
            if rate > 0 {
                t.push_str(&format!(" · voice {} kHz{stereo}", rate / 1000));
            } else {
                t.push_str(" · voice playing");
            }
        } else {
            t.push_str(" · voice -");
        }
    }
    t
}

fn lock_json(ctx: &UiContext, l: &LockView) -> Value {
    let now = Instant::now();
    let raster25 = ctx.is_raster25(l.freq);
    let actual = crate::airband::is_airband(l.freq).then(|| {
        format!(
            "frequency: {} MHz ({} kHz channel)",
            crate::airband::actual(l.freq),
            if raster25 { "25" } else { "8.33" }
        )
    });
    let digi = l.mode == crate::plan::Modulation::Digi;
    json!({
        "t": "lock",
        "freq": l.freq,
        "channel": l.channel,
        "label": ctx.freq_label(l.freq),
        "actual": actual,
        "name": ctx.names.get(l.channel).cloned().flatten(),
        "mode": l.mode.name(),
        "bw": format!("{} kHz", khz(l.bw)),
        "rssi": round1(l.rssi_db),
        "noise": round1(l.noise_db),
        "snr": round1(l.rssi_db - l.noise_db),
        "peak_snr": round1(l.peak_snr_db),
        "peak_rssi": round1(l.peak_rssi_db),
        "elapsed": now.saturating_duration_since(l.start).as_secs_f64().round(),
        "hang_left": l.hang.map(|(at, pause)| {
            (pause.saturating_sub(now.saturating_duration_since(at)).as_secs_f64() * 10.0).round() / 10.0
        }),
        "agc": l.agc_db.map(round1),
        "digi": digi.then(|| digi_text(ctx)),
    })
}

/// Frequency in kHz rounded to 2 decimal places, without trailing zeros (8.33, 12.5, 25).
fn khz(hz: f64) -> String {
    let s = format!("{:.2}", hz / 1e3);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn history_item_json(ctx: &UiContext, h: &HistoryItem) -> Value {
    json!({
        "time": h.at.format("%H:%M:%S").to_string(),
        "freq": h.freq,
        "channel": h.channel,
        "label": ctx.freq_label(h.freq),
        "name": ctx.names.get(h.channel).cloned().flatten(),
        "mode": h.mode.name(),
        "snr": round1(h.peak_snr_db),
        "rssi": round1(h.peak_rssi_db),
        "dur": format_duration(h.duration),
    })
}

fn history_json(ctx: &UiContext, history: &VecDeque<HistoryItem>) -> Value {
    let items: Vec<Value> = history.iter().map(|h| history_item_json(ctx, h)).collect();
    json!({ "t": "history", "items": items })
}

fn activity_json(ctx: &UiContext) -> Value {
    let seconds = |s: f64| format_duration(Duration::from_secs_f64(s.max(0.0)));
    let items: Vec<Value> = ctx
        .activity_rows()
        .iter()
        .map(|r| {
            json!({
                "freq": r.freq,
                "label": ctx.freq_label(r.freq),
                "name": ctx.name_near(r.freq),
                "mode": ctx.activity_mode(r),
                "total": seconds(r.total),
                "session": seconds(r.session),
                "hits": r.hits,
                // `last` is ISO 8601 local time: the date and the time without seconds.
                "last": r.last.get(..16).unwrap_or(&r.last).replace('T', " "),
            })
        })
        .collect();
    json!({ "t": "activity", "items": items })
}

fn exclusions_json(ctx: &UiContext) -> Value {
    let items: Vec<Value> = ctx
        .exclusion_list()
        .iter()
        .map(|(f, note)| {
            json!({
                "freq": f,
                "label": ctx.freq_label(*f),
                "name": ctx.name_near(*f),
                "note": note,
            })
        })
        .collect();
    json!({ "t": "exclusions", "items": items })
}

/// Drains the audio tap: band-pass, anti-alias low-pass, resampling to 8 kHz, μ-law, 20 ms
/// frames to the browsers.
async fn audio_task(
    hub: Arc<Hub>,
    mut rx: rtrb::Consumer<f32>,
    rate: f64,
    mut stop: watch::Receiver<bool>,
) {
    let mut band = Sos::bandpass(AUDIO_BAND.0, AUDIO_BAND.1, rate);
    let mut anti_alias = Sos::lowpass(ANTI_ALIAS.0, ANTI_ALIAS.1, ANTI_ALIAS.2, rate);
    let mut resampler = Resampler::new(rate, AUDIO_RATE);
    let mut frame: Vec<u8> = Vec::with_capacity(AUDIO_FRAME);
    let mut tick = tokio::time::interval(AUDIO_PERIOD);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = stop.changed() => return,
        }
        let Ok(chunk) = rx.read_chunk(rx.slots()) else {
            continue;
        };
        for x in chunk {
            resampler.push(anti_alias.process(band.process(x)), |y| {
                frame.push(mulaw(y));
                if frame.len() == AUDIO_FRAME {
                    let _ = hub.tx.send(Out::Audio(Bytes::from(std::mem::replace(
                        &mut frame,
                        Vec::with_capacity(AUDIO_FRAME),
                    ))));
                }
            });
        }
    }
}

/// G.711 μ-law encoding of a sample in ±1.
fn mulaw(x: f32) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let s = (x.clamp(-1.0, 1.0) * 32767.0) as i32;
    let sign = if s < 0 { 0x80 } else { 0 };
    let mag = (s.abs().min(CLIP) + BIAS) as u32;
    // Segment: the position of the highest set bit above bit 7.
    let exponent = (31 - mag.leading_zeros()).saturating_sub(7).min(7);
    let mantissa = (mag >> (exponent + 3)) & 0x0F;
    !(sign | (exponent << 4) as u8 | mantissa as u8)
}

/// A random token from the standard library's randomly seeded hasher (no extra dependency;
/// enough for a home network).
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    (0..2u64)
        .map(|i| {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(i);
            h.write_u128(nanos);
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// The link printed at startup. For an address of all interfaces (`0.0.0.0`) the address of
/// the interface with the default route is shown.
fn page_url(addr: SocketAddr, token: Option<&str>) -> String {
    let host = if addr.ip().is_unspecified() {
        local_ip().map_or_else(|| addr.ip().to_string(), |ip| ip.to_string())
    } else {
        addr.ip().to_string()
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    match token {
        Some(t) => format!("http://{host}:{}/?t={t}", addr.port()),
        None => format!("http://{host}:{}/", addr.port()),
    }
}

/// The local address of the default route: a UDP socket "connected" to a documentation address
/// (TEST-NET-1); connecting a UDP socket sends nothing.
fn local_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes μ-law (the page does the same in JavaScript).
    fn decode(u: u8) -> f32 {
        let u = !u;
        let sign = u & 0x80;
        let exponent = ((u >> 4) & 7) as i32;
        let mantissa = (u & 0x0F) as i32;
        let mag = (((mantissa << 3) + 0x84) << exponent) - 0x84;
        (if sign != 0 { -mag } else { mag }) as f32 / 32768.0
    }

    #[test]
    fn mulaw_round_trip() {
        assert_eq!(mulaw(0.0), 0xFF);
        for x in [-1.0f32, -0.5, -0.01, 0.001, 0.02, 0.3, 0.9, 1.0] {
            let y = decode(mulaw(x));
            // μ-law keeps about 3 % relative precision (plus a small absolute step near 0).
            assert!((y - x).abs() <= 0.035 * x.abs() + 0.001, "{x} -> {y}");
        }
    }

    /// The anti-alias filter keeps the band and removes what would alias at 8 kHz.
    #[test]
    fn anti_alias_filter() {
        for fs in [44_100.0, 48_000.0] {
            let f = Sos::lowpass(ANTI_ALIAS.0, ANTI_ALIAS.1, ANTI_ALIAS.2, fs);
            assert!(f.attenuation_db(3000.0, fs) < 1.0);
            assert!(f.attenuation_db(ANTI_ALIAS.0, fs) <= 3.01);
            let mut hz = 4000.0;
            while hz < fs / 2.0 {
                assert!(f.attenuation_db(hz, fs) >= 59.9, "{fs}: {hz} Hz");
                hz += 50.0;
            }
        }
    }

    #[test]
    fn token_and_url() {
        let t = random_token();
        assert_eq!(t.len(), 32);
        assert_ne!(t, random_token());
        let a: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(page_url(a, None), "http://127.0.0.1:8080/");
        assert_eq!(page_url(a, Some("abc")), "http://127.0.0.1:8080/?t=abc");
    }

    #[test]
    fn client_messages() {
        let m: ClientMsg = serde_json::from_str(
            r#"{"c":"exclude","freq":145500000,"channel":3,"wide":true,"note":"x"}"#,
        )
        .unwrap();
        assert!(matches!(
            m,
            ClientMsg::Exclude {
                freq: 145_500_000,
                channel: Some(3),
                wide: true,
                ..
            }
        ));
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(r#"{"c":"nr_step","d":-1}"#).unwrap(),
            ClientMsg::NrStep { d: -1 }
        ));
        assert_eq!(clean_text("  a\u{7}b  "), "ab");
    }
}
