//! The worktree list: which columns survive the width, and each row.
//!
//! A row reads left to right as the question it answers: which branch, is
//! it up, where, and anything unusual about it.
//!
//! ```text
//!  ▸ ● feat/checkout * running  http://localhost:17342  ◈ isolated  ↑2  ◍42
//!    ○ fix/typo        stopped
//! ```
//!
//! The `*` of a worktree with uncommitted changes sits against the label
//! and is never shed: it is what stops a removal, and what somebody
//! switching branches most needs to see.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph};

use crate::actions;
use crate::state::Aggregate;
use crate::theme::{
    border, cyan, green, highlight_bg, magenta, orange, red, text, text_dim, text_muted, yellow,
};
use crate::tui::app::{App, Mode};
use crate::worktree::{PrInfo, PrState, Worktree};

use super::{distinct_offsets, pad, truncate, truncate_distinct, truncate_line};

/// `"● "` — what the worktree is doing. Never shed: it is the question
/// this list exists to answer, and the one column that still answers it
/// when the words have gone.
const ROW_RUN_WIDTH: usize = 2;

/// `" ▸ "` — the list's own highlight column.
const ROW_CHROME_WIDTH: usize = 3;

/// Below this a label stops being an identifier, so a column is dropped to
/// buy it back.
const ROW_NAME_MIN: usize = 12;

/// The status column is never narrower than its commonest words
/// (`starting`, `stopping`, `stopped`), so a row going from `starting` to
/// `running` does not shift every column after it by one.
const STATUS_MIN_WIDTH: usize = 8;

/// The optional columns of a row, in the order they are painted after the
/// label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    /// `*` for uncommitted changes. Never shed.
    Dirty,
    /// The directory name, when it is not just the branch with its
    /// slashes encoded — an adopted worktree somebody named themselves.
    Aside,
    /// `running`, `stopped`, `stopping`…
    Status,
    /// The whole local URL.
    Url,
    /// Only its port, `:17342`: what the URL shrinks to first.
    Port,
    /// Every port it was given, `web 17342 api 17343`, for a worktree that
    /// runs more than one process. What a wide screen has room for, and
    /// the first thing a narrow one gives up.
    Ports,
    /// `◈` for a worktree with a public URL.
    Share,
    /// `isolated` for a worktree with private copies of the services.
    Mode,
    /// Ahead and behind, or gone or locked.
    Signals,
    Pr,
    /// A worktree pando did not create.
    Adopted,
}

/// Left to right.
const LAYOUT: [Col; 11] = [
    Col::Dirty,
    Col::Aside,
    Col::Status,
    Col::Url,
    Col::Port,
    Col::Ports,
    Col::Share,
    Col::Mode,
    Col::Signals,
    Col::Pr,
    Col::Adopted,
];

/// The order columns give way in when the label is too narrow to read:
/// the least useful first, the status word last. The URL gives way to its
/// port before either goes.
const SHED: [Col; 10] = [
    Col::Ports,
    Col::Aside,
    Col::Adopted,
    Col::Signals,
    Col::Pr,
    Col::Mode,
    Col::Url,
    Col::Port,
    Col::Share,
    Col::Status,
];

/// Which optional columns survive at this pane width, given each one's
/// widest cell (`0` for a column no row has anything in). Decided once for
/// the whole list, so every row sheds the same column and each column
/// keeps one straight edge.
pub fn list_columns(list_width: usize, widths: &[(Col, usize)]) -> Vec<Col> {
    let width_of = |col: Col| {
        widths
            .iter()
            .find(|(c, _)| *c == col)
            .map(|(_, w)| *w)
            .unwrap_or(0)
    };
    let used = |shown: &[Col]| {
        ROW_CHROME_WIDTH + ROW_RUN_WIDTH + shown.iter().map(|&c| width_of(c) + 1).sum::<usize>()
    };
    // The port stands in for the URL only once the URL has gone.
    let mut shown: Vec<Col> = LAYOUT
        .into_iter()
        .filter(|&c| c != Col::Port && width_of(c) > 0)
        .collect();
    for col in SHED {
        if list_width.saturating_sub(used(&shown)) >= ROW_NAME_MIN {
            break;
        }
        if let Some(at) = shown.iter().position(|&c| c == col) {
            shown.remove(at);
            if col == Col::Url && width_of(Col::Port) > 0 {
                shown.insert(at, Col::Port);
            }
        }
    }
    shown
}

/// One row's cells, before the list decides which columns it can afford.
struct RowCells {
    glyph: (&'static str, Color),
    label: String,
    cells: Vec<(Col, String, Style)>,
}

impl RowCells {
    fn cell(&self, col: Col) -> Option<&(Col, String, Style)> {
        self.cells.iter().find(|(c, _, _)| *c == col)
    }
}

fn row_cells(app: &App, wt: &Worktree) -> RowCells {
    let mut cells = Vec::new();
    let label = wt.branch.clone().unwrap_or_else(|| wt.name.clone());
    let aside = match &wt.branch {
        Some(branch) if actions::sanitize_branch_to_dir(branch) != wt.name => Some(wt.name.clone()),
        Some(_) => None,
        None => Some("detached".to_string()),
    };
    if let Some(aside) = aside {
        cells.push((Col::Aside, aside, Style::new().fg(text_muted())));
    }

    let phase = app.phase_of(&wt.name);
    let (glyph, word, color) = match app.pending_on(&wt.name) {
        // What is being done to it outranks what it was doing: a stop in
        // flight is not `running`.
        Some(_) if app.awaiting_answer() => ("? ", "waiting", orange()),
        Some(pending) => ("◌ ", pending.kind.verb(), yellow()),
        None => {
            let (glyph, color) = run_marker(phase.as_ref());
            (glyph, phase.as_ref().map_or("stopped", |p| p.word()), color)
        }
    };
    let mut status_style = Style::new().fg(color);
    if matches!(
        phase,
        Some(Aggregate::Failed { .. }) | Some(Aggregate::Running { .. })
    ) {
        status_style = status_style.add_modifier(Modifier::BOLD);
    }
    // Thirty `stopped` down one column drown out the two rows that are
    // up. Faint, so the word is there for whoever looks for it and the
    // eye goes to the ones that run, start or failed.
    if phase.is_none() && app.pending_on(&wt.name).is_none() {
        status_style = status_style.add_modifier(Modifier::DIM);
    }
    cells.push((Col::Status, word.to_string(), status_style));

    // Only while something runs: a stopped or failed worktree keeps its
    // ports, but a URL on its row would promise a page that is not there.
    if matches!(
        phase,
        Some(Aggregate::Running { .. } | Aggregate::Starting { .. })
    ) && let Some(url) = app.url_of(&wt.name)
    {
        let port = url
            .rsplit_once(':')
            .map(|(_, port)| format!(":{}", port.trim_end_matches('/')))
            .unwrap_or_default();
        cells.push((Col::Url, url, Style::new().fg(cyan())));
        if !port.is_empty() {
            cells.push((Col::Port, port, Style::new().fg(cyan())));
        }
    }
    if matches!(
        phase,
        Some(Aggregate::Running { .. } | Aggregate::Starting { .. })
    ) && let Some(record) = app.record_for(&wt.name)
        && record.ports.len() > 1
    {
        let ports = record
            .ports
            .iter()
            .map(|(role, port)| format!("{role} {port}"))
            .collect::<Vec<_>>()
            .join(" ");
        cells.push((Col::Ports, ports, Style::new().fg(text_dim())));
    }
    if app.public_url_of(&wt.name).is_some() {
        cells.push((Col::Share, "◈".to_string(), Style::new().fg(green())));
    }
    if app.record_for(&wt.name).is_some_and(|r| r.isolated) {
        cells.push((
            Col::Mode,
            "isolated".to_string(),
            Style::new().fg(magenta()),
        ));
    }
    if wt.dirty == Some(true) && !wt.prunable && !wt.locked {
        cells.push((Col::Dirty, "*".to_string(), Style::new().fg(yellow())));
    }
    let drift = drift_text(wt);
    if !drift.is_empty() {
        cells.push((Col::Signals, drift, Style::new().fg(signal_color(wt))));
    }
    if let Some(pr) = app.pr_for(wt) {
        cells.push((Col::Pr, pr_chip(pr), Style::new().fg(pr_color(pr))));
    }
    if !app.created_by_pando.get(&wt.name).copied().unwrap_or(false) {
        cells.push((
            Col::Adopted,
            "adopted".to_string(),
            Style::new().fg(text_muted()),
        ));
    }
    RowCells {
        glyph: (glyph, color),
        label,
        cells,
    }
}

pub(super) fn render_list(f: &mut Frame, area: Rect, app: &mut App) {
    let title = if app.filter.is_empty() {
        format!(" worktrees ({}) ", app.worktrees.len())
    } else {
        format!(
            " worktrees ({}/{}) ",
            app.filtered_indices.len(),
            app.worktrees.len()
        )
    };
    let block = Block::bordered()
        .title(Span::styled(title, Style::new().fg(text_dim())))
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let (filter_area, list_area) = if app.mode == Mode::Filter || !app.filter.is_empty() {
        let [fa, la] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        (Some(fa), la)
    } else {
        (None, inner)
    };
    app.list_area = Some(list_area);

    if let Some(fa) = filter_area {
        let cursor = if app.mode == Mode::Filter { "▏" } else { "" };
        let hint = if app.mode == Mode::Filter {
            "  ⏎ keep · esc clear"
        } else {
            "  / edits · esc clears"
        };
        f.render_widget(
            Paragraph::new(truncate_line(
                Line::from(vec![
                    Span::styled(
                        " / ",
                        Style::new().fg(orange()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(app.filter.clone(), Style::new().fg(text())),
                    Span::styled(cursor, Style::new().fg(orange())),
                    Span::styled(hint, Style::new().fg(text_muted())),
                ]),
                fa.width as usize,
            )),
            fa,
        );
    }

    if app.filtered_indices.is_empty() {
        let message = if app.filter.is_empty() {
            "no worktrees — press n to create one".to_string()
        } else {
            format!("no matches for \"{}\" — esc to clear", app.filter)
        };
        let [_, middle, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(list_area);
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate(&message, list_area.width as usize),
                Style::new().fg(text_muted()),
            ))
            .alignment(Alignment::Center),
            middle,
        );
        return;
    }

    let rows: Vec<RowCells> = app
        .filtered_indices
        .iter()
        .map(|&idx| row_cells(app, &app.worktrees[idx]))
        .collect();
    // Column widths are a list-wide decision, so every row sacrifices the
    // same column and each column keeps one straight edge.
    let widths: Vec<(Col, usize)> = LAYOUT
        .into_iter()
        .map(|col| {
            let widest = rows
                .iter()
                .filter_map(|row| row.cell(col))
                .map(|(_, text, _)| text.chars().count())
                .max()
                .unwrap_or(0);
            // Every row has a status, so the floor only ever widens it.
            let widest = if col == Col::Status {
                widest.max(STATUS_MIN_WIDTH)
            } else {
                widest
            };
            (col, widest)
        })
        .collect();
    let width = list_area.width as usize;
    let mut shown = list_columns(width, &widths);
    // The label is as wide as the widest one, and no wider: on a wide
    // screen the room left over goes after the columns, not into a gap
    // between the branch and whether it runs.
    let widest_label = rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0);
    let label_room = |shown: &[Col]| {
        let columns_width: usize = shown
            .iter()
            .map(|col| widths.iter().find(|(c, _)| c == col).map_or(0, |(_, w)| *w) + 1)
            .sum();
        width.saturating_sub(ROW_CHROME_WIDTH + ROW_RUN_WIDTH + columns_width)
    };
    // Every port is a nicety one running row brings to all thirty; a
    // branch cut short is the thing the list exists to show. The ports
    // give way before any label is cut.
    if label_room(&shown) < widest_label {
        shown.retain(|&col| col != Col::Ports);
    }
    let label_width = label_room(&shown).min(widest_label.max(ROW_NAME_MIN));
    // Where each label differs from its nearest neighbour, so thirty
    // branches that share a long prefix keep the part that tells them
    // apart.
    let labels: Vec<String> = rows.iter().map(|row| row.label.clone()).collect();
    let distinct = distinct_offsets(&labels);

    let items: Vec<ListItem> = rows
        .iter()
        .zip(&distinct)
        .map(|(row, &differs_at)| {
            let (glyph, color) = row.glyph;
            let mut spans = vec![
                Span::styled(glyph, Style::new().fg(color)),
                Span::styled(
                    pad(
                        &truncate_distinct(&row.label, label_width, differs_at),
                        label_width,
                    ),
                    Style::new().fg(text()),
                ),
            ];
            for col in &shown {
                let col_width = widths.iter().find(|(c, _)| c == col).map_or(0, |(_, w)| *w);
                let (text, style) = row
                    .cell(*col)
                    .map(|(_, text, style)| (text.as_str(), *style))
                    .unwrap_or(("", Style::new()));
                spans.push(Span::styled(
                    format!(" {}", pad(&truncate(text, col_width), col_width)),
                    style,
                ));
            }
            ListItem::new(truncate_line(Line::from(spans), width))
        })
        .collect();

    let list = List::new(items)
        .highlight_symbol(" ▸ ")
        .highlight_style(Style::new().bg(highlight_bg()).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, list_area, &mut app.list_state);
}

/// What a worktree is doing, in one cell.
pub(in crate::tui) fn run_marker(phase: Option<&Aggregate>) -> (&'static str, Color) {
    match phase {
        Some(Aggregate::Running { .. }) => ("● ", green()),
        Some(Aggregate::Starting { .. }) => ("◌ ", yellow()),
        Some(Aggregate::Failed { .. }) => ("✗ ", red()),
        None => ("○ ", text_muted()),
    }
}

/// How far a row's branch has drifted, or what is wrong with it: the
/// signals without the dirty mark, which has a column of its own.
pub(super) fn drift_text(wt: &Worktree) -> String {
    if wt.prunable || wt.locked {
        return signal_text(wt);
    }
    signal_text(wt).trim_start_matches('*').to_string()
}

/// The git state of a row, in one short cell: what is wrong first, then
/// how far the branch has drifted.
pub(super) fn signal_text(wt: &Worktree) -> String {
    if wt.prunable {
        return "gone".to_string();
    }
    if wt.locked {
        return "locked".to_string();
    }
    let mut out = String::new();
    if wt.dirty == Some(true) {
        out.push('*');
    }
    if let Some((ahead, behind)) = wt.ahead_behind {
        if ahead > 0 {
            out.push_str(&format!("↑{}", cap(ahead)));
        }
        if behind > 0 {
            out.push_str(&format!("↓{}", cap(behind)));
        }
    }
    out
}

pub(super) fn signal_color(wt: &Worktree) -> Color {
    if wt.prunable {
        red()
    } else if wt.locked {
        magenta()
    } else if wt.dirty == Some(true) {
        yellow()
    } else {
        text_dim()
    }
}

fn cap(n: u32) -> String {
    if n > 99 {
        "99+".to_string()
    } else {
        n.to_string()
    }
}

fn pr_chip(pr: &PrInfo) -> String {
    let mark = match pr.state {
        PrState::Open if pr.draft => "◌",
        PrState::Open => "◍",
        PrState::Merged => "✓",
        PrState::Closed => "✗",
    };
    format!("{mark}{}", pr.number)
}

pub(super) fn pr_color(pr: &PrInfo) -> Color {
    match pr.state {
        PrState::Open if pr.draft => text_muted(),
        PrState::Open => cyan(),
        PrState::Merged => green(),
        PrState::Closed => red(),
    }
}
