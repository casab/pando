//! The detail pane: status, processes, services, ports, and the log tail.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::log_tail::LogLevel;
use crate::state::{Aggregate, Phase, ProcessRecord};
use crate::theme::{border, cyan, green, magenta, orange, red, text_dim, text_muted, yellow};
use crate::tui::app::App;

use super::list::{pr_color, run_marker};
use super::truncate;

// Detail rows, by how much they are worth keeping when the pane is short.
// `KEEP_ALWAYS` rows are the pane's reason to exist and are never shed.
pub(super) const KEEP_ALWAYS: u8 = 0;

pub(super) const KEEP_URL: u8 = 1;

/// One row per process, for a worktree that runs more than one. Worth more
/// than the git metadata below: which half of a pair is down is what the
/// pane is being looked at for.
const KEEP_PROCESSES: u8 = 2;

pub(super) const KEEP_PR: u8 = 3;

pub(super) const KEEP_HEAD: u8 = 4;

const KEEP_AGE: u8 = 5;

pub(super) const KEEP_PATH: u8 = 6;

/// A blank line plus the log's header, and the fewest log lines worth the
/// space they take.
const TAIL_CHROME: usize = 2;

const MIN_TAIL_ROWS: usize = 2;

/// Sheds detail rows until the block fits `budget`, dropping the
/// least-valuable first. A pane too short even for the `KEEP_ALWAYS` rows
/// clips rather than hiding the status.
pub fn fit_detail_rows<'a>(mut rows: Vec<(u8, Line<'a>)>, budget: usize) -> Vec<Line<'a>> {
    while rows.len() > budget {
        let Some(index) = rows
            .iter()
            .enumerate()
            .filter(|(_, (keep, _))| *keep > KEEP_ALWAYS)
            .max_by_key(|(i, (keep, _))| (*keep, *i))
            .map(|(i, _)| i)
        else {
            break;
        };
        rows.remove(index);
    }
    rows.into_iter().map(|(_, line)| line).collect()
}

fn detail_row<'a>(label: &str, value: Vec<Span<'a>>) -> Line<'a> {
    let mut spans = vec![Span::styled(
        format!(" {label:<7}"),
        Style::new().fg(text_muted()),
    )];
    spans.extend(value);
    Line::from(spans)
}

pub(super) fn render_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let selected = app.selected_worktree().map(|w| w.name.clone());
    let title = match &selected {
        Some(name) => format!(" {name} "),
        None => " detail ".to_string(),
    };
    let block = Block::bordered()
        .title(Span::styled(
            truncate(&title, area.width.saturating_sub(2) as usize),
            Style::new().fg(text_dim()),
        ))
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.tail_scroll > 0 {
            orange()
        } else {
            border()
        }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(name) = selected else {
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate(" nothing selected", inner.width as usize),
                Style::new().fg(text_muted()),
            )),
            inner,
        );
        return;
    };
    let width = inner.width as usize;
    let wt = app
        .selected_worktree()
        .cloned()
        .expect("a name means a worktree");

    let mut rows: Vec<(u8, Line)> = Vec::new();
    rows.push((
        KEEP_ALWAYS,
        detail_row(
            "branch",
            vec![
                Span::styled(
                    truncate(
                        wt.branch.as_deref().unwrap_or("(detached)"),
                        width.saturating_sub(18),
                    ),
                    Style::new().fg(magenta()),
                ),
                Span::styled(
                    format!("  {}", wt.head_sha.as_deref().unwrap_or("-")),
                    Style::new().fg(text_muted()),
                ),
            ],
        ),
    ));
    rows.push((KEEP_ALWAYS, status_row(app, &name, width)));
    // Below the worktree's own status, and shed before the URL: with one
    // process there are none of these at all.
    for line in process_rows(app, &name, width) {
        rows.push((KEEP_PROCESSES, line));
    }
    // And one per private service. Kept at the same priority as the
    // processes: in an isolated worktree a database that is down is
    // exactly as interesting as a dev server that is.
    for line in service_rows(app, &name, width) {
        rows.push((KEEP_PROCESSES, line));
    }
    if let Some(url) = app.url_of(&name) {
        rows.push((
            KEEP_URL,
            detail_row(
                "url",
                vec![Span::styled(
                    truncate(&url, width.saturating_sub(9)),
                    Style::new().fg(cyan()).add_modifier(Modifier::UNDERLINED),
                )],
            ),
        ));
    }
    // Right under the local URL, and kept at the same priority: when a
    // worktree is shared, the public URL is the line somebody is here to
    // read.
    if let Some(public) = app.public_url_of(&name) {
        rows.push((
            KEEP_URL,
            detail_row(
                "public",
                vec![Span::styled(
                    truncate(&public, width.saturating_sub(9)),
                    Style::new().fg(green()).add_modifier(Modifier::UNDERLINED),
                )],
            ),
        ));
    }
    if let Some(ports) = ports_row(app, &name, width) {
        rows.push((KEEP_URL, ports));
    }
    if let Some(pr) = app.pr_for(&wt) {
        rows.push((
            KEEP_PR,
            detail_row(
                "pr",
                vec![
                    Span::styled(format!("#{}", pr.number), Style::new().fg(pr_color(pr))),
                    Span::styled(
                        format!("  {}", truncate(&pr.title, width.saturating_sub(18))),
                        Style::new().fg(text_dim()),
                    ),
                ],
            ),
        ));
    }
    if let Some(subject) = &wt.head_subject {
        rows.push((
            KEEP_HEAD,
            detail_row(
                "head",
                vec![Span::styled(
                    truncate(subject, width.saturating_sub(9)),
                    Style::new().fg(text_dim()),
                )],
            ),
        ));
    }
    if let Some(age) = &wt.head_age {
        rows.push((
            KEEP_AGE,
            detail_row(
                "age",
                vec![Span::styled(age.clone(), Style::new().fg(text_dim()))],
            ),
        ));
    }
    rows.push((
        KEEP_PATH,
        detail_row(
            "path",
            vec![Span::styled(
                truncate(&wt.path.display().to_string(), width.saturating_sub(9)),
                Style::new().fg(text_dim()),
            )],
        ),
    ));

    let height = inner.height as usize;
    // The log only gets space once the rows that explain the worktree have
    // theirs; a two-row pane is a status line, not a log viewer.
    let tail_rows = height
        .saturating_sub(rows.len().min(height) + TAIL_CHROME)
        .min(height);
    let show_tail = tail_rows >= MIN_TAIL_ROWS;
    let meta_budget = if show_tail {
        height - tail_rows - TAIL_CHROME
    } else {
        height
    };
    let mut lines = fit_detail_rows(rows, meta_budget);
    if show_tail {
        lines.push(Line::raw(""));
        lines.push(tail_header(app, &name, width));
        lines.extend(tail_lines(app, &name, tail_rows, width));
    }
    // What the paint decided, so PgUp/PgDn move by a screenful of what is
    // really there rather than a guess.
    app.tail_rows = if show_tail { tail_rows } else { 0 };
    f.render_widget(Paragraph::new(lines), inner);
}

fn status_row<'a>(app: &App, name: &str, width: usize) -> Line<'a> {
    let Some(phase) = app.phase_of(name) else {
        return detail_row(
            "status",
            vec![Span::styled(
                "stopped — press s to start",
                Style::new().fg(text_muted()),
            )],
        );
    };
    let age = uptime(chrono::Utc::now().signed_duration_since(phase.since()));
    // A worktree running one process has no rows below to carry its pid,
    // and with several it would be the wrong one to show.
    let processes = app.processes_of(name);
    let pid = match processes.as_slice() {
        [(_, only)] => format!("pid {}  ", only.pid),
        _ => String::new(),
    };
    match phase {
        Aggregate::Running { .. } => detail_row(
            "status",
            vec![
                Span::styled(
                    "● running",
                    Style::new().fg(green()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {pid}up {age}"), Style::new().fg(text_muted())),
            ],
        ),
        Aggregate::Starting { .. } => detail_row(
            "status",
            vec![
                Span::styled("◌ starting", Style::new().fg(yellow())),
                Span::styled(format!("  {pid}for {age}"), Style::new().fg(text_muted())),
            ],
        ),
        // Named: `failed` on a worktree running three processes is a
        // question until it says which one.
        Aggregate::Failed { .. } => detail_row(
            "status",
            vec![
                Span::styled(
                    "✗ failed",
                    Style::new().fg(red()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {}",
                        truncate(
                            &phase.reason().unwrap_or_default(),
                            width.saturating_sub(19)
                        )
                    ),
                    Style::new().fg(text_dim()),
                ),
            ],
        ),
    }
}

/// One row per process, under the worktree's own status: what each of them
/// is doing, and which one the tail below is showing.
fn process_rows<'a>(app: &App, name: &str, width: usize) -> Vec<Line<'a>> {
    let processes = app.processes_of(name);
    if processes.len() < 2 {
        // One process has nothing to disambiguate: the status row above is
        // already its status, and a pane in a tmux split has no rows to
        // spare for saying the same thing twice.
        return Vec::new();
    }
    let shown = app.tail_target().map(|(_, process, _)| process);
    let label_width = processes
        .iter()
        .map(|(process, _)| process.chars().count())
        .max()
        .unwrap_or(0);
    processes
        .iter()
        .map(|(process, record)| {
            let (glyph, color) = run_marker(Some(&phase_as_aggregate(process, record)));
            let marker = if shown.as_deref() == Some(process.as_str()) {
                "▸"
            } else {
                " "
            };
            let detail = match &record.phase {
                Phase::Running { since } => format!(
                    "running   pid {}  up {}",
                    record.pid,
                    uptime(chrono::Utc::now().signed_duration_since(*since))
                ),
                Phase::Starting { since } => format!(
                    "starting  for {}",
                    uptime(chrono::Utc::now().signed_duration_since(*since))
                ),
                Phase::Failed { reason, .. } => format!("failed    {reason}"),
            };
            Line::from(vec![
                Span::styled(format!(" {marker} "), Style::new().fg(text_muted())),
                Span::styled(glyph, Style::new().fg(color)),
                Span::styled(
                    format!(" {process:<label_width$}  "),
                    Style::new().fg(text_dim()),
                ),
                Span::styled(
                    truncate(&detail, width.saturating_sub(label_width + 7)),
                    Style::new().fg(text_muted()),
                ),
            ])
        })
        .collect()
}

/// One row per private service of the selected worktree: its port, and
/// whether anything is answering on it.
///
/// Read from the record rather than probed here: a paint may not make a
/// network call, however short. The refresh worker is what asks, and the
/// row shows what it last said.
fn service_rows<'a>(app: &App, name: &str, width: usize) -> Vec<Line<'a>> {
    let services = app.services_of(name);
    if services.is_empty() {
        return Vec::new();
    }
    let label_width = services
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    services
        .iter()
        .map(|service| {
            let (glyph, color) = if service.up {
                ("●", green())
            } else {
                ("○", red())
            };
            let detail = match service.port {
                Some(port) if service.up => format!("service   port {port}"),
                Some(port) => format!("down      port {port}"),
                None => "service   no port".to_string(),
            };
            Line::from(vec![
                Span::styled("   ", Style::new().fg(text_muted())),
                Span::styled(glyph, Style::new().fg(color)),
                Span::styled(
                    format!(" {:<label_width$}  ", service.name),
                    Style::new().fg(text_dim()),
                ),
                Span::styled(
                    truncate(&detail, width.saturating_sub(label_width + 7)),
                    Style::new().fg(text_muted()),
                ),
            ])
        })
        .collect()
}

/// One process's phase as the aggregate of itself, so the same colours and
/// glyphs are used for a row and for the worktree above it.
fn phase_as_aggregate(process: &str, record: &ProcessRecord) -> Aggregate {
    match &record.phase {
        Phase::Running { since } => Aggregate::Running { since: *since },
        Phase::Starting { since } => Aggregate::Starting { since: *since },
        Phase::Failed { at, reason } => Aggregate::Failed {
            process: process.to_string(),
            at: *at,
            reason: reason.clone(),
        },
    }
}

fn ports_row<'a>(app: &App, name: &str, width: usize) -> Option<Line<'a>> {
    let record = app.record_for(name)?;
    if record.ports.is_empty() {
        return None;
    }
    let assigned = record
        .ports
        .iter()
        .map(|(role, port)| format!("{role} {port}"))
        .collect::<Vec<_>>()
        .join("  ");
    // Only the ports pando did not ask for are worth naming separately;
    // the rest are already in the line above.
    let extra: Vec<String> = record
        .observed_ports
        .iter()
        .filter(|p| !record.ports.values().any(|assigned| assigned == *p))
        .map(u16::to_string)
        .collect();
    let mut spans = vec![Span::styled(
        truncate(&assigned, width.saturating_sub(9)),
        Style::new().fg(text_dim()),
    )];
    if !extra.is_empty() {
        spans.push(Span::styled(
            format!("  also {}", extra.join(" ")),
            Style::new().fg(text_muted()),
        ));
    }
    Some(detail_row("ports", spans))
}

fn tail_header<'a>(app: &App, name: &str, width: usize) -> Line<'a> {
    let target = app.tail_target();
    let count = target
        .as_ref()
        .and_then(|(key, _, _)| app.log_tails.get(key))
        .map(|t| t.lines().len())
        .unwrap_or(0);
    let processes = app.processes_of(name).len();
    let label = match &target {
        Some((_, process, _)) if processes > 1 => format!(" log {process}"),
        _ => " log".to_string(),
    };
    let hint = if app.tail_scroll > 0 {
        "  PgDn for newer, l to open"
    } else if processes > 1 {
        "  l to open, tab to switch"
    } else {
        "  l to open"
    };
    Line::from(vec![
        Span::styled(label.clone(), Style::new().fg(text_muted())),
        Span::styled(
            truncate(
                &format!("  {count} lines{hint}"),
                width.saturating_sub(label.chars().count()),
            ),
            Style::new().fg(text_muted()),
        ),
    ])
}

/// The last lines of the dev log, coloured by level.
fn tail_lines<'a>(app: &App, _name: &str, rows: usize, width: usize) -> Vec<Line<'a>> {
    let Some(tail) = app
        .tail_target()
        .and_then(|(key, _, _)| app.log_tails.get(&key))
    else {
        return vec![Line::styled(
            truncate(" (no log yet)", width),
            Style::new().fg(text_muted()),
        )];
    };
    let lines = tail.lines();
    if lines.is_empty() {
        return vec![Line::styled(
            truncate(" (nothing logged yet)", width),
            Style::new().fg(text_muted()),
        )];
    }
    let end = lines.len().saturating_sub(app.tail_scroll);
    let start = end.saturating_sub(rows);
    lines
        .iter()
        .take(end)
        .skip(start)
        .map(|line| {
            Line::styled(
                truncate(&format!(" {}", line.plain), width),
                Style::new().fg(level_color(line.level)),
            )
        })
        .collect()
}

pub(super) fn level_color(level: LogLevel) -> ratatui::style::Color {
    match level {
        LogLevel::Error => red(),
        LogLevel::Warn => yellow(),
        LogLevel::Debug => text_muted(),
        LogLevel::Info => text_dim(),
    }
}

fn uptime(d: chrono::TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
}
