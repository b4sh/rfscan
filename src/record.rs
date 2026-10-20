//! Raw IQ recording of one channel to WAV files (`--record-iq`).
//!
//! I is the left and Q the right channel; the samples stay in the native stream format (CS16 as
//! 16-bit PCM, CU8 as 8-bit PCM, CS8 shifted to unsigned 8-bit PCM, CF32 as 32-bit float). The
//! metadata are inside the file: the `auxi` chunk of SDR#/HDSDR (start and stop time, center
//! frequency, sample rate, the name of the next file), and the file name follows the SDR#
//! convention `rfscan_YYYYMMDD_HHMMSSZ_<Hz>Hz_IQ.wav`. A WAV file holds at most 4 GB, so the
//! recording continues in a new file.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::device::{IqBuffer, SampleFormat};

/// Size of the `auxi` chunk body (HDSDR layout).
const AUXI_LEN: u32 = 164;
/// Length of the next file name field in `auxi`.
const NEXT_NAME_LEN: usize = 96;
/// Header bytes before the samples: RIFF (12), fmt (8 + 16), auxi (8 + 164), data (8).
const HEADER_LEN: u64 = 12 + 24 + 8 + AUXI_LEN as u64 + 8;
/// Write buffer of the file.
const WRITE_BUFFER: usize = 1 << 20;

/// WAV sample layout of a stream format: format tag (1 PCM, 3 float) and bits per value.
fn wav_format(f: SampleFormat) -> (u16, u16) {
    match f {
        SampleFormat::Cu8 | SampleFormat::Cs8 => (1, 8),
        SampleFormat::Cs16 => (1, 16),
        SampleFormat::Cf32 => (3, 32),
    }
}

/// Bytes of one IQ sample (both channels) in the WAV file.
pub fn bytes_per_sample(f: SampleFormat) -> usize {
    2 * usize::from(wav_format(f).1 / 8)
}

/// Recording to WAV files in a directory.
pub struct Recorder {
    dir: PathBuf,
    center_hz: u64,
    rate: u32,
    format: SampleFormat,
    /// Largest number of sample bytes in one file.
    limit: u64,
    file: Option<Open>,
    /// Native samples as WAV bytes (reused).
    bytes: Vec<u8>,
    /// All files of the recording and the total sample bytes.
    pub files: Vec<PathBuf>,
    pub total_bytes: u64,
}

/// The file being written.
struct Open {
    out: BufWriter<File>,
    path: PathBuf,
    start: DateTime<Utc>,
    data: u64,
}

impl Recorder {
    pub fn new(dir: &Path, center_hz: u64, rate: f64, format: SampleFormat) -> Self {
        let (_, bits) = wav_format(format);
        let frame = 2 * u64::from(bits / 8);
        Recorder {
            dir: dir.to_path_buf(),
            center_hz,
            rate: rate.round() as u32,
            format,
            // The RIFF size (u32) counts everything after its own field.
            limit: (u64::from(u32::MAX) - HEADER_LEN) / frame * frame,
            file: None,
            bytes: Vec::new(),
            files: Vec::new(),
            total_bytes: 0,
        }
    }

    /// The file name for a recording started at `t`.
    fn file_name(&self, t: DateTime<Utc>) -> String {
        format!(
            "rfscan_{}_{}Hz_IQ.wav",
            t.format("%Y%m%d_%H%M%SZ"),
            self.center_hz
        )
    }

    /// The path of a file started at `start`; a part started within the same second as an
    /// existing file gets a number.
    fn new_path(&self, start: DateTime<Utc>) -> PathBuf {
        let name = self.file_name(start);
        let mut path = self.dir.join(&name);
        let mut n = 1;
        while path.exists() {
            n += 1;
            path = self
                .dir
                .join(name.replace("_IQ.wav", &format!("_{n}_IQ.wav")));
        }
        path
    }

    /// Starts a new file at `path`.
    fn open(&mut self, path: PathBuf, start: DateTime<Utc>) -> Result<(), String> {
        let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut out = BufWriter::with_capacity(WRITE_BUFFER, file);
        let header = self.header(start, start, 0, "");
        out.write_all(&header)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.files.push(path.clone());
        self.file = Some(Open {
            out,
            path,
            start,
            data: 0,
        });
        Ok(())
    }

    /// Writes `n` samples of `buf`. Returns the path of a new file when one was started.
    pub fn write(&mut self, buf: &IqBuffer, n: usize) -> Result<Option<PathBuf>, String> {
        self.bytes.clear();
        to_wav_bytes(buf, n, &mut self.bytes);
        let mut started = None;
        let mut pos = 0;
        while pos < self.bytes.len() {
            if self.file.as_ref().is_none_or(|f| f.data >= self.limit) {
                // The previous file gets the name of the next one in its metadata.
                let start = Utc::now();
                let path = self.new_path(start);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                self.close_with(&name)?;
                self.open(path.clone(), start)?;
                started = Some(path);
            }
            let Some(f) = self.file.as_mut() else {
                return Err("no recording file".into());
            };
            let room = (self.limit - f.data) as usize;
            let end = self.bytes.len().min(pos + room);
            f.out
                .write_all(&self.bytes[pos..end])
                .map_err(|e| format!("{}: {e}", f.path.display()))?;
            f.data += (end - pos) as u64;
            self.total_bytes += (end - pos) as u64;
            pos = end;
        }
        Ok(started)
    }

    /// Finishes the recording: the sizes and the stop time in the last file.
    pub fn finish(&mut self) -> Result<(), String> {
        self.close_with("")
    }

    /// Closes the current file (if any) with `next` as the name of the next file.
    fn close_with(&mut self, next: &str) -> Result<(), String> {
        let Some(mut f) = self.file.take() else {
            return Ok(());
        };
        let err = |e: std::io::Error| format!("{}: {e}", f.path.display());
        let header = self.header(f.start, Utc::now(), f.data, next);
        f.out.flush().map_err(err)?;
        let file = f.out.get_mut();
        file.seek(SeekFrom::Start(0)).map_err(err)?;
        file.write_all(&header).map_err(err)?;
        file.flush().map_err(err)?;
        Ok(())
    }

    /// The WAV header: RIFF, fmt, auxi and the data chunk header.
    fn header(&self, start: DateTime<Utc>, stop: DateTime<Utc>, data: u64, next: &str) -> Vec<u8> {
        let (tag, bits) = wav_format(self.format);
        let block = (2 * bits / 8) as u32;
        let mut h = Vec::with_capacity(HEADER_LEN as usize);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&((HEADER_LEN - 8 + data) as u32).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&tag.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&self.rate.to_le_bytes());
        h.extend_from_slice(&(self.rate * block).to_le_bytes());
        h.extend_from_slice(&(block as u16).to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        h.extend_from_slice(b"auxi");
        h.extend_from_slice(&AUXI_LEN.to_le_bytes());
        system_time(&mut h, start);
        system_time(&mut h, stop);
        // Center frequency, A/D rate, IF frequency, bandwidth, IQ offset, 4 unused fields.
        let center = u32::try_from(self.center_hz).unwrap_or(u32::MAX);
        for v in [center, self.rate, 0, self.rate, 0, 0, 0, 0, 0] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        let mut name = [0u8; NEXT_NAME_LEN];
        for (d, s) in name.iter_mut().zip(next.bytes().take(NEXT_NAME_LEN - 1)) {
            *d = s;
        }
        h.extend_from_slice(&name);
        h.extend_from_slice(b"data");
        h.extend_from_slice(&(data as u32).to_le_bytes());
        debug_assert_eq!(h.len() as u64, HEADER_LEN);
        h
    }
}

/// Windows SYSTEMTIME: year, month, day of the week, day, hour, minute, second, millisecond.
fn system_time(h: &mut Vec<u8>, t: DateTime<Utc>) {
    let fields = [
        t.year() as u16,
        t.month() as u16,
        t.weekday().num_days_from_sunday() as u16,
        t.day() as u16,
        t.hour() as u16,
        t.minute() as u16,
        t.second() as u16,
        (t.timestamp_subsec_millis() % 1000) as u16,
    ];
    for v in fields {
        h.extend_from_slice(&v.to_le_bytes());
    }
}

/// The first `n` samples as WAV bytes: I, Q interleaved, little endian.
fn to_wav_bytes(buf: &IqBuffer, n: usize, out: &mut Vec<u8>) {
    match buf {
        IqBuffer::Cu8(b) => {
            for s in &b[..n] {
                out.extend_from_slice(&[s.re, s.im]);
            }
        }
        // 8-bit WAV is unsigned: zero at 128.
        IqBuffer::Cs8(b) => {
            for s in &b[..n] {
                out.extend_from_slice(&[(s.re as u8) ^ 0x80, (s.im as u8) ^ 0x80]);
            }
        }
        IqBuffer::Cs16(b) => {
            for s in &b[..n] {
                out.extend_from_slice(&s.re.to_le_bytes());
                out.extend_from_slice(&s.im.to_le_bytes());
            }
        }
        IqBuffer::Cf32(b) => {
            for s in &b[..n] {
                out.extend_from_slice(&s.re.to_le_bytes());
                out.extend_from_slice(&s.im.to_le_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex;

    /// Offsets in the header: the auxi body and the data size.
    const AUXI_AT: usize = 12 + 24 + 8;
    const DATA_SIZE_AT: usize = HEADER_LEN as usize - 4;

    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    #[test]
    fn wav_files_with_auxi_and_split() {
        let dir = std::env::temp_dir().join(format!("rfscan-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = Recorder::new(&dir, 145_481_250, 6e6, SampleFormat::Cs16);
        // A small limit to check the split: 3 samples (12 bytes) per file.
        rec.limit = 12;
        let buf = IqBuffer::Cs16(
            (0..5)
                .map(|i| Complex::new(i as i16, -(i as i16)))
                .collect(),
        );
        assert!(rec.write(&buf, 5).unwrap().is_some());
        rec.finish().unwrap();
        assert_eq!(rec.files.len(), 2);
        assert_eq!(rec.total_bytes, 20);
        let first = std::fs::read(&rec.files[0]).unwrap();
        let second = std::fs::read(&rec.files[1]).unwrap();
        let name = rec.files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            name.starts_with("rfscan_") && name.ends_with("_145481250Hz_IQ.wav"),
            "{name}"
        );
        // RIFF size, format, rate, the auxi center frequency and the next file name.
        assert_eq!(&first[..4], b"RIFF");
        assert_eq!(u32_at(&first, 4) as usize, first.len() - 8);
        assert_eq!(u32_at(&first, 24), 6_000_000);
        let auxi = AUXI_AT;
        assert_eq!(u32_at(&first, auxi + 32), 145_481_250);
        let next = rec.files[1]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(&first[auxi + 68..auxi + 68 + next.len()], next.as_bytes());
        assert_eq!(u32_at(&first, DATA_SIZE_AT), 12);
        assert_eq!(u32_at(&second, DATA_SIZE_AT), 8);
        // Samples: I, Q interleaved, little endian (the 4th sample opens the second file).
        let data = HEADER_LEN as usize;
        assert_eq!(&first[data + 4..data + 8], &[1, 0, 0xFF, 0xFF]);
        assert_eq!(&second[data..data + 4], &[3, 0, 0xFD, 0xFF]);
        // The last file names no next one.
        assert_eq!(second[auxi + 68], 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cs8_becomes_unsigned() {
        let mut out = Vec::new();
        to_wav_bytes(
            &IqBuffer::Cs8(vec![Complex::new(0, -128), Complex::new(127, -1)]),
            2,
            &mut out,
        );
        assert_eq!(out, [128, 0, 255, 127]);
    }
}
