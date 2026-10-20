//! `digi` output for DSD-FME: a TCP server with a continuous PCM s16le, mono,
//! 48 kHz stream.
//!
//! The scanner is the server, DSD-FME the client (`dsd-fme -i tcp:127.0.0.1:7355`). A separate
//! thread with its own 20 ms clock (960 samples) takes discriminator samples from a ring buffer
//! and fills with zeros when there are none, so the stream flows without gaps independently of
//! the scanner and the DSP. The thread ticks also without a client (data is discarded and does
//! not accumulate).
//!
//! - One active client: a new connection replaces the previous one, the listening socket stays.
//! - Non-blocking writes with `TCP_NODELAY`. When the socket does not accept data, the chunk is
//!   dropped and counted (a real-time stream). An unfinished write is completed on the next
//!   tick, so the client always receives whole samples.
//! - The buffer has a bounded depth: the excess of the oldest samples is dropped, so the latency
//!   does not grow with SDR clock drift relative to the system clock.
//! - After a gap in the data, playback starts only after a short reserve is collected (DSP
//!   jitter), so no zeros are inserted between blocks in the middle of a transmission.
//!
//! `std::net::TcpListener::bind` does not set `SO_REUSEADDR` on Windows (another process could
//! take the same port), and does on Linux, where a quick restart then does not fail.

use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

/// Stream sample rate (what the DSD-FME TCP input expects).
pub const DIGI_RATE: f64 = 48_000.0;
/// Send period: 20 ms = 960 samples = 1920 B.
const TICK: Duration = Duration::from_millis(20);
const CHUNK: usize = 960;
const CHUNK_BYTES: usize = 2 * CHUNK;
/// Reserve before playback starts after a gap in the data (40 ms).
const PREBUFFER: usize = 2 * CHUNK;
/// Maximum buffer depth (150 ms): above it the oldest samples are dropped.
const MAX_DEPTH: usize = 7_200;
/// Ring buffer capacity between the DSP thread and the sender (0.5 s).
const RING: usize = 24_000;

/// Connection event (for the scanner log).
#[derive(Debug)]
pub enum DigiEvent {
    Connected(SocketAddr),
    Disconnected {
        peer: SocketAddr,
        reason: String,
    },
    /// A message about the DSD-FME process started by rfscan (`--digi`).
    Dsd(String),
}

/// Digi output statistics, read by the scanner.
#[derive(Default)]
pub struct DigiStats {
    pub connected: AtomicBool,
    pub bytes_sent: AtomicU64,
    /// Chunks dropped because the client was not receiving data.
    pub dropped_chunks: AtomicU64,
    /// Samples dropped on buffer overflow (clock drift).
    pub trimmed_samples: AtomicU64,
    pub connections: AtomicU64,
}

pub struct DigiServer {
    pub addr: SocketAddr,
    pub stats: Arc<DigiStats>,
    pub events: Receiver<DigiEvent>,
    /// For other digi event sources (the DSD-FME supervisor).
    pub event_sender: Sender<DigiEvent>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for DigiServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Starts listening and the sender thread. Returns the server and the ring buffer input for the
/// DSP thread (s16 samples at 48 kHz). A listen error (e.g. a busy port) is returned at once.
pub fn start(addr: SocketAddr) -> Result<(DigiServer, rtrb::Producer<i16>), String> {
    let listener =
        TcpListener::bind(addr).map_err(|e| format!("--digi-tcp: cannot listen on {addr}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("--digi-tcp: {e}"))?;
    let addr = listener.local_addr().unwrap_or(addr);
    let (producer, consumer) = rtrb::RingBuffer::new(RING);
    let stats = Arc::new(DigiStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, events) = crossbeam_channel::bounded(16);
    let handle = {
        let (stats, stop) = (stats.clone(), stop.clone());
        std::thread::Builder::new()
            .name("rfscan-digi".into())
            .spawn({
                let tx = tx.clone();
                move || Pump::new(listener, consumer, stats, tx).run(&stop)
            })
            .map_err(|e| format!("cannot start the digi thread: {e}"))?
    };
    Ok((
        DigiServer {
            addr,
            stats,
            events,
            event_sender: tx,
            stop,
            handle: Some(handle),
        },
        producer,
    ))
}

/// Sender thread state.
struct Pump {
    listener: TcpListener,
    input: rtrb::Consumer<i16>,
    stats: Arc<DigiStats>,
    events: Sender<DigiEvent>,
    client: Option<(TcpStream, SocketAddr)>,
    playing: bool,
    chunk: [u8; CHUNK_BYTES],
    /// Unfinished write: `pending[pending_from..pending_len]`.
    pending: [u8; CHUNK_BYTES],
    pending_from: usize,
    pending_len: usize,
}

impl Pump {
    fn new(
        listener: TcpListener,
        input: rtrb::Consumer<i16>,
        stats: Arc<DigiStats>,
        events: Sender<DigiEvent>,
    ) -> Self {
        Pump {
            listener,
            input,
            stats,
            events,
            client: None,
            playing: false,
            chunk: [0; CHUNK_BYTES],
            pending: [0; CHUNK_BYTES],
            pending_from: 0,
            pending_len: 0,
        }
    }

    fn run(mut self, stop: &AtomicBool) {
        let mut next = Instant::now() + TICK;
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > 5 * TICK {
                // The thread was suspended: no catching up with a burst of chunks.
                next = now;
            }
            next += TICK;
            self.accept();
            self.fill_chunk();
            self.send();
        }
    }

    /// Accepts new connections; a new one replaces the previous one.
    fn accept(&mut self) {
        while let Ok((stream, peer)) = self.listener.accept() {
            let _ = stream.set_nonblocking(true);
            let _ = stream.set_nodelay(true);
            self.disconnect("replaced by a new connection");
            self.client = Some((stream, peer));
            self.stats.connections.fetch_add(1, Ordering::Relaxed);
            self.stats.connected.store(true, Ordering::Relaxed);
            let _ = self.events.try_send(DigiEvent::Connected(peer));
        }
    }

    fn disconnect(&mut self, reason: &str) {
        if let Some((_, peer)) = self.client.take() {
            self.pending_len = 0;
            self.pending_from = 0;
            self.stats.connected.store(false, Ordering::Relaxed);
            let _ = self.events.try_send(DigiEvent::Disconnected {
                peer,
                reason: reason.to_string(),
            });
        }
    }

    /// The next 20 ms chunk: samples from the buffer, or zeros when there are none.
    fn fill_chunk(&mut self) {
        let mut available = self.input.slots();
        if available > MAX_DEPTH {
            let excess = available - MAX_DEPTH;
            if let Ok(c) = self.input.read_chunk(excess) {
                c.commit_all();
            }
            self.stats
                .trimmed_samples
                .fetch_add(excess as u64, Ordering::Relaxed);
            available = MAX_DEPTH;
        }
        if !self.playing && available >= PREBUFFER {
            self.playing = true;
        }
        let n = if self.playing {
            available.min(CHUNK)
        } else {
            0
        };
        if self.playing && available <= CHUNK {
            // The buffer is running empty: after this chunk wait for a new reserve.
            self.playing = false;
        }
        self.chunk.fill(0);
        if let Ok(c) = self.input.read_chunk(n) {
            let (a, b) = c.as_slices();
            for (i, &s) in a.iter().chain(b).enumerate() {
                self.chunk[2 * i..2 * i + 2].copy_from_slice(&s.to_le_bytes());
            }
            c.commit_all();
        }
    }

    /// Sends the chunk to the client (if any), without blocking.
    fn send(&mut self) {
        let Some((stream, _)) = self.client.as_mut() else {
            return;
        };
        // First the rest of the previous chunk: keeps sample boundaries.
        if self.pending_from < self.pending_len {
            match stream.write(&self.pending[self.pending_from..self.pending_len]) {
                Ok(0) => return self.disconnect("the client closed the connection"),
                Ok(n) => {
                    self.pending_from += n;
                    self.stats.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return self.disconnect(&e.to_string()),
            }
            if self.pending_from < self.pending_len {
                self.stats.dropped_chunks.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        match stream.write(&self.chunk) {
            Ok(0) => self.disconnect("the client closed the connection"),
            Ok(n) => {
                self.stats.bytes_sent.fetch_add(n as u64, Ordering::Relaxed);
                if n < CHUNK_BYTES {
                    self.pending = self.chunk;
                    self.pending_from = n;
                    self.pending_len = CHUNK_BYTES;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                self.stats.dropped_chunks.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => self.disconnect(&e.to_string()),
        }
    }
}

/// Discriminator sample -> s16 with gain and clipping to full scale.
pub fn to_s16(v: f32, gain: f32) -> i16 {
    ((v * gain).clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The stream flows at a constant rate also without data (silence), and the data reaches
    /// the client.
    #[test]
    fn continuous_stream_with_silence_and_data() {
        let (server, mut input) = start("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut client = TcpStream::connect(server.addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // Data: 0.1 s of a constant value (less than the 150 ms buffer depth limit).
        for _ in 0..4_800 {
            input.push(1000).unwrap();
        }
        let t0 = Instant::now();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while t0.elapsed() < Duration::from_millis(500) {
            let n = client.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        let secs = t0.elapsed().as_secs_f64();
        // About 96,000 B/s (with tolerance for startup and the system clock).
        let rate = got.len() as f64 / secs;
        assert!((70_000.0..120_000.0).contains(&rate), "{rate} B/s");
        assert_eq!(got.len() % 2, 0);
        let samples: Vec<i16> = got
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        let data = samples.iter().filter(|&&s| s == 1000).count();
        assert_eq!(data, 4_800);
        assert!(samples.iter().all(|&s| s == 0 || s == 1000));
        assert!(server.stats.connected.load(Ordering::Relaxed));
        assert!(matches!(
            server.events.try_recv(),
            Ok(DigiEvent::Connected(_))
        ));
    }

    /// The excess above 150 ms (clock drift) is dropped starting from the oldest samples.
    #[test]
    fn depth_is_limited() {
        let (server, mut input) = start("127.0.0.1:0".parse().unwrap()).unwrap();
        for _ in 0..9_600 {
            input.push(1).unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(server.stats.trimmed_samples.load(Ordering::Relaxed), 2_400);
    }

    #[test]
    fn busy_port_is_an_error() {
        let (server, _input) = start("127.0.0.1:0".parse().unwrap()).unwrap();
        let err = start(server.addr).err().unwrap();
        assert!(err.contains("cannot listen"), "{err}");
    }

    #[test]
    fn reconnect_replaces_client() {
        let (server, _input) = start("127.0.0.1:0".parse().unwrap()).unwrap();
        let first = TcpStream::connect(server.addr).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(first);
        let mut second = TcpStream::connect(server.addr).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 1920];
        assert!(second.read(&mut buf).unwrap() > 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(server.stats.connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn s16_scaling_clips() {
        assert_eq!(to_s16(0.5, 1.0), 16383);
        assert_eq!(to_s16(2.0, 1.0), i16::MAX);
        assert_eq!(to_s16(-0.5, 4.0), -i16::MAX);
    }
}
