//! The scan list of the session: the ranges (range mode) or the channels (channel mode) that are
//! switched on and off, and ranges named, while scanning.
//!
//! - Ranges come from the range file and from `--scan-range` (command line or `rfscan.conf`). A
//!   command line range with the start, stop and step of a file line is that file entry (the
//!   command line wins for its mode and `bw` in this session); any other one exists only in this
//!   session until it is named, which appends it to the file. Without any range the default
//!   range is used, listed like a command line range. Overlapping ranges are an error; the list
//!   is in ascending order of the start frequency.
//! - The plan is built from all items, enabled or not; a disabled item only masks its channels
//!   (`off_mask`). Switching writes the `off` mark of a file entry (range or channel file) at
//!   once; items of the session only change in memory.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::activity::format_mhz;
use crate::plan::{Modulation, RangeSpec, ScanPlan};

/// Where a range of the session comes from (for messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSource {
    /// A line of the range file.
    File(usize),
    CommandLine,
    Conf,
    Default,
}

/// A range of the session, resolved, before the plan is built.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRange {
    pub spec: RangeSpec,
    pub name: Option<String>,
    pub off: bool,
    /// The range file line it belongs to: a file entry, also when given on the command line.
    pub line: Option<usize>,
    pub source: RangeSource,
}

impl SessionRange {
    /// `144.000-146.000 MHz (rfscan-ranges.txt line 3)`.
    fn label(&self) -> String {
        let src = match (self.line, self.source) {
            (Some(l), _) | (None, RangeSource::File(l)) => {
                format!("{} line {l}", crate::ranges::FILE_NAME)
            }
            (None, RangeSource::CommandLine) => "command line".into(),
            (None, RangeSource::Conf) => "rfscan.conf".into(),
            (None, RangeSource::Default) => "default range".into(),
        };
        format!("{} MHz ({src})", span(&self.spec))
    }
}

/// `144.000-146.000`.
fn span(r: &RangeSpec) -> String {
    format!("{}-{}", format_mhz(r.start), format_mhz(r.stop))
}

/// A difference of the channel width below this is rounding (`8.33k` written to the file is
/// 8330 Hz, the default AM width is 25/3 kHz).
const BW_TOLERANCE_HZ: f64 = 10.0;

/// Merges the ranges of the range file (resolved, with their entries) and the ranges of the
/// command line (`cli_source`: the command line or `rfscan.conf`), in ascending order of the
/// start. Without any range, `default` is used. Returns warnings for command line ranges that
/// change the mode or `bw` of a file entry.
pub fn merge_ranges(
    file: Vec<(RangeSpec, crate::ranges::RangeEntry)>,
    cli: Vec<RangeSpec>,
    cli_source: RangeSource,
    default: RangeSpec,
) -> (Vec<SessionRange>, Vec<String>) {
    let mut out: Vec<SessionRange> = file
        .into_iter()
        .map(|(spec, e)| SessionRange {
            spec,
            name: e.name,
            off: e.off,
            line: Some(e.line),
            source: RangeSource::File(e.line),
        })
        .collect();
    let in_file = out.len();
    let mut warnings = Vec::new();
    for spec in cli {
        let same = out[..in_file].iter_mut().find(|r| {
            r.spec.start == spec.start && r.spec.stop == spec.stop && r.spec.step == spec.step
        });
        match same {
            Some(r) => {
                if r.spec.mode != spec.mode || (r.spec.bw - spec.bw).abs() > BW_TOLERANCE_HZ {
                    warnings.push(format!(
                        "WARNING: {} is in the range file as {} bw {} kHz{}, and on the {} as {} bw {} kHz: the {} wins for this session, the file is not changed",
                        span(&spec),
                        r.spec.mode.name(),
                        crate::ranges::khz_text(r.spec.bw).trim_end_matches('k'),
                        r.line.map_or(String::new(), |l| format!(" (line {l})")),
                        source_name(cli_source),
                        spec.mode.name(),
                        crate::ranges::khz_text(spec.bw).trim_end_matches('k'),
                        source_name(cli_source),
                    ));
                }
                r.spec = spec;
                r.source = cli_source;
            }
            None => out.push(SessionRange {
                spec,
                name: None,
                off: false,
                line: None,
                source: cli_source,
            }),
        }
    }
    if out.is_empty() {
        out.push(SessionRange {
            spec: default,
            name: None,
            off: false,
            line: None,
            source: RangeSource::Default,
        });
    }
    out.sort_by_key(|r| (r.spec.start, r.spec.stop));
    (out, warnings)
}

fn source_name(s: RangeSource) -> &'static str {
    match s {
        RangeSource::File(_) => "range file",
        RangeSource::CommandLine => "command line",
        RangeSource::Conf => "rfscan.conf",
        RangeSource::Default => "default range",
    }
}

/// Grid channel `n` of a range: start + round(n·step).
fn channel(r: &RangeSpec, n: u64) -> u64 {
    r.start + (2 * n * r.step.num + r.step.den) / (2 * r.step.den)
}

/// The first grid channel of `r` above `x`, if any.
fn first_above(r: &RangeSpec, x: u64) -> Option<u64> {
    if x < r.start {
        return Some(r.start);
    }
    let mut n = (x - r.start) * r.step.den / r.step.num;
    while channel(r, n) <= x {
        n += 1;
    }
    let f = channel(r, n);
    (f <= r.stop).then_some(f)
}

/// The last grid channel of `r` below `x`, if any.
fn last_below(r: &RangeSpec, x: u64) -> Option<u64> {
    if x <= r.start {
        return None;
    }
    let last = (r.stop - r.start) * r.step.den / r.step.num;
    let mut n = ((x - r.start) * r.step.den / r.step.num).min(last);
    while n > 0 && channel(r, n) >= x {
        n -= 1;
    }
    let f = channel(r, n);
    (f < x).then_some(f)
}

/// Checks that no two ranges (switched off ones included) have a common point, a shared edge
/// included. The error names every overlapping pair with a suggested fix.
pub fn check_overlaps(ranges: &[SessionRange]) -> Result<(), String> {
    let mut problems = Vec::new();
    for (i, a) in ranges.iter().enumerate() {
        for b in &ranges[i + 1..] {
            let (x, y) = (&a.spec, &b.spec);
            if !(x.start <= y.stop && y.start <= x.stop) {
                continue;
            }
            problems.push(overlap_fix(a, b));
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "overlapping ranges (no frequency may be scanned twice):\n  - {}",
        problems.join("\n  - ")
    ))
}

/// The message for one overlapping pair, with a suggested fix.
fn overlap_fix(a: &SessionRange, b: &SessionRange) -> String {
    let (x, y) = (&a.spec, &b.spec);
    let inside = |i: &RangeSpec, o: &RangeSpec| o.start <= i.start && i.stop <= o.stop;
    // A range lying inside another (an equal one counts as the later one inside).
    let contained = if inside(y, x) {
        Some((b, a))
    } else if inside(x, y) {
        Some((a, b))
    } else {
        None
    };
    if let Some((inner, outer)) = contained {
        let (i, o) = (&inner.spec, &outer.spec);
        // A piece of a single channel is not offered: a range needs a start below its stop.
        let low = last_below(o, i.start)
            .filter(|&f| f > o.start)
            .map(|f| format!("{}-{}", format_mhz(o.start), format_mhz(f)));
        let high = first_above(o, i.stop)
            .filter(|&f| f < o.stop)
            .map(|f| format!("{}-{}", format_mhz(f), format_mhz(o.stop)));
        let split = match (low, high) {
            (Some(l), Some(h)) => format!(", or split {} into {l} and {h}", span(o)),
            (Some(p), None) | (None, Some(p)) => format!(", or change {} to {p}", span(o)),
            (None, None) => String::new(),
        };
        return format!(
            "{} lies inside {}: remove {}{split}",
            inner.label(),
            outer.label(),
            span(i)
        );
    }
    let (early, late) = if x.start <= y.start { (a, b) } else { (b, a) };
    let (e, l) = (&early.spec, &late.spec);
    let mut fixes = Vec::new();
    if let Some(f) = first_above(l, e.stop).filter(|&f| f < l.stop) {
        fixes.push(format!("start {} at {} MHz", span(l), format_mhz(f)));
    }
    if let Some(f) = last_below(e, l.start).filter(|&f| f > e.start) {
        fixes.push(format!("end {} at {} MHz", span(e), format_mhz(f)));
    }
    format!(
        "{} and {} overlap: {}",
        early.label(),
        late.label(),
        if fixes.is_empty() {
            "remove one of them".to_string()
        } else {
            fixes.join(", or ")
        }
    )
}

/// Which list the session has: the two scan modes are exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Ranges,
    Channels,
}

/// Where the switch and the name of an item are saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// A line of the range file.
    RangeFile,
    /// An entry of the channel file (channel mode from the file).
    ChannelFile,
    /// Nothing is written (a command line range until it is named, a listed channel).
    Session,
}

/// One row of the scan list.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanItem {
    /// The range, or the channel frequency (start = stop).
    pub start: u64,
    pub stop: u64,
    /// Grid step (Hz), 0 for a channel.
    pub step: f64,
    pub bw: f64,
    pub mode: Modulation,
    /// The range name (channel names come from the channel file).
    pub name: Option<String>,
    pub enabled: bool,
    pub store: Store,
    /// Plan channel indices of the item (empty when it is not in the plan).
    pub channels: std::ops::Range<usize>,
    /// Why the item cannot be switched, e.g. a digi channel without a digi output.
    pub inactive: Option<&'static str>,
}

impl ScanItem {
    /// Scanned: switched on and in the plan.
    pub fn scanned(&self) -> bool {
        self.enabled && self.inactive.is_none()
    }
}

/// The scan list with the files its switches are saved to.
#[derive(Clone, Debug)]
pub struct ScanList {
    pub kind: ListKind,
    pub items: Vec<ScanItem>,
    pub ranges_path: Option<PathBuf>,
    pub channels_path: Option<PathBuf>,
    /// The error of the last switch or name, if it failed (shown by the interfaces).
    pub error: Option<String>,
}

/// The list shared by the scanner (which switches it) and the interfaces (which show it).
pub type SharedScanList = Arc<Mutex<ScanList>>;

/// Hint of a `digi` channel that is not scanned without a digi output.
pub const DIGI_HINT: &str = "digi needs --digi or --digi-tcp";

impl ScanList {
    /// The list of range mode: one item per range, in plan order (`ChannelInfo::range`).
    pub fn ranges(ranges: &[SessionRange], plan: &ScanPlan, path: Option<PathBuf>) -> Self {
        let items = ranges
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let first = plan.channels.iter().position(|c| c.range == i);
                let count = plan.channels.iter().filter(|c| c.range == i).count();
                let first = first.unwrap_or(0);
                ScanItem {
                    start: r.spec.start,
                    stop: r.spec.stop,
                    step: r.spec.step.hz(),
                    bw: r.spec.bw,
                    mode: r.spec.mode,
                    name: r.name.clone(),
                    enabled: !r.off,
                    store: if r.line.is_some() {
                        Store::RangeFile
                    } else {
                        Store::Session
                    },
                    channels: first..first + count,
                    inactive: None,
                }
            })
            .collect();
        ScanList {
            kind: ListKind::Ranges,
            items,
            ranges_path: path,
            channels_path: None,
            error: None,
        }
    }

    /// The list of channel mode: one item per plan channel, enabled unless `off(freq)` (the
    /// channel file in channel mode from the file), plus the `skipped` digi channels of the file
    /// (not in the plan), all in ascending order. `from_file`: switches write the channel file.
    pub fn channels(
        plan: &ScanPlan,
        off: impl Fn(u64) -> bool,
        skipped: &[(u64, f64)],
        from_file: bool,
        path: Option<PathBuf>,
    ) -> Self {
        let mut items: Vec<ScanItem> = plan
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| ScanItem {
                start: c.freq,
                stop: c.freq,
                step: 0.0,
                bw: c.bw,
                mode: c.mode,
                name: None,
                enabled: !off(c.freq),
                store: if from_file {
                    Store::ChannelFile
                } else {
                    Store::Session
                },
                channels: i..i + 1,
                inactive: None,
            })
            .collect();
        items.extend(skipped.iter().map(|&(freq, bw)| ScanItem {
            start: freq,
            stop: freq,
            step: 0.0,
            bw,
            mode: Modulation::Digi,
            name: None,
            enabled: !off(freq),
            store: Store::ChannelFile,
            channels: 0..0,
            inactive: Some(DIGI_HINT),
        }));
        items.sort_by_key(|i| i.start);
        ScanList {
            kind: ListKind::Channels,
            items,
            ranges_path: None,
            channels_path: path,
            error: None,
        }
    }

    /// Scanned items and all items, for the counter (`Ranges 3/5`).
    pub fn counts(&self) -> (usize, usize) {
        let on = self.items.iter().filter(|i| i.scanned()).count();
        (on, self.items.len())
    }

    /// Per plan channel: switched off (true) by its item.
    pub fn off_mask(&self, channels: usize) -> Vec<bool> {
        let mut mask = vec![false; channels];
        for item in self.items.iter().filter(|i| !i.enabled) {
            for m in mask.get_mut(item.channels.clone()).into_iter().flatten() {
                *m = true;
            }
        }
        mask
    }

    /// Switches items on or off, as one write per file. Items that cannot be switched or are
    /// already in that state are left alone. Returns the items that changed; after an error
    /// nothing changes.
    pub fn switch(&mut self, ids: &[usize], on: bool) -> Result<Vec<usize>, String> {
        let ids: Vec<usize> = ids
            .iter()
            .copied()
            .filter(|&i| {
                self.items
                    .get(i)
                    .is_some_and(|it| it.inactive.is_none() && it.enabled != on)
            })
            .collect();
        let ranges: Vec<(u64, u64)> = ids
            .iter()
            .map(|&i| &self.items[i])
            .filter(|it| it.store == Store::RangeFile)
            .map(|it| (it.start, it.stop))
            .collect();
        if !ranges.is_empty() {
            let path = self
                .ranges_path
                .as_ref()
                .ok_or("no range file path (use --ranges-file)")?;
            crate::ranges::set_off(path, &ranges, !on)?;
        }
        let channels: Vec<u64> = ids
            .iter()
            .map(|&i| &self.items[i])
            .filter(|it| it.store == Store::ChannelFile)
            .map(|it| it.start)
            .collect();
        if !channels.is_empty() {
            let path = self
                .channels_path
                .as_ref()
                .ok_or("no channel file path (use --channels-file)")?;
            // The entries are the plan channels themselves: an exact match.
            crate::channel_db::set_off(path, &channels, 1.0, !on)?;
        }
        for &i in &ids {
            self.items[i].enabled = on;
        }
        Ok(ids)
    }

    /// Names a range (`None` removes the name). A file entry gets the name on its line; a range
    /// of the session is appended to the range file with all its fields and becomes a file entry
    /// (without a name nothing is written).
    pub fn rename(&mut self, id: usize, name: Option<String>) -> Result<(), String> {
        let Some(item) = self.items.get(id) else {
            return Err("no such item".into());
        };
        if self.kind != ListKind::Ranges {
            return Err("only ranges are named here (channels in the channel file)".into());
        }
        match (item.store, &name) {
            (Store::RangeFile, _) => {
                let path = self
                    .ranges_path
                    .as_ref()
                    .ok_or("no range file path (use --ranges-file)")?;
                crate::ranges::set_name(path, (item.start, item.stop), name.as_deref())?;
            }
            (_, Some(n)) => {
                let path = self
                    .ranges_path
                    .as_ref()
                    .ok_or("no range file path (use --ranges-file)")?;
                let line = crate::ranges::entry_text(
                    item.start,
                    item.stop,
                    item.step,
                    item.mode,
                    item.bw,
                    !item.enabled,
                    Some(n),
                )?;
                crate::ranges::append(path, &line)?;
                self.items[id].store = Store::RangeFile;
            }
            (_, None) => {}
        }
        self.items[id].name = name;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Step;
    use crate::ranges::RangeEntry;

    fn spec(start: u64, stop: u64, step: u64) -> RangeSpec {
        RangeSpec {
            start,
            stop,
            step: Step::from_hz(step),
            bw: step as f64,
            bw_explicit: false,
            mode: Modulation::Nfm,
        }
    }

    fn entry(start: u64, stop: u64, line: usize, off: bool, name: Option<&str>) -> RangeEntry {
        RangeEntry {
            start,
            stop,
            step: None,
            mode: None,
            bw: None,
            off,
            name: name.map(str::to_string),
            line,
        }
    }

    const DEFAULT: (u64, u64) = (144_000_000, 146_000_000);

    fn default() -> RangeSpec {
        spec(DEFAULT.0, DEFAULT.1, 12_500)
    }

    /// The same start, stop and step is the file entry (the command line wins for the mode and
    /// bw, with a warning); other command line ranges are session ranges; ascending order.
    #[test]
    fn command_line_ranges_merge_with_the_file() {
        let file = vec![
            (
                spec(434_000_000, 435_000_000, 25_000),
                entry(434_000_000, 435_000_000, 3, true, Some("B")),
            ),
            (
                spec(144_000_000, 146_000_000, 12_500),
                entry(144_000_000, 146_000_000, 1, false, Some("A")),
            ),
        ];
        let am = RangeSpec {
            mode: Modulation::Am,
            ..spec(434_000_000, 435_000_000, 25_000)
        };
        let cli = vec![
            spec(144_000_000, 146_000_000, 12_500),
            am,
            spec(430_000_000, 431_000_000, 12_500),
            // Another step: a different range (and an overlap, found later).
            spec(144_000_000, 146_000_000, 25_000),
        ];
        let (r, w) = merge_ranges(file, cli, RangeSource::CommandLine, default());
        assert_eq!(r.len(), 4);
        let starts: Vec<u64> = r.iter().map(|r| r.spec.start).collect();
        assert_eq!(starts, [144_000_000, 144_000_000, 430_000_000, 434_000_000]);
        // The same range without differences: no warning, the file name and line.
        assert_eq!(r[0].line, Some(1));
        assert_eq!(r[0].name.as_deref(), Some("A"));
        assert_eq!((r[1].line, r[1].source), (None, RangeSource::CommandLine));
        assert_eq!(
            (r[2].line, r[2].name.clone(), r[2].off),
            (None, None, false)
        );
        assert_eq!(r[3].spec.mode, Modulation::Am);
        assert!(r[3].off && r[3].line == Some(3));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("434.000-435.000") && w[0].contains("NFM") && w[0].contains("AM"),
            "{}",
            w[0]
        );
        assert!(w[0].contains("(line 3)"), "{}", w[0]);
    }

    /// The default range only without any range from the file or the command line.
    #[test]
    fn default_range_only_without_ranges() {
        let (r, _) = merge_ranges(Vec::new(), Vec::new(), RangeSource::Conf, default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].source, RangeSource::Default);
        assert_eq!((r[0].spec.start, r[0].spec.stop), DEFAULT);
        let file = vec![(
            spec(430_000_000, 440_000_000, 25_000),
            entry(430_000_000, 440_000_000, 1, true, None),
        )];
        let (r, _) = merge_ranges(file, Vec::new(), RangeSource::Conf, default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].spec.start, 430_000_000);
        let (r, _) = merge_ranges(
            Vec::new(),
            vec![spec(430_000_000, 440_000_000, 25_000)],
            RangeSource::Conf,
            default(),
        );
        assert_eq!((r.len(), r[0].source), (1, RangeSource::Conf));
    }

    fn session(start: u64, stop: u64, step: u64, source: RangeSource) -> SessionRange {
        SessionRange {
            spec: spec(start, stop, step),
            name: None,
            off: false,
            line: match source {
                RangeSource::File(l) => Some(l),
                _ => None,
            },
            source,
        }
    }

    /// A shared edge, containment and partial overlaps (also of switched off ranges) are errors
    /// naming both sources and a fix.
    #[test]
    fn overlaps_are_errors_with_a_fix() {
        let ok = [
            session(144_000_000, 145_000_000, 12_500, RangeSource::File(1)),
            session(145_012_500, 146_000_000, 12_500, RangeSource::CommandLine),
        ];
        assert!(check_overlaps(&ok).is_ok());

        // A shared edge: 145.000 is in both.
        let mut edge = [
            session(144_000_000, 145_000_000, 12_500, RangeSource::File(2)),
            session(145_000_000, 146_000_000, 12_500, RangeSource::CommandLine),
        ];
        edge[0].off = true;
        let e = check_overlaps(&edge).unwrap_err();
        assert!(e.contains("144.000-145.000 MHz (rfscan-ranges.txt line 2) and 145.000-146.000 MHz (command line) overlap"), "{e}");
        assert!(
            e.contains(
                "start 145.000-146.000 at 145.0125 MHz, or end 144.000-145.000 at 144.9875 MHz"
            ),
            "{e}"
        );

        // A partial overlap on the 8.33 kHz grid.
        let part = [
            session(118_000_000, 125_000_000, 8_330, RangeSource::Conf),
            session(124_000_000, 130_000_000, 8_330, RangeSource::File(4)),
        ];
        let e = check_overlaps(&part).unwrap_err();
        assert!(
            e.contains("(rfscan.conf) and 124.000-130.000 MHz (rfscan-ranges.txt line 4) overlap"),
            "{e}"
        );
        assert!(
            e.contains(
                "start 124.000-130.000 at 125.008333 MHz, or end 118.000-125.000 at 123.991667 MHz"
            ),
            "{e}"
        );

        // Containment: remove the inner one or split the outer one around it.
        let inside = [
            session(430_000_000, 440_000_000, 25_000, RangeSource::File(1)),
            session(433_000_000, 434_000_000, 12_500, RangeSource::CommandLine),
            session(436_000_000, 437_000_000, 12_500, RangeSource::File(3)),
        ];
        let e = check_overlaps(&inside).unwrap_err();
        assert_eq!(e.lines().count(), 3, "{e}");
        assert!(e.contains("433.000-434.000 MHz (command line) lies inside 430.000-440.000 MHz (rfscan-ranges.txt line 1): remove 433.000-434.000, or split 430.000-440.000 into 430.000-432.975 and 434.025-440.000"), "{e}");
        // Inside, at the edge of the outer range: only one piece is left.
        let edge_in = [
            session(430_000_000, 440_000_000, 25_000, RangeSource::File(1)),
            session(430_000_000, 431_000_000, 25_000, RangeSource::File(2)),
        ];
        let e = check_overlaps(&edge_in).unwrap_err();
        assert!(
            e.contains("lies inside") && e.contains("or change 430.000-440.000 to 431.025-440.000"),
            "{e}"
        );
        // A single channel left over is not a range: no piece of one channel is offered.
        let one_left = [
            session(430_000_000, 431_000_000, 12_500, RangeSource::CommandLine),
            session(430_000_000, 430_987_500, 12_500, RangeSource::File(1)),
        ];
        let e = check_overlaps(&one_left).unwrap_err();
        assert!(e.ends_with("remove 430.000-430.9875"), "{e}");
        let both_ends = [
            session(430_000_000, 431_000_000, 12_500, RangeSource::CommandLine),
            session(430_012_500, 430_987_500, 12_500, RangeSource::File(1)),
        ];
        let e = check_overlaps(&both_ends).unwrap_err();
        assert!(e.ends_with("remove 430.0125-430.9875"), "{e}");
        let short_late = [
            session(430_000_000, 431_000_000, 12_500, RangeSource::CommandLine),
            session(431_000_000, 431_012_500, 12_500, RangeSource::File(1)),
        ];
        let e = check_overlaps(&short_late).unwrap_err();
        assert!(
            e.ends_with("overlap: end 430.000-431.000 at 430.9875 MHz"),
            "{e}"
        );
        let both_short = [
            session(430_000_000, 430_012_500, 12_500, RangeSource::CommandLine),
            session(430_012_500, 430_025_000, 12_500, RangeSource::File(1)),
        ];
        let e = check_overlaps(&both_short).unwrap_err();
        assert!(e.ends_with("overlap: remove one of them"), "{e}");
    }

    #[test]
    fn grid_neighbours() {
        let r = spec(120_000_000, 136_000_000, 8_330);
        assert_eq!(first_above(&r, 120_000_000), Some(120_008_333));
        assert_eq!(first_above(&r, 118_000_000), Some(120_000_000));
        assert_eq!(first_above(&r, 136_000_000), None);
        assert_eq!(last_below(&r, 120_016_667), Some(120_008_333));
        assert_eq!(last_below(&r, 120_000_000), None);
        assert_eq!(last_below(&r, 137_000_000), Some(136_000_000));
    }

    fn plan_of(ranges: &[SessionRange]) -> ScanPlan {
        let specs: Vec<RangeSpec> = ranges.iter().map(|r| r.spec).collect();
        crate::plan::ranges_plan(&specs, 2_000_000, 1).unwrap()
    }

    /// Switching ranges writes `off` to the file as one write (only file entries); naming a
    /// session range appends it to the file.
    #[test]
    fn switch_and_name_ranges() {
        let dir = std::env::temp_dir().join(format!("rfscan-scanlist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ranges.txt");
        std::fs::write(&path, "144.000-145.000 12.5k \"A\"\n").unwrap();
        let mut ranges = vec![
            session(144_000_000, 145_000_000, 12_500, RangeSource::File(1)),
            session(430_000_000, 431_000_000, 12_500, RangeSource::CommandLine),
        ];
        ranges[0].name = Some("A".into());
        let plan = plan_of(&ranges);
        let mut list = ScanList::ranges(&ranges, &plan, Some(path.clone()));
        assert_eq!(list.counts(), (2, 2));
        assert_eq!(list.items[0].channels, 0..81);
        assert_eq!(list.items[1].channels, 81..162);
        assert_eq!(list.switch(&[0, 1], false).unwrap(), [0, 1]);
        assert_eq!(list.counts(), (0, 2));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "144.000-145.000 12.5k off \"A\"\n"
        );
        let mask = list.off_mask(plan.channels.len());
        assert!(mask.iter().all(|&m| m));
        // Already off: nothing changes.
        assert!(list.switch(&[0], false).unwrap().is_empty());
        assert_eq!(list.switch(&[1], true).unwrap(), [1]);
        assert_eq!(list.off_mask(162).iter().filter(|&&m| m).count(), 81);
        // A session range named: appended with its fields, now a file entry.
        list.rename(1, Some("70cm".into())).unwrap();
        assert_eq!(list.items[1].store, Store::RangeFile);
        list.switch(&[1], false).unwrap();
        list.rename(0, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "144.000-145.000 12.5k off\n430.000-431.000 12.5k fm bw=12.5k off \"70cm\"\n"
        );
        // A write error changes nothing.
        std::fs::remove_file(&path).unwrap();
        assert!(list.switch(&[0], true).is_err());
        assert!(!list.items[0].enabled);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Channel mode: the `off` of the channel file, digi channels skipped without a digi output
    /// listed as inactive, switches written to the channel file (from the file only).
    #[test]
    fn channel_list() {
        let dir = std::env::temp_dir().join(format!("rfscan-scanlist-ch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("channels.txt");
        std::fs::write(&path, "145.500 \"A\"\n145.600 off\n438.800 digi\n").unwrap();
        let ch = |f| crate::plan::ChannelSpec {
            freq: f,
            bw: 12_500.0,
            mode: Modulation::Nfm,
            name: None,
            raster25: false,
        };
        let plan =
            crate::plan::channels_plan(&[ch(145_600_000), ch(145_500_000)], 2_000_000, 1).unwrap();
        let mut list = ScanList::channels(
            &plan,
            |f| f == 145_600_000,
            &[(438_800_000, 12_500.0)],
            true,
            Some(path.clone()),
        );
        let freqs: Vec<u64> = list.items.iter().map(|i| i.start).collect();
        assert_eq!(freqs, [145_500_000, 145_600_000, 438_800_000]);
        assert_eq!(list.counts(), (1, 3));
        assert_eq!(list.items[2].inactive, Some(DIGI_HINT));
        assert_eq!(list.off_mask(2), [false, true]);
        // The inactive digi channel is not switched.
        assert_eq!(list.switch(&[0, 1, 2], true).unwrap(), [1]);
        assert_eq!(list.switch(&[0, 1, 2], false).unwrap(), [0, 1]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "145.500 off \"A\"\n145.600 off\n438.800 digi\n"
        );
        assert!(list.rename(0, Some("x".into())).is_err());
        // A command line list writes nothing.
        let mut cli = ScanList::channels(&plan, |_| false, &[], false, Some(path.clone()));
        assert_eq!(cli.switch(&[0, 1], false).unwrap(), [0, 1]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "145.500 off \"A\"\n145.600 off\n438.800 digi\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
