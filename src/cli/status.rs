//! `status`: each worktree's processes, services and share, as text
//! or JSON.

use super::JSON_VERSION;
use super::ellipsize;
use super::ls::COL_GAP;
use super::ls::ProjectOut;
use super::ls::terminal_width;
use super::report_refresh;
use crate::actions;
use crate::actions::worktree_url;
use crate::paths::PandoPaths;
use crate::state::{Phase, ProcessRecord, WorktreeRecord};
use crate::worktree::Worktree;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;

// ---- status ---------------------------------------------------------------

/// One process, flattened for the machine-readable shape.
#[derive(Serialize)]
struct ProcessOut {
    pid: u32,
    /// `starting`, `running`, or `failed`.
    phase: &'static str,
    since: DateTime<Utc>,
    /// `null` unless the phase is `failed`.
    reason: Option<String>,
    log: String,
}

#[derive(Serialize)]
struct HookOut {
    fingerprint: Option<String>,
    ran_at: DateTime<Utc>,
}

/// One private service, flattened for the machine-readable shape.
#[derive(Serialize)]
struct ServiceOut {
    /// `compose` for now; `native` joins it in Phase 6.
    kind: &'static str,
    port: Option<u16>,
    /// Whether something answers on that port right now.
    up: bool,
    /// Whether a log pump is running in front of it. `false` while the
    /// worktree is stopped, and `false` with `up` true when the pump died:
    /// the log tab has stopped filling, and the next `start` or `restart`
    /// puts it back.
    logging: bool,
    /// The compose project the container belongs to, which is what `rm`
    /// takes down.
    project: Option<String>,
}

/// A worktree's public URL, when it has one.
///
/// No cookie, ever: the value `auth_cmd` produced lives in the proxy's
/// environment and nowhere else, and this shape is printed, logged, and
/// piped into things.
#[derive(Serialize)]
struct ShareOut {
    url: String,
    /// The port being published — the application's own.
    local_port: u16,
    /// The proxy in front of it, when `auth_cmd` put one there.
    proxy_port: Option<u16>,
    since: DateTime<Utc>,
}

#[derive(Serialize)]
struct StatusWorktreeOut {
    name: String,
    branch: Option<String>,
    path: String,
    ports: BTreeMap<String, u16>,
    observed_ports: Vec<u16>,
    /// The readiness role's URL, when this worktree has one.
    url: Option<String>,
    /// Whether this worktree runs private copies of the project's
    /// services.
    isolated: bool,
    /// `null` when the worktree is not shared.
    share: Option<ShareOut>,
    processes: BTreeMap<String, ProcessOut>,
    services: BTreeMap<String, ServiceOut>,
    hooks: BTreeMap<String, HookOut>,
}

#[derive(Serialize)]
struct StatusOutput {
    version: u32,
    project: ProjectOut,
    worktrees: Vec<StatusWorktreeOut>,
}

fn phase_word(phase: &Phase) -> &'static str {
    match phase {
        Phase::Starting { .. } => "starting",
        Phase::Running { .. } => "running",
        Phase::Failed { .. } => "failed",
    }
}

fn phase_since(phase: &Phase) -> DateTime<Utc> {
    match phase {
        Phase::Starting { since } | Phase::Running { since } => *since,
        Phase::Failed { at, .. } => *at,
    }
}

fn phase_reason(phase: &Phase) -> Option<String> {
    match phase {
        Phase::Failed { reason, .. } => Some(reason.clone()),
        _ => None,
    }
}

pub fn status_json<W: Write>(paths: &PandoPaths, only: Option<&str>, out: &mut W) -> Result<()> {
    let refreshed = actions::refresh(paths);
    report_refresh(&refreshed);
    let worktrees = actions::ls(paths)?;
    let output = StatusOutput {
        version: JSON_VERSION,
        project: ProjectOut {
            id: paths.project.id.clone(),
            root: paths.project.root.display().to_string(),
            name: paths.project.display_name.clone(),
        },
        worktrees: worktrees
            .into_iter()
            .filter(|w| only.is_none_or(|name| w.name == name))
            .map(|w| {
                let record = refreshed.state.worktrees.get(&w.name);
                let empty = WorktreeRecord::new(&w.path, false);
                let record = record.unwrap_or(&empty);
                StatusWorktreeOut {
                    name: w.name.clone(),
                    branch: w.branch.clone(),
                    path: w.path.display().to_string(),
                    ports: record.ports.clone(),
                    observed_ports: record.observed_ports.clone(),
                    url: worktree_url(record),
                    isolated: record.isolated,
                    share: record.share.as_ref().map(|share| ShareOut {
                        url: share.public_url.clone(),
                        local_port: share.local_port,
                        proxy_port: share.proxy_port,
                        since: share.started_at,
                    }),
                    services: actions::service_statuses(record)
                        .into_iter()
                        .map(|status| {
                            let recorded = record.services.iter().find(|s| s.name == status.name);
                            (
                                status.name,
                                ServiceOut {
                                    kind: match recorded.map(|s| s.kind) {
                                        Some(crate::state::ServiceKind::Native) => "native",
                                        _ => "compose",
                                    },
                                    port: status.port,
                                    up: status.up,
                                    logging: status.logging,
                                    project: recorded.and_then(|s| s.compose_project.clone()),
                                },
                            )
                        })
                        .collect(),
                    processes: record
                        .processes
                        .iter()
                        .map(|(name, p)| {
                            (
                                name.clone(),
                                ProcessOut {
                                    pid: p.pid,
                                    phase: phase_word(&p.phase),
                                    since: phase_since(&p.phase),
                                    reason: phase_reason(&p.phase),
                                    log: p.log_path.display().to_string(),
                                },
                            )
                        })
                        .collect(),
                    hooks: record
                        .hooks
                        .iter()
                        .map(|(name, h)| {
                            (
                                name.clone(),
                                HookOut {
                                    fingerprint: h.fingerprint.clone(),
                                    ran_at: h.ran_at,
                                },
                            )
                        })
                        .collect(),
                }
            })
            .collect(),
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
    Ok(())
}

pub fn status_text<W: Write>(paths: &PandoPaths, only: Option<&str>, out: &mut W) -> Result<()> {
    status_text_at(paths, only, out, terminal_width())
}

/// [`status_text`] at a given terminal width, so the shedding is testable
/// without a terminal — the shape `ls_text_at` already has.
pub fn status_text_at<W: Write>(
    paths: &PandoPaths,
    only: Option<&str>,
    out: &mut W,
    width: usize,
) -> Result<()> {
    let refreshed = actions::refresh(paths);
    report_refresh(&refreshed);
    let worktrees = actions::ls(paths)?;
    let shown: Vec<&Worktree> = worktrees
        .iter()
        .filter(|w| only.is_none_or(|name| w.name == name))
        .collect();
    if shown.is_empty() {
        match only {
            Some(name) => writeln!(out, "no worktree named \"{name}\"")?,
            None => writeln!(out, "no worktrees — `pando new <branch>` creates one")?,
        }
        return Ok(());
    }
    let names = shown
        .iter()
        .map(|w| w.name.chars().count())
        .max()
        .unwrap_or(4);
    for w in shown {
        let record = refreshed.state.worktrees.get(&w.name);
        writeln!(
            out,
            "{:<names$}  {}",
            w.name,
            worktree_line(record, width.saturating_sub(names + COL_GAP))
        )?;
        // One line per process under it, so a worktree that is `failed`
        // says which of its processes is, and each one's pid is reachable.
        let Some(record) = record else { continue };
        let process_width = record
            .processes
            .keys()
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0);
        for (name, p) in &record.processes {
            // The same shape as the worktree line above, and the same
            // degradation: a reason or an uptime is truncated rather than
            // allowed to wrap the row under it.
            let row = format!(
                "  {:<process_width$}  {}",
                name,
                process_line(p),
                process_width = process_width
            );
            writeln!(out, "{}", ellipsize(&row, width))?;
        }
        // And one per private service, so a worktree whose database is
        // down says which one rather than only that its app failed.
        let services = actions::service_statuses(record);
        let service_width = services
            .iter()
            .map(|s| s.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(process_width);
        for service in &services {
            let port = match service.port {
                Some(port) => port.to_string(),
                None => "-".to_string(),
            };
            // Only for a service that is *up*: a stopped worktree has no
            // pump by design, and saying so on every line of every stopped
            // service would bury the one case that matters — a container
            // answering while nothing fills its log tab. `start` and
            // `restart` put the pump back; no read path ever does.
            let pump = match service.up && !service.logging {
                true => ", no log pump",
                false => "",
            };
            let row = format!(
                "  {:<service_width$}  {:<PHASE_CELL$}  service on {port}{pump}",
                service.name,
                if service.up { "up" } else { "down" },
            );
            writeln!(out, "{}", ellipsize(&row, width))?;
        }
        // And the public URL, last, because it is the line somebody is
        // most often here to copy.
        if let Some(share) = &record.share {
            let through = match share.proxy_port {
                Some(port) => format!(" through a proxy on {port}"),
                None => String::new(),
            };
            let row = format!(
                "  {:<service_width$}  {:<PHASE_CELL$}  {}{through}",
                "share", "public", share.public_url,
            );
            writeln!(out, "{}", ellipsize(&row, width))?;
        }
    }
    Ok(())
}

/// Width the phase word is padded to, so the cell after it lines up
/// whichever of the four words is printed.
const PHASE_CELL: usize = 8;

/// The worktree's own line: its aggregate phase, its ports, and the one URL
/// it serves on, fitted to `room` characters.
fn worktree_line(record: Option<&WorktreeRecord>, room: usize) -> String {
    let Some(record) = record else {
        return "stopped".to_string();
    };
    let ports = ports_text(&record.ports);
    let Some(aggregate) = crate::state::aggregate_phase(record) else {
        if record.ports.is_empty() {
            return "stopped".to_string();
        }
        // The ports survive a stop, and saying so is how a developer knows
        // the URL they bookmarked will still be theirs.
        return fit_line("stopped", &ports, None, None, room);
    };
    let age = human_duration(Utc::now().signed_duration_since(aggregate.since()));
    match aggregate {
        crate::state::Aggregate::Running { .. } => fit_line(
            "running",
            &ports,
            worktree_url(record).as_deref(),
            Some(&format!("up {age}")),
            room,
        ),
        crate::state::Aggregate::Starting { .. } => {
            fit_line("starting", &ports, None, Some(&format!("for {age}")), room)
        }
        // Named: `failed` on a worktree running three processes is a
        // question until it says which one.
        crate::state::Aggregate::Failed { .. } => {
            fit_line("failed", &ports, None, aggregate.reason().as_deref(), room)
        }
    }
}

/// Assembles a worktree's status line and fits it into `room` characters.
///
/// The TUI is used in tmux splits and `pando status` is read in the same
/// ones, so this degrades rather than wraps. The URL goes first: it is the
/// longest cell by far and `--json` still carries it. The ports cell is
/// truncated after that, because a developer who can see three roles and
/// two numbers knows more than one looking at a line that wrapped.
fn fit_line(word: &str, ports: &str, url: Option<&str>, tail: Option<&str>, room: usize) -> String {
    let assemble = |ports: &str, url: Option<&str>| {
        let mut parts = vec![format!("{word:<PHASE_CELL$}"), ports.to_string()];
        parts.extend(url.map(str::to_string));
        parts.extend(tail.map(str::to_string));
        parts.join(&" ".repeat(COL_GAP))
    };
    let full = assemble(ports, url);
    if full.chars().count() <= room {
        return full;
    }
    let without_url = assemble(ports, None);
    if without_url.chars().count() <= room {
        return without_url;
    }
    let over = without_url.chars().count() - room;
    let keep = ports.chars().count().saturating_sub(over);
    // And if even an empty ports cell will not fit — a split narrow enough
    // that the phase word and the age are already too much — the line is
    // truncated rather than left to wrap onto the process rows below it.
    ellipsize(&assemble(&ellipsize(ports, keep), None), room)
}

fn process_line(p: &ProcessRecord) -> String {
    match &p.phase {
        Phase::Running { since } => format!(
            "running   pid {}  up {}",
            p.pid,
            human_duration(Utc::now().signed_duration_since(*since))
        ),
        Phase::Starting { since } => format!(
            "starting  pid {}  for {}",
            p.pid,
            human_duration(Utc::now().signed_duration_since(*since))
        ),
        Phase::Failed { reason, .. } => format!("failed    {reason}"),
    }
}

fn ports_text(ports: &BTreeMap<String, u16>) -> String {
    if ports.is_empty() {
        return "-".to_string();
    }
    ports
        .iter()
        .map(|(role, port)| format!("{role} {port}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Uptime in the shortest form that is still precise enough to be useful.
pub(super) fn human_duration(d: chrono::TimeDelta) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}
