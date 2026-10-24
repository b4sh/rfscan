# rfscan: documentation

The full description of rfscan's functions. A short introduction and the quick start are in [README.md](README.md); `rfscan -h` lists the most used options, `rfscan --help` all of them.

- [Installation](#installation)
- [Receiver setup](#receiver-setup)
- [Scanning](#scanning)
- [Detection and stopping](#detection-and-stopping)
- [Channel file: names and modes](#channel-file-names-and-modes)
- [Exclusions](#exclusions)
- [Activity log](#activity-log)
- [Airband channel numbers (8.33 kHz)](#airband-channel-numbers-833-khz)
- [Audio](#audio)
- [Digital voice through DSD-FME](#digital-voice-through-dsd-fme)
- [TUI](#tui)
- [Console mode and log file](#console-mode-and-log-file)
- [Web GUI](#web-gui)
- [IQ recording](#iq-recording)
- [Configuration file](#configuration-file)
- [Files](#files)

## Installation

rfscan needs SoapySDR with the module (driver) of your receiver.

**Windows, ready package.** Download the zip from the Releases page of the repository and unpack it anywhere. It contains `rfscan.exe` with SoapySDR, the receiver drivers and their libraries, so PothosSDR is not needed (an installed PothosSDR does not get in the way). Install the WinUSB driver for your receiver with [Zadig](https://zadig.akeo.ie/), e.g. for an RTL-SDR. An Airspy needs no driver installation (Windows sets up its driver by itself), an SDRplay needs the SDRplay API service instead. For an SDRplay, copy `extras\sdrplay\sdrPlaySupport.dll` of the package to `lib\SoapySDR\modules0.8\` and the `sdrplay_api.dll` (x64) of your SDRplay API installation next to `rfscan.exe` (the module is kept aside because without that library SoapySDR would report a failed module at every start).

**Windows, building from source.** Install [PothosSDR](https://downloads.myriadrf.org/builds/PothosSDR/) and add it to `PATH` (the installer offers it). The USB driver is installed as for the ready package (above). Build rfscan with [Rust](https://rustup.rs/): `cargo build --release`.

- **RTL-SDR with PothosSDR:** the last PothosSDR (2021) has an old `rtlsdr.dll`, with which rfscan crashes when it closes an RTL-SDR (exit code 139; the scan itself works and everything is saved before). Replace `C:\Program Files\PothosSDR\bin\rtlsdr.dll` with the x64 `rtlsdr.dll` from a current [rtl-sdr-blog](https://github.com/rtlsdrblog/rtl-sdr-blog/releases) release (keep a copy of the old one), or put it next to `rfscan.exe`, which affects rfscan only.
- **SDRplay with PothosSDR:** PothosSDR has the SDRplay API library 3.07 (`sdrplay_api.dll` from 2020), which does not work with a current SDRplay API service (no device found, the RSP is reset). Put the `sdrplay_api.dll` (x64) of your installed SDRplay API next to `rfscan.exe`; the module then warns that the version differs from its build version, but works (checked with API 3.15).

**Linux, including the Raspberry Pi.** Tested on a Raspberry Pi 4 (4 GB) with Debian 13 (64-bit). Install SoapySDR with its development files, the modules of your receivers and the build tools from the system packages, then [Rust](https://rustup.rs/), and build and install rfscan (as a normal user, not with `sudo`):

```
sudo apt install build-essential pkg-config clang libclang-dev git libsoapysdr-dev soapysdr-module-rtlsdr soapysdr-module-airspy libasound2-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://github.com/b4sh/rfscan
cd rfscan
packaging/linux/install.sh
```

The script checks the build dependencies (when one is missing it prints the `apt` command and stops; it installs no system packages itself), builds rfscan with `cargo build --release` and asks for `sudo` only to copy the program to `/usr/local/bin/rfscan` and the documentation to `/usr/local/share/doc/rfscan`. Another location: `PREFIX=/opt/rfscan packaging/linux/install.sh`. `--no-build` installs an already built `target/release/rfscan`, `--uninstall` removes the installed files (your files in `~/.config/rfscan` stay). To update: `git pull` and run the script again. Without installing, the program is `target/release/rfscan` after `cargo build --release`. The build takes about 7 minutes on a Raspberry Pi 4. The Airspy and RTL-SDR packages set up the USB access by themselves (udev rules), so the receivers work without root. An RTL-SDR is also taken by the kernel's DVB-T driver; librtlsdr detaches it when rfscan opens the receiver and attaches it again on exit, but after a few runs the stream may stop (`the stream delivers no samples`). Block the DVB-T driver once (the Debian packages do not do it): unplug the receiver, run

```
printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832_sdr\nblacklist rtl2832\n' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
sudo modprobe -r dvb_usb_rtl28xxu rtl2832_sdr rtl2832
```

and plug it in again.

**SDRplay on Linux.** The SDRplay API is not in the system packages. Install the API for Linux from [sdrplay.com/api](https://www.sdrplay.com/api/) (the installer `SDRplay_RSP_API-Linux-*.run` also covers the 64-bit Raspberry Pi; it asks you to accept the SDRplay license and installs the `sdrplay` service), then build the SoapySDR module:

```
git clone https://github.com/pothosware/SoapySDRPlay3
cd SoapySDRPlay3 && cmake -S . -B build && cmake --build build -j4 && sudo cmake --install build && sudo ldconfig
```

It is installed to `/usr/local/lib/SoapySDR/modules0.8`, where SoapySDR finds it (`rfscan --list-devices` shows `sdrplay OK` among the loaded modules).

On Linux `--list-devices` lists every ALSA device of a sound card (`hw:`, `plughw:`, `dmix:` ...), with its ALSA name in brackets, e.g. `--audio-device "plughw:CARD=Headphones"`. A Raspberry Pi without speakers runs with `--local-audio off` and plays the audio in the [web GUI](#web-gui).

`rfscan --list-devices` shows the receivers SoapySDR sees, with their frequency ranges, sample rates, gain stages, sample format, driver settings and stream arguments, plus the SoapySDR version and module diagnostics (useful when no receiver is found). Sound cards (SoapySDR `driver=audio`) are listed but never used as receivers. `--device` chooses a receiver in Soapy syntax, e.g. `--device driver=rtlsdr` or `--device driver=airspy,serial=...`; without it rfscan takes the first free one.

## Receiver setup

Good results depend first of all on a few settings of your receiver, and rfscan cannot guess them:

- **Sample rate (`--sample-rate`).** Without it rfscan takes the highest rate the device reports, which is not always the best one. A rate the receiver or the USB link cannot sustain loses samples (overflows: the driver prints `O` in the console, rfscan counts them in the statistics) and gives false or missed detections. Example: an RTL-SDR reports up to 3.2 MSps, but runs stably at `--sample-rate 2.4M`; at a higher rate, especially with a too short `--settle`, it can stop responding, and rfscan then ends with `setFrequency ... failed`. Choose the highest rate that runs without overflows.
- **Gain (`--gain`).** The receiver's automatic gain is often a poor choice for scanning (on an RTL-SDR it is very bad). Too little gain hides weak signals, too much overloads the receiver with intermod and false signals. Find good values for your antenna and location, e.g. in SDR# or another SDR program, and give them as gain stages (`rfscan --list-devices` lists the stages of your receiver), e.g. `--gain "LNA=9,MIX=10,VGA=7"` on an Airspy or `--gain "TUNER=30"` on an RTL-SDR.
- **Settle time (`--settle`).** After a retune the receiver still delivers samples of the previous frequency for a while; scanning them shows "ghosts" of signals in wrong places. The default 20 ms fits an Airspy, but not every receiver. `rfscan --calibrate` measures it (see below).
- **Stream buffers (`--stream-args`).** Some drivers deliver samples in large blocks, so after a retune a whole block still comes from the previous frequency and the settle gets long. Smaller buffers shorten it. The arguments differ between drivers: `rfscan --list-devices` lists them for your receiver (e.g. `bufflen` and `buffers` on an RTL-SDR). A sign of large buffers: a long settle from `--calibrate`, and a `--bench` warning that the read is shorter than the duration of the samples.

`--gain` holds the whole receiver configuration in Soapy syntax `key=value,...`, applied in the order given: gain stages (`LNA=9`), driver settings (`bitpack=true`), and `agc=true|false`, `antenna=NAME`, `bandwidth=Hz`. Setting a gain stage switches the hardware AGC off, unless the same option says `agc=true` (e.g. `agc=true,VGA=7` on an Airspy: AGC controls LNA and mixer, VGA is manual). Without `--gain` rfscan changes no driver settings. rfscan never switches on a bias tee (antenna power) by itself; if your antenna needs it, give it explicitly, e.g. `--gain "biastee=true"` on an Airspy.

Settings found so far (other receivers welcome):

| Receiver | Settings | `--settle` |
|---|---|---|
| Airspy Mini | `--gain "agc=true,VGA=7"` (stream arguments not needed) | 20 ms |
| RTL-SDR (R828D, rtl-sdr-blog driver) | `--sample-rate 2.4M --gain "TUNER=28" --stream-args "bufflen=16384,buffers=64"` | 63 ms (113-125 ms with the default stream buffers) |
| RTL-SDR (R828D) on a Raspberry Pi 4 (Debian librtlsdr 2.0) | the same | 63 ms (`--calibrate` on a carrier with an SNR of 33 dB suggests 23 ms, 88 ms with the default stream buffers; in a band with very strong signals ghosts stayed up to about 45 ms, while in a quiet band 30 ms worked) |
| SDRplay RSP1A (API 3.15) | `--sample-rate 6M --gain "IFGR=43,RFGR=4"` (no stream arguments; above 6 MSps occasional overflows; `IFGR` is an attenuation, a higher value means less gain; the hardware AGC, `agc=true`, changes the gain while scanning, so the noise level jumps and the whole mini-spectrum rises for a while) | 63 ms (measured at 8 MSps) |
| SDRplay RSP1A on a Raspberry Pi 4 (API 3.15.2, SoapySDRPlay3 0.5.2) | the same | 63 ms (`--calibrate` suggests 38 ms, but in a band with very strong signals ghosts stayed at 38 ms; clean from about 50 ms) |

The gain depends on your antenna and location; the stream arguments and the settle depend on the receiver. All of these can be kept in the [configuration file](#configuration-file), one file per receiver if you have several.

A receiver without a TCXO may need a frequency correction: `--ppm 1.5` (the same value as in SDR# or SDR++). It is done by the SoapySDR driver; `rfscan --list-devices` shows whether the driver supports it (`frequency correction (ppm): yes`), otherwise `--ppm` gives an error.

### Calibrating `--settle`

```
rfscan --calibrate --scan-range 144M-146M
```

with your other settings (sample rate, gain, stream arguments) on the command line or in the configuration file. The calibration needs a strong constant signal (SNR of at least 20 dB) in the given range: a beacon, a repeater with a carrier, or, with the antenna disconnected, a carrier of the receiver itself (most receivers have a few; SDR# shows them). It alternately tunes to the window with the signal and to a window one sample rate away, and checks after how many milliseconds the signal no longer shows up in the second one. It measures for up to about 4 minutes (`--duration` shortens it), prints a line per settle value tried, suggests a value with a margin and asks whether to save it to the configuration file (`--config FILE` or the default one; other lines of the file stay unchanged). When the settle is long and the driver has stream arguments, it also points to them.

### Benchmark

`rfscan --bench --scan-range ...` retunes through the scan plan without detection and reports the retune time, windows per second, MHz per second and overflows (10 s, or `--duration`). Useful for comparing sample rates and stream arguments.

## Scanning

There are two scan modes; one of them is used at a time.

**Ranges:** `--scan-range START-STOP[:STEP][:nfm|am|digi][:bw=BW]`, may be given several times; the ranges are scanned in the order given. Values take the suffixes `k`, `M`, `G`. Examples:

```
--scan-range 144M-146M                 # NFM, 12.5 kHz step
--scan-range 118M-137M:am              # AM, 8.33 kHz step
--scan-range 430M-440M:25k:nfm
--scan-range 118M-137M:8.33k:am:bw=10k
```

**Channels:** `--scan-channels F[:nfm|am|digi][:bw=BW],...`, e.g. `--scan-channels 145.5M,118.1M:am:bw=10k`. Without a list, `--scan-channels` scans all channels from the [channel file](#channel-file-names-and-modes). Channels that fit in one window are checked with a single tuning.

Without a scan mode rfscan scans 144-146 MHz, NFM, 12.5 kHz.

**Step and bandwidth** are two separate things. The step is the channel grid (channel n = start + n·step, computed in whole Hz without drift); `bw` is the received bandwidth of a channel: where the detector measures the power and the width of the demodulator filter. An 8.33 kHz step means exactly 25/3 kHz, the airband grid. The step is one of the standard steps 2.5k, 5k, 6.25k, 7.5k, 8.33k, 9k, 10k, 12.5k, 15k, 20k, 25k, 30k, 50k, and `bw` lies between 2.5k and 50k; other values are an error (a number without a suffix is in Hz, so `8.33` is 8 Hz and rfscan suggests `8.33k`). Defaults: AM step and `bw` 8.33 kHz; NFM in ranges step 12.5 kHz and `bw` equal to the step; NFM and `digi` in a channel list `bw` 12.5 kHz. `--step`, `--bw` and `--mode` set the defaults for ranges and channels without their own. A `bw` smaller than the step leaves gaps between channels (a warning), a `bw` above 25 kHz in FM gives a warning (wideband FM is not supported).

**Windows and passes.** A range is divided into windows: one tuner setting covers the channels that fit in the usable part of the band (`--usable`, default 0.8 of the sample rate; the band edges are attenuated). The tuner is never set on a channel: `--dc-guard N` (default 1) channels on each side of the tuner frequency form a DC zone that the window does not measure; a second pass with shifted windows measures them, and the two passes alternate (`--dc-guard 0` gives a single pass). `--dc-mask` masks FFT bins around DC as well. The FFT size (`--fft-size`) is chosen so that a channel spans several bins.

**Speed and sensitivity.** After every retune rfscan discards `--settle` ms of samples, then measures for `--dwell` ms (default 1 ms). A longer dwell averages more and finds weaker signals, but scans slower.

## Detection and stopping

- **Threshold.** A channel stops the scanner when its SNR (power above the noise floor of its window) reaches `--squelch-snr` dB (default 10). `--squelch-abs dBFS` adds an absolute power threshold. The levels are in dBFS, relative to the full scale of the receiver's converter (not dBm).
- **Strongest channel.** Of several channels above the threshold in one window, the strongest one wins. A channel weaker than its direct neighbor (also an excluded one) is leakage of the neighbor's signal and does not stop the scanner.
- **Confirmation** (`--confirm on|off`, default on). Before stopping, rfscan measures the channel again with the tuner set elsewhere (for about 60 ms). A real signal stays on its frequency; images and spurs of the receiver move with the tuner and are rejected (`rejected` in the console). Cost: about one settle plus 60 ms per hit.
- **States.** LOCKED: the channel is demodulated and played. When the SNR falls below the threshold minus `--hysteresis` (default 3 dB) for 30 ms, the scanner waits (HANG) for `--pause` seconds (default 2); if the signal returns, it goes back to the channel, otherwise it goes on scanning from the next window.

## Channel file: names and modes

The channel file (default `~/.config/rfscan/rfscan-channels.txt`, or `--channels-file`) gives names and modes to known frequencies. It is read at every start; rfscan writes it only when you name a channel on the activity screen. One channel per line, `#` starts a comment:

```
# rfscan: channels
145.500 "CALLING"
118.100 am bw=10k "TOWER"
145.600 fm "REPEATER"
438.800 digi "DMR REPEATER"
```

Line syntax: `FREQUENCY_MHz [am|fm|digi] [bw=WIDTH] ["name"]`; the mode and `bw=` in any order before the name, everything except the frequency optional. Names may contain any characters (UTF-8). A line that cannot be read gives a warning with its number and is skipped.

- The name is shown when the scanner stops on the frequency (TUI, web GUI, console, history, activity, `--report`), both when scanning a range and channels. Unknown frequencies show no name.
- The mode of the file applies also while scanning a range: a known `digi` frequency in an NFM range is handled as `digi`. Precedence: the mode in `--scan-channels F:MODE`, then the file, then the range suffix, then `--mode`. The `bw=` of the file sets the demodulator width.
- In range mode an entry matches the nearest grid channel within half a step, and rfscan listens on that grid channel. An entry farther than 1% of the step from the grid (e.g. `433.450` in `433.420M-433.470M` with a 12.5 kHz step) gives a warning at startup: choose the range start or step so that the grid includes it.

## Exclusions

Frequencies you do not want to stop on (interference, digital data, constant carriers) go to the exclusion file (default `~/.config/rfscan/rfscan-exclude.txt`, or `--exclude-file`), one per line, with an optional note as a comment:

```
# rfscan: exclusions
144.000       # receiver carrier
433.920       # data bursts
```

In the TUI `x` excludes the current frequency (a popup asks for an optional note), `X` also the adjacent channels (`--exclude-wide N` on each side, default 2), `u` undoes the last exclusion; the file is written at once. `Tab` in the popup makes it a timed exclusion for 10, 30 or 60 minutes instead: it is not written to the file, ends by itself and is gone after a restart. The `e` screen lists the exclusions (the timed ones first, with the minutes left), `d` removes one and `s` keeps a timed one for good (written to the file with its note). `--exclude F,...` adds temporary exclusions for this run only. An exclusion applies to the frequency in every range and channel list.

## Activity log

rfscan counts how many seconds of audio it played on each frequency (only while LOCKED, not in the pause). The log is `~/.config/rfscan/rfscan-activity.txt` (or `--activity-file`), cumulative across runs, saved every 30 s and at exit:

```
145.500:140.4s hits=7 last=2026-10-13T14:02:11 mode=nfm
438.800:12.0s hits=2 last=2026-10-13T13:40:03 mode=digi
```

`rfscan --report` prints it as a table sorted by time (frequency, mode, name, time, hits, last). The activity screen (`a`) in the TUI and the Activity tab of the web GUI show it too; there you can sort it by any column, name a channel (saved to the channel file, also as `digi`), exclude it, delete one entry, or clear the whole log. `--activity-reset` moves the old log to `.bak` and starts from zero.

## Airband channel numbers (8.33 kHz)

In the airband (118-137 MHz) rfscan shows channel numbers as used by pilots, controllers, AIP and charts, not the actual transmit frequencies. On the 8.33 kHz raster they differ: the channel `118.035` is transmitted on 118.0333 MHz, `126.855` on 126.8500 MHz. The lock screen shows both:

```
118.035 MHz
frequency: 118.0333 MHz  (8.33 kHz channel)
```

The 8.33 kHz raster is the default. Channels that stay on the 25 kHz raster (e.g. distress and SAR) are marked in the channel file by writing their 25 kHz number:

```
121.500 am "GUARD"
123.100 am "SAR"
```

You can type channel numbers everywhere (channel file, exclusion file, `--exclude`, `--scan-channels 118.035M:am`): an airband value with at most three decimal places is a channel number, a value with four or more decimal places (`118.0333`) is an actual frequency. Entries added from the TUI are written as channel numbers.

## Audio

The audio chain: demodulation, FM de-emphasis (`--deemph`, default 75 µs, `off`), a band-pass filter (`--audio-band`, default 300-3000 Hz, removes subaudible tones), AGC (`--agc on|off`, target levels `--agc-target` for NFM and `--agc-target-am` for AM, maximum gain `--agc-max-gain`), speech noise reduction, a limiter and short fades at the start and end of a transmission. `--audio-device NAME` chooses the output (the list is in `--list-devices`), `--local-audio off` turns the local output off (e.g. a computer without a sound card, listening only in the web GUI).

**Noise reduction.** The analog audio (AM and NFM) goes through speech noise reduction (RNNoise, through the `nnnoiseless` crate). It is on by default and works best on AM, where there is more noise than on NFM. Digital (`digi`) channels never go through it.

| Control | Option | TUI key |
|---|---|---|
| on / off | `--nr rnnoise` / `--nr off` | `n` |
| strength | `--nr-strength PERCENT` (0-100, default 60) | `<` / `>` (or `,` / `.`), steps of 10% |

The strength mixes the denoised audio with the original: 0% is half of each, 100% the denoised audio only. The reduction delays the audio by 10 ms.

## Digital voice through DSD-FME

rfscan does not decode digital signals. On channels in `digi` mode it sends the raw FM discriminator to DSD-FME, which decodes the voice and sends it back to rfscan over UDP; rfscan plays it through its own audio output (and the web GUI), mixed with the analog channels. The noise reduction does not apply to the decoded voice.

A `digi` channel is marked in the channel file (`438.800 digi "DMR REPEATER"`) or on the command line: `--scan-channels 438.8M:digi`, `--scan-range 438M-439M:digi`, `--mode digi`. On the activity screen `[l]` and then `[Tab]` marks the selected channel as `digi` in the channel file (the name is optional then); the change applies at once. Without `--digi` or `--digi-tcp`, `digi` channels from the channel file are skipped, and an explicit `digi` on the command line is an error.

### DSD-FME started by rfscan (`--digi`)

```
rfscan --scan-range 438M-439M --digi --dsd-fme C:\Radio\dsd-fme\dsd-fme.exe
```

rfscan starts DSD-FME in the background (without a window) and stops it on exit. Without `--dsd-fme` it looks for `dsd-fme` in `PATH`; if it is not found, rfscan exits with a message. The decoder is DSD-FME's auto detection (`-fa`: DMR, P25 phase 1, D-STAR, X2-TDMA, YSF); other systems and DSD-FME options go to `--dsd-fme-args`, e.g. `--dsd-fme-args "-fn"` for NXDN96 or `--dsd-fme-args "-fa -G group.csv"` for talkgroup names. The input, the output and the event log are always set by rfscan.

On Linux DSD-FME is built from source with its vocoder library mbelib (read the patent notice in the [mbelib README](https://github.com/lwvmobile/mbelib#readme) first). On Debian 13, including a Raspberry Pi:

```
sudo apt install cmake libpulse-dev libsndfile1-dev libfftw3-dev liblapack-dev libusb-1.0-0-dev librtlsdr-dev libncurses-dev libcodec2-dev
git clone https://github.com/lwvmobile/mbelib
cd mbelib && git checkout ambe_tones && cmake -S . -B build && cmake --build build -j4 && sudo cmake --install build && sudo ldconfig && cd ..
git clone https://github.com/lwvmobile/dsd-fme
cd dsd-fme && cmake -S . -B build && cmake --build build -j4 && sudo cmake --install build && cd ..
```

This installs `dsd-fme` to `/usr/local/bin`, where rfscan finds it without `--dsd-fme`.

DSD-FME's console output goes to `~/.config/rfscan/dsd-fme.log`; while the scanner is on a `digi` channel, the TUI shows its last lines in a window over the detection history (e.g. ` SLOT 1 TGT=1234 SRC=567 Group` for DMR). The event log of finished calls is in `~/.config/rfscan/dsd-fme-events.log`. `--dsd-fme-logs DIR` puts both files in another (existing) directory: needed when several rfscan instances run with `--digi`, e.g. one configuration file per receiver with its own `dsd-fme-logs`, `activity-file` and `device`. If DSD-FME exits during scanning, rfscan restarts it (at most 3 times per minute); the `[ DIGI ]` line on the lock screen and the `digi` statistic show its state and whether its voice is playing.

### DSD-FME started by hand (`--digi-tcp`)

For DSD-FME on another computer, or with your own settings. **Start order: rfscan first, then DSD-FME.**

1. rfscan with the digi output (listens from startup, a port alone means `127.0.0.1`) and, for the voice, a UDP port:

   ```
   rfscan --scan-range 438M-439M --digi-tcp 7355 --digi-audio-udp 23456
   ```

2. DSD-FME as the client, sending its voice to rfscan:

   ```
   dsd-fme -fa -i tcp:127.0.0.1:7355 -o udp:127.0.0.1:23456
   ```

The discriminator stream is PCM s16le, mono, 48 kHz, continuous (about 96,000 B/s): silence outside `digi` channels, the discriminator during a transmission on a `digi` channel. Disconnecting and reconnecting DSD-FME does not interrupt scanning. The voice sample rate is estimated from the stream; give it as `--digi-audio-udp 23456:8000` if needed. Without `--digi-audio-udp`, DSD-FME plays the voice itself. The stream level is changed with `--digi-gain dB`.

## TUI

When the output is a terminal, rfscan runs with the TUI. The layout adapts to the window size: from about 100x30 the frequency is shown in a large font, down to 80x24 in a compact layout. The scanning screen shows the range, the position in it, the mini-spectrum of the current window and the statistics; the lock screen shows the frequency, the name, the SNR bar with the thresholds and the measurements; below is the detection history.

| Key | Action |
|---|---|
| `q`, Ctrl+C | quit |
| `p` | pause scanning |
| space | skip the current signal once |
| `x` / `X` | exclude the current frequency (with adjacent channels), with an optional note; `Tab` in the popup: for good or for 10 / 30 / 60 min |
| `u` | undo the last exclusion |
| `+` / `-` | squelch threshold |
| `m` | mute |
| `n`, `<` / `>` | noise reduction on/off, strength |
| `a` | activity screen (`←` / `→` sort column, `s` reverse the order, `l` name a channel, `Tab` in the popup: `digi`, `x` exclude, `d` delete the entry, `c` clear) |
| `e` | exclusions screen (`d` remove, `s` keep a timed exclusion for good) |
| `Tab` | select a detection history row (arrows move, `l` name the channel, `x` / `X` exclude it (with adjacent channels), `Tab` / `Esc` back) |

## Console mode and log file

With `--headless` (or when the output is redirected to a file) rfscan writes events to the console instead of the TUI: each event line starts with the local time, e.g.

```
15:38:12  LOCKED    145.500 MHz  NFM   RSSI  -61.2 dBFS  noise  -88.0 dBFS  SNR  26.8 dB  (window B1)  "CALLING"
15:38:13  locked    145.500 MHz  RSSI  -62.0 dBFS  noise  -88.1 dBFS  SNR  26.1 dB  AGC +4.0 dB ...
15:38:16  HANG      145.500 MHz  SNR   9.1 dB  (signal 3.9 s, max RSSI -60.9 dBFS, max SNR 27.4 dB, pause 2.000 s)
15:38:18  SCANNING  (pause of 2.010 s elapsed, resuming from window B2)
```

`locked` is repeated every second during a transmission, `status` (scan speed, overflows) right after the start and then every 30 s, `rejected` marks hits rejected by the confirmation. On a terminal the lines are colored (LOCKED green, HANG yellow, rejected hits and the status dimmed); in a redirected output and in the log file there are no colors.

`--log FILE` appends a log with the date and time of every line: with `--log-level info` (default) the transmissions and the session start and end, with `--log-level debug` everything, including the window list of the scan plan. `--duration SECONDS` ends the run after the given time.

## Web GUI

`--web-gui [HOST:]PORT` starts a web page for watching, hearing and controlling the scanner from a phone or a desktop browser, e.g. for a receiver without a monitor near the antenna:

```
rfscan --scan-range 144M-146M --web-gui 8080            # this computer only: http://127.0.0.1:8080/
rfscan --scan-range 144M-146M --web-gui 0.0.0.0:8080    # the home network
```

rfscan prints the link at startup. The page shows the frequency or channel number with its name, the state, the signal level, and has buttons for pause, skip, exclude (with a note, for good or for 10 / 30 / 60 minutes), undo, squelch, noise reduction and audio; the history and the activity log (tapping an entry names the channel, sets the `digi` mode or excludes it; an activity entry can also be deleted, and the activity log is sorted by tapping a column header) and the exclusions (a timed one shows the minutes left and can be kept for good) are separate tabs. It has a dark and a light theme. It works with the TUI or with `--headless`, and several browsers can be connected at once, each with full control.

The audio is sent to the browser as 8 kHz μ-law (64 kbit/s, 300-3000 Hz) after **Audio** is tapped (browsers need a tap before playing sound); the delay is about 0.2-0.3 s more than the local audio. The local audio keeps playing: **Mute PC** mutes it (like `m` in the TUI), and `--local-audio off` turns the local output off completely (the web page still gets the audio).

**Security.** The web GUI is meant for a home network or a VPN only. It uses plain HTTP (no HTTPS, no encryption) and has no advanced security: do not expose it to the internet (no port forwarding on the router). On an address other than `127.0.0.1` the page needs a token in the link (`http://IP:PORT/?t=TOKEN`): a random one at every start, or a fixed one from `--web-token TOKEN` (e.g. in `rfscan.conf`, so a bookmark on the phone keeps working). Windows may ask to allow rfscan through the firewall the first time it listens on the network.

**Home screen.** Added to the home screen of a phone, the page opens as an app without the address bar: on iOS also over plain HTTP, on Android only over HTTPS (e.g. through a reverse proxy) or on `localhost`; otherwise Android opens it as a browser tab. In Chrome on Android, with rfscan behind a reverse proxy with HTTPS, choose **Install app** (not **Create shortcut**): the installed app opens without the address bar and with the colors of the page in the system bars. The shortcut keeps the link with the token, so use a fixed `--web-token`.

**Behind a reverse proxy** (e.g. nginx with HTTPS and its own login): the page uses a WebSocket at `/ws`, so the proxy must pass WebSocket connections (in nginx `proxy_http_version 1.1; proxy_set_header Upgrade $http_upgrade; proxy_set_header Connection "upgrade";`), and the page must be served at the root of a host name (e.g. `https://rfscan.example.org/`), not under a sub-path. With the proxy on the same computer, run rfscan with `--web-gui 127.0.0.1:PORT`: no token is needed and the proxy controls the access. Never leave such a proxy without its login (e.g. HTTP basic authentication): the page controls the scanner and edits the exclusion and channel files, and behind a proxy on `127.0.0.1` no token protects it.

## IQ recording

```
rfscan --scan-channels 145.5M --record-iq C:\Radio\iq --sample-rate 3M
```

records the raw IQ stream around one channel to WAV files in the given (existing) directory until Ctrl+C or `--duration`. Only one frequency, and only with `--scan-channels`; nothing else runs meanwhile (no detection, audio or TUI). The receiver is tuned as for scanning that channel, with the tuner frequency next to it, so the channel is a little off the center of the recording and the DC spike is outside it.

The files open in SDR#, HDSDR, SDR++ and similar programs: I and Q are the left and the right channel in the receiver's own sample format (16-bit on an Airspy, 8-bit on an RTL-SDR), the center frequency and the sample rate are inside the file (the SDR#/HDSDR `auxi` chunk) and in its name, e.g. `rfscan_20261020_144341Z_145481250Hz_IQ.wav`. A WAV file holds at most 4 GB, so a long recording continues in further files. Mind the size: 6 MSps in 16 bits is 24 MB/s (4 GB in under 3 minutes); a lower `--sample-rate` gives smaller files. Lost samples (overflows) are reported at the end.

## Configuration file

Options you give at every start can be kept in `rfscan.conf`. `rfscan --gen-conf` writes it with all options, each with its description and its default commented out (`rfscan --gen-conf FILE` writes it elsewhere; the directory must exist). An existing file is overwritten. Uncomment and change what you need:

```toml
gain = "agc=true,VGA=7"
settle = 20
scan-range = ["144M-146M", "430M-440M:25k"]
squelch-snr = 12
digi = true
dsd-fme = 'C:\Radio\dsd-fme\dsd-fme.exe'
dsd-fme-args = "-fa -G group.csv"
web-token = "a-long-secret"
```

Keys are the long options without `--`, values use the command line syntax, options given multiple times are lists, switches are `true`/`false`. Windows paths go in single quotes (no escapes). Command line options take precedence: an option given on the command line replaces the file value completely (also a list), and so does an option it excludes (`--scan-channels` drops the file's `scan-range`). `--config FILE` loads another file (e.g. one per receiver), `--no-config` none. One-time actions (`--list-devices`, `--report`, `--bench`, `--calibrate`, `--record-iq`, `--activity-reset`, `--power`, `--duration`, `--gen-conf`), the choice of the file itself (`--config`, `--no-config`) and the temporary `--exclude` are allowed only on the command line. In `dsd-fme-args` you can give any DSD-FME options (e.g. a decryption key); rfscan always adds its own input, output and event log after them.

## Files

rfscan keeps its files in `~/.config/rfscan` (on Windows `C:\Users\USER\.config\rfscan`; on Linux `$XDG_CONFIG_HOME/rfscan` when `XDG_CONFIG_HOME` is set). The directory is created when the first file is saved, and the paths are printed at startup.

| File | Contents | Option |
|---|---|---|
| `rfscan-channels.txt` | channel names, modes and bandwidths | `--channels-file` |
| `rfscan-exclude.txt` | excluded frequencies with notes | `--exclude-file` |
| `rfscan-activity.txt` | activity log (`rfscan --report`) | `--activity-file` |
| `rfscan.conf` | options used at every start | `--config` |
| `dsd-fme.log`, `dsd-fme-events.log` | DSD-FME console and call log (with `--digi`) | `--dsd-fme-logs` |
