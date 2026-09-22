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

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

use super::app::App;

use chrome::{render_footer, render_header};
use detail::render_detail;
use list::render_list;
use log_viewer::{render_inspect, render_log_viewer};

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
        if let Some(modal) = &app.modal {
            super::modal::render_modal(f, f.area(), modal, app);
        }
        return;
    }

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_header(f, header, app);
    let [list_area, detail_area] = body_layout(body);
    render_list(f, list_area, app);
    render_detail(f, detail_area, app);
    render_footer(f, footer, app);

    if let Some(modal) = &app.modal {
        super::modal::render_modal(f, f.area(), modal, app);
    }
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
