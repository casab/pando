//! The clap front end. A thin wrapper: every behaviour lives in `actions`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use crate::actions;
use crate::cache;
use crate::config::Config;
use crate::log_tail::{self, LogLevel};
use crate::paths::PandoPaths;
use crate::state::{Phase, ProcessRecord, WorktreeRecord};
use crate::worktree::{PrState, Worktree};

/// Shape version for machine-readable output, bumped independently of the
/// crate version so agents can pin what they parse.
pub const JSON_VERSION: u32 = 1;

/// What `git log --format=%h` abbreviates to, and what the JSON documents.
const SHORT_SHA_LEN: usize = 7;

/// Documented on `--help` because an agent driving pando needs to know that
/// 3 is "ask the human", not "it broke".
const EXIT_CODE_HELP: &str = "Exit codes:\n  \
     0  ok\n  \
     1  error\n  \
     2  usage\n  \
     3  needs an answer — the question is printed on stderr; --yes accepts \
     pando's own recommendation";

#[derive(Parser, Debug)]
#[command(
    name = "pando",
    version,
    about = "One repo. Every branch alive.",
    after_help = EXIT_CODE_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a worktree and branch from the default base.
    New {
        /// Branch name. Slashes become plus signs in the directory name.
        branch: String,
        /// Accept pando's own recommendation for anything it would ask.
        #[arg(long)]
        yes: bool,
        /// Base to fork a new branch from. A bare name prefers the
        /// remote-tracking ref, so a stale local branch is never the fork
        /// point.
        #[arg(long)]
        base: Option<String>,
    },
    /// List worktrees with their git state.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree and wipe its pando data. The branch is kept.
    Rm {
        name: String,
        /// Confirm removing a worktree pando did not create.
        #[arg(long)]
        yes: bool,
        /// Let git discard modified or untracked files.
        #[arg(long)]
        force: bool,
    },
    /// Print a worktree's absolute path.
    Path { name: String },
    /// Start a worktree's dev process.
    Start {
        name: String,
        /// Accept pando's own recommendation for anything it would ask.
        /// Without it, an unanswerable question exits 3.
        #[arg(long)]
        yes: bool,
    },
    /// Stop a worktree's processes, or every worktree's when given no name.
    Stop { name: Option<String> },
    /// Stop and start again, keeping the ports.
    Restart { name: String },
    /// What is running, and on which ports.
    Status {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print a worktree's log.
    Logs {
        name: String,
        /// Which log: `dev` by default, or a hook's name.
        #[arg(long, default_value = "dev")]
        source: String,
        /// How many lines from the end.
        #[arg(long, default_value_t = DEFAULT_TAIL)]
        tail: usize,
        /// Keep printing as the log grows. Ends on Ctrl-C.
        #[arg(short = 'f', long)]
        follow: bool,
        /// One JSON object per line: timestamp, level, text.
        #[arg(long)]
        json: bool,
    },
}

impl Command {
    /// Whether this command acts on `pando.toml`.
    ///
    /// The ones that do not run on whatever layers are left when pando's
    /// own is unusable: a broken config is exactly when `stop`, `ls` and
    /// `logs` are worth having.
    pub fn needs_config(&self) -> bool {
        matches!(
            self,
            Command::New { .. } | Command::Start { .. } | Command::Restart { .. }
        )
    }
}

/// Lines `logs` prints when nothing else is asked for.
const DEFAULT_TAIL: usize = 50;

pub fn dispatch(command: Command, paths: &PandoPaths, config: &Config) -> Result<()> {
    let mut out = std::io::stdout();
    match command {
        Command::New { branch, base, yes } => {
            let config = &actions::resolve_for_new(paths, config, &asker(yes), &notice)?;
            let name = actions::new(paths, config, &branch, base.as_deref(), &notice)?;
            // The canonical path, the one the state record and `pando path`
            // carry: the raw one differs on macOS (/var against /private/var)
            // and reads as a second, different location.
            let created = config.worktrees_dir(paths).join(&name);
            let created = std::fs::canonicalize(&created).unwrap_or(created);
            writeln!(out, "created {name} at {}", created.display())?;
            Ok(())
        }
        Command::Ls { json } => {
            if json {
                ls_json(paths, &mut out)
            } else {
                ls_text(paths, &mut out)
            }
        }
        Command::Rm { name, yes, force } => {
            actions::rm(paths, &name, yes, force)?;
            writeln!(out, "removed {name}")?;
            Ok(())
        }
        Command::Path { name } => {
            writeln!(out, "{}", actions::path(paths, &name)?.display())?;
            Ok(())
        }
        Command::Start { name, yes } => {
            let config = &actions::resolve_process(paths, config, &asker(yes), &notice)?;
            let outcome = actions::start(paths, config, &name, &notice)?;
            let started = outcome.process();
            if started.reassigned {
                eprintln!("pando: the ports {name} had were taken; it moved to new ones");
            }
            match &outcome {
                actions::StartOutcome::AlreadyRunning(_) => {
                    writeln!(out, "{name} is already running{}", url_suffix(started))?;
                }
                actions::StartOutcome::Started(_) => {
                    writeln!(out, "started {name}{}", url_suffix(started))?;
                }
            }
            Ok(())
        }
        Command::Stop { name } => match name {
            Some(name) => {
                match actions::stop(paths, &name)? {
                    actions::StopOutcome::Stopped => writeln!(out, "stopped {name}")?,
                    actions::StopOutcome::NotRunning => writeln!(out, "{name} was not running")?,
                }
                Ok(())
            }
            None => {
                let stopped = actions::stop_all(paths)?;
                if stopped.is_empty() {
                    writeln!(out, "nothing was running")?;
                } else {
                    writeln!(out, "stopped {}", stopped.join(", "))?;
                }
                Ok(())
            }
        },
        Command::Restart { name } => {
            let outcome = actions::restart(paths, config, &name, &notice)?;
            writeln!(out, "restarted {name}{}", url_suffix(outcome.process()))?;
            Ok(())
        }
        Command::Status { name, json } => {
            if json {
                status_json(paths, name.as_deref(), &mut out)
            } else {
                status_text(paths, name.as_deref(), &mut out)
            }
        }
        Command::Logs {
            name,
            source,
            tail,
            follow,
            json,
        } => logs(paths, &name, &source, tail, follow, json, &mut out),
    }
}

/// Everything pando narrates goes to stderr, so a command's stdout stays
/// exactly what a script asked for.
fn notice(message: &str) {
    eprintln!("pando: {message}");
}

/// The CLI's way of asking.
///
/// With `--yes`, the rules' own recommendation is taken and said out loud.
/// On a terminal, a numbered prompt. Anywhere else — a script, an agent, a
/// pipe — the question becomes exit code 3 rather than a process that
/// blocks on a `read` nobody will ever answer.
fn asker(yes: bool) -> impl Fn(&actions::Question) -> Result<actions::Answer> {
    move |question: &actions::Question| {
        if yes {
            return match question.preselect {
                Some(index) => {
                    notice(&format!("--yes: taking {:?}", question.options[index].0));
                    // Not `Choice`: nobody chose it, so the line written to
                    // the config says a flag took it rather than claiming a
                    // confidence the rules never had.
                    Ok(actions::Answer::Auto(index))
                }
                None => Err(needs_answer(question)),
            };
        }
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Err(needs_answer(question));
        }
        prompt(question)
    }
}

fn needs_answer(question: &actions::Question) -> anyhow::Error {
    anyhow::Error::new(actions::NeedsAnswer {
        question: question.clone(),
    })
}

/// A numbered prompt on stderr, so a piped stdout is still only the
/// command's own output.
fn prompt(question: &actions::Question) -> Result<actions::Answer> {
    prompt_with(question, &mut std::io::stderr(), || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            // The terminal went away mid-question.
            return Ok(None);
        }
        Ok(Some(line))
    })
}

/// [`prompt`] with its terminal injected. `read` gives the next line, or
/// `None` when there is no more input.
fn prompt_with(
    question: &actions::Question,
    out: &mut impl Write,
    mut read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    let width = question
        .options
        .iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);
    writeln!(out, "pando: {}", question.prompt)?;
    for (i, (value, why)) in question.options.iter().enumerate() {
        let marker = if question.preselect == Some(i) {
            "*"
        } else {
            " "
        };
        writeln!(out, "  {marker}{}) {value:<width$}  {why}", i + 1)?;
    }
    if question.allow_custom {
        writeln!(out, "   c) something else — type the command")?;
    }
    if question.allow_none {
        writeln!(out, "   n) none — this process has no port")?;
    }
    let default = question.preselect.map(|i| i + 1);
    loop {
        match default {
            Some(n) => write!(out, "  [{n}] > ")?,
            None => write!(out, "  > ")?,
        }
        out.flush()?;
        let Some(line) = read()? else {
            return Err(needs_answer(question));
        };
        let line = line.trim();
        if line.is_empty()
            && let Some(index) = question.preselect
        {
            return Ok(actions::Answer::Choice(index));
        }
        if question.allow_none && (line == "n" || line == "N") {
            return Ok(actions::Answer::None);
        }
        if question.allow_custom && (line == "c" || line == "C") {
            write!(out, "  command > ")?;
            out.flush()?;
            let custom = read()?.unwrap_or_default().trim().to_string();
            if !custom.is_empty() {
                return Ok(actions::Answer::Custom(custom));
            }
            writeln!(out, "pando: an empty command is not an answer")?;
            continue;
        }
        // A number is a choice, always — even one that is out of range.
        // Letting it fall through to "then it must be a command" is how a
        // fat-fingered `5` on a four-option question became `cmd = "5"`,
        // written down as an answer and never questioned again. A command
        // that really is only digits is still reachable through `c`.
        match line.parse::<i64>() {
            Ok(_) if !question.options.is_empty() => {
                let chosen = line
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1 && *n <= question.options.len());
                match chosen {
                    Some(n) => return Ok(actions::Answer::Choice(n - 1)),
                    None => writeln!(
                        out,
                        "pando: pick a number between 1 and {}",
                        question.options.len()
                    )?,
                }
            }
            // Anything else is taken as the command itself, so nobody has
            // to discover that `c` exists first.
            _ if question.allow_custom && !line.is_empty() => {
                return Ok(actions::Answer::Custom(line.to_string()));
            }
            _ => writeln!(
                out,
                "pando: pick a number between 1 and {}",
                question.options.len()
            )?,
        }
    }
}

/// What exit code 3 prints: the question, its options, and the two ways out.
pub fn render_needs_answer(needs: &actions::NeedsAnswer) -> String {
    let mut out = format!(
        "pando: {}
",
        needs.question.prompt
    );
    for (i, (value, why)) in needs.question.options.iter().enumerate() {
        out.push_str(&format!(
            "  {}) {value}  ({why})
",
            i + 1
        ));
    }
    if needs.question.options.is_empty() {
        // `--yes` takes the first option, and there is no first option, so
        // offering it is an instruction to run the same failure again.
        out.push_str(
            "  (pando found no candidates for this)
pando: answer it in pando.toml — nothing pando can accept for you exists here
",
        );
        return out;
    }
    out.push_str(
        "pando: answer it in pando.toml, or rerun with --yes to take the first option
",
    );
    out
}

fn url_suffix(started: &actions::StartedProcess) -> String {
    match &started.url {
        Some(url) => format!(" — {url}"),
        None => String::new(),
    }
}

/// Always stderr, never the listing: `ls --json`'s stdout has to stay
/// parseable, and this is not part of the documented shape.
fn warn_about(owned: &actions::Ownership) {
    if let Some(warning) = &owned.warning {
        eprintln!("pando: {warning}");
    }
}

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

const ORDER: [Col; 7] = [
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
const COL_GAP: usize = 2;

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
fn terminal_width() -> usize {
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
    if let Some(warning) = &refreshed.warning {
        eprintln!("pando: {warning}");
    }
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

fn status_cell(record: Option<&WorktreeRecord>) -> String {
    let Some(record) = record else {
        return "-".to_string();
    };
    match record.processes.values().next() {
        Some(p) => phase_word(&p.phase).to_string(),
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
struct ProjectOut {
    id: String,
    root: String,
    name: String,
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

#[derive(Serialize)]
struct StatusWorktreeOut {
    name: String,
    branch: Option<String>,
    path: String,
    ports: BTreeMap<String, u16>,
    observed_ports: Vec<u16>,
    /// The readiness role's URL, when this worktree has one.
    url: Option<String>,
    processes: BTreeMap<String, ProcessOut>,
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

/// The URL a worktree serves on: what it is really listening on when that is
/// known, falling back to the port pando assigned.
fn worktree_url(record: &WorktreeRecord) -> Option<String> {
    let assigned = record
        .ports
        .get("web")
        .or_else(|| record.ports.values().next());
    let port = match assigned {
        Some(port) if record.observed_ports.contains(port) => *port,
        // It was given a port and is listening somewhere else: a framework
        // that ignores `PORT`, or one that picked the next free number. The
        // URL follows what is really serving, not what pando asked for.
        Some(port) => record.observed_ports.first().copied().unwrap_or(*port),
        None => *record.observed_ports.first()?,
    };
    Some(format!("http://localhost:{port}"))
}

pub fn status_json<W: Write>(paths: &PandoPaths, only: Option<&str>, out: &mut W) -> Result<()> {
    let refreshed = actions::refresh(paths);
    if let Some(warning) = &refreshed.warning {
        eprintln!("pando: {warning}");
    }
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
    let refreshed = actions::refresh(paths);
    if let Some(warning) = &refreshed.warning {
        eprintln!("pando: {warning}");
    }
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
    let width = shown
        .iter()
        .map(|w| w.name.chars().count())
        .max()
        .unwrap_or(4);
    for w in shown {
        let record = refreshed.state.worktrees.get(&w.name);
        let detail = match record {
            Some(r) if !r.processes.is_empty() => {
                let (_, p) = r.processes.iter().next().expect("not empty");
                process_line(p, r)
            }
            Some(r) if !r.ports.is_empty() => {
                format!("stopped   {}", ports_text(&r.ports))
            }
            _ => "stopped".to_string(),
        };
        writeln!(out, "{:<width$}  {detail}", w.name)?;
    }
    Ok(())
}

fn process_line(p: &ProcessRecord, record: &WorktreeRecord) -> String {
    let ports = ports_text(&record.ports);
    match &p.phase {
        Phase::Running { since } => {
            let url = worktree_url(record).unwrap_or_default();
            format!(
                "running   {ports}  {url}  pid {}  up {}",
                p.pid,
                human_duration(Utc::now().signed_duration_since(*since))
            )
        }
        Phase::Starting { since } => format!(
            "starting  {ports}  pid {}  for {}",
            p.pid,
            human_duration(Utc::now().signed_duration_since(*since))
        ),
        Phase::Failed { reason, .. } => format!("failed    {ports}  {reason}"),
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
fn human_duration(d: chrono::TimeDelta) -> String {
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

// ---- logs -----------------------------------------------------------------

/// How often `-f` looks for new lines. Fast enough to feel live, slow
/// enough not to spin a core.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// Lines a follower keeps room for between two polls. Big enough that a
/// dev server's startup burst is printed in full rather than clipped to
/// whatever `--tail` happened to be.
const FOLLOW_CAPACITY: usize = 4096;

#[derive(Serialize)]
struct LogLineOut<'a> {
    /// The line's own timestamp when it has one pando can read, else null.
    ts: Option<String>,
    level: &'static str,
    line: &'a str,
}

fn level_word(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

#[allow(clippy::too_many_arguments)]
pub fn logs<W: Write>(
    paths: &PandoPaths,
    name: &str,
    source: &str,
    tail: usize,
    follow: bool,
    json: bool,
    out: &mut W,
) -> Result<()> {
    let path = paths.log_file(name, source);
    if !path.exists() {
        let available = available_sources(paths, name);
        if available.is_empty() {
            anyhow::bail!(
                "no logs for {name} yet — `pando start {name}` writes them to {}",
                paths.logs_dir(name).display()
            );
        }
        anyhow::bail!(
            "no {source} log for {name} — this worktree has: {}",
            available.join(", ")
        );
    }
    // A one-shot read keeps only what it prints. A follower needs room for
    // whatever arrives between two polls, which has nothing to do with how
    // many lines the reader asked to see first.
    let capacity = if follow {
        tail.max(FOLLOW_CAPACITY)
    } else {
        tail.max(1)
    };
    let mut tailer = log_tail::LogTail::new(path.clone(), capacity);
    tailer
        .poll()
        .with_context(|| format!("read {}", path.display()))?;
    let first = tailer.lines().len().saturating_sub(tail);
    for line in tailer.lines().iter().skip(first) {
        write_log_line(out, &line.plain, line.level, json)?;
    }
    if !follow {
        return Ok(());
    }
    // How many lines have gone past, not how many are in the buffer: the
    // buffer stops growing at `capacity`, and comparing against its length
    // is how a follower goes silent forever a few seconds into a run.
    let mut printed = tailer.lines_seen();
    // Ends on Ctrl-C, which is what `-f` means everywhere else.
    loop {
        std::thread::sleep(FOLLOW_INTERVAL);
        let grew = tailer
            .poll()
            .with_context(|| format!("read {}", path.display()))?;
        if !grew {
            continue;
        }
        let seen = tailer.lines_seen();
        // Truncation resets the file's offsets but not this count, so a
        // restarted process picks up where the follower left off.
        let fresh = seen.saturating_sub(printed) as usize;
        printed = seen;
        let lines: Vec<(String, LogLevel)> = tailer
            .lines()
            .iter()
            .skip(tailer.lines().len().saturating_sub(fresh))
            .map(|l| (l.plain.clone(), l.level))
            .collect();
        for (plain, level) in lines {
            write_log_line(out, &plain, level, json)?;
        }
        out.flush()?;
    }
}

fn write_log_line<W: Write>(out: &mut W, plain: &str, level: LogLevel, json: bool) -> Result<()> {
    if json {
        let entry = LogLineOut {
            ts: leading_timestamp(plain),
            level: level_word(level),
            line: plain,
        };
        writeln!(out, "{}", serde_json::to_string(&entry)?)?;
    } else {
        writeln!(out, "{plain}")?;
    }
    Ok(())
}

/// Every log source this worktree has, from the files that exist. Nothing
/// enumerates the set: a hook adds one by writing one.
fn available_sources(paths: &PandoPaths, name: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(paths.logs_dir(name)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension()?.to_str()? == "log")
                .then(|| path.file_stem()?.to_str().map(str::to_string))?
        })
        .collect();
    out.sort();
    out
}

/// The timestamp a line starts with, when it has one pando can read.
///
/// Dev servers disagree about log formats, so this is deliberately narrow:
/// an RFC 3339 stamp, optionally in brackets, at the very start. Anything
/// else is `null` rather than a guess.
fn leading_timestamp(line: &str) -> Option<String> {
    let first = line.split_whitespace().next()?;
    let trimmed = first.trim_start_matches('[').trim_end_matches(']');
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|ts| ts.with_timezone(&Utc).to_rfc3339())
}

/// The sha `ls --json` publishes: seven characters, always. `head_sha` is
/// enrichment's abbreviation and porcelain's `head` is the full forty, so a
/// worktree whose enrichment failed would otherwise put a different shape
/// into a field documented as `"abc1234"`.
fn short_head(w: &Worktree) -> Option<String> {
    w.head_sha.clone().or_else(|| {
        w.head
            .as_ref()
            .map(|sha| sha.chars().take(SHORT_SHA_LEN).collect())
    })
}

/// Truncate to at most `max` chars with a trailing ellipsis, so a long name
/// never overflows its column.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::testutil::git;
    use clap::CommandFactory;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    struct Fx {
        _dir: TempDir,
        root: PathBuf,
        paths: PandoPaths,
        config: Config,
    }

    fn fixture() -> Fx {
        let dir = tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "root"]);
        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        Fx {
            root: paths.root().to_path_buf(),
            paths,
            config: Config::default(),
            _dir: dir,
        }
    }

    fn capture(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_documents_the_needs_answer_exit_code() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("Exit codes"), "{help}");
        assert!(
            help.contains("3  needs an answer"),
            "an agent has to be able to tell a question from a failure: {help}"
        );
    }

    // ---- ls columns ------------------------------------------------------

    fn widths(pairs: &[(Col, usize)]) -> BTreeMap<Col, usize> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_wide_terminal_keeps_every_column() {
        let w = widths(&[
            (Col::Name, 8),
            (Col::Branch, 8),
            (Col::Head, 7),
            (Col::State, 7),
            (Col::Ports, 5),
            (Col::Status, 8),
            (Col::Path, 40),
        ]);
        assert_eq!(keep_columns(200, &w), ORDER.to_vec());
    }

    // A tmux split is the normal case, so the listing has to survive one:
    // what a worktree is doing outlives what git thinks of it.
    #[test]
    fn a_narrow_terminal_sheds_columns_and_keeps_the_name() {
        let w = widths(&[
            (Col::Name, 10),
            (Col::Branch, 10),
            (Col::Head, 7),
            (Col::State, 7),
            (Col::Ports, 5),
            (Col::Status, 8),
            (Col::Path, 60),
        ]);
        let mid = keep_columns(60, &w);
        assert!(mid.contains(&Col::Name) && mid.contains(&Col::Status));
        assert!(
            !mid.contains(&Col::Path),
            "the path is the first thing to go after the sha"
        );
        let tight = keep_columns(20, &w);
        assert_eq!(tight, vec![Col::Name, Col::Status]);
        let sliver = keep_columns(4, &w);
        assert_eq!(sliver, vec![Col::Name], "the name is never dropped");
    }

    #[test]
    fn ls_shows_the_ports_and_status_of_a_running_worktree() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        let record = store.worktrees.get_mut(&name).unwrap();
        record.ports.insert("web".to_string(), 17_342);
        record.processes.insert(
            "dev".to_string(),
            crate::state::ProcessRecord {
                pid: std::process::id(),
                pgid: std::process::id() as i32,
                started_at: Utc::now(),
                log_path: fx.paths.log_file(&name, "dev"),
                ready_port: Some(17_342),
                ready_timeout_s: None,
                phase: Phase::Running { since: Utc::now() },
            },
        );
        crate::state::save(&fx.paths.state_file(), &store).unwrap();

        let text = capture(|b| ls_text_at(&fx.paths, b, 200));
        assert!(text.contains("PORTS") && text.contains("STATUS"), "{text}");
        assert!(text.contains("17342"), "{text}");
        assert!(text.contains("running"), "{text}");

        let narrow = capture(|b| ls_text_at(&fx.paths, b, 24));
        assert!(narrow.contains("feat+one"), "{narrow}");
        assert!(
            !narrow.contains("PATH"),
            "a narrow listing sheds the path: {narrow}"
        );
    }

    #[test]
    fn a_worktree_with_nothing_running_shows_dashes() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| ls_text_at(&fx.paths, b, 200));
        assert!(text.contains("feat+one"), "{text}");
        assert!(text.contains(" -"), "{text}");
    }

    // The documented behaviour — "what it is really listening on when that
    // is known" — was two identical match arms, so a framework that ignored
    // `PORT` and bound something else still had the assigned port printed
    // as its URL.
    #[test]
    fn the_url_prefers_a_port_the_process_is_really_listening_on() {
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record.ports.insert("web".to_string(), 17_342);

        record.observed_ports = vec![17_342, 17_399];
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:17342"),
            "the assigned port is among them, so it is the one"
        );

        record.observed_ports = vec![3_000];
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:3000"),
            "it ignored the port pando gave it; the URL follows the process"
        );

        record.observed_ports = vec![];
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:17342"),
            "nothing observed at all falls back to what was assigned"
        );
    }

    // ---- questions -------------------------------------------------------

    fn dev_question(options: &[&str]) -> actions::Question {
        actions::Question {
            slot: crate::detect::Slot::DevCmd,
            prompt: "Which command starts the local development server?".to_string(),
            options: options
                .iter()
                .map(|v| (v.to_string(), "a signal".to_string()))
                .collect(),
            preselect: (!options.is_empty()).then_some(0),
            allow_custom: true,
            allow_none: false,
        }
    }

    /// The prompt driven by a script of typed lines, as a terminal would.
    fn answer_with(
        question: &actions::Question,
        lines: &[&str],
    ) -> (Result<actions::Answer>, String) {
        let mut typed = lines.iter().map(|l| format!("{l}\n"));
        let mut out = Vec::new();
        let answer = prompt_with(question, &mut out, || Ok(typed.next()));
        (answer, String::from_utf8(out).unwrap())
    }

    // A fat-fingered number used to fall through to "it must be a command",
    // so `5` on a four-option question became `cmd = "5"`, dated as if a
    // human had meant it, and `start` reported success over a shell error.
    #[test]
    fn a_number_at_the_prompt_is_always_a_choice() {
        let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
        let (answer, printed) = answer_with(&question, &["5", "0", "99", "2"]);
        assert_eq!(answer.unwrap(), actions::Answer::Choice(1));
        assert_eq!(
            printed.matches("pick a number between 1 and 2").count(),
            3,
            "every out-of-range number reprints the range: {printed}"
        );
        assert!(
            !printed.contains("command > "),
            "and none of them is a command: {printed}"
        );
    }

    // The way a command that is only digits is still reachable.
    #[test]
    fn a_number_typed_after_c_is_taken_as_the_command() {
        let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
        let (answer, _) = answer_with(&question, &["c", "5"]);
        assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
    }

    // Unchanged: anything that is not a number is the command itself, so
    // nobody has to discover that `c` exists first.
    #[test]
    fn a_line_that_is_not_a_number_is_still_the_command() {
        let question = dev_question(&["pnpm dev"]);
        let (answer, _) = answer_with(&question, &["./my-own-server"]);
        assert_eq!(
            answer.unwrap(),
            actions::Answer::Custom("./my-own-server".to_string())
        );
    }

    // With nothing on offer, a number cannot be a choice at all.
    #[test]
    fn a_question_with_no_options_takes_a_number_as_the_command() {
        let question = dev_question(&[]);
        let (answer, _) = answer_with(&question, &["5"]);
        assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
    }

    // `--yes` takes the first option; it cannot take one that is not there.
    #[test]
    fn a_question_with_nothing_to_offer_does_not_point_at_yes() {
        let needs = actions::NeedsAnswer {
            question: dev_question(&[]),
        };
        let text = render_needs_answer(&needs);
        assert!(
            !text.contains("--yes"),
            "there is nothing for --yes to take: {text}"
        );
        assert!(text.contains("pando.toml"), "{text}");
    }

    #[test]
    fn a_question_with_options_still_points_at_yes() {
        let needs = actions::NeedsAnswer {
            question: dev_question(&["pnpm dev", "pnpm dev:web"]),
        };
        assert!(render_needs_answer(&needs).contains("--yes"));
    }

    // ---- status ----------------------------------------------------------

    #[test]
    fn status_json_carries_the_documented_shape() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        let record = store.worktrees.get_mut(&name).unwrap();
        record.ports.insert("web".to_string(), 17_342);
        record.observed_ports = vec![17_342, 17_399];
        // Alive, so the read path leaves it Running, with a process group
        // that no longer exists — so a scan that runs and finds nothing is
        // an answer, and the ports it is really listening on are none. The
        // last good answer survives only a scan that could not run at all.
        record.processes.insert(
            "dev".to_string(),
            crate::state::ProcessRecord {
                pid: std::process::id(),
                pgid: 999_998,
                started_at: Utc::now(),
                log_path: fx.paths.log_file(&name, "dev"),
                ready_port: Some(17_342),
                ready_timeout_s: None,
                phase: Phase::Running { since: Utc::now() },
            },
        );
        record.processes.insert(
            "worker".to_string(),
            crate::state::ProcessRecord {
                pid: 4242,
                pgid: 4242,
                started_at: Utc::now(),
                log_path: fx.paths.log_file(&name, "worker"),
                ready_port: None,
                ready_timeout_s: None,
                phase: Phase::Failed {
                    at: Utc::now(),
                    reason: "process exited".to_string(),
                },
            },
        );
        record.hooks.insert(
            "install".to_string(),
            crate::state::HookRecord {
                fingerprint: Some("md5:abc".to_string()),
                ran_at: Utc::now(),
            },
        );
        crate::state::save(&fx.paths.state_file(), &store).unwrap();

        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["project"]["name"], "acme-shop");
        let wt = &v["worktrees"][0];
        assert_eq!(wt["name"], "feat+one");
        assert_eq!(wt["branch"], "feat/one");
        assert_eq!(wt["ports"]["web"], 17_342);
        assert_eq!(wt["observed_ports"], serde_json::json!([]));
        assert_eq!(wt["url"], "http://localhost:17342");
        let dev = &wt["processes"]["dev"];
        assert_eq!(dev["pid"], std::process::id());
        assert_eq!(dev["phase"], "running");
        assert_eq!(dev["reason"], serde_json::Value::Null);
        assert!(dev["since"].is_string());
        assert!(dev["log"].as_str().unwrap().ends_with("dev.log"));
        let worker = &wt["processes"]["worker"];
        assert_eq!(worker["phase"], "failed");
        assert_eq!(worker["reason"], "process exited");
        assert_eq!(wt["hooks"]["install"]["fingerprint"], "md5:abc");
    }

    #[test]
    fn status_json_reports_a_worktree_that_was_never_started() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let wt = &v["worktrees"][0];
        assert!(wt["processes"].as_object().unwrap().is_empty());
        assert_eq!(wt["url"], serde_json::Value::Null);
        assert!(wt["observed_ports"].as_array().unwrap().is_empty());
    }

    #[test]
    fn status_can_be_asked_about_one_worktree() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        actions::new(&fx.paths, &fx.config, "feat/two", None, &|_| {}).unwrap();
        let text = capture(|b| status_json(&fx.paths, Some("feat+two"), b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"].as_array().unwrap().len(), 1);
        assert_eq!(v["worktrees"][0]["name"], "feat+two");

        let text = capture(|b| status_text(&fx.paths, Some("nope"), b));
        assert!(text.contains("no worktree named"), "{text}");
    }

    #[test]
    fn status_text_names_what_each_worktree_is_doing() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| status_text(&fx.paths, None, b));
        assert!(text.contains("feat+one"), "{text}");
        assert!(text.contains("stopped"), "{text}");
    }

    #[test]
    fn uptime_reads_in_the_unit_that_fits() {
        use chrono::TimeDelta;
        assert_eq!(human_duration(TimeDelta::seconds(9)), "9s");
        assert_eq!(human_duration(TimeDelta::seconds(70)), "1m10s");
        assert_eq!(human_duration(TimeDelta::seconds(3_700)), "1h1m");
        assert_eq!(human_duration(TimeDelta::seconds(90_000)), "1d1h");
        assert_eq!(human_duration(TimeDelta::seconds(-5)), "0s");
    }

    // ---- logs ------------------------------------------------------------

    fn write_log(fx: &Fx, name: &str, source: &str, text: &str) {
        let path = fx.paths.log_file(name, source);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn logs_prints_the_last_lines() {
        let fx = fixture();
        write_log(&fx, "feat+one", "dev", "one\ntwo\nthree\nfour\n");
        let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 2, false, false, b));
        assert_eq!(text, "three\nfour\n");
    }

    #[test]
    fn logs_json_emits_one_object_per_line() {
        let fx = fixture();
        write_log(
            &fx,
            "feat+one",
            "dev",
            "2026-09-20T10:00:00Z ready in 412ms\nError: it broke\n",
        );
        let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 10, false, true, b));
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ts"], "2026-09-20T10:00:00+00:00");
        assert_eq!(lines[0]["level"], "info");
        assert!(lines[0]["line"].as_str().unwrap().contains("ready in"));
        assert_eq!(lines[1]["ts"], serde_json::Value::Null);
        assert_eq!(lines[1]["level"], "error");
    }

    #[test]
    fn a_line_with_no_timestamp_pando_can_read_gets_null() {
        assert_eq!(
            leading_timestamp("2026-09-20T10:00:00Z ready"),
            Some("2026-09-20T10:00:00+00:00".to_string())
        );
        assert_eq!(
            leading_timestamp("[2026-09-20T10:00:00+02:00] ready"),
            Some("2026-09-20T08:00:00+00:00".to_string())
        );
        assert_eq!(leading_timestamp("ready in 412ms"), None);
        assert_eq!(leading_timestamp(""), None);
        assert_eq!(leading_timestamp("20/09/2026 10:00:00 ready"), None);
    }

    #[test]
    fn logs_names_the_sources_a_worktree_has() {
        let fx = fixture();
        let err = logs(
            &fx.paths,
            "feat+one",
            "dev",
            10,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("no logs for feat+one"),
            "{err:#}"
        );

        write_log(&fx, "feat+one", "install", "installing\n");
        let err = logs(
            &fx.paths,
            "feat+one",
            "dev",
            10,
            false,
            false,
            &mut Vec::new(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("install"),
            "it says what is there instead: {msg}"
        );
    }

    #[test]
    fn ls_text_says_so_when_there_are_no_worktrees() {
        let fx = fixture();
        let text = capture(|b| ls_text(&fx.paths, b));
        assert!(text.contains("no worktrees"), "{text}");
    }

    #[test]
    fn ls_text_lists_name_branch_head_state_and_path() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| ls_text(&fx.paths, b));

        assert!(text.contains("NAME"), "{text}");
        assert!(text.contains("feat+one"), "{text}");
        assert!(text.contains("feat/one"), "{text}");
        assert!(text.contains("pando"), "state column: {text}");
        assert!(
            text.contains(
                &fx.paths
                    .worktrees_dir()
                    .join("feat+one")
                    .display()
                    .to_string()
            ),
            "{text}"
        );
    }

    #[test]
    fn ls_text_marks_adopted_dirty_and_gone_worktrees() {
        let fx = fixture();
        let adopted = fx.root.parent().unwrap().join("adopted");
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "adopted",
                adopted.to_str().unwrap(),
            ],
        );
        let dirty = actions::new(&fx.paths, &fx.config, "feat/dirty", None, &|_| {}).unwrap();
        std::fs::write(
            fx.paths.worktrees_dir().join(&dirty).join("scratch.txt"),
            "wip",
        )
        .unwrap();
        let gone = actions::new(&fx.paths, &fx.config, "feat/gone", None, &|_| {}).unwrap();
        std::fs::remove_dir_all(fx.paths.worktrees_dir().join(&gone)).unwrap();

        let text = capture(|b| ls_text(&fx.paths, b));
        for word in ["adopted", "dirty", "gone"] {
            assert!(text.contains(word), "missing {word:?} in:\n{text}");
        }
    }

    #[test]
    fn ls_json_emits_the_documented_shape() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(v["version"], 1);
        assert_eq!(v["project"]["id"], fx.paths.project.id.as_str());
        assert_eq!(v["project"]["name"], "acme-shop");
        assert_eq!(v["project"]["root"], fx.root.display().to_string().as_str());

        let w = &v["worktrees"][0];
        assert_eq!(w["name"], "feat+one");
        assert_eq!(w["branch"], "feat/one");
        assert_eq!(w["detached"], false);
        assert_eq!(w["dirty"], false);
        assert_eq!(w["ahead"], 0);
        assert_eq!(w["behind"], 0);
        assert_eq!(w["created_by_pando"], true);
        assert_eq!(w["prunable"], false);
        assert_eq!(w["locked"], serde_json::Value::Null);
        assert_eq!(w["pr"], serde_json::Value::Null);
        assert!(w["head"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(w["path"].as_str().is_some_and(|s| s.starts_with('/')));
    }

    #[test]
    fn ls_json_is_an_empty_list_rather_than_an_error_with_no_worktrees() {
        let fx = fixture();
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ls_json_reports_a_locked_worktree_with_its_reason() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        git(
            &fx.root,
            &[
                "worktree",
                "lock",
                "--reason",
                "benchmarking",
                fx.paths.worktrees_dir().join(&name).to_str().unwrap(),
            ],
        );
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["locked"], "benchmarking");
    }

    // The CLI never spawns `gh`; chips come from whatever the TUI last saw.
    #[test]
    fn ls_json_fills_the_pr_field_from_the_cache() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let mut prs = cache::PrCacheFile::new();
        prs.prs.insert(
            "feat/one".into(),
            crate::worktree::PrInfo {
                number: 42,
                title: "feat: one".into(),
                branch: "feat/one".into(),
                author: "dev".into(),
                draft: false,
                state: PrState::Open,
                url: "https://example.test/pull/42".into(),
            },
        );
        cache::save_prs(&fx.paths.pr_cache_file(), &prs).unwrap();

        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["pr"]["number"], 42);
        assert_eq!(v["worktrees"][0]["pr"]["state"], "open");
        assert_eq!(
            v["worktrees"][0]["pr"]["url"],
            "https://example.test/pull/42"
        );
    }

    #[test]
    fn ellipsize_keeps_short_strings_and_truncates_long_ones() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("abcdefghij", 5), "abcd…");
    }

    // `head` is documented as "abc1234". Enrichment supplies the seven
    // characters, but porcelain's sha is all forty, so a worktree whose
    // enrichment failed used to publish a different shape in the same field.
    #[test]
    fn the_json_head_is_always_the_short_sha() {
        let mut w = crate::tui::app::tests::wt("feat+one");
        w.head = Some("0123456789012345678901234567890123456789".into());
        w.head_sha = None;
        assert_eq!(short_head(&w).as_deref(), Some("0123456"));

        w.head_sha = Some("abc1234".into());
        assert_eq!(short_head(&w).as_deref(), Some("abc1234"));

        w.head = None;
        w.head_sha = None;
        assert_eq!(short_head(&w), None);
    }
}
