//! Painting. Nothing here talks to git, the filesystem, or a subprocess.
//!
//! The TUI is used in tmux splits, so nothing may assume a wide terminal:
//! list rows shed columns, the footer sheds hints, popups keep a minimum
//! width, and text truncates rather than clipping at a pane edge.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, Paragraph};

use super::app::{App, Mode};
use crate::theme::{
    blue, border, cyan, green, highlight_bg, magenta, orange, red, surface, text, text_dim,
    text_muted, yellow,
};
use crate::worktree::{PrInfo, PrState, Worktree};

/// `"● "` — ownership marker.
const ROW_DOT_WIDTH: usize = 2;
/// `" ▸ "` — the list's own highlight column.
const ROW_CHROME_WIDTH: usize = 3;
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
    let fixed = ROW_DOT_WIDTH + ROW_CHROME_WIDTH;
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

pub fn render(f: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_header(f, header, app);
    render_list(f, body, app);
    render_footer(f, footer, app);

    if let Some(modal) = &app.modal {
        super::modal::render_modal(f, f.area(), modal, app);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    // A just-finished action takes over the whole bar for its few seconds;
    // the footer keeps its key hints throughout.
    if let Some((message, is_error)) = app.active_status() {
        let color = if is_error { red() } else { green() };
        let line = Line::from(vec![
            Span::styled(" ● ", Style::new().fg(color)),
            Span::styled(
                truncate(message, area.width.saturating_sub(4) as usize),
                Style::new().fg(text()),
            ),
        ]);
        f.render_widget(Paragraph::new(line).style(Style::new().bg(surface())), area);
        return;
    }

    let project = &app.paths.project.display_name;
    let branch = app
        .main
        .as_ref()
        .and_then(|m| m.branch.clone())
        .unwrap_or_else(|| "(detached)".to_string());
    let count = app.worktrees.len();
    let mut spans = vec![
        Span::styled(
            " pando ",
            Style::new().fg(green()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(project.clone(), Style::new().fg(text())),
        Span::styled(" · ", Style::new().fg(text_muted())),
        Span::styled(branch, Style::new().fg(blue())),
        Span::styled(" · ", Style::new().fg(text_muted())),
        Span::styled(
            format!("{count} worktree{}", if count == 1 { "" } else { "s" }),
            Style::new().fg(text_dim()),
        ),
    ];
    if app.enriching {
        spans.push(Span::styled(
            " · reading git",
            Style::new().fg(text_muted()),
        ));
    }
    f.render_widget(
        Paragraph::new(truncate_line(Line::from(spans), area.width as usize))
            .style(Style::new().bg(surface())),
        area,
    );
}

fn render_list(f: &mut Frame, area: Rect, app: &mut App) {
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
    for &idx in &app.filtered_indices {
        let wt = &app.worktrees[idx];
        branch_width = branch_width.max(branch_text(wt).chars().count().min(ROW_BRANCH_MAX));
        signal_width = signal_width.max(signal_text(wt).chars().count());
        if let Some(pr) = app.pr_for(wt) {
            pr_width = pr_width.max(pr_chip(pr).chars().count());
        }
    }
    let width = list_area.width as usize;
    let cols = list_columns(width, branch_width, signal_width, pr_width);
    let name_width = width.saturating_sub(
        ROW_DOT_WIDTH
            + ROW_CHROME_WIDTH
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
            let mut spans = vec![
                Span::styled(
                    if ours { "● " } else { "○ " },
                    Style::new().fg(if ours { green() } else { text_muted() }),
                ),
                Span::styled(
                    pad(&truncate(&wt.name, name_width), name_width),
                    Style::new().fg(text()),
                ),
            ];
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

fn branch_text(wt: &Worktree) -> String {
    wt.branch
        .clone()
        .unwrap_or_else(|| "(detached)".to_string())
}

/// The git state of a row, in one short cell: what is wrong first, then
/// how far the branch has drifted.
fn signal_text(wt: &Worktree) -> String {
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

fn pr_color(pr: &PrInfo) -> ratatui::style::Color {
    match pr.state {
        PrState::Open if pr.draft => text_muted(),
        PrState::Open => cyan(),
        PrState::Merged => green(),
        PrState::Closed => red(),
    }
}

/// Key hints, most valuable first. The essential ones are never dropped.
const HINTS: [(&str, &str, bool); 7] = [
    ("j/k", "move", true),
    ("n", "new", true),
    ("d", "remove", true),
    ("/", "filter", false),
    ("y", "copy path", false),
    ("?", "help", false),
    ("q", "quit", true),
];

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let line = match app.mode {
        Mode::Filter => hint_line(
            &[
                ("↑↓", "move", true),
                ("⏎", "accept", true),
                ("esc", "clear", true),
            ],
            width,
        ),
        Mode::Normal => hint_line(&HINTS, width),
    };
    f.render_widget(Paragraph::new(line).style(Style::new().bg(surface())), area);
}

fn hint_line(hints: &[(&str, &str, bool)], width: usize) -> Line<'static> {
    const SEPARATOR: &str = " · ";
    let items: Vec<(usize, bool)> = hints
        .iter()
        .map(|(key, label, essential)| {
            (key.chars().count() + 1 + label.chars().count(), *essential)
        })
        .collect();
    let keep = keep_hints(&items, SEPARATOR.chars().count(), width.saturating_sub(1));

    let mut spans = vec![Span::raw(" ")];
    let mut first = true;
    for ((key, label, _), keep) in hints.iter().zip(&keep) {
        if !keep {
            continue;
        }
        if !first {
            spans.push(Span::styled(SEPARATOR, Style::new().fg(text_muted())));
        }
        first = false;
        spans.push(Span::styled(
            (*key).to_string(),
            Style::new().fg(orange()).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(text_muted()),
        ));
    }
    Line::from(spans)
}

/// Width-adaptive hint selection: keep the essential hints, drop
/// lower-priority ones from the tail until the line fits. Without this the
/// rightmost hints fall off the edge with no ellipsis.
pub fn keep_hints(items: &[(usize, bool)], separator: usize, width: usize) -> Vec<bool> {
    let mut keep = vec![true; items.len()];
    while kept_width(items, separator, &keep) > width {
        match (0..items.len()).rev().find(|&i| keep[i] && !items[i].1) {
            Some(i) => keep[i] = false,
            // Only essentials left: let it clip rather than vanish.
            None => break,
        }
    }
    keep
}

fn kept_width(items: &[(usize, bool)], separator: usize, keep: &[bool]) -> usize {
    let kept = keep.iter().filter(|&&k| k).count();
    let widths: usize = items
        .iter()
        .zip(keep)
        .filter(|&(_, &k)| k)
        .map(|(&(w, _), _)| w)
        .sum();
    widths + separator * kept.saturating_sub(1)
}

/// A centred popup that keeps a minimum width, so a narrow split gets a
/// readable box rather than a sliver.
pub fn centered_rect(percent_x: u16, min_width: u16, height: u16, area: Rect) -> Rect {
    let target = area.width * percent_x / 100;
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
mod tests {
    use super::*;
    use crate::tui::app::tests::{test_app, wt};
    use crate::tui::app::{BranchLoadState, Modal, RemoveBlocker};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn text_of(buf: &Buffer) -> String {
        let area = *buf.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_at_any_terminal_size_without_panicking() {
        let mut app = test_app(&["feat+one", "feat+two", "a-very-long-worktree-name-here"]);
        app.worktrees[1].dirty = Some(true);
        app.worktrees[2].prunable = true;
        app.main = Some(wt("acme-shop"));

        for width in 1..=120u16 {
            for height in [1u16, 2, 3, 5, 12, 40] {
                draw(&mut app, width, height);
            }
        }
    }

    #[test]
    fn renders_every_modal_at_any_terminal_size() {
        let modals = [
            Modal::Help,
            Modal::Create {
                input: "feat/new".into(),
                branches: BranchLoadState::Loading,
                selected: 0,
            },
            Modal::Remove {
                name: "feat+one".into(),
                blocker: Some(RemoveBlocker::Locked(Some("benchmarking".into()))),
                created_by_pando: false,
            },
        ];
        for modal in modals {
            let mut app = test_app(&["feat+one"]);
            app.modal = Some(modal);
            for width in [1u16, 4, 20, 41, 80, 200] {
                for height in [1u16, 3, 8, 24] {
                    draw(&mut app, width, height);
                }
            }
        }
    }

    #[test]
    fn renders_an_empty_list_and_a_filter_with_no_matches() {
        let mut app = test_app(&[]);
        assert!(text_of(&draw(&mut app, 60, 10)).contains("press n to create one"));

        let mut app = test_app(&["feat+one"]);
        for code in [KeyCode::Char('/'), KeyCode::Char('z'), KeyCode::Char('z')] {
            app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        let rendered = text_of(&draw(&mut app, 60, 10));
        assert!(rendered.contains("no matches"), "{rendered}");
    }

    #[test]
    fn the_header_shows_the_project_branch_and_count() {
        let mut app = test_app(&["feat+one", "feat+two"]);
        app.main = Some(wt("acme-shop"));
        let rendered = text_of(&draw(&mut app, 80, 10));
        assert!(rendered.contains("acme-shop"), "{rendered}");
        assert!(rendered.contains("2 worktrees"), "{rendered}");
    }

    #[test]
    fn the_header_gives_the_whole_bar_to_a_status_message() {
        let mut app = test_app(&["feat+one"]);
        app.main = Some(wt("acme-shop"));
        app.set_status("created feat+one");
        let first_line = text_of(&draw(&mut app, 80, 10))
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(first_line.contains("created feat+one"), "{first_line}");
        assert!(!first_line.contains("worktrees"), "{first_line}");
    }

    #[test]
    fn list_columns_shed_branch_then_signals_then_pr() {
        // Wide enough for everything.
        assert_eq!(
            list_columns(80, 20, 6, 4),
            ListColumns {
                branch: true,
                signals: true,
                pr: true
            }
        );
        // Branch goes first.
        assert_eq!(
            list_columns(44, 20, 6, 4),
            ListColumns {
                branch: false,
                signals: true,
                pr: true
            }
        );
        // Then the signal grid.
        assert_eq!(
            list_columns(24, 20, 6, 4),
            ListColumns {
                branch: false,
                signals: false,
                pr: true
            }
        );
        // The PR chip is the last thing standing.
        assert_eq!(
            list_columns(18, 20, 6, 4),
            ListColumns {
                branch: false,
                signals: false,
                pr: false
            }
        );
    }

    #[test]
    fn list_columns_keep_everything_when_the_optional_columns_are_empty() {
        assert_eq!(
            list_columns(20, 0, 0, 0),
            ListColumns {
                branch: true,
                signals: true,
                pr: true
            },
            "columns with no content cost nothing"
        );
    }

    #[test]
    fn a_narrow_list_still_shows_the_name() {
        let mut app = test_app(&["feat+one"]);
        let rendered = text_of(&draw(&mut app, 30, 8));
        assert!(rendered.contains("feat+one"), "{rendered}");
    }

    #[test]
    fn the_row_marks_adopted_worktrees_differently() {
        let mut app = test_app(&["mine", "theirs"]);
        app.created_by_pando.insert("theirs".into(), false);
        let rendered = text_of(&draw(&mut app, 80, 10));
        assert!(rendered.contains("● mine"), "{rendered}");
        assert!(rendered.contains("○ theirs"), "{rendered}");
    }

    #[test]
    fn signals_report_gone_locked_dirty_and_drift() {
        let mut gone = wt("g");
        gone.prunable = true;
        assert_eq!(signal_text(&gone), "gone");

        let mut locked = wt("l");
        locked.locked = true;
        assert_eq!(signal_text(&locked), "locked");

        let mut dirty = wt("d");
        dirty.dirty = Some(true);
        dirty.ahead_behind = Some((2, 3));
        assert_eq!(signal_text(&dirty), "*↑2↓3");

        let mut clean = wt("c");
        clean.dirty = Some(false);
        clean.ahead_behind = Some((0, 0));
        assert_eq!(signal_text(&clean), "");

        let mut lots = wt("m");
        lots.dirty = Some(false);
        lots.ahead_behind = Some((250, 0));
        assert_eq!(signal_text(&lots), "↑99+");
    }

    #[test]
    fn keep_hints_drops_optional_hints_from_the_tail_first() {
        let items = [(5, true), (4, false), (6, false), (4, true)];
        assert_eq!(keep_hints(&items, 3, 100), vec![true, true, true, true]);
        assert_eq!(
            keep_hints(&items, 3, 22),
            vec![true, true, false, true],
            "the last optional hint goes first"
        );
        assert_eq!(keep_hints(&items, 3, 12), vec![true, false, false, true]);
        assert_eq!(
            keep_hints(&items, 3, 1),
            vec![true, false, false, true],
            "essentials survive even when they cannot fit"
        );
    }

    #[test]
    fn truncate_adds_an_ellipsis_only_when_it_cuts() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate("", 5), "");
    }

    #[test]
    fn truncate_line_keeps_the_surviving_spans_styled() {
        let line = Line::from(vec![
            Span::styled("abc", Style::new().fg(green())),
            Span::styled("defgh", Style::new().fg(red())),
        ]);
        let cut = truncate_line(line, 5);
        assert_eq!(cut.spans.len(), 2);
        assert_eq!(cut.spans[0].content, "abc");
        assert_eq!(cut.spans[1].content, "d…");
        assert_eq!(cut.spans[1].style.fg, Some(red()));
    }

    #[test]
    fn centered_rect_honours_the_minimum_width_and_never_escapes_the_area() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered_rect(50, 40, 10, area);
        assert_eq!(popup.width, 50);

        let narrow = Rect::new(0, 0, 20, 40);
        let popup = centered_rect(50, 40, 10, narrow);
        assert_eq!(popup.width, 20, "a popup never grows past its area");

        let short = Rect::new(0, 0, 100, 4);
        let popup = centered_rect(50, 40, 10, short);
        assert!(popup.height <= 4);
    }
}
