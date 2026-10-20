//! AM demodulator: envelope detector normalized by the slowly averaged carrier amplitude
//! (carrier-level AGC) with the DC component removed.

use num_complex::Complex;

/// Time constant of the average carrier amplitude.
const CARRIER_S: f64 = 0.3;
/// At the start of an activity the carrier is estimated faster, so the first syllable has a level.
const CARRIER_START_S: f64 = 0.02;
const START_S: f64 = 0.1;

pub struct AmDemod {
    carrier: f32,
    k: f32,
    k_start: f32,
    start_samples: u32,
    n: u32,
}

impl AmDemod {
    pub fn new(rate: f64) -> Self {
        let coef = |tau: f64| (1.0 - (-1.0 / (tau * rate)).exp()) as f32;
        AmDemod {
            carrier: 0.0,
            k: coef(CARRIER_S),
            k_start: coef(CARRIER_START_S),
            start_samples: (START_S * rate) as u32,
            n: 0,
        }
    }

    pub fn reset(&mut self) {
        self.carrier = 0.0;
        self.n = 0;
    }

    /// Baseband sample -> audio sample (modulation depth · 0.5). No allocation.
    pub fn process(&mut self, x: Complex<f32>) -> f32 {
        let env = x.norm();
        if self.n == 0 {
            // The carrier average starts from the first sample, not from zero: dividing by almost
            // zero gave a huge impulse at the start of an activity (a click and AGC squeezed by ~30 dB).
            self.carrier = env;
        }
        let k = if self.n < self.start_samples {
            self.n += 1;
            self.k_start
        } else {
            self.k
        };
        self.carrier += k * (env - self.carrier);
        if self.carrier <= 1e-12 {
            return 0.0;
        }
        // With modulation up to 100% the result fits in ±0.5; the clamp protects against impulses.
        (0.5 * (env / self.carrier - 1.0)).clamp(-1.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_has_no_impulse() {
        let mut d = AmDemod::new(48_000.0);
        // First samples of an activity with an unmodulated carrier: output close to zero.
        for _ in 0..100 {
            let y = d.process(Complex::new(0.3, 0.0));
            assert!(y.abs() < 0.01, "{y}");
        }
    }

    #[test]
    fn output_does_not_depend_on_carrier_level() {
        let rate = 48_000.0;
        // 50% modulation with a 1 kHz tone at two carrier levels differing by 40 dB.
        let peak = |carrier: f32| {
            let mut d = AmDemod::new(rate);
            let mut p = 0.0f32;
            for i in 0..48_000 {
                let m = 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin();
                let y = d.process(Complex::new(carrier * (1.0 + m), 0.0));
                if i > 24_000 {
                    p = p.max(y.abs());
                }
            }
            p
        };
        let (a, b) = (peak(0.5), peak(0.005));
        assert!(
            (a - 0.25).abs() < 0.02 && (b - 0.25).abs() < 0.02,
            "{a} {b}"
        );
    }
}
