//! Overlays: create, remove, help. Pure painting, like `render`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph};

use super::app::{App, BranchLoadState, CreateRow, Modal, RemoveBlocker, create_rows};
use super::render::{centered_rect, truncate};
use crate::theme::{
    border, green, highlight_bg, magenta, orange, red, surface, text, text_dim, text_muted, yellow,
};
use crate::worktree::BranchSource;

/// Popups never shrink below this; a narrow tmux split gets a readable box
/// rather than a sliver.
const MIN_POPUP_WIDTH: u16 = 34;

pub fn render_modal(f: &mut Frame, area: Rect, modal: &Modal, app: &App) {
    match modal {
        Modal::Create {
            input,
            branches,
            selected,
        } => render_create(f, area, input, branches, *selected, app),
        Modal::Remove {
            name,
            blocker,
            created_by_pando,
        } => render_remove(f, area, name, blocker.as_ref(), *created_by_pando),
        Modal::Help => render_help(f, area, app.help_scroll),
    }
}

fn popup(f: &mut Frame, area: Rect, title: &str, height: u16, percent: u16) -> Option<Rect> {
    let rect = centered_rect(percent, MIN_POPUP_WIDTH, height, area);
    if rect.width < 3 || rect.height < 3 {
        return None;
    }
    let block = Block::bordered()
        .title(Span::styled(
            truncate(&format!(" {title} "), rect.width.saturating_sub(2) as usize),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ))
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border()))
        .style(Style::new().bg(surface()));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    Some(inner)
}

fn render_create(
    f: &mut Frame,
    area: Rect,
    input: &str,
    branches: &BranchLoadState,
    selected: usize,
    app: &App,
) {
    let Some(inner) = popup(f, area, "new worktree", 16, 60) else {
        return;
    };
    let [prompt, hint, list_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let width = inner.width as usize;

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("branch ", Style::new().fg(text_muted())),
            Span::styled(
                truncate(input, width.saturating_sub(8)),
                Style::new().fg(text()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::new().fg(orange())),
        ])),
        prompt,
    );

    // The base is shown, not asked for: Enter on the new-branch row accepts
    // it. A different base is a `--base` away on the CLI.
    let base = app.default_base.as_deref().unwrap_or("the default base");
    f.render_widget(
        Paragraph::new(Line::styled(
            truncate(&format!("forks from {base}"), width),
            Style::new().fg(text_muted()),
        )),
        hint,
    );

    if branches.is_loading() {
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate("reading branches…", width),
                Style::new().fg(text_muted()),
            )),
            list_area,
        );
        return;
    }
    let rows = create_rows(input, branches.as_slice());
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate("type a name for the new branch", width),
                Style::new().fg(text_muted()),
            )),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let marker = if i == selected { "▸ " } else { "  " };
            let (label, tag, color) = match row {
                CreateRow::NewBranch(name) => (name.clone(), "new", green()),
                CreateRow::Existing(entry) => (
                    entry.name.clone(),
                    match entry.source {
                        BranchSource::Local => "local",
                        BranchSource::Remote => "remote",
                    },
                    text_dim(),
                ),
            };
            let tag_width = tag.chars().count() + 1;
            let label_width = width.saturating_sub(marker.len() + tag_width);
            let mut line = vec![
                Span::styled(marker, Style::new().fg(orange())),
                Span::styled(truncate(&label, label_width), Style::new().fg(text())),
                Span::raw(" "),
                Span::styled(tag.to_string(), Style::new().fg(color)),
            ];
            if i == selected {
                line = line
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.bg(highlight_bg())))
                    .collect();
            }
            ListItem::new(Line::from(line))
        })
        .collect();
    f.render_widget(List::new(items), list_area);
}

fn render_remove(
    f: &mut Frame,
    area: Rect,
    name: &str,
    blocker: Option<&RemoveBlocker>,
    created_by_pando: bool,
) {
    let Some(inner) = popup(f, area, "remove worktree", 9, 60) else {
        return;
    };
    let width = inner.width as usize;
    let mut lines = vec![
        Line::styled(
            truncate(&format!("remove {name}?"), width),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            truncate("the branch is kept; logs and data are wiped", width),
            Style::new().fg(text_muted()),
        ),
        Line::raw(""),
    ];
    if !created_by_pando {
        lines.push(Line::styled(
            truncate("⚠ pando did not create this worktree", width),
            Style::new().fg(yellow()),
        ));
    }
    if let Some(blocker) = blocker {
        let color = match blocker {
            RemoveBlocker::Locked(_) => magenta(),
            RemoveBlocker::Dirty => yellow(),
            RemoveBlocker::NotOurs => yellow(),
        };
        lines.push(Line::styled(
            truncate(&blocker.line(), width),
            Style::new().fg(color),
        ));
    }
    lines.push(Line::raw(""));
    let confirm = if blocker.is_some_and(RemoveBlocker::is_fatal) {
        Line::from(vec![Span::styled(
            truncate("esc to close — this one cannot be removed", width),
            Style::new().fg(red()),
        )])
    } else {
        Line::from(vec![
            Span::styled("y", Style::new().fg(orange()).add_modifier(Modifier::BOLD)),
            Span::styled(" remove   ", Style::new().fg(text_muted())),
            Span::styled(
                "esc",
                Style::new().fg(orange()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" cancel", Style::new().fg(text_muted())),
        ])
    };
    lines.push(confirm);
    f.render_widget(Paragraph::new(lines), inner);
}

/// Every key the list view answers to. Scrollable, because a tmux split is
/// often shorter than the keymap.
const HELP: [(&str, &str); 11] = [
    ("j / ↓", "move down"),
    ("k / ↑", "move up"),
    ("g / G", "first / last"),
    ("/", "filter by name or branch"),
    ("n", "new worktree"),
    ("d", "remove the selected worktree"),
    ("y", "copy the worktree path"),
    ("r", "refresh now"),
    ("?", "this help"),
    ("q / esc", "quit"),
    ("ctrl-c", "quit from anywhere"),
];

fn render_help(f: &mut Frame, area: Rect, scroll: usize) {
    let Some(inner) = popup(f, area, "keys", HELP.len() as u16 + 2, 50) else {
        return;
    };
    let width = inner.width as usize;
    let visible = inner.height as usize;
    let max_scroll = HELP.len().saturating_sub(visible);
    let start = scroll.min(max_scroll);
    let lines: Vec<Line> = HELP
        .iter()
        .skip(start)
        .take(visible)
        .map(|(key, label)| {
            Line::from(vec![
                Span::styled(
                    format!("{key:<9}"),
                    Style::new().fg(orange()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate(label, width.saturating_sub(9)),
                    Style::new().fg(text_dim()),
                ),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}
