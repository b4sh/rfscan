//! `SdrSource` implementation through SoapySDR, and `--list-devices`.
//!
//! Most calls go through the `soapysdr` crate. Functions that this crate does not expose
//! (SoapySDR versions, paths and modules, `listSampleRates`, `getSettingInfo`,
//! `hasFrequencyCorrection`) are called through `soapysdr-sys` in the `raw` module.

use std::io::Write;

use num_complex::Complex;
use soapysdr::{Device, Direction, ErrorCode, Format, Range, RxStream};

use super::{IqBuffer, ReadOutcome, SampleFormat, SdrError, SdrSource};

const RX: Direction = Direction::Rx;

fn driver_err(op: &str, e: soapysdr::Error) -> SdrError {
    SdrError::Driver {
        op: op.to_string(),
        message: e.to_string(),
    }
}

/// Minimal, safe wrappers of SoapySDR C API functions missing from the `soapysdr` crate.
mod raw {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::ptr::NonNull;
    use std::slice;

    use soapysdr_sys as sys;

    use crate::device::SdrError;

    /// Copies a C string managed by SoapySDR (not freed by the caller).
    fn borrowed_str(p: *const c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: SoapySDR returns a pointer to a static, null-terminated string, valid for the
        // lifetime of the library. We checked that it is not null.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }

    /// Converts a string list allocated by SoapySDR into a `Vec<String>` and frees it.
    fn take_strings(mut ptr: *mut *mut c_char, len: usize) -> Vec<String> {
        if ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: SoapySDR returned an array of `len` null-terminated strings. We copy them and
        // then free the whole list with the function SoapySDR provides for that.
        unsafe {
            let out = slice::from_raw_parts(ptr, len)
                .iter()
                .map(|&s| borrowed_str(s))
                .collect();
            sys::SoapySDRStrings_clear(&mut ptr, len);
            out
        }
    }

    pub fn api_version() -> String {
        // SAFETY: a function without arguments, returns a static string.
        borrowed_str(unsafe { sys::SoapySDR_getAPIVersion() })
    }

    pub fn abi_version() -> String {
        // SAFETY: as above.
        borrowed_str(unsafe { sys::SoapySDR_getABIVersion() })
    }

    pub fn lib_version() -> String {
        // SAFETY: as above.
        borrowed_str(unsafe { sys::SoapySDR_getLibVersion() })
    }

    pub fn root_path() -> String {
        // SAFETY: as above.
        borrowed_str(unsafe { sys::SoapySDR_getRootPath() })
    }

    pub fn search_paths() -> Vec<String> {
        let mut len = 0usize;
        // SAFETY: `len` is a valid output pointer, the result is freed by `take_strings`.
        let ptr = unsafe { sys::SoapySDR_listSearchPaths(&mut len) };
        take_strings(ptr, len)
    }

    pub fn modules() -> Vec<String> {
        let mut len = 0usize;
        // SAFETY: as above.
        let ptr = unsafe { sys::SoapySDR_listModules(&mut len) };
        take_strings(ptr, len)
    }

    pub fn module_version(path: &str) -> Option<String> {
        let c = CString::new(path).ok()?;
        // SAFETY: `c` is a valid C string. The result is allocated by SoapySDR, we copy it and
        // free it with `SoapySDR_free`.
        unsafe {
            let p = sys::SoapySDR_getModuleVersion(c.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = borrowed_str(p);
            sys::SoapySDR_free(p as *mut c_void);
            if s.is_empty() { None } else { Some(s) }
        }
    }

    /// Module load result: pairs (registration name, error message; empty = OK).
    pub fn loader_result(path: &str) -> Vec<(String, String)> {
        let Ok(c) = CString::new(path) else {
            return Vec::new();
        };
        // SAFETY: `c` is a valid C string. The returned kwargs structure is owned by the caller:
        // we copy the pairs and free it with `SoapySDRKwargs_clear`.
        unsafe {
            let mut kw = sys::SoapySDR_getLoaderResult(c.as_ptr());
            let mut out = Vec::with_capacity(kw.size);
            if !kw.keys.is_null() && !kw.vals.is_null() {
                let keys = slice::from_raw_parts(kw.keys, kw.size);
                let vals = slice::from_raw_parts(kw.vals, kw.size);
                for (&k, &v) in keys.iter().zip(vals) {
                    out.push((borrowed_str(k), borrowed_str(v)));
                }
            }
            sys::SoapySDRKwargs_clear(&mut kw);
            out
        }
    }

    fn last_status() -> Result<(), SdrError> {
        // SAFETY: reads the status of the last call in the current thread (SoapySDR thread-local
        // storage).
        let status = unsafe { sys::SoapySDRDevice_lastStatus() };
        if status == 0 {
            Ok(())
        } else {
            Err(SdrError::Driver {
                op: "SoapySDR".into(),
                // SAFETY: the error string of the last call, valid until the next call.
                message: borrowed_str(unsafe { sys::SoapySDRDevice_lastError() }),
            })
        }
    }

    /// Driver setting description (`getSettingInfo`).
    pub struct SettingInfo {
        pub key: String,
        pub default: String,
        pub kind: &'static str,
        pub description: String,
        pub options: Vec<String>,
    }

    /// A second C handle to the device opened by `soapysdr::Device`.
    ///
    /// `SoapySDRDevice_make` keeps a table of open devices with a reference count: a call with
    /// the same arguments returns the same device object without opening the hardware again,
    /// and `unmake` only decrements the counter.
    pub struct RawDevice(NonNull<sys::SoapySDRDevice>);

    impl RawDevice {
        pub fn make(args: &soapysdr::Args) -> Result<Self, SdrError> {
            // SAFETY: `args` is a valid kwargs structure for the duration of the call.
            let p = unsafe { sys::SoapySDRDevice_make(args.as_raw_const()) };
            match NonNull::new(p) {
                Some(p) => Ok(RawDevice(p)),
                None => Err(last_status().err().unwrap_or(SdrError::Driver {
                    op: "SoapySDRDevice_make".into(),
                    message: "null device handle".into(),
                })),
            }
        }

        const RX: c_int = sys::SOAPY_SDR_RX as c_int;

        pub fn list_sample_rates(&self, channel: usize) -> Result<Vec<f64>, SdrError> {
            let mut len = 0usize;
            // SAFETY: the handle is valid (NonNull, released only in Drop). The result is an
            // array of `len` f64 values allocated by SoapySDR: we copy it and free it.
            unsafe {
                let p = sys::SoapySDRDevice_listSampleRates(
                    self.0.as_ptr(),
                    Self::RX,
                    channel,
                    &mut len,
                );
                last_status()?;
                if p.is_null() {
                    return Ok(Vec::new());
                }
                let out = slice::from_raw_parts(p, len).to_vec();
                sys::SoapySDR_free(p as *mut c_void);
                Ok(out)
            }
        }

        pub fn has_frequency_correction(&self, channel: usize) -> Result<bool, SdrError> {
            // SAFETY: the handle is valid.
            let r = unsafe {
                sys::SoapySDRDevice_hasFrequencyCorrection(self.0.as_ptr(), Self::RX, channel)
            };
            last_status()?;
            Ok(r)
        }

        /// Sets the frequency correction in ppm (`setFrequencyCorrection`) and returns the value
        /// read back from the driver.
        pub fn set_frequency_correction(&self, channel: usize, ppm: f64) -> Result<f64, SdrError> {
            // SAFETY: the handle is valid.
            let r = unsafe {
                sys::SoapySDRDevice_setFrequencyCorrection(self.0.as_ptr(), Self::RX, channel, ppm)
            };
            if r != 0 {
                return Err(SdrError::Driver {
                    op: "setFrequencyCorrection".into(),
                    // SAFETY: the error string of the last call, valid until the next call.
                    message: borrowed_str(unsafe { sys::SoapySDRDevice_lastError() }),
                });
            }
            // SAFETY: the handle is valid.
            let actual = unsafe {
                sys::SoapySDRDevice_getFrequencyCorrection(self.0.as_ptr(), Self::RX, channel)
            };
            last_status()?;
            Ok(actual)
        }

        pub fn setting_info(&self) -> Result<Vec<SettingInfo>, SdrError> {
            let mut len = 0usize;
            // SAFETY: the handle is valid; `arg_info` takes over the returned array.
            unsafe {
                let p = sys::SoapySDRDevice_getSettingInfo(self.0.as_ptr(), &mut len);
                Self::arg_info(p, len)
            }
        }

        /// Stream arguments known to the driver (`getStreamArgsInfo`, `--stream-args`).
        pub fn stream_args_info(&self, channel: usize) -> Result<Vec<SettingInfo>, SdrError> {
            let mut len = 0usize;
            // SAFETY: the handle is valid; `arg_info` takes over the returned array.
            unsafe {
                let p = sys::SoapySDRDevice_getStreamArgsInfo(
                    self.0.as_ptr(),
                    Self::RX,
                    channel,
                    &mut len,
                );
                Self::arg_info(p, len)
            }
        }

        /// Copies an ArgInfo list returned by SoapySDR and frees it.
        ///
        /// # Safety
        /// `p` is null or an array of `len` ArgInfo structures allocated by SoapySDR, returned
        /// by the call just made (its status is checked here); it must not be used afterwards.
        unsafe fn arg_info(
            p: *mut sys::SoapySDRArgInfo,
            len: usize,
        ) -> Result<Vec<SettingInfo>, SdrError> {
            // SAFETY: guaranteed by the caller: we copy the fields and free the array with
            // `SoapySDRArgInfoList_clear`.
            unsafe {
                last_status()?;
                if p.is_null() {
                    return Ok(Vec::new());
                }
                let out = slice::from_raw_parts(p, len)
                    .iter()
                    .map(|a| SettingInfo {
                        key: borrowed_str(a.key),
                        default: borrowed_str(a.value),
                        kind: match a.type_ {
                            sys::SOAPY_SDR_ARG_INFO_BOOL => "bool",
                            sys::SOAPY_SDR_ARG_INFO_INT => "int",
                            sys::SOAPY_SDR_ARG_INFO_FLOAT => "float",
                            sys::SOAPY_SDR_ARG_INFO_STRING => "string",
                            _ => "?",
                        },
                        description: borrowed_str(a.description),
                        options: if a.options.is_null() {
                            Vec::new()
                        } else {
                            slice::from_raw_parts(a.options, a.numOptions)
                                .iter()
                                .map(|&o| borrowed_str(o))
                                .collect()
                        },
                    })
                    .collect();
                sys::SoapySDRArgInfoList_clear(p, len);
                Ok(out)
            }
        }
    }

    impl Drop for RawDevice {
        fn drop(&mut self) {
            // SAFETY: the handle comes from `SoapySDRDevice_make` and is released exactly once.
            unsafe {
                sys::SoapySDRDevice_unmake(self.0.as_ptr());
            }
        }
    }
}

/// RX stream in one of the supported formats.
enum RxKind {
    Cu8(RxStream<Complex<u8>>),
    Cs8(RxStream<Complex<i8>>),
    Cs16(RxStream<Complex<i16>>),
    Cf32(RxStream<Complex<f32>>),
}

impl RxKind {
    /// `args`: stream arguments for the driver (`--stream-args`), passed unchanged.
    fn setup(
        dev: &Device,
        channel: usize,
        format: SampleFormat,
        args: &[(String, String)],
    ) -> Result<Self, SdrError> {
        let ch = [channel];
        let mut a = soapysdr::Args::new();
        for (k, v) in args {
            a.set(k.as_str(), v.as_str());
        }
        let r = match format {
            SampleFormat::Cu8 => dev.rx_stream_args(&ch, a).map(RxKind::Cu8),
            SampleFormat::Cs8 => dev.rx_stream_args(&ch, a).map(RxKind::Cs8),
            SampleFormat::Cs16 => dev.rx_stream_args(&ch, a).map(RxKind::Cs16),
            SampleFormat::Cf32 => dev.rx_stream_args(&ch, a).map(RxKind::Cf32),
        };
        r.map_err(|e| driver_err("setupStream", e))
    }

    fn mtu(&self) -> Result<usize, SdrError> {
        match self {
            RxKind::Cu8(s) => s.mtu(),
            RxKind::Cs8(s) => s.mtu(),
            RxKind::Cs16(s) => s.mtu(),
            RxKind::Cf32(s) => s.mtu(),
        }
        .map_err(|e| driver_err("getStreamMTU", e))
    }
}

/// Maps the device's native format to a format supported by the DSP.
fn supported_format(f: Format) -> Option<SampleFormat> {
    match f {
        Format::CU8 => Some(SampleFormat::Cu8),
        Format::CS8 => Some(SampleFormat::Cs8),
        Format::CS16 => Some(SampleFormat::Cs16),
        Format::CF32 => Some(SampleFormat::Cf32),
        _ => None,
    }
}

fn in_ranges(ranges: &[Range], v: f64) -> bool {
    ranges.iter().any(|r| v >= r.minimum && v <= r.maximum)
}

fn fmt_ranges(ranges: &[Range], scale: f64, unit: &str) -> String {
    if ranges.is_empty() {
        return "none".into();
    }
    ranges
        .iter()
        .map(|r| {
            if r.minimum == r.maximum {
                format!("{} {unit}", r.minimum / scale)
            } else if r.step > 0.0 {
                format!(
                    "{}-{} {unit} (step {})",
                    r.minimum / scale,
                    r.maximum / scale,
                    r.step / scale
                )
            } else {
                format!("{}-{} {unit}", r.minimum / scale, r.maximum / scale)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn fmt_rates(rates: &[f64]) -> String {
    rates
        .iter()
        .map(|r| format!("{}", r / 1e6))
        .collect::<Vec<_>>()
        .join(", ")
        + " MSps"
}

/// SoapySDR drivers that are not radio receivers (a sound card through SoapyAudio).
const NOT_A_RECEIVER: &[&str] = &["audio"];

fn is_receiver(args: &soapysdr::Args) -> bool {
    !args
        .get("driver")
        .is_some_and(|d| NOT_A_RECEIVER.contains(&d))
}

/// Opens the first free receiver matching the arguments: devices that are not receivers are
/// skipped, and a device that cannot be opened (e.g. used by another program) gives way to the
/// next one. Returns the full arguments from the enumeration and the open device.
fn open_free_device(args: &str) -> Result<(soapysdr::Args, Device), SdrError> {
    let found = soapysdr::enumerate(args).map_err(|e| driver_err("enumerate", e))?;
    let mut failed = Vec::new();
    for a in found.into_iter().filter(is_receiver) {
        match Device::new(clone_args(&a)) {
            Ok(dev) => return Ok((a, dev)),
            Err(e) => failed.push(format!("  {a}: {e}")),
        }
    }
    let filter = if args.is_empty() {
        String::new()
    } else {
        format!(" matching '{args}'")
    };
    let mut msg = format!(
        "no free SDR receiver{filter} found: connect a receiver or close the program that uses it, see --list-devices"
    );
    if !failed.is_empty() {
        msg.push_str("\nreceivers that could not be opened:\n");
        msg.push_str(&failed.join("\n"));
    }
    Err(SdrError::Unsupported(msg))
}

/// A copy of `Args` (the crate does not implement `Clone`).
fn clone_args(a: &soapysdr::Args) -> soapysdr::Args {
    let mut out = soapysdr::Args::new();
    for (k, v) in a {
        out.set(k, v);
    }
    out
}

pub struct SoapySource {
    dev: Device,
    /// The device arguments, for calls outside the crate (`raw`).
    args: soapysdr::Args,
    channel: usize,
    description: String,
    rates: Vec<f64>,
    freq_ranges: Vec<Range>,
    native: String,
    format: SampleFormat,
    full_scale: f64,
    stream: RxKind,
    mtu: usize,
    sample_rate: f64,
    /// Gain stages (`listGains`).
    gains: Vec<String>,
    /// Driver setting keys (`getSettingInfo`).
    settings: Vec<String>,
    /// Stream argument keys of the driver (`getStreamArgsInfo`).
    stream_arg_keys: Vec<String>,
}

impl SoapySource {
    /// `stream_args`: arguments for the driver's stream (`--stream-args`); keys the driver does
    /// not list in `getStreamArgsInfo` are an error, because SoapySDR ignores them silently.
    pub fn open(
        device_args: &str,
        channel: usize,
        stream_args: &[(String, String)],
    ) -> Result<Self, SdrError> {
        let (args, dev) = open_free_device(device_args)?;
        let description = args.to_string();

        let channels = dev
            .num_channels(RX)
            .map_err(|e| driver_err("getNumChannels", e))?;
        if channel >= channels {
            return Err(SdrError::Unsupported(format!(
                "RX channel {channel} does not exist, the device has {channels} RX channel(s)"
            )));
        }

        // The same device object (SoapySDR table), only for queries outside the crate.
        let raw = raw::RawDevice::make(&args)?;
        let mut rates = raw.list_sample_rates(channel)?;
        let settings = raw.setting_info()?.into_iter().map(|s| s.key).collect();
        let known: Vec<String> = raw
            .stream_args_info(channel)?
            .into_iter()
            .map(|s| s.key)
            .collect();
        if let Some((k, _)) = stream_args.iter().find(|(k, _)| !known.contains(k)) {
            return Err(SdrError::Unsupported(format!(
                "--stream-args: '{k}' is not a stream argument of this driver; known: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )));
        }
        drop(raw);
        let gains = dev
            .list_gains(RX, channel)
            .map_err(|e| driver_err("listGains", e))?;
        rates.sort_by(f64::total_cmp);
        rates.dedup();

        let freq_ranges = dev
            .frequency_range(RX, channel)
            .map_err(|e| driver_err("getFrequencyRange", e))?;

        let (native, full_scale_native) = match dev.native_stream_format(RX, channel) {
            Ok((f, fs)) => (Some(f), fs),
            Err(_) => (None, 0.0),
        };
        let native_name = native.map_or_else(|| "unknown".to_string(), |f| f.to_string());
        let (format, full_scale) = match native.and_then(supported_format) {
            Some(f) => (f, full_scale_native),
            // CF32 has a full scale of 1.0 by definition.
            None => (SampleFormat::Cf32, 1.0),
        };
        if full_scale <= 0.0 {
            return Err(SdrError::Unsupported(format!(
                "the device reported an invalid full scale {full_scale} for format {native_name}"
            )));
        }

        let stream = RxKind::setup(&dev, channel, format, stream_args)?;
        let mtu = stream.mtu()?;
        let sample_rate = dev
            .sample_rate(RX, channel)
            .map_err(|e| driver_err("getSampleRate", e))?;

        Ok(SoapySource {
            dev,
            args,
            channel,
            description,
            rates,
            freq_ranges,
            native: native_name,
            format,
            full_scale,
            stream,
            mtu,
            sample_rate,
            gains,
            settings,
            stream_arg_keys: known,
        })
    }

    /// Stream argument keys of the driver (`--stream-args`).
    pub fn stream_arg_keys(&self) -> &[String] {
        &self.stream_arg_keys
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    /// Native format reported by the device (Soapy name).
    pub fn native_format_name(&self) -> &str {
        &self.native
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    /// Sets one configuration key (see `SdrSource::configure`).
    fn configure_one(&mut self, key: &str, value: &str) -> Result<(), SdrError> {
        let ch = self.channel;
        let number = || {
            value.trim().parse::<f64>().map_err(|_| {
                SdrError::Unsupported(format!("{key}={value}: the value is not a number"))
            })
        };

        if let Some(name) = self.gains.iter().find(|g| g.eq_ignore_ascii_case(key)) {
            return self
                .dev
                .set_gain_element(RX, ch, name.as_str(), number()?)
                .map_err(|e| driver_err(&format!("setGain({name})"), e));
        }
        if let Some(name) = self.settings.iter().find(|s| s.as_str() == key) {
            return self
                .dev
                .write_setting(name.as_str(), value)
                .map_err(|e| driver_err(&format!("writeSetting({name})"), e));
        }
        match key {
            "agc" => {
                let on = match value.trim() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => {
                        return Err(SdrError::Unsupported(format!(
                            "agc={value}: expected true or false"
                        )));
                    }
                };
                let has_agc = self
                    .dev
                    .has_gain_mode(RX, ch)
                    .map_err(|e| driver_err("hasGainMode", e))?;
                if !has_agc {
                    return Err(SdrError::Unsupported(
                        "the device does not support hardware AGC (agc)".into(),
                    ));
                }
                self.dev
                    .set_gain_mode(RX, ch, on)
                    .map_err(|e| driver_err("setGainMode", e))
            }
            "antenna" => self
                .dev
                .set_antenna(RX, ch, value)
                .map_err(|e| driver_err("setAntenna", e)),
            "bandwidth" => self
                .dev
                .set_bandwidth(RX, ch, number()?)
                .map_err(|e| driver_err("setBandwidth", e)),
            _ => Err(SdrError::Unsupported(format!(
                "unknown key '{key}'; available: gain stages [{}], driver settings [{}], and agc, antenna, bandwidth",
                self.gains.join(", "),
                self.settings.join(", ")
            ))),
        }
    }

    fn is_gain_stage(&self, key: &str) -> bool {
        self.gains.iter().any(|g| g.eq_ignore_ascii_case(key))
    }
}

impl SdrSource for SoapySource {
    fn sample_rates(&self) -> &[f64] {
        &self.rates
    }

    fn set_sample_rate(&mut self, hz: f64) -> Result<f64, SdrError> {
        // A rate from the list or from the continuous ranges (e.g. an RTL-SDR lists 2.56 MSps but
        // accepts 2.4 MSps from its 0.9-3.2 MSps range).
        let listed = self.rates.iter().any(|&r| (r - hz).abs() < 0.5);
        if !listed {
            let ranges = self
                .dev
                .get_sample_rate_range(RX, self.channel)
                .map_err(|e| driver_err("getSampleRateRange", e))?;
            if !in_ranges(&ranges, hz) {
                let mut available = Vec::new();
                if !self.rates.is_empty() {
                    available.push(fmt_rates(&self.rates));
                }
                if !ranges.is_empty() {
                    available.push(format!("ranges {}", fmt_ranges(&ranges, 1e6, "MSps")));
                }
                return Err(SdrError::Unsupported(format!(
                    "sample rate {} MSps is not supported; available: {}",
                    hz / 1e6,
                    available.join("; ")
                )));
            }
        }
        self.dev
            .set_sample_rate(RX, self.channel, hz)
            .map_err(|e| driver_err("setSampleRate", e))?;
        self.sample_rate = self
            .dev
            .sample_rate(RX, self.channel)
            .map_err(|e| driver_err("getSampleRate", e))?;
        Ok(self.sample_rate)
    }

    fn set_frequency(&mut self, hz: f64) -> Result<f64, SdrError> {
        if !in_ranges(&self.freq_ranges, hz) {
            return Err(SdrError::Unsupported(format!(
                "frequency {} MHz is outside the device range: {}",
                hz / 1e6,
                fmt_ranges(&self.freq_ranges, 1e6, "MHz")
            )));
        }
        self.dev
            .set_frequency(RX, self.channel, hz, ())
            .map_err(|e| driver_err("setFrequency", e))?;
        self.dev
            .frequency(RX, self.channel)
            .map_err(|e| driver_err("getFrequency", e))
    }

    fn tune(&mut self, hz: f64) -> Result<(), SdrError> {
        self.dev
            .set_frequency(RX, self.channel, hz, ())
            .map_err(|e| driver_err("setFrequency", e))
    }

    fn configure(&mut self, config: &[(String, String)]) -> Result<(), SdrError> {
        // Manual stage gains require AGC to be off (in Airspy: LNA AGC and Mixer AGC), otherwise
        // the AGC overrides the given values. An explicit `agc=...` from the user takes
        // precedence, e.g. `agc=true,VGA=10` (AGC controls LNA and MIX, VGA manual).
        let manual_gain = config.iter().any(|(k, _)| self.is_gain_stage(k));
        let explicit_agc = config.iter().any(|(k, _)| k == "agc");
        if manual_gain && !explicit_agc {
            let has_agc = self
                .dev
                .has_gain_mode(RX, self.channel)
                .map_err(|e| driver_err("hasGainMode", e))?;
            if has_agc {
                self.dev
                    .set_gain_mode(RX, self.channel, false)
                    .map_err(|e| driver_err("setGainMode", e))?;
            }
        }
        for (k, v) in config {
            self.configure_one(k, v)?;
        }
        Ok(())
    }

    fn set_frequency_correction(&mut self, ppm: f64) -> Result<f64, SdrError> {
        // The same device object (SoapySDR table): the crate has no frequency correction call.
        let raw = raw::RawDevice::make(&self.args)?;
        if !raw.has_frequency_correction(self.channel)? {
            return Err(SdrError::Unsupported(
                "this device (its SoapySDR driver) does not support frequency correction".into(),
            ));
        }
        raw.set_frequency_correction(self.channel, ppm)
    }

    fn config_summary(&self) -> String {
        let ch = self.channel;
        let mut parts: Vec<String> = self
            .gains
            .iter()
            .map(|g| match self.dev.gain_element(RX, ch, g.as_str()) {
                Ok(v) => format!("{g}={v}"),
                Err(e) => format!("{g}=? ({e})"),
            })
            .collect();
        if let Ok(total) = self.dev.gain(RX, ch) {
            parts.push(format!("total {total} dB"));
        }
        if self.dev.has_gain_mode(RX, ch).unwrap_or(false) {
            match self.dev.gain_mode(RX, ch) {
                Ok(on) => parts.push(format!("agc={on}")),
                Err(e) => parts.push(format!("agc=? ({e})")),
            }
        }
        if let Ok(a) = self.dev.antenna(RX, ch) {
            parts.push(format!("antenna={a}"));
        }
        for key in &self.settings {
            if let Ok(v) = self.dev.read_setting(key.as_str()) {
                parts.push(format!("{key}={v}"));
            }
        }
        parts.join(", ")
    }

    fn stream_format(&self) -> (SampleFormat, f64) {
        (self.format, self.full_scale)
    }

    fn buffer_len(&self) -> usize {
        if self.mtu > 0 {
            self.mtu
        } else {
            // The driver did not report an MTU: a buffer for about 10 ms of samples.
            ((self.sample_rate / 100.0) as usize).max(1)
        }
    }

    fn start(&mut self) -> Result<(), SdrError> {
        match &mut self.stream {
            RxKind::Cu8(s) => s.activate(None),
            RxKind::Cs8(s) => s.activate(None),
            RxKind::Cs16(s) => s.activate(None),
            RxKind::Cf32(s) => s.activate(None),
        }
        .map_err(|e| driver_err("activateStream", e))
    }

    fn read(
        &mut self,
        buf: &mut IqBuffer,
        max: usize,
        timeout_us: i64,
    ) -> Result<ReadOutcome, SdrError> {
        let r = match (&mut self.stream, buf) {
            (RxKind::Cu8(s), IqBuffer::Cu8(b)) => {
                let n = max.min(b.len());
                s.read(&mut [&mut b[..n]], timeout_us)
            }
            (RxKind::Cs8(s), IqBuffer::Cs8(b)) => {
                let n = max.min(b.len());
                s.read(&mut [&mut b[..n]], timeout_us)
            }
            (RxKind::Cs16(s), IqBuffer::Cs16(b)) => {
                let n = max.min(b.len());
                s.read(&mut [&mut b[..n]], timeout_us)
            }
            (RxKind::Cf32(s), IqBuffer::Cf32(b)) => {
                let n = max.min(b.len());
                s.read(&mut [&mut b[..n]], timeout_us)
            }
            (_, b) => {
                return Err(SdrError::Unsupported(format!(
                    "buffer in format {} does not match stream {}",
                    b.format().name(),
                    self.format.name()
                )));
            }
        };
        match r {
            Ok(n) => Ok(ReadOutcome::Samples(n)),
            Err(e) if e.code == ErrorCode::Overflow => Ok(ReadOutcome::Overflow),
            Err(e) if e.code == ErrorCode::Timeout => Ok(ReadOutcome::Timeout),
            Err(e) => Err(driver_err("readStream", e)),
        }
    }
}

impl Drop for SoapySource {
    fn drop(&mut self) {
        // An error on close cannot affect anything anymore, and the stream's Drop releases it
        // anyway.
        let _ = match &mut self.stream {
            RxKind::Cu8(s) => s.deactivate(None),
            RxKind::Cs8(s) => s.deactivate(None),
            RxKind::Cs16(s) => s.deactivate(None),
            RxKind::Cf32(s) => s.deactivate(None),
        };
    }
}

/// `--list-devices`: SoapySDR versions and modules, then the capabilities of each device.
pub fn list_devices(out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "SoapySDR")?;
    writeln!(
        out,
        "  API version: {}, ABI: {}, library: {}",
        raw::api_version(),
        raw::abi_version(),
        raw::lib_version()
    )?;
    writeln!(out, "  root directory: {}", raw::root_path())?;
    writeln!(out, "  module paths:")?;
    for p in raw::search_paths() {
        writeln!(out, "    {p}")?;
    }
    let modules = raw::modules();
    writeln!(out, "  modules ({}):", modules.len())?;
    for m in &modules {
        match raw::module_version(m) {
            Some(v) => writeln!(out, "    {m} (version {v})")?,
            None => writeln!(out, "    {m}")?,
        }
    }
    out.flush()?;

    // The enumeration loads the modules, so load results are available only after it.
    let found = soapysdr::enumerate("");

    writeln!(out, "  module load results:")?;
    for m in &modules {
        let res = raw::loader_result(m);
        if res.is_empty() {
            writeln!(out, "    {m}: no entries")?;
        }
        for (key, err) in res {
            if err.is_empty() {
                writeln!(out, "    {m}: {key} OK")?;
            } else {
                writeln!(out, "    {m}: {key} ERROR: {err}")?;
            }
        }
    }

    let found = match found {
        Ok(f) => f,
        Err(e) => {
            writeln!(out, "\nDevice enumeration error: {e}")?;
            return Ok(());
        }
    };
    writeln!(out, "\nDevices: {}", found.len())?;
    for (i, args) in found.iter().enumerate() {
        writeln!(out, "\n[{i}] {args}")?;
        if !is_receiver(args) {
            writeln!(out, "    not a radio receiver, rfscan does not use it")?;
            continue;
        }
        if let Err(e) = describe_device(out, args) {
            writeln!(out, "    error: {e}")?;
        }
    }
    Ok(())
}

/// Prints the capabilities of one device. An error of a single query does not stop the list.
fn describe_device(out: &mut impl Write, args: &soapysdr::Args) -> Result<(), SdrError> {
    let io = |e: std::io::Error| SdrError::Driver {
        op: "write".into(),
        message: e.to_string(),
    };
    let dev = Device::new(clone_args(args)).map_err(|e| driver_err("make", e))?;
    let raw = raw::RawDevice::make(args)?;

    macro_rules! line {
        ($($t:tt)*) => { writeln!(out, $($t)*).map_err(io)? };
    }
    fn show<T>(r: Result<T, impl std::fmt::Display>, f: impl FnOnce(T) -> String) -> String {
        match r {
            Ok(v) => f(v),
            Err(e) => format!("error: {e}"),
        }
    }

    line!(
        "    driver: {}, hardware: {}",
        show(dev.driver_key(), |s| s),
        show(dev.hardware_key(), |s| s)
    );
    line!(
        "    hardware info: {}",
        show(dev.hardware_info(), |a| a.to_string())
    );
    let channels = dev
        .num_channels(RX)
        .map_err(|e| driver_err("getNumChannels", e))?;
    line!("    RX channels: {channels}");

    for ch in 0..channels {
        line!("    RX channel {ch}:");
        line!(
            "      antennas: {}",
            show(dev.antennas(RX, ch), |v| v.join(", "))
        );
        line!(
            "      frequency: {}",
            show(dev.frequency_range(RX, ch), |r| fmt_ranges(&r, 1e6, "MHz"))
        );
        line!(
            "      sample rates (listSampleRates): {}",
            show(raw.list_sample_rates(ch), |v| fmt_rates(&v))
        );
        line!(
            "      sample rates (getSampleRateRange): {}",
            show(dev.get_sample_rate_range(RX, ch), |r| fmt_ranges(
                &r, 1e6, "MSps"
            ))
        );
        line!(
            "      analog bandwidth: {}",
            show(dev.bandwidth_range(RX, ch), |r| fmt_ranges(&r, 1e6, "MHz"))
        );
        line!(
            "      total gain: {}",
            show(dev.gain_range(RX, ch), |r| fmt_ranges(&[r], 1.0, "dB"))
        );
        match dev.list_gains(RX, ch) {
            Ok(names) => {
                for name in names {
                    let r = dev.gain_element_range(RX, ch, name.as_str());
                    line!(
                        "        stage {name}: {}",
                        show(r, |r| fmt_ranges(&[r], 1.0, "dB"))
                    );
                }
            }
            Err(e) => line!("        gain stages: error: {e}"),
        }
        let yes_no = |b: bool| if b { "yes" } else { "no" }.to_string();
        line!(
            "      hardware AGC: {}",
            show(dev.has_gain_mode(RX, ch), yes_no)
        );
        line!(
            "      automatic DC correction: {}",
            show(dev.has_dc_offset_mode(RX, ch), yes_no)
        );
        line!(
            "      frequency correction (ppm): {}",
            show(raw.has_frequency_correction(ch), yes_no)
        );
        line!(
            "      stream formats: {}",
            show(dev.stream_formats(RX, ch), |v| v
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(", "))
        );
        match dev.native_stream_format(RX, ch) {
            Ok((f, fs)) => {
                let note = if supported_format(f).is_some() {
                    ""
                } else {
                    " (not supported by rfscan, CF32 will be used)"
                };
                line!("      native format: {f}, full scale {fs}{note}");
                let fmt = supported_format(f).unwrap_or(SampleFormat::Cf32);
                line!(
                    "      stream MTU: {}",
                    show(
                        RxKind::setup(&dev, ch, fmt, &[]).and_then(|s| s.mtu()),
                        |m| { format!("{m} samples") }
                    )
                );
            }
            Err(e) => line!("      native format: error: {e}"),
        }
    }

    match raw.setting_info() {
        Ok(settings) if settings.is_empty() => line!("    driver settings: none"),
        Ok(settings) => {
            line!("    driver settings:");
            for s in settings {
                let opts = if s.options.is_empty() {
                    String::new()
                } else {
                    format!(", options: {}", s.options.join("|"))
                };
                line!(
                    "      {} ({}, default '{}'{opts}) {}",
                    s.key,
                    s.kind,
                    s.default,
                    s.description
                );
            }
        }
        Err(e) => line!("    driver settings: error: {e}"),
    }
    match raw.stream_args_info(0) {
        Ok(args) if args.is_empty() => line!("    stream arguments (--stream-args): none"),
        Ok(args) => {
            line!("    stream arguments (--stream-args):");
            for s in args {
                line!(
                    "      {} ({}, default '{}') {}",
                    s.key,
                    s.kind,
                    s.default,
                    s.description
                );
            }
        }
        Err(e) => line!("    stream arguments: error: {e}"),
    }
    Ok(())
}
