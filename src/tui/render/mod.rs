//! Painting. Nothing here talks to git, the filesystem, or a subprocess.
//!
//! The TUI is used in tmux splits, so nothing may assume a wide terminal:
//! list rows shed columns, the footer sheds hints, popups keep a minimum
//! width, and text truncates rather than clipping at a pane edge.
//!
//! The frame layout and the text helpers every part shares live here; the
//! list, the detail pane, the log viewer and the header and footer each
//! have their own file.

mod chrome;
mod detail;
mod list;
mod log_viewer;
mod welcome;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::app::App;

use chrome::{header_height, render_footer, render_header};
use detail::render_detail;
use list::render_list;
pub(super) use list::run_marker;
use log_viewer::{render_inspect, render_log_viewer};
use welcome::render_welcome;

/// Below this the list and the detail pane stop both being readable side by
/// side — the list can no longer hold a name and the detail pane can no
/// longer hold `http://localhost:17000` — so they stack instead.
pub const SIDE_BY_SIDE_MIN_WIDTH: u16 = 72;

/// Stacking costs two extra border rows. Under this the panes would be a
/// couple of rows each, and a cramped side-by-side is the lesser evil.
const STACK_MIN_LIST_HEIGHT: u16 = 5;

const STACK_MIN_DETAIL_HEIGHT: u16 = 6;

/// List and detail rects for the body: side by side when there is width for
/// both, stacked when there is not. pando is used in a tmux split, so the
/// narrow case is the normal one.
pub fn body_layout(body: Rect) -> [Rect; 2] {
    let stack = body.width < SIDE_BY_SIDE_MIN_WIDTH
        && body.height >= STACK_MIN_LIST_HEIGHT + STACK_MIN_DETAIL_HEIGHT;
    if !stack {
        return Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
            .areas(body);
    }
    let list_height = (body.height * 2 / 5)
        .max(STACK_MIN_LIST_HEIGHT)
        .min(body.height.saturating_sub(STACK_MIN_DETAIL_HEIGHT));
    Layout::vertical([Constraint::Length(list_height), Constraint::Fill(1)]).areas(body)
}

pub fn render(f: &mut Frame, app: &mut App) {
    // The viewer takes the whole screen: the header and footer it would
    // share with the list are the two rows a long stack trace needs most.
    if app.log_view().is_some() {
        render_log_viewer(f, f.area(), app);
        if app.inspect.is_some() {
            render_inspect(f, f.area(), app);
        }
        paint_modal(f, app);
        return;
    }

    // The header grows for an error that does not fit on one row: the end
    // of an error is usually the part that says what to do.
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(header_height(app, f.area().width, f.area().height)),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_header(f, header, app);
    // No worktrees at all is the first run, and a welcome says more than
    // an empty box beside a pane about nothing.
    if app.worktrees.is_empty() {
        app.list_area = None;
        render_welcome(f, body, app);
    } else {
        let [list_area, detail_area] = body_layout(body);
        render_list(f, list_area, app);
        render_detail(f, detail_area, app);
    }
    render_footer(f, footer, app);

    paint_modal(f, app);
}

/// The open modal, on top of whatever is under it, keeping how far help
/// or messages can scroll for the keys that scroll them.
fn paint_modal(f: &mut Frame, app: &mut App) {
    let Some(modal) = &app.modal else {
        return;
    };
    if let Some(bottom) = super::modal::render_modal(f, f.area(), modal, app) {
        app.help_scroll_max = bottom;
    }
}

/// A centred popup of exactly `width` × `height`, clamped to the area.
pub fn centered_box(width: u16, height: u16, area: Rect) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width.min(area.width)),
        Constraint::Fill(1),
    ])
    .areas(middle);
    popup
}

/// A centred popup that keeps a minimum width, so a narrow split gets a
/// readable box rather than a sliver.
pub fn centered_rect(percent_x: u16, min_width: u16, height: u16, area: Rect) -> Rect {
    // In u32: a percentage of a width past 1092 overflows a u16 before the
    // division brings it back — a panic in debug, and silently the wrong
    // popup width in release.
    let target = (area.width as u32 * percent_x as u32 / 100) as u16;
    let width = target.max(min_width).min(area.width);
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height.min(area.height)),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .areas(middle);
    popup
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Cuts the middle out of a string that does not fit, keeping both ends:
/// the start of a path says where it is, the end says what it is.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max < 5 {
        return truncate(s, max);
    }
    let tail = (max - 1) / 2;
    let head = max - 1 - tail;
    let head: String = s.chars().take(head).collect();
    let tail: String = s.chars().skip(len - tail).collect();
    format!("{head}…{tail}")
}

/// A path as a person writes it: `~` for the home directory.
pub fn home_relative(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return shown;
    };
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => shown,
    }
}

/// Cuts a label so that what tells it apart from its neighbours survives.
///
/// `distinct_from` is where the label first differs from the most similar
/// other label in the list. When that is past what a plain cut would keep —
/// thirty branches called `feature/very-long-branch-name-number-N` — the
/// cut is made at the front instead, starting shortly before the part
/// that differs.
pub fn truncate_distinct(s: &str, max: usize, distinct_from: usize) -> String {
    /// Characters kept before the point of difference, so `number-12`
    /// reads as a number and not as a stray `12`.
    const CONTEXT: usize = 8;
    let len = s.chars().count();
    if len <= max || max < 4 || distinct_from + 1 < max {
        return truncate(s, max);
    }
    // A short leading segment — `feature/` — is kept: it is the kind of
    // branch, and cheap.
    let head: String = match s.find('/') {
        Some(slash) if s[..=slash].chars().count() <= max / 3 => s[..=slash].to_string(),
        _ => String::new(),
    };
    let head_len = head.chars().count();
    let room = max - head_len - 1;
    let start = distinct_from.saturating_sub(CONTEXT).max(head_len).min(len);
    if len - start <= room {
        // The rest fits after the ellipsis: keep as much of the end as the
        // width allows.
        let tail: String = s.chars().skip(len - room).collect();
        return format!("{head}…{tail}");
    }
    let middle: String = s.chars().skip(start).take(room - 1).collect();
    format!("{head}…{middle}…")
}

/// Where each label first differs from the most similar of the others: the
/// length of its longest common prefix with any of them. Sorted, the most
/// similar label is always a neighbour, so this is one sort and one pass.
pub fn distinct_offsets(labels: &[String]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..labels.len()).collect();
    order.sort_by(|&a, &b| labels[a].cmp(&labels[b]));
    let common = |a: &str, b: &str| a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    let mut offsets = vec![0; labels.len()];
    for (at, &i) in order.iter().enumerate() {
        let before = at
            .checked_sub(1)
            .map(|p| common(&labels[i], &labels[order[p]]))
            .unwrap_or(0);
        let after = order
            .get(at + 1)
            .map(|&n| common(&labels[i], &labels[n]))
            .unwrap_or(0);
        offsets[i] = before.max(after);
    }
    offsets
}

/// Word-wraps plain text to `width`, breaking a word only when it is wider
/// than a whole row. Always at least one row.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        let mut row_len = 0usize;
        for word in paragraph.split(' ') {
            let mut word: Vec<char> = word.chars().collect();
            loop {
                let needed = if row_len == 0 {
                    word.len()
                } else {
                    row_len + 1 + word.len()
                };
                if needed <= width {
                    if row_len > 0 {
                        row.push(' ');
                    }
                    row.extend(word.iter());
                    row_len = needed;
                    break;
                }
                // A word that fits a row of its own goes to the next one
                // whole. One wider than any row — a path, usually — breaks
                // after a `/` where it can, filling what is left of this
                // row first, and is only cut mid-name when it has none.
                let fits_alone = word.len() <= width;
                let room = if row_len > 0 {
                    width.saturating_sub(row_len + 1)
                } else {
                    width
                };
                let slash = if fits_alone {
                    None
                } else {
                    slash_break(&word, room)
                };
                if let Some(at) = slash {
                    let rest = word.split_off(at);
                    if row_len > 0 {
                        row.push(' ');
                    }
                    row.extend(word.iter());
                    rows.push(std::mem::take(&mut row));
                    row_len = 0;
                    word = rest;
                    continue;
                }
                if row_len > 0 {
                    rows.push(std::mem::take(&mut row));
                    row_len = 0;
                    continue;
                }
                // Wider than the row, and nowhere to break it: cut it,
                // carry the rest.
                let rest = word.split_off(width);
                rows.push(word.iter().collect());
                word = rest;
            }
        }
        rows.push(row);
    }
    rows
}

/// Where to break `word` so its first part fits in `room`: just after the
/// last `/` that leaves a part that fits. `None` when there is no such
/// `/`, or it would leave nothing on the row.
fn slash_break(word: &[char], room: usize) -> Option<usize> {
    let limit = room.min(word.len());
    (1..=limit).rev().find(|&at| word[at - 1] == '/')
}

fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// Cuts a styled line to `width` cells, keeping the styling of the spans
/// that survive.
pub fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= width {
        return line;
    }
    let mut remaining = width;
    let mut spans = Vec::new();
    for span in line.spans {
        if remaining == 0 {
            break;
        }
        let len = span.content.chars().count();
        if len <= remaining {
            remaining -= len;
            spans.push(span);
        } else {
            let cut = truncate(&span.content, remaining);
            remaining = 0;
            spans.push(Span::styled(cut, span.style));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests;
