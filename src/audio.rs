//! Audio output through `cpal` (system default device) with a lock-free buffer.
//!
//! The sound card callback reads samples from a ring buffer without blocking. After the buffer
//! runs empty it first waits until about 50 ms accumulate (prefill), to avoid clicks at the start
//! of an activity. An underrun during activity (empty buffer while the DSP thread is playing) is
//! counted and replaced with silence. The decoded voice from DSD-FME comes in a second
//! buffer and is added to every channel; the sum goes through a soft limiter. The mono mix also
//! goes to the web GUI, and the frames with audio to the recordings. With `--local-audio off` a
//! thread with its own clock drains the buffers instead of the sound card.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// Audio buffer capacity in seconds.
const BUFFER_S: f64 = 0.5;
/// Prefill after the buffer runs empty.
const PREFILL_S: f64 = 0.05;

pub struct AudioOut {
    /// The sound card stream, or the mixer clock with `--local-audio off`.
    _stream: Option<cpal::Stream>,
    _clock: Option<SilentClock>,
    pub rate: f64,
    pub channels: usize,
    pub device: String,
    pub underruns: Arc<AtomicU64>,
    /// Set by the DSP thread during an activity (for counting underruns).
    pub active: Arc<AtomicBool>,
    /// Mute (`m` key in the TUI): the output plays silence, the buffer keeps draining.
    pub muted: Arc<AtomicBool>,
    /// A recording is playing (TUI): the output plays it instead of the scanner audio.
    pub playback_active: Arc<AtomicBool>,
}

/// State shared between the audio callback and the rest of the program.
#[derive(Clone)]
struct Shared {
    underruns: Arc<AtomicU64>,
    active: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
}

/// Decoded voice from DSD-FME, mono at the output rate: the reserve collected after
/// the buffer runs empty (network jitter) and the maximum depth (older samples are dropped).
const VOICE_PREFILL_S: f64 = 0.06;
const VOICE_MAX_S: f64 = 0.25;

/// Soft limiting of the sum of the analog audio and the decoded voice, like the limiter of the
/// audio chain (`audio_dsp`): above `LIMIT` asymptotically to 1.0.
fn soft_limit(x: f32) -> f32 {
    const LIMIT: f32 = 0.891;
    let a = x.abs();
    if a > LIMIT {
        x.signum() * (LIMIT + (1.0 - LIMIT) * ((a - LIMIT) / (1.0 - LIMIT)).tanh())
    } else {
        x
    }
}

/// Ring buffer parameters of the decoded voice (samples at the output rate).
struct VoiceBuffer {
    rx: rtrb::Consumer<f32>,
    prefill: usize,
    max: usize,
}

/// Mixes the analog audio and the decoded voice into the output frames. Used by the sound card
/// callback, or with `--local-audio off` by a thread with its own clock. The mono mix (before
/// the mute) also goes to the web GUI tap, if any.
struct Mixer {
    rx: rtrb::Consumer<f32>,
    voice: VoiceBuffer,
    shared: Shared,
    prefill: usize,
    channels: usize,
    primed: bool,
    vprimed: bool,
    /// Mono mix for the web GUI; samples are dropped when it is full.
    web: Option<rtrb::Producer<f32>>,
    /// Mono mix for the recordings (`--record-dir`): only frames with audio (analog or decoded
    /// voice), so the silence between them, e.g. in the Hang state, is not recorded.
    rec: Option<rtrb::Producer<f32>>,
    /// A recording played instead of the scanner audio while `active` is set (mono at the output
    /// rate); otherwise the buffer is drained. The taps keep getting the scanner audio.
    playback: Option<(rtrb::Consumer<f32>, Arc<AtomicBool>)>,
}

impl Mixer {
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let channels = self.channels;
        let frames = data.len() / channels;
        let available = self.rx.slots();
        if !self.primed && available >= self.prefill {
            self.primed = true;
        }
        // Whole frames only (all channels), so the channels do not shift.
        let n = if self.primed {
            available.min(data.len()) / channels * channels
        } else {
            0
        };
        // Decoded voice: bounded depth, a reserve after running empty.
        let vrx = &mut self.voice.rx;
        let mut vavail = vrx.slots();
        if vavail > self.voice.max {
            if let Ok(c) = vrx.read_chunk(vavail - self.voice.max) {
                c.commit_all();
            }
            vavail = self.voice.max;
        }
        if !self.vprimed && vavail >= self.voice.prefill {
            self.vprimed = true;
        }
        let vn = if self.vprimed { vavail.min(frames) } else { 0 };
        if self.vprimed && vn < frames {
            self.vprimed = false;
        }
        let gain = if self.shared.muted.load(Ordering::Relaxed) {
            0.0
        } else {
            1.0
        };
        let analog = self.rx.read_chunk(n);
        let decoded = vrx.read_chunk(vn);
        let mut playback = match &mut self.playback {
            Some((p, active)) if active.load(Ordering::Relaxed) => Some(p),
            Some((p, _)) => {
                if let Ok(c) = p.read_chunk(p.slots()) {
                    c.commit_all();
                }
                None
            }
            None => None,
        };
        {
            let empty: &[f32] = &[];
            let (a1, a2) = analog.as_ref().map_or((empty, empty), |c| c.as_slices());
            let (v1, v2) = decoded.as_ref().map_or((empty, empty), |c| c.as_slices());
            let mut a = a1.iter().chain(a2);
            let mut v = v1.iter().chain(v2);
            let analog_frames = n / channels;
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                let vs = v.next().copied().unwrap_or(0.0);
                let played = playback.as_mut().map(|p| p.pop().unwrap_or(0.0));
                let mut mono = None;
                for d in frame {
                    let s = soft_limit(a.next().copied().unwrap_or(0.0) + vs);
                    mono.get_or_insert(s);
                    *d = T::from_sample(played.unwrap_or(s) * gain);
                }
                if let Some(w) = &mut self.web {
                    let _ = w.push(mono.unwrap_or(0.0));
                }
                if let Some(r) = &mut self.rec
                    && (i < analog_frames || i < vn)
                {
                    let _ = r.push(mono.unwrap_or(0.0));
                }
            }
        }
        if let Ok(c) = analog {
            c.commit_all();
        }
        if let Ok(c) = decoded {
            c.commit_all();
        }
        if n < data.len() && self.primed {
            if self.shared.active.load(Ordering::Relaxed) {
                self.shared.underruns.fetch_add(1, Ordering::Relaxed);
            }
            self.primed = false;
        }
    }
}

fn run<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut mixer: Mixer,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream::<T, _, _>(
            config,
            move |data: &mut [T], _| mixer.fill(data),
            |e| eprintln!("WARNING: audio stream error: {e}"),
            None,
        )
        .map_err(|e| format!("cannot open the audio stream: {e}"))
}

/// Output rate without a sound card (`--local-audio off`): the audio chain rate.
const SILENT_RATE: f64 = 48_000.0;
/// Clock period of the mixer thread without a sound card.
const SILENT_TICK: Duration = Duration::from_millis(10);

/// The mixer driven by the monotonic clock instead of a sound card (`--local-audio off`): the
/// buffers drain at the output rate and the web GUI gets the mix; nothing is played.
struct SilentClock {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SilentClock {
    fn start(mut mixer: Mixer, rate: f64) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        // A chunk of 4 ticks: a late tick is made up in a few chunks.
        let mut scratch = vec![0.0f32; (4.0 * SILENT_TICK.as_secs_f64() * rate) as usize];
        let thread = std::thread::Builder::new()
            .name("rfscan-audio-clock".into())
            .spawn(move || {
                let start = Instant::now();
                let mut done: u64 = 0;
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(SILENT_TICK);
                    let due = (start.elapsed().as_secs_f64() * rate) as u64;
                    while done < due {
                        let n = ((due - done) as usize).min(scratch.len());
                        mixer.fill(&mut scratch[..n]);
                        done += n as u64;
                    }
                }
            })
            .map_err(|e| format!("cannot start the audio clock thread: {e}"))?;
        Ok(SilentClock {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for SilentClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Device names, unique in the list: a description shared by several devices (ALSA gives every
/// PCM of a card, `hw:`, `plughw:`, `dmix:`..., the same one) gets the driver's device name in
/// brackets.
fn device_names(devices: &[cpal::Device]) -> Vec<String> {
    let descs: Vec<Option<cpal::DeviceDescription>> =
        devices.iter().map(|d| d.description().ok()).collect();
    let base = |desc: &Option<cpal::DeviceDescription>| {
        desc.as_ref().map_or("?", |d| d.name()).to_string()
    };
    descs
        .iter()
        .map(|desc| {
            let name = base(desc);
            let shared = descs.iter().filter(|o| base(o) == name).count() > 1;
            match desc.as_ref().and_then(|d| d.driver()) {
                Some(driver) if shared && driver != name => format!("{name} [{driver}]"),
                _ => name,
            }
        })
        .collect()
}

fn output_devices(host: &cpal::Host) -> Result<Vec<cpal::Device>, String> {
    Ok(host
        .output_devices()
        .map_err(|e| format!("audio device list: {e}"))?
        .collect())
}

/// Audio output device names; `true` for the default one.
pub fn list_outputs() -> Result<Vec<(String, bool)>, String> {
    let host = cpal::default_host();
    let default = host.default_output_device().and_then(|d| d.id().ok());
    let devices = output_devices(&host)?;
    Ok(device_names(&devices)
        .into_iter()
        .zip(&devices)
        .map(|(name, d)| {
            let is_default = default.is_some() && d.id().ok() == default;
            (name, is_default)
        })
        .collect())
}

/// Device with the given name: first the exact name, then the only match of a fragment
/// (case-insensitive). Without a name: the system default output. Returns the device and its
/// name.
fn find_device(host: &cpal::Host, wanted: Option<&str>) -> Result<(cpal::Device, String), String> {
    let Some(wanted) = wanted else {
        let d = host
            .default_output_device()
            .ok_or("no default audio output device")?;
        let name = device_names(std::slice::from_ref(&d)).swap_remove(0);
        return Ok((d, name));
    };
    let devices = output_devices(host)?;
    let names = device_names(&devices);
    let list = || names.join(", ");
    if let Some(i) = names.iter().position(|n| n == wanted) {
        return Ok((devices[i].clone(), names[i].clone()));
    }
    let w = wanted.to_lowercase();
    let hits: Vec<usize> = (0..names.len())
        .filter(|&i| names[i].to_lowercase().contains(&w))
        .collect();
    match hits.as_slice() {
        [i] => Ok((devices[*i].clone(), names[*i].clone())),
        [] => Err(format!(
            "no audio output matches '{wanted}'; available: {}",
            list()
        )),
        _ => Err(format!(
            "'{wanted}' matches several audio outputs: {}",
            hits.iter()
                .map(|&i| names[i].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Web GUI and recording tap capacity in seconds (drained every few tens of ms).
const WEB_TAP_S: f64 = 0.5;
const REC_TAP_S: f64 = 1.0;
/// Playback buffer of a recording (filled by the player at the pace it is played).
const PLAYBACK_S: f64 = 0.3;

/// The audio output with its buffers.
pub struct Opened {
    pub out: AudioOut,
    /// Sample buffer producer (interleaved channels, f32 in ±1).
    pub audio: rtrb::Producer<f32>,
    /// Decoded voice producer (mono at the output rate, mixed in).
    pub voice: rtrb::Producer<f32>,
    /// The mono mix at the output rate for the web GUI (with `web`).
    pub web: Option<rtrb::Consumer<f32>>,
    /// The mono mix at the output rate for the recordings, only frames with audio (with `rec`).
    pub rec: Option<rtrb::Consumer<f32>>,
    /// Playback of recordings on the local output (with `rec` and a local output).
    pub playback: Option<rtrb::Producer<f32>>,
}

impl AudioOut {
    /// Opens the audio output (`wanted`: a name or a part of it; without a name the system
    /// default output). `local`: false plays nothing and drives the mixer by the clock
    /// (`--local-audio off`); `web`: also returns the mono mix for the web GUI; `rec`: also
    /// returns the mono mix for the recordings.
    pub fn open(wanted: Option<&str>, local: bool, web: bool, rec: bool) -> Result<Opened, String> {
        let output = if local {
            let host = cpal::default_host();
            let (device, name) = find_device(&host, wanted)?;
            let supported = device
                .default_output_config()
                .map_err(|e| format!("audio device {name}: {e}"))?;
            Some((device, name, supported))
        } else {
            None
        };
        let (name, rate, channels) = match &output {
            Some((_, n, s)) => (
                n.clone(),
                s.config().sample_rate as f64,
                s.config().channels as usize,
            ),
            None => ("none (--local-audio off)".to_string(), SILENT_RATE, 1),
        };
        let capacity = ((BUFFER_S * rate) as usize).max(1) * channels;
        let (tx, rx) = rtrb::RingBuffer::new(capacity);
        let (vtx, vrx) = rtrb::RingBuffer::new(((BUFFER_S * rate) as usize).max(1));
        let (web_tx, web_rx) = if web {
            let (t, r) = rtrb::RingBuffer::new(((WEB_TAP_S * rate) as usize).max(1));
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        let (rec_tx, rec_rx) = if rec {
            let (t, r) = rtrb::RingBuffer::new(((REC_TAP_S * rate) as usize).max(1));
            (Some(t), Some(r))
        } else {
            (None, None)
        };
        // Playback of recordings needs a local output.
        let playback_active = Arc::new(AtomicBool::new(false));
        let (play_tx, play_rx) = if rec && local {
            let (t, r) = rtrb::RingBuffer::new(((PLAYBACK_S * rate) as usize).max(1));
            (Some(t), Some((r, playback_active.clone())))
        } else {
            (None, None)
        };
        let underruns = Arc::new(AtomicU64::new(0));
        let active = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let mixer = Mixer {
            rx,
            voice: VoiceBuffer {
                rx: vrx,
                prefill: (VOICE_PREFILL_S * rate) as usize,
                max: (VOICE_MAX_S * rate) as usize,
            },
            shared: Shared {
                underruns: underruns.clone(),
                active: active.clone(),
                muted: muted.clone(),
            },
            prefill: (PREFILL_S * rate) as usize * channels,
            channels,
            primed: false,
            vprimed: false,
            web: web_tx,
            rec: rec_tx,
            playback: play_rx,
        };
        let (stream, clock) = match output {
            Some((device, _, supported)) => {
                let config = supported.config();
                let stream = match supported.sample_format() {
                    cpal::SampleFormat::F32 => run::<f32>(&device, config, mixer),
                    cpal::SampleFormat::I16 => run::<i16>(&device, config, mixer),
                    cpal::SampleFormat::U16 => run::<u16>(&device, config, mixer),
                    cpal::SampleFormat::I32 => run::<i32>(&device, config, mixer),
                    f => Err(format!("unsupported audio sample format {f:?}")),
                }?;
                stream
                    .play()
                    .map_err(|e| format!("cannot start audio: {e}"))?;
                (Some(stream), None)
            }
            None => (None, Some(SilentClock::start(mixer, rate)?)),
        };
        Ok(Opened {
            out: AudioOut {
                _stream: stream,
                _clock: clock,
                rate,
                channels,
                device: name,
                underruns,
                active,
                muted,
                playback_active,
            },
            audio: tx,
            voice: vtx,
            web: web_rx,
            rec: rec_rx,
            playback: play_tx,
        })
    }
}
