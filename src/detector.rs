//! Channel power measurement in a window: FFT, aggregation of bins into channels, noise floor.
//!
//! Normalization: bin power = |X_k|² / (N · Σw²), so the sum of all bin powers for a full-scale
//! complex signal (|x| = 1) is 1, i.e. 0 dBFS (Parseval's theorem including the window). The
//! channel power is the sum of bin powers within `bw` around its center (partially covered bins
//! weighted by coverage).
//!
//! Noise floor: the median FFT bin power within the window's usable band (excluding DC mask
//! bins), with a median -> mean correction for averaged frames, smoothed over time and scaled to
//! each channel's width. This keeps noise and RSSI in the same bandwidth, and the estimate also
//! works in a window with a single channel (channel mode).

use std::ops::Range;
use std::sync::Arc;

use num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::plan::ScanPlan;

/// Noise floor smoothing factor over time (per window measurement).
const FLOOR_ALPHA: f32 = 0.3;
/// A channel with less than this fraction of its band outside the DC mask cannot be measured.
const MIN_UNMASKED: f64 = 0.5;

pub struct DetectorConfig {
    pub fft_size: usize,
    /// Number of averaged FFT frames per window.
    pub frames: usize,
    /// Masked bins on each side of DC.
    pub dc_mask: usize,
    pub sample_rate: f64,
    /// Usable window band (Hz): the noise floor is computed from these bins.
    pub usable_hz: f64,
}

/// Contribution of one bin (index after the shift, DC = N/2) to the channel power.
#[derive(Clone, Copy)]
struct BinWeight {
    bin: u32,
    weight: f32,
}

/// Measurement of one window.
pub struct WindowResult<'a> {
    /// Noise floor in each channel's width (dBFS, smoothed over time), parallel to `powers_db`.
    pub floors_db: &'a [f32],
    /// Power of the window's measured channels (dBFS) and their frequencies, ascending.
    pub powers_db: &'a [f32],
    pub freqs: &'a [u64],
    /// Channel indices in the plan grid.
    pub indices: &'a [u32],
}

pub struct Detector {
    fft: Arc<dyn Fft<f32>>,
    n: usize,
    frames: usize,
    taper: Vec<f32>,
    /// 1 / (frames · N · Σw²).
    norm: f32,
    frame: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    power: Vec<f32>,
    weights: Vec<BinWeight>,
    /// For each measured channel: the range in `weights` and the DC mask factor.
    channel_bins: Vec<Range<usize>>,
    channel_scale: Vec<f32>,
    channel_freq: Vec<u64>,
    channel_index: Vec<u32>,
    /// 10·log10(channel coverage in bins): converts noise per bin into noise in the channel.
    channel_cov_db: Vec<f32>,
    /// Bins (after the shift) in the usable band, outside the DC mask: the noise floor sample.
    noise_bins: Vec<u32>,
    /// Median -> mean correction for a bin power averaged over `frames` frames (dB).
    median_to_mean_db: f32,
    /// For each window (passes in order): the range in the channel arrays.
    windows: Vec<Range<usize>>,
    powers_db: Vec<f32>,
    floors_db: Vec<f32>,
    sort_scratch: Vec<f32>,
    /// Noise floor per bin for each window (dBFS, smoothed).
    floor_db: Vec<f32>,
    floor_ready: Vec<bool>,
    bin_hz: f64,
    dc_mask: usize,
}

impl Detector {
    /// Builds the FFT plan and bin tables for all plan windows (once, at startup).
    pub fn new(plan: &ScanPlan, cfg: &DetectorConfig) -> Result<Self, String> {
        let n = cfg.fft_size;
        if n < 16 {
            return Err(format!("FFT size {n} is too small"));
        }
        let fft = FftPlanner::new().plan_fft_forward(n);
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        // Hann window.
        let taper: Vec<f32> = (0..n)
            .map(|i| {
                let x = std::f64::consts::PI * 2.0 * i as f64 / n as f64;
                (0.5 - 0.5 * x.cos()) as f32
            })
            .collect();
        let sum_w2: f64 = taper.iter().map(|&w| (w as f64) * (w as f64)).sum();
        let norm = (1.0 / (cfg.frames as f64 * n as f64 * sum_w2)) as f32;

        let bin_hz = cfg.sample_rate / n as f64;
        let half = (n / 2) as f64;
        let dc = (n / 2) as i64;
        let mut weights = Vec::new();
        let mut channel_bins = Vec::new();
        let mut channel_scale = Vec::new();
        let mut channel_freq = Vec::new();
        let mut channel_index = Vec::new();
        let mut channel_cov_db = Vec::new();
        let mut windows = Vec::new();
        let mut max_channels = 0;

        for w in plan.passes.iter().flatten() {
            let first_channel = channel_freq.len();
            for i in w.first_index..w.first_index + w.channels {
                if !w.is_measured(i) {
                    continue;
                }
                let freq = plan.channel(i);
                let bw = plan.channels[i as usize].bw;
                let offset = freq as f64 - w.center as f64;
                let (lo, hi) = (offset - bw / 2.0, offset + bw / 2.0);
                let s_lo = ((lo / bin_hz + half).round() as i64).max(0);
                let s_hi = ((hi / bin_hz + half).round() as i64).min(n as i64 - 1);
                let start = weights.len();
                let (mut total, mut unmasked) = (0.0, 0.0);
                for s in s_lo..=s_hi {
                    let a = (s as f64 - half - 0.5) * bin_hz;
                    let cover = ((a + bin_hz).min(hi) - a.max(lo)) / bin_hz;
                    if cover <= 0.0 {
                        continue;
                    }
                    total += cover;
                    if (s - dc).unsigned_abs() as usize <= cfg.dc_mask {
                        continue;
                    }
                    unmasked += cover;
                    weights.push(BinWeight {
                        bin: s as u32,
                        weight: cover as f32,
                    });
                }
                if total <= 0.0 {
                    return Err(format!(
                        "channel {:.6} MHz does not fit in the band of the window with LO {:.6} MHz",
                        freq as f64 / 1e6,
                        w.center as f64 / 1e6
                    ));
                }
                if unmasked < MIN_UNMASKED * total {
                    return Err(format!(
                        "channel {:.6} MHz in the window with LO {:.6} MHz lies mostly under the DC mask; increase --dc-guard or decrease --dc-mask",
                        freq as f64 / 1e6,
                        w.center as f64 / 1e6
                    ));
                }
                channel_bins.push(start..weights.len());
                channel_scale.push((total / unmasked) as f32);
                channel_freq.push(freq);
                channel_index.push(i as u32);
                channel_cov_db.push((10.0 * total.log10()) as f32);
            }
            max_channels = max_channels.max(channel_freq.len() - first_channel);
            windows.push(first_channel..channel_freq.len());
        }

        let nwin = windows.len();
        let noise_bins: Vec<u32> = (0..n as i64)
            .filter(|&s| {
                let offset = (s as f64 - half) * bin_hz;
                offset.abs() <= cfg.usable_hz / 2.0
                    && (s - dc).unsigned_abs() as usize > cfg.dc_mask
            })
            .map(|s| s as u32)
            .collect();
        if noise_bins.is_empty() {
            return Err("no FFT bins to estimate the noise floor".into());
        }
        // A bin power from F averaged frames has a Gamma(F) distribution; median to mean ratio:
        // ln 2 for F = 1, otherwise the approximation (F - 1/3 + 0.02/F) / F (error < 0.05 dB).
        let f = cfg.frames as f64;
        let ratio = if cfg.frames == 1 {
            std::f64::consts::LN_2
        } else {
            (f - 1.0 / 3.0 + 0.02 / f) / f
        };
        let median_to_mean_db = (-10.0 * ratio.log10()) as f32;
        Ok(Detector {
            fft,
            n,
            frames: cfg.frames,
            taper,
            norm,
            frame: vec![Complex::new(0.0, 0.0); n],
            scratch,
            power: vec![0.0; n],
            weights,
            channel_bins,
            channel_scale,
            channel_freq,
            channel_index,
            channel_cov_db,
            sort_scratch: vec![0.0; noise_bins.len()],
            noise_bins,
            median_to_mean_db,
            windows,
            powers_db: vec![0.0; max_channels],
            floors_db: vec![0.0; max_channels],
            floor_db: vec![0.0; nwin],
            floor_ready: vec![false; nwin],
            bin_hz,
            dc_mask: cfg.dc_mask,
        })
    }

    /// Number of samples needed to measure one window.
    pub fn samples_needed(&self) -> usize {
        self.n * self.frames
    }

    /// Measures the window with index `w` (order: pass A, then B) from samples `iq`
    /// (normalized to ±1, at least `samples_needed()`). No allocation.
    pub fn measure(&mut self, w: usize, iq: &[Complex<f32>]) -> WindowResult<'_> {
        self.spectrum(iq);

        let range = self.windows[w].clone();
        let m = range.len();
        for (j, ch) in range.clone().enumerate() {
            let p: f32 = self.weights[self.channel_bins[ch].clone()]
                .iter()
                .map(|b| b.weight * self.power[b.bin as usize])
                .sum();
            let p = p * self.norm * self.channel_scale[ch];
            self.powers_db[j] = 10.0 * p.max(1e-20).log10();
        }

        // Noise floor: median bin power in the usable band (outside the DC mask), smoothed over
        // time.
        let bin_db = self.floor_bin_db();
        if self.floor_ready[w] {
            self.floor_db[w] += FLOOR_ALPHA * (bin_db - self.floor_db[w]);
        } else {
            self.floor_db[w] = bin_db;
            self.floor_ready[w] = true;
        }
        for (j, ch) in range.clone().enumerate() {
            self.floors_db[j] = self.floor_db[w] + self.channel_cov_db[ch];
        }

        WindowResult {
            floors_db: &self.floors_db[..m],
            powers_db: &self.powers_db[..m],
            freqs: &self.channel_freq[range.clone()],
            indices: &self.channel_index[range],
        }
    }

    /// Measures one channel lying `offset_hz` from the LO, `bw` (Hz) wide, at any LO (hit
    /// confirmation, Locked state). Returns the channel power and the noise floor in the same
    /// width (dBFS). No allocation.
    pub fn measure_channel(&mut self, iq: &[Complex<f32>], offset_hz: f64, bw: f64) -> (f32, f32) {
        self.spectrum(iq);
        let n = self.n;
        let half = (n / 2) as f64;
        let dc = (n / 2) as i64;
        let (lo, hi) = (offset_hz - bw / 2.0, offset_hz + bw / 2.0);
        let s_lo = ((lo / self.bin_hz + half).round() as i64).max(0);
        let s_hi = ((hi / self.bin_hz + half).round() as i64).min(n as i64 - 1);
        let (mut total, mut unmasked, mut p) = (0.0f64, 0.0f64, 0.0f32);
        for s in s_lo..=s_hi {
            let a = (s as f64 - half - 0.5) * self.bin_hz;
            let cover = ((a + self.bin_hz).min(hi) - a.max(lo)) / self.bin_hz;
            if cover <= 0.0 {
                continue;
            }
            total += cover;
            if (s - dc).unsigned_abs() as usize <= self.dc_mask {
                continue;
            }
            unmasked += cover;
            p += cover as f32 * self.power[s as usize];
        }
        if unmasked <= 0.0 {
            return (f32::NEG_INFINITY, 0.0);
        }
        let p = p * self.norm * (total / unmasked) as f32;
        let floor = self.floor_bin_db() + (10.0 * total.log10()) as f32;
        (10.0 * p.max(1e-20).log10(), floor)
    }

    /// Power spectrum of block `iq` (averaged frames, DC at index N/2) in `self.power`.
    fn spectrum(&mut self, iq: &[Complex<f32>]) {
        let n = self.n;
        let half = n / 2;
        self.power.fill(0.0);
        for f in 0..self.frames {
            self.frame.copy_from_slice(&iq[f * n..(f + 1) * n]);
            // Remove the DC component of the current block before the FFT.
            let mean = self.frame.iter().sum::<Complex<f32>>() / n as f32;
            for (x, &t) in self.frame.iter_mut().zip(&self.taper) {
                *x = (*x - mean) * t;
            }
            self.fft
                .process_with_scratch(&mut self.frame, &mut self.scratch);
            // Spectrum shift: DC at index N/2, frequencies ascending.
            for (k, x) in self.frame.iter().enumerate() {
                let s = if k < half { k + half } else { k - half };
                self.power[s] += x.norm_sqr();
            }
        }
    }

    /// Noise floor per bin (dBFS): median of the usable band bins outside the DC mask, with the
    /// median -> mean correction.
    fn floor_bin_db(&mut self) -> f32 {
        for (dst, &b) in self.sort_scratch.iter_mut().zip(&self.noise_bins) {
            *dst = self.power[b as usize];
        }
        let mid = self.sort_scratch.len() / 2;
        let (_, median, _) = self
            .sort_scratch
            .select_nth_unstable_by(mid, f32::total_cmp);
        10.0 * (*median * self.norm).max(1e-30).log10() + self.median_to_mean_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Modulation, RangeSpec, Step, ranges_plan};

    fn plan(start: u64, stop: u64, step: Step, usable: u64, guard: u64) -> Option<ScanPlan> {
        let r = RangeSpec {
            start,
            stop,
            step,
            bw: 12_500.0,
            bw_explicit: false,
            mode: Modulation::Nfm,
        };
        ranges_plan(&[r], usable, guard).ok()
    }

    const RATE: f64 = 6_000_000.0;

    fn detector(p: &ScanPlan) -> Detector {
        let cfg = DetectorConfig {
            fft_size: 2048,
            frames: 2,
            dc_mask: 1,
            sample_rate: RATE,
            usable_hz: 4_800_000.0,
        };
        Detector::new(p, &cfg).unwrap()
    }

    /// Complex tone with amplitude `amp` at frequency `f` relative to the LO.
    fn tone(f: f64, amp: f32, len: usize) -> Vec<Complex<f32>> {
        (0..len)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * f * i as f64 / RATE;
                Complex::new(ph.cos() as f32, ph.sin() as f32) * amp
            })
            .collect()
    }

    #[test]
    fn measure_channel_gives_0_dbfs_for_full_scale_tone() {
        let p = plan(
            430_000_000,
            440_000_000,
            Step::from_hz(12_500),
            4_800_000,
            1,
        )
        .unwrap();
        let mut d = detector(&p);
        // A channel anywhere in the band, also off the window grid (confirmation at another LO).
        for offset in [-1_234_567.0, 18_750.0, 2_000_000.0] {
            let iq = tone(offset, 1.0, d.samples_needed());
            let (power, floor) = d.measure_channel(&iq, offset, 12_500.0);
            assert!(power.abs() < 0.5, "{offset}: {power}");
            assert!(floor < power - 40.0, "{offset}: {floor}");
        }
    }
    #[test]
    fn full_scale_tone_is_0_dbfs_in_its_channel() {
        let p = plan(
            430_000_000,
            440_000_000,
            Step::from_hz(12_500),
            4_800_000,
            1,
        )
        .unwrap();
        let mut d = detector(&p);
        let w = p.passes[0][0];
        let target = 431_000_000u64;
        let iq = tone(target as f64 - w.center as f64, 1.0, d.samples_needed());
        let r = d.measure(0, &iq);
        let j = r.freqs.iter().position(|&f| f == target).unwrap();
        assert!(r.powers_db[j].abs() < 0.5, "power {} dBFS", r.powers_db[j]);
        // Adjacent channels clearly lower, noise floor (median) very low.
        assert!(r.powers_db[j - 2] < -30.0 && r.powers_db[j + 2] < -30.0);
        assert!(r.floors_db[j] < -60.0);
    }

    /// Complex white noise with power `power` (deterministic generator, Gaussian approximation).
    fn noise(power: f32, len: usize) -> Vec<Complex<f32>> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut uniform = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32 - 0.5
        };
        // The sum of 12 uniform distributions has variance 1.
        let sigma = (power / 2.0).sqrt();
        let mut gauss = move || (0..12).map(|_| uniform()).sum::<f32>() * sigma;
        (0..len).map(|_| Complex::new(gauss(), gauss())).collect()
    }

    #[test]
    fn floor_matches_noise_power_in_channel() {
        let p = plan(
            430_000_000,
            440_000_000,
            Step::from_hz(12_500),
            4_800_000,
            1,
        )
        .unwrap();
        let mut d = detector(&p);
        // Noise of -40 dBFS in 6 MHz: in a 12.5 kHz channel that is -40 + 10·log10(12.5/6000)
        // ≈ -66.8 dBFS.
        let iq = noise(1e-4, d.samples_needed());
        let r = d.measure(0, &iq);
        let expected = -40.0 + 10.0 * (12_500.0f32 / 6_000_000.0).log10();
        let mean_snr = r
            .powers_db
            .iter()
            .zip(r.floors_db)
            .map(|(p, f)| p - f)
            .sum::<f32>()
            / r.powers_db.len() as f32;
        assert!(
            (r.floors_db[0] - expected).abs() < 1.0,
            "noise {} dBFS, expected {expected}",
            r.floors_db[0]
        );
        // Mean SNR of noise alone close to 0 dB (median -> mean correction).
        assert!(mean_snr.abs() < 0.7, "mean noise SNR {mean_snr} dB");
    }

    #[test]
    fn dc_offset_does_not_raise_channels() {
        let p = plan(
            430_000_000,
            440_000_000,
            Step::from_hz(12_500),
            4_800_000,
            0,
        )
        .unwrap();
        let mut d = detector(&p);
        // A strong DC component: after mean removal and masking it does not raise any channel.
        let iq = vec![Complex::new(0.5f32, -0.3); d.samples_needed()];
        let r = d.measure(0, &iq);
        let max = r.powers_db.iter().cloned().fold(f32::MIN, f32::max);
        assert!(max < -60.0, "max channel power {max} dBFS");
    }
}
