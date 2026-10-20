//! Aeronautical VHF channel numbers (ICAO 8.33 kHz channel naming).
//!
//! In the airband (117.975-137.000 MHz) every 25 kHz block starting at `f0` holds three
//! 8.33 kHz channels with the actual frequencies `f0`, `f0 + 25/3 kHz` and `f0 + 50/3 kHz`.
//! Pilots and controllers use channel numbers with three decimal places that differ from the
//! actual frequency, so an 8.33 kHz channel can be told apart from a 25 kHz channel:
//!
//! | actual frequency | 8.33 kHz channel | 25 kHz channel |
//! |---|---|---|
//! | f0 (118.0000) | 118.005 | 118.000 |
//! | f0 + 8.333 kHz (118.0083) | 118.010 | - |
//! | f0 + 16.667 kHz (118.0167) | 118.015 | - |
//!
//! Channel numbers ending in 20, 45, 70 and 95 do not exist. In Europe the 8.33 kHz raster is
//! the norm; channels that stay on the 25 kHz raster (e.g. distress and SAR) are marked in the
//! channel file by writing them with their 25 kHz number.

/// Airband limits (Hz).
const LOW: u64 = 117_975_000;
const HIGH: u64 = 137_000_000;
const BLOCK: u64 = 25_000;
/// Tolerance for recognizing a frequency on the 25/3 kHz grid (Hz).
const GRID_TOLERANCE: f64 = 100.0;

pub fn is_airband(hz: u64) -> bool {
    (LOW..=HIGH).contains(&hz)
}

/// Position on the 8.33 kHz grid: the 25 kHz block start and the slot 0, 1 or 2.
fn slot(hz: u64) -> Option<(u64, u64)> {
    if !is_airband(hz) {
        return None;
    }
    let f0 = hz / BLOCK * BLOCK;
    let offset = (hz - f0) as f64;
    // The third slot may round up into the next block's start: take the nearest slot.
    (0..=3u64)
        .map(|k| (k, (offset - k as f64 * BLOCK as f64 / 3.0).abs()))
        .find(|&(_, d)| d <= GRID_TOLERANCE)
        .map(|(k, _)| if k == 3 { (f0 + BLOCK, 0) } else { (f0, k) })
}

/// Channel number of the actual frequency `hz` (e.g. `118.035` for 118.0333 MHz), or `None`
/// outside the airband or off the 25/3 kHz grid. `raster25`: the block start is a 25 kHz
/// channel (its number equals the frequency).
pub fn channel_number(hz: u64, raster25: bool) -> Option<String> {
    let (f0, k) = slot(hz)?;
    let name = if k == 0 && raster25 {
        f0
    } else {
        f0 + 5_000 * (k + 1)
    };
    Some(format!("{:.3}", name as f64 / 1e6))
}

/// Text for display: the channel number in the airband, otherwise the frequency in MHz.
pub fn display(hz: u64, raster25: bool) -> String {
    channel_number(hz, raster25).unwrap_or_else(|| crate::activity::format_mhz(hz))
}

/// Actual frequency with 4 decimal places, truncated like in licences (118.0166 MHz).
pub fn actual(hz: u64) -> String {
    let tenth_khz = hz / 100;
    format!("{}.{:04}", tenth_khz / 10_000, tenth_khz % 10_000)
}

/// Reads a frequency written in MHz (`text` is the number as typed, `hz` its value in Hz). In
/// the airband a value with at most three decimal places is a channel number and is converted
/// to the actual frequency; the result says whether it is a 25 kHz channel. A value with more
/// decimal places, or outside the airband, is an actual frequency.
pub fn from_text(text: &str, hz: u64) -> Result<(u64, bool), String> {
    let decimals = text.split_once('.').map_or(0, |(_, d)| {
        d.chars().take_while(char::is_ascii_digit).count()
    });
    if decimals > 3 || !is_airband(hz) {
        return Ok((hz, false));
    }
    let f0 = hz / BLOCK * BLOCK;
    match hz - f0 {
        0 => Ok((f0, true)),
        5_000 => Ok((f0, false)),
        10_000 => Ok((f0 + (BLOCK as f64 / 3.0).round() as u64, false)),
        15_000 => Ok((f0 + (2.0 * BLOCK as f64 / 3.0).round() as u64, false)),
        _ => Err(format!(
            "'{text}': not an airband channel number (8.33 kHz channels end in 05, 10, 15, 30, 35, 40, 55, 60, 65, 80, 85 or 90; 25 kHz channels in 00, 25, 50 or 75)"
        )),
    }
}

/// Text for writing a frequency to a file: the channel number in the airband (as in AIP and on
/// charts), otherwise the frequency in MHz.
pub fn file_text(hz: u64, raster25: bool) -> String {
    display(hz, raster25)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_numbers_follow_icao() {
        // The examples from the document: 118.0333 -> 118.035, 126.8500 -> 126.855.
        assert_eq!(
            channel_number(118_033_333, false).as_deref(),
            Some("118.035")
        );
        assert_eq!(
            channel_number(126_850_000, false).as_deref(),
            Some("126.855")
        );
        assert_eq!(
            channel_number(118_000_000, false).as_deref(),
            Some("118.005")
        );
        assert_eq!(
            channel_number(118_008_333, false).as_deref(),
            Some("118.010")
        );
        assert_eq!(
            channel_number(118_016_667, false).as_deref(),
            Some("118.015")
        );
        assert_eq!(
            channel_number(118_091_667, false).as_deref(),
            Some("118.090")
        );
        // A 25 kHz channel keeps its frequency as the number.
        assert_eq!(
            channel_number(121_500_000, true).as_deref(),
            Some("121.500")
        );
        // Outside the airband or off the grid: no channel number.
        assert_eq!(channel_number(145_500_000, false), None);
        assert_eq!(channel_number(118_004_000, false), None);
        assert_eq!(display(145_500_000, false), "145.500");
        assert_eq!(actual(118_016_667), "118.0166");
        assert_eq!(actual(126_850_000), "126.8500");
    }

    #[test]
    fn channel_numbers_are_read_back() {
        assert_eq!(from_text("118.005", 118_005_000), Ok((118_000_000, false)));
        assert_eq!(from_text("118.010", 118_010_000), Ok((118_008_333, false)));
        assert_eq!(from_text("118.035", 118_035_000), Ok((118_033_333, false)));
        assert_eq!(from_text("126.855", 126_855_000), Ok((126_850_000, false)));
        assert_eq!(from_text("121.5", 121_500_000), Ok((121_500_000, true)));
        // Four decimal places: the actual frequency as written.
        assert_eq!(from_text("118.0083", 118_008_300), Ok((118_008_300, false)));
        // Outside the airband nothing changes.
        assert_eq!(from_text("145.005", 145_005_000), Ok((145_005_000, false)));
        assert!(from_text("118.020", 118_020_000).is_err());
        // Every channel number maps back to itself.
        for hz in (117_975_000..=136_975_000).step_by(25_000) {
            for k in 0..3u64 {
                let f = hz + (k as f64 * 25_000.0 / 3.0).round() as u64;
                let name = channel_number(f, false).unwrap();
                let v = (name.parse::<f64>().unwrap() * 1e6).round() as u64;
                assert_eq!(from_text(&name, v), Ok((f, false)), "{name}");
            }
        }
    }
}
