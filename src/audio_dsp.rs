//! Audio chain after demodulation, mandatory order:
//! band-pass filter -> AGC -> noise reduction (optional) -> limiter -> fade. FM de-emphasis is in `demod::fm`, before this chain.
//!
//! The band-pass filter is a cascade of a high-pass and a low-pass Chebyshev II filter (flat
//! passband, equiripple stopband), designed at startup with the bilinear transform and
//! implemented as biquad sections (SOS) in f64. Default 300-3000 Hz at 48 kHz: HP with the
//! stopband from 250 Hz (40 dB), LP with the stopband from 4000 Hz (25 dB), the lowest orders
//! meeting these conditions with <= 3 dB attenuation at the band edges.
//!
//! Computed response of the default filter (the `default_band_meets_spec` test prints it and
//! checks the requirements):
//!
//! | frequency | attenuation |
//! |---|---|
//! | 100 Hz | 45.5 dB |
//! | 150 Hz | 46.5 dB |
//! | 250 Hz | 40.0 dB |
//! | 300 Hz | 1.9 dB |
//! | 1000 Hz | 0.0 dB |
//! | 3000 Hz | 1.4 dB |
//! | 4000 Hz | 25.0 dB |
//!
//! Requirements: >= 40 dB at 150 Hz, >= 20 dB at 250 Hz, >= 20 dB at 4000 Hz,
//! <= 3 dB at 300 and 3000 Hz.

use num_complex::Complex;

/// Sample rate of the audio chain (a constant of the method).
pub const AUDIO_RATE: f64 = 48_000.0;

/// The HP stopband starts at `low · 5/6` (250 Hz for 300 Hz), attenuation 40 dB.
const HP_STOP_RATIO: f64 = 5.0 / 6.0;
const HP_STOP_DB: f64 = 40.0;
/// The LP stopband starts at `high · 4/3` (4000 Hz for 3000 Hz), attenuation 25 dB.
const LP_STOP_RATIO: f64 = 4.0 / 3.0;
const LP_STOP_DB: f64 = 25.0;
/// Allowed attenuation at the passband edge.
const PASS_DB: f64 = 3.0;

/// Biquad section: b0 + b1 z^-1 + b2 z^-2 / (1 + a1 z^-1 + a2 z^-2), transposed direct form II.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    s: [f64; 2],
}

impl Biquad {
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.s[0];
        self.s[0] = self.b[1] * x - self.a[0] * y + self.s[1];
        self.s[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    fn response(&self, z: Complex<f64>) -> Complex<f64> {
        let zi = z.inv();
        let num = self.b[0] + self.b[1] * zi + self.b[2] * zi * zi;
        let den = 1.0 + self.a[0] * zi + self.a[1] * zi * zi;
        num / den
    }
}

/// Cascade of biquad sections.
#[derive(Clone, Debug, Default)]
pub struct Sos {
    sections: Vec<Biquad>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    LowPass,
    HighPass,
}

/// A 2nd-order polynomial from a pair of roots (conjugate or two real ones) or a 1st-order one
/// from a single real root: [1, c1, c2].
fn poly(roots: &[Complex<f64>]) -> [f64; 3] {
    match roots {
        [r] => [1.0, -r.re, 0.0],
        [r1, r2] => {
            let sum = r1 + r2;
            let prod = r1 * r2;
            [1.0, -sum.re, prod.re]
        }
        _ => [1.0, 0.0, 0.0],
    }
}

/// Splits roots into groups: conjugate pairs and real pairs (or a single real root).
fn group_roots(roots: &[Complex<f64>]) -> Vec<Vec<Complex<f64>>> {
    let mut groups = Vec::new();
    let mut reals = Vec::new();
    for r in roots {
        if r.im > 1e-12 {
            groups.push(vec![*r, r.conj()]);
        } else if r.im.abs() <= 1e-12 {
            reals.push(Complex::new(r.re, 0.0));
        }
    }
    for pair in reals.chunks(2) {
        groups.push(pair.to_vec());
    }
    groups
}

impl Sos {
    /// Chebyshev II design of order `n` with the stopband edge `stop_hz` and stopband
    /// attenuation `stop_db`, using the bilinear transform with prewarping.
    fn cheby2(kind: Kind, n: usize, stop_hz: f64, stop_db: f64, fs: f64) -> Self {
        let eps = 1.0 / (10f64.powf(stop_db / 10.0) - 1.0).sqrt();
        let mu = (1.0 / eps).asinh() / n as f64;
        let k = 2.0 * fs;
        let ws = k * (std::f64::consts::PI * stop_hz / fs).tan();

        // Low-pass prototype with the stopband edge at 1 rad/s.
        let mut poles = Vec::with_capacity(n);
        let mut zeros = Vec::with_capacity(n);
        for i in 1..=n {
            let th = std::f64::consts::PI * (2 * i - 1) as f64 / (2 * n) as f64;
            let cheb1 = Complex::new(-mu.sinh() * th.sin(), mu.cosh() * th.cos());
            poles.push(cheb1.inv());
            if th.cos().abs() > 1e-9 {
                zeros.push(Complex::new(0.0, 1.0 / th.cos()));
            }
        }
        let infinite = n - zeros.len();

        // Transform to LP or HP with the edge ws, then bilinear: z = (k + s) / (k - s).
        let map = |s: Complex<f64>| (k + s) / (k - s);
        let (dp, mut dz): (Vec<_>, Vec<_>) = match kind {
            Kind::LowPass => (
                poles.iter().map(|&p| map(p * ws)).collect(),
                zeros.iter().map(|&z| map(z * ws)).collect(),
            ),
            Kind::HighPass => (
                poles.iter().map(|&p| map(ws / p)).collect(),
                zeros.iter().map(|&z| map(ws / z)).collect(),
            ),
        };
        // Zeros at infinity: LP -> z = -1, HP (zeros at s = 0) -> z = 1.
        let at_inf = if kind == Kind::LowPass { -1.0 } else { 1.0 };
        dz.extend((0..infinite).map(|_| Complex::new(at_inf, 0.0)));

        let pole_groups = group_roots(&dp);
        let mut zero_groups = group_roots(&dz);
        let mut sections = Vec::with_capacity(pole_groups.len());
        for pg in &pole_groups {
            // Zeros closest to the poles of this section.
            let best = zero_groups
                .iter()
                .enumerate()
                .filter(|(_, zg)| zg.len() == pg.len())
                .min_by(|a, b| {
                    let da = (a.1[0] - pg[0]).norm();
                    let db = (b.1[0] - pg[0]).norm();
                    da.total_cmp(&db)
                })
                .map(|(i, _)| i)
                .unwrap_or(0);
            let zg = if zero_groups.is_empty() {
                Vec::new()
            } else {
                zero_groups.remove(best)
            };
            let a = poly(pg);
            sections.push(Biquad {
                b: poly(&zg),
                a: [a[1], a[2]],
                s: [0.0; 2],
            });
        }
        let mut sos = Sos { sections };
        // Unity gain in the passband (DC for LP, Nyquist for HP).
        let z = Complex::new(if kind == Kind::LowPass { 1.0 } else { -1.0 }, 0.0);
        let g = sos.response(z).norm();
        if let Some(first) = sos.sections.first_mut() {
            for b in &mut first.b {
                *b /= g;
            }
        }
        sos
    }

    /// The lowest Chebyshev II order meeting attenuation `stop_db` from `stop_hz` and at most
    /// `PASS_DB` at `pass_hz` (analog frequencies after prewarping).
    fn order(pass_hz: f64, stop_hz: f64, stop_db: f64, fs: f64) -> usize {
        let w = |f: f64| (std::f64::consts::PI * f / fs).tan();
        let ratio = (w(pass_hz) / w(stop_hz)).max(w(stop_hz) / w(pass_hz));
        let d = ((10f64.powf(stop_db / 10.0) - 1.0) / (10f64.powf(PASS_DB / 10.0) - 1.0)).sqrt();
        (d.acosh() / ratio.acosh()).ceil().max(1.0) as usize
    }

    /// Band-pass filter `low`-`high` Hz (cascaded Chebyshev II HP and LP).
    pub fn bandpass(low: f64, high: f64, fs: f64) -> Self {
        let hp_stop = low * HP_STOP_RATIO;
        let lp_stop = high * LP_STOP_RATIO;
        let hp = Sos::cheby2(
            Kind::HighPass,
            Sos::order(low, hp_stop, HP_STOP_DB, fs),
            hp_stop,
            HP_STOP_DB,
            fs,
        );
        let lp = Sos::cheby2(
            Kind::LowPass,
            Sos::order(high, lp_stop, LP_STOP_DB, fs),
            lp_stop,
            LP_STOP_DB,
            fs,
        );
        let mut sections = hp.sections;
        sections.extend(lp.sections);
        Sos { sections }
    }

    /// Low-pass filter (Chebyshev II): at most 3 dB at `pass_hz`, at least `stop_db` from
    /// `stop_hz` (e.g. the anti-alias filter of the web GUI audio before 8 kHz).
    pub fn lowpass(pass_hz: f64, stop_hz: f64, stop_db: f64, fs: f64) -> Self {
        Sos::cheby2(
            Kind::LowPass,
            Sos::order(pass_hz, stop_hz, stop_db, fs),
            stop_hz,
            stop_db,
            fs,
        )
    }

    pub fn response(&self, z: Complex<f64>) -> Complex<f64> {
        self.sections.iter().map(|s| s.response(z)).product()
    }

    /// Attenuation in dB at frequency `f` (for response tests).
    #[cfg(test)]
    pub fn attenuation_db(&self, f: f64, fs: f64) -> f64 {
        let z = Complex::from_polar(1.0, 2.0 * std::f64::consts::PI * f / fs);
        -20.0 * self.response(z).norm().log10()
    }

    pub fn process(&mut self, x: f32) -> f32 {
        let mut y = x as f64;
        for s in &mut self.sections {
            y = s.process(y);
        }
        y as f32
    }

    pub fn reset(&mut self) {
        for s in &mut self.sections {
            s.s = [0.0; 2];
        }
    }
}

/// RNNoise frame: 10 ms at 48 kHz.
const NR_FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;
/// RNNoise works on samples in the 16-bit integer range.
const NR_SCALE: f32 = 32_768.0;

/// Share of the denoised signal at strength 0%: below about half RNNoise is not heard at all
/// (user's listening, 2026-10-19), so the strength scale covers 50-100% of the mix.
const NR_MIX_MIN: f32 = 0.5;

/// Mix of the denoised signal (0-1) for the strength in percent (`--nr-strength`, 0-100).
pub fn nr_mix(percent: u32) -> f32 {
    NR_MIX_MIN + (1.0 - NR_MIX_MIN) * percent.min(100) as f32 / 100.0
}

/// Speech noise reduction (RNNoise through `nnnoiseless`). It works in 10 ms
/// frames, so every sample is delayed by one frame; when switched off the samples go through the
/// same buffer unchanged, so switching on and off does not change the delay. No allocation
/// after `new`.
struct NoiseReducer {
    state: Box<nnnoiseless::DenoiseState<'static>>,
    input: [f32; NR_FRAME],
    output: [f32; NR_FRAME],
    pos: usize,
}

impl NoiseReducer {
    fn new() -> Self {
        NoiseReducer {
            state: nnnoiseless::DenoiseState::new(),
            input: [0.0; NR_FRAME],
            output: [0.0; NR_FRAME],
            pos: 0,
        }
    }

    /// Start of an activity: the frame buffers are cleared. The RNNoise state is kept (a new
    /// one would allocate); it adapts to the new station within a few frames.
    fn start(&mut self) {
        self.input = [0.0; NR_FRAME];
        self.output = [0.0; NR_FRAME];
        self.pos = 0;
    }

    /// `mix`: share of the denoised signal (0-1), the rest is the original (strength).
    fn process(&mut self, x: f32, on: bool, mix: f32) -> f32 {
        let y = self.output[self.pos];
        self.input[self.pos] = x * NR_SCALE;
        self.pos += 1;
        if self.pos == NR_FRAME {
            self.pos = 0;
            if on {
                self.state.process_frame(&mut self.output, &self.input);
                if mix < 1.0 {
                    for (y, x) in self.output.iter_mut().zip(&self.input) {
                        *y = mix * *y + (1.0 - mix) * x;
                    }
                }
            } else {
                self.output = self.input;
            }
        }
        y / NR_SCALE
    }
}

/// Audio chain settings from the command line.
#[derive(Clone, Copy, Debug)]
pub struct AudioDspConfig {
    /// Filter band (Hz) or `None` (`--audio-band off`).
    pub band: Option<(f64, f64)>,
    pub agc: bool,
    /// Target RMS level (dBFS) for NFM and for AM.
    pub agc_target_db: f32,
    pub agc_target_am_db: f32,
    pub agc_max_gain_db: f32,
}

/// AGC time constants.
const AGC_ATTACK_S: f32 = 0.007;
const AGC_RELEASE_S: f32 = 0.4;
/// Fast convergence only at the very start of an activity, shorter than the chain lookahead
/// (delay line of about 50 ms): the gain settles on the first samples before anything is heard,
/// and then the normal, slow release applies (no pumping in pauses).
const AGC_START_S: f32 = 0.04;
const AGC_START_RELEASE_S: f32 = 0.01;
/// Below this input level (RMS after the filter) the gain is frozen (silence, gaps).
const AGC_SILENCE_DB: f32 = -45.0;
/// Time constant of the input level measurement (mean power, RMS).
const LEVEL_S: f32 = 0.01;
/// Level peak with slow decay: the reference for detecting gaps between words.
const PEAK_DECAY_S: f32 = 1.0;
/// The gain does not rise when the level dropped by more than this relative to the peak (gap).
const GAP_DB: f32 = -12.0;
/// Limiter: soft limiting above -1 dBFS.
const LIMIT: f32 = 0.891;
/// Fade-in and fade-out length.
pub const FADE_S: f32 = 0.015;

fn coef(tau_s: f32, fs: f32) -> f32 {
    1.0 - (-1.0 / (tau_s * fs)).exp()
}

fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Audio chain with lookahead: `analyze` (band-pass filter, level measurement, gain computation)
/// works on samples entering the caller's delay line, and `finish` (gain, limiter, fade-in) on
/// samples leaving it. The AGC therefore knows the sound ahead by the line length and the first
/// syllable has the right level immediately. The caller does the fade-out via `fade_out_gain`.
/// No allocation.
pub struct AudioDsp {
    nr: NoiseReducer,
    /// Noise reduction on (`--nr`, the `n` key): set by the DSP thread.
    pub nr_on: bool,
    /// Noise reduction strength (`--nr-strength`, the `<`/`>` keys): share of the denoised
    /// signal, 0-1.
    pub nr_mix: f32,
    band: Option<Sos>,
    agc: bool,
    target_db: f32,
    max_gain_db: f32,
    /// AGC gain in dB.
    gain_db: f32,
    level: f32,
    peak: f32,
    since_start: f32,
    fade_len: f32,
    fade_pos: f32,
    /// Counters for the log.
    pub limiter_hits: u64,
    pub frozen_samples: u64,
    k_attack: f32,
    k_release: f32,
    k_start_release: f32,
    k_level: f32,
    k_peak: f32,
    gap: f32,
    silence: f32,
    dt: f32,
}

impl AudioDsp {
    pub fn new(cfg: &AudioDspConfig) -> Self {
        let fs = AUDIO_RATE as f32;
        AudioDsp {
            nr: NoiseReducer::new(),
            nr_on: false,
            nr_mix: 1.0,
            band: cfg.band.map(|(lo, hi)| Sos::bandpass(lo, hi, AUDIO_RATE)),
            agc: cfg.agc,
            target_db: cfg.agc_target_db,
            max_gain_db: cfg.agc_max_gain_db,
            gain_db: 0.0,
            level: 0.0,
            peak: 0.0,
            since_start: 0.0,
            fade_len: FADE_S * fs,
            fade_pos: 0.0,
            limiter_hits: 0,
            frozen_samples: 0,
            k_attack: coef(AGC_ATTACK_S, fs),
            k_release: coef(AGC_RELEASE_S, fs),
            k_start_release: coef(AGC_START_RELEASE_S, fs),
            k_level: coef(LEVEL_S, fs),
            k_peak: coef(PEAK_DECAY_S, fs),
            gap: db_to_lin(GAP_DB),
            silence: db_to_lin(AGC_SILENCE_DB),
            dt: 1.0 / fs,
        }
    }

    /// Start of an activity with target level `target_db`: resets the filter state and fade-in.
    /// The AGC gain is kept from the previous activity (bounded by the limit), so the first
    /// syllable has a sensible level.
    pub fn start(&mut self, target_db: f32) {
        self.target_db = target_db;
        self.nr.start();
        if let Some(b) = &mut self.band {
            b.reset();
        }
        self.gain_db = self.gain_db.min(self.max_gain_db);
        self.level = 0.0;
        self.peak = 0.0;
        self.since_start = 0.0;
        self.fade_pos = 0.0;
    }

    /// AGC gain in dB (for the log).
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Analysis of a sample entering the delay line: band-pass filter and AGC gain update.
    /// Returns the filtered sample (without gain).
    pub fn analyze(&mut self, x: f32) -> f32 {
        let x = match &mut self.band {
            Some(b) => b.process(x),
            None => x,
        };
        if self.agc {
            // Input level (RMS) and its peak with slow decay.
            let p = x * x;
            self.level += self.k_level * (p - self.level);
            let rms = self.level.sqrt();
            self.peak = rms.max(self.peak - self.k_peak * self.peak);
            // Silence or a gap between words: the gain is frozen (does not rise).
            let gap = rms < self.silence || rms < self.peak * self.gap;
            let rms_db = 20.0 * rms.max(1e-9).log10();
            let desired = (self.target_db - rms_db).min(self.max_gain_db);
            if desired < self.gain_db {
                self.gain_db += self.k_attack * (desired - self.gain_db);
            } else if gap || p < 0.5 * self.level {
                // The level is falling: do not raise the gain.
                self.frozen_samples += u64::from(gap);
            } else {
                let k = if self.since_start < AGC_START_S {
                    self.k_start_release
                } else {
                    self.k_release
                };
                self.gain_db += k * (desired - self.gain_db);
            }
        }
        self.since_start += self.dt;
        x
    }

    /// A sample leaving the delay line: current AGC gain, noise reduction (on the level set by
    /// the AGC, RNNoise depends on the input level), limiter, fade-in.
    pub fn finish(&mut self, x: f32) -> f32 {
        let mut y = if self.agc {
            x * db_to_lin(self.gain_db)
        } else {
            x
        };
        y = self.nr.process(y, self.nr_on, self.nr_mix);

        // Limiter: soft limiting above LIMIT (asymptotically to 1.0).
        let a = y.abs();
        if a > LIMIT {
            self.limiter_hits += 1;
            let over = (a - LIMIT) / (1.0 - LIMIT);
            y = y.signum() * (LIMIT + (1.0 - LIMIT) * over.tanh());
        }

        // Fade-in.
        if self.fade_pos < self.fade_len {
            y *= self.fade_pos / self.fade_len;
            self.fade_pos += 1.0;
        }
        y
    }

    /// Analysis and output without a delay line (chain tests).
    #[cfg(test)]
    pub fn process(&mut self, x: f32) -> f32 {
        let a = self.analyze(x);
        self.finish(a)
    }
    /// Fade-out factor for sample `i` of the last `n` (linearly to zero).
    pub fn fade_out_gain(i: usize, n: usize) -> f32 {
        if n == 0 {
            0.0
        } else {
            1.0 - (i as f32 + 1.0) / n as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_band_meets_spec() {
        let f = Sos::bandpass(300.0, 3000.0, AUDIO_RATE);
        let a = |hz: f64| f.attenuation_db(hz, AUDIO_RATE);
        for hz in [100.0, 150.0, 250.0, 300.0, 1000.0, 3000.0, 4000.0] {
            println!("{hz:6} Hz: {:5.1} dB", a(hz));
        }
        assert!(a(150.0) >= 40.0 - 0.05, "150 Hz: {}", a(150.0));
        assert!(a(250.0) >= 20.0, "250 Hz: {}", a(250.0));
        assert!(a(300.0) <= 3.0, "300 Hz: {}", a(300.0));
        assert!(a(1000.0) < 0.5, "1000 Hz: {}", a(1000.0));
        assert!(a(3000.0) <= 3.0, "3000 Hz: {}", a(3000.0));
        assert!(a(4000.0) >= 20.0, "4000 Hz: {}", a(4000.0));
        // All poles inside the unit circle (stable filter).
        for s in &f.sections {
            let (a1, a2) = (s.a[0], s.a[1]);
            assert!(
                a2.abs() < 1.0 && a1.abs() < 1.0 + a2,
                "unstable section {a1} {a2}"
            );
        }
    }

    #[test]
    fn filter_processes_without_blowing_up() {
        let mut f = Sos::bandpass(300.0, 3000.0, AUDIO_RATE);
        // A 1 kHz tone passes with amplitude ~1, 100 Hz is strongly attenuated.
        let tone = |f: &mut Sos, hz: f64| {
            f.reset();
            let mut peak = 0.0f32;
            for i in 0..48_000 {
                let x = (2.0 * std::f64::consts::PI * hz * i as f64 / AUDIO_RATE).sin() as f32;
                let y = f.process(x);
                if i > 24_000 {
                    peak = peak.max(y.abs());
                }
            }
            peak
        };
        assert!((tone(&mut f, 1000.0) - 1.0).abs() < 0.06);
        assert!(tone(&mut f, 100.0) < 0.01);
    }

    #[test]
    fn agc_levels_and_limits() {
        let cfg = AudioDspConfig {
            band: Some((300.0, 3000.0)),
            agc: true,
            agc_target_db: -18.0,
            agc_target_am_db: -18.0,
            agc_max_gain_db: 36.0,
        };
        // Two 1 kHz signals differing by 20 dB give a similar output level.
        let level = |amp: f32| {
            let mut d = AudioDsp::new(&cfg);
            d.start(cfg.agc_target_db);
            let mut acc = 0.0f64;
            let mut peak = 0.0f32;
            for i in 0..96_000 {
                let x = amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin();
                let y = d.process(x);
                peak = peak.max(y.abs());
                if i >= 48_000 {
                    acc += (y as f64) * (y as f64);
                }
            }
            (10.0 * (acc / 48_000.0).log10(), peak)
        };
        let (loud, peak_loud) = level(0.5);
        let (quiet, _) = level(0.05);
        assert!((loud + 18.0).abs() < 1.5, "loud: {loud} dBFS");
        assert!((quiet + 18.0).abs() < 1.5, "quiet: {quiet} dBFS");
        // Limiter: nothing exceeds full scale.
        assert!(peak_loud < 1.0);
    }

    /// After an activity with a weak station (high gain) a strong station does not start too
    /// loud: with a 50 ms lookahead the first 100 ms of output are close to the target level.
    #[test]
    fn lookahead_start_is_not_too_loud() {
        let cfg = AudioDspConfig {
            band: Some((300.0, 3000.0)),
            agc: true,
            agc_target_db: -10.0,
            agc_target_am_db: -10.0,
            agc_max_gain_db: 36.0,
        };
        let tone = |amp: f32, i: usize| {
            amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin()
        };
        let mut d = AudioDsp::new(&cfg);
        // Weak station: the gain rises high.
        d.start(-10.0);
        for i in 0..96_000 {
            d.process(tone(0.02, i));
        }
        assert!(d.gain_db() > 20.0, "{}", d.gain_db());
        // Strong station with a 50 ms delay line (2400 samples).
        d.start(-10.0);
        let delay = 2400;
        let mut line = std::collections::VecDeque::new();
        let (mut acc, mut n) = (0.0f64, 0);
        for i in 0..delay + 4800 {
            line.push_back(d.analyze(tone(0.5, i)));
            if line.len() > delay {
                let y = d.finish(line.pop_front().unwrap_or(0.0));
                // Skip the fade-in (15 ms).
                if i >= delay + 720 {
                    acc += (y as f64) * (y as f64);
                    n += 1;
                }
            }
        }
        let rms_db = 10.0 * (acc / n as f64).log10();
        assert!((rms_db + 10.0).abs() < 3.0, "first 100 ms: {rms_db} dBFS");
    }

    #[test]
    fn noise_reducer_delays_and_stays_bounded() {
        // Deterministic white noise (LCG).
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let input: Vec<f32> = (0..96_000).map(|_| 0.2 * noise()).collect();
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();

        // Off: the same samples one frame later.
        let mut nr = NoiseReducer::new();
        let off: Vec<f32> = input.iter().map(|&x| nr.process(x, false, 1.0)).collect();
        assert!(off[..NR_FRAME].iter().all(|&y| y == 0.0));
        assert!(
            off[NR_FRAME..]
                .iter()
                .zip(&input)
                .all(|(y, x)| (y - x).abs() < 1e-6)
        );

        // On with strength 0: only the original, delayed by one frame.
        let mut nr = NoiseReducer::new();
        let dry: Vec<f32> = input.iter().map(|&x| nr.process(x, true, 0.0)).collect();
        assert!(
            dry[NR_FRAME..]
                .iter()
                .zip(&input)
                .all(|(y, x)| (y - x).abs() < 1e-6)
        );

        // Strength scale: 0% = half of the denoised signal, 60% = 0.8 (the user's choice).
        assert_eq!(nr_mix(0), 0.5);
        assert!((nr_mix(60) - 0.8).abs() < 1e-6);
        assert_eq!(nr_mix(100), 1.0);

        // On: the output stays finite and bounded. How much radio noise is removed is judged by
        // listening: RNNoise was trained on other noise, synthetic white noise is
        // attenuated unevenly depending on its level, so it is only printed here.
        let mut nr = NoiseReducer::new();
        let on: Vec<f32> = input.iter().map(|&x| nr.process(x, true, 1.0)).collect();
        assert!(on.iter().all(|y| y.is_finite() && y.abs() < 1.0));
        let gain_db = 20.0 * (rms(&on[48_000..]) / rms(&input[48_000..])).log10();
        println!("white noise attenuation: {gain_db:.1} dB");
    }

    #[test]
    fn agc_freezes_in_silence() {
        let cfg = AudioDspConfig {
            band: None,
            agc: true,
            agc_target_db: -18.0,
            agc_target_am_db: -18.0,
            agc_max_gain_db: 36.0,
        };
        let mut d = AudioDsp::new(&cfg);
        d.start(cfg.agc_target_db);
        for i in 0..48_000 {
            d.process(0.1 * (i as f32 * 0.13).sin());
        }
        let before = d.gain_db();
        // One second of silence (level below the threshold): the gain does not change.
        for _ in 0..48_000 {
            d.process(1e-4);
        }
        assert!(
            (d.gain_db() - before).abs() < 0.5,
            "change {} dB",
            d.gain_db() - before
        );
        assert!(d.frozen_samples > 40_000);
    }
}
