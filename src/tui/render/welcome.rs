//! The first run: what pando is, what it already knows about the project,
//! and the one key that gets a worktree onto the list.
//!
//! Everything shown is already in memory — the config and the paths the
//! app was started with. Detection is not run for it: a first frame that
//! waits on a scan of the project is a first frame that is late.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};

use crate::config::ServiceConfig;
use crate::theme::{blue, border, orange, text, text_dim, text_muted};
use crate::tui::app::App;

use super::{home_relative, truncate, truncate_middle, wrap_text};

/// `"  worktrees  "` — the widest fact label and its gap.
const FACT_LABEL: usize = 11;

pub(super) fn render_welcome(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .title(Span::styled(" welcome ", Style::new().fg(text_dim())))
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border()))
        .padding(if area.width > 40 && area.height > 12 {
            Padding::new(2, 2, 1, 0)
        } else {
            Padding::ZERO
        });
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let project = &app.paths.project.display_name;

    let mut lines: Vec<Line> = Vec::new();
    let intro = format!(
        "pando runs one dev environment per branch of {project}: a git worktree, \
         its dev server on its own port, and its logs."
    );
    for row in wrap_text(&intro, width) {
        lines.push(Line::styled(row, Style::new().fg(text())));
    }
    lines.push(Line::raw(""));

    let root = app
        .main
        .as_ref()
        .map(|m| m.path.clone())
        .unwrap_or_else(|| app.paths.root().to_path_buf());
    let branch = app
        .main
        .as_ref()
        .and_then(|m| m.branch.clone())
        .map(|b| format!("  on {b}"))
        .unwrap_or_default();
    lines.push(fact(
        "project",
        format!("{}{branch}", home_relative(&root)),
        width,
    ));
    lines.push(fact("dev", dev_summary(app), width));
    if let Some(services) = services_summary(app) {
        lines.push(fact("services", services, width));
    }
    let worktrees_dir = app.config.worktrees_dir(&app.paths);
    lines.push(fact("worktrees", home_relative(&worktrees_dir), width));
    lines.push(Line::raw(""));

    for (key, what) in [
        (
            "n",
            "create a worktree: type a branch name, new or existing",
        ),
        ("?", "every key, and what the marks in the list mean"),
        ("q", "quit"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<4}"),
                Style::new().fg(orange()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(what, width.saturating_sub(4)),
                Style::new().fg(text_dim()),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    // The real home, not `~/.pando`: `PANDO_HOME` moves it, and a
    // welcome that names the wrong directory sends somebody looking there.
    // So does `[project] worktrees_dir`, for the worktrees alone.
    let home = home_relative(&app.paths.home);
    let where_they_live = if worktrees_dir.starts_with(&app.paths.home) {
        format!("worktrees, logs and config live under {home}")
    } else {
        format!(
            "worktrees live in {}, logs and config under {home}",
            home_relative(&worktrees_dir)
        )
    };
    for row in wrap_text(
        &format!("Your checkout is never touched: {where_they_live}."),
        width,
    ) {
        lines.push(Line::styled(row, Style::new().fg(text_muted())));
    }

    // Top-aligned in a short pane, a little lower in a tall one, so it
    // does not sit in a corner of an empty screen.
    let content = lines.len() as u16;
    let [_, body, _] = Layout::vertical([
        Constraint::Length(inner.height.saturating_sub(content) / 4),
        Constraint::Length(content.min(inner.height)),
        Constraint::Fill(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(lines), body);
}

fn fact<'a>(label: &str, value: String, width: usize) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{label:<FACT_LABEL$}"),
            Style::new().fg(text_muted()),
        ),
        Span::styled(
            truncate_middle(&value, width.saturating_sub(FACT_LABEL)),
            Style::new().fg(blue()),
        ),
    ])
}

/// What `start` will run, as far as config already says.
fn dev_summary(app: &App) -> String {
    let processes: Vec<String> = app
        .config
        .processes
        .iter()
        .filter(|(_, process)| !process.cmd.is_empty())
        .map(|(name, process)| format!("{name}: {}", process.cmd))
        .collect();
    if processes.is_empty() {
        "not settled yet: the first start detects it, and asks if it has to".to_string()
    } else {
        processes.join(" · ")
    }
}

fn services_summary(app: &App) -> Option<String> {
    let names: Vec<String> = app
        .config
        .services
        .iter()
        .map(|service| match service {
            ServiceConfig::Compose { file, include, .. } if include.is_empty() => {
                format!("compose ({file})")
            }
            ServiceConfig::Compose { file, include, .. } => {
                format!("{} ({file})", include.join(", "))
            }
            ServiceConfig::Native { name, .. } => name.clone(),
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "{} — shared by default, i isolates one worktree",
        names.join(" · ")
    ))
}
