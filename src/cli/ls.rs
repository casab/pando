//! `ls`: the worktree table, fitted to the terminal, and its JSON.

use super::JSON_VERSION;
use super::ellipsize;
use super::report_refresh;
use super::short_head;
use super::warn_about;
use crate::actions;
use crate::cache;
use crate::paths::PandoPaths;
use crate::state::WorktreeRecord;
use crate::worktree::{PrState, Worktree};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;

pub fn ls_text<W: Write>(paths: &PandoPaths, out: &mut W) -> Result<()> {
    ls_text_at(paths, out, terminal_width())
}

/// Columns of the text listing, in the order they are printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Col {
    Name,
    Branch,
    Head,
    State,
    Ports,
    Status,
    Path,
}

impl Col {
    fn header(self) -> &'static str {
        match self {
            Col::Name => "NAME",
            Col::Branch => "BRANCH",
            Col::Head => "HEAD",
            Col::State => "STATE",
            Col::Ports => "PORTS",
            Col::Status => "STATUS",
            Col::Path => "PATH",
        }
    }
}

/// Every column except the name, in the order they are dropped as the
/// terminal narrows. The name is the identifier, so it never goes; what a
/// worktree is *doing* outlives what git thinks of it, because that is the
/// question this listing exists to answer.
const SHED_ORDER: [Col; 6] = [
    Col::Head,
    Col::Path,
    Col::Branch,
    Col::State,
    Col::Ports,
    Col::Status,
];

pub(super) const ORDER: [Col; 7] = [
    Col::Name,
    Col::Branch,
    Col::Head,
    Col::State,
    Col::Ports,
    Col::Status,
    Col::Path,
];

/// Two spaces between columns, so a value with a space in it still reads as
/// one cell.
pub(super) const COL_GAP: usize = 2;

/// Which columns fit in `width`. Dropping is all-or-nothing per column, so
/// every column keeps one straight edge.
pub fn keep_columns(width: usize, widths: &BTreeMap<Col, usize>) -> Vec<Col> {
    let mut kept: Vec<Col> = ORDER
        .iter()
        .copied()
        .filter(|c| widths.contains_key(c))
        .collect();
    for candidate in SHED_ORDER {
        if row_width(&kept, widths) <= width {
            break;
        }
        kept.retain(|c| *c != candidate);
    }
    kept
}

fn row_width(kept: &[Col], widths: &BTreeMap<Col, usize>) -> usize {
    let sum: usize = kept
        .iter()
        .map(|c| widths.get(c).copied().unwrap_or(0))
        .sum();
    sum + COL_GAP * kept.len().saturating_sub(1)
}

/// The terminal's width, or "as wide as you like" when the output is not a
/// terminal — a listing being piped into a file should not lose columns
/// because the window happened to be narrow.
pub(super) fn terminal_width() -> usize {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return usize::MAX;
    }
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80)
}

pub fn ls_text_at<W: Write>(paths: &PandoPaths, out: &mut W, width: usize) -> Result<()> {
    let worktrees = actions::ls(paths)?;
    let refreshed = actions::refresh(paths);
    report_refresh(&refreshed);
    let owned = actions::ownership(&refreshed.state, &worktrees);
    if worktrees.is_empty() {
        writeln!(out, "no worktrees — `pando new <branch>` creates one")?;
        return Ok(());
    }

    let rows: Vec<BTreeMap<Col, String>> = worktrees
        .iter()
        .map(|w| {
            let record = refreshed.state.worktrees.get(&w.name);
            BTreeMap::from([
                (Col::Name, ellipsize(&w.name, 32)),
                (
                    Col::Branch,
                    ellipsize(w.branch.as_deref().unwrap_or("(detached)"), 32),
                ),
                (Col::Head, w.head_sha.as_deref().unwrap_or("-").to_string()),
                (
                    Col::State,
                    state_word(w, owned.get(&w.name).copied().unwrap_or(false)).to_string(),
                ),
                (Col::Ports, ports_cell(record)),
                (Col::Status, status_cell(record)),
                (Col::Path, w.path.display().to_string()),
            ])
        })
        .collect();

    let mut widths: BTreeMap<Col, usize> = BTreeMap::new();
    for col in ORDER {
        let content = rows
            .iter()
            .map(|r| r[&col].chars().count())
            .max()
            .unwrap_or(0);
        widths.insert(col, content.max(col.header().chars().count()));
    }
    let kept = keep_columns(width, &widths);

    writeln!(
        out,
        "{}",
        render_row(&kept, &widths, |col| col.header().to_string())
    )?;
    for row in &rows {
        writeln!(
            out,
            "{}",
            render_row(&kept, &widths, |col| row[&col].clone())
        )?;
    }
    Ok(())
}

/// Pads every cell but the last, so a trailing column never carries spaces
/// to the end of the line.
fn render_row(kept: &[Col], widths: &BTreeMap<Col, usize>, cell: impl Fn(Col) -> String) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(kept.len());
    for (i, col) in kept.iter().enumerate() {
        let text = cell(*col);
        if i + 1 == kept.len() {
            parts.push(text);
        } else {
            let width = widths.get(col).copied().unwrap_or(0);
            let pad = width.saturating_sub(text.chars().count());
            parts.push(format!("{text}{}", " ".repeat(pad)));
        }
    }
    parts.join(&" ".repeat(COL_GAP))
}

/// The ports column: bare numbers for a single role, `role:port` once there
/// is more than one to tell apart.
fn ports_cell(record: Option<&WorktreeRecord>) -> String {
    let Some(record) = record else {
        return "-".to_string();
    };
    if record.ports.is_empty() {
        return "-".to_string();
    }
    if record.ports.len() == 1 {
        return record.ports.values().next().expect("one").to_string();
    }
    record
        .ports
        .iter()
        .map(|(role, port)| format!("{role}:{port}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One word for a whole worktree, however many processes it runs: the
/// aggregate, so a row never reads `running` while one of its processes is
/// dead.
fn status_cell(record: Option<&WorktreeRecord>) -> String {
    let Some(record) = record else {
        return "-".to_string();
    };
    match crate::state::aggregate_phase(record) {
        Some(aggregate) => aggregate.word().to_string(),
        None => "-".to_string(),
    }
}

/// One word per worktree for the text listing, ordered by how much it should
/// stop you: a gone or locked entry first, then dirty, then ownership.
fn state_word(w: &Worktree, created_by_pando: bool) -> &'static str {
    if w.prunable {
        "gone"
    } else if w.locked {
        "locked"
    } else if w.dirty == Some(true) {
        "dirty"
    } else if created_by_pando {
        "pando"
    } else {
        "adopted"
    }
}

#[derive(Serialize)]
struct LsOutput {
    version: u32,
    project: ProjectOut,
    worktrees: Vec<WorktreeOut>,
}

#[derive(Serialize)]
pub(super) struct ProjectOut {
    pub(super) id: String,
    pub(super) root: String,
    pub(super) name: String,
}

#[derive(Serialize)]
struct WorktreeOut {
    name: String,
    path: String,
    branch: Option<String>,
    head: Option<String>,
    detached: bool,
    /// `null` when git could not be asked.
    dirty: Option<bool>,
    ahead: Option<u32>,
    behind: Option<u32>,
    created_by_pando: bool,
    prunable: bool,
    /// `null` when unlocked; the lock reason, possibly empty, when locked.
    locked: Option<String>,
    pr: Option<PrOut>,
}

#[derive(Serialize)]
struct PrOut {
    number: u32,
    state: PrState,
    url: String,
}

pub fn ls_json<W: Write>(paths: &PandoPaths, out: &mut W) -> Result<()> {
    let worktrees = actions::ls(paths)?;
    let owned = actions::created_by_pando(paths, &worktrees);
    warn_about(&owned);
    // Cache only: the CLI never spawns `gh`, so `ls --json` stays fast and
    // works offline. The TUI is what refreshes this.
    let prs = cache::load_prs(&paths.pr_cache_file());

    let output = LsOutput {
        version: JSON_VERSION,
        project: ProjectOut {
            id: paths.project.id.clone(),
            root: paths.project.root.display().to_string(),
            name: paths.project.display_name.clone(),
        },
        worktrees: worktrees
            .into_iter()
            .map(|w| {
                let head = short_head(&w);
                let pr = w
                    .branch
                    .as_deref()
                    .and_then(|b| prs.prs.get(b))
                    .map(|p| PrOut {
                        number: p.number,
                        state: p.state,
                        url: p.url.clone(),
                    });
                WorktreeOut {
                    created_by_pando: owned.by_name.get(&w.name).copied().unwrap_or(false),
                    name: w.name,
                    path: w.path.display().to_string(),
                    branch: w.branch,
                    head,
                    detached: w.detached,
                    dirty: w.dirty,
                    ahead: w.ahead_behind.map(|(a, _)| a),
                    behind: w.ahead_behind.map(|(_, b)| b),
                    prunable: w.prunable,
                    locked: if w.locked {
                        Some(w.lock_reason.unwrap_or_default())
                    } else {
                        None
                    },
                    pr,
                }
            })
            .collect(),
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
    Ok(())
}
