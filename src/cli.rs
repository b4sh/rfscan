//! Command line arguments.

use clap::Parser;

use crate::plan::Modulation;

/// Soapy-style key=value pairs, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub struct Kwargs(pub Vec<(String, String)>);

/// Section headings in `--help`.
const H_SDR: &str = "SDR hardware";
const H_RADIO: &str = "Radio parameters";
const H_SCAN: &str = "Scan parameters";
const H_AUDIO: &str = "Audio parameters";
const H_OTHER: &str = "Other";

#[derive(Parser, Debug)]
#[command(
    name = "rfscan",
    version,
    about = "Fast radio scanner (SoapySDR)",
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Cli {
    // SDR hardware
    /// List devices seen by SoapySDR (with module diagnostics) and exit
    #[arg(long, help_heading = H_SDR)]
    pub list_devices: bool,

    /// Receiver in Soapy format, e.g. driver=airspy
    ///
    /// Device arguments in Soapy format (e.g. driver=airspy, serial=...).
    /// Default: the first one found
    #[arg(long, value_name = "ARGS", default_value = "", help_heading = H_SDR)]
    pub device: String,

    /// Device RX channel number
    #[arg(long, value_name = "N", hide_short_help = true, default_value_t = 0, help_heading = H_SDR)]
    pub channel: usize,

    /// Sample rate in Hz (suffixes k, M). Default: the highest reported by the device
    #[arg(long, value_name = "HZ", value_parser = parse_hz, help_heading = H_SDR)]
    pub sample_rate: Option<u64>,

    /// Receiver settings, e.g. "agc=true,VGA=7" or "LNA=9,MIX=10,VGA=7"
    ///
    /// Receiver configuration in Soapy syntax "key=value,...", applied in the order given.
    /// Keys: gain stages (e.g. LNA=9,MIX=10,VGA=7), driver settings (e.g. bitpack=true),
    /// agc=true|false, antenna=NAME, bandwidth=Hz. See --list-devices for the list.
    /// Without this option rfscan does not change any driver settings
    #[arg(long, value_name = "KEY=VALUE,...", value_parser = parse_kwargs, help_heading = H_SDR)]
    pub gain: Option<Kwargs>,

    /// Stream arguments for the driver in Soapy syntax "key=value,...", passed unchanged (e.g.
    /// bufflen=16384 on an RTL-SDR: smaller USB transfers, shorter delay after retuning). See
    /// --list-devices for the keys of your receiver
    #[arg(long, value_name = "KEY=VALUE,...", hide_short_help = true, value_parser = parse_kwargs, help_heading = H_SDR)]
    pub stream_args: Option<Kwargs>,

    /// Frequency correction of the receiver in ppm (e.g. 1.5 or -2.3), done by the SoapySDR
    /// driver; an error when the driver does not support it (see --list-devices). Receivers with
    /// a TCXO do not need it
    #[arg(long, hide_short_help = true, value_name = "PPM", allow_negative_numbers = true, help_heading = H_SDR)]
    pub ppm: Option<f64>,

    // Radio parameters
    /// Default modulation: am, fm (NFM) or digi
    ///
    /// Default modulation for ranges and channels without their own: am, fm (always NFM) or digi
    /// (requires --digi or --digi-tcp). Default: NFM
    #[arg(long, value_parser = parse_mode, value_name = "am|fm|digi", help_heading = H_RADIO)]
    pub mode: Option<Modulation>,

    /// Channel grid step for ranges without their own step (suffixes k, M). Default: AM 8.33 kHz
    /// (25/3 kHz airband grid), NFM 12.5 kHz
    #[arg(long, value_name = "HZ", hide_short_help = true, value_parser = parse_hz, help_heading = H_RADIO)]
    pub step: Option<u64>,

    /// Default channel bandwidth (Hz, suffixes k, M). Default: AM 8.33 kHz, NFM in ranges equal
    /// to the step, NFM in a channel list 12.5 kHz
    #[arg(long, value_name = "HZ", hide_short_help = true, value_parser = parse_hz, help_heading = H_RADIO)]
    pub bw: Option<u64>,

    /// Detection threshold: channel SNR in dB above the noise floor
    #[arg(long, value_name = "DB", default_value_t = 10.0, allow_negative_numbers = true, help_heading = H_RADIO)]
    pub squelch_snr: f32,

    /// Additional absolute channel power threshold in dBFS
    #[arg(long, value_name = "DBFS", hide_short_help = true, allow_negative_numbers = true, help_heading = H_RADIO)]
    pub squelch_abs: Option<f32>,

    /// Hysteresis in dB: the threshold for leaving Locked is this much lower than the entry
    /// threshold
    #[arg(long, value_name = "DB", hide_short_help = true, default_value_t = 3.0, help_heading = H_RADIO)]
    pub hysteresis: f32,

    /// Hit confirmation: before stopping, the scanner measures the channel again at a different
    /// LO and rejects signals that disappear (LO-dependent receiver artifacts): on or off
    #[arg(long, hide_short_help = true, default_value = "on", value_parser = parse_on_off, action = clap::ArgAction::Set, value_name = "on|off", help_heading = H_RADIO)]
    pub confirm: bool,

    /// Usable part of the window band (fraction of the sample rate)
    #[arg(long, value_name = "FRACTION", hide_short_help = true, default_value_t = 0.8, value_parser = parse_usable, help_heading = H_RADIO)]
    pub usable: f64,

    /// DC zone: number of channels (channel widths) on each side of the LO that a window does
    /// not measure. In ranges they are measured by the second, shifted pass; in channel mode the
    /// LO is placed away from the channels
    #[arg(long, value_name = "CHANNELS", hide_short_help = true, default_value_t = 1, help_heading = H_RADIO)]
    pub dc_guard: u64,

    /// Number of masked FFT bins on each side of DC
    #[arg(long, value_name = "BINS", hide_short_help = true, default_value_t = 1, help_heading = H_RADIO)]
    pub dc_mask: usize,

    /// FFT size. Default: the smallest power of 2 with bin <= min(step, bw) / 4
    #[arg(long, value_name = "N", hide_short_help = true, help_heading = H_RADIO)]
    pub fft_size: Option<usize>,

    // Scan parameters
    /// Range to scan, e.g. 144M-146M or 118M-137M:am (may be given multiple times)
    ///
    /// Range mode: START-STOP[:STEP][:nfm|am|digi][:bw=BW], may be given multiple times, e.g.
    /// 118M-137M:8.33k:am:bw=10k. Without suffixes: --step, --bw, --mode. Without a mode: 144M-146M
    #[arg(long, value_name = "RANGE", value_parser = parse_range_arg, conflicts_with = "scan_channels", help_heading = H_SCAN)]
    pub scan_range: Vec<RangeArg>,

    /// Channels to scan, e.g. 145.5M,118.1M:am; without a list: the channel file
    ///
    /// Channel mode: list F[:nfm|am|digi][:bw=BW],... (e.g. 145.5M,118.1M:am). Without a list:
    /// all channels from the channel file (--channels-file)
    #[arg(long, value_name = "LIST", num_args = 0..=1, value_parser = parse_channel_list, help_heading = H_SCAN)]
    pub scan_channels: Option<Option<ChannelList>>,

    /// Channel file (names, modes, bw). Default: ~/.config/rfscan/rfscan-channels.txt
    #[arg(long, value_name = "FILE", hide_short_help = true, help_heading = H_SCAN)]
    pub channels_file: Option<std::path::PathBuf>,

    /// Temporary exclusions of single channels (MHz, comma separated), e.g. 144.0,156.0.
    /// Not saved to the file
    #[arg(long, value_name = "MHZ,...", hide_short_help = true, help_heading = H_SCAN)]
    pub exclude: Option<String>,

    /// Persistent exclusion file. Default: ~/.config/rfscan/rfscan-exclude.txt
    #[arg(long, value_name = "FILE", hide_short_help = true, help_heading = H_SCAN)]
    pub exclude_file: Option<std::path::PathBuf>,

    /// How many adjacent channels on each side the `X` key excludes in the TUI
    #[arg(long, value_name = "N", hide_short_help = true, default_value_t = 2, help_heading = H_SCAN)]
    pub exclude_wide: u64,

    /// Wait time after the signal disappears before scanning resumes, in seconds
    #[arg(long, value_name = "SECONDS", default_value_t = 2.0, value_parser = parse_pause, help_heading = H_SCAN)]
    pub pause: f64,

    /// Time for discarding samples after retuning, in ms (must cover the device stream delay,
    /// otherwise the window shows signals from the previous tuning)
    #[arg(long, value_name = "MS", hide_short_help = true, default_value_t = 20.0, value_parser = parse_ms, help_heading = H_SCAN)]
    pub settle: f64,

    /// Sample collection time per window, in ms
    #[arg(long, value_name = "MS", hide_short_help = true, default_value_t = 1.0, value_parser = parse_ms, help_heading = H_SCAN)]
    pub dwell: f64,

    /// Retuning benchmark across the scan plan windows (ranges or channels), without detection
    #[arg(long, hide_short_help = true, help_heading = H_SCAN)]
    pub bench: bool,

    /// Measure --settle for this receiver and offer to save it
    ///
    /// Measure how long the samples after a retune still come from the previous frequency (a
    /// ghost test on a strong constant signal in the scanned range, with or without the antenna,
    /// with the scan settings in use), suggest --settle and offer to save it to the configuration
    /// file. A few minutes; --duration limits it
    #[arg(long, help_heading = H_SCAN)]
    pub calibrate: bool,

    /// Record the raw IQ stream of one channel to WAV files in DIR, until Ctrl+C or --duration.
    /// Only with --scan-channels and exactly one frequency (e.g. --scan-channels 145.5M
    /// --record-iq .); nothing else runs meanwhile (no detection, audio or TUI). I/Q as the
    /// left/right channel in the native sample format, the frequency and the sample rate inside
    /// the file (SDR#/HDSDR auxi chunk), a new file every 4 GB
    #[arg(long, hide_short_help = true, value_name = "DIR", conflicts_with_all = ["scan_range", "bench", "calibrate", "power"], help_heading = H_SCAN)]
    pub record_iq: Option<std::path::PathBuf>,

    // Audio parameters
    /// NFM de-emphasis: `off` or a time constant in µs (tune by ear)
    #[arg(long, value_name = "off|US", hide_short_help = true, default_value = "75", value_parser = parse_deemph, help_heading = H_AUDIO)]
    pub deemph: Deemph,

    /// Audio band-pass filter LOW-HIGH in Hz or `off`
    #[arg(long, value_name = "LOW-HIGH|off", hide_short_help = true, default_value = "300-3000", value_parser = parse_audio_band, help_heading = H_AUDIO)]
    pub audio_band: AudioBand,

    /// Audio AGC: on or off (the limiter is always active)
    #[arg(long, hide_short_help = true, default_value = "on", value_parser = parse_on_off, action = clap::ArgAction::Set, value_name = "on|off", help_heading = H_AUDIO)]
    pub agc: bool,

    /// Target RMS audio level for NFM in dBFS
    #[arg(long, value_name = "DBFS", hide_short_help = true, default_value_t = -6.0, allow_negative_numbers = true, help_heading = H_AUDIO)]
    pub agc_target: f32,

    /// Target RMS audio level for AM in dBFS (AM speech has higher peaks, so quieter than NFM)
    #[arg(long, value_name = "DBFS", hide_short_help = true, default_value_t = -10.0, allow_negative_numbers = true, help_heading = H_AUDIO)]
    pub agc_target_am: f32,

    /// Maximum AGC gain in dB
    #[arg(long, value_name = "DB", hide_short_help = true, default_value_t = 36.0, help_heading = H_AUDIO)]
    pub agc_max_gain: f32,

    /// Noise reduction: off or rnnoise
    ///
    /// Speech noise reduction of the analog audio (AM, NFM): `off` or `rnnoise`. The `n` key
    /// switches it in the TUI
    #[arg(long, default_value = "rnnoise", value_parser = parse_nr, action = clap::ArgAction::Set, value_name = "off|rnnoise", help_heading = H_AUDIO)]
    pub nr: bool,

    /// Noise reduction strength in percent: 0 mixes half of the denoised signal with half of the
    /// original, 100 is the denoised signal only. The `<` and `>` keys change it in the TUI
    #[arg(long, hide_short_help = true, default_value_t = 60, value_parser = clap::value_parser!(u32).range(0..=100), value_name = "PERCENT", help_heading = H_AUDIO)]
    pub nr_strength: u32,

    /// Audio output: name or a part of it (see --list-devices)
    ///
    /// Audio output: name or a part of it (see --list-devices). Default: the system default
    /// output
    #[arg(long, value_name = "NAME", help_heading = H_AUDIO)]
    pub audio_device: Option<String>,

    /// Local audio output: on or off (e.g. an RPi without a sound card, listening only in the
    /// web GUI)
    #[arg(long, hide_short_help = true, default_value = "on", value_parser = parse_on_off, action = clap::ArgAction::Set, value_name = "on|off", help_heading = H_AUDIO)]
    pub local_audio: bool,

    /// Digi output for DSD-FME: TCP server [HOST:]PORT (PORT alone = 127.0.0.1), e.g. 7355.
    /// PCM stream 48 kHz s16le mono: silence or the raw discriminator on digi channels.
    /// Without this option digi channels from the channel file are skipped
    #[arg(long, hide_short_help = true, value_parser = parse_digi_addr, value_name = "[HOST:]PORT", help_heading = H_AUDIO)]
    pub digi_tcp: Option<std::net::SocketAddr>,

    /// Digi stream gain in dB (clipped to full scale)
    #[arg(long, value_name = "DB", hide_short_help = true, default_value_t = 0.0, allow_negative_numbers = true, help_heading = H_AUDIO)]
    pub digi_gain: f32,

    /// Digital voice through DSD-FME started by rfscan
    ///
    /// Digital mode with DSD-FME started by rfscan in the background: rfscan sends it the
    /// discriminator of digi channels and plays its decoded voice. DSD-FME is taken from
    /// --dsd-fme or found in PATH
    #[arg(long, conflicts_with = "digi_tcp", help_heading = H_AUDIO)]
    pub digi: bool,

    /// The DSD-FME program for --digi. Default: dsd-fme in PATH
    #[arg(long, value_name = "PATH", requires = "digi", help_heading = H_AUDIO)]
    pub dsd_fme: Option<std::path::PathBuf>,

    /// DSD-FME decoder arguments for --digi, e.g. "-fn" for NXDN96 or "-fa -G group.csv"
    /// (rfscan sets the input, the output and the event log)
    #[arg(long, hide_short_help = true, default_value = "-fa", allow_hyphen_values = true, value_name = "ARGS", help_heading = H_AUDIO)]
    pub dsd_fme_args: String,

    /// Directory for DSD-FME's console output (dsd-fme.log) and event log (dsd-fme-events.log)
    /// with --digi; it must exist. Separate directories let several rfscan instances run with
    /// --digi. Default: ~/.config/rfscan
    #[arg(long, hide_short_help = true, value_name = "DIR", requires = "digi", help_heading = H_AUDIO)]
    pub dsd_fme_logs: Option<std::path::PathBuf>,

    /// Decoded voice from a DSD-FME started by hand (with --digi-tcp): UDP [HOST:]PORT[:RATE]
    /// (PORT alone = 127.0.0.1), DSD-FME sends it with -o udp:HOST:PORT. Without RATE the sample
    /// rate is estimated from the stream (mono or stereo always is)
    #[arg(long, hide_short_help = true, value_parser = parse_voice_addr, requires = "digi_tcp", value_name = "[HOST:]PORT[:RATE]", help_heading = H_AUDIO)]
    pub digi_audio_udp: Option<VoiceAddr>,
    // Other
    /// Activity log file. Default: ~/.config/rfscan/rfscan-activity.txt
    #[arg(long, value_name = "FILE", hide_short_help = true, help_heading = H_OTHER)]
    pub activity_file: Option<std::path::PathBuf>,

    /// Archive the old activity log (.bak) and start from zero
    #[arg(long, hide_short_help = true, help_heading = H_OTHER)]
    pub activity_reset: bool,

    /// Print the activity report from the file and exit (without opening the device)
    #[arg(long, help_heading = H_OTHER)]
    pub report: bool,

    /// Write the log to a file
    ///
    /// Write the log to a file (appended, with date and time). Without this option the log goes
    /// to the console only
    #[arg(long, value_name = "FILE", help_heading = H_OTHER)]
    pub log: Option<std::path::PathBuf>,

    /// File log level: info (detected transmissions) or debug (everything from the console)
    #[arg(long, hide_short_help = true, default_value = "info", value_parser = crate::log::Level::parse, value_name = "info|debug", help_heading = H_OTHER)]
    pub log_level: crate::log::Level,

    /// Without the TUI: events on the console
    ///
    /// Run without the TUI: events on stdout (also automatic when the output is not a terminal)
    #[arg(long, help_heading = H_OTHER)]
    pub headless: bool,

    /// Web page for a phone or a browser, e.g. 8080 or 0.0.0.0:8080
    ///
    /// Web GUI: HTTP server [HOST:]PORT (PORT alone = 127.0.0.1), e.g. 8080 or 0.0.0.0:8080 for
    /// the home network. No HTTPS and no advanced security: for a home network or a VPN only
    #[arg(long, value_parser = parse_digi_addr, value_name = "[HOST:]PORT", help_heading = H_OTHER)]
    pub web_gui: Option<std::net::SocketAddr>,

    /// Web GUI access token (the link is http://HOST:PORT/?t=TOKEN). Required on a non-loopback
    /// address: without this option a random token is generated at every start. Ignored without
    /// --web-gui (so it can stay in the configuration file)
    #[arg(long, hide_short_help = true, value_name = "TOKEN", help_heading = H_OTHER)]
    pub web_token: Option<String>,

    /// Exit after the given time in seconds (fractions allowed)
    #[arg(long, value_name = "SECONDS", value_parser = parse_duration, help_heading = H_OTHER)]
    pub duration: Option<f64>,

    /// Diagnostics: receive on one frequency and print the whole band power every second
    #[arg(long, value_name = "HZ", hide_short_help = true, value_parser = parse_hz, help_heading = H_OTHER)]
    pub power: Option<u64>,

    /// Configuration file. Default: ~/.config/rfscan/rfscan.conf
    ///
    /// Configuration file (TOML, keys are the long options without "--"; command line options
    /// take precedence). Default: ~/.config/rfscan/rfscan.conf if it exists
    #[arg(long, value_name = "FILE", help_heading = H_OTHER)]
    pub config: Option<std::path::PathBuf>,

    /// Read no configuration file: only the command line and the built-in defaults
    #[arg(long, hide_short_help = true, conflicts_with = "config", help_heading = H_OTHER)]
    pub no_config: bool,

    /// Write a configuration file with all options and exit
    ///
    /// Write a configuration file with all options (defaults commented out) and exit; an
    /// existing file is overwritten. Default: ~/.config/rfscan/rfscan.conf
    #[arg(long, num_args = 0..=1, value_name = "FILE", help_heading = H_OTHER)]
    pub gen_conf: Option<Option<std::path::PathBuf>>,

    // `-h` and `--help` are handled by the `help` module before parsing; the version is named in
    // the line after the option list.
    /// Show version
    #[arg(short = 'V', long, action = clap::ArgAction::Version, hide = true)]
    #[allow(dead_code)]
    version: Option<bool>,
}

/// Parses a frequency with an optional `k`, `M`, `G` suffix into whole Hz.
pub fn parse_hz(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1e3),
        Some('M') => (&s[..s.len() - 1], 1e6),
        Some('G' | 'g') => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid frequency '{s}' (examples: 145.5M, 8.33k, 6000000)"))?;
    let hz = (value * mult).round();
    if !hz.is_finite() || hz <= 0.0 || hz > u64::MAX as f64 {
        return Err(format!("frequency '{s}' must be positive"));
    }
    Ok(hz as u64)
}

/// Parses "key=value, key=value" (Soapy kwargs syntax). Values are not interpreted.
pub fn parse_kwargs(s: &str) -> Result<Kwargs, String> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let Some((k, v)) = part.split_once('=') else {
            return Err(format!(
                "'{part}': expected key=value (e.g. LNA=9,MIX=10,VGA=7)"
            ));
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            return Err(format!("'{part}': empty key"));
        }
        out.push((k.to_string(), v.to_string()));
    }
    if out.is_empty() {
        return Err("empty configuration (e.g. LNA=9,MIX=10,VGA=7)".into());
    }
    Ok(Kwargs(out))
}

/// Parses a START-STOP range, e.g. `430M-440M`.
pub fn parse_range(s: &str) -> Result<(u64, u64), String> {
    let Some((a, b)) = s.split_once('-') else {
        return Err(format!(
            "'{s}': expected a START-STOP range, e.g. 430M-440M"
        ));
    };
    let (start, stop) = (parse_hz(a)?, parse_hz(b)?);
    if start >= stop {
        return Err(format!("'{s}': range start must be lower than its end"));
    }
    Ok((start, stop))
}

/// A range from `--scan-range`: missing fields are filled from `--step`, `--bw` and the default
/// modulation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeArg {
    pub start: u64,
    pub stop: u64,
    pub step: Option<u64>,
    pub mode: Option<Modulation>,
    pub bw: Option<u64>,
}

/// A channel from `--scan-channels`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelArg {
    pub freq: u64,
    pub mode: Option<Modulation>,
    pub bw: Option<u64>,
    /// Airband 25 kHz channel (given with its 25 kHz channel number).
    pub raster25: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelList(pub Vec<ChannelArg>);

/// Fields after the colon: step, modulation, bw.
type Suffixes = (Option<u64>, Option<Modulation>, Option<u64>);

/// Fields after the colon: mode (`nfm`, `fm`, `am`, `digi`), `bw=...` and (if `allow_step`)
/// the step.
fn parse_suffixes(fields: &[&str], allow_step: bool) -> Result<Suffixes, String> {
    let (mut step, mut mode, mut bw) = (None, None, None);
    for &f in fields {
        let f = f.trim();
        if let Some(v) = f.strip_prefix("bw=") {
            if bw.replace(parse_hz(v)?).is_some() {
                return Err("bw= given twice".into());
            }
        } else if let Some(m) = Modulation::parse(f) {
            if mode.replace(m).is_some() {
                return Err("modulation given twice".into());
            }
        } else if allow_step {
            if step.replace(parse_hz(f)?).is_some() {
                return Err("step given twice".into());
            }
        } else {
            return Err(format!("'{f}': expected nfm, am, digi or bw=..."));
        }
    }
    Ok((step, mode, bw))
}

/// Parses `START-STOP[:STEP][:nfm|am|digi][:bw=BW]`, e.g. `118M-137M:8.33k:am:bw=10k`.
pub fn parse_range_arg(s: &str) -> Result<RangeArg, String> {
    let mut fields = s.split(':');
    let (start, stop) = parse_range(fields.next().unwrap_or(""))?;
    let rest: Vec<&str> = fields.collect();
    let (step, mode, bw) = parse_suffixes(&rest, true).map_err(|e| format!("'{s}': {e}"))?;
    Ok(RangeArg {
        start,
        stop,
        step,
        mode,
        bw,
    })
}

/// Parses a list `F[:nfm|am|digi][:bw=BW],...`, e.g. `145.5M,118.1M:am:bw=10k`.
pub fn parse_channel_list(s: &str) -> Result<ChannelList, String> {
    let mut out = Vec::new();
    for item in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let mut fields = item.split(':');
        let text = fields.next().unwrap_or("");
        let freq = parse_hz(text)?;
        // In MHz with at most three decimal places, an airband value is a channel number.
        let (freq, raster25) = match text.trim().strip_suffix('M') {
            Some(mhz) => crate::airband::from_text(mhz, freq)?,
            None => (freq, false),
        };
        let rest: Vec<&str> = fields.collect();
        let (_, mode, bw) = parse_suffixes(&rest, false).map_err(|e| format!("'{item}': {e}"))?;
        out.push(ChannelArg {
            freq,
            mode,
            bw,
            raster25,
        });
    }
    if out.is_empty() {
        return Err("empty channel list".into());
    }
    Ok(ChannelList(out))
}

/// De-emphasis: time constant in µs or none.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Deemph(pub Option<f64>);

/// Audio filter band in Hz or no filter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioBand(pub Option<(f64, f64)>);

fn parse_nr(s: &str) -> Result<bool, String> {
    match s {
        "off" => Ok(false),
        "rnnoise" => Ok(true),
        _ => Err(format!("'{s}': expected off or rnnoise")),
    }
}

fn parse_deemph(s: &str) -> Result<Deemph, String> {
    if s.eq_ignore_ascii_case("off") {
        return Ok(Deemph(None));
    }
    let us: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("'{s}': expected 'off' or a time constant in µs (e.g. 75)"))?;
    if !us.is_finite() || us <= 0.0 {
        return Err(format!("'{s}': time constant must be positive"));
    }
    Ok(Deemph(Some(us)))
}

fn parse_audio_band(s: &str) -> Result<AudioBand, String> {
    if s.eq_ignore_ascii_case("off") {
        return Ok(AudioBand(None));
    }
    let bad = || format!("'{s}': expected LOW-HIGH in Hz (e.g. 300-3000) or 'off'");
    let (a, b) = s.split_once('-').ok_or_else(bad)?;
    let lo: f64 = a.trim().parse().map_err(|_| bad())?;
    let hi: f64 = b.trim().parse().map_err(|_| bad())?;
    // The upper edge with its stopband (4/3) must fit below half of 48 kHz.
    if !(lo > 0.0 && hi > lo && hi * 4.0 / 3.0 < 24_000.0) {
        return Err(format!(
            "'{s}': required 0 < LOW < HIGH and HIGH < 18000 Hz"
        ));
    }
    Ok(AudioBand(Some((lo, hi))))
}

fn parse_mode(s: &str) -> Result<Modulation, String> {
    Modulation::parse(s).ok_or_else(|| format!("'{s}': expected am, fm or digi"))
}

/// Digi output address: `HOST:PORT` or `PORT` alone (then 127.0.0.1).
pub fn parse_digi_addr(s: &str) -> Result<std::net::SocketAddr, String> {
    use std::net::ToSocketAddrs;
    let s = s.trim();
    if let Ok(port) = s.parse::<u16>() {
        return Ok(std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    }
    s.to_socket_addrs()
        .map_err(|e| format!("'{s}': expected [HOST:]PORT ({e})"))?
        .next()
        .ok_or_else(|| format!("'{s}': cannot resolve the address"))
}

/// Decoded voice address with an optional sample rate (`--digi-audio-udp`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceAddr {
    pub addr: std::net::SocketAddr,
    pub rate: Option<u32>,
}

/// `[HOST:]PORT[:RATE]`: a trailing `:RATE` is taken only when the rest is a valid address.
fn parse_voice_addr(s: &str) -> Result<VoiceAddr, String> {
    let s = s.trim();
    if let Some((head, tail)) = s.rsplit_once(':')
        && let (Ok(addr), Ok(rate)) = (parse_digi_addr(head), tail.parse::<u32>())
    {
        if !(8_000..=48_000).contains(&rate) {
            return Err(format!("'{s}': the sample rate must be 8000-48000 Hz"));
        }
        return Ok(VoiceAddr {
            addr,
            rate: Some(rate),
        });
    }
    Ok(VoiceAddr {
        addr: parse_digi_addr(s)?,
        rate: None,
    })
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("'{s}': expected on or off")),
    }
}

fn parse_usable(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}': expected a fraction, e.g. 0.8"))?;
    if !(v > 0.0 && v <= 1.0) {
        return Err(format!("'{s}': --usable must be in the range (0, 1]"));
    }
    Ok(v)
}

fn parse_ms(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}': expected a time in ms"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("'{s}': time cannot be negative"));
    }
    Ok(v)
}

fn parse_pause(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}': expected a time in seconds"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("'{s}': pause cannot be negative"));
    }
    Ok(v)
}

fn parse_duration(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("invalid time '{s}' (seconds)"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("time '{s}' must be positive"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelArg, Cli, Modulation, RangeArg, parse_channel_list, parse_hz, parse_kwargs,
        parse_range, parse_range_arg, parse_voice_addr,
    };
    use clap::Parser;

    #[test]
    fn voice_address_with_optional_rate() {
        let a = |s: &str| parse_voice_addr(s).unwrap();
        assert_eq!(a("23456").addr, "127.0.0.1:23456".parse().unwrap());
        assert_eq!(a("23456").rate, None);
        assert_eq!(a("23456:8000").rate, Some(8_000));
        assert_eq!(a("127.0.0.1:23456").rate, None);
        assert_eq!(a("127.0.0.1:23456").addr.port(), 23456);
        assert_eq!(a("127.0.0.1:23456:48000").rate, Some(48_000));
        assert!(parse_voice_addr("23456:100").is_err());
    }

    #[test]
    fn digi_options_exclude_each_other() {
        assert!(Cli::try_parse_from(["rfscan", "--digi", "--digi-tcp", "7355"]).is_err());
        assert!(Cli::try_parse_from(["rfscan", "--dsd-fme", "x.exe"]).is_err());
        assert!(Cli::try_parse_from(["rfscan", "--digi-audio-udp", "23456"]).is_err());
        assert!(Cli::try_parse_from(["rfscan", "--dsd-fme-logs", "logs"]).is_err());
        let c = Cli::try_parse_from(["rfscan", "--digi", "--dsd-fme-logs", "logs"]).unwrap();
        assert_eq!(c.dsd_fme_logs, Some("logs".into()));
        let c =
            Cli::try_parse_from(["rfscan", "--digi", "--dsd-fme-args", "-fn -G g.csv"]).unwrap();
        assert_eq!(c.dsd_fme_args, "-fn -G g.csv");
        assert_eq!(Cli::try_parse_from(["rfscan"]).unwrap().dsd_fme_args, "-fa");
    }
    #[test]
    fn range_args() {
        assert_eq!(
            parse_range_arg("118M-137M:8.33k:am:bw=10k"),
            Ok(RangeArg {
                start: 118_000_000,
                stop: 137_000_000,
                step: Some(8_330),
                mode: Some(Modulation::Am),
                bw: Some(10_000),
            })
        );
        let r = parse_range_arg("144M-146M").unwrap();
        assert_eq!((r.step, r.mode, r.bw), (None, None, None));
        assert_eq!(
            parse_range_arg("144M-146M:nfm").unwrap().mode,
            Some(Modulation::Nfm)
        );
        assert!(parse_range_arg("144M-146M:12.5k:25k").is_err());
        assert!(parse_range_arg("144M-146M:xyz").is_err());
    }

    #[test]
    fn channel_lists() {
        let l = parse_channel_list("145.5M, 118.1M:am:bw=10k").unwrap().0;
        assert_eq!(
            l,
            vec![
                ChannelArg {
                    freq: 145_500_000,
                    mode: None,
                    bw: None,
                    raster25: false
                },
                ChannelArg {
                    freq: 118_100_000,
                    mode: Some(Modulation::Am),
                    bw: Some(10_000),
                    raster25: true
                },
            ]
        );
        // Airband channel numbers are converted to the actual frequency.
        let l = parse_channel_list("118.005M:am,118.010M:am").unwrap().0;
        assert_eq!((l[0].freq, l[0].raster25), (118_000_000, false));
        assert_eq!(l[1].freq, 118_008_333);
        assert!(parse_channel_list("118.020M:am").is_err());
        assert!(parse_channel_list("145.5M:12.5k").is_err());
    }

    #[test]
    fn scan_modes() {
        let c = Cli::try_parse_from([
            "rfscan",
            "--scan-range",
            "144M-146M",
            "--scan-range",
            "430M-440M:25k",
        ])
        .unwrap();
        assert_eq!(c.scan_range.len(), 2);
        assert!(c.scan_channels.is_none());
        let c = Cli::try_parse_from(["rfscan", "--scan-channels"]).unwrap();
        assert_eq!(c.scan_channels, Some(None));
        let c = Cli::try_parse_from(["rfscan", "--scan-channels", "145.5M"]).unwrap();
        assert!(matches!(c.scan_channels, Some(Some(_))));
        // --agc takes an on|off value, default on; --agc-target default -6 dBFS.
        let c = Cli::try_parse_from(["rfscan"]).unwrap();
        assert!(c.agc);
        assert_eq!(c.agc_target, -6.0);
        assert!(!Cli::try_parse_from(["rfscan", "--agc", "off"]).unwrap().agc);
        assert!(Cli::try_parse_from(["rfscan", "--agc", "on"]).unwrap().agc);
        assert!(Cli::try_parse_from(["rfscan", "--agc", "xyz"]).is_err());
        // --mode, --digi-tcp (port alone = loopback).
        let c = Cli::try_parse_from(["rfscan", "--mode", "digi", "--digi-tcp", "7355"]).unwrap();
        assert_eq!(c.mode, Some(Modulation::Digi));
        assert_eq!(c.digi_tcp, Some("127.0.0.1:7355".parse().unwrap()));
        let c = Cli::try_parse_from(["rfscan", "--digi-tcp", "0.0.0.0:7000"]).unwrap();
        assert_eq!(c.digi_tcp, Some("0.0.0.0:7000".parse().unwrap()));
        assert!(Cli::try_parse_from(["rfscan", "--mode", "wfm"]).is_err());
        let c = Cli::try_parse_from(["rfscan", "--scan-range", "438M-439M:digi"]).unwrap();
        assert_eq!(c.scan_range[0].mode, Some(Modulation::Digi));
        // The modes are mutually exclusive.
        assert!(
            Cli::try_parse_from(["rfscan", "--scan-range", "144M-146M", "--scan-channels"])
                .is_err()
        );
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("430M-440M"), Ok((430_000_000, 440_000_000)));
        assert!(parse_range("440M-430M").is_err());
        assert!(parse_range("144M").is_err());
    }

    #[test]
    fn kwargs_order_and_errors() {
        let kw = parse_kwargs("LNA=9, MIX=10 ,VGA=7,bitpack=true").map(|k| k.0);
        let pairs = [
            ("LNA", "9"),
            ("MIX", "10"),
            ("VGA", "7"),
            ("bitpack", "true"),
        ];
        let expected: Vec<_> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(kw, Ok(expected));
        assert!(parse_kwargs("LNA").is_err());
        assert!(parse_kwargs("=5").is_err());
        assert!(parse_kwargs("").is_err());
    }

    #[test]
    fn hz_suffixes() {
        assert_eq!(parse_hz("145.5M"), Ok(145_500_000));
        assert_eq!(parse_hz("433.00625M"), Ok(433_006_250));
        assert_eq!(parse_hz("8.33k"), Ok(8_330));
        assert_eq!(parse_hz("1.2G"), Ok(1_200_000_000));
        assert_eq!(parse_hz("6000000"), Ok(6_000_000));
        assert!(parse_hz("0").is_err());
        assert!(parse_hz("-5M").is_err());
        assert!(parse_hz("abc").is_err());
    }
}
