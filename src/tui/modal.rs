//! Overlays: create, remove, unshare, questions, help, messages. Pure
//! painting, like `render`.
//!
//! Every popup is sized to what it holds, with a margin inside its border,
//! and clamped to the screen: a two-line confirmation is a small box, and a
//! question with long options grows to fit them before it ever cuts one.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use super::app::{
    App, BranchLoadState, CreateRow, INSPECT_LEGEND, KeyHelp, LIST_KEYS, LIST_LEGEND, LOG_KEYS,
    Modal, RemoveBlocker, StatusKind, create_rows,
};
use super::render::{centered_box, truncate, truncate_middle, wrap_text};
use crate::theme::{
    blue, border, cyan, green, highlight_bg, magenta, orange, red, surface, text, text_dim,
    text_muted, yellow,
};
use crate::worktree::BranchSource;

/// Popups never shrink below this; a narrow tmux split gets a readable box
/// rather than a sliver.
const MIN_POPUP_WIDTH: u16 = 34;

/// The footer of help and messages. `j` and `k` scroll rather than close,
/// so "any key closes" was not true; these are the keys that do.
pub(super) const CLOSE_HINT: &str = "esc/q/? close";

pub(super) const SCROLL_CLOSE_HINT: &str = "j/k g/G scroll · esc/q/? close";

/// Branch rows the create picker shows at once. Fixed, so the box does not
/// jump about as typing narrows the list.
const CREATE_LIST_ROWS: usize = 8;

/// The widest a popup's content may be on this screen: nine tenths of it,
/// less the border and the margin.
fn max_content_width(area: Rect) -> usize {
    ((area.width as usize * 9) / 10).saturating_sub(6).max(20)
}

/// Paints `modal`. Returns, for help and messages, the furthest they can
/// scroll on this screen, which the scroll keys clamp to.
pub fn render_modal(f: &mut Frame, area: Rect, modal: &Modal, app: &App) -> Option<usize> {
    match modal {
        Modal::Create {
            input,
            branches,
            selected,
            base,
        } => render_create(f, area, input, branches, *selected, base.as_deref(), app),
        Modal::Remove { name, .. } => {
            render_remove(f, area, &app.label_of(name), &app.remove_blockers(name))
        }
        Modal::Unshare { name, url } => render_unshare(f, area, &app.label_of(name), url),
        Modal::StopAll { names } => render_stop_all(f, area, names, app),
        Modal::Question {
            question,
            selected,
            custom,
            ..
        } => render_question(
            f,
            area,
            question,
            *selected,
            custom.as_deref(),
            &app.question_checked,
        ),
        Modal::Help => {
            let (keys, legend_title, legend): (&[KeyHelp], &str, &[(&str, &str)]) =
                if app.log_view().is_some() {
                    (LOG_KEYS, "in the inspect overlay", INSPECT_LEGEND)
                } else {
                    (LIST_KEYS, "in the list", LIST_LEGEND)
                };
            return Some(render_help(
                f,
                area,
                app.help_scroll,
                keys,
                legend_title,
                legend,
            ));
        }
        Modal::Messages => return Some(render_messages(f, area, app)),
    }
    None
}

/// Draws a popup whose content is `width` × `height`, with a margin inside
/// the border when the screen can afford one. Returns the content area.
fn popup(
    f: &mut Frame,
    area: Rect,
    title: &str,
    footer: Option<&str>,
    width: usize,
    height: usize,
) -> Option<Rect> {
    let (width, height) = (width as u16, height as u16);
    // Two cells either side and a row above and below, then one and none,
    // then nothing: a margin is the first thing a small screen gives up.
    let (px, py) = if area.width >= width + 6 + 2 && area.height >= height + 4 + 2 {
        (2, 1)
    } else if area.width >= width + 4 {
        (1, 0)
    } else {
        (0, 0)
    };
    let rect = centered_box(
        (width + 2 + 2 * px).max(MIN_POPUP_WIDTH).min(area.width),
        height + 2 + 2 * py,
        area,
    );
    if rect.width < 3 || rect.height < 3 {
        return None;
    }
    let mut block = Block::bordered()
        .title(Span::styled(
            truncate(&format!(" {title} "), rect.width.saturating_sub(2) as usize),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ))
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border()))
        .style(Style::new().bg(surface()))
        .padding(Padding::new(px, px, py, py));
    if let Some(footer) = footer {
        block = block.title_bottom(Span::styled(
            truncate(
                &format!(" {footer} "),
                rect.width.saturating_sub(2) as usize,
            ),
            Style::new().fg(text_muted()),
        ));
    }
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

/// The width of the widest line.
fn widest(lines: &[Line]) -> usize {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.chars().count()).sum())
        .max()
        .unwrap_or(0)
}

fn key_span(key: &str) -> Span<'static> {
    Span::styled(
        key.to_string(),
        Style::new().fg(orange()).add_modifier(Modifier::BOLD),
    )
}

fn hint_span(label: &str) -> Span<'static> {
    Span::styled(label.to_string(), Style::new().fg(text_muted()))
}

/// A question, its options with the signal that found each one, and a line
/// for a command typed by hand. Every slot accepts one, so there is never a
/// dead end.
///
/// Nothing in it is cut: the prompt and the report wrap, and each option's
/// value is shown whole — it is the command being chosen — with the reason
/// for it on the row below, dimmed. A question too tall for the screen
/// scrolls to keep the selected option in view.
fn render_question(
    f: &mut Frame,
    area: Rect,
    question: &crate::actions::Question,
    selected: usize,
    custom: Option<&str>,
    checked: &[usize],
) {
    // "type the variable names" for ports, "a shell line" for the prelude:
    // what a typed answer is, as the CLI's prompt says it.
    let type_it = format!("type the {}", question.slot.custom_noun());
    let footer: Vec<(&str, &str)> = match custom {
        Some(_) => vec![("⏎", "accept"), ("esc", "back")],
        None if question.multi => vec![
            ("space", "toggle"),
            ("⏎", "accept"),
            ("n", "none"),
            ("esc", "cancel"),
        ],
        None if question.allow_none => vec![
            ("⏎", "choose"),
            ("c", type_it.as_str()),
            ("n", "none"),
            ("esc", "cancel"),
        ],
        None => vec![("⏎", "choose"), ("c", type_it.as_str()), ("esc", "cancel")],
    };
    let footer_line = Line::from(
        footer
            .iter()
            .enumerate()
            .flat_map(|(i, (key, label))| {
                let gap = if i == 0 { "" } else { "   " };
                [
                    hint_span(gap),
                    key_span(key),
                    hint_span(&format!(" {label}")),
                ]
            })
            .collect::<Vec<_>>(),
    );

    // The natural width: whatever the longest unbroken thing needs, capped
    // at what the screen has.
    let cap = max_content_width(area);
    let natural = std::iter::once(question.prompt.chars().count())
        .chain(question.details.iter().map(|d| d.chars().count()))
        .chain(
            question
                .options
                .iter()
                .flat_map(|(value, why)| [value.chars().count() + 6, why.chars().count() + 6]),
        )
        .chain(std::iter::once(widest(std::slice::from_ref(&footer_line))))
        .max()
        .unwrap_or(0);
    let width = natural.clamp(40, cap.max(40)).min(cap);

    let mut body: Vec<Line> = Vec::new();
    for row in wrap_text(&question.prompt, width) {
        body.push(Line::styled(
            row,
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ));
    }
    for detail in &question.details {
        for row in wrap_text(detail, width) {
            body.push(Line::styled(row, Style::new().fg(text_muted())));
        }
    }
    body.push(Line::raw(""));

    // Where each option's rows start and end, to scroll the selected one
    // into view.
    let mut spans_of: Vec<(usize, usize)> = Vec::new();
    for (i, (value, why)) in question.options.iter().enumerate() {
        let cursor = i == selected && custom.is_none();
        let marker = match (question.multi, cursor) {
            // A set question shows what it would take as well as where the
            // cursor is; one marker cannot say both.
            (true, cursor) => {
                let box_ = if checked.contains(&i) { "[x]" } else { "[ ]" };
                format!("{} {box_} ", if cursor { "▸" } else { " " })
            }
            (false, true) => "▸ ".to_string(),
            (false, false) => "  ".to_string(),
        };
        let indent = marker.chars().count();
        let start = body.len();
        let value_style = if cursor {
            Style::new()
                .fg(text())
                .bg(highlight_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(text())
        };
        for (row_at, row) in wrap_text(value, width.saturating_sub(indent))
            .into_iter()
            .enumerate()
        {
            let lead = if row_at == 0 {
                marker.clone()
            } else {
                " ".repeat(indent)
            };
            body.push(Line::from(vec![
                Span::styled(lead, Style::new().fg(orange())),
                Span::styled(row, value_style),
            ]));
        }
        if !why.is_empty() {
            for row in wrap_text(why, width.saturating_sub(indent + 2)) {
                body.push(Line::from(vec![
                    Span::raw(" ".repeat(indent + 2)),
                    Span::styled(row, Style::new().fg(text_muted())),
                ]));
            }
        }
        spans_of.push((start, body.len()));
    }
    if question.options.is_empty() {
        body.push(Line::styled(
            "pando found nothing to suggest here",
            Style::new().fg(text_muted()),
        ));
    }
    if let Some(typed) = custom {
        body.push(Line::raw(""));
        for (row_at, row) in wrap_text(&format!("{typed}▏"), width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if row_at == 0 { "> " } else { "  " };
            body.push(Line::from(vec![
                Span::styled(lead, Style::new().fg(orange())),
                Span::styled(row, Style::new().fg(text())),
            ]));
        }
        spans_of.clear();
        spans_of.push((body.len().saturating_sub(1), body.len()));
    }

    // The body scrolls; the key line under it does not.
    let height = body.len() + 2;
    let Some(inner) = popup(f, area, "pando needs an answer", None, width, height) else {
        return;
    };
    let [body_area, footer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(inner);
    let visible = body_area.height as usize;
    let target = if custom.is_some() {
        spans_of.first().copied()
    } else {
        spans_of.get(selected).copied()
    };
    let offset = match target {
        Some((_, end)) if end > visible => end - visible,
        _ => 0,
    };
    let offset = match target {
        Some((start, _)) if start < offset => start,
        _ => offset,
    };
    let more = body.len().saturating_sub(offset + visible);
    f.render_widget(
        Paragraph::new(body.clone()).scroll((offset as u16, 0)),
        body_area,
    );
    let mut footer_lines = vec![Line::raw("")];
    let mut footer_line = footer_line;
    if more > 0 || offset > 0 {
        footer_line.spans.push(hint_span("   ↑↓ more"));
    }
    footer_lines.push(footer_line);
    f.render_widget(Paragraph::new(footer_lines), footer_area);
}

fn render_create(
    f: &mut Frame,
    area: Rect,
    input: &str,
    branches: &BranchLoadState,
    selected: usize,
    base: Option<&str>,
    app: &App,
) {
    let width = 60.min(max_content_width(area));
    // The prompt, the base, a gap, the list, a gap, the keys.
    let height = 3 + CREATE_LIST_ROWS + 2;
    let Some(inner) = popup(f, area, "new worktree", None, width, height) else {
        return;
    };
    let [prompt, hint, _, list_area, _, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let width = inner.width as usize;

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("branch ", Style::new().fg(text_muted())),
            Span::styled(
                truncate_middle(input, width.saturating_sub(8)),
                Style::new().fg(text()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::new().fg(orange())),
        ])),
        prompt,
    );

    // The base is shown, and tab walks it: the default first, then every
    // branch the picker read.
    let chosen = base.is_some();
    let base = base
        .or(app.default_base.as_deref())
        .unwrap_or("the default base");
    let base_line = Line::from(vec![
        Span::styled("new branches fork from ", Style::new().fg(text_muted())),
        Span::styled(
            base.to_string(),
            Style::new()
                .fg(if chosen { orange() } else { text_dim() })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  tab changes it", Style::new().fg(text_muted())),
    ]);
    f.render_widget(
        Paragraph::new(super::render::truncate_line(base_line, width)),
        hint,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            key_span("⏎"),
            hint_span(" create   "),
            key_span("↑↓"),
            hint_span(" choose   "),
            key_span("tab"),
            hint_span(" base   "),
            key_span("esc"),
            hint_span(" cancel"),
        ])),
        keys,
    );

    if branches.is_loading() && input.trim().is_empty() {
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

    let visible = list_area.height as usize;
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, row)| {
            let marker = if i == selected { "▸ " } else { "  " };
            let label = match row {
                CreateRow::NewBranch(name) => name.clone(),
                CreateRow::Existing(entry) => entry.name.clone(),
            };
            // A branch that already has a worktree says so before enter
            // is pressed, and enter goes to it rather than failing.
            let (tag, color) = if app.is_main_branch(&label) {
                ("checked out in the main checkout", text_muted())
            } else if app.worktree_for_branch(&label).is_some() {
                ("has a worktree · ⏎ selects it", orange())
            } else {
                match row {
                    CreateRow::NewBranch(_) => ("new branch", green()),
                    CreateRow::Existing(entry) => match entry.source {
                        BranchSource::Local => ("local", text_dim()),
                        BranchSource::Remote => ("remote", text_dim()),
                    },
                }
            };
            let tag_width = tag.chars().count() + 2;
            let label_width = width.saturating_sub(marker.chars().count() + tag_width);
            let shown = truncate_middle(&label, label_width);
            let gap = width
                .saturating_sub(
                    marker.chars().count() + shown.chars().count() + tag.chars().count(),
                )
                .max(1);
            // Not a choice: git keeps one checkout per branch.
            let label_style = if app.is_main_branch(&label) {
                Style::new().fg(text_muted())
            } else {
                Style::new().fg(text())
            };
            let mut line = vec![
                Span::styled(marker, Style::new().fg(orange())),
                Span::styled(shown, label_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(tag.to_string(), Style::new().fg(color)),
            ];
            if i == selected {
                line = line
                    .into_iter()
                    .map(|s| Span::styled(s.content, s.style.bg(highlight_bg())))
                    .collect();
            }
            Line::from(line)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), list_area);
}

/// Confirming that a public URL goes away.
///
/// The URL is shown in full, wrapped if it has to be, because the thing
/// being taken away is exactly the thing somebody may have open in another
/// window.
fn render_unshare(f: &mut Frame, area: Rect, label: &str, url: &str) {
    let width = (url.chars().count().max(40)).min(max_content_width(area));
    let mut lines = vec![Line::styled(
        truncate(&format!("stop sharing {label}?"), width),
        Style::new().fg(text()).add_modifier(Modifier::BOLD),
    )];
    let chars: Vec<char> = url.chars().collect();
    for chunk in chars.chunks(width.max(1)) {
        lines.push(Line::styled(
            chunk.iter().collect::<String>(),
            Style::new().fg(cyan()),
        ));
    }
    lines.push(Line::styled(
        truncate("anyone with that link loses it at once", width),
        Style::new().fg(yellow()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        key_span("y"),
        hint_span(" stop sharing   "),
        key_span("esc"),
        hint_span(" keep it"),
    ]));
    let width = widest(&lines).min(width);
    let Some(inner) = popup(f, area, "stop sharing", None, width, lines.len()) else {
        return;
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// Confirming `X`: what is up, one row apiece, before any of it goes down.
fn render_stop_all(f: &mut Frame, area: Rect, names: &[String], app: &App) {
    let cap = max_content_width(area);
    // A tmux split has few rows; past this many the rest are counted.
    const MAX_ROWS: usize = 12;
    let mut lines = vec![Line::styled(
        truncate(
            &match names.len() {
                1 => "stop the one worktree that is up?".to_string(),
                n => format!("stop all {n} worktrees that are up?"),
            },
            cap,
        ),
        Style::new().fg(text()).add_modifier(Modifier::BOLD),
    )];
    for name in names.iter().take(MAX_ROWS) {
        let (glyph, color) = super::render::run_marker(app.phase_of(name).as_ref());
        lines.push(Line::from(vec![
            Span::styled(glyph, Style::new().fg(color)),
            Span::styled(
                truncate_middle(&app.label_of(name), cap.saturating_sub(2)),
                Style::new().fg(text_dim()),
            ),
        ]));
    }
    if names.len() > MAX_ROWS {
        lines.push(hint_span(&format!("… and {} more", names.len() - MAX_ROWS)).into());
    }
    lines.push(Line::styled(
        truncate("their services and public URLs go down too", cap),
        Style::new().fg(yellow()),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        key_span("y"),
        hint_span(" stop all   "),
        key_span("esc"),
        hint_span(" cancel"),
    ]));
    let width = widest(&lines).min(cap);
    let Some(inner) = popup(f, area, "stop everything", None, width, lines.len()) else {
        return;
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// What removing takes with it and what would stop it, all of it before
/// the key is pressed: running, uncommitted changes, not pando's, locked.
fn render_remove(f: &mut Frame, area: Rect, label: &str, blockers: &[RemoveBlocker]) {
    let cap = max_content_width(area);
    let mut lines = vec![
        Line::styled(
            truncate_middle(&format!("remove {label}?"), cap),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "the branch is kept; its logs and data are deleted",
            Style::new().fg(text_muted()),
        ),
    ];
    // Said here rather than found out from the progress line afterwards,
    // or from git refusing after the dialog has gone.
    for blocker in blockers {
        let (mark, color) = match blocker {
            RemoveBlocker::Locked(_) => ("⚠", magenta()),
            RemoveBlocker::Dirty => ("*", yellow()),
            RemoveBlocker::DirtyUnknown => ("?", text_muted()),
            RemoveBlocker::Running => ("●", yellow()),
            RemoveBlocker::NotOurs => ("⚠", yellow()),
        };
        for (i, row) in wrap_text(&blocker.line(), cap.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 { mark } else { " " };
            lines.push(Line::styled(
                format!("{lead} {row}"),
                Style::new().fg(color),
            ));
        }
    }
    lines.push(Line::raw(""));
    let dirty = blockers.contains(&RemoveBlocker::Dirty);
    lines.push(if blockers.iter().any(RemoveBlocker::is_fatal) {
        Line::from(vec![
            key_span("esc"),
            Span::styled(
                " close — this one cannot be removed",
                Style::new().fg(red()),
            ),
        ])
    } else if dirty {
        // `y` would only be refused by git, so the key offered is the one
        // that works — and what it costs is on the line above.
        Line::from(vec![
            key_span("F"),
            hint_span(" remove, discarding changes   "),
            key_span("esc"),
            hint_span(" cancel"),
        ])
    } else {
        Line::from(vec![
            key_span("y"),
            hint_span(" remove   "),
            key_span("F"),
            hint_span(" force   "),
            key_span("esc"),
            hint_span(" cancel"),
        ])
    });
    let width = widest(&lines).min(cap);
    let Some(inner) = popup(f, area, "remove worktree", None, width, lines.len()) else {
        return;
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// Every key of the view it was opened from, then what the marks mean.
/// Scrollable, because a tmux split is often shorter than the keymap.
fn render_help(
    f: &mut Frame,
    area: Rect,
    scroll: usize,
    keys: &[KeyHelp],
    legend_title: &str,
    legend: &[(&str, &str)],
) -> usize {
    // The key column is as wide as its widest entry, so `PgUp PgDn` never
    // runs into what it does.
    let key_width = keys
        .iter()
        .map(|k| k.keys.chars().count())
        .chain(legend.iter().map(|(mark, _)| mark.chars().count()))
        .max()
        .unwrap_or(0)
        + 2;
    let row = |key: &str, what: &str, key_style: Style| {
        Line::from(vec![
            Span::styled(format!("{key:<key_width$}"), key_style),
            Span::styled(what.to_string(), Style::new().fg(text_dim())),
        ])
    };
    let key_style = Style::new().fg(orange()).add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|k| row(k.keys, k.action, key_style))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        legend_title.to_string(),
        Style::new().fg(text_muted()).add_modifier(Modifier::BOLD),
    ));
    for (mark, what) in legend {
        lines.push(row(mark, what, Style::new().fg(blue())));
    }

    let width = widest(&lines).min(max_content_width(area));
    let lines: Vec<Line> = lines
        .into_iter()
        .map(|line| super::render::truncate_line(line, width))
        .collect();
    let fits = lines.len() as u16 + 2 <= area.height;
    let footer = if fits { CLOSE_HINT } else { SCROLL_CLOSE_HINT };
    let Some(inner) = popup(f, area, "keys", Some(footer), width, lines.len()) else {
        return 0;
    };
    let visible = inner.height as usize;
    let bottom = lines.len().saturating_sub(visible);
    let start = scroll.min(bottom);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(start).collect::<Vec<_>>()),
        inner,
    );
    bottom
}

/// Everything the header has said this session, newest first and in full.
fn render_messages(f: &mut Frame, area: Rect, app: &App) -> usize {
    let width = 72.min(max_content_width(area));
    let mut lines: Vec<Line> = Vec::new();
    for status in app.messages.iter().rev() {
        let (mark, color) = match status.kind {
            StatusKind::Success => ("✓ ", green()),
            StatusKind::Error => ("✗ ", red()),
            StatusKind::Info | StatusKind::Progress => ("› ", blue()),
        };
        let age = ago(status.at.elapsed());
        let style = if status.is_error() {
            Style::new().fg(red())
        } else {
            Style::new().fg(text())
        };
        for (i, row) in wrap_text(&status.message, width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 { mark } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(lead, Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(row, style),
            ]));
        }
        lines.push(Line::styled(
            format!("  {age}"),
            Style::new().fg(text_muted()),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "nothing yet — what pando says about your actions collects here",
            Style::new().fg(text_muted()),
        ));
    }
    let width = widest(&lines).min(width);
    let fits = lines.len() as u16 + 2 <= area.height;
    let footer = if fits { CLOSE_HINT } else { SCROLL_CLOSE_HINT };
    let Some(inner) = popup(f, area, "messages", Some(footer), width, lines.len()) else {
        return 0;
    };
    let visible = inner.height as usize;
    let bottom = lines.len().saturating_sub(visible);
    let start = app.help_scroll.min(bottom);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(start).collect::<Vec<_>>()),
        inner,
    );
    bottom
}

fn ago(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..5 => "just now".to_string(),
        5..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}
