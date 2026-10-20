//! Demodulation in the Locked state: a DSP thread that receives the window IQ samples
//! from the scanner, extracts the channel and feeds the audio output.
//!
//! Chain: DC removal (long-term, not reset on retune) -> NCO (channel to baseband, without
//! retuning the tuner: the channel lies in the window away from DC) -> multi-stage decimation to
//! about 48 kHz -> channel filter `bw` -> FM/AM demodulator -> resampling to 48 kHz -> audio
//! chain (`audio_dsp`) -> fade-out delay line -> resampling to the sound card rate -> audio
//! buffer.
//!
//! `digi` mode: the same channelizer, then the raw FM discriminator (no
//! de-emphasis) -> resampling to 48 kHz -> s16 to the TCP sender thread (`digi_tcp`). No
//! `audio_dsp` (band-pass filter, AGC, limiter) and no fade; local audio is silent.
//!
//! The scanner and the DSP thread exchange samples through a lock-free ring buffer, and the
//! Start/Stop messages carry a position in the sample stream, so the DSP thread knows exactly
//! at which sample an activity starts and ends. The scanner never waits for the DSP thread.

pub mod am;
pub mod fm;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use num_complex::Complex;

use crate::audio_dsp::{AUDIO_RATE, AudioDsp, AudioDspConfig, FADE_S};
use crate::digi_tcp::{DIGI_RATE, to_s16};
use crate::plan::Modulation;
use am::AmDemod;
use fm::FmDemod;

/// Target rate after decimation: the nearest integer divisor not lower than 48 kHz.
const MIN_CHANNEL_RATE: f64 = AUDIO_RATE;
/// Long-term DC removal: `dc += α·(x - dc)`.
const DC_ALPHA: f32 = 1e-4;
/// Channel filter transition width as a fraction of `bw`.
const CHANNEL_TRANSITION: f64 = 0.25;
/// Blackman window: transition width ≈ 5.5 / N (normalized to the input rate).
const BLACKMAN_WIDTH: f64 = 5.5;
const MAX_TAPS: usize = 2047;
/// Report to the scanner every this many seconds of audio.
const REPORT_EVERY_S: f64 = 0.5;

/// FIR low-pass filter (Blackman window), `cutoff` as a fraction of the input rate.
fn lowpass(cutoff: f64, transition: f64) -> Vec<f32> {
    let n = ((BLACKMAN_WIDTH / transition).ceil() as usize).clamp(3, MAX_TAPS) | 1;
    let m = (n - 1) as f64;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 - m / 2.0;
            let sinc = if t == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
            };
            let x = 2.0 * std::f64::consts::PI * i as f64 / m;
            let w = 0.42 - 0.5 * x.cos() + 0.08 * (2.0 * x).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter().map(|&t| (t / sum) as f32).collect()
}

/// FIR decimator (factor 1 = filter only). History in a double buffer, so the sample window is
/// always a contiguous slice. No allocation in `push`.
struct Decimator {
    taps: Vec<f32>,
    hist: Vec<Complex<f32>>,
    pos: usize,
    factor: usize,
    phase: usize,
}

impl Decimator {
    fn new(taps: Vec<f32>, factor: usize) -> Self {
        let n = taps.len();
        Decimator {
            taps,
            hist: vec![Complex::new(0.0, 0.0); 2 * n],
            pos: 0,
            factor,
            phase: 0,
        }
    }

    fn reset(&mut self) {
        self.hist.fill(Complex::new(0.0, 0.0));
        self.pos = 0;
        self.phase = 0;
    }

    fn push(&mut self, x: Complex<f32>) -> Option<Complex<f32>> {
        let n = self.taps.len();
        self.hist[self.pos] = x;
        self.hist[self.pos + n] = x;
        self.pos = (self.pos + 1) % n;
        self.phase += 1;
        if self.phase < self.factor {
            return None;
        }
        self.phase = 0;
        // Window: the oldest sample at position `pos`, the newest at `pos + n - 1`.
        let window = &self.hist[self.pos..self.pos + n];
        let mut acc = Complex::new(0.0f32, 0.0);
        for (s, &t) in window.iter().zip(self.taps.iter().rev()) {
            acc += s * t;
        }
        Some(acc)
    }
}

/// Prime factorization (ascending): successive decimation stages.
fn factors(mut d: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut p = 2;
    while d > 1 {
        while d.is_multiple_of(p) {
            out.push(p);
            d /= p;
        }
        p += 1;
    }
    out
}

/// Channel extraction: NCO, multi-stage decimation and channel filter.
struct Channelizer {
    dc: Complex<f32>,
    nco: Complex<f64>,
    nco_step: Complex<f64>,
    nco_count: u32,
    stages: Vec<Decimator>,
    channel: Decimator,
    rate: f64,
    /// Warm-up after start (in output samples): the time to fill the history of all filters.
    /// Until then the output ramps up from zero and does not reach the demodulator (AM impulse,
    /// click).
    warmup: usize,
    skip: usize,
}

impl Channelizer {
    fn new(fs: f64, bw: f64) -> Result<Self, String> {
        let total = (fs / MIN_CHANNEL_RATE).floor() as usize;
        if total < 1 {
            return Err(format!(
                "sample rate {fs} Hz is lower than {MIN_CHANNEL_RATE} Hz"
            ));
        }
        let rate = fs / total as f64;
        let pass = bw / 2.0;
        if rate - 2.0 * pass <= 0.0 {
            return Err(format!(
                "bw {} kHz too wide for the demodulation chain ({rate:.0} Hz)",
                bw / 1e3
            ));
        }
        let mut stages = Vec::new();
        let mut fs_in = fs;
        let mut fill_s = 0.0;
        for m in factors(total) {
            let fs_out = fs_in / m as f64;
            // Protects the channel band from aliases: stopband from fs_out - bw/2.
            let transition = (fs_out - 2.0 * pass) / fs_in;
            let cutoff = fs_out / 2.0 / fs_in;
            let taps = lowpass(cutoff, transition);
            fill_s += taps.len() as f64 / fs_in;
            stages.push(Decimator::new(taps, m));
            fs_in = fs_out;
        }
        let channel_taps = lowpass(pass / rate, CHANNEL_TRANSITION * bw / rate);
        fill_s += channel_taps.len() as f64 / rate;
        let channel = Decimator::new(channel_taps, 1);
        let warmup = (fill_s * rate).ceil() as usize;
        Ok(Channelizer {
            dc: Complex::new(0.0, 0.0),
            nco: Complex::new(1.0, 0.0),
            nco_step: Complex::new(1.0, 0.0),
            nco_count: 0,
            stages,
            channel,
            rate,
            warmup,
            skip: warmup,
        })
    }

    /// New activity: channel offset relative to the LO. The DC state is kept.
    fn start(&mut self, offset_hz: f64, fs: f64) {
        self.nco = Complex::new(1.0, 0.0);
        self.nco_step = Complex::from_polar(1.0, -2.0 * std::f64::consts::PI * offset_hz / fs);
        for s in &mut self.stages {
            s.reset();
        }
        self.channel.reset();
        self.skip = self.warmup;
    }

    fn push(&mut self, x: Complex<f32>) -> Option<Complex<f32>> {
        self.dc += (x - self.dc) * DC_ALPHA;
        let x = x - self.dc;
        let mixed = x * Complex::new(self.nco.re as f32, self.nco.im as f32);
        self.nco *= self.nco_step;
        self.nco_count += 1;
        if self.nco_count >= 4096 {
            // Phasor renormalization, so rounding errors do not change the amplitude.
            self.nco /= self.nco.norm();
            self.nco_count = 0;
        }
        let mut v = mixed;
        for s in &mut self.stages {
            v = s.push(v)?;
        }
        let y = self.channel.push(v)?;
        if self.skip > 0 {
            self.skip -= 1;
            return None;
        }
        Some(y)
    }
}

/// Fractional resampler (3rd-order Hermite interpolation), fully sufficient for speech.
pub struct Resampler {
    step: f64,
    pos: f64,
    hist: [f32; 4],
}

impl Resampler {
    pub fn new(fs_in: f64, fs_out: f64) -> Self {
        Resampler {
            step: fs_in / fs_out,
            pos: 0.0,
            hist: [0.0; 4],
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9
    }

    /// Adds an input sample and calls `out` for each output sample.
    pub fn push(&mut self, x: f32, mut out: impl FnMut(f32)) {
        if self.is_identity() {
            out(x);
            return;
        }
        self.hist = [self.hist[1], self.hist[2], self.hist[3], x];
        while self.pos < 1.0 {
            let t = self.pos as f32;
            let [y0, y1, y2, y3] = self.hist;
            let c0 = y1;
            let c1 = 0.5 * (y2 - y0);
            let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
            let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
            out(((c3 * t + c2) * t + c1) * t + c0);
            self.pos += self.step;
        }
        self.pos -= 1.0;
    }
}

/// Message from the scanner to the DSP thread. `at` is the position in the stream of samples
/// passed through the ring buffer (the number of all samples pushed so far).
#[derive(Clone, Copy, Debug)]
pub enum Ctrl {
    Start {
        at: u64,
        freq: u64,
        offset_hz: f64,
        mode: Modulation,
        bw: f64,
    },
    /// End of an activity. Samples from `tail_from` to `at` are the tail after the signal
    /// disappeared (the time to confirm the drop below the threshold): their audio is cut out
    /// before the fade-out.
    Stop { at: u64, tail_from: u64 },
}

impl Ctrl {
    fn at(&self) -> u64 {
        match *self {
            Ctrl::Start { at, .. } | Ctrl::Stop { at, .. } => at,
        }
    }
}

/// DSP thread report: demodulated audio time since the previous report and the chain state.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub freq: u64,
    pub audio_seconds: f64,
    pub agc_gain_db: f32,
    pub limiter_hits: u64,
    pub frozen_seconds: f64,
    pub audio_dropped: u64,
    /// Digi samples dropped because the send buffer was full.
    pub digi_dropped: u64,
}

pub struct WorkerConfig {
    pub sample_rate: f64,
    /// Plan channel widths: channel chains are built once, at startup.
    pub bandwidths: Vec<f64>,
    pub deemph_us: Option<f64>,
    pub audio: AudioDspConfig,
    pub output_rate: f64,
    /// Audio output channels (each sample is copied to all of them).
    pub output_channels: usize,
    /// The longest tail to cut on Stop (s): extends the audio delay line.
    pub tail_max_s: f64,
    /// Digi stream gain (linear, `--digi-gain`).
    pub digi_gain: f32,
    /// Speech noise reduction on (`--nr`, switched by the TUI).
    pub nr: Arc<AtomicBool>,
    /// Noise reduction strength in percent (`--nr-strength`, changed by the TUI).
    pub nr_strength: Arc<AtomicU32>,
}

/// Starts the DSP thread. Returns the thread handle; the thread ends when `ctrl` is closed.
pub fn spawn_worker(
    cfg: WorkerConfig,
    ctrl: Receiver<Ctrl>,
    mut iq: rtrb::Consumer<Complex<f32>>,
    mut audio: rtrb::Producer<f32>,
    audio_active: Arc<AtomicBool>,
    reports: Sender<Report>,
    mut digi: Option<rtrb::Producer<i16>>,
) -> Result<JoinHandle<()>, String> {
    let mut chains = Vec::new();
    for &bw in &cfg.bandwidths {
        if !chains.iter().any(|(b, _): &(f64, Channelizer)| *b == bw) {
            chains.push((bw, Channelizer::new(cfg.sample_rate, bw)?));
        }
    }
    let fade_len = (FADE_S as f64 * AUDIO_RATE) as usize;
    // The audio buffer should not grow beyond about 0.25 s (SDR and sound card clock drift).
    let max_fill = (0.25 * cfg.output_rate) as usize * cfg.output_channels;

    let handle = std::thread::Builder::new()
        .name("rfscan-dsp".into())
        .spawn(move || {
            let mut dsp = AudioDsp::new(&cfg.audio);
            let mut out_rs = Resampler::new(AUDIO_RATE, cfg.output_rate);
            let mut fm = FmDemod::new(
                chains.first().map_or(AUDIO_RATE, |c| c.1.rate),
                cfg.deemph_us,
            );
            let mut am = AmDemod::new(chains.first().map_or(AUDIO_RATE, |c| c.1.rate));
            // Raw discriminator for digi: no de-emphasis.
            let mut raw = FmDemod::new(chains.first().map_or(AUDIO_RATE, |c| c.1.rate), None);
            let mut digi_dropped: u64 = 0;
            let mut to_48k = Resampler::new(AUDIO_RATE, AUDIO_RATE);
            // Delay line (48 kHz samples): fade-out and the tail to cut (extra audio delay).
            let tail_len = (cfg.tail_max_s * AUDIO_RATE).ceil() as usize;
            let mut delay = vec![0.0f32; (fade_len + tail_len).max(1)];
            let mut delay_fill = 0usize;
            let mut delay_pos = 0usize;

            let mut consumed: u64 = 0;
            let mut pending: Option<Ctrl> = None;
            // Active channel: chain index, frequency, modulation.
            let mut active: Option<(usize, u64, Modulation)> = None;
            let mut audio_samples: u64 = 0;
            let mut reported_samples: u64 = 0;
            let mut dropped: u64 = 0;
            let report_every = (REPORT_EVERY_S * AUDIO_RATE) as u64;
            let mut last_frozen: u64 = 0;

            // Sends a 48 kHz sample to the sound card output (through the resampler), without
            // blocking.
            let emit = |y: f32,
                        audio: &mut rtrb::Producer<f32>,
                        out_rs: &mut Resampler,
                        dropped: &mut u64| {
                out_rs.push(y, |s| {
                    let used = audio.buffer().capacity() - audio.slots();
                    if used >= max_fill || audio.slots() < cfg.output_channels {
                        *dropped += 1;
                        return;
                    }
                    for _ in 0..cfg.output_channels {
                        let _ = audio.push(s);
                    }
                });
            };

            loop {
                if pending.is_none() {
                    match ctrl.recv_timeout(Duration::from_millis(1)) {
                        Ok(m) => pending = Some(m),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                let limit = pending.map_or(u64::MAX, |m| m.at());
                let available = iq.slots() as u64;
                let take = available.min(limit.saturating_sub(consumed)) as usize;
                if take > 0 {
                    if let Ok(chunk) = iq.read_chunk(take) {
                        let (a, b) = chunk.as_slices();
                        if let Some((ci, freq, mode)) = active {
                            dsp.nr_on = cfg.nr.load(Ordering::Relaxed);
                            dsp.nr_mix =
                                crate::audio_dsp::nr_mix(cfg.nr_strength.load(Ordering::Relaxed));
                            let ch = &mut chains[ci].1;
                            for &x in a.iter().chain(b) {
                                let Some(bb) = ch.push(x) else { continue };
                                let v = match mode {
                                    Modulation::Nfm => fm.process(bb),
                                    Modulation::Am => am.process(bb),
                                    Modulation::Digi => {
                                        // Straight to the TCP sender, bypassing the audio chain.
                                        let v = raw.process(bb);
                                        to_48k.push(v, |s| {
                                            audio_samples += 1;
                                            let ok = digi.as_mut().is_some_and(|d| {
                                                d.push(to_s16(s, cfg.digi_gain)).is_ok()
                                            });
                                            if !ok {
                                                digi_dropped += 1;
                                            }
                                        });
                                        continue;
                                    }
                                };
                                to_48k.push(v, |s| {
                                    // The analysis enters the line, the gain is applied at the
                                    // output.
                                    let y = dsp.analyze(s);
                                    audio_samples += 1;
                                    // Delay line: the oldest sample leaves.
                                    let old = delay[delay_pos];
                                    delay[delay_pos] = y;
                                    delay_pos = (delay_pos + 1) % delay.len();
                                    if delay_fill < delay.len() {
                                        delay_fill += 1;
                                    } else {
                                        emit(
                                            dsp.finish(old),
                                            &mut audio,
                                            &mut out_rs,
                                            &mut dropped,
                                        );
                                    }
                                });
                            }
                            if audio_samples - reported_samples >= report_every {
                                let _ = reports.try_send(Report {
                                    freq,
                                    audio_seconds: (audio_samples - reported_samples) as f64
                                        / AUDIO_RATE,
                                    agc_gain_db: dsp.gain_db(),
                                    limiter_hits: dsp.limiter_hits,
                                    frozen_seconds: (dsp.frozen_samples - last_frozen) as f64
                                        / AUDIO_RATE,
                                    audio_dropped: dropped,
                                    digi_dropped,
                                });
                                last_frozen = dsp.frozen_samples;
                                reported_samples = audio_samples;
                            }
                        }
                        chunk.commit_all();
                        consumed += take as u64;
                    }
                    continue;
                }

                let Some(m) = pending else { continue };
                if consumed < m.at() {
                    // The samples have not reached the ring buffer yet.
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                pending = None;
                match m {
                    Ctrl::Start {
                        freq,
                        offset_hz,
                        mode,
                        bw,
                        ..
                    } => {
                        let Some(ci) = chains.iter().position(|(b, _)| *b == bw) else {
                            continue;
                        };
                        let rate = chains[ci].1.rate;
                        chains[ci].1.start(offset_hz, cfg.sample_rate);
                        fm = FmDemod::new(rate, cfg.deemph_us);
                        am = AmDemod::new(rate);
                        raw = FmDemod::new(rate, None);
                        fm.reset();
                        am.reset();
                        to_48k = Resampler::new(rate, AUDIO_RATE);
                        delay_fill = 0;
                        delay_pos = 0;
                        active = Some((ci, freq, mode));
                        match mode {
                            Modulation::Nfm => dsp.start(cfg.audio.agc_target_db),
                            Modulation::Am => dsp.start(cfg.audio.agc_target_am_db),
                            // Local audio is silent, the stream goes to DSD-FME.
                            Modulation::Digi => to_48k = Resampler::new(rate, DIGI_RATE),
                        }
                        audio_active.store(mode != Modulation::Digi, Ordering::Relaxed);
                    }
                    Ctrl::Stop { at, tail_from } => {
                        // The tail (noise after the signal disappeared) is the newest samples
                        // of the line: cut it out.
                        let tail = ((at - tail_from) as f64 / cfg.sample_rate * AUDIO_RATE).round()
                            as usize;
                        let n = delay_fill;
                        let cut = tail.min(n);
                        let keep = n - cut;
                        // Fade-out on the last samples before the tail.
                        let f = fade_len.min(keep);
                        let start = (delay_pos + delay.len() - n) % delay.len();
                        for i in 0..keep {
                            let g = if i + f >= keep {
                                AudioDsp::fade_out_gain(i + f - keep, f)
                            } else {
                                1.0
                            };
                            emit(
                                dsp.finish(delay[(start + i) % delay.len()]) * g,
                                &mut audio,
                                &mut out_rs,
                                &mut dropped,
                            );
                        }
                        delay_fill = 0;
                        // The cut tail is not activity time. In digi the tail flows to DSD-FME
                        // unchanged (the signal is not modified), but it is not counted either.
                        let not_counted = match active {
                            Some((_, _, Modulation::Digi)) => tail,
                            _ => cut,
                        };
                        audio_samples = audio_samples
                            .saturating_sub(not_counted as u64)
                            .max(reported_samples);
                        if let Some((_, freq, _)) = active.take() {
                            let _ = reports.try_send(Report {
                                freq,
                                audio_seconds: (audio_samples - reported_samples) as f64
                                    / AUDIO_RATE,
                                agc_gain_db: dsp.gain_db(),
                                limiter_hits: dsp.limiter_hits,
                                frozen_seconds: (dsp.frozen_samples - last_frozen) as f64
                                    / AUDIO_RATE,
                                audio_dropped: dropped,
                                digi_dropped,
                            });
                            last_frozen = dsp.frozen_samples;
                            reported_samples = audio_samples;
                        }
                        audio_active.store(false, Ordering::Relaxed);
                    }
                }
            }
        })
        .map_err(|e| format!("cannot start the DSP thread: {e}"))?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factorization() {
        assert_eq!(factors(125), vec![5, 5, 5]);
        assert_eq!(factors(62), vec![2, 31]);
        assert_eq!(factors(50), vec![2, 5, 5]);
        assert_eq!(factors(1), Vec::<usize>::new());
    }

    /// A tone in a channel offset by 1.2 MHz from the LO passes, a tone 25 kHz away is
    /// attenuated.
    #[test]
    fn channelizer_selects_channel() {
        let fs = 6_000_000.0;
        let offset = 1_200_000.0;
        let power = |f: f64| {
            let mut ch = Channelizer::new(fs, 12_500.0).unwrap();
            ch.start(offset, fs);
            let mut acc = 0.0f64;
            let mut n = 0;
            for i in 0..600_000 {
                let ph = 2.0 * std::f64::consts::PI * f * i as f64 / fs;
                if let Some(y) = ch.push(Complex::from_polar(1.0, ph as f32)) {
                    n += 1;
                    if n > 1000 {
                        acc += y.norm_sqr() as f64;
                    }
                }
            }
            10.0 * (acc / (n - 1000) as f64).log10()
        };
        let inside = power(offset + 1_000.0);
        let adjacent = power(offset + 25_000.0);
        let far = power(offset + 1_000_000.0);
        assert!(inside.abs() < 0.5, "in channel {inside} dB");
        assert!(adjacent < -50.0, "adjacent channel {adjacent} dB");
        assert!(far < -50.0, "far away {far} dB");
    }

    #[test]
    fn channel_rate_near_48k() {
        for (fs, expect) in [(6e6, 48_000.0), (3e6, 3e6 / 62.0), (2.4e6, 48_000.0)] {
            let ch = Channelizer::new(fs, 12_500.0).unwrap();
            assert!((ch.rate - expect).abs() < 1e-6, "{fs}: {}", ch.rate);
        }
    }

    #[test]
    fn resampler_keeps_tone() {
        let mut r = Resampler::new(48_387.0, 48_000.0);
        let mut out = Vec::new();
        for i in 0..48_387 {
            let x = (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_387.0).sin() as f32;
            r.push(x, |y| out.push(y));
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "{}", out.len());
        let peak = out[1000..].iter().fold(0.0f32, |m, &y| m.max(y.abs()));
        assert!((peak - 1.0).abs() < 0.01, "{peak}");
    }
}
