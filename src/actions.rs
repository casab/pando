//! The orchestration layer. Everything user-facing — the CLI and the TUI —
//! calls into here; those two stay thin wrappers.
//!
//! Invariant 1 is enforced at this level: the only things written inside a
//! worktree are paths the project's own gitignore already ignores, checked
//! with `git check-ignore` before anything is created.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::config::{self, Config, ProcessConfig, ProvisionMode};
use crate::detect::{self, Slot};
use crate::hooks;
use crate::paths::PandoPaths;
use crate::ports;
use crate::process::{self as proc, SpawnOptions};
use crate::state::{self, Phase, ProcessRecord, WorktreeRecord};
use crate::template;
use crate::worktree::{self, Worktree};

/// Directory name for a branch: `feat/checkout` becomes `feat+checkout`.
/// Slashes are the only thing that cannot appear in a directory name, and a
/// plus reads as a join rather than an escape.
pub fn sanitize_branch_to_dir(branch: &str) -> String {
    branch.replace('/', "+")
}

/// Which of the three shapes `new` is in. Mirrors the decision git itself
/// would otherwise make implicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CreateSource {
    /// The branch already exists locally; check it out as-is.
    Local,
    /// The branch exists only on `origin`; track it.
    Remote,
    /// A new branch, forked from `base` and deliberately not tracking it.
    Fork { base: String },
}

/// Creates a worktree for `branch`, returning its directory name.
///
/// A rejected `new` leaves no directory, no branch, and no state behind.
/// Most refusals happen before git is asked to do anything; the ones that
/// cannot — the worktree's own gitignore has the last word on provisioning,
/// and it can only be read once the worktree exists — unwind what was
/// created and say so in the error.
pub fn new(
    paths: &PandoPaths,
    config: &Config,
    branch: &str,
    base: Option<&str>,
    progress: &dyn Fn(&str),
) -> Result<String> {
    let root = paths.root().to_path_buf();
    validate_branch_name(&root, branch)?;

    let dir_name = sanitize_branch_to_dir(branch);
    let existing = worktree::discover_all(&paths.project)?;
    if existing.main.name == dir_name {
        bail!("{dir_name:?} is the main checkout's directory name");
    }
    if let Some(found) = existing.worktrees.iter().find(|w| w.name == dir_name) {
        bail!(
            "a worktree named {dir_name:?} already exists at {}",
            found.path.display()
        );
    }

    // The cheap early refusal: a project that needs an untracked,
    // non-ignored file is refused with the path named, not fixed up. The
    // worktree gets asked again once it exists, because it may have a
    // different `.gitignore` checked out.
    for rel in &config.project.provision {
        ensure_gitignored(&root, rel)?;
    }

    let worktrees_dir = config.worktrees_dir(paths);
    let target = worktrees_dir.join(&dir_name);
    if target.exists() {
        bail!("{} already exists", target.display());
    }

    let source = resolve_create_source(&root, branch, base, config, progress)?;

    paths.ensure_home()?;
    // State is locked and read *before* git creates anything: a state file
    // pando cannot use has to refuse while there is still nothing to undo.
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // Under the lock, so the porcelain read cannot race a concurrent `new`
    // whose record is already saved but whose worktree this process has not
    // seen yet. Every record it may drop is signalled first.
    sweep_orphaned_groups(&store)?;
    drop_stale_worktree_records(&mut store, &root);

    std::fs::create_dir_all(&worktrees_dir)
        .with_context(|| format!("create {}", worktrees_dir.display()))?;

    let target_str = target.to_str().context("worktree path is not utf-8")?;
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&root).args(["worktree", "add"]);
    match &source {
        CreateSource::Local => {
            cmd.args([target_str, branch]);
        }
        CreateSource::Remote => {
            cmd.args([
                "--track",
                "-b",
                branch,
                target_str,
                &format!("origin/{branch}"),
            ]);
        }
        CreateSource::Fork { base } => {
            // --no-track, or the new branch's upstream becomes the base and a
            // later `git pull` merges the base into the feature branch.
            cmd.args(["--no-track", "-b", branch, target_str, base]);
        }
    }
    progress(&format!("checking out {branch}"));
    // Captured, not inherited: `git worktree add` narrates on stdout and
    // stderr, which would paint over the TUI's alternate screen.
    let out = cmd.output().context("spawn git worktree add")?;
    if !out.status.success() {
        bail!("git worktree add failed: {}", git_failure_reason(&out));
    }

    // Past this point the worktree exists, so every failure has something to
    // undo before it is reported.
    let finish = (|| -> Result<()> {
        progress("provisioning");
        provision_worktree_files(paths, config, &target)?;
        let canonical = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        store
            .worktrees
            .insert(dir_name.clone(), WorktreeRecord::new(canonical, true));
        state::save(&paths.state_file(), &store)
    })();
    if let Err(e) = finish {
        return Err(unwind_new(&root, &target, branch, &source, e));
    }
    // The lock goes before the install runs: `npm ci` takes minutes, and
    // holding the state lock through it would stall every `ls` and freeze
    // the TUI's tick.
    drop(_lock);

    // A failed install keeps the worktree. The branch is checked out, the
    // files are provisioned, and the next `start` tries the install again —
    // so the error is worth an exit code, but not an unwind.
    progress("installing");
    install_if_needed(paths, config, &dir_name, Some(branch), &target, progress)
        .with_context(|| format!("{dir_name} was created, but its install step failed"))?;
    Ok(dir_name)
}

/// The first thing `git worktree remove` would refuse over, when there is
/// one: a tracked file that differs, or an untracked file the project does
/// not ignore. `None` means git has no objection.
///
/// A prunable worktree has no directory left to look in, and clearing its
/// entry removes nothing, so there is nothing to refuse.
fn dirty_entry(worktree: &Worktree) -> Option<String> {
    if worktree.prunable {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(&worktree.path)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let entry = text.lines().find(|l| !l.trim().is_empty())?;
    Some(entry.trim().to_string())
}

/// The name of the built-in install hook, and of its log file.
pub const INSTALL_HOOK: &str = "install";

/// Runs `[project].install` when the lockfiles have changed since it last
/// ran here.
///
/// A built-in hook rather than a special case: the same fingerprint gate
/// `[[hooks]]` gets in the next phase, keyed on the lockfiles because a
/// lockfile changing is what "the dependencies changed" means.
///
/// Deliberately not under the state lock. It reads the recorded fingerprint
/// without one — the worst a race can do is run an idempotent install twice
/// — and takes the lock only to write the result.
pub fn install_if_needed(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    branch: Option<&str>,
    worktree: &Path,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let Some(install) = config.project.install.as_deref() else {
        return Ok(());
    };
    if install.trim().is_empty() {
        return Ok(());
    }
    let globs: Vec<String> = detect::LOCKFILES.iter().map(|l| l.to_string()).collect();
    let current = hooks::fingerprint(worktree, &globs);
    let recorded = state::load(&paths.state_file()).ok().and_then(|store| {
        store
            .worktrees
            .get(name)
            .and_then(|r| r.hooks.get(INSTALL_HOOK))
            .and_then(|h| h.fingerprint.clone())
    });
    // No fingerprint at all means nothing here can say the dependencies are
    // unchanged, so the hook runs every time.
    if current.is_some() && current == recorded {
        return Ok(());
    }

    let log_file = paths.log_file(name, INSTALL_HOOK);
    progress(&format!("{INSTALL_HOOK}: {install}"));
    let env = pando_env(paths, name, branch, worktree);
    hooks::run(&log_file, &with_prelude(config, install), worktree, &env)
        .with_context(|| format!("the {INSTALL_HOOK} hook failed"))?;

    // A frozen install should leave the lockfile exactly as it found it.
    // When one does not — and package managers do, on a lockfile they
    // consider malformed — two things have to happen: the developer is told,
    // because a tracked file changing under a worktree is what Invariant 1
    // exists to prevent; and the fingerprint recorded is the one the install
    // *left behind*, or the hook sees a change it made itself and re-runs on
    // every single start from then on.
    let after = hooks::fingerprint(worktree, &globs);
    if after != current {
        progress(&format!(
            "warning: {install:?} changed a lockfile in this worktree — that command is not as \
             frozen as it looks, and `git status` there will show it"
        ));
    }

    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // `or_insert_with`, not `get_mut`: on `start` for an adopted worktree
    // there is no record yet — `start` creates it after this returns — and
    // dropping the result on the floor would re-install on every start
    // forever. A record pando did not create is not claimed as its own.
    store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(worktree, false))
        .hooks
        .insert(
            INSTALL_HOOK.to_string(),
            state::HookRecord {
                fingerprint: after,
                ran_at: Utc::now(),
            },
        );
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

/// The variables every command pando runs gets, whether or not it has ports
/// yet. A hook has to be able to find out which worktree it is in.
fn pando_env(
    paths: &PandoPaths,
    name: &str,
    branch: Option<&str>,
    worktree: &Path,
) -> Vec<(String, String)> {
    vec![
        ("PANDO_NAME".to_string(), name.to_string()),
        // The git branch, the same string the dev process is given. A hook
        // doing `git checkout "$PANDO_BRANCH"` with the *directory* name
        // checks out the wrong thing, or nothing at all. A detached HEAD
        // has no branch, and then the directory name is all there is.
        (
            "PANDO_BRANCH".to_string(),
            branch.unwrap_or(name).to_string(),
        ),
        ("PANDO_WORKTREE".to_string(), worktree.display().to_string()),
        ("PANDO_ROOT".to_string(), paths.root().display().to_string()),
        ("PANDO_PROJECT".to_string(), paths.project_id().to_string()),
    ]
}

/// Undoes a `new` that failed after `git worktree add`. The worktree goes;
/// so does the branch, but only when pando created it in this same call —
/// a branch that existed before is the user's work, not pando's to delete.
///
/// The returned error is the original one plus what was actually undone, so
/// a partial unwind never reads as a clean one.
fn unwind_new(
    root: &Path,
    target: &Path,
    branch: &str,
    source: &CreateSource,
    err: anyhow::Error,
) -> anyhow::Error {
    let Some(target_str) = target.to_str() else {
        return err;
    };
    if !git_succeeds(root, &["worktree", "remove", "--force", target_str]) {
        return anyhow::anyhow!(
            "{err:#} — the partial worktree at {} could not be removed; remove it with \
             `git worktree remove --force` and delete the branch if it is new",
            target.display()
        );
    }
    if matches!(source, CreateSource::Local) {
        return anyhow::anyhow!("{err:#} — the partial worktree was removed");
    }
    if !git_succeeds(root, &["branch", "-D", branch]) {
        return anyhow::anyhow!(
            "{err:#} — the partial worktree was removed, but the new branch {branch} is still \
             there; delete it with `git branch -D {branch}`"
        );
    }
    anyhow::anyhow!("{err:#} — the partial worktree and the new branch {branch} were removed")
}

fn git_succeeds(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Removes a worktree, its logs, and its data directory. The branch is kept;
/// deleting it is a separate decision.
pub fn rm(paths: &PandoPaths, name: &str, yes: bool, force: bool) -> Result<()> {
    let discovery = worktree::discover_all(&paths.project)?;
    if discovery.main.name == name {
        bail!("{name:?} is the main checkout — pando never removes it");
    }
    let target = discovery
        .worktrees
        .iter()
        .find(|w| w.name == name)
        .with_context(|| format!("no worktree named {name:?}"))?;

    // Locked worktrees are always refused. pando never unlocks, and never
    // passes --force twice to talk git out of it.
    if target.locked {
        let reason = target
            .lock_reason
            .as_deref()
            .unwrap_or("no reason recorded");
        bail!("{name} is locked ({reason}) — unlock it with `git worktree unlock` first");
    }

    // `rm` can be the first command a project ever sees (an adopted
    // worktree), and taking the lock creates the project directory.
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // Before any record is dropped, whichever worktree it belongs to.
    sweep_orphaned_groups(&store)?;
    drop_stale_worktree_records(&mut store, paths.root());
    let created_by_pando = store
        .worktrees
        .get(name)
        .is_some_and(|r| r.created_by_pando && record_is_for(r, target));
    if !created_by_pando && !yes {
        bail!(
            "pando did not create {name} ({}) — pass --yes to remove it anyway",
            target.path.display()
        );
    }

    // git's own refusal is the last one, so it is asked first: stopping the
    // dev server and *then* being told the worktree stays leaves a process
    // that is gone, a worktree that is not, and a record blaming the
    // process for pando's kill. The question is the one git asks without
    // `--force`; ignored files never block a removal and do not show here.
    if !force && let Some(entry) = dirty_entry(target) {
        bail!(
            "{name} contains modified or untracked files ({entry}) — commit or remove them, \
             or pass --force to let git discard them"
        );
    }

    // Whatever is running goes next. A dev server whose working directory
    // has just been deleted is not a process anyone can do anything with,
    // and `rm` removes the record that is the only way to find it again.
    let stopped = stop_recorded(&mut store, name)?;

    // Nothing is unlinked first. Verified against git 2.51: an ignored file
    // does not block `git worktree remove`, and `--force` does not follow a
    // symlink out of the worktree — so unlinking bought nothing, and a
    // removal git then refused (a dirty tree without `--force`) left the
    // worktree alive without the `.env` pando had provisioned for it.
    //
    // Always run, even when the directory is already gone: the same command
    // clears a prunable entry, and only that one. `git worktree prune` is
    // global and would sweep entries pando has no business touching.
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(paths.root()).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(target.path.to_str().context("worktree path is not utf-8")?);
    let out = cmd.output().context("spawn git worktree remove")?;
    if !out.status.success() {
        // Anything that was running has already been stopped by now and
        // that cannot be taken back, so the cleared record is saved rather
        // than left behind to resurface as a phantom failure — and the
        // message says what actually happened.
        state::save(&paths.state_file(), &store)?;
        let reason = git_failure_reason(&out);
        if stopped == StopOutcome::Stopped {
            bail!(
                "git worktree remove failed: {reason} — the dev server was stopped; the \
                 worktree was kept"
            );
        }
        bail!("git worktree remove failed: {reason}");
    }

    let _ = std::fs::remove_dir_all(paths.logs_dir(name));
    let _ = std::fs::remove_dir_all(paths.data_dir(name));
    store.worktrees.remove(name);
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

// ---- questions ------------------------------------------------------------

/// Something pando needs to know and cannot work out on its own.
///
/// Asked at the moment the answer is needed, answered once, and written to
/// `pando.toml` so it is never asked again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub slot: Slot,
    pub prompt: String,
    /// Each option as its value and the signal that found it, in rule order.
    pub options: Vec<(String, String)>,
    /// The option the rules put first. `None` when they found nothing, in
    /// which case only a typed answer will do.
    pub preselect: Option<usize>,
    /// Whether a command typed by hand is acceptable. Always true in this
    /// phase: every slot accepts a shell command, so there is no dead end.
    pub allow_custom: bool,
    /// Whether "this process has none" is an answer. True for the port
    /// question: a worker or a watcher really has no port, and that has to
    /// be sayable, or the question comes back on every start.
    pub allow_none: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Choice(usize),
    /// The first option, taken because `--yes` was passed rather than
    /// because anyone chose it. Written down as exactly that: a config that
    /// claims a rule decided something a flag decided is a config nobody
    /// can review.
    Auto(usize),
    Custom(String),
    /// "This process has none of those." Only offered where a question has
    /// an empty answer that means something, which this phase is the port.
    None,
}

/// How a front end asks. The CLI prompts on a terminal and refuses
/// elsewhere; the TUI opens a modal; a test hands back a scripted answer.
pub type Ask<'a> = &'a dyn Fn(&Question) -> Result<Answer>;

/// The error every front end turns into exit code 3.
///
/// A question is not a failure, and an agent has to be able to tell them
/// apart without reading English.
#[derive(Debug, Clone)]
pub struct NeedsAnswer {
    pub question: Question,
}

impl std::fmt::Display for NeedsAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.question.prompt)
    }
}

impl std::error::Error for NeedsAnswer {}

/// The slots `new` fills: what to install, what pins the runtime, and which
/// local files a worktree needs a copy of.
pub const NEW_SLOTS: [Slot; 3] = [Slot::Install, Slot::VersionFiles, Slot::Provision];

/// The slots `start` fills: the dev process and how it takes its port.
pub const START_SLOTS: [Slot; 2] = [Slot::DevCmd, Slot::PortEnv];

/// Fills the dev process from detection when config has none.
pub fn resolve_process(
    paths: &PandoPaths,
    config: &Config,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    resolve(paths, config, &START_SLOTS, ask, progress)
}

/// Fills what `new` needs before it creates anything.
pub fn resolve_for_new(
    paths: &PandoPaths,
    config: &Config,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    resolve(paths, config, &NEW_SLOTS, ask, progress)
}

/// Detects, asks where it has to, and writes every answer to `pando.toml`.
///
/// Returns the config with the answers applied, so the caller does not have
/// to re-read the file it just wrote.
pub fn resolve(
    paths: &PandoPaths,
    config: &Config,
    slots: &[Slot],
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    let mut config = config.clone();
    if slots.iter().all(|slot| already_answered(*slot, &config)) {
        return Ok(config);
    }
    // A project whose config already declares its processes has answered
    // both process slots by declaring them — including a `[dev]` whose
    // ports the developer deliberately left out. Detection can only write
    // the `[dev]` shorthand, and `[dev]` beside `[processes]` is a file
    // pando's own loader refuses, so the slots are not resolved at all
    // rather than resolved into a file nothing can read afterwards.
    let declared_processes = !config.processes.is_empty();
    let signals = detect::signals(paths.root());
    let proposals = detect::propose(paths.root(), &signals);

    for slot in slots {
        if declared_processes && matches!(slot, Slot::DevCmd | Slot::PortEnv) {
            continue;
        }
        // Just in time, and once: a slot the developer has already filled
        // in, by hand or by answering before, is never asked about again.
        if already_answered(*slot, &config) || !detect::still_needed(*slot, &config) {
            continue;
        }
        let Some(proposal) = proposals.iter().find(|p| p.slot == *slot) else {
            continue;
        };
        let candidate = if proposal.decided {
            let candidate = proposal
                .preferred()
                .expect("a decided proposal has a candidate")
                .clone();
            // Every guess is visible: a one-line notice now, and a comment
            // in the file afterwards.
            progress(&format!(
                "using {:?} for {} (detected: {})",
                candidate.value,
                slot_label(*slot),
                candidate.why
            ));
            let why = candidate.why.clone();
            (candidate, config::Note::Detected(why))
        } else {
            let question = question_for(proposal);
            let offered = question.options.len();
            match ask(&question)? {
                Answer::Choice(index) => {
                    let candidate = pick(proposal, index)?;
                    let why = candidate.why.clone();
                    (candidate, config::Note::Detected(why))
                }
                Answer::Auto(index) => (pick(proposal, index)?, config::Note::TookFirst(offered)),
                Answer::Custom(value) => {
                    (detect::custom(*slot, value.trim()), config::Note::Answered)
                }
                // Written down as an empty list rather than left out: "this
                // process has no ports" and "nobody has said yet" have to
                // be different states, or the question returns on every
                // start and the answer is a port nothing will ever bind.
                Answer::None if *slot == Slot::PortEnv => {
                    let (table, key) = Slot::PortEnv.key();
                    config::set_detected(
                        paths,
                        table,
                        key,
                        toml_edit::Value::Array(toml_edit::Array::new()),
                        config::Note::Answered,
                    )?;
                    config
                        .processes
                        .entry(detect::DEV.to_string())
                        .or_default()
                        .ports = Some(config::PortsSpec::List(Vec::new()));
                    continue;
                }
                Answer::None => bail!("{} has no \"none\" answer", slot_label(*slot)),
            }
        };
        let (candidate, note) = candidate;
        if candidate.value.trim().is_empty() {
            bail!("an empty answer is not a {}", slot_label(*slot));
        }
        for (table, key, value) in detect::edits(*slot, &candidate) {
            config::set_detected(paths, table, key, value, note.clone())?;
        }
        detect::apply(*slot, &candidate, &mut config);
    }
    Ok(config)
}

/// The candidate an answer chose, by index.
fn pick(proposal: &detect::Proposal, index: usize) -> Result<detect::Candidate> {
    proposal
        .candidates
        .get(index)
        .with_context(|| format!("option {index} is not on offer"))
        .cloned()
}

fn question_for(proposal: &detect::Proposal) -> Question {
    Question {
        slot: proposal.slot,
        prompt: proposal.slot.prompt().to_string(),
        options: proposal
            .candidates
            .iter()
            .map(|c| (c.value.clone(), c.why.clone()))
            .collect(),
        preselect: (!proposal.candidates.is_empty()).then_some(0),
        allow_custom: true,
        allow_none: proposal.slot == Slot::PortEnv,
    }
}

fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Install => "install command",
        Slot::VersionFiles => "runtime version file",
        Slot::DevCmd => "dev command",
        Slot::PortEnv => "port variable",
        Slot::Provision => "provision list",
    }
}

/// Whether config already says what this slot needs, from any layer.
fn already_answered(slot: Slot, config: &Config) -> bool {
    match slot {
        Slot::Install => config.project.install.is_some(),
        Slot::VersionFiles => !config.runtime.version_files.is_empty(),
        Slot::Provision => !config.project.provision.is_empty(),
        Slot::DevCmd => config
            .processes
            .get(detect::DEV)
            .is_some_and(|p| !p.cmd.trim().is_empty()),
        Slot::PortEnv => !detect::still_needed(Slot::PortEnv, config),
    }
}

// ---- start, stop, restart -------------------------------------------------

/// How long a process group gets to exit on its own before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The role `share` and the browser-open key default to, and the role a
/// readiness rule watches when none is named.
const DEFAULT_READY_ROLE: &str = "web";

/// What `start` produced, and everything a caller needs to report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedProcess {
    pub worktree: String,
    /// The process's name in config — `dev` for the `[dev]` shorthand.
    pub process: String,
    pub ports: BTreeMap<String, u16>,
    /// The URL of the readiness role, when this process has one.
    pub url: Option<String>,
    pub record: ProcessRecord,
    /// Ports this worktree owned had been taken, so it moved. Worth saying
    /// out loud: a URL the developer had bookmarked just changed.
    pub reassigned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    Started(Box<StartedProcess>),
    AlreadyRunning(Box<StartedProcess>),
}

impl StartOutcome {
    pub fn process(&self) -> &StartedProcess {
        match self {
            StartOutcome::Started(p) | StartOutcome::AlreadyRunning(p) => p,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Stopped,
    /// Nothing was running. Not an error: `stop` is how you make sure.
    NotRunning,
}

/// Starts a worktree's dev process.
///
/// The shape is `new`'s: take the lock, decide, act, record, save. A process
/// that is already running is reported rather than started twice; a record
/// left over from one that died is signalled and cleared first, because a
/// dead leader does not mean a dead process group.
pub fn start(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    progress: &dyn Fn(&str),
) -> Result<StartOutcome> {
    let worktree = find_worktree(paths, name)?;
    let (process_name, process) = single_process(config)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());

    paths.ensure_home()?;
    // Before the lock, and before anything is spawned: a worktree whose
    // lockfiles changed under it — a rebase, a branch switch — gets its
    // dependencies brought up to date, and a long install does not hold the
    // lock the rest of pando needs.
    // A start that will only report what is already up must not install
    // first: `npm ci` inside a worktree whose dev server is live is a
    // surprise nobody asked for. Read without the lock, like the hook's own
    // fingerprint — the decision it guards is "can this step be skipped",
    // and the authoritative one is made under the lock below.
    if !already_running(paths, name, &process_name) {
        install_if_needed(
            paths,
            config,
            name,
            worktree.branch.as_deref(),
            &canonical,
            progress,
        )?;
    }

    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;

    // Before `reconcile` can drop them: a record whose leader is gone may
    // still have a live child holding the port. Dropping the record first
    // would leak that child and hand its port to someone else.
    if let Some(record) = store.worktrees.get(name) {
        if let Some(existing) = record.processes.get(&process_name) {
            let running = matches!(
                existing.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            ) && proc::is_alive(existing.pid);
            if running {
                let ports = record.ports.clone();
                return Ok(StartOutcome::AlreadyRunning(Box::new(StartedProcess {
                    worktree: name.to_string(),
                    process: process_name,
                    url: url_for(&ports, existing.ready_port),
                    ports,
                    record: existing.clone(),
                    reassigned: false,
                })));
            }
        }
        // *Every* group recorded for this worktree, not only the one being
        // started: a record under another name — state a newer pando wrote,
        // or a process the developer has since renamed in config — is about
        // to be dropped by `reconcile`, and a live child of it would be left
        // holding a port nothing could find again.
        let groups: Vec<i32> = record.processes.values().map(|p| p.pgid).collect();
        if !groups.is_empty() {
            progress("clearing what is left of the last run");
        }
        for pgid in groups {
            proc::stop(pgid, STOP_GRACE)?;
        }
    }
    // And every *other* worktree's dead-leader group, because `reconcile`
    // drops those records too.
    sweep_orphaned_groups(&store)?;
    state::reconcile(&mut store, proc::is_alive);

    // A record only vouches for the worktree it was written for; a stale one
    // at another path is replaced rather than inherited.
    let record = store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(canonical.clone(), false));
    if crate::paths::resolve_for_compare(&record.path)
        != crate::paths::resolve_for_compare(&canonical)
    {
        *record = WorktreeRecord::new(canonical.clone(), false);
    }
    // A sticky failure is for display; starting is the user acting on it.
    // Everything goes, because everything was just signalled.
    record.processes.clear();
    record.observed_ports.clear();

    let roles = process.roles();
    let assignment = ports::assign(paths, &mut store, name, &roles)?;
    let ready_role = ready_role(process, &roles)?;
    let ready_port = ready_role
        .as_deref()
        .and_then(|r| assignment.ports.get(r))
        .copied();

    let log_file = paths.log_file(name, &process_name);
    let ctx = template::Context {
        name,
        branch: worktree.branch.as_deref(),
        worktree: &canonical,
        root: paths.root(),
        project: paths.project_id(),
        ports: &assignment.ports,
        default_role: ready_role.as_deref(),
        log: Some(&log_file),
    };

    let cmd = template::render(&process.cmd, &ctx)
        .with_context(|| format!("in the command for process {process_name}"))?;
    let cwd = process_cwd(&canonical, &process_name, process, &ctx)?;
    let env = process_env(paths, name, &worktree, process, &ctx)?;
    let shell_cmd = with_prelude(config, &cmd);

    // Truncated, not appended: the classifier reads the tail of this file to
    // explain a failure, and the closing lines of the *previous* run would
    // be a confident wrong answer.
    reset_log(&log_file)?;
    progress(&format!("starting {process_name}"));
    let spawn = proc::spawn_detached(SpawnOptions {
        shell_cmd: &shell_cmd,
        cwd: &cwd,
        log_file: &log_file,
        env: &env,
    })?;

    let now = Utc::now();
    let record = ProcessRecord {
        pid: spawn.pid,
        pgid: spawn.pgid,
        started_at: now,
        log_path: log_file,
        ready_port,
        ready_timeout_s: process.ready.as_ref().and_then(|r| r.timeout_s),
        phase: Phase::Starting { since: now },
    };
    store
        .worktrees
        .get_mut(name)
        .expect("the record was just inserted")
        .processes
        .insert(process_name.clone(), record.clone());
    state::save(&paths.state_file(), &store)?;

    Ok(StartOutcome::Started(Box::new(StartedProcess {
        worktree: name.to_string(),
        process: process_name,
        url: url_for(&assignment.ports, ready_port),
        ports: assignment.ports,
        record,
        reassigned: assignment.reassigned,
    })))
}

/// Whether state already records a live process under this name. Best
/// effort and lock-free: every caller re-decides under the lock.
fn already_running(paths: &PandoPaths, name: &str, process: &str) -> bool {
    let Ok(store) = state::load(&paths.state_file()) else {
        return false;
    };
    store
        .worktrees
        .get(name)
        .and_then(|record| record.processes.get(process))
        .is_some_and(|p| {
            matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. })
                && proc::is_alive(p.pid)
        })
}

/// Stops everything running for a worktree. The worktree, its ports, and its
/// logs survive: only the processes go.
pub fn stop(paths: &PandoPaths, name: &str) -> Result<StopOutcome> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let outcome = stop_recorded(&mut store, name)?;
    // `reconcile` drops dead-leader records for every worktree in the
    // project, not only this one, so every one of them is signalled first.
    sweep_orphaned_groups(&store)?;
    state::reconcile(&mut store, proc::is_alive);
    state::save(&paths.state_file(), &store)?;
    Ok(outcome)
}

/// Stops every worktree pando has a process for, returning their names.
pub fn stop_all(paths: &PandoPaths) -> Result<Vec<String>> {
    stop_all_with(paths, |pgid| proc::stop(pgid, STOP_GRACE))
}

/// [`stop_all`] with the signal injected, so a test can drive the path
/// where a group refuses to die without needing one that really does.
pub fn stop_all_with(paths: &PandoPaths, stop: impl Fn(i32) -> Result<()>) -> Result<Vec<String>> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let names: Vec<String> = store
        .worktrees
        .iter()
        .filter(|(_, r)| !r.processes.is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    for name in names {
        // One worktree that will not die must not leave the rest running —
        // and must not lose its record either. The failures are collected
        // and reported once every other group has been signalled.
        match stop_recorded_with(&mut store, &name, &stop) {
            Ok(StopOutcome::Stopped) => stopped.push(name),
            Ok(StopOutcome::NotRunning) => {}
            Err(e) => failures.push(format!("stopping {name}: {e:#}")),
        }
    }
    // Nothing is dropped while a group is still unaccounted for: the pgid
    // in that record is the only way back to it.
    let mut sweep_failed = None;
    if failures.is_empty() {
        match sweep_orphaned_groups(&store) {
            Ok(()) => {
                state::reconcile(&mut store, proc::is_alive);
            }
            Err(e) => sweep_failed = Some(e),
        }
    }
    // Saved either way, so the groups that *were* signalled do not come
    // back as phantom records on the next read.
    state::save(&paths.state_file(), &store)?;
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    if let Some(e) = sweep_failed {
        return Err(e);
    }
    Ok(stopped)
}

/// Signals every process group recorded for `name` and drops the records.
/// The caller holds the lock and saves.
///
/// The signal is unconditional, Failed records included. pando is not the
/// process's parent by then, so "failed" only ever meant "its leader is
/// gone" — the group can still be serving.
fn stop_recorded(store: &mut state::State, name: &str) -> Result<StopOutcome> {
    stop_recorded_with(store, name, |pgid| proc::stop(pgid, STOP_GRACE))
}

fn stop_recorded_with(
    store: &mut state::State,
    name: &str,
    stop: impl Fn(i32) -> Result<()>,
) -> Result<StopOutcome> {
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(StopOutcome::NotRunning);
    };
    if record.processes.is_empty() {
        return Ok(StopOutcome::NotRunning);
    }
    let groups: Vec<(String, i32)> = record
        .processes
        .iter()
        .map(|(process, p)| (process.clone(), p.pgid))
        .collect();
    let mut failures = Vec::new();
    for (process, pgid) in groups {
        // Signal first, drop second. A record cleared for a group that was
        // never signalled is a process nothing can find again.
        match stop(pgid) {
            Ok(()) => {
                record.processes.remove(&process);
            }
            Err(e) => failures.push(format!("{process} (group {pgid}): {e:#}")),
        }
    }
    if record.processes.is_empty() {
        record.observed_ports.clear();
    }
    if !failures.is_empty() {
        bail!("{name}: {}", failures.join("; "));
    }
    Ok(StopOutcome::Stopped)
}

/// Signals every process group in the project whose leader is dead, so that
/// no record is ever dropped without being signalled first.
///
/// `reconcile` drops dead-leader records for *every* worktree in the state
/// file, while an action only signals the worktree it was asked about. That
/// seam is how a sibling worktree — one whose `bash -lc` exited while a
/// child it backgrounded still holds a port — loses its record and leaves a
/// process nothing can find again. So the sweep is global, and runs before
/// anything that drops records.
///
/// One group that will not die does not stop the sweep: the rest are still
/// signalled and the failures are reported together. A caller that gets an
/// error must not go on to drop records.
fn sweep_orphaned_groups(store: &state::State) -> Result<()> {
    let mut failures = Vec::new();
    for (name, record) in &store.worktrees {
        for (process, p) in &record.processes {
            if proc::is_alive(p.pid) {
                continue;
            }
            // Unconditional, like every other signal pando sends: a dead
            // leader is not a dead group. The caveat is pid wraparound — a
            // sticky Failed record keeps its pgid indefinitely, and after
            // enough pid churn that number can belong to an unrelated
            // session leader. Fixing that needs the group's start time
            // recorded and compared, which is a per-platform lookup; until
            // then the leak this prevents is by far the likelier harm.
            if let Err(e) = proc::stop(p.pgid, STOP_GRACE) {
                failures.push(format!("{name}/{process} (group {}): {e:#}", p.pgid));
            }
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    bail!(
        "could not signal {} process group(s) before dropping their records: {}",
        failures.len(),
        failures.join("; ")
    )
}

/// Stop, then start. The ports come back from the record `stop` left behind,
/// so a restart keeps the URL.
pub fn restart(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    progress: &dyn Fn(&str),
) -> Result<StartOutcome> {
    stop(paths, name)?;
    start(paths, config, name, progress)
}

/// The one process this phase starts.
///
/// The config already models several — state records one entry per process
/// from the start — but running more than one needs a log tab and a
/// readiness rule each, which is the next phase.
fn single_process(config: &Config) -> Result<(String, &ProcessConfig)> {
    let mut processes = config.processes.iter();
    let Some((name, process)) = processes.next() else {
        bail!("no processes configured; add [dev] to pando.toml");
    };
    if processes.next().is_some() {
        let names: Vec<&str> = config.processes.keys().map(String::as_str).collect();
        bail!(
            "this version starts one process per worktree, and pando.toml configures {}: {}",
            names.len(),
            names.join(", ")
        );
    }
    // `cmd` is optional so that a half-written process table does not take
    // every other command down with it; this is where it has to be there.
    if process.cmd.trim().is_empty() {
        let table = if name == detect::DEV {
            "[dev]".to_string()
        } else {
            format!("[processes.{name}]")
        };
        bail!("{table} in pando.toml has no cmd — add the command that starts this process");
    }
    Ok((name.clone(), process))
}

/// The role whose port has to bind before the process counts as running.
///
/// `web` when it owns one, because that is the role everything else defaults
/// to; otherwise its first role. A process with no ports has none, and is
/// running as soon as it is alive.
fn ready_role(process: &ProcessConfig, roles: &[String]) -> Result<Option<String>> {
    if let Some(named) = process.ready.as_ref().and_then(|r| r.role.as_deref()) {
        if !roles.iter().any(|r| r == named) {
            bail!(
                "ready.role = {named:?} names a role this process does not own — it owns {}",
                if roles.is_empty() {
                    "none".to_string()
                } else {
                    roles.join(", ")
                }
            );
        }
        return Ok(Some(named.to_string()));
    }
    Ok(roles
        .iter()
        .find(|r| r.as_str() == DEFAULT_READY_ROLE)
        .or_else(|| roles.first())
        .cloned())
}

fn url_for(ports: &BTreeMap<String, u16>, ready_port: Option<u16>) -> Option<String> {
    let port = ready_port.or_else(|| ports.get(DEFAULT_READY_ROLE).copied())?;
    Some(format!("http://localhost:{port}"))
}

/// The directory the process runs in: the worktree, or the subdirectory
/// config names. A monorepo app sets `cwd = "apps/web"`.
fn process_cwd(
    worktree: &Path,
    name: &str,
    process: &ProcessConfig,
    ctx: &template::Context<'_>,
) -> Result<PathBuf> {
    let Some(relative) = process.cwd.as_deref() else {
        return Ok(worktree.to_path_buf());
    };
    let rendered = template::render(relative, ctx).context("in cwd")?;
    let dir = worktree.join(&rendered);
    if !dir.is_dir() {
        bail!(
            "cwd {rendered:?} does not exist in this worktree ({}) — check the cwd of process \
             {name:?}",
            dir.display()
        );
    }
    // `config::validate` refuses the literal ways out — an absolute path, a
    // `..` — but a template renders at start time and a symlink resolves
    // later still, so the directory that will really be entered is compared
    // against the worktree that owns it. A process that ran outside its own
    // worktree would be writing into a repository, which Invariant 1
    // forbids.
    let resolved = crate::paths::resolve_for_compare(&dir);
    let owner = crate::paths::resolve_for_compare(worktree);
    if !resolved.starts_with(&owner) {
        bail!(
            "cwd {rendered:?} for process {name:?} resolves to {}, which is outside the worktree \
             ({})",
            resolved.display(),
            owner.display()
        );
    }
    Ok(dir)
}

/// The environment the process is started with: the ports the map form of
/// `ports` is sugar for, then whatever `env` sets, then pando's own
/// variables.
///
/// `PANDO_*` last and unconditional: a hook or a script needs to be able to
/// find out which worktree it is in, and config cannot be allowed to lie
/// about that.
fn process_env(
    paths: &PandoPaths,
    name: &str,
    worktree: &Worktree,
    process: &ProcessConfig,
    ctx: &template::Context<'_>,
) -> Result<Vec<(String, String)>> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (var, tmpl) in process.port_env() {
        env.insert(
            var.clone(),
            template::render(&tmpl, ctx).with_context(|| format!("in ports.{var}"))?,
        );
    }
    for (var, tmpl) in &process.env {
        env.insert(
            var.clone(),
            template::render(tmpl, ctx).with_context(|| format!("in env.{var}"))?,
        );
    }
    env.insert("PANDO_NAME".into(), name.to_string());
    env.insert(
        "PANDO_BRANCH".into(),
        worktree.branch.clone().unwrap_or_else(|| name.to_string()),
    );
    env.insert("PANDO_WORKTREE".into(), ctx.worktree.display().to_string());
    env.insert("PANDO_ROOT".into(), paths.root().display().to_string());
    env.insert("PANDO_PROJECT".into(), paths.project_id().to_string());
    Ok(env.into_iter().collect())
}

/// `[runtime].prelude`, when set, runs before every command pando starts, so
/// a version manager sourced from a login profile is in effect. The shell is
/// `bash -lc`, so `.bash_profile` is read and `.zshrc` is not.
pub fn with_prelude(config: &Config, cmd: &str) -> String {
    match config.runtime.prelude.as_deref().map(str::trim) {
        Some(prelude) if !prelude.is_empty() => format!("{prelude} && {cmd}"),
        _ => cmd.to_string(),
    }
}

/// Empties a log file before a run, creating its directory.
fn reset_log(log_file: &Path) -> Result<()> {
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    std::fs::write(log_file, b"").with_context(|| format!("truncate {}", log_file.display()))
}

/// The managed worktree called `name`, or an error naming what is there.
fn find_worktree(paths: &PandoPaths, name: &str) -> Result<Worktree> {
    let discovery = worktree::discover_all(&paths.project)?;
    if discovery.main.name == name {
        bail!("{name:?} is the main checkout — pando starts worktrees, not the repository itself");
    }
    discovery
        .worktrees
        .into_iter()
        .find(|w| w.name == name)
        .with_context(|| format!("no worktree named {name:?}"))
}

/// Refuses, once per run and before anything is written, a pando home or a
/// `worktrees_dir` that lies inside the repository or any worktree git knows
/// about.
///
/// `config::validate` already checks `worktrees_dir` against the repository
/// root, which is all it can do without git. This is the version that has
/// the porcelain list, so it also covers linked worktrees, and it covers the
/// home — which nothing validates, and which is where state, caches, logs
/// and every worktree pando creates would land.
pub fn guard_write_locations(paths: &PandoPaths, config: &Config) -> Result<()> {
    let discovery = worktree::discover_all(&paths.project)?;
    let worktrees: Vec<PathBuf> = discovery
        .worktrees
        .iter()
        .map(|w| w.path.clone())
        .chain(std::iter::once(discovery.main.path.clone()))
        .collect();
    crate::paths::ensure_outside_repository("pando home", &paths.home, paths.root(), &worktrees)?;
    crate::paths::ensure_outside_repository(
        "worktrees_dir",
        &config.worktrees_dir(paths),
        paths.root(),
        &worktrees,
    )
}

/// Every managed worktree, enriched with git metadata.
pub fn ls(paths: &PandoPaths) -> Result<Vec<Worktree>> {
    let mut worktrees = worktree::discover(&paths.project)?;
    worktree::enrich_from_git(&mut worktrees, paths.root()).ok();
    Ok(worktrees)
}

/// The absolute, canonical path of a worktree.
pub fn path(paths: &PandoPaths, name: &str) -> Result<PathBuf> {
    let discovery = worktree::discover_all(&paths.project)?;
    if discovery.main.name == name {
        return Ok(discovery.main.path);
    }
    discovery
        .worktrees
        .into_iter()
        .find(|w| w.name == name)
        .map(|w| w.path)
        .with_context(|| format!("no worktree named {name:?}"))
}

// ---- reading state --------------------------------------------------------

/// How many log lines the failure classifier reads.
const FAILURE_TAIL_LINES: usize = 40;

/// State as of right now: phases advanced, observed ports captured, and the
/// reason a dead process died written down.
///
/// This is the read path. It advances phases rather than reconciling,
/// because a crashed dev server has to stay visible as Failed until the
/// developer acts on it — `reconcile`, which drops dead records, would erase
/// exactly the thing worth showing. It never fails: a state file pando
/// cannot use becomes one warning line, the same one `rm` refuses with.
#[derive(Debug, Default, Clone)]
pub struct Refreshed {
    pub state: state::State,
    pub warning: Option<String>,
}

pub fn refresh(paths: &PandoPaths) -> Refreshed {
    // Nothing has ever been started here, so there is nothing to advance and
    // no reason for a read-only command to create a home.
    if !paths.state_file().exists() {
        return Refreshed::default();
    }
    if let Err(e) = paths.ensure_home() {
        return Refreshed {
            state: state::State::new(),
            warning: Some(format!("{e:#}")),
        };
    }
    let _lock = match state::lock(&paths.lock_file()) {
        Ok(lock) => lock,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
            };
        }
    };
    let mut store = match state::load(&paths.state_file()) {
        Ok(store) => store,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
            };
        }
    };

    let failed_before = failed_processes(&store);
    // One scan of every live group, used for both questions this pass
    // answers: whether a starting process has opened its port yet, and what
    // every group is really listening on.
    let scans = scan_groups(&store);
    let mut changed = state::advance_phases(&mut store, proc::is_alive, |pgid, port| {
        port_is_bound(&scans, pgid, port)
    });
    changed |= capture_observed_ports(&mut store, &scans);
    changed |= explain_new_failures(&mut store, &failed_before);
    if changed {
        // A read path that cannot write is still a read path: the phases are
        // right in memory either way, so a save that fails is not worth
        // failing the command the user actually ran.
        if let Err(e) = state::save(&paths.state_file(), &store) {
            return Refreshed {
                state: store,
                warning: Some(format!("{e:#}")),
            };
        }
    }
    Refreshed {
        state: store,
        warning: None,
    }
}

/// `(worktree, process)` pairs already in `Failed`, so a reason is explained
/// once — when it happens — rather than re-read from the log on every tick.
fn failed_processes(store: &state::State) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, record) in &store.worktrees {
        for (process, p) in &record.processes {
            if matches!(p.phase, Phase::Failed { .. }) {
                out.push((name.clone(), process.clone()));
            }
        }
    }
    out
}

/// The ports every live process group is listening on, scanned once.
///
/// `Starting` groups are scanned too, not only `Running` ones: the scan is
/// how a process *becomes* Running, and scanning only what is already
/// running is a deadlock. `None` against a pgid means the scan itself could
/// not run — no `lsof`, denied, or timed out — which is a different answer
/// from "listening on nothing".
fn scan_groups(store: &state::State) -> BTreeMap<i32, Option<Vec<u16>>> {
    let mut scans: BTreeMap<i32, Option<Vec<u16>>> = BTreeMap::new();
    for record in store.worktrees.values() {
        for p in record.processes.values() {
            if !matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }) {
                continue;
            }
            scans
                .entry(p.pgid)
                .or_insert_with(|| crate::observe::observed_ports_checked(p.pgid));
        }
    }
    scans
}

/// Whether the process group has opened `port` yet.
///
/// Never by binding it: a probe that takes the port to find out whether it
/// is taken is one an `EADDRINUSE` away from killing the very server it is
/// waiting for, and it answers about the port rather than about *this*
/// process. The scan of the group's own sockets answers both properly; a
/// connection is the fallback for a machine where the scan cannot run.
fn port_is_bound(scans: &BTreeMap<i32, Option<Vec<u16>>>, pgid: i32, port: u16) -> bool {
    match scans.get(&pgid) {
        Some(Some(ports)) => ports.contains(&port),
        _ => ports::something_is_listening(port),
    }
}

/// Records the ports each live group is really listening on.
///
/// Configured ports are what pando asked for; these are what happened. A
/// framework that ignores `PORT`, or one that opens a second socket for hot
/// reload, shows up here and nowhere else.
fn capture_observed_ports(
    store: &mut state::State,
    scans: &BTreeMap<i32, Option<Vec<u16>>>,
) -> bool {
    let mut changed = false;
    for record in store.worktrees.values_mut() {
        let groups: Vec<i32> = record
            .processes
            .values()
            .filter(|p| matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }))
            .map(|p| p.pgid)
            .collect();
        let mut observed = Vec::new();
        let mut scanned = false;
        for pgid in &groups {
            if let Some(Some(ports)) = scans.get(pgid) {
                scanned = true;
                observed.extend(ports.iter().copied());
            }
        }
        observed.sort_unstable();
        observed.dedup();
        // A scan that could not run says nothing at all, so the last good
        // answer stands rather than being cleared by a missing `lsof`.
        if !groups.is_empty() && !scanned {
            continue;
        }
        if record.observed_ports != observed {
            record.observed_ports = observed;
            changed = true;
        }
    }
    changed
}

/// Appends the classifier's one-line hint to a failure that just happened.
///
/// pando is not the process's parent by the time it dies, so there is no
/// exit status to read. The last lines of its log are the only evidence, and
/// four patterns cover most of what actually goes wrong.
fn explain_new_failures(store: &mut state::State, failed_before: &[(String, String)]) -> bool {
    let mut changed = false;
    for (name, record) in store.worktrees.iter_mut() {
        for (process, p) in record.processes.iter_mut() {
            let Phase::Failed { at, reason } = &p.phase else {
                continue;
            };
            if failed_before
                .iter()
                .any(|(w, pr)| w == name && pr == process)
            {
                continue;
            }
            let lines =
                crate::log_tail::snapshot(&p.log_path, FAILURE_TAIL_LINES).unwrap_or_default();
            let Some(hint) = crate::observe::classify_failure(&lines) else {
                continue;
            };
            p.phase = Phase::Failed {
                at: *at,
                reason: format!("{reason} — {}", hint.hint),
            };
            changed = true;
        }
    }
    changed
}

/// Which worktrees pando created, from a state it has already read.
pub fn ownership(
    store: &state::State,
    worktrees: &[Worktree],
) -> std::collections::BTreeMap<String, bool> {
    store
        .worktrees
        .iter()
        .map(|(name, record)| {
            let ours = record.created_by_pando
                && worktrees
                    .iter()
                    .any(|w| &w.name == name && record_is_for(record, w));
            (name.clone(), ours)
        })
        .collect()
}

/// Which worktrees pando created, and anything that stopped the answer
/// being certain.
#[derive(Debug, Default, Clone)]
pub struct Ownership {
    pub by_name: std::collections::BTreeMap<String, bool>,
    /// The one-line reason the map may be empty or wrong — the same message
    /// `rm` refuses with, so a listing never says "adopted" about a state
    /// file `rm` would not touch.
    pub warning: Option<String>,
}

/// Which worktrees pando created, from state. A read path, so it goes
/// through [`refresh`]: phases are advanced and a crashed process stays
/// visible as Failed.
///
/// `worktrees` is what git currently reports: a record only vouches for a
/// worktree at the same path it was written for, so a record left behind by
/// a worktree removed outside pando cannot adopt a later namesake.
pub fn created_by_pando(paths: &PandoPaths, worktrees: &[Worktree]) -> Ownership {
    let refreshed = refresh(paths);
    Ownership {
        by_name: ownership(&refreshed.state, worktrees),
        warning: refreshed.warning,
    }
}

/// Whether a state record is really about this worktree. Keying on the
/// directory basename alone is not enough: the record a worktree removed
/// outside pando leaves behind would otherwise vouch for any later worktree
/// of the same name, anywhere on disk.
fn record_is_for(record: &WorktreeRecord, wt: &Worktree) -> bool {
    // A prunable entry's directory is gone, so `Worktree::path` is whatever
    // git recorded rather than a canonical path. Comparing it would start
    // demanding `--yes` for pando's own prunable worktrees, so the name is
    // trusted for those — clearing a prunable entry removes no directory.
    wt.prunable
        || crate::paths::resolve_for_compare(&record.path)
            == crate::paths::resolve_for_compare(&wt.path)
}

/// Drops records for worktrees git no longer lists. Callers hold the flock;
/// best effort, because a porcelain that cannot be read is not a reason to
/// refuse the command that is running.
fn drop_stale_worktree_records(store: &mut state::State, root: &Path) {
    let Ok(live) = worktree::porcelain_paths(root) else {
        return;
    };
    store
        .worktrees
        .retain(|_, record| live.contains(&crate::paths::resolve_for_compare(&record.path)));
}

fn resolve_create_source(
    root: &Path,
    branch: &str,
    base: Option<&str>,
    config: &Config,
    progress: &dyn Fn(&str),
) -> Result<CreateSource> {
    if ref_exists(root, &format!("refs/heads/{branch}")) {
        return Ok(CreateSource::Local);
    }
    if has_origin(root) {
        if ref_exists(root, &format!("refs/remotes/origin/{branch}")) {
            return Ok(CreateSource::Remote);
        }
        // The branch may exist on the remote but not be fetched yet — a PR
        // opened since the last fetch. A failure here just means it does not
        // exist there either, so this falls through to a new branch.
        progress(&format!("looking for origin/{branch}"));
        let fetched = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["fetch", "--quiet", "origin", branch])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if fetched && ref_exists(root, &format!("refs/remotes/origin/{branch}")) {
            return Ok(CreateSource::Remote);
        }
    } else if has_any_remote(root) {
        // Named explicitly rather than guessed at: pando only knows how to
        // look a branch up on a remote called `origin`.
        progress("no remote named origin; treating this as a new branch");
    }

    let requested = base
        .map(str::to_string)
        .or_else(|| config.base_for_branch(branch).map(str::to_string));
    let base = match requested {
        Some(b) => {
            let resolved = resolve_create_base(root, &b);
            if !ref_exists(root, &resolved) {
                bail!("base {b:?} does not exist in this repository");
            }
            resolved
        }
        None => worktree::resolve_base_branch(root).context(
            "cannot work out a base branch (no origin/HEAD, main, or master) — pass --base",
        )?,
    };
    Ok(CreateSource::Fork { base })
}

/// A bare base name would fork from the possibly stale local branch, so a
/// worktree created weeks after the last fetch would silently miss
/// everything merged since. Anything already qualified is used untouched.
fn resolve_create_base(root: &Path, base: &str) -> String {
    if !base.contains('/') && ref_exists(root, &format!("refs/remotes/origin/{base}")) {
        return format!("origin/{base}");
    }
    base.to_string()
}

fn validate_branch_name(root: &Path, branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("a branch name is required");
    }
    let ok = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ref-format", "--branch", branch])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        bail!("{branch:?} is not a valid branch name");
    }
    Ok(())
}

/// Exit 0 means ignored, 1 means not ignored (including a tracked file),
/// 128 is a git error worth surfacing. `dir` is whichever checkout has the
/// last word: the main one for the pre-flight, the new worktree for the
/// check that actually authorises a write.
fn ensure_gitignored(dir: &Path, rel: &str) -> Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", "--", rel])
        .output()
        .context("spawn git check-ignore")?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!(
            "provision path {rel:?} is not ignored in {} — pando only creates files your project \
             already ignores. Add it to .gitignore, or drop it from provision.",
            dir.display()
        ),
        Some(128) => bail!(
            "git check-ignore failed for {rel:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        other => bail!("git check-ignore exited with {other:?} for {rel:?}"),
    }
}

/// Links (or copies) the configured files into a new worktree.
///
/// The caller proved every path gitignored in the main checkout, but this
/// worktree has a different commit checked out and can have a different
/// `.gitignore` — an older branch, or an uncommitted edit the pre-flight
/// read. Invariant 1 is about the worktree the file lands in, so the
/// authorising `check-ignore` is re-run there, immediately before each
/// write.
fn provision_worktree_files(paths: &PandoPaths, config: &Config, worktree: &Path) -> Result<()> {
    for rel in &config.project.provision {
        let src = paths.root().join(rel);
        let dst = worktree.join(rel);
        if !src.exists() || dst.exists() {
            continue;
        }
        ensure_gitignored(worktree, rel)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        match config.project.provision_mode {
            ProvisionMode::Link => std::os::unix::fs::symlink(&src, &dst)
                .with_context(|| format!("symlink {} → {}", src.display(), dst.display()))?,
            ProvisionMode::Copy => {
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
            }
        }
    }
    Ok(())
}

fn ref_exists(root: &Path, refname: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_origin(root: &Path) -> bool {
    remotes(root).iter().any(|r| r == "origin")
}

fn has_any_remote(root: &Path) -> bool {
    !remotes(root).is_empty()
}

fn remotes(root: &Path) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("remote")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The last non-empty stderr line: git narrates before it fails, so the
/// closing line is the reason and everything above it is progress noise.
fn git_failure_reason(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(line) => line.trim().trim_start_matches("fatal: ").to_string(),
        None => format!("exit {}", out.status.code().unwrap_or(-1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::testutil::git;
    use tempfile::{TempDir, tempdir};

    struct Fx {
        _dir: TempDir,
        root: PathBuf,
        paths: PandoPaths,
        config: Config,
    }

    impl Fx {
        fn worktrees_dir(&self) -> PathBuf {
            self.config.worktrees_dir(&self.paths)
        }

        fn names(&self) -> Vec<String> {
            worktree::discover(&self.paths.project)
                .unwrap()
                .into_iter()
                .map(|w| w.name)
                .collect()
        }

        fn state(&self) -> state::State {
            state::load(&self.paths.state_file()).unwrap()
        }
    }

    fn noop(_: &str) {}

    // ---- start, stop, restart helpers ------------------------------------

    use crate::config::{PortsSpec, ReadySpec};
    use crate::testutil::{Detached, python_listener, python3_available, wait_until};
    use std::time::Duration;

    /// A dev process with one `web` role exposed as `PORT`, the shape almost
    /// every JavaScript project has.
    fn dev(cmd: &str) -> ProcessConfig {
        ProcessConfig {
            cmd: cmd.to_string(),
            ports: Some(PortsSpec::Map(BTreeMap::from([(
                "PORT".to_string(),
                "web".to_string(),
            )]))),
            ..Default::default()
        }
    }

    fn with_dev(fx: &mut Fx, process: ProcessConfig) {
        fx.config.processes.insert("dev".to_string(), process);
    }

    /// Stops whatever a test started even when an assertion panics first. No
    /// test in this crate may leave a process behind.
    fn guard(outcome: &StartOutcome) -> Detached {
        let p = outcome.process();
        Detached {
            pid: p.record.pid,
            pgid: p.record.pgid,
        }
    }

    fn log_of(fx: &Fx, name: &str) -> String {
        std::fs::read_to_string(fx.paths.log_file(name, "dev")).unwrap_or_default()
    }

    /// Creates a worktree and returns its directory name.
    fn worktree_named(fx: &Fx, branch: &str) -> String {
        new(&fx.paths, &fx.config, branch, None, &noop).unwrap()
    }

    // ---- start -----------------------------------------------------------

    #[test]
    fn start_spawns_the_dev_process_and_records_it() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let started = outcome.process();
        assert!(matches!(outcome, StartOutcome::Started(_)));
        assert_eq!(started.worktree, name);
        assert_eq!(started.process, "dev");
        assert_eq!(started.ports.len(), 1, "one role, one port");
        let port = started.ports["web"];
        assert_eq!(
            started.url.as_deref(),
            Some(&*format!("http://localhost:{port}"))
        );
        assert!(!started.reassigned);

        let record = &fx.state().worktrees[&name].processes["dev"];
        assert_eq!(record.ready_port, Some(port));
        assert!(matches!(record.phase, Phase::Starting { .. }));
        assert!(crate::process::is_alive(record.pid));
        assert_eq!(
            record.log_path,
            fx.paths.log_file(&name, "dev"),
            "the log lives under pando's home, one file per source"
        );
        assert_eq!(
            fx.state().worktrees[&name].ports["web"],
            port,
            "the port is recorded, so a stopped worktree keeps it"
        );
    }

    #[test]
    fn start_runs_in_the_worktree_with_the_ports_and_pando_variables_in_the_environment() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("pwd && env | sort && sleep 30"));
        fx.config.runtime.prelude = Some("echo prelude-ran".to_string());
        let name = worktree_named(&fx, "feat/one");

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];
        let worktree = fx.worktrees_dir().join(&name).canonicalize().unwrap();

        assert!(
            wait_until(Duration::from_secs(10), || log_of(&fx, &name)
                .contains("PANDO_PROJECT")),
            "the environment never reached the log: {:?}",
            log_of(&fx, &name)
        );
        let log = log_of(&fx, &name);
        assert!(
            log.contains("prelude-ran"),
            "the prelude did not run: {log}"
        );
        assert!(
            log.contains(&worktree.display().to_string()),
            "the process must run in its worktree: {log}"
        );
        for expected in [
            format!("PORT={port}"),
            format!("PANDO_NAME={name}"),
            "PANDO_BRANCH=feat/one".to_string(),
            format!("PANDO_WORKTREE={}", worktree.display()),
            format!("PANDO_ROOT={}", fx.root.display()),
            format!("PANDO_PROJECT={}", fx.paths.project_id()),
        ] {
            assert!(log.contains(&expected), "missing {expected} in:\n{log}");
        }
    }

    #[test]
    fn start_renders_the_port_into_the_command_for_a_positional_framework() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "echo serving on 127.0.0.1:{port:web} && sleep 30".to_string(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];
        assert!(
            wait_until(Duration::from_secs(10), || log_of(&fx, &name)
                .contains(&format!("127.0.0.1:{port}"))),
            "{:?}",
            log_of(&fx, &name)
        );
    }

    #[test]
    fn start_runs_in_the_configured_subdirectory() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "pwd && sleep 30".to_string(),
                cwd: Some("apps/web".to_string()),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let worktree = fx.worktrees_dir().join(&name);
        std::fs::create_dir_all(worktree.join("apps/web")).unwrap();

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(
            wait_until(Duration::from_secs(10), || log_of(&fx, &name)
                .contains("apps/web")),
            "{:?}",
            log_of(&fx, &name)
        );
    }

    #[test]
    fn a_cwd_that_does_not_exist_is_refused_by_name() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                cwd: Some("apps/nope".to_string()),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let err = start(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("apps/nope"), "{msg}");
        assert!(
            fx.state().worktrees[&name].processes.is_empty(),
            "a refused start records nothing"
        );
    }

    // `config::validate` refuses a literal `..` or an absolute path, but a
    // symlink resolves only when the process is about to be started, and a
    // process running outside its worktree writes into a repository.
    #[test]
    fn a_cwd_that_is_a_symlink_out_of_the_worktree_is_refused() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                cwd: Some("escape".to_string()),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let worktree = fx.worktrees_dir().join(&name);
        let outside = fx.root.parent().expect("a parent").join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, worktree.join("escape")).unwrap();

        let err = start(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("outside the worktree"), "{msg}");
        assert!(msg.contains("\"dev\""), "the process is named: {msg}");
        assert!(
            fx.state().worktrees[&name].processes.is_empty(),
            "a refused start records nothing"
        );
    }

    #[test]
    fn a_project_with_no_processes_says_what_to_add() {
        let fx = fixture();
        let name = worktree_named(&fx, "feat/one");
        let err = start(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "no processes configured; add [dev] to pando.toml"
        );
    }

    #[test]
    fn starting_a_worktree_that_does_not_exist_says_so() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let err = start(&fx.paths, &fx.config, "nope", &noop).unwrap_err();
        assert!(format!("{err:#}").contains("no worktree named"));
        let err = start(&fx.paths, &fx.config, "acme-shop", &noop).unwrap_err();
        assert!(format!("{err:#}").contains("main checkout"));
    }

    #[test]
    fn a_second_start_while_running_reports_the_process_that_is_already_up() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");

        let first = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&first);
        let second = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        assert!(
            matches!(second, StartOutcome::AlreadyRunning(_)),
            "a running process is reported, not started twice"
        );
        assert_eq!(second.process().record.pid, first.process().record.pid);
        assert_eq!(second.process().ports, first.process().ports);
    }

    // A failure is sticky for display; starting is the user acting on it.
    #[test]
    fn start_clears_a_failed_record_and_starts_fresh() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let first_pid = first.process().record.pid;
        let first_ports = first.process().ports.clone();
        drop(guard(&first));

        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(first_pid)
        }));
        // Mark it Failed the way a read path would.
        let mut store = fx.state();
        store
            .worktrees
            .get_mut(&name)
            .unwrap()
            .processes
            .get_mut("dev")
            .unwrap()
            .phase = Phase::Failed {
            at: Utc::now(),
            reason: "process exited".into(),
        };
        state::save(&fx.paths.state_file(), &store).unwrap();

        let second = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&second);
        assert!(matches!(second, StartOutcome::Started(_)));
        assert_ne!(second.process().record.pid, first_pid);
        assert_eq!(
            second.process().ports,
            first_ports,
            "ports are stable across a failure"
        );
        assert!(matches!(
            fx.state().worktrees[&name].processes["dev"].phase,
            Phase::Starting { .. }
        ));
    }

    #[test]
    fn a_process_with_no_ports_gets_no_ready_port() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(outcome.process().ports.is_empty());
        assert_eq!(outcome.process().record.ready_port, None);
        assert_eq!(outcome.process().url, None);

        // Which is what makes it Running as soon as it is alive: nothing
        // is watching a port, so no probe can have an opinion.
        let mut store = fx.state();
        assert!(state::advance_phases(
            &mut store,
            crate::process::is_alive,
            |_, _| false
        ));
        assert!(matches!(
            store.worktrees[&name].processes["dev"].phase,
            Phase::Running { .. }
        ));
    }

    #[test]
    fn a_ready_role_the_process_does_not_own_is_refused() {
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ready: Some(ReadySpec {
                    role: Some("api".to_string()),
                    timeout_s: None,
                }),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let err = start(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("api") && msg.contains("web"), "{msg}");
    }

    #[test]
    fn the_readiness_port_is_the_one_a_listener_binds() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: python_listener_template(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];

        assert!(
            wait_until(Duration::from_secs(20), || !crate::ports::is_port_free(
                port
            )),
            "the listener never bound {port}: {:?}",
            log_of(&fx, &name)
        );
        let mut store = fx.state();
        // Through the readiness probe the read path really uses: the
        // group's own listening sockets, never a bind of the port.
        let scans = scan_groups(&store);
        assert!(state::advance_phases(
            &mut store,
            crate::process::is_alive,
            |pgid, port| port_is_bound(&scans, pgid, port)
        ));
        assert!(
            matches!(
                store.worktrees[&name].processes["dev"].phase,
                Phase::Running { .. }
            ),
            "a bound ready port is what running means"
        );
    }

    /// The listener, with its port coming from a template rather than the
    /// environment — the positional shape Django and friends use.
    fn python_listener_template() -> String {
        python_listener(0).replace("',0)", "',{port:web})")
    }

    fn python_listener_v6_template() -> String {
        crate::testutil::python_listener_v6(0).replace("',0)", "',{port:web})")
    }

    // A server on `[::1]` leaves both IPv4 addresses bindable, so a probe
    // that decides readiness by binding says "not up" forever — and the
    // observed-port scan never ran for a process that had not reached
    // Running, so the two mechanisms deadlocked each other.
    #[test]
    fn a_dev_server_on_ipv6_loopback_alone_still_becomes_running() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        if !crate::testutil::ipv6_loopback_available() {
            eprintln!("skipping: no IPv6 loopback on this machine");
            return;
        }
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: python_listener_v6_template(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ready: Some(ReadySpec {
                    role: None,
                    timeout_s: Some(20),
                }),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];

        assert!(
            wait_until(Duration::from_secs(20), || matches!(
                refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
                Phase::Running { .. }
            )),
            "a server that is serving must not read as failed: {:?}",
            log_of(&fx, &name)
        );
        assert!(
            refresh(&fx.paths).state.worktrees[&name]
                .observed_ports
                .contains(&port),
            "and the port it really bound is recorded"
        );
    }

    // Readiness used to be answered by binding the port: "free" meant not
    // up yet. That says nothing about *which* process is listening, so an
    // unrelated squatter on the port made a dev server that had not even
    // opened a socket read as running.
    #[test]
    fn a_port_something_else_holds_does_not_make_this_process_ready() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 300"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];

        let squatter = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        let refreshed = refresh(&fx.paths);
        assert!(
            matches!(
                refreshed.state.worktrees[&name].processes["dev"].phase,
                Phase::Starting { .. }
            ),
            "readiness is about this group's own sockets, not about the port"
        );
        drop(squatter);
    }

    /// A "dev server" that tries to bind the IPv4 wildcard over and over and
    /// counts how often it was refused — the class Django's
    /// `runserver 0.0.0.0:P`, `vite --host 0.0.0.0` and Rails' `-b 0.0.0.0`
    /// all belong to. A probe holding `0.0.0.0:P` locks every one of them
    /// out, `SO_REUSEADDR` or not.
    fn wildcard_bind_loop() -> String {
        "python3 -u -c \"
import os,socket,time
p=int(os.environ['PORT'])
f=0
for i in range(1500):
    s=socket.socket()
    s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
    try:
        s.bind(('0.0.0.0',p))
        s.listen(5)
    except OSError:
        f+=1
    s.close()
    time.sleep(0.001)
print('bind failures',f)
time.sleep(300)
\""
        .to_string()
    }

    // Deciding "is it up yet?" by *taking* the port meant nothing else
    // could take it for the length of every probe — including the server
    // pando was waiting for, which then died with EADDRINUSE and a
    // classifier pointing at the wrong culprit.
    #[test]
    fn polling_readiness_never_refuses_the_server_its_own_port() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: wildcard_bind_loop(),
                ports: Some(PortsSpec::Map(BTreeMap::from([(
                    "PORT".to_string(),
                    "web".to_string(),
                )]))),
                ready: Some(ReadySpec {
                    role: None,
                    timeout_s: Some(120),
                }),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);

        // Polled the whole time the server is coming up, which is what
        // `ls`, `status` and the TUI's tick each do.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline && !log_of(&fx, &name).contains("bind failures")
        {
            refresh(&fx.paths);
        }
        let log = log_of(&fx, &name);
        assert!(
            log.contains("bind failures 0"),
            "the server was refused its own port while pando was checking on it: {log:?}"
        );
    }

    // ---- stop ------------------------------------------------------------

    #[test]
    fn stop_ends_the_process_and_keeps_the_ports() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.process().record.pgid;
        let ports = outcome.process().ports.clone();

        assert_eq!(stop(&fx.paths, &name).unwrap(), StopOutcome::Stopped);
        assert!(
            !crate::process::group_alive(pgid),
            "the group must be empty"
        );
        let record = &fx.state().worktrees[&name];
        assert!(
            record.processes.is_empty(),
            "the record goes with the process"
        );
        assert_eq!(
            record.ports, ports,
            "a stopped worktree still owns its ports"
        );
        assert!(record.created_by_pando, "and is still ours");
    }

    // The origin tool skipped the signal for a record it had written off,
    // and leaked every child whose shell had already exited.
    #[test]
    fn stop_signals_the_group_even_when_the_leader_is_already_gone() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30 & exit 0"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let (pid, pgid) = (outcome.process().record.pid, outcome.process().record.pgid);

        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(pid)
        }));
        assert!(
            crate::process::group_alive(pgid),
            "the backgrounded child is still there"
        );
        // Written off as failed, which is exactly when the signal used to be
        // skipped.
        let mut store = fx.state();
        store
            .worktrees
            .get_mut(&name)
            .unwrap()
            .processes
            .get_mut("dev")
            .unwrap()
            .phase = Phase::Failed {
            at: Utc::now(),
            reason: "process exited".into(),
        };
        state::save(&fx.paths.state_file(), &store).unwrap();

        assert_eq!(stop(&fx.paths, &name).unwrap(), StopOutcome::Stopped);
        assert!(
            !crate::process::group_alive(pgid),
            "a failed record's group must still be killed"
        );
    }

    #[test]
    fn stopping_something_that_is_not_running_is_not_an_error() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        assert_eq!(stop(&fx.paths, "nope").unwrap(), StopOutcome::NotRunning);
        let name = worktree_named(&fx, "feat/one");
        assert_eq!(stop(&fx.paths, &name).unwrap(), StopOutcome::NotRunning);
    }

    #[test]
    fn stop_all_stops_every_worktree_that_is_running() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let one = worktree_named(&fx, "feat/one");
        let two = worktree_named(&fx, "feat/two");
        let a = start(&fx.paths, &fx.config, &one, &noop).unwrap();
        let _ga = guard(&a);
        let b = start(&fx.paths, &fx.config, &two, &noop).unwrap();
        let _gb = guard(&b);
        assert_ne!(
            a.process().ports["web"],
            b.process().ports["web"],
            "two worktrees never share a port"
        );

        let mut stopped = stop_all(&fx.paths).unwrap();
        stopped.sort();
        assert_eq!(stopped, vec![one.clone(), two.clone()]);
        assert!(!crate::process::group_alive(a.process().record.pgid));
        assert!(!crate::process::group_alive(b.process().record.pgid));
        assert!(fx.state().worktrees[&one].processes.is_empty());
        assert!(fx.state().worktrees[&two].processes.is_empty());
        assert!(
            stop_all(&fx.paths).unwrap().is_empty(),
            "and it is idempotent"
        );
    }

    // A record under a name this pando does not start — state a newer one
    // wrote, or a process since renamed in config — is still a process
    // group, and `reconcile` is about to drop it.
    #[test]
    fn start_signals_every_group_recorded_for_the_worktree() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");

        // A leader that exits and leaves its child behind, recorded under
        // another process name.
        let log = fx.paths.log_file(&name, "worker");
        let stray = crate::testutil::spawn_guarded("sleep 30 & exit 0", &fx.root, &log);
        let (stray_pid, stray_pgid) = (stray.pid, stray.pgid);
        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(stray_pid)
        }));
        assert!(crate::process::group_alive(stray_pgid));

        let mut store = fx.state();
        store.worktrees.get_mut(&name).unwrap().processes.insert(
            "worker".to_string(),
            ProcessRecord {
                pid: stray_pid,
                pgid: stray_pgid,
                started_at: Utc::now(),
                log_path: log,
                ready_port: None,
                ready_timeout_s: None,
                phase: Phase::Running { since: Utc::now() },
            },
        );
        state::save(&fx.paths.state_file(), &store).unwrap();

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(
            !crate::process::group_alive(stray_pgid),
            "a group nothing would record again must not be left running"
        );
        assert_eq!(
            fx.state().worktrees[&name].processes.len(),
            1,
            "and its record goes with it"
        );
    }

    // ---- orphans in *another* worktree ------------------------------------

    /// A worktree whose dev process has already lost its leader: the shell
    /// exits at once and the child it backgrounded keeps the group — and its
    /// port — alive. This is the record every `reconcile` is about to drop,
    /// and dropping it unsignalled is how the child becomes unfindable.
    ///
    /// The fixture's config is left holding a plain `sleep`, so a command
    /// run against *another* worktree afterwards starts something ordinary.
    fn orphaned_sibling(fx: &mut Fx) -> (String, Detached) {
        with_dev(fx, dev("sleep 300 & exit 0"));
        let name = worktree_named(fx, "feat/orphan");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let orphan = guard(&outcome);
        assert!(
            wait_until(Duration::from_secs(5), || {
                !crate::process::is_alive(orphan.pid)
            }),
            "the shell that backgrounded the child should have exited"
        );
        assert!(
            crate::process::group_alive(orphan.pgid),
            "the child holds the group open"
        );
        with_dev(fx, dev("sleep 30"));
        (name, orphan)
    }

    #[test]
    fn stopping_one_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (orphan_name, orphan) = orphaned_sibling(&mut fx);
        let other = worktree_named(&fx, "feat/other");

        assert_eq!(stop(&fx.paths, &other).unwrap(), StopOutcome::NotRunning);
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "a record dropped by reconcile must have been signalled first"
        );
        assert!(
            fx.state().worktrees[&orphan_name].processes.is_empty(),
            "and only then is it dropped"
        );
    }

    #[test]
    fn starting_one_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (_orphan_name, orphan) = orphaned_sibling(&mut fx);
        let other = worktree_named(&fx, "feat/other");

        let outcome = start(&fx.paths, &fx.config, &other, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "starting one worktree must not orphan another one's child"
        );
    }

    #[test]
    fn removing_one_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (_orphan_name, orphan) = orphaned_sibling(&mut fx);
        let other = worktree_named(&fx, "feat/other");

        rm(&fx.paths, &other, false, false).unwrap();
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "removing one worktree must not orphan another one's child"
        );
    }

    #[test]
    fn creating_a_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (_orphan_name, orphan) = orphaned_sibling(&mut fx);

        worktree_named(&fx, "feat/other");
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "creating a worktree must not orphan another one's child"
        );
    }

    // A group that would not die must not have its record cleared: the pgid
    // is the only way back to it.
    #[test]
    fn stop_all_keeps_a_record_whose_group_it_could_not_signal() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let stubborn = worktree_named(&fx, "feat/stubborn");
        let willing = worktree_named(&fx, "feat/willing");
        let mut store = fx.state();
        for (name, pgid) in [(&stubborn, 4242), (&willing, 4243)] {
            store
                .worktrees
                .entry(name.clone())
                .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(name), true))
                .processes
                .insert("dev".to_string(), fake_record(pgid));
        }
        state::save(&fx.paths.state_file(), &store).unwrap();

        let err = stop_all_with(&fx.paths, |pgid| {
            if pgid == 4242 {
                anyhow::bail!("killpg refused");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            format!("{err:#}").contains(&stubborn),
            "the failure names the worktree: {err:#}"
        );

        let saved = fx.state();
        assert!(
            saved.worktrees[&stubborn].processes.contains_key("dev"),
            "a group that was not signalled keeps its record"
        );
        assert!(
            saved.worktrees[&willing].processes.is_empty(),
            "the ones that were signalled are cleared, and saved"
        );
    }

    /// A process record for a group that does not exist, for tests about
    /// bookkeeping rather than about signals.
    fn fake_record(pgid: i32) -> ProcessRecord {
        ProcessRecord {
            pid: pgid as u32,
            pgid,
            started_at: Utc::now(),
            log_path: PathBuf::from("/does/not/exist/dev.log"),
            ready_port: None,
            ready_timeout_s: None,
            phase: Phase::Running { since: Utc::now() },
        }
    }

    // ---- restart ---------------------------------------------------------

    #[test]
    fn restart_stops_the_old_process_and_keeps_the_ports() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let first_pgid = first.process().record.pgid;
        let ports = first.process().ports.clone();
        drop(guard(&first));

        let second = restart(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&second);
        assert!(matches!(second, StartOutcome::Started(_)));
        assert_ne!(second.process().record.pid, first.process().record.pid);
        assert!(!crate::process::group_alive(first_pgid));
        assert_eq!(
            second.process().ports,
            ports,
            "a restart keeps the URL the developer had open"
        );
        assert!(!second.process().reassigned);
    }

    #[test]
    fn restarting_something_that_was_never_started_just_starts_it() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = restart(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(matches!(outcome, StartOutcome::Started(_)));
    }

    // ---- the install hook ------------------------------------------------

    /// A fixture with a lockfile, so the install hook has a fingerprint.
    fn installable_fixture(install: &str) -> Fx {
        let mut fx = fixture();
        std::fs::write(fx.root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "lock"]);
        fx.config.project.install = Some(install.to_string());
        fx
    }

    fn install_log(fx: &Fx, name: &str) -> String {
        std::fs::read_to_string(fx.paths.log_file(name, INSTALL_HOOK)).unwrap_or_default()
    }

    fn install_fingerprint(fx: &Fx, name: &str) -> Option<String> {
        fx.state().worktrees[name]
            .hooks
            .get(INSTALL_HOOK)?
            .fingerprint
            .clone()
    }

    #[test]
    fn the_install_hook_runs_after_new_and_records_its_fingerprint() {
        let fx = installable_fixture("echo installed-once");
        let name = worktree_named(&fx, "feat/one");
        assert!(install_log(&fx, &name).contains("installed-once"));
        let recorded = install_fingerprint(&fx, &name).expect("a fingerprint");
        assert!(recorded.starts_with("md5:"), "{recorded}");
    }

    #[test]
    fn the_install_hook_is_skipped_while_the_lockfile_is_unchanged() {
        let mut fx = installable_fixture("echo run");
        let name = worktree_named(&fx, "feat/one");
        assert_eq!(install_log(&fx, &name).matches("run").count(), 1);

        with_dev(&mut fx, dev("sleep 30"));
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert_eq!(
            install_log(&fx, &name).matches("run").count(),
            1,
            "nothing changed, so nothing to install"
        );
    }

    // The case the fingerprint exists for: a branch with different
    // dependencies, or a rebase that moved the lockfile under a worktree.
    #[test]
    fn the_install_hook_runs_again_when_the_lockfile_changes() {
        let mut fx = installable_fixture("echo run");
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let first = install_fingerprint(&fx, &name).unwrap();

        let worktree = fx.worktrees_dir().join(&name);
        std::fs::write(worktree.join("pnpm-lock.yaml"), "lockfileVersion: '10.0'\n").unwrap();

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert_eq!(
            install_log(&fx, &name).matches("run").count(),
            2,
            "a changed lockfile is what makes it run again"
        );
        assert_ne!(install_fingerprint(&fx, &name).unwrap(), first);
    }

    // The worktree is the expensive thing; the install is retryable. A
    // failure reports itself and leaves everything else alone.
    #[test]
    fn a_failed_install_keeps_the_worktree_and_names_its_log() {
        let fx = installable_fixture("echo could-not-resolve >&2 && exit 1");
        let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("install"), "{msg}");
        assert!(msg.contains("could-not-resolve"), "{msg}");
        assert!(
            msg.contains("install.log"),
            "the log path is in the message: {msg}"
        );

        assert_eq!(
            fx.names(),
            vec!["feat+one".to_string()],
            "the worktree stays"
        );
        assert!(fx.worktrees_dir().join("feat+one").is_dir());
        assert!(
            install_fingerprint(&fx, "feat+one").is_none(),
            "a failed install records no fingerprint, so the next start retries"
        );
    }

    #[test]
    fn a_failed_install_is_retried_by_the_next_start() {
        let mut fx = installable_fixture("exit 1");
        assert!(new(&fx.paths, &fx.config, "feat/one", None, &noop).is_err());
        fx.config.project.install = Some("echo recovered".to_string());
        with_dev(&mut fx, dev("sleep 30"));

        let outcome = start(&fx.paths, &fx.config, "feat+one", &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(install_log(&fx, "feat+one").contains("recovered"));
        assert!(install_fingerprint(&fx, "feat+one").is_some());
    }

    // Verified by hand against pnpm 9: `pnpm install --frozen-lockfile` on a
    // lockfile it considers malformed rewrites it anyway. pando cannot stop
    // that, but it must not then re-install on every start forever.
    #[test]
    fn an_install_that_rewrites_its_own_lockfile_still_settles() {
        let mut fx = installable_fixture("echo rewriting && echo changed >> pnpm-lock.yaml");
        with_dev(&mut fx, dev("sleep 30"));
        let notices = std::cell::RefCell::new(Vec::<String>::new());
        let record_notice = |m: &str| notices.borrow_mut().push(m.to_string());

        let name = new(&fx.paths, &fx.config, "feat/one", None, &record_notice).unwrap();
        assert_eq!(install_log(&fx, &name).matches("rewriting").count(), 1);
        assert!(
            notices.borrow().iter().any(|n| n.contains("not as frozen")),
            "a lockfile changing under a worktree is worth saying out loud: {:?}",
            notices.borrow()
        );

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert_eq!(
            install_log(&fx, &name).matches("rewriting").count(),
            1,
            "the fingerprint recorded is the one the install left behind, so it settles"
        );
    }

    // On `start` for a worktree pando did not create there is no record yet,
    // so a hook result written through `get_mut` would be dropped and the
    // install would run again on every single start.
    #[test]
    fn the_install_hook_settles_on_an_adopted_worktree_too() {
        let mut fx = installable_fixture("echo run");
        with_dev(&mut fx, dev("sleep 30"));
        // Created by git, not by pando: no state record exists.
        let adopted = fx.worktrees_dir().join("adopted");
        std::fs::create_dir_all(fx.worktrees_dir()).unwrap();
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
        assert!(!fx.state().worktrees.contains_key("adopted"));

        let first = start(&fx.paths, &fx.config, "adopted", &noop).unwrap();
        drop(guard(&first));
        assert_eq!(install_log(&fx, "adopted").matches("run").count(), 1);
        assert!(
            install_fingerprint(&fx, "adopted").is_some(),
            "the fingerprint is recorded even with no record to hang it on yet"
        );
        assert!(
            !fx.state().worktrees["adopted"].created_by_pando,
            "and pando does not claim a worktree it found"
        );

        stop(&fx.paths, "adopted").unwrap();
        let second = start(&fx.paths, &fx.config, "adopted", &noop).unwrap();
        let _guard = guard(&second);
        assert_eq!(
            install_log(&fx, "adopted").matches("run").count(),
            1,
            "nothing changed, so the install does not run again"
        );
    }

    // The hook's `PANDO_BRANCH` was the directory name and the dev
    // process's was the git branch, so `git checkout "$PANDO_BRANCH"` in a
    // hook checked out the wrong thing — or nothing at all.
    #[test]
    fn pando_branch_is_the_git_branch_for_hooks_as_well_as_processes() {
        let mut fx = fixture();
        fx.config.project.install = Some("echo INSTALL PANDO_BRANCH=$PANDO_BRANCH".to_string());
        with_dev(
            &mut fx,
            dev("echo DEV PANDO_BRANCH=$PANDO_BRANCH && sleep 30"),
        );
        let name = worktree_named(&fx, "feat/one");
        assert_eq!(name, "feat+one", "the directory name is the sanitised one");

        assert!(
            install_log(&fx, &name).contains("PANDO_BRANCH=feat/one"),
            "the hook gets the branch, not the directory: {:?}",
            install_log(&fx, &name)
        );

        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(
            wait_until(Duration::from_secs(10), || log_of(&fx, &name)
                .contains("PANDO_BRANCH=")),
            "the process never logged its environment: {:?}",
            log_of(&fx, &name)
        );
        assert!(
            log_of(&fx, &name).contains("PANDO_BRANCH=feat/one"),
            "and both agree: {:?}",
            log_of(&fx, &name)
        );
    }

    #[test]
    fn a_project_with_no_install_step_runs_no_hook() {
        let fx = fixture();
        let name = worktree_named(&fx, "feat/one");
        assert!(!fx.paths.log_file(&name, INSTALL_HOOK).exists());
        assert!(fx.state().worktrees[&name].hooks.is_empty());
    }

    // No lockfile means nothing can say the dependencies are unchanged, so
    // the hook has to run every time rather than guess.
    #[test]
    fn an_install_with_nothing_to_fingerprint_runs_every_time() {
        let mut fx = fixture();
        fx.config.project.install = Some("echo run".to_string());
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        assert_eq!(install_log(&fx, &name).matches("run").count(), 2);
    }

    // ---- questions -------------------------------------------------------

    use crate::detect::Slot;

    /// Questions a scripted `ask` was asked, shared with the test.
    type AskedQuestions = std::rc::Rc<std::cell::RefCell<Vec<Question>>>;

    /// An `ask` that answers from a script and records what it was asked.
    fn scripted(answers: Vec<Answer>) -> (impl Fn(&Question) -> Result<Answer>, AskedQuestions) {
        let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = asked.clone();
        let answers = std::cell::RefCell::new(answers.into_iter());
        let ask = move |q: &Question| -> Result<Answer> {
            seen.borrow_mut().push(q.clone());
            answers
                .borrow_mut()
                .next()
                .ok_or_else(|| anyhow::anyhow!("asked more questions than the test scripted"))
        };
        (ask, asked)
    }

    fn refuse(_: &Question) -> Result<Answer> {
        panic!("nothing should have been asked")
    }

    /// A fixture with a package.json, a lockfile, and an env example — the
    /// shape detection is built for.
    fn detectable_fixture(scripts: &str, env_example: &str) -> Fx {
        let fx = fixture();
        std::fs::write(
            fx.root.join("package.json"),
            format!("{{\n  \"name\": \"x\",\n  \"scripts\": {scripts}\n}}\n"),
        )
        .unwrap();
        std::fs::write(fx.root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(fx.root.join(".env.example"), env_example).unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "app"]);
        fx
    }

    #[test]
    fn an_unambiguous_project_is_resolved_without_asking_anything() {
        let fx = detectable_fixture(
            r#"{ "dev": "next dev", "build": "next build" }"#,
            "PORT=3000\n",
        );
        let notices = std::cell::RefCell::new(Vec::<String>::new());
        let config = resolve_process(&fx.paths, &fx.config, &refuse, &|m| {
            notices.borrow_mut().push(m.to_string())
        })
        .unwrap();
        let notices = notices.into_inner();
        assert_eq!(config.processes["dev"].cmd, "pnpm dev");
        assert_eq!(config.processes["dev"].roles(), vec!["web"]);
        assert!(
            notices.iter().any(|n| n.contains("pnpm dev")),
            "every guess is visible: {notices:?}"
        );
    }

    #[test]
    fn an_answer_is_written_to_the_config_with_a_comment() {
        let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
        resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            written.contains("cmd = \"pnpm dev\"  # detected: package.json scripts.dev"),
            "{written}"
        );
        assert!(written.contains("[dev]"), "{written}");
        assert!(
            written.contains("ports = { PORT = \"web\" }"),
            "the map form is what a developer would have written: {written}"
        );
        // And the file pando wrote is one pando reads back.
        let loaded = crate::config::load(&fx.paths).unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.processes["dev"].cmd, "pnpm dev");
    }

    #[test]
    fn an_ambiguous_project_asks_once_and_never_again() {
        let fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\nAPI_PORT=3001\n",
        );
        let (ask, asked) = scripted(vec![Answer::Choice(1), Answer::Choice(0)]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert_eq!(config.processes["dev"].cmd, "pnpm dev:web");
        assert_eq!(config.processes["dev"].port_env()["PORT"], "{port:web}");
        let questions = asked.borrow();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].slot, Slot::DevCmd);
        assert_eq!(questions[0].preselect, Some(0));
        assert!(questions[0].allow_custom);
        assert_eq!(questions[1].slot, Slot::PortEnv);

        // Second run, reading the config that was just written: nothing left
        // to ask.
        let loaded = crate::config::load(&fx.paths).unwrap().config;
        let again = resolve_process(&fx.paths, &loaded, &refuse, &noop).unwrap();
        assert_eq!(again.processes["dev"].cmd, "pnpm dev:web");
    }

    #[test]
    fn a_typed_answer_is_taken_as_written_and_dated() {
        let fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\nAPI_PORT=3001\n",
        );
        let (ask, _) = scripted(vec![Answer::Custom(
            "./scripts/serve.sh --port {port:web}".to_string(),
        )]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert_eq!(
            config.processes["dev"].cmd,
            "./scripts/serve.sh --port {port:web}"
        );
        assert_eq!(
            config.processes["dev"].roles(),
            vec!["web"],
            "a command carrying {{port:web}} has answered the port question"
        );
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(written.contains("# answered:"), "{written}");
        assert!(written.contains("ports = [\"web\"]"), "{written}");
    }

    // "no ports" and "not answered yet" used to be the same state, so a
    // process that really has none — a worker, a watcher, a queue consumer
    // — was asked again on every start, and given a port it would never
    // bind if anything answered for it.
    #[test]
    fn answering_none_to_the_port_question_is_written_down_as_no_ports() {
        let fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\nAPI_PORT=3001\n",
        );
        let (ask, asked) = scripted(vec![
            Answer::Custom("./worker.sh".to_string()),
            Answer::None,
        ]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert_eq!(asked.borrow().len(), 2, "the command, then the port");
        assert!(
            asked.borrow()[1].allow_none,
            "the port question has to offer \"none\" for this to be answerable"
        );
        assert!(
            config.processes["dev"].roles().is_empty(),
            "a process with no ports has no roles"
        );

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            written.contains("ports = []"),
            "an empty list, so it reads as answered rather than missing: {written}"
        );

        // And asked once: a second resolve has nothing left to ask about.
        let again = resolve_process(&fx.paths, &config, &refuse, &noop).unwrap();
        assert!(again.processes["dev"].roles().is_empty());
    }

    #[test]
    fn a_config_the_developer_already_wrote_is_never_questioned() {
        let mut fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\nAPI_PORT=3001\n",
        );
        with_dev(&mut fx, dev("./my-own-server"));
        let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert_eq!(config.processes["dev"].cmd, "./my-own-server");
        assert!(
            !fx.paths.config_file().exists(),
            "resolving nothing writes nothing"
        );
    }

    // Detection may only ever write `[dev]`, and a file holding both `[dev]`
    // and `[processes]` is one pando's own loader refuses — which used to
    // brick every later command, `stop` included.
    #[test]
    fn detection_never_writes_a_dev_table_next_to_a_configured_process() {
        let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
        fx.config
            .processes
            .insert("web".to_string(), dev("sleep 30"));

        let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert!(
            !config.processes.contains_key("dev"),
            "a project that declares its processes has answered both slots"
        );
        assert!(
            !fx.paths.config_file().exists(),
            "and nothing at all was written: {:?}",
            std::fs::read_to_string(fx.paths.config_file()).ok()
        );
    }

    #[test]
    fn a_library_is_resolved_to_nothing_at_all() {
        let fx = fixture();
        let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert!(
            config.processes.is_empty(),
            "a repository with no server gets no process and no question"
        );
        assert!(!fx.paths.config_file().exists());
    }

    #[test]
    fn new_resolves_install_version_files_and_provision() {
        let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
        std::fs::write(fx.root.join(".nvmrc"), "22\n").unwrap();
        git(&fx.root, &["add", ".nvmrc"]);
        git(&fx.root, &["commit", "--quiet", "-m", "nvmrc"]);

        let config = resolve_for_new(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert_eq!(
            config.project.install.as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
        assert_eq!(config.runtime.version_files, vec![".nvmrc"]);
        assert_eq!(config.project.provision, vec![".env"]);
        assert!(
            config.processes.is_empty(),
            "new does not need the dev command yet"
        );
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(written.contains("# detected: pnpm-lock.yaml"), "{written}");
    }

    // The case `--yes` cannot rescue: pando knows there is a server here and
    // has no candidate to offer.
    #[test]
    fn a_framework_with_no_command_shape_asks_with_no_options() {
        let fx = fixture();
        std::fs::write(
            fx.root.join("package.json"),
            "{\n  \"scripts\": { \"build\": \"node build.js\" }\n}\n",
        )
        .unwrap();
        let (ask, asked) = scripted(vec![Answer::Custom("node server.js".to_string())]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert_eq!(config.processes["dev"].cmd, "node server.js");
        let questions = asked.borrow();
        assert_eq!(questions[0].slot, Slot::DevCmd);
        assert!(questions[0].options.is_empty());
        assert_eq!(
            questions[0].preselect, None,
            "there is nothing to recommend, so --yes has nothing to take"
        );
        assert!(questions[0].allow_custom);
    }

    #[test]
    fn an_empty_answer_is_refused() {
        let fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\n",
        );
        let (ask, _) = scripted(vec![Answer::Custom("   ".to_string())]);
        let err = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("empty answer"), "{err:#}");
    }

    // A dev server whose working directory has just been deleted is not a
    // process anyone can do anything with — and `rm` removes the record
    // that is the only way to find it again.
    #[test]
    fn rm_stops_what_is_running_before_it_removes_the_worktree() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.process().record.pgid;
        assert!(crate::process::group_alive(pgid));

        rm(&fx.paths, &name, false, false).unwrap();
        assert!(
            !crate::process::group_alive(pgid),
            "rm must not leave a process behind with no record of it"
        );
        assert!(!fx.state().worktrees.contains_key(&name));
        assert!(fx.names().is_empty());
    }

    // git's refusal is the last one, and it used to come *after* the kill:
    // the dev server was stopped, git then kept the worktree, and the next
    // read blamed the process for pando's own kill.
    #[test]
    fn rm_refuses_a_dirty_worktree_before_it_stops_anything() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 300"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.process().record.pgid;
        std::fs::write(
            fx.worktrees_dir().join(&name).join("README.md"),
            "edited in the worktree\n",
        )
        .unwrap();

        let err = rm(&fx.paths, &name, false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("modified or untracked"),
            "{err:#}"
        );
        assert!(
            crate::process::group_alive(pgid),
            "a removal pando declined has not touched what is running"
        );
        let record = &fx.state().worktrees[&name].processes["dev"];
        assert!(
            matches!(record.phase, Phase::Starting { .. } | Phase::Running { .. }),
            "and the process is not written off for a kill that never happened: {:?}",
            record.phase
        );
    }

    // A start that will only report what is already up must not run an
    // install first: `npm ci` inside a worktree whose dev server is live is
    // a surprise nobody asked for.
    #[test]
    fn a_start_that_reports_a_running_process_runs_no_install() {
        let mut fx = fixture();
        // No lockfile here, so the hook has no fingerprint and runs on
        // every start — which is what makes this visible at all.
        let marker = fx.paths.project_dir().join("install-ran");
        fx.config.project.install = Some(format!("echo ran >> {}", marker.display()));
        with_dev(&mut fx, dev("sleep 300"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let runs = || {
            std::fs::read_to_string(&marker)
                .unwrap_or_default()
                .lines()
                .count()
        };
        let before = runs();
        assert!(
            before > 0,
            "the hook has to have run at all for this to mean anything"
        );

        let second = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        assert!(matches!(second, StartOutcome::AlreadyRunning(_)));
        assert_eq!(
            runs(),
            before,
            "nothing was started, so nothing was installed"
        );
    }

    // A refusal must still leave the worktree usable: it is only stopped
    // once every reason to refuse has been checked.
    #[test]
    fn a_refused_rm_leaves_the_process_running() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.process().record.pgid;

        // Locked worktrees are always refused, before anything is touched.
        git(
            &fx.root,
            &[
                "worktree",
                "lock",
                fx.worktrees_dir().join(&name).to_str().unwrap(),
            ],
        );
        assert!(rm(&fx.paths, &name, true, true).is_err());
        assert!(
            crate::process::group_alive(pgid),
            "a removal pando declined has not touched what is running"
        );
        git(
            &fx.root,
            &[
                "worktree",
                "unlock",
                fx.worktrees_dir().join(&name).to_str().unwrap(),
            ],
        );
    }

    // ---- refresh ---------------------------------------------------------

    #[test]
    fn refresh_on_a_project_that_never_started_anything_writes_nothing() {
        let fx = fixture();
        let refreshed = refresh(&fx.paths);
        assert!(refreshed.state.worktrees.is_empty());
        assert!(refreshed.warning.is_none());
        assert!(
            !fx.paths.state_file().exists(),
            "a read path must not create state"
        );
    }

    #[test]
    fn refresh_moves_a_starting_process_to_running_once_its_port_binds() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: python_listener_template(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ..Default::default()
            },
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.process().ports["web"];

        assert!(
            wait_until(Duration::from_secs(20), || matches!(
                refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
                Phase::Running { .. }
            )),
            "never reached Running: {:?}",
            log_of(&fx, &name)
        );
        // Saved, not just computed: the next command must see it too.
        assert!(matches!(
            fx.state().worktrees[&name].processes["dev"].phase,
            Phase::Running { .. }
        ));
        assert!(
            wait_until(Duration::from_secs(10), || refresh(&fx.paths)
                .state
                .worktrees[&name]
                .observed_ports
                .contains(&port)),
            "the port it is really listening on is recorded"
        );
    }

    // The whole reason read paths advance instead of reconciling: a crashed
    // dev server has to stay on screen until the developer acts on it.
    #[test]
    fn a_crashed_process_stays_failed_across_reads() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("echo boom && exit 1"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);

        assert!(wait_until(Duration::from_secs(10), || matches!(
            refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
            Phase::Failed { .. }
        )));
        for _ in 0..3 {
            let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
                .phase
                .clone();
            assert!(
                matches!(&phase, Phase::Failed { reason, .. } if reason.starts_with("process exited")),
                "a failure must not be swept away by the next read: {phase:?}"
            );
        }
        // And the worktree keeps its ports, so a restart reuses them.
        assert!(!fx.state().worktrees[&name].ports.is_empty());
    }

    #[test]
    fn a_failure_the_log_explains_carries_the_hint() {
        let mut fx = fixture();
        // A port that is already taken, reported the way Node reports it.
        with_dev(
            &mut fx,
            dev("echo 'Error: listen EADDRINUSE: address already in use :::3000' && exit 1"),
        );
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _guard = guard(&outcome);

        assert!(wait_until(Duration::from_secs(10), || matches!(
            refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
            Phase::Failed { .. }
        )));
        let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
            .phase
            .clone();
        let Phase::Failed { reason, .. } = &phase else {
            panic!("expected a failure, got {phase:?}");
        };
        assert!(reason.starts_with("process exited"), "{reason}");
        assert!(reason.contains("3000"), "the hint names the port: {reason}");
        let again = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
            .phase
            .clone();
        let Phase::Failed { reason: again, .. } = &again else {
            panic!("still failed: {again:?}");
        };
        assert_eq!(
            again, reason,
            "the hint is written once, not appended again on every read"
        );
    }

    #[test]
    fn refresh_reports_a_state_file_it_cannot_use_instead_of_failing() {
        let fx = fixture();
        fx.paths.ensure_home().unwrap();
        std::fs::write(fx.paths.state_file(), "{ not json").unwrap();
        let refreshed = refresh(&fx.paths);
        assert!(refreshed.state.worktrees.is_empty());
        let warning = refreshed.warning.expect("a broken state file is reported");
        assert!(warning.contains("state"), "{warning}");
        assert_eq!(
            std::fs::read_to_string(fx.paths.state_file()).unwrap(),
            "{ not json",
            "and it is never overwritten"
        );
    }

    #[test]
    fn ownership_still_reads_through_refresh() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let worktrees = ls(&fx.paths).unwrap();
        let owned = created_by_pando(&fx.paths, &worktrees);
        assert_eq!(owned.by_name.get(&name), Some(&true));
        assert!(owned.warning.is_none());
    }

    #[test]
    fn the_prelude_is_prefixed_to_the_command() {
        let mut config = Config::default();
        assert_eq!(with_prelude(&config, "pnpm dev"), "pnpm dev");
        config.runtime.prelude = Some("  ".to_string());
        assert_eq!(
            with_prelude(&config, "pnpm dev"),
            "pnpm dev",
            "a blank prelude adds nothing"
        );
        config.runtime.prelude = Some("nvm use 22".to_string());
        assert_eq!(with_prelude(&config, "pnpm dev"), "nvm use 22 && pnpm dev");
    }

    /// A repo with one commit, a gitignore listing `.env`, and an untracked
    /// ignored `.env` present so provisioning has something to link.
    fn fixture() -> Fx {
        let dir = tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
        std::fs::write(root.join("README.md"), "# acme\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "root"]);
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();

        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        Fx {
            root: paths.root().to_path_buf(),
            paths,
            config: Config::default(),
            _dir: dir,
        }
    }

    /// The same fixture, cloned from a bare origin so remote-tracking refs
    /// exist. `remote_branches` are pushed to origin and not checked out.
    fn fixture_with_origin(remote_branches: &[&str]) -> Fx {
        let dir = tempdir().unwrap();
        let bare = dir.path().join("origin.git");
        git(
            dir.path(),
            &[
                "init",
                "--bare",
                "--quiet",
                "--initial-branch=main",
                bare.to_str().unwrap(),
            ],
        );
        let seed = dir.path().join("seed");
        git(
            dir.path(),
            &[
                "clone",
                "--quiet",
                bare.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        std::fs::write(seed.join(".gitignore"), ".env\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "--quiet", "-m", "root"]);
        git(&seed, &["push", "--quiet", "origin", "main"]);
        for branch in remote_branches {
            git(&seed, &["checkout", "--quiet", "-b", branch]);
            git(
                &seed,
                &["commit", "--quiet", "--allow-empty", "-m", "remote work"],
            );
            git(&seed, &["push", "--quiet", "origin", branch]);
        }
        let root = dir.path().join("acme-shop");
        git(
            dir.path(),
            &[
                "clone",
                "--quiet",
                bare.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();

        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        Fx {
            root: paths.root().to_path_buf(),
            paths,
            config: Config::default(),
            _dir: dir,
        }
    }

    fn upstream_of(root: &Path, branch: &str) -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                &format!("{branch}@{{upstream}}"),
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    fn sanitize_turns_slashes_into_plus_signs() {
        assert_eq!(sanitize_branch_to_dir("feat/checkout"), "feat+checkout");
        assert_eq!(sanitize_branch_to_dir("a/b/c"), "a+b+c");
        assert_eq!(sanitize_branch_to_dir("plain"), "plain");
    }

    #[test]
    fn new_creates_the_branch_and_worktree_under_pando_home() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();

        assert_eq!(name, "feat+one");
        let target = fx.worktrees_dir().join("feat+one");
        assert!(
            target.is_dir(),
            "worktree not created at {}",
            target.display()
        );
        assert!(
            target.starts_with(&fx.paths.home),
            "worktrees must live under pando's home by default"
        );
        assert_eq!(fx.names(), vec!["feat+one"]);
    }

    #[test]
    fn new_records_created_by_pando_in_state() {
        let fx = fixture();
        new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        let rec = fx.state().worktrees.get("feat+one").cloned().unwrap();
        assert!(rec.created_by_pando);
        assert_eq!(
            rec.path,
            fx.worktrees_dir().join("feat+one").canonicalize().unwrap()
        );
    }

    // A tracked new branch would turn a later `git pull` into "merge main
    // into my feature branch".
    #[test]
    fn a_new_branch_has_no_upstream() {
        let fx = fixture_with_origin(&[]);
        new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        assert_eq!(
            upstream_of(&fx.root, "feat/one"),
            None,
            "a forked branch must not track its base"
        );
    }

    #[test]
    fn new_with_an_existing_local_branch_checks_it_out() {
        let fx = fixture();
        git(&fx.root, &["branch", "feat/existing"]);
        git(
            &fx.root,
            &["commit", "--quiet", "--allow-empty", "-m", "main moves on"],
        );

        let name = new(&fx.paths, &fx.config, "feat/existing", None, &noop).unwrap();
        let head = Command::new("git")
            .arg("-C")
            .arg(fx.worktrees_dir().join(&name))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "feat/existing",
            "an existing branch is checked out, not recreated"
        );
    }

    #[test]
    fn new_with_a_remote_only_branch_tracks_the_remote() {
        let fx = fixture_with_origin(&["feat/remote"]);
        new(&fx.paths, &fx.config, "feat/remote", None, &noop).unwrap();
        assert_eq!(
            upstream_of(&fx.root, "feat/remote").as_deref(),
            Some("origin/feat/remote"),
            "checking out a remote branch should track it"
        );
    }

    // The remote-tracking ref does not exist locally yet: pando has to fetch
    // before it can tell a remote branch from a brand new one.
    #[test]
    fn new_fetches_a_remote_branch_that_has_not_been_fetched_yet() {
        let fx = fixture_with_origin(&[]);
        let bare = fx.root.parent().unwrap().join("origin.git");
        let seed = fx.root.parent().unwrap().join("seed");
        git(&seed, &["checkout", "--quiet", "-b", "feat/late"]);
        git(&seed, &["commit", "--quiet", "--allow-empty", "-m", "late"]);
        git(&seed, &["push", "--quiet", "origin", "feat/late"]);
        assert!(bare.exists());
        assert!(!ref_exists(&fx.root, "refs/remotes/origin/feat/late"));

        new(&fx.paths, &fx.config, "feat/late", None, &noop).unwrap();
        assert_eq!(
            upstream_of(&fx.root, "feat/late").as_deref(),
            Some("origin/feat/late")
        );
    }

    #[test]
    fn new_forks_from_the_requested_base() {
        let fx = fixture();
        git(&fx.root, &["checkout", "--quiet", "-b", "release"]);
        git(
            &fx.root,
            &["commit", "--quiet", "--allow-empty", "-m", "release only"],
        );
        git(&fx.root, &["checkout", "--quiet", "main"]);

        let name = new(&fx.paths, &fx.config, "fix/one", Some("release"), &noop).unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(fx.worktrees_dir().join(&name))
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "release only");
    }

    #[test]
    fn branch_rules_choose_the_base_when_no_argument_is_given() {
        let mut fx = fixture();
        git(&fx.root, &["checkout", "--quiet", "-b", "beta"]);
        git(
            &fx.root,
            &["commit", "--quiet", "--allow-empty", "-m", "beta only"],
        );
        git(&fx.root, &["checkout", "--quiet", "main"]);
        fx.config.branches.rules = vec![crate::config::BranchRule {
            match_: "*-beta".into(),
            base: "beta".into(),
        }];

        let name = new(&fx.paths, &fx.config, "fix/thing-beta", None, &noop).unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(fx.worktrees_dir().join(&name))
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "beta only");
    }

    // A bare base name must mean "current origin state", not a local branch
    // that has not been pulled in weeks.
    #[test]
    fn a_bare_base_name_prefers_the_remote_tracking_ref() {
        let fx = fixture_with_origin(&[]);
        let seed = fx.root.parent().unwrap().join("seed");
        git(&seed, &["checkout", "--quiet", "main"]);
        git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "origin moved"],
        );
        git(&seed, &["push", "--quiet", "origin", "main"]);
        git(&fx.root, &["fetch", "--quiet", "origin"]);

        let name = new(&fx.paths, &fx.config, "feat/fresh", Some("main"), &noop).unwrap();
        let out = Command::new("git")
            .arg("-C")
            .arg(fx.worktrees_dir().join(&name))
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "origin moved",
            "a bare base should fork from origin/main, not the stale local main"
        );
    }

    #[test]
    fn new_refuses_an_invalid_branch_name_before_creating_anything() {
        let fx = fixture();
        for bad in ["feat//two", "-leading-dash", "has space", "", "ends.lock"] {
            assert!(
                new(&fx.paths, &fx.config, bad, None, &noop).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(
            !fx.worktrees_dir().exists(),
            "a refused create must not even make the worktrees directory"
        );
    }

    #[test]
    fn new_refuses_a_name_already_checked_out_in_another_worktree() {
        let fx = fixture();
        new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
        assert!(
            format!("{err:#}").contains("already exists"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fx.names().len(), 1);
    }

    #[test]
    fn new_refuses_a_branch_checked_out_elsewhere_and_surfaces_gits_reason() {
        let fx = fixture();
        // Adopt a worktree in another location holding the branch, then ask
        // for the same branch under a different directory name.
        let elsewhere = fx.root.parent().unwrap().join("elsewhere");
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "taken",
                elsewhere.to_str().unwrap(),
            ],
        );
        let err = new(&fx.paths, &fx.config, "taken", None, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("git worktree add failed"), "{msg}");
        assert!(msg.contains("taken"), "{msg}");
    }

    #[test]
    fn new_refuses_a_provision_path_that_is_not_gitignored() {
        let mut fx = fixture();
        fx.config.project.provision = vec!["README.md".into()];
        let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not ignored"), "{msg}");
        assert!(msg.contains("README.md"), "{msg}");
        assert!(
            fx.names().is_empty(),
            "the refusal must happen before git is asked to do anything"
        );
    }

    #[test]
    fn new_refuses_a_provision_path_outside_the_repository() {
        let mut fx = fixture();
        fx.config.project.provision = vec!["../escape.env".into()];
        let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
        assert!(
            format!("{err:#}").contains("check-ignore"),
            "git's own refusal should be surfaced: {err:#}"
        );
    }

    #[test]
    fn provisioned_files_are_symlinked_by_default_and_copied_on_request() {
        let mut fx = fixture();
        fx.config.project.provision = vec![".env".into()];
        let name = new(&fx.paths, &fx.config, "feat/link", None, &noop).unwrap();
        let linked = fx.worktrees_dir().join(&name).join(".env");
        assert!(
            std::fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link mode must produce a symlink"
        );
        assert_eq!(std::fs::read_to_string(&linked).unwrap(), "SECRET=1\n");

        fx.config.project.provision_mode = ProvisionMode::Copy;
        let name = new(&fx.paths, &fx.config, "feat/copy", None, &noop).unwrap();
        let copied = fx.worktrees_dir().join(&name).join(".env");
        assert!(
            !std::fs::symlink_metadata(&copied)
                .unwrap()
                .file_type()
                .is_symlink(),
            "copy mode must produce a real file"
        );
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "SECRET=1\n");
    }

    #[test]
    fn a_missing_provision_source_is_skipped_rather_than_invented() {
        let mut fx = fixture();
        std::fs::remove_file(fx.root.join(".env")).unwrap();
        fx.config.project.provision = vec![".env".into()];
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        assert!(!fx.worktrees_dir().join(&name).join(".env").exists());
    }

    #[test]
    fn rm_removes_a_pando_worktree_with_its_logs_and_data() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        std::fs::create_dir_all(fx.paths.logs_dir(&name)).unwrap();
        std::fs::write(fx.paths.log_file(&name, "dev"), "log line\n").unwrap();
        std::fs::create_dir_all(fx.paths.data_dir(&name)).unwrap();

        rm(&fx.paths, &name, false, false).unwrap();

        assert!(fx.names().is_empty());
        assert!(!fx.worktrees_dir().join(&name).exists());
        assert!(!fx.paths.logs_dir(&name).exists());
        assert!(!fx.paths.data_dir(&name).exists());
        assert!(!fx.state().worktrees.contains_key(&name));
    }

    // Any command that creates a directory under pando's home must make the
    // home itself 0700 first: later phases copy env files in there, and a
    // home created by a stray `create_dir_all` would carry the umask.
    #[test]
    fn a_first_run_rm_still_creates_a_private_home() {
        use std::os::unix::fs::PermissionsExt;
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
        assert!(!fx.paths.home.exists(), "nothing has written the home yet");

        rm(&fx.paths, "adopted", true, false).unwrap();

        let mode = std::fs::metadata(&fx.paths.home)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "pando home must be private from the first write"
        );
    }

    #[test]
    fn rm_keeps_the_branch() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        rm(&fx.paths, &name, false, false).unwrap();
        assert!(
            ref_exists(&fx.root, "refs/heads/feat/one"),
            "rm removes the worktree, not the work"
        );
    }

    #[test]
    fn rm_refuses_an_adopted_worktree_without_yes() {
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

        let err = rm(&fx.paths, "adopted", false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("--yes"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fx.names(), vec!["adopted"]);

        rm(&fx.paths, "adopted", true, false).unwrap();
        assert!(fx.names().is_empty());
    }

    #[test]
    fn rm_refuses_a_dirty_worktree_without_force() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        std::fs::write(fx.worktrees_dir().join(&name).join("scratch.txt"), "wip").unwrap();

        let err = rm(&fx.paths, &name, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("modified or untracked"), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert!(
            msg.contains("scratch.txt"),
            "it names what is in the way: {msg}"
        );
        assert_eq!(fx.names(), vec![name.clone()]);

        rm(&fx.paths, &name, false, true).unwrap();
        assert!(fx.names().is_empty());
    }

    // An ignored, provisioned file is pando's own doing and must never be
    // the reason a removal needs --force.
    #[test]
    fn a_provisioned_env_file_does_not_block_removal() {
        let mut fx = fixture();
        fx.config.project.provision = vec![".env".into()];
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        rm(&fx.paths, &name, false, false).unwrap();
        assert!(fx.names().is_empty());
        assert_eq!(
            std::fs::read_to_string(fx.root.join(".env")).unwrap(),
            "SECRET=1\n",
            "the main checkout's file must survive its symlink being removed"
        );
    }

    #[test]
    fn rm_always_refuses_a_locked_worktree_and_shows_the_reason() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        git(
            &fx.root,
            &[
                "worktree",
                "lock",
                "--reason",
                "benchmark running",
                fx.worktrees_dir().join(&name).to_str().unwrap(),
            ],
        );

        for (yes, force) in [(false, false), (true, false), (true, true)] {
            let err = rm(&fx.paths, &name, yes, force).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("locked"), "{msg}");
            assert!(msg.contains("benchmark running"), "{msg}");
        }
        assert_eq!(fx.names(), vec![name]);
    }

    #[test]
    fn rm_clears_one_prunable_entry_and_leaves_the_others_alone() {
        let fx = fixture();
        let gone = new(&fx.paths, &fx.config, "feat/gone", None, &noop).unwrap();
        let other = new(&fx.paths, &fx.config, "feat/other", None, &noop).unwrap();
        std::fs::remove_dir_all(fx.worktrees_dir().join(&gone)).unwrap();
        std::fs::remove_dir_all(fx.worktrees_dir().join(&other)).unwrap();

        rm(&fx.paths, &gone, false, false).unwrap();

        let left = fx.names();
        assert_eq!(
            left,
            vec![other],
            "removing one prunable entry must not sweep the others"
        );
    }

    #[test]
    fn rm_refuses_the_main_checkout_and_an_unknown_name() {
        let fx = fixture();
        let main_name = worktree::discover_all(&fx.paths.project).unwrap().main.name;
        let err = rm(&fx.paths, &main_name, true, true).unwrap_err();
        assert!(format!("{err:#}").contains("main checkout"), "{err:#}");

        let err = rm(&fx.paths, "nope", true, true).unwrap_err();
        assert!(format!("{err:#}").contains("no worktree named"), "{err:#}");
    }

    #[test]
    fn ls_lists_managed_worktrees_with_enrichment() {
        let fx = fixture();
        new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        let listed = ls(&fx.paths).unwrap();
        assert_eq!(listed.len(), 1);
        let w = &listed[0];
        assert_eq!(w.name, "feat+one");
        assert_eq!(w.branch.as_deref(), Some("feat/one"));
        assert!(w.head_sha.is_some());
        assert_eq!(w.dirty, Some(false));
        assert_eq!(w.ahead_behind, Some((0, 0)));
    }

    #[test]
    fn path_prints_the_absolute_canonical_path() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        let p = path(&fx.paths, &name).unwrap();
        assert!(p.is_absolute());
        assert_eq!(p, fx.worktrees_dir().join(&name).canonicalize().unwrap());
        assert!(path(&fx.paths, "nope").is_err());
    }

    #[test]
    fn created_by_pando_distinguishes_adopted_worktrees() {
        let fx = fixture();
        new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
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

        let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
        assert_eq!(owned.by_name.get("feat+one"), Some(&true));
        assert_eq!(
            owned.by_name.get("adopted"),
            None,
            "an adopted worktree has no record"
        );
    }

    #[test]
    fn a_configured_worktrees_dir_outside_the_repository_is_honoured() {
        let mut fx = fixture();
        let elsewhere = fx.root.parent().unwrap().join("custom-trees");
        fx.config.project.worktrees_dir = Some(elsewhere.clone());

        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        assert!(elsewhere.join(&name).is_dir());
        assert_eq!(
            path(&fx.paths, &name).unwrap(),
            elsewhere.join(&name).canonicalize().unwrap()
        );

        rm(&fx.paths, &name, false, false).unwrap();
        assert!(!elsewhere.join(&name).exists());
    }

    // `ls` labels a worktree "adopted" from the same file `rm` keys its
    // confirmation off. When that file cannot be read, both have to say the
    // same thing rather than one shrugging and the other failing.
    #[test]
    fn created_by_pando_reports_a_state_file_it_cannot_use() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        std::fs::write(fx.paths.state_file(), r#"{"version":3,"worktrees":{}}"#).unwrap();

        let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
        assert!(owned.by_name.is_empty());
        let warning = owned
            .warning
            .expect("a state file pando cannot use must be reported, not swallowed");
        assert!(warning.contains("version 3"), "{warning}");

        let err = rm(&fx.paths, &name, true, false).unwrap_err();
        assert_eq!(
            warning,
            format!("{err:#}"),
            "the listing and rm must give the same line"
        );
    }

    // A record is keyed by basename, and the worktree it was written for can
    // be removed behind pando's back. The record that survives must not then
    // vouch for a different worktree that happens to share the name — `rm`
    // would delete a directory pando never created, without asking.
    #[test]
    fn a_stale_record_does_not_make_an_unrelated_worktree_ours() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/x", None, &noop).unwrap();
        let ours = fx.worktrees_dir().join(&name);
        git(
            &fx.root,
            &["worktree", "remove", "--force", ours.to_str().unwrap()],
        );
        assert!(
            fx.state().worktrees.contains_key(&name),
            "the record outlives the worktree git forgot"
        );

        let elsewhere = fx.root.parent().unwrap().join("elsewhere").join(&name);
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "--quiet",
                elsewhere.to_str().unwrap(),
                "feat/x",
            ],
        );

        let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
        assert_eq!(
            owned.by_name.get(&name),
            Some(&false),
            "a record for a directory that is gone must not vouch for another one"
        );
        assert!(owned.warning.is_none(), "{:?}", owned.warning);

        let err = rm(&fx.paths, &name, false, false).unwrap_err();
        assert!(format!("{err:#}").contains("--yes"), "{err:#}");
        assert!(
            elsewhere.is_dir(),
            "the adopted worktree must still be there"
        );

        rm(&fx.paths, &name, true, false).unwrap();
        assert!(!elsewhere.exists());
    }

    // A refused `rm` must change nothing at all. Unlinking the provisioned
    // files before asking git left the worktree alive and stripped of its
    // `.env`, with nothing to re-provision it.
    #[test]
    fn a_refused_rm_leaves_the_provisioned_files_alone() {
        let mut fx = fixture();
        fx.config.project.provision = vec![".env".into()];
        let name = new(&fx.paths, &fx.config, "feat/p", None, &noop).unwrap();
        let worktree = fx.worktrees_dir().join(&name);
        let env = worktree.join(".env");
        std::fs::write(worktree.join("DIRTY.txt"), "wip").unwrap();

        let err = rm(&fx.paths, &name, false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("modified or untracked"),
            "{err:#}"
        );
        assert_eq!(fx.names(), vec![name], "the worktree is still there");
        assert!(
            std::fs::symlink_metadata(&env)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
            "a refused rm must leave the provisioned symlink where it was"
        );
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "SECRET=1\n");
    }

    // Invariant 1 covers the whole repository, and a linked worktree is part
    // of it. Neither pando's home nor the directory it creates worktrees in
    // may sit inside any of them.
    #[test]
    fn write_locations_inside_the_repository_or_a_worktree_are_refused() {
        let mut fx = fixture();
        let linked = fx.root.parent().unwrap().join("linked");
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        guard_write_locations(&fx.paths, &fx.config)
            .expect("a home beside the repository is what every test uses");

        fx.config.project.worktrees_dir = Some(linked.join("nested"));
        let err = guard_write_locations(&fx.paths, &fx.config).unwrap_err();
        assert!(
            format!("{err:#}").contains("inside the worktree"),
            "{err:#}"
        );

        fx.config.project.worktrees_dir = None;
        for (home, expected) in [
            (linked.join(".pando"), "inside the worktree"),
            (fx.root.join(".pando"), "inside the repository"),
        ] {
            let paths = PandoPaths::new(&home, fx.paths.project.clone());
            let err = guard_write_locations(&paths, &fx.config).unwrap_err();
            assert!(format!("{err:#}").contains(expected), "{err:#}");
            assert!(!home.exists(), "nothing may be created for a refused home");
        }
    }

    /// Commits an ignore rule on `main` and a branch whose own committed
    /// `.gitignore` predates it, so the main checkout authorises a write the
    /// worktree would not.
    fn with_a_branch_that_does_not_ignore(fx: &Fx, rel: &str, branch: &str) {
        std::fs::write(
            fx.root.join(".gitignore"),
            format!(".env\nnode_modules/\n{rel}\n"),
        )
        .unwrap();
        git(&fx.root, &["add", ".gitignore"]);
        git(&fx.root, &["commit", "--quiet", "-m", "ignore it"]);
        std::fs::write(fx.root.join(rel), "TOKEN=1\n").unwrap();

        git(&fx.root, &["checkout", "--quiet", "-b", branch]);
        std::fs::write(fx.root.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
        git(&fx.root, &["commit", "--quiet", "-am", "older gitignore"]);
        git(&fx.root, &["checkout", "--quiet", "main"]);
    }

    // State is read before git is asked to create anything, so a state file
    // pando cannot parse refuses while there is still nothing to undo.
    #[test]
    fn a_broken_state_file_refuses_new_before_anything_is_created() {
        let fx = fixture();
        fx.paths.ensure_home().unwrap();
        std::fs::write(fx.paths.state_file(), "not json").unwrap();

        let err = new(&fx.paths, &fx.config, "feat/b", None, &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("parse state file"), "{msg}");
        assert!(fx.names().is_empty(), "no worktree may have been created");
        assert!(
            !ref_exists(&fx.root, "refs/heads/feat/b"),
            "no branch may have been created"
        );
        assert!(!fx.worktrees_dir().join("feat+b").exists());
    }

    // The worktree's own gitignore is the last word, so a refusal can happen
    // after `git worktree add` — which makes the unwind what keeps `new`
    // all-or-nothing.
    #[test]
    fn a_refusal_after_the_worktree_exists_unwinds_it() {
        let mut fx = fixture();
        with_a_branch_that_does_not_ignore(&fx, "local.pando", "legacy");
        fx.config.project.provision = vec!["local.pando".into()];

        // A forked branch is pando's own doing, so it goes too.
        let err = new(&fx.paths, &fx.config, "feat/new", Some("legacy"), &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not ignored"), "{msg}");
        assert!(
            msg.contains("removed"),
            "the error must say what it undid: {msg}"
        );
        assert!(fx.names().is_empty(), "the worktree must be gone");
        assert!(!fx.worktrees_dir().join("feat+new").exists());
        assert!(
            !ref_exists(&fx.root, "refs/heads/feat/new"),
            "a branch pando created must be deleted by the unwind"
        );
        assert!(!fx.state().worktrees.contains_key("feat+new"));

        // An existing branch was only checked out, so it survives.
        let err = new(&fx.paths, &fx.config, "legacy", None, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("not ignored"), "{err:#}");
        assert!(fx.names().is_empty());
        assert!(
            ref_exists(&fx.root, "refs/heads/legacy"),
            "a branch pando did not create must survive the unwind"
        );
    }
}
