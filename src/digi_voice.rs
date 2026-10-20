//! Decoded digital voice from DSD-FME over UDP.
//!
//! DSD-FME with `-o udp:HOST:PORT` sends its decoded audio as raw PCM s16le datagrams: 8 kHz
//! stereo in the auto mode `-fa` and the other TDMA modes (DMR slot 1 left, slot 2 right), 8 kHz
//! mono in some modes, 48 kHz mono for analog audio. A thread receives them, mixes stereo to
//! mono, converts the samples to f32, resamples them to the audio output rate and pushes them
//! into the voice buffer of the audio output (`audio`), which mixes them with the analog audio.
//! The thread never blocks the scanner or the DSP: when the voice buffer is full, samples are
//! dropped and counted.
//!
//! The format is not in the stream: it is estimated from the byte rate over the first ~0.5 s
//! of voice (8 kHz mono = 16,000 B/s, 8 kHz stereo = 32,000 B/s, 48 kHz mono = 96,000 B/s).
//! A given sample rate (`--digi-audio-udp ...:RATE`) leaves only mono or stereo to estimate.
//! The bytes received during the estimation are kept and played after it.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::demod::Resampler;

/// Receive timeout: how often the thread checks the stop flag.
const POLL: Duration = Duration::from_millis(100);
/// Voice measured to estimate the sample rate.
const ESTIMATE: Duration = Duration::from_millis(500);
/// A pause longer than this restarts the estimation (the voice came in bursts).
const ESTIMATE_GAP: Duration = Duration::from_millis(200);
/// Byte rate between 8 kHz mono (16,000 B/s) and 8 kHz stereo (32,000 B/s).
const STEREO_SPLIT_BPS: f64 = 24_000.0;
/// Byte rate between 8 kHz stereo (32,000 B/s) and 48 kHz mono (96,000 B/s).
const RATE_SPLIT_BPS: f64 = 60_000.0;
/// Bytes kept during the estimation (1 s at 48 kHz).
const ESTIMATE_BUFFER: usize = 96_000;
/// Gain of the decoded voice (-6 dB): DSD-FME's voice is louder than the analog audio after
/// its AGC (user's listening, 2026-10-19).
const VOICE_GAIN: f32 = 0.5;
/// The voice counts as playing this long after the last datagram (for the TUI).
pub const PLAYING_HOLD: Duration = Duration::from_millis(300);

/// Receiver statistics, read by the scanner and the TUI.
pub struct VoiceStats {
    start: Instant,
    /// Time of the last datagram in ms since `start` (0: none yet).
    last_ms: AtomicU64,
    pub datagrams: AtomicU64,
    pub bytes: AtomicU64,
    /// Samples dropped because the voice buffer was full.
    pub dropped: AtomicU64,
    /// Detected or given sample rate (0: not known yet).
    pub rate: AtomicU32,
    /// Detected number of channels, 1 or 2 (0: not known yet).
    pub channels: AtomicU32,
}

impl VoiceStats {
    fn new() -> Self {
        VoiceStats {
            start: Instant::now(),
            last_ms: AtomicU64::new(0),
            datagrams: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
        }
    }

    /// Voice received within the last `PLAYING_HOLD`.
    pub fn playing(&self) -> bool {
        let last = self.last_ms.load(Ordering::Relaxed);
        last > 0
            && self.start.elapsed().as_millis() as u64 <= last + PLAYING_HOLD.as_millis() as u64
    }
}

pub struct VoiceReceiver {
    pub addr: SocketAddr,
    pub stats: Arc<VoiceStats>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for VoiceReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Binds the UDP socket and starts the receiving thread. `rate`: the given sample rate, or
/// `None` to estimate it. `out_rate`: the audio output rate.
pub fn start(
    addr: SocketAddr,
    rate: Option<u32>,
    out_rate: f64,
    out: rtrb::Producer<f32>,
) -> Result<VoiceReceiver, String> {
    let socket =
        UdpSocket::bind(addr).map_err(|e| format!("cannot receive the voice on {addr}: {e}"))?;
    socket
        .set_read_timeout(Some(POLL))
        .map_err(|e| format!("voice socket: {e}"))?;
    let addr = socket.local_addr().unwrap_or(addr);
    let stats = Arc::new(VoiceStats::new());
    if let Some(r) = rate {
        stats.rate.store(r, Ordering::Relaxed);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (stats, stop) = (stats.clone(), stop.clone());
        std::thread::Builder::new()
            .name("rfscan-voice".into())
            .spawn(move || Receiver::new(socket, stats, out, out_rate, rate).run(&stop))
            .map_err(|e| format!("cannot start the voice thread: {e}"))?
    };
    Ok(VoiceReceiver {
        addr,
        stats,
        stop,
        handle: Some(handle),
    })
}

/// Sample rate and number of channels from the byte rate of s16 samples; `rate`: the given
/// sample rate.
fn format_from_bps(bps: f64, rate: Option<u32>) -> (u32, usize) {
    match rate {
        // Between mono (2 * rate B/s) and stereo (4 * rate B/s).
        Some(r) => (r, if bps < 3.0 * r as f64 { 1 } else { 2 }),
        None if bps < STEREO_SPLIT_BPS => (8_000, 1),
        None if bps < RATE_SPLIT_BPS => (8_000, 2),
        None => (48_000, 1),
    }
}

struct Receiver {
    socket: UdpSocket,
    stats: Arc<VoiceStats>,
    out: rtrb::Producer<f32>,
    out_rate: f64,
    /// The given sample rate.
    rate: Option<u32>,
    /// Known after the estimation.
    resampler: Option<Resampler>,
    channels: usize,
    /// Bytes of an incomplete frame (one sample per channel) left from the last datagram.
    carry: [u8; 4],
    carry_len: usize,
    /// Estimation: first datagram time, last datagram time and the kept bytes.
    estimate_from: Option<Instant>,
    estimate_last: Instant,
    kept: Vec<u8>,
}

impl Receiver {
    fn new(
        socket: UdpSocket,
        stats: Arc<VoiceStats>,
        out: rtrb::Producer<f32>,
        out_rate: f64,
        rate: Option<u32>,
    ) -> Self {
        Receiver {
            socket,
            stats,
            out,
            out_rate,
            rate,
            resampler: None,
            channels: 1,
            carry: [0; 4],
            carry_len: 0,
            estimate_from: None,
            estimate_last: Instant::now(),
            kept: Vec::with_capacity(ESTIMATE_BUFFER),
        }
    }

    fn run(mut self, stop: &AtomicBool) {
        // The largest UDP datagram; allocated once.
        let mut buf = vec![0u8; 65_536];
        while !stop.load(Ordering::Relaxed) {
            let Ok(n) = self.socket.recv(&mut buf) else {
                continue;
            };
            let now = Instant::now();
            let s = &self.stats;
            s.datagrams.fetch_add(1, Ordering::Relaxed);
            s.bytes.fetch_add(n as u64, Ordering::Relaxed);
            s.last_ms.store(
                s.start.elapsed().as_millis().max(1) as u64,
                Ordering::Relaxed,
            );
            if self.resampler.is_some() {
                self.play(&buf[..n]);
            } else {
                self.estimate(&buf[..n], now);
            }
        }
    }

    /// Collects voice until the sample rate can be estimated, then plays the kept bytes.
    fn estimate(&mut self, data: &[u8], now: Instant) {
        let from = match self.estimate_from {
            Some(t) if now - self.estimate_last <= ESTIMATE_GAP => t,
            _ => {
                self.kept.clear();
                self.estimate_from = Some(now);
                now
            }
        };
        self.estimate_last = now;
        let room = ESTIMATE_BUFFER - self.kept.len();
        self.kept.extend_from_slice(&data[..data.len().min(room)]);
        let elapsed = now - from;
        if elapsed < ESTIMATE {
            return;
        }
        // The first datagram arrived at `from`: its bytes belong to the time before it.
        let bps = self.kept.len() as f64 / elapsed.as_secs_f64();
        let (rate, channels) = format_from_bps(bps, self.rate);
        self.stats.rate.store(rate, Ordering::Relaxed);
        self.stats
            .channels
            .store(channels as u32, Ordering::Relaxed);
        self.channels = channels;
        self.resampler = Some(Resampler::new(rate as f64, self.out_rate));
        let kept = std::mem::take(&mut self.kept);
        self.play(&kept);
    }

    /// s16le frames -> mono f32 -> output rate -> voice buffer.
    fn play(&mut self, data: &[u8]) {
        let Some(rs) = self.resampler.as_mut() else {
            return;
        };
        let frame = 2 * self.channels;
        let (out, stats) = (&mut self.out, &self.stats);
        // Stereo: the sum of the slots (usually one of them is silent), limited to full scale.
        let mut push = |f: &[u8]| {
            let x = f
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b) as f32 / 32_768.0)
                .sum::<f32>()
                .clamp(-1.0, 1.0)
                * VOICE_GAIN;
            rs.push(x, |y| {
                if out.push(y).is_err() {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            });
        };
        let mut rest = data;
        if self.carry_len > 0 {
            let take = (frame - self.carry_len).min(rest.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&rest[..take]);
            self.carry_len += take;
            rest = &rest[take..];
            if self.carry_len < frame {
                return;
            }
            push(&self.carry[..frame]);
            self.carry_len = 0;
        }
        let mut frames = rest.chunks_exact(frame);
        for f in frames.by_ref() {
            push(f);
        }
        let tail = frames.remainder();
        self.carry[..tail.len()].copy_from_slice(tail);
        self.carry_len = tail.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone in the first channel (stereo: the other one silent), 20 ms datagrams in real time.
    fn send_tone(to: SocketAddr, rate: u32, channels: usize, secs: f64) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let per = (rate / 50) as usize;
        let mut i = 0usize;
        let t0 = Instant::now();
        let mut sent = 0.0;
        while sent < secs {
            let mut d = Vec::with_capacity(2 * channels * per);
            for _ in 0..per {
                let x = (8_000.0 * (i as f64 * 0.05).sin()) as i16;
                d.extend_from_slice(&x.to_le_bytes());
                if channels == 2 {
                    d.extend_from_slice(&0i16.to_le_bytes());
                }
                i += 1;
            }
            sock.send_to(&d, to).unwrap();
            sent += 0.02;
            let due = Duration::from_secs_f64(sent);
            if let Some(w) = due.checked_sub(t0.elapsed()) {
                std::thread::sleep(w);
            }
        }
    }

    /// The format is estimated from the stream and the voice reaches the output buffer at its
    /// real duration and level.
    #[test]
    fn estimates_format_and_plays() {
        for (rate, channels) in [(8_000u32, 1usize), (8_000, 2), (48_000, 1)] {
            let (tx, mut rx) = rtrb::RingBuffer::new(200_000);
            let v = start("127.0.0.1:0".parse().unwrap(), None, 48_000.0, tx).unwrap();
            send_tone(v.addr, rate, channels, 0.8);
            std::thread::sleep(Duration::from_millis(200));
            let st = &v.stats;
            assert_eq!(st.rate.load(Ordering::Relaxed), rate);
            assert_eq!(st.channels.load(Ordering::Relaxed), channels as u32);
            // About 0.8 s of output at 48 kHz, the kept bytes included.
            let got = rx.slots();
            assert!((30_000..45_000).contains(&got), "{rate}/{channels}: {got}");
            let peak = (0..got)
                .map(|_| rx.pop().unwrap_or(0.0).abs())
                .fold(0.0f32, f32::max);
            assert!((0.2..0.3).contains(&(peak / VOICE_GAIN)), "{peak}");
        }
    }

    /// Stereo frames split across datagrams of any length are mixed to mono in order.
    #[test]
    fn frames_across_datagrams() {
        let (tx, mut rx) = rtrb::RingBuffer::new(10_000);
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut r = Receiver::new(socket, Arc::new(VoiceStats::new()), tx, 48_000.0, None);
        r.resampler = Some(Resampler::new(48_000.0, 48_000.0));
        r.channels = 2;
        // Left i * 100, right i: the mono sample is i * 101.
        let bytes: Vec<u8> = (0..100i16)
            .flat_map(|i| [(i * 100).to_le_bytes(), i.to_le_bytes()])
            .flatten()
            .collect();
        for part in [&bytes[..1], &bytes[1..3], &bytes[3..51], &bytes[51..]] {
            r.play(part);
        }
        let got: Vec<f32> = (0..rx.slots()).filter_map(|_| rx.pop().ok()).collect();
        assert_eq!(got.len(), 100);
        for (i, y) in got.iter().enumerate() {
            assert!(
                (y - i as f32 * 101.0 / 32_768.0 * VOICE_GAIN).abs() < 1e-6,
                "{i}: {y}"
            );
        }
    }

    #[test]
    fn format_split() {
        assert_eq!(format_from_bps(16_000.0, None), (8_000, 1));
        assert_eq!(format_from_bps(32_000.0, None), (8_000, 2));
        assert_eq!(format_from_bps(96_000.0, None), (48_000, 1));
        assert_eq!(format_from_bps(16_000.0, Some(8_000)), (8_000, 1));
        assert_eq!(format_from_bps(32_000.0, Some(8_000)), (8_000, 2));
        assert_eq!(format_from_bps(32_000.0, Some(16_000)), (16_000, 1));
    }
}
