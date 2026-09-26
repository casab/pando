//! The detail pane: status, processes, services, where it is reached, what
//! it has checked out, and the log tail filling whatever height is left.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::log_tail::LogLevel;
use crate::state::{Aggregate, Phase, ProcessRecord, ServiceMode};
use crate::theme::{
    border, cyan, green, highlight_bg, magenta, namespaced, red, text, text_dim, text_muted, yellow,
};
use crate::tui::app::{App, compact_age};

use super::list::{pr_color, run_marker, signal_color, signal_text};
use super::{chunk_cells, home_relative, text_width, truncate, truncate_line, truncate_middle};

// Detail rows, by how much they are worth keeping when the pane is short.
// `KEEP_ALWAYS` rows are the pane's reason to exist and are never shed.
pub(super) const KEEP_ALWAYS: u8 = 0;

pub(super) const KEEP_URL: u8 = 1;

/// One row per process, for a worktree that runs more than one. Worth more
/// than the git metadata below: which half of a pair is down is what the
/// pane is being looked at for.
const KEEP_PROCESSES: u8 = 2;

/// Uncommitted changes and drift: what a removal would lose and what a
/// rebase would have to carry, kept before the branch and commit rows.
const KEEP_GIT: u8 = 2;

pub(super) const KEEP_PR: u8 = 3;

/// Shared or isolated: only shown for a project that has services, and
/// then worth about as much as the PR.
const KEEP_MODE: u8 = 3;

pub(super) const KEEP_HEAD: u8 = 4;

/// The blank line between what runs and what git says: the first thing a
/// short pane gives up, after the path.
const KEEP_SPACER: u8 = 5;

pub(super) const KEEP_PATH: u8 = 6;

/// `" status "` — a space, then the label padded to seven.
const LABEL_WIDTH: usize = 8;

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

/// A labelled value that is never cut: a URL somebody is about to copy.
/// What does not fit on the row carries on under it, indented to the
/// value's column.
fn wrapped_rows<'a>(label: &str, value: &str, style: Style, width: usize) -> Vec<Line<'a>> {
    let room = width.saturating_sub(LABEL_WIDTH).max(1);
    chunk_cells(value, room)
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            if i == 0 {
                detail_row(label, vec![Span::styled(piece, style)])
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(LABEL_WIDTH)),
                    Span::styled(piece, style),
                ])
            }
        })
        .collect()
}

pub(super) fn render_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let selected = app.selected_worktree().map(|w| w.name.clone());
    // The title leads with the row's own glyph, in its colour, so the pane
    // says which state it describes before a word of it is read.
    let (glyph, glyph_color) = match &selected {
        Some(name) => run_marker(app.phase_of(name).as_ref()),
        None => ("", text_muted()),
    };
    let title = match &selected {
        Some(name) => format!("{} ", app.label_of(name)),
        None => " detail ".to_string(),
    };
    let title_room = (area.width.saturating_sub(4) as usize).saturating_sub(text_width(glyph));
    let title = Line::from(vec![
        Span::raw(if glyph.is_empty() { "" } else { " " }),
        Span::styled(glyph, Style::new().fg(glyph_color)),
        Span::styled(
            truncate_middle(&title, title_room),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
    ]);
    let block = Block::bordered()
        .title(title)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if app.tail_scroll > 0 {
            yellow()
        } else {
            border()
        }));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(name) = selected else {
        // The list is not empty — the welcome covers that — so a filter
        // is hiding everything.
        f.render_widget(
            Paragraph::new(Line::styled(
                truncate(" no worktree matches the filter", inner.width as usize),
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
    rows.push((KEEP_ALWAYS, status_row(app, &name, width)));
    for line in failure_rows(app, &name, width) {
        rows.push((KEEP_URL, line));
    }
    for line in nothing_to_run_rows(app, &name, width) {
        rows.push((KEEP_URL, line));
    }

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
    // And what it holds in the main checkout's own servers, as private
    // services are: what is its own there, and what stays shared.
    for line in namespace_rows(app, &name, width) {
        rows.push((KEEP_PROCESSES, line));
    }
    // Never truncated, either of them: the URL is the thing that gets
    // copied, and one missing its end is worse than none.
    if let Some(phase) = app.phase_of(&name)
        && let Some(url) = app.url_of(&name)
    {
        // A failed worktree's URL may still answer (another process can
        // hold it), so it stays; but it is not drawn as a live link.
        let style = if matches!(phase, Aggregate::Failed { .. }) {
            Style::new().fg(text_dim())
        } else {
            Style::new().fg(cyan()).add_modifier(Modifier::UNDERLINED)
        };
        for line in wrapped_rows("url", &url, style, width) {
            rows.push((KEEP_URL, line));
        }
    }
    // Right under the local URL, and kept at the same priority: when a
    // worktree is shared, the public URL is the line somebody is here to
    // read.
    if let Some(public) = app.public_url_of(&name) {
        let style = Style::new().fg(green()).add_modifier(Modifier::UNDERLINED);
        for line in wrapped_rows("public", &public, style, width) {
            rows.push((KEEP_URL, line));
        }
    }
    if let Some(ports) = ports_row(app, &name, width) {
        rows.push((KEEP_URL, ports));
    }
    if let Some(mode) = mode_row(app, &name, width) {
        rows.push((KEEP_MODE, mode));
    }
    // Two groups: what runs, above, and what git says, below.
    rows.push((KEEP_SPACER, Line::raw("")));
    rows.push((KEEP_GIT, git_row(app, &wt, width)));
    for line in branch_rows(&wt, width) {
        rows.push((KEEP_HEAD, line));
    }
    if let Some(commit) = commit_row(app, &wt, width) {
        rows.push((KEEP_HEAD, commit));
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
    let adopted = !app.created_by_pando.get(&name).copied().unwrap_or(false);
    let note = if adopted { "  adopted" } else { "" };
    rows.push((
        KEEP_PATH,
        detail_row(
            "path",
            vec![
                // The middle goes, not the end: the end is which worktree
                // it is.
                Span::styled(
                    truncate_middle(
                        &home_relative(&wt.path),
                        width.saturating_sub(LABEL_WIDTH + note.len()),
                    ),
                    Style::new().fg(text_dim()),
                ),
                Span::styled(note, Style::new().fg(text_muted())),
            ],
        ),
    ));

    let height = inner.height as usize;
    // The log only gets space once the rows that explain the worktree have
    // theirs; a two-row pane is a status line, not a log viewer. Then it
    // takes every row that is left.
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

/// Which services it talks to, and the key that switches — for a project
/// that has services to separate at all.
fn mode_row<'a>(app: &App, name: &str, width: usize) -> Option<Line<'a>> {
    let mode = app.record_for(name).map(|r| r.mode()).unwrap_or_default();
    if app.config.services.is_empty() && mode == ServiceMode::Shared {
        return None;
    }
    let (color, what) = match mode {
        ServiceMode::Isolated => (magenta(), "  private services · S shares"),
        ServiceMode::Namespaced => (
            namespaced(),
            "  its own database and slot in the project's servers · S shares",
        ),
        ServiceMode::Shared => (text_dim(), "  the project's services · i isolates"),
    };
    let word = mode.word();
    Some(detail_row(
        "mode",
        vec![
            Span::styled(word, Style::new().fg(color)),
            Span::styled(
                truncate(what, width.saturating_sub(LABEL_WIDTH + word.len())),
                Style::new().fg(text_muted()),
            ),
        ],
    ))
}

/// What git says about the working tree: uncommitted changes, and how far
/// the branch stands from its base. The same facts `pando ls` prints in its
/// GIT column, in words.
fn git_row(app: &App, wt: &crate::worktree::Worktree, width: usize) -> Line<'static> {
    let mut parts: Vec<(String, Style)> = Vec::new();
    match wt.dirty {
        Some(true) => parts.push((
            "uncommitted changes".to_string(),
            Style::new().fg(yellow()).add_modifier(Modifier::BOLD),
        )),
        Some(false) => parts.push(("clean".to_string(), Style::new().fg(text_dim()))),
        None => parts.push(("reading git…".to_string(), Style::new().fg(text_muted()))),
    }
    if let Some((ahead, behind)) = wt.ahead_behind {
        let base = app
            .default_base
            .as_deref()
            .map(|b| format!(" of {b}"))
            .unwrap_or_default();
        let drift = match (ahead, behind) {
            (0, 0) => format!(
                "even with {}",
                app.default_base.as_deref().unwrap_or("the base")
            ),
            (a, 0) => format!("↑{a} ahead{base}"),
            (0, b) => format!("↓{b} behind{base}"),
            (a, b) => format!("↑{a} ahead, ↓{b} behind{base}"),
        };
        parts.push((drift, Style::new().fg(text_dim())));
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (text, style)) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(text_muted())));
        }
        spans.push(Span::styled(text, style));
    }
    super::truncate_line(detail_row("git", spans), width)
}

/// The branch in full, and anything wrong with its entry.
/// Whole, wrapped if it has to be: a long branch name differs from its
/// siblings somewhere in the middle, and a cut there loses exactly that.
fn branch_rows<'a>(wt: &crate::worktree::Worktree, width: usize) -> Vec<Line<'a>> {
    let branch = wt.branch.as_deref().unwrap_or("(detached)");
    let mut drift = Vec::new();
    if wt.prunable || wt.locked {
        drift.push(signal_text(wt));
    }
    let drift = if drift.is_empty() {
        String::new()
    } else {
        format!("  {}", drift.join(" · "))
    };
    let room = width.saturating_sub(LABEL_WIDTH);
    let drift_style = Style::new().fg(signal_color(wt));
    if text_width(branch) + text_width(&drift) <= room {
        return vec![detail_row(
            "branch",
            vec![
                Span::styled(branch.to_string(), Style::new().fg(magenta())),
                Span::styled(drift, drift_style),
            ],
        )];
    }
    let mut rows = wrapped_rows("branch", branch, Style::new().fg(magenta()), width);
    if !drift.is_empty() {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(LABEL_WIDTH)),
            Span::styled(truncate(drift.trim_start(), room), drift_style),
        ]));
    }
    rows
}

/// The checked-out commit: its sha, its subject, and how old it is — now,
/// not when git was last asked.
fn commit_row<'a>(app: &App, wt: &crate::worktree::Worktree, width: usize) -> Option<Line<'a>> {
    let sha = wt.head_sha.clone()?;
    let age = app
        .commit_age(wt)
        .map(|age| format!(" · {age}"))
        .unwrap_or_default();
    let subject = wt.head_subject.clone().unwrap_or_default();
    let room = width.saturating_sub(LABEL_WIDTH + sha.len() + 1 + text_width(&age));
    Some(detail_row(
        "commit",
        vec![
            Span::styled(sha, Style::new().fg(text_muted())),
            Span::raw(" "),
            Span::styled(truncate(&subject, room), Style::new().fg(text_dim())),
            Span::styled(age, Style::new().fg(text_muted())),
        ],
    ))
}

fn status_row<'a>(app: &App, name: &str, width: usize) -> Line<'a> {
    // An action in flight is the status: a worktree being stopped is not
    // `running`, and one whose start is waiting on a question is not
    // `stopped`.
    if let Some(pending) = app.pending_on(name) {
        let (word, rest) = if app.awaiting_answer() {
            ("? waiting for your answer".to_string(), String::new())
        } else {
            (
                format!("◌ {}…", pending.kind.verb()),
                pending
                    .stage
                    .as_ref()
                    .map(|stage| format!("  {stage}"))
                    .unwrap_or_default(),
            )
        };
        let room = width.saturating_sub(LABEL_WIDTH + text_width(&word));
        return detail_row(
            "status",
            vec![
                Span::styled(word, Style::new().fg(yellow()).add_modifier(Modifier::BOLD)),
                Span::styled(truncate(&rest, room), Style::new().fg(text_muted())),
            ],
        );
    }
    let Some(phase) = app.phase_of(name) else {
        // With nothing to run there is no key that starts it; the rows
        // below say where to add something.
        let hint = if app.nothing_to_run {
            ""
        } else {
            "  ⏎ picks a mode to start"
        };
        return detail_row(
            "status",
            vec![
                Span::styled("○ stopped", Style::new().fg(text_dim())),
                Span::styled(
                    truncate(hint, width.saturating_sub(LABEL_WIDTH + 9)),
                    Style::new().fg(text_muted()),
                ),
            ],
        );
    };
    // Up since the oldest process that runs: `P` restarting one of three
    // does not make the worktree two seconds old.
    let since = match phase {
        Aggregate::Running { .. } => app.up_since(name).unwrap_or(phase.since()),
        _ => phase.since(),
    };
    let age = uptime(chrono::Utc::now().signed_duration_since(since));
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
        // The reason is on the rows below, wrapped rather than cut.
        Aggregate::Failed { .. } => detail_row(
            "status",
            vec![
                Span::styled(
                    "✗ failed",
                    Style::new().fg(red()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate("  l shows the log", width.saturating_sub(LABEL_WIDTH + 8)),
                    Style::new().fg(text_muted()),
                ),
            ],
        ),
    }
}

/// Where to add a process, for a project that has none: wrapped, never
/// cut, because the end of it is the path of the file to edit.
fn nothing_to_run_rows<'a>(app: &App, name: &str, width: usize) -> Vec<Line<'a>> {
    if !app.nothing_to_run || app.phase_of(name).is_some() || app.pending_on(name).is_some() {
        return Vec::new();
    }
    let room = width.saturating_sub(LABEL_WIDTH).max(1);
    super::wrap_text(&app.nothing_to_run_line(), room)
        .into_iter()
        .map(|row| {
            Line::from(vec![
                Span::raw(" ".repeat(LABEL_WIDTH)),
                Span::styled(row, Style::new().fg(yellow())),
            ])
        })
        .collect()
}

/// Why a worktree failed, wrapped under its status: the reason is usually
/// a sentence that ends with what to do, and a cut one loses exactly that.
fn failure_rows<'a>(app: &App, name: &str, width: usize) -> Vec<Line<'a>> {
    /// A stack trace belongs in the log; the pane keeps a paragraph.
    const MAX_REASON_ROWS: usize = 4;
    let Some(reason) = app.phase_of(name).and_then(|phase| phase.reason()) else {
        return Vec::new();
    };
    if app.pending_on(name).is_some() {
        return Vec::new();
    }
    let room = width.saturating_sub(LABEL_WIDTH).max(1);
    let mut rows = super::wrap_text(&reason, room);
    if rows.len() > MAX_REASON_ROWS {
        rows.truncate(MAX_REASON_ROWS);
        if let Some(last) = rows.last_mut() {
            *last = truncate(&format!("{last} …"), room);
        }
    }
    rows.into_iter()
        .map(|row| {
            Line::from(vec![
                Span::raw(" ".repeat(LABEL_WIDTH)),
                Span::styled(row, Style::new().fg(text_dim())),
            ])
        })
        .collect()
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
        .map(|(process, _)| text_width(process))
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
            // What comes before the detail: ` ▸ `, the glyph, and the
            // name padded between a space and two.
            let lead = 3 + text_width(glyph) + 1 + label_width + 2;
            Line::from(vec![
                Span::styled(format!(" {marker} "), Style::new().fg(text_muted())),
                Span::styled(glyph, Style::new().fg(color)),
                Span::styled(
                    format!(" {process:<label_width$}  "),
                    Style::new().fg(text_dim()),
                ),
                Span::styled(
                    truncate(&detail, width.saturating_sub(lead)),
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
        .map(|s| text_width(&s.name))
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

/// One row per namespace the worktree holds in the main checkout's own
/// servers — `own database northwind_traders__feat_x`, `slot 3`, or `kept`
/// for one waiting through another mode until `rm` — and, while it runs
/// namespaced, one per service that stays on main's data, with why.
fn namespace_rows<'a>(app: &App, name: &str, width: usize) -> Vec<Line<'a>> {
    let Some(record) = app.record_for(name) else {
        return Vec::new();
    };
    let namespaced_now = record.mode() == ServiceMode::Namespaced;
    let mut rows: Vec<(String, &str, String)> = record
        .namespaces
        .iter()
        .map(|ns| {
            let what = match ns.kind {
                crate::state::NamespaceKind::Database => format!("database {}", ns.name),
                crate::state::NamespaceKind::Slot => format!("slot {}", ns.name),
            };
            match namespaced_now {
                true => (ns.service.clone(), "own", what),
                false => (ns.service.clone(), "kept", format!("{what} until rm")),
            }
        })
        .collect();
    if namespaced_now {
        let shared = app.namespace_shared.get_or_init(|| {
            crate::actions::namespace_lines(&app.paths, Some(&app.config), record)
                .into_iter()
                .filter(|(_, word, _)| *word == "shared")
                .map(|(service, _, why)| (service, why))
                .collect()
        });
        for (service, why) in shared {
            rows.push((service.clone(), "shared", why.clone()));
        }
    }
    let label_width = rows
        .iter()
        .map(|(service, _, _)| text_width(service))
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|(service, word, what)| {
            let color = match word {
                "shared" => text_dim(),
                _ => namespaced(),
            };
            let detail = format!("{word:<6}  {what}");
            Line::from(vec![
                Span::styled("   ", Style::new().fg(text_muted())),
                Span::styled("◆", Style::new().fg(color)),
                Span::styled(
                    format!(" {service:<label_width$}  "),
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
    // A port nothing listens on while the worktree reads as running: the
    // half of a two-server script that died behind the half that is up.
    let silent: Vec<String> = crate::state::silent_ports(record, chrono::Utc::now())
        .into_iter()
        .map(|(role, port)| format!("{role} {port}"))
        .collect();
    if !silent.is_empty() {
        spans.push(Span::styled(
            format!("  nothing on {} — l shows why", silent.join(", ")),
            Style::new().fg(yellow()),
        ));
    }
    Some(detail_row("ports", spans))
}

fn tail_header<'a>(app: &App, name: &str, width: usize) -> Line<'a> {
    let target = app.tail_target();
    if target.is_none() {
        // Nothing runs, so there is nothing live to count.
        return Line::from(vec![
            Span::styled(
                " log",
                Style::new().fg(text_dim()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(
                    "  not running · l opens its earlier logs",
                    width.saturating_sub(4),
                ),
                Style::new().fg(text_muted()),
            ),
        ]);
    }
    let count = target
        .as_ref()
        .and_then(|(key, _, _)| app.log_tails.get(key))
        .map(|t| t.lines().len())
        .unwrap_or(0);
    let processes = app.processes_of(name);
    let hint = if app.tail_scroll > 0 {
        format!(" · scrolled back {} · PgDn newer", app.tail_scroll)
    } else if processes.len() > 1 {
        " · tab next · P restarts it".to_string()
    } else {
        " · l opens".to_string()
    };
    let unit = if count == 1 { "line" } else { "lines" };
    let mut spans = vec![Span::styled(
        " log",
        Style::new().fg(text_dim()).add_modifier(Modifier::BOLD),
    )];
    // Every process by name, the one on screen marked, so what `tab`
    // moves and what `P` would restart can be seen rather than guessed.
    if processes.len() > 1 {
        let shown = target.as_ref().map(|(_, process, _)| process.as_str());
        spans.push(Span::raw("  "));
        for (i, (process, _)) in processes.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ", Style::new().fg(text_muted())));
            }
            spans.push(if Some(process.as_str()) == shown {
                Span::styled(
                    format!("▸{process}"),
                    Style::new()
                        .fg(text())
                        .bg(highlight_bg())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(process.clone(), Style::new().fg(text_muted()))
            });
        }
    }
    spans.push(Span::styled(
        format!("  {count} {unit}{hint}"),
        Style::new().fg(text_muted()),
    ));
    truncate_line(Line::from(spans), width)
}

/// The last lines of the dev log, coloured by level.
fn tail_lines<'a>(app: &App, _name: &str, rows: usize, width: usize) -> Vec<Line<'a>> {
    // Not running: the header above already says so.
    let target = app.tail_target();
    if target.is_none() {
        return Vec::new();
    }
    let Some(tail) = target.and_then(|(key, _, _)| app.log_tails.get(&key)) else {
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

/// The pane's one duration style, shared with the commit age.
fn uptime(d: chrono::TimeDelta) -> String {
    compact_age(d.num_seconds())
}
