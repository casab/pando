//! The worktree list: which columns survive the width, and each row.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph};

use crate::state::Aggregate;
use crate::theme::{
    border, cyan, green, highlight_bg, magenta, orange, red, text, text_dim, text_muted, yellow,
};
use crate::tui::app::{App, Mode};
use crate::worktree::{PrInfo, PrState, Worktree};

use super::{pad, truncate, truncate_line};

/// `"● "` — ownership marker.
const ROW_DOT_WIDTH: usize = 2;

/// `"● "` — what the dev process is doing. Never shed: it is the question
/// this list exists to answer.
const ROW_RUN_WIDTH: usize = 2;

/// `" ▸ "` — the list's own highlight column.
const ROW_CHROME_WIDTH: usize = 3;

/// `"◈ "` — the public-URL marker. Reserved list-wide, and only when some
/// row has one: a column of blanks costs every other column a cell.
const ROW_SHARE_WIDTH: usize = 2;

/// Below this a name stops being an identifier, so a column is dropped to
/// buy it back.
const ROW_NAME_MIN: usize = 12;

/// Widest branch column before it starts eating the name.
const ROW_BRANCH_MAX: usize = 28;

/// Which optional columns survive at this pane width. Decided once for the
/// whole list so every row sheds the same column and each column keeps one
/// straight edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListColumns {
    pub branch: bool,
    pub signals: bool,
    pub pr: bool,
}

/// Widest layout first; take the first that still leaves a readable name.
/// The PR column outlives everything else — review state is the list's most
/// requested signal — and the branch goes first, since the name is derived
/// from it.
pub fn list_columns(
    list_width: usize,
    branch_width: usize,
    signal_width: usize,
    pr_width: usize,
) -> ListColumns {
    let fixed = ROW_DOT_WIDTH + ROW_RUN_WIDTH + ROW_CHROME_WIDTH;
    let tiers = [
        ListColumns {
            branch: true,
            signals: true,
            pr: true,
        },
        ListColumns {
            branch: false,
            signals: true,
            pr: true,
        },
        ListColumns {
            branch: false,
            signals: false,
            pr: true,
        },
        ListColumns {
            branch: false,
            signals: false,
            pr: false,
        },
    ];
    for cols in tiers {
        let used = fixed
            + column(cols.branch, branch_width)
            + column(cols.signals, signal_width)
            + column(cols.pr, pr_width);
        if list_width.saturating_sub(used) >= ROW_NAME_MIN {
            return cols;
        }
    }
    tiers[tiers.len() - 1]
}

/// A reserved column costs its content plus one separating space.
fn column(shown: bool, width: usize) -> usize {
    if shown && width > 0 { width + 1 } else { 0 }
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
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " / ",
                    Style::new().fg(orange()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(app.filter.clone(), Style::new().fg(text())),
                Span::styled(cursor, Style::new().fg(orange())),
            ])),
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

    // Column widths are a list-wide decision, so every row sacrifices the
    // same column and each column keeps one straight edge.
    let mut branch_width = 0usize;
    let mut signal_width = 0usize;
    let mut pr_width = 0usize;
    let mut any_shared = false;
    for &idx in &app.filtered_indices {
        let wt = &app.worktrees[idx];
        branch_width = branch_width.max(branch_text(wt).chars().count().min(ROW_BRANCH_MAX));
        signal_width = signal_width.max(signal_text(wt).chars().count());
        if let Some(pr) = app.pr_for(wt) {
            pr_width = pr_width.max(pr_chip(pr).chars().count());
        }
        any_shared |= app.public_url_of(&wt.name).is_some();
    }
    // One cell, list-wide, and only when something is shared: a marker
    // every row pays for when no row has one is a column of blanks.
    let share_width = if any_shared { ROW_SHARE_WIDTH } else { 0 };
    let width = list_area.width as usize;
    let cols = list_columns(width, branch_width, signal_width, pr_width);
    let name_width = width.saturating_sub(
        ROW_DOT_WIDTH
            + ROW_RUN_WIDTH
            + ROW_CHROME_WIDTH
            + share_width
            + column(cols.branch, branch_width)
            + column(cols.signals, signal_width)
            + column(cols.pr, pr_width),
    );

    let items: Vec<ListItem> = app
        .filtered_indices
        .iter()
        .map(|&idx| {
            let wt = &app.worktrees[idx];
            let ours = app.created_by_pando.get(&wt.name).copied().unwrap_or(false);
            let (run_glyph, run_color) = run_marker(app.phase_of(&wt.name).as_ref());
            let mut spans = vec![
                Span::styled(
                    if ours { "● " } else { "○ " },
                    Style::new().fg(if ours { green() } else { text_muted() }),
                ),
                Span::styled(run_glyph, Style::new().fg(run_color)),
            ];
            if share_width > 0 {
                let shared = app.public_url_of(&wt.name).is_some();
                spans.push(Span::styled(
                    if shared { "◈ " } else { "  " },
                    Style::new().fg(green()),
                ));
            }
            spans.push(Span::styled(
                pad(&truncate(&wt.name, name_width), name_width),
                Style::new().fg(text()),
            ));
            if cols.branch && branch_width > 0 {
                spans.push(Span::styled(
                    format!(
                        " {}",
                        pad(&truncate(&branch_text(wt), branch_width), branch_width)
                    ),
                    Style::new().fg(text_dim()),
                ));
            }
            if cols.signals && signal_width > 0 {
                spans.push(Span::styled(
                    format!(" {}", pad(&signal_text(wt), signal_width)),
                    Style::new().fg(signal_color(wt)),
                ));
            }
            if cols.pr && pr_width > 0 {
                let chip = app.pr_for(wt).map(pr_chip).unwrap_or_default();
                let color = app.pr_for(wt).map(pr_color).unwrap_or_else(text_muted);
                spans.push(Span::styled(
                    format!(" {}", pad(&chip, pr_width)),
                    Style::new().fg(color),
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

/// What the dev process is doing, in one cell.
pub(super) fn run_marker(phase: Option<&Aggregate>) -> (&'static str, ratatui::style::Color) {
    match phase {
        Some(Aggregate::Running { .. }) => ("● ", green()),
        Some(Aggregate::Starting { .. }) => ("◌ ", yellow()),
        Some(Aggregate::Failed { .. }) => ("✗ ", red()),
        None => ("  ", text_muted()),
    }
}

fn branch_text(wt: &Worktree) -> String {
    wt.branch
        .clone()
        .unwrap_or_else(|| "(detached)".to_string())
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

fn signal_color(wt: &Worktree) -> ratatui::style::Color {
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

pub(super) fn pr_color(pr: &PrInfo) -> ratatui::style::Color {
    match pr.state {
        PrState::Open if pr.draft => text_muted(),
        PrState::Open => cyan(),
        PrState::Merged => green(),
        PrState::Closed => red(),
    }
}
