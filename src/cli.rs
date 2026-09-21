//! The clap front end. A thin wrapper: every behaviour lives in `actions`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use crate::actions;
use crate::actions::worktree_url;
use crate::cache;
use crate::config::Config;
use crate::log_tail::{self, LogLevel};
use crate::paths::PandoPaths;
use crate::share_proxy;
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
    /// Start a worktree's processes.
    Start {
        name: String,
        /// Accept pando's own recommendation for anything it would ask.
        /// Without it, an unanswerable question exits 3.
        #[arg(long)]
        yes: bool,
        /// One process by name, instead of every one config declares. The
        /// rest are left exactly as they are, on the ports they have.
        #[arg(long)]
        only: Option<String>,
        /// Run private copies of the project's services for this worktree,
        /// on ports of its own. Remembered: a later plain `start` keeps
        /// them.
        #[arg(long)]
        isolated: bool,
    },
    /// Answer every question pando has about this project, in one pass.
    ///
    /// The batch form of the questions `new` and `start` ask just in time,
    /// through the same paths: nothing is started, nothing is written into
    /// the repository, and a second run asks nothing.
    Init {
        /// Accept pando's own recommendation for anything it would ask.
        /// Without it, an unanswerable question exits 3.
        #[arg(long)]
        yes: bool,
        /// A JSON file of answers — `-` reads stdin. One key per question,
        /// named as `pando signals` names it, whose value is the option's
        /// own text, a command of your own, a list for a question whose
        /// answer is a set, or null for "none of them". Every answer goes
        /// through the same checks a person's does and is written down as
        /// a program's.
        #[arg(long, value_name = "PATH")]
        answers: Option<String>,
        /// Print the config this would write, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Stop a worktree's processes, or every worktree's when given no name.
    Stop {
        name: Option<String>,
        /// One process by name. The others keep running.
        #[arg(long, requires = "name")]
        only: Option<String>,
    },
    /// Stop and start again, keeping the ports.
    Restart {
        name: String,
        /// Accept pando's own recommendation for anything it would ask.
        /// Without it, an unanswerable question exits 3.
        #[arg(long)]
        yes: bool,
        /// One process by name. The others are not restarted.
        #[arg(long)]
        only: Option<String>,
        /// Run private copies of the project's services for this worktree.
        #[arg(long)]
        isolated: bool,
    },
    /// What is running, and on which ports.
    Status {
        name: Option<String>,
        #[arg(long)]
        json: bool,
        /// `export KEY=value` lines for this worktree's resolved
        /// environment, so a shell can `eval "$(pando status --env <name>)"`
        /// and run the project's own commands by hand.
        #[arg(long, requires = "name", conflicts_with = "json")]
        env: bool,
    },
    /// Publish a running worktree at a public URL.
    Share { name: String },
    /// Take a worktree's public URL down.
    Unshare { name: String },
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
    /// The share proxy. Spawned by `share`, never run by hand: it reads its
    /// cookie from the environment and would have nothing to inject.
    #[command(name = share_proxy::SUBCOMMAND, hide = true)]
    ShareProxy {
        #[arg(long)]
        listen: u16,
        #[arg(long)]
        upstream: u16,
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
            Command::New { .. }
                | Command::Start { .. }
                | Command::Restart { .. }
                // `[share]` says which provider to use and whether there is
                // an auth command. `unshare` needs none of that: the record
                // holds both pgids, and taking a URL down is something you
                // want most when the config is broken.
                | Command::Share { .. }
                // `init`'s whole job is to fill this file in. A layer
                // pando cannot read is exactly the thing to fix first,
                // and patching on top of one it could not parse would
                // lose whatever is in it.
                | Command::Init { .. }
                // `--env` renders templates, which only config holds. Plain
                // `status` still runs on whatever is left.
                | Command::Status { env: true, .. }
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
            actions::rm(paths, &name, yes, force, &notice)?;
            writeln!(out, "removed {name}")?;
            Ok(())
        }
        Command::Path { name } => {
            writeln!(out, "{}", actions::path(paths, &name)?.display())?;
            Ok(())
        }
        Command::Start {
            name,
            yes,
            only,
            isolated,
        } => {
            let config = &actions::resolve_process(paths, config, isolated, &asker(yes), &notice)?;
            let report = actions::start(paths, config, &name, only.as_deref(), isolated, &notice)?;
            if report.reassigned {
                eprintln!("pando: the ports {name} had were taken; it moved to new ones");
            }
            let url = url_suffix(report.url.as_deref());
            if report.started_nothing() {
                writeln!(out, "{name} is already running{url}")?;
            } else {
                writeln!(out, "started {name}{url}")?;
            }
            Ok(())
        }
        Command::Init {
            yes,
            answers,
            dry_run,
        } => {
            let answers = answers.as_deref().map(read_answers).transpose()?;
            let ask = init_asker(answers.as_ref(), yes);
            let (report, preview) = match dry_run {
                true => actions::init_dry_run(paths, config, &ask, &notice)?,
                false => (actions::init(paths, config, &ask, &notice)?, Vec::new()),
            };
            // Before the summary, because it is about what the file the
            // summary describes does *not* say.
            if let Some(answers) = &answers {
                report_unused(answers, config);
            }
            for warning in &report.warnings {
                notice(warning);
            }
            if !dry_run {
                write!(out, "{}", render_init(&report, "wrote"))?;
                return Ok(());
            }
            for (path, body) in &preview {
                writeln!(out, "# {}", path.display())?;
                write!(out, "{body}")?;
            }
            // The summary goes to stderr on this path: stdout is the
            // config, so `pando init --dry-run > preview.toml` is a file
            // and nothing else.
            eprint!("{}", render_init(&report, "would write"));
            Ok(())
        }
        Command::Stop { name, only } => match name {
            Some(name) => {
                match actions::stop(paths, &name, only.as_deref(), &notice)? {
                    actions::StopOutcome::Stopped(processes) => {
                        // Empty when the worktree had only its services
                        // left up, which is a real thing to stop and a
                        // silly thing to narrate as "stopped ".
                        if !processes.is_empty() {
                            notice(&format!("stopped {}", processes.join(", ")));
                        }
                        writeln!(out, "stopped {name}")?;
                    }
                    actions::StopOutcome::NotRunning => writeln!(out, "{name} was not running")?,
                }
                Ok(())
            }
            None => {
                let stopped = actions::stop_all(paths, &notice)?;
                if stopped.is_empty() {
                    writeln!(out, "nothing was running")?;
                } else {
                    writeln!(out, "stopped {}", stopped.join(", "))?;
                }
                Ok(())
            }
        },
        Command::Restart {
            name,
            yes,
            only,
            isolated,
        } => {
            // Resolved exactly as `start` resolves it: a project whose
            // process question has never been answered gets the question,
            // not a refusal.
            let config = &actions::resolve_process(paths, config, isolated, &asker(yes), &notice)?;
            let report =
                actions::restart(paths, config, &name, only.as_deref(), isolated, &notice)?;
            writeln!(out, "restarted {name}{}", url_suffix(report.url.as_deref()))?;
            Ok(())
        }
        Command::Status { name, json, env } => {
            if env {
                let name = name.as_deref().expect("--env requires a name");
                let resolved = actions::resolved_env(paths, config, name)?;
                write!(out, "{}", actions::export_lines(&resolved))?;
                return Ok(());
            }
            if json {
                status_json(paths, name.as_deref(), &mut out)
            } else {
                status_text(paths, name.as_deref(), &mut out)
            }
        }
        Command::Share { name } => {
            let outcome = actions::share(paths, config, &name, &notice)?;
            if outcome.already {
                notice(&format!("{name} was already shared"));
            }
            if outcome.pre_authed {
                notice("a proxy in front of it is injecting the Cookie header from auth_cmd");
            }
            // The URL alone on stdout, so `open "$(pando share x)"` works.
            writeln!(out, "{}", outcome.public_url)?;
            Ok(())
        }
        Command::Unshare { name } => {
            actions::unshare(paths, &name)?;
            writeln!(out, "unshared {name}")?;
            Ok(())
        }
        Command::Logs {
            name,
            source,
            tail,
            follow,
            json,
        } => logs(paths, &name, &source, tail, follow, json, &mut out),
        // Never reached: `main` runs the proxy before it goes looking for a
        // repository, because the proxy has none.
        Command::ShareProxy { listen, upstream } => run_share_proxy(listen, upstream),
    }
}

/// Runs the share proxy, reading its cookie from the environment.
///
/// Called from `main` before project discovery: the proxy's working
/// directory is a temp directory, it has no repository and no config, and
/// the one thing it needs is an environment variable.
pub fn run_share_proxy(listen: u16, upstream: u16) -> Result<()> {
    let cookie = std::env::var(share_proxy::ENV_COOKIE).with_context(|| {
        format!(
            "{} is not set — `{}` is spawned by `pando share`, not run by hand",
            share_proxy::ENV_COOKIE,
            share_proxy::SUBCOMMAND
        )
    })?;
    share_proxy::run_in_process(listen, upstream, &cookie)
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
            // A set answer: `--yes` takes the options a rule already
            // resolved and leaves the rest, which is exactly what the
            // question pre-ticks.
            if question.multi {
                let taken: Vec<&str> = question
                    .checked
                    .iter()
                    .filter_map(|i| question.options.get(*i))
                    .map(|(value, _)| value.as_str())
                    .collect();
                notice(&format!("--yes: taking {}", joined(&taken)));
                // `Auto`, not `Many`: nobody chose these, a flag took the
                // ones the rules had resolved, and the comment written to
                // `pando.toml` has to say which of the two happened.
                return Ok(actions::Answer::Auto(0));
            }
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
    read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    if question.multi {
        return prompt_many(question, out, read);
    }
    prompt_one(question, out, read)
}

/// The report a question carries, indented under its prompt. Everything
/// pando knows that made it worth asking, and on stderr like the question
/// itself.
fn write_details(question: &actions::Question, out: &mut impl Write) -> Result<()> {
    for line in &question.details {
        writeln!(out, "  {line}")?;
    }
    Ok(())
}

/// What "none" means, which is different for each slot that offers it.
fn none_label(slot: crate::detect::Slot) -> &'static str {
    match slot {
        crate::detect::Slot::Prelude => {
            "none — this machine needs no line in front of its commands"
        }
        crate::detect::Slot::Provision => "none — a new worktree needs no local file of yours",
        _ => "none — this process has no port",
    }
}

/// `a, b` — the readable form of a set, for a notice.
fn joined(values: &[&str]) -> String {
    if values.is_empty() {
        return "none of them".to_string();
    }
    values.join(", ")
}

/// A question whose answer is a *set*: every option with a checkbox,
/// toggled by number, confirmed with enter.
///
/// Toggling rather than typing a list, because the pre-ticked set is
/// already the answer for most projects: enter accepts it, and the one
/// service the rules could not place is one keystroke away.
fn prompt_many(
    question: &actions::Question,
    out: &mut impl Write,
    mut read: impl FnMut() -> Result<Option<String>>,
) -> Result<actions::Answer> {
    let mut checked: Vec<bool> = (0..question.options.len())
        .map(|i| question.checked.contains(&i))
        .collect();
    let width = question
        .options
        .iter()
        .map(|(value, _)| value.chars().count())
        .max()
        .unwrap_or(0);
    writeln!(out, "pando: {}", question.prompt)?;
    write_details(question, out)?;
    loop {
        for (i, (value, why)) in question.options.iter().enumerate() {
            let box_ = if checked[i] { "[x]" } else { "[ ]" };
            writeln!(out, "  {box_} {}) {value:<width$}  {why}", i + 1)?;
        }
        write!(
            out,
            "  a number toggles it, ⏎ accepts [{}] > ",
            joined(
                &question
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| checked[*i])
                    .map(|(_, (value, _))| value.as_str())
                    .collect::<Vec<_>>()
            )
        )?;
        out.flush()?;
        let Some(line) = read()? else {
            return Err(needs_answer(question));
        };
        let line = line.trim();
        if line.is_empty() {
            let chosen: Vec<usize> = checked
                .iter()
                .enumerate()
                .filter(|(_, on)| **on)
                .map(|(i, _)| i)
                .collect();
            return Ok(if chosen.is_empty() {
                actions::Answer::None
            } else {
                actions::Answer::Many(chosen)
            });
        }
        if question.allow_none && (line == "n" || line == "N") {
            return Ok(actions::Answer::None);
        }
        match line
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1 && *n <= checked.len())
        {
            Some(n) => checked[n - 1] = !checked[n - 1],
            None => writeln!(
                out,
                "pando: a number between 1 and {} toggles one, n takes none, ⏎ accepts",
                checked.len()
            )?,
        }
    }
}

fn prompt_one(
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
    write_details(question, out)?;
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
        writeln!(out, "   n) {}", none_label(question.slot))?;
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

// ---- an answers file ------------------------------------------------------

/// A mistake in what was asked for, rather than a failure of what pando
/// tried to do. Exit 2, the code clap's own argument errors use.
///
/// Its own type because `--answers` is a contract a program writes
/// against: "you named a question pando does not ask" has to be
/// distinguishable from "the command failed" without reading English.
#[derive(Debug, Clone)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(message.into()))
}

/// The name a slot goes by in an answers file and in `signals` — derived
/// from the type itself, so the file a program writes and the JSON it read
/// cannot drift apart.
pub fn slot_name(slot: crate::detect::Slot) -> String {
    serde_json::to_value(slot)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .expect("every slot serialises to its name")
}

fn slot_named(name: &str) -> Option<crate::detect::Slot> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// Every name an answers file may use, in the order the questions come.
fn slot_names() -> Vec<String> {
    actions::ALL_SLOTS.iter().copied().map(slot_name).collect()
}

/// What a program answered, slot by slot.
///
/// The write path an agent uses, so it is a contract rather than a
/// convenience. Nothing here writes TOML: every value goes through the
/// same `resolve` a person's answer does — the same candidates, the same
/// checks, the same file — and lands with a comment saying a program chose
/// it.
#[derive(Debug)]
pub struct Answers {
    by_slot: BTreeMap<crate::detect::Slot, serde_json::Value>,
    /// Which slots a question actually reached, so the ones nothing asked
    /// about can be reported instead of silently dropped.
    asked: std::cell::RefCell<std::collections::BTreeSet<crate::detect::Slot>>,
}

impl Answers {
    /// Parses the file. A name pando does not ask about is a usage error
    /// naming it: a silent skip would leave a program believing it had
    /// answered something.
    pub fn parse(text: &str) -> Result<Answers> {
        let parsed: serde_json::Value =
            serde_json::from_str(text).map_err(|e| usage(format!("--answers is not JSON: {e}")))?;
        let serde_json::Value::Object(object) = parsed else {
            return Err(usage(
                "--answers takes a JSON object of question name to answer",
            ));
        };
        let mut by_slot = BTreeMap::new();
        for (name, value) in object {
            let slot = slot_named(&name).ok_or_else(|| {
                usage(format!(
                    "--answers names {name:?}, which is not a question pando asks — it asks \
                     about: {}",
                    slot_names().join(", ")
                ))
            })?;
            // Checked here, before a single key is written: a shape this
            // slot cannot take is knowable from the slot alone, and a
            // program should learn it from the file it just sent rather
            // than from a half-written config.
            check_shape(slot, &value)?;
            by_slot.insert(slot, value);
        }
        Ok(Answers {
            by_slot,
            asked: std::cell::RefCell::new(std::collections::BTreeSet::new()),
        })
    }

    /// The answer for this question, when the file has one for its slot.
    fn for_question(&self, question: &actions::Question) -> Option<Result<actions::Answer>> {
        let value = self.by_slot.get(&question.slot)?;
        self.asked.borrow_mut().insert(question.slot);
        Some(answer_from(question, value))
    }

    /// Slots the file answered that no question ever asked about.
    fn unasked(&self) -> Vec<crate::detect::Slot> {
        let asked = self.asked.borrow();
        self.by_slot
            .keys()
            .copied()
            .filter(|slot| !asked.contains(slot))
            .collect()
    }
}

/// Which shapes a slot's answer may take, decided from the slot alone.
///
/// Three, and each one means the same thing everywhere: the text of an
/// option or a command of your own, a list for an answer that is a set or
/// a list of files, and null for "none of them". Anything else is a usage
/// error naming the question, never a silent skip — and it is caught when
/// the file is read, so a bad shape on a slot no rule ends up asking
/// about is still reported.
fn check_shape(slot: crate::detect::Slot, value: &serde_json::Value) -> Result<()> {
    let name = slot_name(slot);
    match value {
        serde_json::Value::Null if !slot.allows_none() => Err(usage(format!(
            "{name} has no \"none\" answer — null answers only the questions that have one"
        ))),
        serde_json::Value::Null => Ok(()),
        serde_json::Value::String(text) if text.trim().is_empty() => Err(usage(format!(
            "{name} was answered with an empty string — null is how you say none"
        ))),
        serde_json::Value::String(_) if slot.is_multi() => Err(usage(format!(
            "{name} is answered with a list of the options, or null for none of them"
        ))),
        serde_json::Value::String(_) => Ok(()),
        serde_json::Value::Array(items) => {
            if !slot.is_multi() && !slot.is_list() {
                return Err(usage(format!(
                    "{name} takes one answer, not a list of them"
                )));
            }
            if items.iter().any(|item| !item.is_string()) {
                return Err(usage(format!("{name} takes a list of strings")));
            }
            // An empty list is the recorded answer "none of them" at the
            // set question, and nothing at all at a list of files.
            if !slot.is_multi() && items.is_empty() {
                return Err(usage(format!(
                    "{name} was answered with an empty list — null is how you say none"
                )));
            }
            Ok(())
        }
        other => Err(usage(format!(
            "{name} takes a string, a list of strings, or null — not {other}"
        ))),
    }
}

/// One JSON value, as an answer to one question.
///
/// The shapes are [`check_shape`]'s; what is left here is matching what a
/// program sent against what the rules actually offered.
fn answer_from(question: &actions::Question, value: &serde_json::Value) -> Result<actions::Answer> {
    check_shape(question.slot, value)?;
    let name = slot_name(question.slot);
    // Everything from here is a program's answer, which is what the note
    // beside the key it fills has to say.
    let program = |answer| Ok(actions::Answer::Program(Box::new(answer)));
    match value {
        serde_json::Value::Null => program(actions::Answer::None),
        serde_json::Value::String(text) => {
            let text = text.trim();
            match option_named(question, text) {
                // By value, not by index: the option carries more than its
                // text — the roles a command owns, the whole process table
                // a workspace answer is — and a list of indexes is a
                // contract that breaks the day a rule finds one more
                // candidate.
                Some(index) => program(actions::Answer::Choice(index)),
                None if question.allow_custom => program(actions::Answer::Custom(text.to_string())),
                None => Err(unknown_option(question, &name, text)),
            }
        }
        serde_json::Value::Array(items) => {
            let chosen: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.trim().to_string()))
                .collect();
            if question.multi {
                let mut indexes = Vec::new();
                for text in &chosen {
                    let index = option_named(question, text)
                        .ok_or_else(|| unknown_option(question, &name, text))?;
                    indexes.push(index);
                }
                // The empty set is the recorded answer "none of them",
                // which is a different thing from never having been asked.
                return match indexes.is_empty() {
                    true => program(actions::Answer::None),
                    false => program(actions::Answer::Many(indexes)),
                };
            }
            // A list slot's answer is one value with the files in it, and
            // a program should not have to know how they are joined.
            let joined = crate::detect::join_list(&chosen);
            match option_named(question, &joined) {
                Some(index) => program(actions::Answer::Choice(index)),
                None => program(actions::Answer::Custom(joined)),
            }
        }
        other => Err(usage(format!(
            "{name} takes a string, a list of strings, or null — not {other}"
        ))),
    }
}

fn option_named(question: &actions::Question, text: &str) -> Option<usize> {
    question
        .options
        .iter()
        .position(|(value, _)| value.trim() == text)
}

/// Naming what was on offer, because a set answer has no other way in: a
/// service pando did not find is not a service it can run.
fn unknown_option(question: &actions::Question, name: &str, text: &str) -> anyhow::Error {
    let offered: Vec<&str> = question
        .options
        .iter()
        .map(|(value, _)| value.as_str())
        .collect();
    usage(format!(
        "{name} was answered {text:?}, which is not one of its options — they are: {}",
        match offered.is_empty() {
            true => "none at all".to_string(),
            false => offered.join(", "),
        }
    ))
}

/// Reads an answers file, or stdin for `-`.
fn read_answers(path: &str) -> Result<Answers> {
    let text = match path {
        "-" => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                .map_err(|e| usage(format!("--answers -: {e}")))?;
            buf
        }
        path => {
            std::fs::read_to_string(path).map_err(|e| usage(format!("--answers {path}: {e}")))?
        }
    };
    Answers::parse(&text)
}

/// The `init` asker: the answers file where it has something to say, and
/// the ordinary one — a terminal, `--yes`, or exit 3 — everywhere else.
fn init_asker(
    answers: Option<&Answers>,
    yes: bool,
) -> impl Fn(&actions::Question) -> Result<actions::Answer> + '_ {
    let fallback = asker(yes);
    move |question: &actions::Question| {
        if let Some(answers) = answers
            && let Some(answer) = answers.for_question(question)
        {
            return answer;
        }
        fallback(question)
    }
}

/// What the answers file said that nothing used.
///
/// Never silent: a program that answered a question pando did not ask has
/// to learn that from the run rather than from a config that looks nothing
/// like what it sent.
fn report_unused(answers: &Answers, before: &Config) {
    for slot in answers.unasked() {
        let name = slot_name(slot);
        if actions::already_answered(slot, before) {
            notice(&format!(
                "{name} is already answered — the answers file was not applied to it"
            ));
        } else {
            notice(&format!(
                "nothing asked about {name} in this run — the answers file's value for it was \
                 not used"
            ));
        }
    }
}

/// What `init` prints: where the answers went, and what they say.
///
/// The path first, because the one thing a developer wants after a batch
/// of questions is the file to go and read.
fn render_init(report: &actions::InitReport, verb: &str) -> String {
    let mut out = String::new();
    if report.answered_anything() {
        out.push_str(&format!("{verb} {}\n", report.config_file.display()));
        // Named only when this run put something there: the prelude is
        // about the machine, and a developer who never answered it should
        // not be pointed at a file pando did not touch.
        if let Some(user) = &report.user_file {
            out.push_str(&format!("{verb} {}\n", user.display()));
        }
    } else if report.config_file.exists() {
        out.push_str(&format!(
            "nothing left to answer — {} already says it all\n",
            report.config_file.display()
        ));
    } else {
        out.push_str("nothing to answer — pando has no questions about this project\n");
    }
    let width = report
        .slots
        .iter()
        .filter(|slot| slot.value.is_some())
        .map(|slot| slot.label.chars().count())
        .max()
        .unwrap_or(0);
    for slot in &report.slots {
        let Some(value) = &slot.value else { continue };
        out.push_str(&format!("  {:<width$}  {value}\n", slot.label));
    }
    out
}

/// What exit code 3 prints: the question, its options, and the two ways out.
pub fn render_needs_answer(needs: &actions::NeedsAnswer) -> String {
    let mut out = format!(
        "pando: {}
",
        needs.question.prompt
    );
    for line in &needs.question.details {
        out.push_str(&format!("  {line}\n"));
    }
    for (i, (value, why)) in needs.question.options.iter().enumerate() {
        // A set question shows what it would take, because that is what
        // `--yes` would accept and the thing an agent has to decide about.
        let prefix = match needs.question.multi {
            true if needs.question.checked.contains(&i) => "[x] ".to_string(),
            true => "[ ] ".to_string(),
            false => String::new(),
        };
        out.push_str(&format!(
            "  {prefix}{}) {value}  ({why})
",
            i + 1
        ));
    }
    if needs.question.multi {
        out.push_str(
            "pando: answer it in pando.toml with a [[services]] table, or rerun with --yes to \
             take the ticked ones
",
        );
        return out;
    }
    let file = answer_file(needs.question.slot);
    if needs.question.options.is_empty() {
        // `--yes` takes the first option, and there is no first option, so
        // offering it is an instruction to run the same failure again.
        out.push_str(&format!(
            "  (pando found no candidates for this)
pando: answer it in {file} — nothing pando can accept for you exists here
"
        ));
        return out;
    }
    if needs.question.preselect.is_none() {
        // There are options, and none of them is one a flag may take: this
        // slot's answer would have pando create a file out of contents it
        // did not write. Pointing at `--yes` here is an instruction to run
        // the same failure again.
        out.push_str(&format!(
            "pando: answer it in {file} — none of these is an option --yes may take for you
"
        ));
        return out;
    }
    out.push_str(&format!(
        "pando: answer it in {file}, or rerun with --yes to take the first option
"
    ));
    out
}

/// Which file an answer to this question is written to, so the way out
/// names the file to edit rather than the usual one.
///
/// The prelude is about the machine, not the project, and its answer lives
/// in pando's machine-wide config — telling an agent to put it in
/// `pando.toml` would send it to a file pando will not read it from.
fn answer_file(slot: crate::detect::Slot) -> &'static str {
    match slot.layer() {
        crate::config::Layer::User => "~/.pando/config.toml",
        crate::config::Layer::Project => "pando.toml",
    }
}

fn url_suffix(url: Option<&str>) -> String {
    match url {
        Some(url) => format!(" — {url}"),
        None => String::new(),
    }
}

/// What a refresh found and what it had to do about it — a share whose
/// tunnel died, most often. Always stderr: `--json`'s stdout has to stay
/// parseable, and none of this is part of the documented shape.
fn report_refresh(refreshed: &actions::Refreshed) {
    if let Some(warning) = &refreshed.warning {
        eprintln!("pando: {warning}");
    }
    for notice in &refreshed.notices {
        eprintln!("pando: {notice}");
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

/// One private service, flattened for the machine-readable shape.
#[derive(Serialize)]
struct ServiceOut {
    /// `compose` for now; `native` joins it in Phase 6.
    kind: &'static str,
    port: Option<u16>,
    /// Whether something answers on that port right now.
    up: bool,
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
            let row = format!(
                "  {:<service_width$}  {:<PHASE_CELL$}  service on {port}",
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
    // `source` is a path component of the file about to be read, so a
    // traversal here reads outside the worktree's log directory entirely —
    // a file `available_sources` would never list.
    crate::paths::validate_log_source("log source", source)?;
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
                observed_ports: Vec::new(),
                swept: false,
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

    /// A process record for a live group with the sockets it was seen
    /// holding.
    fn listening(pgid: i32, observed: &[u16]) -> crate::state::ProcessRecord {
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid,
            started_at: Utc::now(),
            log_path: PathBuf::from("/does/not/exist/dev.log"),
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: observed.to_vec(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        }
    }

    /// A worktree shaped like a one-process start: `dev` owning `web`.
    fn one_process_record(observed: &[u16]) -> WorktreeRecord {
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record.ports.insert("web".to_string(), 17_342);
        record
            .roles
            .insert("dev".to_string(), vec!["web".to_string()]);
        record
            .processes
            .insert("dev".to_string(), listening(101, observed));
        record.observed_ports = observed.to_vec();
        record
    }

    /// A worktree shaped like a two-process start: `web` and `api`, each
    /// owning its own role and running in its own group.
    fn two_process_record(web: &[u16], api: &[u16]) -> WorktreeRecord {
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record.ports.insert("web".to_string(), 17_342);
        record.ports.insert("api".to_string(), 17_343);
        record
            .roles
            .insert("web".to_string(), vec!["web".to_string()]);
        record
            .roles
            .insert("api".to_string(), vec!["api".to_string()]);
        record
            .processes
            .insert("web".to_string(), listening(101, web));
        record
            .processes
            .insert("api".to_string(), listening(102, api));
        let mut union: Vec<u16> = web.iter().chain(api).copied().collect();
        union.sort_unstable();
        union.dedup();
        record.observed_ports = union;
        record
    }

    // The documented behaviour — "what it is really listening on when that
    // is known" — was two identical match arms, so a framework that ignored
    // `PORT` and bound something else still had the assigned port printed
    // as its URL.
    #[test]
    fn the_url_prefers_a_port_the_process_is_really_listening_on() {
        assert_eq!(
            worktree_url(&one_process_record(&[17_342, 17_399])).as_deref(),
            Some("http://localhost:17342"),
            "the assigned port is among them, so it is the one"
        );
        assert_eq!(
            worktree_url(&one_process_record(&[3_000])).as_deref(),
            Some("http://localhost:3000"),
            "it ignored the port pando gave it; the URL follows the process"
        );
        assert_eq!(
            worktree_url(&one_process_record(&[])).as_deref(),
            Some("http://localhost:17342"),
            "nothing observed at all falls back to what was assigned"
        );
        // And nothing running at all: the port survives the stop, so the
        // URL the developer bookmarked is still the one they get.
        let mut record = one_process_record(&[3_000]);
        record.processes.clear();
        record.observed_ports.clear();
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:17342")
        );
    }

    // Phase 2b review, finding 6. One rule with three implementations:
    // `start` took the first role of the first process, `status` and `ls`
    // took the alphabetically first *role*, and the TUI took that and never
    // looked at what was really listening. Two processes whose role names
    // sort the other way round from their own names were all it took for
    // `pando start` and `pando status`, seconds apart, to hand out two
    // different URLs.
    #[test]
    fn the_url_is_the_first_role_of_the_first_process_when_nothing_owns_web() {
        let mut record = WorktreeRecord::new("/trees/feat+url2", true);
        record.ports.insert("srv".to_string(), 19_056);
        record.ports.insert("admin".to_string(), 19_057);
        record
            .roles
            .insert("alpha".to_string(), vec!["srv".to_string()]);
        record
            .roles
            .insert("beta".to_string(), vec!["admin".to_string()]);
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:19056"),
            "alpha comes first, so alpha's first role is the worktree's URL"
        );

        // `web` still wins wherever anything owns it, whatever it sorts
        // against.
        record.ports.insert("web".to_string(), 19_058);
        record
            .roles
            .insert("zeta".to_string(), vec!["web".to_string()]);
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:19058")
        );
    }

    // Phase 2b review, finding 4. `f6093df` narrowed "prefer an observed
    // port" to "prefer one no role claims", but the observed list was one
    // flat set per worktree, so a socket the *api* opened — an HMR socket,
    // `node --inspect`, a metrics port — was indistinguishable from one the
    // web process opened, and became the worktree's URL while the web
    // server was not serving at all.
    #[test]
    fn the_url_follows_a_listener_only_in_the_group_that_owns_the_role() {
        assert_eq!(
            worktree_url(&two_process_record(&[17_342], &[17_343])).as_deref(),
            Some("http://localhost:17342"),
            "both up, each on its own port"
        );

        assert_eq!(
            worktree_url(&two_process_record(&[17_342], &[9876, 17_343])).as_deref(),
            Some("http://localhost:17342"),
            "the api's second socket is the api's, whatever claims it"
        );

        // The review's reproduction: the web process stopped, the api kept
        // serving, and it holds a port no role claims.
        let mut record = two_process_record(&[], &[9876, 17_343]);
        record.processes.remove("web");
        assert_eq!(
            worktree_url(&record).as_deref(),
            Some("http://localhost:17342"),
            "the web role's own port, not whatever the api happens to hold"
        );

        // And a framework that ignored `PORT` is still followed, because
        // there it is the process that owns the role doing the ignoring.
        assert_eq!(
            worktree_url(&two_process_record(&[3000], &[17_343])).as_deref(),
            Some("http://localhost:3000"),
            "the web process itself bound 3000"
        );

        // A port another role already has is never a candidate either.
        assert_eq!(
            worktree_url(&two_process_record(&[17_343], &[17_343])).as_deref(),
            Some("http://localhost:17342")
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
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
        }
    }

    /// The services question of fixture 6: two ticked by a rule, two the
    /// rules could not place.
    fn services_question() -> actions::Question {
        actions::Question {
            slot: crate::detect::Slot::Services,
            prompt: "Run private copies of these services for each worktree?".to_string(),
            options: ["cache", "db", "mail", "queue"]
                .iter()
                .map(|v| (v.to_string(), "docker-compose.yml".to_string()))
                .collect(),
            preselect: Some(0),
            allow_custom: false,
            allow_none: true,
            multi: true,
            checked: vec![1, 2],
            details: Vec::new(),
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

    // ---- the multi-select question ---------------------------------------

    #[test]
    fn a_set_question_starts_from_what_the_rules_resolved() {
        let question = services_question();
        let (answer, printed) = answer_with(&question, &[""]);
        assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
        assert!(printed.contains("[ ] 1) cache"), "{printed}");
        assert!(printed.contains("[x] 2) db"), "{printed}");
        assert!(printed.contains("[x] 3) mail"), "{printed}");
        assert!(printed.contains("[ ] 4) queue"), "{printed}");
        assert!(printed.contains("accepts [db, mail]"), "{printed}");
    }

    #[test]
    fn a_number_toggles_one_option_and_enter_takes_the_rest() {
        let question = services_question();
        // Tick `cache`, untick `mail`, accept.
        let (answer, _) = answer_with(&question, &["1", "3", ""]);
        assert_eq!(answer.unwrap(), actions::Answer::Many(vec![0, 1]));
    }

    #[test]
    fn unticking_everything_is_the_answer_none() {
        let question = services_question();
        let (answer, _) = answer_with(&question, &["2", "3", ""]);
        assert_eq!(answer.unwrap(), actions::Answer::None);
        // And so is saying so outright.
        let (answer, _) = answer_with(&services_question(), &["n"]);
        assert_eq!(answer.unwrap(), actions::Answer::None);
    }

    #[test]
    fn a_number_out_of_range_reprints_the_range_and_ticks_nothing() {
        let question = services_question();
        let (answer, printed) = answer_with(&question, &["9", "not-a-number", ""]);
        assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
        assert_eq!(
            printed
                .matches("a number between 1 and 4 toggles one")
                .count(),
            2,
            "{printed}"
        );
    }

    // `Auto`, not `Many`: the resolver turns `Auto` into the ticked set
    // *and* into a comment saying a flag took it. A `Many` would be
    // written down as if a human had chosen, which is a config nobody can
    // review.
    #[test]
    fn yes_takes_the_ticked_set_as_an_auto_answer_not_a_choice() {
        let question = services_question();
        assert_eq!(asker(true)(&question).unwrap(), actions::Answer::Auto(0));
    }

    #[test]
    fn exit_three_shows_a_set_question_with_its_boxes() {
        let needs = actions::NeedsAnswer {
            question: services_question(),
        };
        let text = render_needs_answer(&needs);
        assert!(text.contains("[ ] 1) cache"), "{text}");
        assert!(text.contains("[x] 2) db"), "{text}");
        assert!(text.contains("[[services]]"), "{text}");
        assert!(
            text.contains("--yes to take the ticked ones"),
            "an agent has to be told what --yes would do: {text}"
        );
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
                observed_ports: Vec::new(),
                swept: false,
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
                observed_ports: Vec::new(),
                swept: false,
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

    /// A worktree with a live share recorded, so the status shapes can be
    /// asserted without a tunnel. Every pid is this process: alive, so the
    /// refresh leaves the record alone — the application it publishes
    /// included, because a share whose application is gone is closed.
    fn with_share(fx: &Fx, name: &str, proxy: Option<u16>) {
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        let record = store.worktrees.get_mut(name).unwrap();
        record.ports.insert("web".to_string(), 17_342);
        record
            .roles
            .insert("dev".to_string(), vec!["web".to_string()]);
        record
            .processes
            .insert("dev".to_string(), listening(std::process::id() as i32, &[]));
        record.share_port = proxy;
        record.share = Some(crate::state::ShareRecord {
            tunnel_pid: std::process::id(),
            tunnel_pgid: 999_998,
            public_url: "https://fake-host.trycloudflare.com".to_string(),
            local_port: 17_342,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(name, "tunnel"),
            proxy_pid: proxy.map(|_| std::process::id()),
            proxy_pgid: proxy.map(|_| 999_997),
            proxy_port: proxy,
        });
        crate::state::save(&fx.paths.state_file(), &store).unwrap();
    }

    #[test]
    fn status_json_carries_the_public_url_and_never_a_cookie() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_share(&fx, &name, Some(17_349));

        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let share = &v["worktrees"][0]["share"];
        assert_eq!(share["url"], "https://fake-host.trycloudflare.com");
        assert_eq!(share["local_port"], 17_342);
        assert_eq!(share["proxy_port"], 17_349);
        assert!(share["since"].is_string());
        assert!(
            !text.to_lowercase().contains("cookie"),
            "a credential must never reach a shape that gets piped into things:\n{text}"
        );
    }

    #[test]
    fn status_json_says_null_for_a_worktree_that_is_not_shared() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["share"], serde_json::Value::Null);
    }

    #[test]
    fn status_json_reports_a_share_with_no_proxy_in_front_of_it() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_share(&fx, &name, None);
        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["worktrees"][0]["share"]["proxy_port"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn status_text_prints_the_public_url_under_its_worktree() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_share(&fx, &name, Some(17_349));

        let text = capture(|b| status_text_at(&fx.paths, None, b, 120));
        assert!(text.contains("share"), "{text}");
        assert!(
            text.contains("https://fake-host.trycloudflare.com"),
            "{text}"
        );
        assert!(
            text.contains("through a proxy on 17349"),
            "the proxy is worth saying: a visitor arrives authenticated: {text}"
        );
    }

    // The same degradation every other row has: truncated, never wrapped.
    #[test]
    fn the_share_row_truncates_on_a_narrow_terminal() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_share(&fx, &name, Some(17_349));

        let text = capture(|b| status_text_at(&fx.paths, None, b, 40));
        for line in text.lines() {
            assert!(
                line.chars().count() <= 40,
                "a row wider than the terminal: {line:?}"
            );
        }
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

    /// A worktree running `web` and `api`, recorded as a refresh would
    /// leave it. `pid` is this test process, which really is alive, so the
    /// read path does not turn the phase into a failure underneath.
    fn with_two_processes(fx: &Fx, name: &str, api_phase: Phase) {
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        let record = store.worktrees.get_mut(name).unwrap();
        record.ports.insert("web".to_string(), 17_342);
        record.ports.insert("api".to_string(), 17_343);
        record.observed_ports = vec![17_342, 17_343];
        record.processes.insert(
            "web".to_string(),
            crate::state::ProcessRecord {
                pid: std::process::id(),
                pgid: 999_998,
                started_at: Utc::now(),
                log_path: fx.paths.log_file(name, "web"),
                ready_port: Some(17_342),
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                swept: false,
                phase: Phase::Running { since: Utc::now() },
            },
        );
        record.processes.insert(
            "api".to_string(),
            crate::state::ProcessRecord {
                pid: std::process::id(),
                pgid: 999_997,
                started_at: Utc::now(),
                log_path: fx.paths.log_file(name, "api"),
                ready_port: Some(17_343),
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                swept: false,
                phase: api_phase,
            },
        );
        crate::state::save(&fx.paths.state_file(), &store).unwrap();
    }

    // `the_url_follows_a_listener_only_when_no_other_role_owns_that_port`
    // lived here. It asserted that a port no role claims becomes the
    // worktree's URL, which is the bug finding 4 reproduces: that port
    // belongs to whichever group opened it.
    // `the_url_follows_a_listener_only_in_the_group_that_owns_the_role`
    // above is the rule it should have pinned.

    #[test]
    fn status_text_lists_every_process_under_its_worktree() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

        let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "one worktree line and two process lines:\n{text}"
        );
        assert!(lines[0].starts_with("feat+one"), "{text}");
        assert!(lines[0].contains("running"), "{text}");
        assert!(
            lines[0].contains("api 17343") && lines[0].contains("web 17342"),
            "the worktree line carries every role: {text}"
        );
        assert!(
            lines[0].contains("http://localhost:17342"),
            "and one URL, the web role's: {text}"
        );
        // Indented, in config order, each with its own pid.
        assert!(lines[1].starts_with("  api"), "{text}");
        assert!(lines[2].starts_with("  web"), "{text}");
        for line in &lines[1..] {
            assert!(
                line.contains(&format!("pid {}", std::process::id())),
                "{text}"
            );
            assert!(line.contains("running"), "{text}");
        }
    }

    // Phase 2b review, finding 9. `ls` sheds columns and the TUI detail
    // pane truncates; `status` had no width parameter at all, and the
    // per-process rows 2b added grow the block it prints. In the tmux split
    // the TUI is designed for, it wrapped.
    #[test]
    fn status_text_sheds_the_url_and_then_the_ports_as_the_terminal_narrows() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

        let wide = capture(|b| status_text_at(&fx.paths, None, b, 200));
        assert!(wide.contains("http://localhost:17342"), "{wide}");
        assert!(
            wide.contains("api 17343") && wide.contains("web 17342"),
            "{wide}"
        );

        for width in [60, 44, 32, 24, 12] {
            let text = capture(|b| status_text_at(&fx.paths, None, b, width));
            for line in text.lines() {
                assert!(
                    line.chars().count() <= width,
                    "{line:?} is wider than {width} columns:\n{text}"
                );
            }
            assert!(
                text.contains("feat+one"),
                "the name is the identifier and never goes: {text}"
            );
        }

        // The URL is the longest cell and `--json` still carries it, so it
        // is the first thing to go; the ports cell is truncated after that.
        let narrow = capture(|b| status_text_at(&fx.paths, None, b, 44));
        assert!(!narrow.contains("http://"), "{narrow}");
        assert!(narrow.contains("running"), "{narrow}");
    }

    #[test]
    fn status_text_says_which_process_failed() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        with_two_processes(
            &fx,
            &name,
            Phase::Failed {
                at: Utc::now(),
                reason: "process exited".to_string(),
            },
        );

        let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].contains("failed") && lines[0].contains("api: process exited"),
            "a worktree with a dead api is failed, and says which: {text}"
        );
        assert!(
            lines[1].contains("api") && lines[1].contains("failed"),
            "{text}"
        );
        assert!(
            lines[2].contains("web") && lines[2].contains("running"),
            "the process that is still up says so: {text}"
        );
    }

    #[test]
    fn the_listing_shows_the_aggregate_not_the_first_process() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        // `api` sorts first and is running; `web` is the failed one, so a
        // row that showed the first process would read "running".
        with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        store
            .worktrees
            .get_mut(&name)
            .unwrap()
            .processes
            .get_mut("web")
            .unwrap()
            .phase = Phase::Failed {
            at: Utc::now(),
            reason: "process exited".to_string(),
        };
        crate::state::save(&fx.paths.state_file(), &store).unwrap();

        let text = capture(|b| ls_text_at(&fx.paths, b, 200));
        assert!(
            text.contains("failed"),
            "the row is the worst of its processes: {text}"
        );
    }

    #[test]
    fn logs_read_the_source_they_are_asked_for() {
        let fx = fixture();
        write_log(&fx, "feat+one", "web", "web line\n");
        write_log(&fx, "feat+one", "api", "api line\n");
        let text = capture(|b| logs(&fx.paths, "feat+one", "api", 5, false, false, b));
        assert_eq!(text, "api line\n");

        let mut out = Vec::new();
        let err = logs(&fx.paths, "feat+one", "worker", 5, false, false, &mut out).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("worker"), "{msg}");
        assert!(
            msg.contains("api") && msg.contains("web"),
            "an unknown source lists the ones there are: {msg}"
        );
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

    // The read side of finding 1: `--source` is the other half of the same
    // path component, so a traversal there reads a file outside the
    // worktree's log directory — one `available_sources` never lists, so
    // nothing even suggests it is reachable.
    #[test]
    fn a_log_source_that_escapes_the_log_directory_is_refused() {
        let fx = fixture();
        write_log(&fx, "feat+one", "web", "web line\n");
        // A real file the traversal would reach, so the refusal is about
        // the name rather than about the file not being there.
        std::fs::create_dir_all(fx.paths.project_dir()).unwrap();
        std::fs::write(fx.paths.project_dir().join("outside.log"), "secret\n").unwrap();

        let mut out = Vec::new();
        let err = logs(
            &fx.paths,
            "feat+one",
            "../../outside",
            5,
            false,
            false,
            &mut out,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("\"../../outside\""), "{msg}");
        assert!(msg.contains("logs/<worktree>"), "{msg}");
        assert!(
            out.is_empty(),
            "nothing outside the log directory may be printed: {}",
            String::from_utf8_lossy(&out)
        );

        // And the hook logs pando writes itself are still readable by name.
        write_log(&fx, "feat+one", "install", "install line\n");
        let text = capture(|b| logs(&fx.paths, "feat+one", "install", 5, false, false, b));
        assert_eq!(text, "install line\n");
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

    // ---- an answers file -------------------------------------------------

    // The names in the file are the names `signals` publishes, because
    // they are the same function of the same type.
    #[test]
    fn every_question_has_one_name_that_round_trips() {
        for slot in actions::ALL_SLOTS {
            let name = slot_name(slot);
            assert!(!name.is_empty(), "{slot:?} has no name");
            assert_eq!(slot_named(&name), Some(slot), "{name} does not round trip");
        }
        assert_eq!(slot_names().len(), actions::ALL_SLOTS.len());
    }

    fn parse_err(json: &str) -> String {
        format!("{:#}", Answers::parse(json).unwrap_err())
    }

    #[test]
    fn a_name_pando_does_not_ask_about_is_a_usage_error_naming_it() {
        let err = parse_err(r#"{"dev_command": "pnpm dev"}"#);
        assert!(err.contains("dev_command"), "{err}");
        assert!(err.contains("dev_cmd"), "and what it does ask about: {err}");
        assert!(
            Answers::parse(r#"{"dev_command": "x"}"#)
                .unwrap_err()
                .downcast_ref::<UsageError>()
                .is_some(),
            "a name that is not a question is a usage error, not a failure"
        );
    }

    // Caught when the file is read, not when a question happens to reach
    // the slot: a shape this slot cannot take is knowable from the slot.
    #[test]
    fn a_shape_the_slot_cannot_take_is_refused_before_anything_is_written() {
        let err = parse_err(r#"{"install": 42}"#);
        assert!(err.contains("install"), "{err}");
        assert!(err.contains("string"), "{err}");

        let err = parse_err(r#"{"install": null}"#);
        assert!(err.contains("no \"none\" answer"), "{err}");

        let err = parse_err(r#"{"install": ["a", "b"]}"#);
        assert!(err.contains("not a list"), "{err}");

        let err = parse_err(r#"{"services": "db"}"#);
        assert!(err.contains("list of the options"), "{err}");

        let err = parse_err(r#"{"install": "   "}"#);
        assert!(err.contains("empty string"), "{err}");

        // And the shapes that are fine everywhere they are offered.
        assert!(Answers::parse(r#"{"port_env": null}"#).is_ok());
        assert!(Answers::parse(r#"{"services": []}"#).is_ok());
        assert!(Answers::parse(r#"{"provision": [".env"]}"#).is_ok());
        assert!(Answers::parse(r#"{"version_files": [".nvmrc"]}"#).is_ok());
    }

    #[test]
    fn a_file_that_is_not_a_json_object_is_a_usage_error() {
        assert!(parse_err("[1, 2]").contains("JSON object"));
        assert!(parse_err("{").contains("not JSON"));
    }

    // By value, never by index: the option carries the roles a command
    // owns and the process tables a workspace answer is, and a list of
    // indexes is a contract that breaks the day a rule finds one more
    // candidate.
    #[test]
    fn an_option_is_answered_by_its_own_text() {
        let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
        let answer = answer_from(&question, &serde_json::json!("pnpm dev:web")).unwrap();
        assert_eq!(
            answer,
            actions::Answer::Program(Box::new(actions::Answer::Choice(1)))
        );
    }

    // Every question has a custom answer, and a program gets the same one.
    #[test]
    fn a_value_no_option_has_is_a_command_of_your_own() {
        let question = dev_question(&["pnpm dev"]);
        let answer = answer_from(&question, &serde_json::json!("./serve.sh")).unwrap();
        assert_eq!(
            answer,
            actions::Answer::Program(Box::new(actions::Answer::Custom("./serve.sh".to_string())))
        );
    }

    // Except at the set question, where there is nothing to type: a
    // service the compose file does not declare is not one pando can run.
    #[test]
    fn a_set_answer_that_names_nothing_on_offer_says_what_is() {
        let question = services_question();
        let err = answer_from(&question, &serde_json::json!(["postgres"])).unwrap_err();
        let printed = format!("{err:#}");
        assert!(printed.contains("postgres"), "{printed}");
        assert!(printed.contains("cache, db, mail, queue"), "{printed}");
        assert!(err.downcast_ref::<UsageError>().is_some());
    }

    #[test]
    fn a_set_answer_is_the_options_it_names_and_an_empty_one_is_none_of_them() {
        let question = services_question();
        assert_eq!(
            answer_from(&question, &serde_json::json!(["db", "cache"])).unwrap(),
            actions::Answer::Program(Box::new(actions::Answer::Many(vec![1, 0])))
        );
        assert_eq!(
            answer_from(&question, &serde_json::json!([])).unwrap(),
            actions::Answer::Program(Box::new(actions::Answer::None))
        );
        assert_eq!(
            answer_from(&question, &serde_json::Value::Null).unwrap(),
            actions::Answer::Program(Box::new(actions::Answer::None))
        );
    }

    // A list slot takes a JSON array, joined into the one value the slot
    // writes — so a program never has to know the separator.
    #[test]
    fn a_list_slot_takes_an_array_and_joins_it_the_way_the_slot_splits_it() {
        let question = actions::Question {
            slot: crate::detect::Slot::Provision,
            prompt: crate::detect::Slot::Provision.prompt().to_string(),
            options: vec![(".env,.env.local".to_string(), "here".to_string())],
            preselect: Some(0),
            allow_custom: true,
            allow_none: true,
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
        };
        // The option's own text, reached without spelling the separator.
        assert_eq!(
            answer_from(&question, &serde_json::json!([".env", ".env.local"])).unwrap(),
            actions::Answer::Program(Box::new(actions::Answer::Choice(0)))
        );
        // And a list nothing offered is still an answer.
        assert_eq!(
            answer_from(&question, &serde_json::json!([".env", ".envrc"])).unwrap(),
            actions::Answer::Program(Box::new(actions::Answer::Custom(".env,.envrc".to_string())))
        );
    }

    // An answer nothing asked about is reported rather than dropped: a
    // program that answered a question pando did not ask has to hear it.
    #[test]
    fn the_answers_a_run_never_used_are_the_ones_nothing_asked_about() {
        let answers = Answers::parse(r#"{"install": "npm ci", "dev_cmd": "pnpm dev"}"#).unwrap();
        let question = dev_question(&["pnpm dev"]);
        assert!(answers.for_question(&question).is_some());
        assert_eq!(answers.unasked(), vec![crate::detect::Slot::Install]);
    }
}
