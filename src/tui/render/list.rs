//! The worktree list: a table, which columns survive the width, and each
//! row.
//!
//! A row reads left to right as the question it answers: is it up, which
//! branch, where, and anything unusual about it. A header row names every
//! column, so a cell never has to be decoded from help:
//!
//! ```text
//!      branch          │ changes     │ port   │ mode     │ git │ status
//! ─────────────────────┼─────────────┼────────┼──────────┼─────┼───────
//!  ▸ ● feat/checkout   │ uncommitted │ :17342 │ isolated │ ↑2  │
//!    ✗ fix/crash       │             │        │          │     │ failed
//! ```
//!
//! Faint lines divide the columns and rule the header off; the rows sit
//! one under the other, and the highlight is what marks the cursor's.
//!
//! A column is only there when some row has something in it, and its
//! title goes with it.
//!
//! The glyph says whether it runs, so the steady states have no word: a
//! word is kept for what needs reading — a failure, or an action in
//! flight — and goes at the end, where it moves nothing as it comes and
//! goes. The full URL is the detail pane's; the row has its port. Being
//! adopted is not on the row: most worktrees are, and it changes nothing
//! about what a key does — the detail pane and the remove dialog say it
//! where it matters.
//!
//! `uncommitted` sits right after the label and is
//! never shed: it is what stops a removal, and what somebody switching
//! branches most needs to see.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::actions;
use crate::state::{Aggregate, ServiceMode};
use crate::theme::{
    blue, border, cyan, green, highlight_bg, magenta, namespaced, orange, red, surface, text,
    text_dim, text_muted, yellow,
};
use crate::tui::app::{App, Mode};
use crate::worktree::{PrInfo, PrState, Worktree};

use super::{distinct_offsets, pad, text_width, truncate, truncate_distinct, truncate_line};

/// `"● "` — what the worktree is doing. Never shed: it is the question
/// this list exists to answer, and the one column that still answers it
/// when the words have gone.
const ROW_RUN_WIDTH: usize = 2;

/// `" ▸ "` — the list's own highlight column.
const ROW_CHROME_WIDTH: usize = 3;

/// Between two columns: a faint rule with a space either side, so each
/// value reads as under its title.
const COL_GAP: usize = 3;
const COL_RULE: &str = " │ ";

/// Below this a label stops being an identifier, so a column is dropped to
/// buy it back.
const ROW_NAME_MIN: usize = 12;

/// The optional columns of a row, in the order they are painted after the
/// label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    /// `uncommitted` for uncommitted changes. Never shed.
    Dirty,
    /// The directory name, when it is not just the branch with its
    /// slashes encoded — an adopted worktree somebody named themselves.
    Aside,
    /// The port of its URL, `:17342`, while something runs.
    Port,
    /// The other ports it was given, `api:17343`, for a worktree that
    /// runs more than one process. What a wide screen has room for, and
    /// the first thing a narrow one gives up.
    Ports,
    /// `◈` for a worktree with a public URL.
    Share,
    /// `isolated` for a worktree with private copies of the services.
    Mode,
    /// Ahead and behind, or prunable or locked.
    Signals,
    Pr,
    /// `failed`, or what is being done to it: `starting`, `stopping`…
    /// Empty for a worktree that simply runs or is stopped.
    Status,
}

impl Col {
    /// What the header row calls it.
    pub fn title(self) -> &'static str {
        match self {
            Col::Dirty => "changes",
            Col::Aside => "dir",
            Col::Port => "port",
            Col::Ports => "other ports",
            Col::Share => "public",
            Col::Mode => "mode",
            Col::Signals => "git",
            Col::Pr => "PR",
            Col::Status => "status",
        }
    }
}

/// The title over the glyph and the label.
const LABEL_TITLE: &str = "branch";

/// Left to right.
const LAYOUT: [Col; 9] = [
    Col::Dirty,
    Col::Aside,
    Col::Port,
    Col::Ports,
    Col::Share,
    Col::Mode,
    Col::Signals,
    Col::Pr,
    Col::Status,
];

/// The order columns give way in when the label is too narrow to read:
/// the least useful first, the status word last.
const SHED: [Col; 8] = [
    Col::Ports,
    Col::Aside,
    Col::Signals,
    Col::Pr,
    Col::Mode,
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
        ROW_CHROME_WIDTH
            + ROW_RUN_WIDTH
            + shown.iter().map(|&c| width_of(c) + COL_GAP).sum::<usize>()
    };
    let mut shown: Vec<Col> = LAYOUT.into_iter().filter(|&c| width_of(c) > 0).collect();
    for col in SHED {
        if list_width.saturating_sub(used(&shown)) >= ROW_NAME_MIN {
            break;
        }
        shown.retain(|&c| c != col);
    }
    shown
}

/// One row's cells, before the list decides which columns it can afford.
struct RowCells {
    glyph: (&'static str, Color),
    label: String,
    /// Whether anything of it is up, or being done to it: its label is
    /// full brightness, and a stopped one's recedes, so the eye goes to
    /// the few rows that run in a list of thirty.
    live: bool,
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
    let pending = app.pending_on(&wt.name);
    let (glyph, word, color) = match pending {
        // What is being done to it outranks what it was doing: a stop in
        // flight is not `running`.
        Some(_) if app.awaiting_answer() => ("? ", Some("waiting"), orange()),
        Some(pending) => ("◌ ", Some(pending.kind.verb()), yellow()),
        None => {
            let (glyph, color) = run_marker(phase.as_ref());
            // Running and stopped are the glyph's to say; the rest have
            // something worth reading.
            let word = match &phase {
                Some(Aggregate::Starting { .. }) => Some("starting"),
                Some(Aggregate::Failed { .. }) => Some("failed"),
                _ => None,
            };
            (glyph, word, color)
        }
    };
    if let Some(word) = word {
        let mut style = Style::new().fg(color);
        if matches!(phase, Some(Aggregate::Failed { .. })) && pending.is_none() {
            style = style.add_modifier(Modifier::BOLD);
        }
        cells.push((Col::Status, word.to_string(), style));
    }

    // Only while something runs: a stopped or failed worktree keeps its
    // ports, but one on its row would promise a page that is not there.
    let up = matches!(
        phase,
        Some(Aggregate::Running { .. } | Aggregate::Starting { .. })
    );
    let url_port = app.url_of(&wt.name).as_deref().and_then(port_of);
    if up && let Some(port) = &url_port {
        cells.push((Col::Port, format!(":{port}"), Style::new().fg(cyan())));
    }
    if up
        && let Some(record) = app.record_for(&wt.name)
        && record.ports.len() > 1
    {
        let others = record
            .ports
            .iter()
            .filter(|(_, port)| url_port.as_deref() != Some(port.to_string().as_str()))
            .map(|(role, port)| format!("{role}:{port}"))
            .collect::<Vec<_>>()
            .join(" ");
        if !others.is_empty() {
            cells.push((Col::Ports, others, Style::new().fg(text_dim())));
        }
    }
    if app.public_url_of(&wt.name).is_some() {
        cells.push((Col::Share, "◈".to_string(), Style::new().fg(green())));
    }
    match app.record_for(&wt.name).map(|r| r.mode()) {
        Some(ServiceMode::Isolated) => cells.push((
            Col::Mode,
            ServiceMode::Isolated.word().to_string(),
            Style::new().fg(magenta()),
        )),
        Some(ServiceMode::Namespaced) => cells.push((
            Col::Mode,
            ServiceMode::Namespaced.word().to_string(),
            Style::new().fg(namespaced()),
        )),
        Some(ServiceMode::Shared) | None => {}
    }
    if wt.dirty == Some(true) && !wt.prunable && !wt.locked {
        cells.push((
            Col::Dirty,
            "uncommitted".to_string(),
            Style::new().fg(yellow()),
        ));
    }
    let drift = drift_text(wt);
    if !drift.is_empty() {
        cells.push((Col::Signals, drift, Style::new().fg(signal_color(wt))));
    }
    if let Some(pr) = app.pr_for(wt) {
        cells.push((Col::Pr, pr_chip(pr), Style::new().fg(pr_color(pr))));
    }
    RowCells {
        glyph: (glyph, color),
        label,
        live: phase.is_some() || pending.is_some(),
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
                .map(|(_, text, _)| text_width(text))
                .max()
                .unwrap_or(0);
            // A column with something in it is at least as wide as its
            // title; one with nothing is not there, title and all.
            let widest = if widest > 0 {
                widest.max(text_width(col.title()))
            } else {
                0
            };
            (col, widest)
        })
        .collect();
    // Text stops one cell short of the border, so the last column never
    // runs into it; rules and highlight bands still reach it.
    let full = list_area.width as usize;
    let width = full.saturating_sub(1);
    let mut shown = list_columns(width, &widths);
    // The label is as wide as the widest one, and no wider: on a wide
    // screen the room left over goes after the columns, not into a gap
    // between the branch and where it runs.
    let widest_label = rows
        .iter()
        .map(|row| text_width(&row.label))
        .max()
        .unwrap_or(0);
    let label_room = |shown: &[Col]| {
        let columns_width: usize = shown
            .iter()
            .map(|col| widths.iter().find(|(c, _)| c == col).map_or(0, |(_, w)| *w) + COL_GAP)
            .sum();
        width.saturating_sub(ROW_CHROME_WIDTH + ROW_RUN_WIDTH + columns_width)
    };
    // A branch cut short is the one thing the list exists to show. The
    // columns the detail pane repeats in full give way, least useful
    // first, before any label is cut.
    for col in [Col::Ports, Col::Aside, Col::Pr, Col::Signals] {
        if label_room(&shown) >= widest_label {
            break;
        }
        shown.retain(|&c| c != col);
    }
    let label_width = label_room(&shown).min(widest_label.max(ROW_NAME_MIN));
    let col_width = |col: &Col| widths.iter().find(|(c, _)| c == col).map_or(0, |(_, w)| *w);
    let grid = Style::new().fg(border());
    let lead = ROW_CHROME_WIDTH + ROW_RUN_WIDTH;
    // A rule across the whole pane, crossing each column line with `┼`.
    let rule = || {
        let mut out = "─".repeat(lead + label_width + 1);
        for col in &shown {
            out.push('┼');
            out.push_str(&"─".repeat(col_width(col) + 2));
        }
        let out = truncate(&out, full).trim_end_matches('…').to_string();
        let fill = full.saturating_sub(text_width(&out));
        Line::styled(format!("{out}{}", "─".repeat(fill)), grid)
    };
    // Spaces to the pane's edge, so a highlight or a header band is one
    // unbroken bar.
    let fill_to = |mut spans: Vec<Span<'static>>| {
        let line = truncate_line(Line::from(std::mem::take(&mut spans)), width);
        let used: usize = line.spans.iter().map(|s| text_width(&s.content)).sum();
        let mut spans = line.spans;
        spans.push(Span::raw(" ".repeat(full.saturating_sub(used))));
        Line::from(spans)
    };

    let height = list_area.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    // The header when there is room for rows under it; the rule under
    // the header only when it would not push a row off a short pane.
    if height >= 3 {
        let title_style = Style::new().fg(text_muted()).add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(
            format!(
                "{}{}",
                " ".repeat(lead),
                pad(&truncate(LABEL_TITLE, label_width), label_width)
            ),
            title_style,
        )];
        for col in &shown {
            let w = col_width(col);
            spans.push(Span::styled(COL_RULE, grid));
            spans.push(Span::styled(pad(&truncate(col.title(), w), w), title_style));
        }
        lines.push(fill_to(spans).style(Style::new().bg(surface())));
        if height >= 3 + rows.len().min(3) {
            lines.push(rule());
        }
    }
    let visible = height.saturating_sub(lines.len()).max(1);
    let selected = app.list_state.selected();
    let mut offset = app.list_state.offset();
    if let Some(sel) = selected {
        if sel < offset {
            offset = sel;
        } else if sel >= offset + visible {
            offset = sel + 1 - visible;
        }
    }
    offset = offset.min(rows.len().saturating_sub(visible));
    *app.list_state.offset_mut() = offset;

    // Where each label differs from its nearest neighbour, so thirty
    // branches that share a long prefix keep the part that tells them
    // apart.
    let labels: Vec<String> = rows.iter().map(|row| row.label.clone()).collect();
    let distinct = distinct_offsets(&labels);

    for (i, (row, &differs_at)) in rows.iter().zip(&distinct).enumerate().skip(offset) {
        if lines.len() + 1 > height {
            break;
        }
        let is_selected = selected == Some(i);
        let (glyph, color) = row.glyph;
        let mut spans = vec![
            // The cursor in the accent, not only the band behind it: a
            // highlight alone is easy to lose on a low-contrast screen.
            Span::styled(
                if is_selected { " ▸ " } else { "   " },
                Style::new().fg(blue()),
            ),
            Span::styled(glyph, Style::new().fg(color)),
            Span::styled(
                pad(
                    &truncate_distinct(&row.label, label_width, differs_at),
                    label_width,
                ),
                Style::new().fg(if row.live { text() } else { text_dim() }),
            ),
        ];
        for col in &shown {
            let w = col_width(col);
            let (text, style) = row
                .cell(*col)
                .map(|(_, text, style)| (text.as_str(), *style))
                .unwrap_or(("", Style::new()));
            spans.push(Span::styled(COL_RULE, grid));
            spans.push(Span::styled(pad(&truncate(text, w), w), style));
        }
        let mut line = fill_to(spans);
        if is_selected {
            line = line.style(Style::new().bg(highlight_bg()).add_modifier(Modifier::BOLD));
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), list_area);
}

/// The port of a URL, `17342` of `http://localhost:17342/app`, when it
/// names one.
pub(super) fn port_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let (_, port) = host.rsplit_once(':')?;
    (!port.is_empty() && port.chars().all(|c| c.is_ascii_digit())).then(|| port.to_string())
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
        return "prunable".to_string();
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

/// Gone is red; locked and uncommitted both need a look, which is yellow.
pub(super) fn signal_color(wt: &Worktree) -> Color {
    if wt.prunable {
        red()
    } else if wt.locked || wt.dirty == Some(true) {
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

/// GitHub's own colours for a pull request's state, so the chip reads the
/// way the pull request page does.
pub(super) fn pr_color(pr: &PrInfo) -> Color {
    match pr.state {
        PrState::Open if pr.draft => text_muted(),
        PrState::Open => green(),
        PrState::Merged => magenta(),
        PrState::Closed => red(),
    }
}
