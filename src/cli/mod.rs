//! The clap front end. A thin wrapper: every behaviour lives in `actions`.

use crate::actions;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::share_proxy;
use crate::worktree::Worktree;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Write;

mod answers;
mod doctor;
mod logs;
mod ls;
mod prompt;
mod signals;
mod status;

use self::answers::init_asker;
use self::answers::read_answers;
use self::answers::render_init;
use self::answers::report_unused;
use self::answers::volunteered_from;
use self::prompt::asker;
pub use answers::{Answers, UsageError, render_needs_answer, slot_name};
pub use doctor::{adopt_project, doctor};
pub use logs::logs;
pub use ls::{Col, keep_columns, ls_json, ls_text, ls_text_at};
pub use signals::signals_json;
pub use status::{status_json, status_text, status_text_at};

/// Shape version for machine-readable output, bumped independently of the
/// crate version so agents can pin what they parse.
pub const JSON_VERSION: u32 = 2;

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
        /// The way back: stop this worktree's private services and use the
        /// project's shared ones. Its processes restart, so they see them.
        #[arg(long, conflicts_with = "isolated")]
        shared: bool,
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
    /// What the repository says about how to run itself, as JSON.
    ///
    /// Read-only, and identical on two runs: the input an agent reads
    /// before deciding anything.
    Signals,
    /// What pando found, from where, and what is wrong.
    ///
    /// Read-only. Exits 0 when nothing found will break a command and 1
    /// when something will — and everything it exits 1 for is printed
    /// above, with what to do about it.
    Doctor {
        /// Move a project folder whose repository has moved under this
        /// repository's current id, keeping its config, its state and its
        /// worktrees. The one thing doctor does rather than reports, and
        /// it asks first.
        #[arg(long, value_name = "OLD-ID")]
        adopt: Option<String>,
        /// Do not ask before adopting.
        #[arg(long, requires = "adopt")]
        yes: bool,
        /// The whole report as one JSON object, versioned like the other
        /// machine-readable shapes. The exit code is the same either way.
        #[arg(long, conflicts_with = "adopt")]
        json: bool,
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
            shared,
        } => {
            let mode = actions::Mode::of(isolated, shared);
            let config = &actions::resolve_process(paths, config, mode, &asker(yes), &notice)?;
            let report = actions::start(paths, config, &name, only.as_deref(), mode, &notice)?;
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
            // The second channel: what the file has to say about a slot no
            // rule proposed anything for, where there is no question to
            // put to anybody and a program is the only one who could know.
            let volunteered = volunteered_from(answers.as_ref());
            let answering = match &volunteered {
                Some(program) => actions::Answering::by_program(&ask, program),
                None => actions::Answering::asking(&ask),
            };
            let (report, preview) = match dry_run {
                true => actions::init_dry_run(paths, config, &answering, &notice)?,
                false => (
                    actions::init(paths, config, &answering, &notice)?,
                    Vec::new(),
                ),
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
        Command::Signals => signals_json(paths, config, &mut out),
        // Deliberately not given the config `main` loaded: the one thing
        // worth reporting about a project layer pando cannot read is the
        // error, and `main` keeps that to itself.
        Command::Doctor { adopt, yes, json } => match adopt {
            Some(old_id) => adopt_project(paths, &old_id, yes, &mut out),
            None => doctor(paths, json, &mut out),
        },
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
            // `--shared` is `start`'s: a restart into shared mode is what
            // `start --shared` already is, since changing the mode
            // restarts the processes anyway.
            let mode = actions::Mode::of(isolated, false);
            let config = &actions::resolve_process(paths, config, mode, &asker(yes), &notice)?;
            let report = actions::restart(paths, config, &name, only.as_deref(), mode, &notice)?;
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
        } => logs(paths, &name, &source, tail, follow, json, &mut out, &notice),
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
mod tests;
