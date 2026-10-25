//! Drawing. The layout is computed on every frame from the terminal size: the large
//! frequency font when the window has room for it, otherwise a compact layout; below the
//! minimum size only a message.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{App, Screen, bigtext};
use crate::activity::format_mhz;
use crate::dsd_fme::DsdState;
use crate::events::{LockView, ScanState, Spectrum};
use crate::exclusions::TIMED_MINUTES;
use crate::plan::Modulation;

/// Minimum terminal size; below it only a message is shown.
const MIN_W: u16 = 80;
const MIN_H: u16 = 24;
/// From this size the frequency is shown in the large font.
const BIG_W: u16 = 100;
const BIG_H: u16 = 30;
/// Full scale of the SNR bar (dB).
const SNR_SCALE_DB: f32 = 60.0;
/// SNR (dB) of the highest mini-spectrum bar.
const SPECTRUM_TOP_DB: f32 = 40.0;
/// Height of the central part (scanning or lock view) with the large font and in the compact
/// layout: enough for the tallest content (lock view with a name and the digi line, scanning
/// view while paused).
const CENTER_BIG: usize = 14;
const CENTER_SMALL: usize = 9;
/// How long a notice replaces the key help.
const NOTICE: Duration = Duration::from_secs(4);
/// Width of the statistics column in a wide window.
const STATS_W: u16 = 31;

const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::Gray;
const BRIGHT: Color = Color::White;
const FREQ: Color = Color::LightGreen;
const NAME: Color = Color::Rgb(255, 191, 0);

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(DIM))
}

fn text(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(TEXT))
}

fn bright(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::new().fg(BRIGHT))
}

fn state_label(state: ScanState) -> (&'static str, Color) {
    match state {
        ScanState::Scanning => ("SCANNING", Color::Cyan),
        ScanState::Locked => ("LOCKED", Color::LightGreen),
        ScanState::Hang => ("HANG", Color::Yellow),
        ScanState::Paused => ("PAUSED", Color::Magenta),
    }
}

/// Truncates `s` to `w` characters with an ellipsis.
fn fit(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        s.to_string()
    } else if w == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(w - 1).collect();
        t.push('…');
        t
    }
}

/// Frequency in kHz rounded to 2 decimal places, without trailing zeros (8.33, 12.5, 25).
fn khz(hz: f64) -> String {
    let s = format!("{:.2}", hz / 1e3);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn mm_ss(d: Duration) -> String {
    let s = d.as_secs_f64();
    format!("{:02}:{:04.1}", (s / 60.0) as u64, s % 60.0)
}

fn h_mm_ss(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A horizontal rule across the whole frame at the row of `row` (inside `inner`), joined with
/// the outer border (`├ ┤`). Optional titles and a junction with the statistics column
/// (`split`: column inside `inner` and its character, `┬` or `┴`).
fn rule(f: &mut Frame, inner: Rect, row: Rect, titles: (&str, &str), split: Option<(u16, char)>) {
    let w = inner.width as usize;
    let mut chars: Vec<char> = vec!['─'; w];
    let mut put = |at: usize, s: &str| {
        for (i, c) in s.chars().enumerate() {
            if at + i < w {
                chars[at + i] = c;
            }
        }
    };
    if !titles.0.is_empty() {
        put(0, &format!(" {} ", titles.0));
    }
    if let Some((col, junction)) = split {
        let col = col as usize;
        if col < w {
            put(col, &junction.to_string());
            if !titles.1.is_empty() {
                put(col + 1, &format!("─ {} ", titles.1));
            }
        }
    }
    // The content has a one-column padding on each side, so the rule covers it too.
    let line = format!("├─{}─┤", chars.into_iter().collect::<String>());
    let area = Rect {
        x: inner.x.saturating_sub(2),
        y: row.y,
        width: inner.width + 4,
        height: 1,
    };
    f.render_widget(Paragraph::new(Line::from(dim(line))), area);
}

/// A separator line inside an area (screens without the outer junctions).
fn separator(width: u16, title: &str) -> Line<'static> {
    let head = if title.is_empty() {
        String::new()
    } else {
        format!("─ {title} ")
    };
    let rest = (width as usize).saturating_sub(head.chars().count());
    Line::from(vec![dim(head), dim("─".repeat(rest))])
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let ctx = &app.ctx;
    let state = app.status.map_or(ScanState::Scanning, |s| s.state);
    let (label, color) = state_label(state);
    let mut right = Vec::new();
    if ctx.nr.load(std::sync::atomic::Ordering::Relaxed) {
        // On a digi channel the noise reduction does not apply: dimmed.
        let digi = app.lock.is_some_and(|l| l.mode == Modulation::Digi);
        right.push(Span::styled(
            format!(
                " NR {}% ",
                ctx.nr_strength.load(std::sync::atomic::Ordering::Relaxed)
            ),
            if digi {
                Style::new().fg(DIM)
            } else {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            },
        ));
    }
    if ctx.muted.load(std::sync::atomic::Ordering::Relaxed) {
        right.push(Span::styled(
            " MUTED ",
            Style::new()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if ctx.player.as_ref().is_some_and(|p| p.playing().is_some()) {
        right.push(Span::styled(
            " ▶ PLAY ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if ctx.rec_writing() {
        right.push(Span::styled(
            " REC ",
            Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }
    right.push(Span::styled(
        format!(" ● {label} "),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    ));
    right.push(dim("─"));
    let title = format!(
        " rfscan ─ {} ─ {:.1} MSps ─ {} ",
        ctx.device,
        ctx.sample_rate / 1e6,
        ctx.gain
    );
    let block = Block::bordered()
        .border_style(Style::new().fg(DIM))
        .title(Line::from(vec![dim("─"), text(title)]))
        .title_top(Line::from(right).right_aligned());
    let inner = block.inner(area);
    // One column of padding inside the frame.
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    f.render_widget(block, area);
    if area.width < MIN_W || area.height < MIN_H {
        too_small(f, inner, area);
        return;
    }
    match app.screen {
        Screen::Main => main_screen(f, inner, app),
        Screen::Activity => activity_screen(f, inner, app),
        Screen::Exclusions => exclusions_screen(f, inner, app),
        Screen::Recordings => recordings_screen(f, inner, app),
    }
    if let Some(edit) = &app.edit {
        label_popup(f, area, app, edit);
    }
    if let Some((action, error)) = &app.confirm {
        confirm_popup(f, area, app, *action, error.as_deref());
    }
    if let Some(msg) = &app.message {
        message_popup(f, area, msg);
    }
}

/// A message until `[Esc]`.
fn message_popup(f: &mut Frame, area: Rect, msg: &str) {
    let rect = centered(area, 50, 6);
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Yellow))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(block, rect);
    let lines = vec![
        Line::from(""),
        Line::from(text(fit(msg, inner.width as usize))).centered(),
        Line::from(""),
        key_line("[Esc] close").centered(),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// A rectangle of `w` x `h` centered in `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Confirmation on the activity screen: clearing the log (`[c] clear`) or deleting one entry
/// (`[d] delete`).
fn confirm_popup(
    f: &mut Frame,
    area: Rect,
    app: &App,
    action: super::ConfirmAction,
    error: Option<&str>,
) {
    let (title, what) = match action {
        super::ConfirmAction::Clear => (
            " clear the activity log ".to_string(),
            "All entries will be removed and the empty file saved.".to_string(),
        ),
        super::ConfirmAction::DeleteRecording(id) => {
            let name = app
                .rec_channel
                .map(|ch| app.ctx.rec_files(ch))
                .and_then(|files| files.into_iter().find(|f| f.id == id))
                .map_or_else(String::new, |f| f.time);
            (
                format!(" delete the recording {name} "),
                "The file will be deleted from the disk.".to_string(),
            )
        }
        super::ConfirmAction::Delete(freq) => {
            let name = app
                .ctx
                .name_near(freq)
                .map(|n| format!(" {n}"))
                .unwrap_or_default();
            (
                format!(" delete {} MHz{name} ", app.ctx.freq_label(freq)),
                "Its time and hits will be removed and the file saved.".to_string(),
            )
        }
    };
    let rect = centered(area, 60, 7);
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::LightRed))
        .title(Line::from(Span::styled(
            fit(&title, 56),
            Style::new()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
        )))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(block, rect);
    let lines = vec![
        Line::from(""),
        Line::from(text(&what)),
        match error {
            Some(e) => Line::from(Span::styled(
                fit(e, inner.width as usize),
                Style::new().fg(Color::LightRed),
            )),
            None => Line::from(""),
        },
        key_line("Are you sure? [y]es / [n]o"),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// The text popup, centered on the screen: a channel name (activity screen, detection
/// history) or an optional exclusion note (`[x]`/`[X]`).
fn label_popup(f: &mut Frame, area: Rect, app: &App, edit: &super::LabelEdit) {
    let (title, color, prompt, hint) = match edit.kind {
        super::EditKind::Label { .. } => (
            " channel: ".to_string(),
            Color::Cyan,
            "> ",
            "[Enter] save to the channel file  [Tab] digi  [Esc] cancel",
        ),
        super::EditKind::Exclude { channel, wide, .. } => (
            if wide && channel.is_some() {
                " exclude with adjacent channels: ".to_string()
            } else {
                " exclude: ".to_string()
            },
            Color::Yellow,
            "note: ",
            "[Enter] exclude (note optional) [Tab] duration [Esc] cancel",
        ),
        super::EditKind::Note => (
            " note: ".to_string(),
            Color::Yellow,
            "note: ",
            "[Enter] save (empty removes the note)   [Esc] cancel",
        ),
    };
    let w = 64.min(area.width.saturating_sub(4));
    let h = 7.min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(color))
        .title(Line::from(vec![
            Span::styled(title, Style::new().fg(TEXT)),
            Span::styled(
                format!("{} MHz ", app.ctx.freq_label(edit.freq)),
                Style::new().fg(FREQ).add_modifier(Modifier::BOLD),
            ),
        ]))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(rect);
    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(block, rect);
    let shown = fit(
        &edit.text,
        (inner.width as usize).saturating_sub(prompt.len() + 2),
    );
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            text(prompt),
            Span::styled(shown, Style::new().fg(NAME).add_modifier(Modifier::BOLD)),
            Span::styled("█", Style::new().fg(BRIGHT)),
        ]),
    ];
    match edit.kind {
        super::EditKind::Label { digi, .. } => lines.push(Line::from(vec![
            text("digi: "),
            if digi { bright("[x]") } else { text("[ ]") },
        ])),
        // The chosen duration in brackets: permanent (saved) or a timed exclusion.
        super::EditKind::Exclude { minutes, .. } => {
            let mut spans = vec![text("for: ")];
            let options = std::iter::once(None).chain(TIMED_MINUTES.iter().map(|&m| Some(m)));
            for m in options {
                let label = m.map_or("permanent".to_string(), |m| format!("{m} min"));
                spans.push(if m == minutes {
                    bright(format!("[{label}]"))
                } else {
                    text(format!(" {label} "))
                });
            }
            lines.push(Line::from(spans));
        }
        super::EditKind::Note => {}
    }
    lines.extend([
        match &edit.error {
            Some(e) => Line::from(Span::styled(
                fit(e, inner.width as usize),
                Style::new().fg(Color::LightRed),
            )),
            None => Line::from(""),
        },
        key_line(hint),
    ]);
    f.render_widget(Paragraph::new(lines), inner);
}

fn too_small(f: &mut Frame, inner: Rect, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(text(format!(
            "Terminal window too small: {}x{}, at least {MIN_W}x{MIN_H} is needed.",
            area.width, area.height
        )))
        .centered(),
        key_line("Scanning and audio keep running.  [q] quit").centered(),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// `lines` padded at the bottom (or cut) to exactly `height` lines.
fn fixed_height(mut lines: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    lines.truncate(height);
    lines.resize(height, Line::from(""));
    lines
}

/// A help line with keys in brackets highlighted, e.g. `[q] quit`.
fn key_line(s: &str) -> Line<'static> {
    let mut spans = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|c| open + c) else {
            break;
        };
        spans.push(text(&rest[..open]));
        spans.push(dim("["));
        spans.push(Span::styled(
            rest[open + 1..close].to_string(),
            Style::new().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ));
        spans.push(dim("]"));
        rest = &rest[close + 1..];
    }
    spans.push(text(rest));
    Line::from(spans)
}

/// Key help, in one or two lines depending on the width.
fn key_lines(width: usize, items: &[&str], short: &[&str]) -> Vec<Line<'static>> {
    let wrap = |items: &[&str]| {
        let mut lines: Vec<String> = vec![String::new()];
        for it in items {
            let last = lines.len() - 1;
            let sep = if lines[last].is_empty() { "" } else { "  " };
            if lines[last].chars().count() + sep.len() + it.chars().count() > width {
                lines.push(it.to_string());
            } else {
                lines[last].push_str(sep);
                lines[last].push_str(it);
            }
        }
        lines
    };
    let mut lines = wrap(items);
    if lines.len() > 2 {
        lines = wrap(short);
    }
    lines.iter().map(|l| key_line(l)).collect()
}

const MAIN_KEYS: [&str; 13] = [
    "[q] quit",
    "[p] pause",
    "[space] skip",
    "[x] exclude",
    "[X] exclude with neighbors",
    "[u] undo exclusion",
    "[+/-] squelch",
    "[m] mute",
    "[n] noise reduction",
    "[</>] NR strength",
    "[a] activity",
    "[e] exclusions",
    "[Tab] history",
];
const MAIN_KEYS_SHORT: [&str; 12] = [
    "[q] quit",
    "[p] pause",
    "[␣] skip",
    "[x/X] excl",
    "[u] undo",
    "[+/-] sq",
    "[m] mute",
    "[n] NR",
    "[</>] NR%",
    "[a] act",
    "[e] excl",
    "[Tab] hist",
];
/// Keys while a detection history row is selected.
const HISTORY_KEYS: [&str; 5] = [
    "[↑/↓] select",
    "[l] edit channel",
    "[r] rec",
    "[x/X] exclude",
    "[Tab/Esc] back",
];

fn main_screen(f: &mut Frame, inner: Rect, app: &App) {
    let width = inner.width as usize;
    let wide = inner.width + 4 >= BIG_W;
    let keys = if app.history_sel.is_some() {
        key_lines(width, &HISTORY_KEYS, &HISTORY_KEYS)
    } else if app.ctx.recordings.is_some() {
        // `[w] recordings` before `[Tab] history`.
        let with = |keys: &[&'static str], w: &'static str| {
            let mut v = keys.to_vec();
            v.insert(v.len() - 1, w);
            v
        };
        key_lines(
            width,
            &with(&MAIN_KEYS, "[w] recordings"),
            &with(&MAIN_KEYS_SHORT, "[w] recs"),
        )
    } else {
        key_lines(width, &MAIN_KEYS, &MAIN_KEYS_SHORT)
    };
    let keys = notice_or(app, keys);
    let progress = progress_lines(app, wide, width);
    let fixed = progress.len() + 1 + 2 + 1 + keys.len();
    let avail = inner.height as usize;
    // The central part has a fixed height for each layout, so the history and statistics do not
    // move when the scanner switches between the scanning and the lock view.
    let use_big =
        inner.width + 4 >= BIG_W && inner.height + 2 >= BIG_H && avail >= fixed + CENTER_BIG + 3;
    let height = if use_big { CENTER_BIG } else { CENTER_SMALL };
    let center = fixed_height(center_lines(app, width, use_big), height);
    let [top, sep1, mid, hist_title, hist, sep2, foot] = Layout::vertical([
        Constraint::Length(progress.len() as u16),
        Constraint::Length(1),
        Constraint::Length(center.len() as u16),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(keys.len() as u16),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(progress), top);
    rule(f, inner, sep1, ("", ""), None);
    f.render_widget(Paragraph::new(center), mid);
    if wide {
        let [h, s] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(STATS_W)]).areas(hist);
        let col = s.x - inner.x;
        rule(
            f,
            inner,
            hist_title,
            ("detection history", "statistics"),
            Some((col, '┬')),
        );
        history(f, h, app);
        let block = Block::new()
            .borders(Borders::LEFT)
            .border_style(Style::new().fg(DIM))
            .padding(ratatui::widgets::Padding::left(1));
        let s_inner = block.inner(s);
        f.render_widget(block, s);
        f.render_widget(Paragraph::new(stats_lines(app)), s_inner);
        rule(f, inner, sep2, ("", ""), Some((col, '┴')));
    } else {
        rule(f, inner, hist_title, ("detection history", ""), None);
        history(f, hist, app);
        rule(f, inner, sep2, ("", ""), None);
    }
    // The DSD-FME console over the history while the scanner is on a digi channel (not while a
    // history row is selected).
    if let (Some(console), Some(lock), None) = (&app.ctx.dsd_console, app.lock, app.history_sel)
        && lock.mode == Modulation::Digi
    {
        let area = Rect {
            x: inner.x,
            y: hist_title.y,
            width: inner.width,
            height: sep2.y + 1 - hist_title.y,
        };
        dsd_console(f, area, console);
    }
    f.render_widget(Paragraph::new(keys), foot);
}

/// A fresh notice (e.g. why `[r]` did nothing) in place of the key help.
fn notice_or(app: &App, keys: Vec<Line<'static>>) -> Vec<Line<'static>> {
    match &app.notice {
        Some((msg, at)) if at.elapsed() < NOTICE => {
            let mut lines = vec![Line::from(Span::styled(
                msg.clone(),
                Style::new().fg(Color::LightRed),
            ))];
            lines.resize(keys.len().max(1), Line::from(""));
            lines
        }
        _ => keys,
    }
}

/// Window with the last DSD-FME console lines, as DSD-FME prints them (without colors).
fn dsd_console(f: &mut Frame, area: Rect, lines: &crate::dsd_fme::ConsoleLines) {
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::LightCyan))
        .title(Line::from(text(" DSD-FME console ")));
    let inner = block.inner(area);
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(block, area);
    let (n, w) = (inner.height as usize, inner.width as usize);
    let shown: Vec<Line<'static>> = lines
        .lock()
        .map(|l| {
            l.iter()
                .skip(l.len().saturating_sub(n))
                .map(|s| Line::from(text(fit(s, w))))
                .collect()
        })
        .unwrap_or_default();
    f.render_widget(Paragraph::new(shown), inner);
}

/// Scan position: the range, a bar as wide as the free space showing which part of the range
/// the current window covers, and the pass, window and cycle (in a narrow window also the
/// statistics in one line).
fn progress_lines(app: &App, wide: bool, width: usize) -> Vec<Line<'static>> {
    let ctx = &app.ctx;
    let Some(s) = app.status else {
        return vec![Line::from(dim("starting…"))];
    };
    let w = s.window.min(ctx.window_range.len().saturating_sub(1));
    let r = ctx.window_range.get(w).copied().unwrap_or(0);
    let label = ctx.ranges.get(r);
    let range = label.map_or("", |l| l.span.as_str());
    let name = ctx.window_names.get(w).map_or("", String::as_str);
    // Several windows since the last refresh: a single window number would be a random sample.
    let sweeping = s.visited > 1;
    let (pass, number) = if sweeping {
        ("-", "-")
    } else {
        name.split_at(name.len().min(1))
    };
    let head = format!("range: {}/{}  {range}  ", r + 1, ctx.ranges.len());
    let tail = if wide {
        format!("  pass: {pass}   window: {number}   cycle: {}", s.cycles)
    } else {
        format!("  pass: {pass}  window: {number}  cycle: {}", s.cycles)
    };
    // The bar takes all the free space between the range and the information at the end.
    let bar_w = width
        .saturating_sub(head.chars().count() + tail.chars().count() + 2)
        .max(4);
    // Highlighted columns: the current window, and while sweeping every window of this range
    // measured since the last refresh.
    let n = ctx.window_names.len().max(1);
    let mut lit = vec![false; bar_w];
    if let Some(l) = label.filter(|l| l.stop > l.start) {
        let span = (l.stop - l.start) as f64;
        let col = |hz: u64| {
            (((hz.saturating_sub(l.start)) as f64 / span) * bar_w as f64).floor() as usize
        };
        for k in 0..s.visited.max(1) {
            let i = (w + n - k) % n;
            if ctx.window_range.get(i) != Some(&r) {
                continue;
            }
            if let Some(&(first, last)) = ctx.window_span.get(i) {
                let from = col(first).min(bar_w - 1);
                let to = col(last).clamp(from + 1, bar_w);
                lit[from..to].iter_mut().for_each(|c| *c = true);
            }
        }
    }
    let mut spans = vec![text(head), dim("[")];
    let mut i = 0;
    while i < bar_w {
        let on = lit[i];
        let len = lit[i..].iter().take_while(|&&c| c == on).count();
        spans.push(if on {
            Span::styled("█".repeat(len), Style::new().fg(Color::Cyan))
        } else {
            dim("─".repeat(len))
        });
        i += len;
    }
    spans.push(dim("]"));
    spans.push(text(tail));
    let mut lines = vec![Line::from(spans)];
    if !wide {
        lines.push(Line::from(text(fit(&stats_compact(app), width))));
    }
    lines
}
/// The statistics in one line (narrow window).
fn stats_compact(app: &App) -> String {
    let ctx = &app.ctx;
    let s = app.status;
    let rec = if ctx.recorder.is_some() {
        format!("  rec:{}", ctx.rec_state())
    } else {
        String::new()
    };
    format!(
        "{:.0} MHz/s {:.0} w/s  ovf:{}  to:{}  rej:{}  excl:{}  digi:{}{rec}  CPU:{}  RAM:{}",
        s.map_or(0.0, |s| s.mhz_per_s),
        s.map_or(0.0, |s| s.windows_per_s),
        s.map_or(0, |s| s.overflows),
        s.map_or(0, |s| s.timeouts),
        s.map_or(0, |s| s.rejected),
        ctx.exclusions.lock().map_or(0, |l| l.len()),
        digi_state(app),
        app.cpu.text("%", "/"),
        app.cpu
            .ram
            .map_or("-".to_string(), |b| format!("{}M", b >> 20)),
    )
}

/// Digi output state for the statistics: `off`, `waiting`, `connected`, `voice` (decoded voice
/// playing), or for DSD-FME started by rfscan `restarting` and `failed`.
fn digi_state(app: &App) -> &'static str {
    let ord = std::sync::atomic::Ordering::Relaxed;
    match app.ctx.dsd.as_deref().map(DsdState::load) {
        Some(DsdState::Failed) => return "failed",
        Some(DsdState::Restarting) => return "restarting",
        _ => {}
    }
    if app.ctx.voice.as_ref().is_some_and(|v| v.playing()) {
        return "voice";
    }
    match &app.ctx.digi {
        None => "off",
        Some((_, st)) if st.connected.load(ord) => "connected",
        Some(_) => "waiting",
    }
}
fn stats_lines(app: &App) -> Vec<Line<'static>> {
    let ctx = &app.ctx;
    let s = app.status;
    let excl = ctx.exclusions.lock().map_or(0, |l| l.len());
    let item =
        |label: &str, value: String| Line::from(vec![text(format!("{label}: ")), bright(value)]);
    vec![
        item(
            "speed",
            format!(
                "{:.0} MHz/s, {:.0} w/s",
                s.map_or(0.0, |s| s.mhz_per_s),
                s.map_or(0.0, |s| s.windows_per_s)
            ),
        ),
        item(
            "ovf / timeouts",
            format!(
                "{} / {}",
                s.map_or(0, |s| s.overflows),
                s.map_or(0, |s| s.timeouts)
            ),
        ),
        item(
            "rejected / excl.",
            format!("{} / {excl}", s.map_or(0, |s| s.rejected)),
        ),
        item("digi", digi_state(app).to_string()),
        item("rec", ctx.rec_state().to_string()),
        item(
            "underruns",
            ctx.underruns
                .load(std::sync::atomic::Ordering::Relaxed)
                .to_string(),
        ),
        item("CPU", app.cpu.text(" %", " / ")),
        item(
            "RAM",
            app.cpu
                .ram
                .map_or("-".to_string(), |b| format!("{} MB", b >> 20)),
        ),
    ]
}

/// The central part: the lock view or the scanning view.
fn center_lines(app: &App, width: usize, big: bool) -> Vec<Line<'static>> {
    match app.lock {
        Some(lock) => lock_lines(app, &lock, width, big),
        None => scan_lines(app, width, big),
    }
}

fn lock_lines(app: &App, lock: &LockView, width: usize, big: bool) -> Vec<Line<'static>> {
    let ctx = &app.ctx;
    let hang = lock.hang.is_some();
    let freq_style = if hang {
        Style::new().fg(DIM).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(FREQ).add_modifier(Modifier::BOLD)
    };
    let name_style = if hang {
        Style::new().fg(DIM).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(NAME).add_modifier(Modifier::BOLD)
    };
    // Airband: the channel number as the headline, the actual frequency in the extra line.
    let raster25 = ctx.is_raster25(lock.freq);
    let mhz = crate::airband::display(lock.freq, raster25);
    let airband = crate::airband::channel_number(lock.freq, raster25).is_some();
    let mode = lock.mode.name();
    let bw = format!("bw: {} kHz", khz(lock.bw));
    let headline = if big {
        bigtext::render(&mhz)
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                let unit = if i == bigtext::HEIGHT - 1 {
                    "  MHz"
                } else {
                    "     "
                };
                Line::from(vec![
                    Span::styled(row, freq_style),
                    Span::styled(unit, freq_style),
                ])
                .centered()
            })
            .collect()
    } else {
        vec![Line::from(vec![
            text("  ▶ "),
            Span::styled(format!("{mhz} MHz"), freq_style),
            text(format!("   {mode}  {bw}")),
        ])]
    };
    // Name line: empty when the channel has no name.
    let info = match ctx.names.get(lock.channel).cloned().flatten() {
        Some(n) => {
            let l = Line::from(Span::styled(
                format!("»  {}  «", fit(&n, width.saturating_sub(8))),
                name_style,
            ));
            if big { l.centered() } else { indent(l) }
        }
        None => Line::from(""),
    };
    let extra = if lock.mode == Modulation::Digi {
        if big {
            digi_line(app).centered()
        } else {
            indent(digi_line(app))
        }
    } else if airband {
        let l = Line::from(vec![
            text("frequency: "),
            bright(format!("{} MHz", crate::airband::actual(lock.freq))),
            dim(if raster25 {
                "  (25 kHz channel)"
            } else {
                "  (8.33 kHz channel)"
            }),
        ]);
        if big { l.centered() } else { indent(l) }
    } else {
        Line::from("")
    };
    let snr = lock.rssi_db - lock.noise_db;
    let mut m1 = Vec::new();
    if big {
        m1.push(bright(format!("  {mode}")));
        m1.push(text(format!("  {bw}      ")));
    } else {
        m1.push(text("  "));
    }
    m1.extend([
        text("SNR: "),
        bright(format!("{snr:5.1} dB")),
        text("    RSSI: "),
        bright(format!("{:6.1} dBFS", lock.rssi_db)),
        text("    noise: "),
        bright(format!("{:6.1} dBFS", lock.noise_db)),
    ]);
    if !big && let Some(agc) = lock.agc_db {
        m1.push(text("    AGC: "));
        m1.push(bright(format!("{agc:+.1} dB")));
    }
    let (squelch, hyst) = app
        .status
        .map_or((0.0, 0.0), |s| (s.squelch_snr, s.hysteresis));
    let bar_w = if big { 46 } else { 30 };
    let mut bar = vec![text("  SNR ")];
    bar.extend(snr_bar(bar_w, snr, squelch, squelch - hyst, hang));
    if big {
        bar.push(dim(format!(" 0-{SNR_SCALE_DB:.0} dB")));
    }
    match lock.hang {
        Some((since, pause)) => {
            let el = since.elapsed().min(pause);
            let cells = 10usize;
            let full = (el.as_secs_f64() / pause.as_secs_f64().max(1e-9) * cells as f64) as usize;
            bar.push(text("  pause: "));
            bar.push(Span::styled(
                "▓".repeat(full.min(cells)),
                Style::new().fg(Color::Yellow),
            ));
            bar.push(dim("░".repeat(cells - full.min(cells))));
            bar.push(text(format!(
                " {:.1} / {:.1} s",
                el.as_secs_f64(),
                pause.as_secs_f64()
            )));
        }
        None => bar.push(text(format!(
            "  threshold: {squelch} dB, exit: {} dB",
            squelch - hyst
        ))),
    }
    let end = lock.hang.map_or_else(Instant::now, |(since, _)| since);
    let mut m3 = vec![
        text("  max SNR: "),
        bright(format!("{:.1} dB", lock.peak_snr_db)),
        text("    max RSSI: "),
        bright(format!("{:.1} dBFS", lock.peak_rssi_db)),
        text("    duration: "),
        bright(mm_ss(end.saturating_duration_since(lock.start))),
    ];
    if big && let Some(agc) = lock.agc_db {
        m3.push(text("    audio AGC: "));
        m3.push(bright(format!("{agc:+.1} dB")));
    }
    slots(
        headline,
        info,
        extra,
        // An empty line separates the measurements from the name and frequency lines.
        vec![
            Line::from(""),
            Line::from(m1),
            Line::from(bar),
            Line::from(m3),
        ],
    )
}

/// The fixed slots of the central part, the same in the scanning and the lock view (user
/// decision): an empty line, the headline (large `SCANNING...` or frequency, or one line in the
/// compact layout), an empty line, the info line (channel name or pause message, otherwise
/// empty), the extra line (digi state, otherwise empty) and the details. Every element keeps
/// its row whether or not the optional lines have content.
fn slots(
    headline: Vec<Line<'static>>,
    info: Line<'static>,
    extra: Line<'static>,
    details: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    lines.extend(headline);
    lines.push(Line::from(""));
    lines.push(info);
    lines.push(extra);
    lines.extend(details);
    lines
}

/// Indents a line by two columns (compact layout).
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![text("  ")];
    spans.extend(line.spans);
    Line::from(spans)
}
/// SNR bar with the entry (┃) and exit (│) thresholds, 0..SNR_SCALE_DB.
fn snr_bar(width: usize, snr: f32, entry: f32, exit: f32, hang: bool) -> Vec<Span<'static>> {
    let pos = |db: f32| ((db / SNR_SCALE_DB).clamp(0.0, 1.0) * (width - 1) as f32).round() as usize;
    let filled = ((snr / SNR_SCALE_DB).clamp(0.0, 1.0) * width as f32).round() as usize;
    let fill = if hang {
        DIM
    } else if snr >= entry {
        Color::LightGreen
    } else if snr >= exit {
        Color::Yellow
    } else {
        TEXT
    };
    let (pe, px) = (pos(entry), pos(exit));
    let mut spans = vec![dim("[")];
    for i in 0..width {
        spans.push(if i == pe {
            Span::styled("┃", Style::new().fg(BRIGHT))
        } else if i == px {
            Span::styled("│", Style::new().fg(Color::Yellow))
        } else if i < filled {
            Span::styled("█", Style::new().fg(fill))
        } else {
            dim("░")
        });
    }
    spans.push(dim("]"));

    spans
}

/// `[ DIGI ]` marker with the DSD-FME connection state.
fn digi_line(app: &App) -> Line<'static> {
    let mut l = vec![Span::styled(
        "[ DIGI ]",
        Style::new()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    )];
    match &app.ctx.digi {
        Some((addr, st)) => {
            let ord = std::sync::atomic::Ordering::Relaxed;
            let connected = st.connected.load(ord);
            // DSD-FME started by rfscan listens on a port chosen by the system: no address.
            let dsd = app.ctx.dsd.as_deref().map(DsdState::load);
            l.push(text(match dsd {
                Some(_) => "   DSD-FME   ".to_string(),
                None => format!("   DSD-FME {addr}   "),
            }));
            l.push(match dsd {
                Some(DsdState::Failed) => Span::styled(
                    "● off (too many restarts)",
                    Style::new().fg(Color::LightRed),
                ),
                Some(DsdState::Restarting) => {
                    Span::styled("● restarting", Style::new().fg(Color::Yellow))
                }
                _ if connected => Span::styled("● connected", Style::new().fg(Color::LightGreen)),
                _ => Span::styled("● waiting for DSD-FME", Style::new().fg(Color::Yellow)),
            });
            if let Some(v) = &app.ctx.voice {
                let rate = v.rate.load(ord);
                let stereo = if v.channels.load(ord) == 2 {
                    " stereo"
                } else {
                    ""
                };
                l.push(text("   voice: "));
                l.push(if v.playing() {
                    bright(if rate > 0 {
                        format!("playing ({} kHz{stereo})", rate / 1000)
                    } else {
                        "playing".to_string()
                    })
                } else {
                    text("-")
                });
            }
            l.push(text(format!(
                "   sent: {:.1} MB   dropped: {}",
                st.bytes_sent.load(ord) as f64 / 1e6,
                st.dropped_chunks.load(ord)
            )));
        }
        None => l.push(text("   digi output off")),
    }
    Line::from(l)
}

/// The scanning view: a large (or, in a small window, normal) `SCANNING` with dots appearing
/// one after another, and the mini-spectrum of the last window. The progress is in the top line.
fn scan_lines(app: &App, width: usize, big: bool) -> Vec<Line<'static>> {
    let paused = app.status.is_some_and(|s| s.state == ScanState::Paused);
    let style = if paused {
        Style::new().fg(DIM).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    };
    // 0..3 dots, a new one every 400 ms; paused: no dots.
    let dots = if paused {
        0
    } else {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        (ms / 400 % 4) as usize
    };
    let word = format!("SCANNING{}{}", ".".repeat(dots), " ".repeat(3 - dots));
    let headline = if big {
        bigtext::render(&word)
            .into_iter()
            .map(|row| Line::from(Span::styled(row, style)).centered())
            .collect()
    } else {
        vec![Line::from(vec![text("  "), Span::styled(word, style)])]
    };
    // Info line: the pause message (or a note before the first measurement), otherwise empty.
    let info = if paused {
        let mut l = vec![Span::styled(
            "PAUSED",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )];
        l.extend(key_line(": press [p] to resume").spans);
        Some(Line::from(l))
    } else if app.status.is_none() {
        Some(Line::from(dim("waiting for the first measurement…")))
    } else {
        None
    };
    let info = match info {
        Some(l) if big => l.centered(),
        Some(l) => indent(l),
        None => Line::from(""),
    };
    let details = match &app.spectrum {
        Some(sp) => spectrum_lines(app, sp, width),
        None => vec![Line::from(dim("   waiting for the first window…"))],
    };
    slots(headline, info, Line::from(""), details)
}
fn spectrum_lines(app: &App, sp: &Spectrum, width: usize) -> Vec<Line<'static>> {
    let squelch = app.status.map_or(f32::INFINITY, |s| s.squelch_snr);
    let name = app
        .ctx
        .window_names
        .get(sp.window)
        .cloned()
        .unwrap_or_default();
    let cols = width.saturating_sub(6).max(10);
    let head = format!(
        "   window: {name}  {}-{} MHz",
        format_mhz(sp.first_hz),
        format_mhz(sp.last_hz)
    );
    let now = format!("now: {} MHz   ", format_mhz(sp.lo_hz));
    let pad = width.saturating_sub(head.chars().count() + now.chars().count());
    const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut bars = vec![text("   ")];
    for c in 0..cols {
        let b = (c * sp.bins / cols).min(sp.bins - 1);
        let snr = sp.snr_db[b];
        let level = ((snr / SPECTRUM_TOP_DB).clamp(0.0, 1.0) * 7.0).round() as usize;
        let color = if sp.excluded[b] && !snr.is_finite() {
            DIM
        } else if snr >= squelch {
            Color::LightGreen
        } else if sp.excluded[b] {
            DIM
        } else {
            Color::Cyan
        };
        bars.push(Span::styled(
            LEVELS[if snr.is_finite() { level } else { 0 }].to_string(),
            Style::new().fg(color),
        ));
    }
    // Axis: start, LO marker, end.
    let span = (sp.last_hz.saturating_sub(sp.first_hz)).max(1) as f64;
    let lo_col = (((sp.lo_hz as f64 - sp.first_hz as f64) / span).clamp(0.0, 1.0)
        * (cols - 1) as f64)
        .round() as usize;
    let left = format!("{:.1}", sp.first_hz as f64 / 1e6);
    let right = format!("{:.1} MHz", sp.last_hz as f64 / 1e6);
    let mut axis: Vec<char> = vec![' '; cols];
    for (i, ch) in left.chars().enumerate().take(cols) {
        axis[i] = ch;
    }
    let rs = cols.saturating_sub(right.chars().count());
    for (i, ch) in right.chars().enumerate() {
        if rs + i < cols {
            axis[rs + i] = ch;
        }
    }
    if lo_col + 4 < rs && lo_col > left.len() + 1 {
        for (i, ch) in "┊ LO".chars().enumerate() {
            axis[lo_col + i] = ch;
        }
    }
    let strongest = match sp.strongest {
        Some((f, snr)) => format!(
            "   strongest channel: {} MHz, SNR: {snr:.1} dB",
            app.ctx.freq_label(f)
        ),
        None => String::new(),
    };
    vec![
        Line::from(vec![text(head), text(" ".repeat(pad)), text(now)]),
        Line::from(bars),
        Line::from(dim(format!("   {}", axis.into_iter().collect::<String>()))),
        Line::from(text(format!(
            "   noise: {:.1} dBFS   SNR threshold: {squelch} dB{strongest}",
            sp.noise_db
        ))),
    ]
}

/// Detection history table (the title is drawn by the caller).
fn history(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let rows = (area.height as usize).saturating_sub(1);
    let fixed = 8 + 2 + 9 + 2 + 4 + 2 + 3 + 2 + 2 + 5 + 2 + 6 + 2 + 6;
    let name_w = width.saturating_sub(fixed).clamp(4, 30);
    let row = |time: &str,
               mhz: &str,
               mode: &str,
               rec: &str,
               name: &str,
               snr: &str,
               rssi: &str,
               dur: &str| {
        format!(
            "{time:<8}  {mhz:>9}  {mode:<4}  {rec:<3}  {:<name_w$}  {snr:>5}  {rssi:>6}  {dur:>6}",
            fit(name, name_w)
        )
    };
    let mut lines = vec![Line::from(dim(row(
        "time", "MHz", "mode", "rec", "name", "SNR", "RSSI", "dur.",
    )))];
    let sel = app
        .history_sel
        .map(|s| s.min(app.history.len().saturating_sub(1)));
    let start = sel.map_or(0, |s| scroll_start(s, app.history.len(), rows));
    for (i, h) in app.history.iter().enumerate().skip(start).take(rows) {
        let name = app
            .ctx
            .names
            .get(h.channel)
            .cloned()
            .flatten()
            .unwrap_or_default();
        lines.push(selected(
            row(
                &h.at.format("%H:%M:%S").to_string(),
                &app.ctx.freq_label(h.freq),
                h.mode.name(),
                if app.ctx.rec_near(h.freq) { " ✓" } else { "" },
                &name,
                &format!("{:.1}", h.peak_snr_db),
                &format!("{:.1}", h.peak_rssi_db),
                &format!("{:.1}s", h.duration.as_secs_f64()),
            ),
            sel == Some(i),
        ));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// A scrolled list: the visible slice start so that `sel` is on screen.
fn scroll_start(sel: usize, len: usize, rows: usize) -> usize {
    if rows == 0 || len <= rows {
        0
    } else {
        sel.saturating_sub(rows - 1).min(len - rows)
    }
}

fn selected(line: String, is_sel: bool) -> Line<'static> {
    if is_sel {
        Line::from(Span::styled(
            line,
            Style::new().fg(Color::Black).bg(Color::Cyan),
        ))
    } else {
        Line::from(text(line))
    }
}

/// Keys of the activity screen.
const ACTIVITY_KEYS: [&str; 10] = [
    "[↑↓] scroll",
    "[←→] sort",
    "[s] reverse",
    "[l] edit channel",
    "[r] rec",
    "[x] exclude",
    "[d] delete",
    "[c] clear",
    "[a/Esc] back",
    "[q] quit",
];

fn activity_screen(f: &mut Frame, inner: Rect, app: &App) {
    let rows = app.activity_rows();
    let width = inner.width as usize;
    let keys = notice_or(app, key_lines(width, &ACTIVITY_KEYS, &ACTIVITY_KEYS));
    let list_h = (inner.height as usize).saturating_sub(2 + keys.len());
    let name_w = width
        .saturating_sub(4 + 10 + 2 + 5 + 2 + 3 + 2 + 2 + 9 + 2 + 9 + 2 + 6 + 2 + 16)
        .clamp(5, 40);
    let fmt = |sel: &str,
               mhz: &str,
               mode: &str,
               rec: &str,
               name: &str,
               total: &str,
               session: &str,
               hits: &str,
               last: &str| {
        format!(
            "{sel:<2}{mhz:>10}  {mode:<5}  {rec:<3}  {:<name_w$}  {total:>9}  {session:>9}  {hits:>6}  {last}",
            fit(name, name_w)
        )
    };
    let sel = app.activity_sel.min(rows.len().saturating_sub(1));
    let start = scroll_start(sel, rows.len(), list_h);
    // Column headers, the sorted one with its direction.
    let sort = app.activity_sort;
    let head = |column: super::ActivityColumn, label: &str| {
        if column == sort.column {
            format!("{label}{}", if sort.descending { "▼" } else { "▲" })
        } else {
            label.to_string()
        }
    };
    use super::ActivityColumn as C;
    let mut lines = vec![
        separator(
            inner.width,
            &format!("activity ({} frequencies)", rows.len()),
        ),
        Line::from(dim(fmt(
            "",
            &head(C::Freq, "MHz"),
            &head(C::Mode, "mode"),
            "rec",
            &head(C::Name, "name"),
            &head(C::Total, "total"),
            &head(C::Session, "session"),
            &head(C::Hits, "hits"),
            &head(C::Last, "last"),
        ))),
    ];
    for (i, r) in rows.iter().enumerate().skip(start).take(list_h) {
        let last = r.last.replace('T', " ");
        lines.push(selected(
            fmt(
                if i == sel { "▶" } else { "" },
                &app.ctx.freq_label(r.freq),
                r.mode.unwrap_or(""),
                if r.rec { " ✓" } else { "" },
                r.name.as_deref().unwrap_or(""),
                &h_mm_ss(r.total),
                &h_mm_ss(r.session),
                &r.hits.to_string(),
                last.get(..16).unwrap_or(&last),
            ),
            i == sel,
        ));
    }
    let [list, keys_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(keys.len() as u16)]).areas(inner);
    f.render_widget(Paragraph::new(lines), list);
    f.render_widget(Paragraph::new(keys), keys_area);
}

const REC_CHANNEL_KEYS: [&str; 4] = ["[↑↓] scroll", "[Enter] files", "[w/Esc] back", "[q] quit"];
const REC_FILE_KEYS: [&str; 5] = [
    "[↑↓] scroll",
    "[Enter] play / stop",
    "[d] delete",
    "[Esc] channels",
    "[q] quit",
];

/// File size, e.g. `1.2 MB`.
fn size(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{} kB", bytes.div_ceil(1 << 10))
    }
}

/// The recordings: the channels, or the files of one channel.
fn recordings_screen(f: &mut Frame, inner: Rect, app: &App) {
    let width = inner.width as usize;
    let mut lines = Vec::new();
    let (keys, list_len) = match app.rec_channel {
        None => {
            let chans = app.ctx.rec_channels();
            let keys = key_lines(width, &REC_CHANNEL_KEYS, &REC_CHANNEL_KEYS);
            let list_h = (inner.height as usize).saturating_sub(2 + keys.len());
            let name_w = width
                .saturating_sub(2 + 10 + 2 + 2 + 6 + 2 + 9 + 2 + 16)
                .clamp(5, 40);
            lines.push(separator(
                inner.width,
                &format!("recordings ({} channels)", chans.len()),
            ));
            lines.push(Line::from(dim(format!(
                "  {:>10}  {:<name_w$}  {:>6}  {:>9}  last",
                "MHz", "name", "files", "total"
            ))));
            let sel = app.rec_sel.min(chans.len().saturating_sub(1));
            for (i, c) in chans
                .iter()
                .enumerate()
                .skip(scroll_start(sel, chans.len(), list_h))
                .take(list_h)
            {
                lines.push(selected(
                    format!(
                        "{:<2}{:>10}  {:<name_w$}  {:>6}  {:>9}  {}",
                        if i == sel { "▶" } else { "" },
                        c.label,
                        fit(c.name.as_deref().unwrap_or(""), name_w),
                        c.count,
                        h_mm_ss(c.total),
                        c.last
                    ),
                    i == sel,
                ));
            }
            (keys, chans.len())
        }
        Some(ch) => {
            let files = app.ctx.rec_files(ch);
            let keys = key_lines(width, &REC_FILE_KEYS, &REC_FILE_KEYS);
            let list_h = (inner.height as usize).saturating_sub(2 + keys.len());
            let chan = app.ctx.rec_channels().into_iter().find(|c| c.id == ch);
            let playing = app.ctx.player.as_ref().and_then(|p| p.playing());
            lines.push(separator(
                inner.width,
                &format!(
                    "recordings: {} MHz {}({} files{})",
                    chan.as_ref().map_or("-", |c| c.label.as_str()),
                    chan.as_ref()
                        .and_then(|c| c.name.as_deref())
                        .map_or_else(String::new, |n| format!("{n} ")),
                    chan.as_ref().map_or(0, |c| c.count),
                    if chan.as_ref().is_some_and(|c| c.count > files.len()) {
                        format!(", the newest {} listed", files.len())
                    } else {
                        String::new()
                    }
                ),
            ));
            lines.push(Line::from(dim(format!(
                "  {:<19}  {:>9}  {:>9}",
                "time", "length", "size"
            ))));
            let sel = app.rec_file_sel.min(files.len().saturating_sub(1));
            for (i, r) in files
                .iter()
                .enumerate()
                .skip(scroll_start(sel, files.len(), list_h))
                .take(list_h)
            {
                let state = if playing == Some(r.id) {
                    "  ▶ playing"
                } else if r.writing {
                    "  ● recording"
                } else {
                    ""
                };
                lines.push(selected(
                    format!(
                        "{:<2}{:<19}  {:>9}  {:>9}{state}",
                        if i == sel { "▶" } else { "" },
                        r.time,
                        r.seconds.map_or_else(|| "-".to_string(), h_mm_ss),
                        size(r.bytes)
                    ),
                    i == sel,
                ));
            }
            (keys, files.len())
        }
    };
    if list_len == 0 {
        lines.push(Line::from(dim("  (no recordings)")));
    }
    let [body, keys_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(keys.len() as u16)]).areas(inner);
    f.render_widget(Paragraph::new(lines), body);
    f.render_widget(Paragraph::new(keys), keys_area);
}

const EXCLUSION_KEYS: [&str; 6] = [
    "[↑↓] scroll",
    "[Enter] edit note",
    "[d] delete",
    "[s] keep a timed one",
    "[e/Esc] back",
    "[q] quit",
];

fn exclusions_screen(f: &mut Frame, inner: Rect, app: &App) {
    let list = app.exclusion_list();
    let keys = key_lines(inner.width as usize, &EXCLUSION_KEYS, &EXCLUSION_KEYS);
    let list_h = (inner.height as usize).saturating_sub(2 + keys.len());
    let sel = app.exclusions_sel.min(list.len().saturating_sub(1));
    let start = scroll_start(sel, list.len(), list_h);
    let timed = list.iter().filter(|e| e.until.is_some()).count();
    let mut lines = vec![separator(
        inner.width,
        &format!(
            "exclusions ({}{}): {}",
            list.len(),
            if timed > 0 {
                format!(", {timed} timed")
            } else {
                String::new()
            },
            app.ctx.exclusions_path.display()
        ),
    )];
    // Columns: frequency, time left (timed exclusions), note (the comment in the file), channel
    // name (if known).
    let width = inner.width as usize;
    let note_w = width.saturating_sub(2 + 14 + 4 + 9 + 3 + 24).clamp(10, 50);
    if !list.is_empty() {
        lines.push(Line::from(dim(format!(
            "  {:>14} {:<3}  {:<7}  {:<note_w$}   name",
            "", "MHz", "for", "note"
        ))));
    }
    let now = Instant::now();
    for (i, e) in list.iter().enumerate().skip(start).take(list_h) {
        let name = app.ctx.name_near(e.freq).unwrap_or("");
        let left = e
            .minutes_left(now)
            .map_or(String::new(), |m| format!("{m} min"));
        lines.push(selected(
            format!(
                "{:<2}{:>14} MHz  {left:<7}  {:<note_w$}   {name}",
                if i == sel { "▶" } else { "" },
                app.ctx.freq_label(e.freq),
                fit(e.note.as_deref().unwrap_or(""), note_w)
            ),
            i == sel,
        ));
    }
    if list.is_empty() {
        lines.push(Line::from(dim("  (no exclusions)")));
    }
    let [body, keys_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(keys.len() as u16)]).areas(inner);
    f.render_widget(Paragraph::new(lines), body);
    f.render_widget(Paragraph::new(keys), keys_area);
}

#[cfg(test)]
mod tests {
    use super::khz;

    #[test]
    fn khz_is_rounded() {
        assert_eq!(khz(25_000.0 / 3.0), "8.33");
        assert_eq!(khz(12_500.0), "12.5");
        assert_eq!(khz(25_000.0), "25");
        assert_eq!(khz(6_250.0), "6.25");
    }
}
