//! IQ sample source abstraction. The scanner does not call anything from the `soapysdr` crate
//! directly.

pub mod soapy;

use num_complex::Complex;

/// Complex sample format supported by the DSP loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Cu8,
    Cs8,
    Cs16,
    Cf32,
}

impl SampleFormat {
    pub fn name(self) -> &'static str {
        match self {
            SampleFormat::Cu8 => "CU8",
            SampleFormat::Cs8 => "CS8",
            SampleFormat::Cs16 => "CS16",
            SampleFormat::Cf32 => "CF32",
        }
    }
}

/// Sample buffer in the stream format. Allocated once, then only overwritten.
pub enum IqBuffer {
    Cu8(Vec<Complex<u8>>),
    Cs8(Vec<Complex<i8>>),
    Cs16(Vec<Complex<i16>>),
    Cf32(Vec<Complex<f32>>),
}

impl IqBuffer {
    pub fn new(format: SampleFormat, len: usize) -> Self {
        match format {
            SampleFormat::Cu8 => IqBuffer::Cu8(vec![Complex::new(0, 0); len]),
            SampleFormat::Cs8 => IqBuffer::Cs8(vec![Complex::new(0, 0); len]),
            SampleFormat::Cs16 => IqBuffer::Cs16(vec![Complex::new(0, 0); len]),
            SampleFormat::Cf32 => IqBuffer::Cf32(vec![Complex::new(0.0, 0.0); len]),
        }
    }

    /// Normalizes the first `n` samples to ±1 according to the full scale and writes them to
    /// `out`. No allocation.
    pub fn to_cf32(&self, n: usize, full_scale: f64, out: &mut [Complex<f32>]) {
        let k = (1.0 / full_scale) as f32;
        match self {
            // Unsigned format: zero in the middle of the range (128 for 8 bits).
            IqBuffer::Cu8(b) => {
                for (o, s) in out.iter_mut().zip(&b[..n]) {
                    *o = Complex::new((s.re as f32 - 128.0) * k, (s.im as f32 - 128.0) * k);
                }
            }
            IqBuffer::Cs8(b) => {
                for (o, s) in out.iter_mut().zip(&b[..n]) {
                    *o = Complex::new(s.re as f32 * k, s.im as f32 * k);
                }
            }
            IqBuffer::Cs16(b) => {
                for (o, s) in out.iter_mut().zip(&b[..n]) {
                    *o = Complex::new(s.re as f32 * k, s.im as f32 * k);
                }
            }
            IqBuffer::Cf32(b) => {
                for (o, s) in out.iter_mut().zip(&b[..n]) {
                    *o = *s * k;
                }
            }
        }
    }

    pub fn format(&self) -> SampleFormat {
        match self {
            IqBuffer::Cu8(_) => SampleFormat::Cu8,
            IqBuffer::Cs8(_) => SampleFormat::Cs8,
            IqBuffer::Cs16(_) => SampleFormat::Cs16,
            IqBuffer::Cf32(_) => SampleFormat::Cf32,
        }
    }
}

/// Result of a single stream read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// This many samples were read (from the start of the buffer).
    Samples(usize),
    /// Stream overflow (samples lost).
    Overflow,
    /// No data within the given time.
    Timeout,
}

#[derive(Debug, thiserror::Error)]
pub enum SdrError {
    /// An error returned by SoapySDR or the driver, passed on in full.
    #[error("{op}: {message}")]
    Driver { op: String, message: String },
    /// The requested parameter is outside the capabilities reported by the device.
    #[error("{0}")]
    Unsupported(String),
}

/// IQ sample source. Values returned by `set_*` are the values actually accepted by the hardware.
pub trait SdrSource {
    /// Sample rates reported by the device (Hz, ascending).
    fn sample_rates(&self) -> &[f64];
    fn set_sample_rate(&mut self, hz: f64) -> Result<f64, SdrError>;
    fn set_frequency(&mut self, hz: f64) -> Result<f64, SdrError>;
    /// Fast retune for the hot loop: no validation and no read-back of the actual value. The
    /// frequency must have been checked with `set_frequency` before.
    fn tune(&mut self, hz: f64) -> Result<(), SdrError>;
    /// Applies the receiver configuration (gain, AGC, antenna, driver settings) in the order
    /// given. rfscan has no defaults or ranges of its own: values go to the driver unchanged and
    /// its error is returned in full. The only rule: a manual stage gain disables AGC, unless the
    /// configuration explicitly contains `agc`.
    fn configure(&mut self, config: &[(String, String)]) -> Result<(), SdrError>;
    /// Frequency correction in ppm, done by the driver (`--ppm`); an error when the driver does
    /// not support it. Returns the value read back.
    fn set_frequency_correction(&mut self, ppm: f64) -> Result<f64, SdrError>;
    /// Current receiver configuration read back from the device (for printing).
    fn config_summary(&self) -> String;
    /// Stream format and full scale (the value corresponding to amplitude 1.0).
    fn stream_format(&self) -> (SampleFormat, f64);
    /// Recommended read buffer size (in samples), chosen from the stream MTU.
    fn buffer_len(&self) -> usize;
    /// Activates the stream (once).
    fn start(&mut self) -> Result<(), SdrError>;
    /// Reads into the caller's buffer, at most `max` samples, without allocation.
    fn read(
        &mut self,
        buf: &mut IqBuffer,
        max: usize,
        timeout_us: i64,
    ) -> Result<ReadOutcome, SdrError>;
}
