//! The header and the footer: counts, status, and key hints that shed
//! themselves to fit.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::{blue, green, orange, red, surface, text, text_dim, text_muted};
use crate::tui::app::{App, Mode};

use super::{truncate, truncate_line};

pub(super) fn render_header(f: &mut Frame, area: Rect, app: &App) {
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
    // One chip per shared service, so a dev server that will not connect
    // says whose fault it is before the developer reads a stack trace.
    // Shed first: on a narrow pane the project and the branch matter more.
    for service in &app.service_health.shared {
        spans.push(Span::styled(" · ", Style::new().fg(text_muted())));
        spans.push(Span::styled(
            "●",
            Style::new().fg(if service.up { green() } else { red() }),
        ));
        spans.push(Span::styled(
            format!(" {}", service.name),
            Style::new().fg(text_dim()),
        ));
    }
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

/// Key hints, most valuable first. The essential ones are never dropped.
const HINTS: [(&str, &str, bool); 14] = [
    ("j/k", "move", true),
    ("s", "start", true),
    ("x", "stop", true),
    ("r", "restart", false),
    ("l", "logs", true),
    ("tab", "log", false),
    ("o", "open", false),
    ("t", "share", false),
    ("O", "public", false),
    ("n", "new", false),
    ("d", "remove", false),
    ("/", "filter", false),
    ("?", "help", false),
    ("q", "quit", true),
];

pub(super) fn render_footer(f: &mut Frame, area: Rect, app: &App) {
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

pub(super) fn hint_line(hints: &[(&str, &str, bool)], width: usize) -> Line<'static> {
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
