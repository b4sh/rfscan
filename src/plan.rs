//! Division of the scanned range into windows (one window = one tuner setting) following the
//! channel grid, protecting channels from the tuner's DC component.
//!
//! Rules (hardware independent):
//! - In one pass each channel belongs to at most one window, so it is not measured twice in
//!   consecutive windows.
//! - The tuner LO always sits on the boundary of two channels, never in the middle of a channel.
//! - `guard` channels on each side of the LO (the DC zone) are not measured in that window.
//!   Such channels are measured by the second pass (B), whose windows are shifted, and passes
//!   A and B alternate. With `guard = 0` there is a single pass.
//! - A block of channels that fits in half of the usable band together with the DC zone gets
//!   its LO next to it, so it loses no channel.

/// Channel grid step as a fraction `num/den` Hz (8.33 kHz = exactly 25000/3 Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub num: u64,
    pub den: u64,
}

impl Step {
    /// Step from whole Hz. Steps near 8.33 kHz mean the 25/3 kHz airband grid.
    pub fn from_hz(hz: u64) -> Self {
        if (3 * hz).abs_diff(25_000) <= 10 {
            Step {
                num: 25_000,
                den: 3,
            }
        } else {
            Step { num: hz, den: 1 }
        }
    }

    pub fn hz(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Frequency `start + half·step/2` rounded to whole Hz (integer arithmetic).
    fn at_half(self, start: u64, half: u64) -> u64 {
        start + (half * self.num + self.den) / (2 * self.den)
    }
}

/// One window: the tuner center frequency (LO) and its channels (Hz).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub center: u64,
    /// Grid index of the block's first channel and the number of channels in the block.
    pub first_index: u64,
    pub channels: u64,
    /// Centers of the block's first and last channel.
    pub first_channel: u64,
    pub last_channel: u64,
    /// Number of measured channels (the block without the DC zone).
    pub measured: u64,
    /// DC zone in the block: indices of the first and last unmeasured channel.
    pub guard: Option<(u64, u64)>,
}

impl Window {
    /// Whether the channel with index `i` (from this window's block) is measured.
    pub fn is_measured(&self, i: u64) -> bool {
        !self.guard.is_some_and(|(g0, g1)| i >= g0 && i <= g1)
    }
}

/// Plan of a single range: one or two alternating passes (channel indices counted from the
/// start of the range).
#[derive(Debug)]
pub struct RangePlan {
    pub passes: Vec<Vec<Window>>,
    pub start: u64,
    pub step: Step,
    pub total_channels: u64,
}

impl RangePlan {
    /// Center of the channel with index `i`: start + round(i·step).
    pub fn channel(&self, i: u64) -> u64 {
        self.step.at_half(self.start, 2 * i)
    }
}

/// A block of channels (grid indices) and the LO position in half steps from the first grid
/// channel.
#[derive(Clone, Copy)]
struct Block {
    first: u64,
    count: u64,
    /// LO = start + lo_half · step / 2. Always odd, i.e. a channel boundary.
    lo_half: u64,
    /// DC zone in the block (inclusive indices).
    guard: Option<(u64, u64)>,
}

fn make_block(first: u64, count: u64, guard: u64, per_window: u64) -> Block {
    let last = first + count - 1;
    if guard > 0 && count + guard <= per_window / 2 {
        // Small block: LO to the right of the block, past the DC zone. Distance from the LO to
        // the left edge of the first channel = (count + guard)·step <= usable band / 2.
        return Block {
            first,
            count,
            lo_half: 2 * last + 1 + 2 * guard,
            guard: None,
        };
    }
    // LO on a channel boundary in the middle of the block (half a step to the right for an odd
    // number of channels).
    let lo_half = 2 * first + count - u64::from(count.is_multiple_of(2));
    let left_of_lo = (lo_half - 1) / 2;
    let guard_range = (guard > 0).then(|| {
        (
            (left_of_lo + 1).saturating_sub(guard).max(first),
            (left_of_lo + guard).min(last),
        )
    });
    Block {
        first,
        count,
        lo_half,
        guard: guard_range,
    }
}

/// Splits `total` channels into blocks: the first has `shift` channels (if
/// 0 < shift < per_window), the following ones `per_window` each.
fn build_pass(total: u64, per_window: u64, guard: u64, shift: u64) -> Vec<Block> {
    let mut out = Vec::new();
    let mut first = 0;
    let mut size = if shift > 0 && shift < per_window {
        shift
    } else {
        per_window
    };
    while first < total {
        let count = size.min(total - first);
        out.push(make_block(first, count, guard, per_window));
        first += count;
        size = per_window;
    }
    out
}

fn measured_mask(blocks: &[Block], total: u64) -> Vec<bool> {
    let mut m = vec![false; total as usize];
    for b in blocks {
        for i in b.first..b.first + b.count {
            let in_guard = b.guard.is_some_and(|(g0, g1)| i >= g0 && i <= g1);
            m[i as usize] = !in_guard;
        }
    }
    m
}

fn active_windows(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .filter(|b| b.count > b.guard.map_or(0, |(g0, g1)| g1 - g0 + 1))
        .count()
}

/// Builds the plan for channels `start + round(n·step)` (up to `stop` inclusive) and usable
/// band `usable_width` (Hz), with a DC zone of `guard` channels on each side of the LO.
pub fn range_plan(
    start: u64,
    stop: u64,
    step: Step,
    usable_width: u64,
    guard: u64,
) -> Result<RangePlan, String> {
    if step.num == 0 || step.den == 0 {
        return Err("the step must be positive".into());
    }
    if stop < start {
        return Err("range start must be lower than its end".into());
    }
    // An even number of channels per window, so the LO falls on a channel boundary.
    let per_window = (usable_width * step.den / step.num) & !1;
    if per_window < 2 {
        return Err(format!(
            "2 channels of {1} kHz do not fit in the usable band of {0} kHz",
            usable_width as f64 / 1e3,
            step.hz() / 1e3
        ));
    }
    if guard > 0 && per_window < 4 * guard + 2 {
        return Err(format!(
            "a DC zone of {guard} channel(s) on each side is too wide for a window with {per_window} channels"
        ));
    }
    let total = (stop - start) * step.den / step.num + 1;

    let pass_a = build_pass(total, per_window, guard, 0);
    let mut passes = vec![pass_a];
    let mask_a = measured_mask(&passes[0], total);

    if mask_a.iter().any(|&m| !m) {
        // Pass B: the shift with which every channel is measured in A or in B, with the fewest
        // windows, and on a tie the one closest to half a window.
        let mut best: Option<(usize, u64, Vec<Block>)> = None;
        for shift in 1..per_window {
            let pass_b = build_pass(total, per_window, guard, shift);
            let mask_b = measured_mask(&pass_b, total);
            if !mask_a.iter().zip(&mask_b).all(|(&a, &b)| a || b) {
                continue;
            }
            let windows = active_windows(&pass_b);
            let dist = shift.abs_diff(per_window / 2);
            let better = best
                .as_ref()
                .is_none_or(|(w, d, _)| windows < *w || (windows == *w && dist < *d));
            if better {
                best = Some((windows, dist, pass_b));
            }
        }
        match best {
            Some((_, _, pass_b)) => passes.push(pass_b),
            None => {
                return Err(format!(
                    "the DC zone channels ({guard} on each side) cannot be covered by a second pass"
                ));
            }
        }
    }

    let to_window = |b: &Block| {
        let guard_len = b.guard.map_or(0, |(g0, g1)| g1 - g0 + 1);
        Window {
            center: step.at_half(start, b.lo_half),
            first_index: b.first,
            channels: b.count,
            first_channel: step.at_half(start, 2 * b.first),
            last_channel: step.at_half(start, 2 * (b.first + b.count - 1)),
            measured: b.count - guard_len,
            guard: b.guard,
        }
    };
    let passes = passes
        .iter()
        .map(|p| {
            p.iter()
                .map(to_window)
                // A window without measured channels is not tuned.
                .filter(|w| w.measured > 0)
                .collect()
        })
        .collect();

    Ok(RangePlan {
        passes,
        start,
        step,
        total_channels: total,
    })
}

/// Channel modulation. `Digi` is the raw FM discriminator for an external decoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modulation {
    Nfm,
    Am,
    Digi,
}

impl Modulation {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "nfm" | "fm" => Some(Modulation::Nfm),
            "am" => Some(Modulation::Am),
            "digi" => Some(Modulation::Digi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Modulation::Nfm => "NFM",
            Modulation::Am => "AM",
            Modulation::Digi => "DIGI",
        }
    }
}

/// A range to scan with its own step, channel width and modulation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeSpec {
    pub start: u64,
    pub stop: u64,
    pub step: Step,
    /// Channel bandwidth (Hz).
    pub bw: f64,
    /// Whether `bw` was given explicitly with the range (`bw=`): then `bw=` from the channel
    /// file does not change it.
    pub bw_explicit: bool,
    pub mode: Modulation,
}

/// A single channel to scan.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelSpec {
    pub freq: u64,
    pub bw: f64,
    pub mode: Modulation,
    pub name: Option<String>,
    /// Airband 25 kHz channel (shown with its 25 kHz channel number).
    pub raster25: bool,
}

/// A plan channel (global index, shared by all ranges or the channel list).
#[derive(Clone, Debug)]
pub struct ChannelInfo {
    pub freq: u64,
    /// Channel width for detection.
    pub bw: f64,
    /// Demodulator filter width after stopping: usually `bw`, in a range also `bw=` from the
    /// channel file.
    pub demod_bw: f64,
    /// Whether the width is set on the command line (range `bw=` or a listed channel).
    pub bw_explicit: bool,
    /// Index of the range the channel comes from (0 in channel mode), for the scan progress.
    pub range: usize,
    /// Airband 25 kHz channel (from the channel file or the command line): shown with its 25 kHz
    /// channel number instead of the 8.33 kHz one.
    pub raster25: bool,
    pub mode: Modulation,
    /// Mode and demodulator width without the channel file (the range, or the listed channel
    /// with `digi` replaced by NFM): restored when `digi` is switched off from the TUI.
    pub base: (Modulation, f64),
    /// Grid spacing assigned to the channel: the range step or the `bw` of a listed channel. An
    /// exclusion covers the channel within half of this value, and the scan speed is computed
    /// from the sum of these spacings.
    pub spacing: f64,
    pub name: Option<String>,
}

/// Default `digi` channel width; also used when a channel is switched to `digi` from
/// the TUI.
pub const DIGI_BW_HZ: f64 = 12_500.0;

/// Scan plan: windows in one or two alternating passes.
/// A window's channels are the consecutive indices `first_index .. first_index + channels`.
#[derive(Debug)]
pub struct ScanPlan {
    pub passes: Vec<Vec<Window>>,
    pub channels: Vec<ChannelInfo>,
}

impl ScanPlan {
    pub fn channel(&self, i: u64) -> u64 {
        self.channels[i as usize].freq
    }

    pub fn total_channels(&self) -> u64 {
        self.channels.len() as u64
    }

    /// Bandwidth of the window's measured channels (Hz), for computing the scan speed.
    pub fn measured_hz(&self, w: &Window) -> f64 {
        (w.first_index..w.first_index + w.channels)
            .filter(|&i| w.is_measured(i))
            .map(|i| self.channels[i as usize].spacing)
            .sum()
    }
}

/// Plan for several ranges scanned in the given order. Each range has its own grid; windows do
/// not cross range boundaries. Pass A covers the A windows of all ranges, pass B (if any range
/// needs it) the B windows.
pub fn ranges_plan(
    ranges: &[RangeSpec],
    usable_width: u64,
    guard: u64,
) -> Result<ScanPlan, String> {
    if ranges.is_empty() {
        return Err("no ranges to scan".into());
    }
    let mut channels = Vec::new();
    let mut pass_a = Vec::new();
    let mut pass_b = Vec::new();
    for (range_index, r) in ranges.iter().enumerate() {
        // When bw > step, the outermost channels of a window stick out by (bw - step)/2 beyond
        // their grid spacing: the range plan gets a band reduced by this excess.
        let excess = (r.bw - r.step.hz()).max(0.0).ceil() as u64;
        let usable = usable_width.saturating_sub(excess);
        let rp = range_plan(r.start, r.stop, r.step, usable, guard).map_err(|e| {
            format!(
                "range {}-{} MHz: {e}",
                r.start as f64 / 1e6,
                r.stop as f64 / 1e6
            )
        })?;
        let offset = channels.len() as u64;
        for i in 0..rp.total_channels {
            channels.push(ChannelInfo {
                freq: rp.channel(i),
                bw: r.bw,
                demod_bw: r.bw,
                bw_explicit: r.bw_explicit,
                range: range_index,
                raster25: false,
                mode: r.mode,
                base: (r.mode, r.bw),
                spacing: r.step.hz(),
                name: None,
            });
        }
        let shift = |w: &Window| Window {
            first_index: w.first_index + offset,
            guard: w.guard.map(|(g0, g1)| (g0 + offset, g1 + offset)),
            ..*w
        };
        pass_a.extend(rp.passes[0].iter().map(shift));
        if let Some(b) = rp.passes.get(1) {
            pass_b.extend(b.iter().map(shift));
        }
    }
    let mut passes = vec![pass_a];
    if !pass_b.is_empty() {
        passes.push(pass_b);
    }
    Ok(ScanPlan { passes, channels })
}

/// The best LO for a group of channels (sorted ascending) or `None` if the group does not fit
/// in one window. The LO lies in a gap between channels or next to the group, at least
/// `bw/2 + guard·bw` from every channel (DC zone), and every channel fits in the usable band.
/// The LO with the smallest reach to the farthest channel is chosen.
fn group_lo(group: &[ChannelSpec], usable_width: u64, guard: u64) -> Option<u64> {
    let clearance = |c: &ChannelSpec| c.bw / 2.0 + guard as f64 * c.bw;
    let first = group.first()?;
    let last = group.last()?;
    let center = (first.freq as f64 + last.freq as f64) / 2.0;
    // Allowed LO intervals: to the left of the group, in the gaps, to the right of the group.
    let mut intervals = Vec::with_capacity(group.len() + 1);
    intervals.push((f64::MIN, first.freq as f64 - clearance(first)));
    for pair in group.windows(2) {
        let lo = pair[0].freq as f64 + clearance(&pair[0]);
        let hi = pair[1].freq as f64 - clearance(&pair[1]);
        if lo <= hi {
            intervals.push((lo, hi));
        }
    }
    intervals.push((last.freq as f64 + clearance(last), f64::MAX));

    let reach = |lo: f64| {
        group
            .iter()
            .map(|c| (c.freq as f64 - lo).abs() + c.bw / 2.0)
            .fold(0.0, f64::max)
    };
    intervals
        .iter()
        .map(|&(a, b)| center.clamp(a, b).round())
        // Rounding to whole Hz must not leave the interval (away from the channels).
        .filter(|&lo| {
            group
                .iter()
                .all(|c| (c.freq as f64 - lo).abs() >= clearance(c) - 0.5)
        })
        .map(|lo| (lo, reach(lo)))
        .filter(|&(_, r)| r <= usable_width as f64 / 2.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(lo, _)| lo as u64)
}

/// Plan for a channel list: channels sorted ascending and grouped into windows so that channels
/// fitting in one window are checked with a single tuning. One pass, the LO always outside the
/// channels and outside the DC zone. Duplicate frequencies: the first entry stays.
pub fn channels_plan(
    list: &[ChannelSpec],
    usable_width: u64,
    guard: u64,
) -> Result<ScanPlan, String> {
    let mut sorted: Vec<ChannelSpec> = Vec::with_capacity(list.len());
    for c in list {
        if !sorted.iter().any(|s| s.freq == c.freq) {
            sorted.push(c.clone());
        }
    }
    if sorted.is_empty() {
        return Err("no channels to scan".into());
    }
    sorted.sort_by_key(|c| c.freq);

    let mut windows = Vec::new();
    let mut start = 0;
    while start < sorted.len() {
        let Some(mut lo) = group_lo(&sorted[start..=start], usable_width, guard) else {
            let c = &sorted[start];
            return Err(format!(
                "channel {:.6} MHz (bw {} kHz) does not fit in the window band",
                c.freq as f64 / 1e6,
                c.bw / 1e3
            ));
        };
        let mut end = start;
        while end + 1 < sorted.len() {
            match group_lo(&sorted[start..=end + 1], usable_width, guard) {
                Some(l) => {
                    lo = l;
                    end += 1;
                }
                None => break,
            }
        }
        windows.push(Window {
            center: lo,
            first_index: start as u64,
            channels: (end - start + 1) as u64,
            first_channel: sorted[start].freq,
            last_channel: sorted[end].freq,
            measured: (end - start + 1) as u64,
            guard: None,
        });
        start = end + 1;
    }
    let channels = sorted
        .into_iter()
        .map(|c| ChannelInfo {
            freq: c.freq,
            bw: c.bw,
            demod_bw: c.bw,
            // The width of a listed channel is already resolved (command line, file, --bw).
            bw_explicit: true,
            range: 0,
            raster25: c.raster25,
            mode: c.mode,
            base: (
                if c.mode == Modulation::Digi {
                    Modulation::Nfm
                } else {
                    c.mode
                },
                c.bw,
            ),
            spacing: c.bw,
            name: c.name,
        })
        .collect();
    Ok(ScanPlan {
        passes: vec![windows],
        channels,
    })
}

#[cfg(test)]
mod scan_plan_tests {
    use super::*;

    fn range(start: u64, stop: u64, step: u64, bw: f64) -> RangeSpec {
        RangeSpec {
            start,
            stop,
            step: Step::from_hz(step),
            bw,
            bw_explicit: false,
            mode: Modulation::Nfm,
        }
    }

    fn ch(freq: u64, bw: f64) -> ChannelSpec {
        ChannelSpec {
            freq,
            bw,
            mode: Modulation::Nfm,
            name: None,
            raster25: false,
        }
    }

    /// Every channel measured at least once, at most once per pass, within the window band and
    /// away from DC.
    fn check(p: &ScanPlan, usable: u64, guard: u64) {
        let n = p.channels.len();
        let mut any = vec![false; n];
        for pass in &p.passes {
            let mut seen = vec![false; n];
            for w in pass {
                for i in w.first_index..w.first_index + w.channels {
                    let c = &p.channels[i as usize];
                    let d = (c.freq as f64 - w.center as f64).abs();
                    assert!(d + c.bw / 2.0 <= usable as f64 / 2.0 + 1.0);
                    if w.is_measured(i) {
                        let width = c.bw.min(c.spacing);
                        assert!(
                            d >= width / 2.0 + guard as f64 * width - 1.0,
                            "channel {} at DC",
                            c.freq
                        );
                        assert!(!seen[i as usize]);
                        seen[i as usize] = true;
                        any[i as usize] = true;
                    }
                }
            }
        }
        assert!(any.iter().all(|&a| a));
    }

    #[test]
    fn two_ranges_with_own_grids() {
        let r = [
            range(118_000_000, 137_000_000, 8_330, 10_000.0),
            RangeSpec {
                mode: Modulation::Am,
                ..range(144_000_000, 146_000_000, 12_500, 12_500.0)
            },
        ];
        let p = ranges_plan(&r, 4_800_000, 1).unwrap();
        assert_eq!(p.total_channels(), 2281 + 161);
        assert_eq!(p.channel(2280), 137_000_000);
        assert_eq!(p.channel(2281), 144_000_000);
        assert_eq!(p.channels[2281].mode, Modulation::Am);
        assert_eq!(p.passes.len(), 2);
        // Windows do not cross range boundaries.
        for w in p.passes.iter().flatten() {
            let a = w.first_index < 2281;
            let b = w.first_index + w.channels - 1 < 2281;
            assert_eq!(a, b);
        }
        check(&p, 4_800_000, 1);
    }

    #[test]
    fn channels_grouped_into_windows() {
        let list = [
            ch(433_006_250, 12_500.0),
            ch(145_500_000, 12_500.0),
            ch(145_600_000, 12_500.0),
            ch(145_500_000, 12_500.0), // duplicate
            ch(118_100_000, 10_000.0),
            ch(433_093_750, 12_500.0),
        ];
        let p = channels_plan(&list, 4_800_000, 1).unwrap();
        assert_eq!(p.total_channels(), 5);
        assert_eq!(p.passes.len(), 1);
        // 118.1 | 145.5 + 145.6 | 433.00625 + 433.09375
        assert_eq!(p.passes[0].len(), 3);
        check(&p, 4_800_000, 1);
    }

    #[test]
    fn dense_channels_still_avoid_dc() {
        // Dense channel grid: LO in a gap or next to it, never in a channel.
        let list: Vec<_> = (0..500)
            .map(|i| ch(145_000_000 + i * 12_500, 12_500.0))
            .collect();
        for guard in [0, 1, 2] {
            let p = channels_plan(&list, 4_800_000, guard).unwrap();
            check(&p, 4_800_000, guard);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: u64 = 12_500;

    fn step(hz: u64) -> Step {
        Step::from_hz(hz)
    }

    /// Checks plan invariants independent of the parameters.
    fn check(p: &RangePlan, usable: u64, guard: u64) {
        let total = p.total_channels as usize;
        let s = p.step.hz();
        let mut any = vec![false; total];
        for pass in &p.passes {
            let mut seen = vec![false; total];
            for w in pass {
                let lo = w.center as f64;
                for i in w.first_index..w.first_index + w.channels {
                    let c = p.channel(i) as f64;
                    let d = (c - lo).abs();
                    // Every channel of the block fits in the usable band (1 Hz rounding tolerance).
                    assert!(
                        d + s / 2.0 <= usable as f64 / 2.0 + 1.0,
                        "channel {c} outside window {lo}"
                    );
                    // LO never in the middle of a channel.
                    assert!(d >= s / 2.0 - 1.0, "LO {lo} in channel {c}");
                    if w.is_measured(i) {
                        // A measured channel is outside the DC zone.
                        assert!(
                            d >= s / 2.0 + guard as f64 * s - 1.0,
                            "channel {c} in the DC zone"
                        );
                        // In one pass a channel is measured at most once.
                        assert!(!seen[i as usize], "channel {c} measured twice");
                        seen[i as usize] = true;
                        any[i as usize] = true;
                    }
                }
            }
        }
        assert!(any.iter().all(|&a| a), "not all channels are measured");
    }

    #[test]
    fn range_118_137_without_guard() {
        let p = range_plan(118_000_000, 137_000_000, step(STEP), 4_800_000, 0).unwrap();
        assert_eq!(p.passes.len(), 1);
        assert_eq!(p.passes[0].len(), 4);
        assert_eq!(p.total_channels, 1521);
        check(&p, 4_800_000, 0);
    }

    #[test]
    fn range_118_137_with_guard() {
        for (usable, windows) in [(4_800_000, 4), (2_400_000, 8)] {
            let p = range_plan(118_000_000, 137_000_000, step(STEP), usable, 1).unwrap();
            assert_eq!(p.passes.len(), 2);
            // Pass B has no more windows than A.
            assert_eq!(p.passes[0].len(), windows);
            assert!(p.passes[1].len() <= windows);
            check(&p, usable, 1);
        }
    }

    #[test]
    fn wider_guards() {
        for guard in [2, 5, 20] {
            let p = range_plan(118_000_000, 137_000_000, step(STEP), 4_800_000, guard).unwrap();
            check(&p, 4_800_000, guard);
        }
        assert!(range_plan(118_000_000, 137_000_000, step(STEP), 4_800_000, 100).is_err());
    }

    #[test]
    fn narrow_ranges() {
        // One channel and several channels: LO next to the block, one pass, nothing is lost.
        for stop in [145_000_000, 145_030_000, 145_500_000] {
            let p = range_plan(145_000_000, stop, step(STEP), 4_800_000, 1).unwrap();
            assert_eq!(p.passes.len(), 1);
            assert_eq!(p.passes[0].len(), 1);
            check(&p, 4_800_000, 1);
        }
        assert!(range_plan(145_000_000, 146_000_000, step(5_000_000), 4_800_000, 0).is_err());
        assert!(range_plan(145_000_000, 146_000_000, step(0), 4_800_000, 0).is_err());
    }

    #[test]
    fn aviation_grid_without_drift() {
        // 8.33 kHz = 25/3 kHz: 118-137 MHz is exactly 2280 steps.
        let st = Step::from_hz(8_330);
        assert_eq!(
            st,
            Step {
                num: 25_000,
                den: 3
            }
        );
        let p = range_plan(118_000_000, 137_000_000, st, 4_800_000, 1).unwrap();
        assert_eq!(p.total_channels, 2281);
        assert_eq!(p.channel(1), 118_008_333);
        assert_eq!(p.channel(2), 118_016_667);
        assert_eq!(p.channel(3), 118_025_000);
        assert_eq!(p.channel(2280), 137_000_000);
        check(&p, 4_800_000, 1);
    }
}
