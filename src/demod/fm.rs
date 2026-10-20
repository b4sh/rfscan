//! Narrowband FM (NFM) demodulator: phase discriminator and optional de-emphasis.

use num_complex::Complex;

/// Deviation that gives an output amplitude of 0.5 (typical speech deviation in 12.5 kHz NFM).
/// This is only the AGC input scale, not a receiver parameter.
const NOMINAL_DEVIATION_HZ: f64 = 2_500.0;

pub struct FmDemod {
    prev: Complex<f32>,
    scale: f32,
    /// De-emphasis coefficient (single-pole filter) or `None` (`--deemph off`).
    deemph: Option<f32>,
    y: f32,
    /// Whether the previous sample belongs to the current activity (the first gives 0, no phase jump).
    primed: bool,
}

impl FmDemod {
    /// `rate`: input sample rate, `deemph_us`: de-emphasis time constant in µs.
    pub fn new(rate: f64, deemph_us: Option<f64>) -> Self {
        let scale = 0.5 * rate / (2.0 * std::f64::consts::PI * NOMINAL_DEVIATION_HZ);
        FmDemod {
            prev: Complex::new(1.0, 0.0),
            scale: scale as f32,
            deemph: deemph_us.map(|us| (1.0 - (-1.0 / (us * 1e-6 * rate)).exp()) as f32),
            y: 0.0,
            primed: false,
        }
    }

    pub fn reset(&mut self) {
        self.prev = Complex::new(1.0, 0.0);
        self.y = 0.0;
        self.primed = false;
    }

    /// Baseband sample -> audio sample. No allocation.
    pub fn process(&mut self, x: Complex<f32>) -> f32 {
        if !self.primed {
            // First sample of an activity: no previous one, so no random phase jump.
            self.prev = x;
            self.primed = true;
            return 0.0;
        }
        let d = x * self.prev.conj();
        self.prev = x;
        let v = d.im.atan2(d.re) * self.scale;
        match self.deemph {
            Some(a) => {
                self.y += a * (v - self.y);
                self.y
            }
            None => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deviation_maps_to_nominal_amplitude() {
        let rate = 48_000.0;
        let mut d = FmDemod::new(rate, None);
        // A constant 2.5 kHz frequency offset gives an output of 0.5.
        let mut ph = 0.0f64;
        let mut last = 0.0;
        for _ in 0..1000 {
            ph += 2.0 * std::f64::consts::PI * 2_500.0 / rate;
            last = d.process(Complex::from_polar(1.0, ph as f32));
        }
        assert!((last - 0.5).abs() < 1e-3, "{last}");
    }
}
