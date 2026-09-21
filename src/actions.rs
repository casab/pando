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
use crate::services;
use crate::share_proxy;
use crate::state::{self, Phase, ProcessRecord, ShareRecord, WorktreeRecord};
use crate::template;
use crate::tunnel;
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
    for notice in sweep_orphaned_groups(&mut store)? {
        progress(&notice);
    }
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
    //
    // No ports yet: they are allocated at `start`, so a create hook that
    // names one fails here by name rather than silently rendering the
    // wrong number. Almost none do; the install step never does.
    progress("installing");
    let no_ports: BTreeMap<String, u16> = BTreeMap::new();
    let no_services: BTreeMap<String, String> = BTreeMap::new();
    let ctx = HookContext {
        name: &dir_name,
        branch: Some(branch),
        worktree: &target,
        ports: &no_ports,
        service_env: &no_services,
    };
    run_hooks(paths, config, config::HookPoint::Create, &ctx, progress)
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
///
/// It shares `logs/<worktree>/` with every process, so it is one of
/// [`crate::paths::RESERVED_LOG_SOURCES`]; a process allowed to take the
/// name would have its log truncated on every start.
pub const INSTALL_HOOK: &str = "install";

/// `[project].install` expressed as the `[[hooks]]` entry it is: a
/// create-point hook keyed on the lockfiles, because a lockfile changing
/// is what "the dependencies changed" means.
///
/// One mechanism rather than two. The only thing still special about it is
/// the warning below when a "frozen" install rewrites a lockfile.
fn install_hook(config: &Config) -> Option<config::HookConfig> {
    let install = config.project.install.as_deref()?.trim();
    if install.is_empty() {
        return None;
    }
    Some(config::HookConfig {
        name: INSTALL_HOOK.to_string(),
        after: config::HookPoint::Create,
        fingerprint: detect::LOCKFILES.iter().map(|l| l.to_string()).collect(),
        cmd: install.to_string(),
        cwd: None,
        fallback: None,
    })
}

/// Every hook that runs at one lifecycle point, in the order they run: the
/// built-in install first, then whatever `[[hooks]]` declares, in file
/// order.
fn hooks_at(config: &Config, point: config::HookPoint) -> Vec<config::HookConfig> {
    let mut out: Vec<config::HookConfig> = Vec::new();
    if point == config::HookPoint::Create
        && let Some(install) = install_hook(config)
    {
        out.push(install);
    }
    out.extend(config.hooks.iter().filter(|h| h.after == point).cloned());
    out
}

/// What a hook is given: the worktree it runs in, the ports of its roles,
/// and the environment the processes get.
pub struct HookContext<'a> {
    pub name: &'a str,
    pub branch: Option<&'a str>,
    pub worktree: &'a Path,
    pub ports: &'a BTreeMap<String, u16>,
    /// Service addresses, so a migration talks to this worktree's own
    /// database rather than the shared one.
    pub service_env: &'a BTreeMap<String, String>,
}

/// Runs every hook at one lifecycle point whose fingerprint has changed.
///
/// Deliberately not under the state lock. The recorded fingerprint is read
/// without one — the worst a race can do is run an idempotent hook twice —
/// and the lock is taken only to write the result, because a migration can
/// take a minute and nothing else should wait on it.
pub fn run_hooks(
    paths: &PandoPaths,
    config: &Config,
    point: config::HookPoint,
    ctx: &HookContext<'_>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    for hook in hooks_at(config, point) {
        run_hook(paths, config, &hook, ctx, progress)?;
    }
    Ok(())
}

fn run_hook(
    paths: &PandoPaths,
    config: &Config,
    hook: &config::HookConfig,
    ctx: &HookContext<'_>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let log_file = paths.log_file(ctx.name, &hook.name);
    let template_ctx = template::Context {
        name: ctx.name,
        branch: ctx.branch,
        worktree: ctx.worktree,
        root: paths.root(),
        project: paths.project_id(),
        ports: ctx.ports,
        default_role: None,
        log: Some(&log_file),
    };
    let cmd = template::render(&hook.cmd, &template_ctx)
        .with_context(|| format!("in the command for hook {}", hook.name))?;
    let fallback = hook
        .fallback
        .as_deref()
        .map(|f| template::render(f, &template_ctx))
        .transpose()
        .with_context(|| format!("in the fallback for hook {}", hook.name))?;
    let cwd = hook_cwd(ctx.worktree, hook, &template_ctx)?;

    // The command is part of the fingerprint, not only the files: editing
    // what a hook runs is a reason to run it again, and keying on the
    // files alone meant a corrected migration command never ran.
    let current = hooks::fingerprint(ctx.worktree, &hook.fingerprint, &cmd);
    let recorded = state::load(&paths.state_file()).ok().and_then(|store| {
        store
            .worktrees
            .get(ctx.name)
            .and_then(|r| r.hooks.get(&hook.name))
            .and_then(|h| h.fingerprint.clone())
    });
    // No fingerprint at all means nothing here can say the inputs are
    // unchanged, so the hook runs every time.
    if current.is_some() && current == recorded {
        return Ok(());
    }
    // …and a hook that *has* globs and matched nothing with them is the
    // same thing by accident: `fingerprint = ["prisma/migrations"]` instead
    // of `["prisma/migrations/**"]` is the natural typo and costs a full
    // migration on every start. Every guess is visible; so is this.
    if !hook.fingerprint.is_empty() && current.is_none() {
        progress(&matched_nothing(ctx.worktree, hook));
    }

    progress(&format!("{}: {cmd}", hook.name));
    let mut env = pando_env(paths, ctx.name, ctx.branch, ctx.worktree);
    env.extend(ctx.service_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    // What the worktree looked like before, so anything the hook leaves
    // behind can be named rather than merely counted.
    let before = porcelain_status(ctx.worktree);
    hooks::run_with_fallback(
        &log_file,
        &with_prelude(config, &cmd),
        fallback
            .as_deref()
            .map(|f| with_prelude(config, f))
            .as_deref(),
        &cwd,
        &env,
    )
    .with_context(|| hook_failed(paths, config, hook))?;

    // A hook that rewrites one of its own inputs — a "frozen" install that
    // normalises a lockfile is the classic — has to be reported, because a
    // tracked file changing under a worktree is what Invariant 1 exists to
    // prevent. And the fingerprint recorded is the one the hook *left
    // behind*, or it sees its own change and re-runs on every start.
    let after = hooks::fingerprint(ctx.worktree, &hook.fingerprint, &cmd);
    if after != current {
        progress(&changed_its_inputs(&hook.name, &cmd));
    }
    // And anything it wrote *outside* what it is keyed on, which the
    // fingerprint can say nothing about. `docs/02-principles.md`: a hook
    // that writes a new file into the worktree is misconfigured, and the
    // developer can only know that if pando says which file.
    let appeared = newly_dirty(&before, &porcelain_status(ctx.worktree));
    if !appeared.is_empty() {
        progress(&format!(
            "warning: the {} hook changed this worktree: {} — `git status` there will show \
             that, and pando never writes into your repository",
            hook.name,
            appeared.join(", ")
        ));
    }

    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // `or_insert_with`, not `get_mut`: on `start` for an adopted worktree
    // there is no record yet — `start` creates it after this returns — and
    // dropping the result on the floor would re-run on every start
    // forever. A record pando did not create is not claimed as its own.
    store
        .worktrees
        .entry(ctx.name.to_string())
        .or_insert_with(|| WorktreeRecord::new(ctx.worktree, false))
        .hooks
        .insert(
            hook.name.clone(),
            state::HookRecord {
                fingerprint: after,
                ran_at: Utc::now(),
            },
        );
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

/// Runs the pre-start probes, refusing to start when one of them fails in
/// a way it recognised.
///
/// The asymmetry is the whole design. A probe that fails with the stderr
/// it was told to watch for is a diagnosis: the start stops and the
/// developer gets the one-line fix. A probe that fails any *other* way is
/// ignored, because a check pando does not understand must never be the
/// reason a project it has never seen refuses to start.
fn run_probes(
    paths: &PandoPaths,
    config: &Config,
    ctx: &HookContext<'_>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    if config.probes.is_empty() {
        return Ok(());
    }
    let template_ctx = template::Context {
        name: ctx.name,
        branch: ctx.branch,
        worktree: ctx.worktree,
        root: paths.root(),
        project: paths.project_id(),
        ports: ctx.ports,
        default_role: None,
        log: None,
    };
    let mut env = pando_env(paths, ctx.name, ctx.branch, ctx.worktree);
    env.extend(ctx.service_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    for probe in &config.probes {
        let cmd = template::render(&probe.cmd, &template_ctx)
            .with_context(|| format!("in the command for probe {}", probe.name))?;
        let Some(stderr) = hooks::probe(&with_prelude(config, &cmd), ctx.worktree, &env)
            .with_context(|| format!("the probe {} could not be run", probe.name))?
        else {
            continue;
        };
        if !stderr.contains(&probe.match_) {
            // Ignored on purpose — and said out loud, so a probe that has
            // quietly stopped recognising anything is visible.
            progress(&format!(
                "the probe {} failed in a way it does not recognise; ignoring it",
                probe.name
            ));
            continue;
        }
        let reason = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        bail!(
            "the probe {} failed: {reason}\n  {}",
            probe.name,
            probe.hint
        );
    }
    Ok(())
}

/// `git status --porcelain --untracked-files=all` inside a worktree, one
/// entry per line.
///
/// Empty when git cannot answer. A hook that ran is not failed because the
/// check after it could not be made — the check is a warning, not a gate.
fn porcelain_status(worktree: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new("git")
        .current_dir(worktree)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The paths in `after` that were not dirty in `before`, without their
/// two-letter status prefix.
fn newly_dirty(before: &[String], after: &[String]) -> Vec<String> {
    after
        .iter()
        .filter(|line| !before.contains(line))
        .map(|line| line.get(3..).unwrap_or(line.as_str()).trim().to_string())
        .collect()
}

/// The context a failing hook carries: which entry it is, and where the one
/// edit that removes it lives.
///
/// For a hook the developer wrote, naming the file is a courtesy. For one
/// *pando* invented — the detected `migrate` this phase adds automatically
/// — it is the difference between "a wrong guess costs one edit" and a
/// mysterious failure at start time.
fn hook_failed(paths: &PandoPaths, config: &Config, hook: &config::HookConfig) -> String {
    let base = format!("the {} hook failed", hook.name);
    // The install step is synthesised from `[project].install`; there is no
    // `[[hooks]]` entry to point at.
    if hook.name == INSTALL_HOOK || !config.hooks.iter().any(|h| h.name == hook.name) {
        return base;
    }
    format!(
        "{base} — this is the [[hooks]] entry named {:?} in {}; delete it if it is wrong",
        hook.name,
        hook_source_file(paths, &hook.name).display()
    )
}

/// Which config file declares a `[[hooks]]` entry by that name. pando's own
/// first, because that is the layer that wins and the one detection writes.
fn hook_source_file(paths: &PandoPaths, name: &str) -> PathBuf {
    let home = paths.config_file();
    if declares_hook(&home, name) {
        return home;
    }
    let committed = paths.root().join("pando.toml");
    if declares_hook(&committed, name) {
        return committed;
    }
    home
}

fn declares_hook(path: &Path, name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("hooks")
        .and_then(|item| item.as_array_of_tables())
        .is_some_and(|tables| {
            tables
                .iter()
                .any(|table| table.get("name").and_then(|v| v.as_str()) == Some(name))
        })
}

/// One notice for a hook whose globs match nothing at all.
fn matched_nothing(worktree: &Path, hook: &config::HookConfig) -> String {
    let globs: Vec<String> = hook
        .fingerprint
        .iter()
        .map(|glob| format!("{glob:?}"))
        .collect();
    let mut message = format!(
        "warning: the {} hook is keyed on {}, which matches nothing in this worktree, so it \
         runs on every start",
        hook.name,
        globs.join(", ")
    );
    // The common shape of the mistake: a literal that names a directory.
    // A glob matches files, so it needs `/**` to reach into one.
    let directories: Vec<String> = hook
        .fingerprint
        .iter()
        .filter(|glob| !glob.contains(['*', '?']) && worktree.join(glob).is_dir())
        .map(|glob| format!("{glob}/**"))
        .collect();
    if !directories.is_empty() {
        message.push_str(&format!(
            " — that is a directory, and a fingerprint matches files; try {}",
            directories.join(", ")
        ));
    }
    message
}

fn changed_its_inputs(hook: &str, cmd: &str) -> String {
    if hook == INSTALL_HOOK {
        return format!(
            "warning: {cmd:?} changed a lockfile in this worktree — that command is not as \
             frozen as it looks, and `git status` there will show it"
        );
    }
    format!(
        "warning: the {hook} hook changed one of the files it is keyed on — `git status` in \
         this worktree will show it, and the hook will run again next time"
    )
}

/// A hook runs in the worktree, or in the subdirectory it names. The same
/// refusals a process's `cwd` gets: a hook that ran outside its own
/// worktree would be writing into a repository.
fn hook_cwd(
    worktree: &Path,
    hook: &config::HookConfig,
    ctx: &template::Context<'_>,
) -> Result<PathBuf> {
    let Some(relative) = hook.cwd.as_deref() else {
        return Ok(worktree.to_path_buf());
    };
    let rendered = template::render(relative, ctx)
        .with_context(|| format!("in the cwd for hook {}", hook.name))?;
    let dir = worktree.join(&rendered);
    if !dir.is_dir() {
        bail!(
            "cwd {rendered:?} does not exist in this worktree ({}) — check the cwd of hook {:?}",
            dir.display(),
            hook.name
        );
    }
    let resolved = crate::paths::resolve_for_compare(&dir);
    let owner = crate::paths::resolve_for_compare(worktree);
    if !resolved.starts_with(&owner) {
        bail!(
            "cwd {rendered:?} for hook {:?} resolves to {}, which is outside the worktree ({})",
            hook.name,
            resolved.display(),
            owner.display()
        );
    }
    Ok(dir)
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
pub fn rm(
    paths: &PandoPaths,
    name: &str,
    yes: bool,
    force: bool,
    progress: &dyn Fn(&str),
) -> Result<()> {
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
    for notice in sweep_orphaned_groups(&mut store)? {
        progress(&notice);
    }
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
    let mut projects: Vec<String> = Vec::new();
    let stopped = stop_recorded(
        &mut store,
        name,
        None,
        MissingOnly::IsAnError,
        &mut projects,
    )?;

    // The containers, the network, and the volumes, before git is asked to
    // remove anything: the record that names the compose project is about
    // to be dropped, and a `down -v` that failed after the worktree was
    // gone would leave a database nothing could ever find. Saved first, so
    // a retry of `rm` has the record it needs.
    if !projects.is_empty() {
        state::save(&paths.state_file(), &store)?;
        stop_compose_projects(paths, &projects, |compose| compose.down_with_volumes())
            .with_context(|| {
                format!("{name} was left in place; its services could not be taken down")
            })?;
    }

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
        if matches!(stopped, StopOutcome::Stopped(_)) {
            bail!(
                "git worktree remove failed: {reason} — what was running was stopped, and any \
                 public URL closed; the worktree was kept"
            );
        }
        bail!("git worktree remove failed: {reason}");
    }

    let _ = std::fs::remove_dir_all(paths.logs_dir(name));
    let _ = std::fs::remove_dir_all(paths.data_dir(name));
    // And the compose override, which is regenerated on every isolated
    // start and would otherwise outlive every worktree that ever had one.
    let _ = std::fs::remove_file(paths.compose_override_file(name));
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
    /// be sayable, or the question comes back on every start. True for the
    /// services question too, where it means "none of them".
    pub allow_none: bool,
    /// Whether the answer is a *set* of the options rather than one of
    /// them: which services this project runs private copies of.
    pub multi: bool,
    /// For a multi-select question, the options that start ticked.
    pub checked: Vec<usize>,
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
    /// Several of the options, for a question whose answer is a set: which
    /// of the compose file's services this project runs private copies of.
    Many(Vec<usize>),
    /// "This process has none of those." Only offered where a question has
    /// an empty answer that means something: the port, and the empty set
    /// at the services question.
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

/// The slots `start` fills: how many processes there are, then the dev
/// process and how it takes its port, then the schema step. `Processes`
/// first, because the answer to it decides whether the others have
/// anything left to ask.
///
/// `Services` is in the list on every start, because a project whose
/// services a rule resolved outright should have them in config — that is
/// what the shared-mode health chips read. It is in [`SILENT_UNLESS_ISOLATED`]
/// too, so on a plain start it is only ever *taken*, never *asked*:
/// "which services do you want private copies of?" is a question about a
/// mode this start is not in.
pub const START_SLOTS: [Slot; 5] = [
    Slot::Processes,
    Slot::DevCmd,
    Slot::PortEnv,
    Slot::Services,
    Slot::SchemaHook,
];

/// Slots a start that is not isolating may accept but must not ask about.
const SILENT_UNLESS_ISOLATED: [Slot; 1] = [Slot::Services];

/// Fills the dev process from detection when config has none.
pub fn resolve_process(
    paths: &PandoPaths,
    config: &Config,
    isolated: bool,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    let silent: &[Slot] = if isolated {
        &[]
    } else {
        &SILENT_UNLESS_ISOLATED
    };
    resolve_silencing(paths, config, &START_SLOTS, silent, ask, progress)
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
    resolve_silencing(paths, config, slots, &[], ask, progress)
}

/// [`resolve`], with slots that may be *taken* when the rules decided them
/// and must never be *asked* about.
///
/// One case so far: the services question is about isolation, and a start
/// that is not isolating has no business asking it — but a project whose
/// services the rules resolved outright should still get them written
/// down, because that is what shared mode reads to show whether the
/// global database is up.
pub fn resolve_silencing(
    paths: &PandoPaths,
    config: &Config,
    slots: &[Slot],
    silent: &[Slot],
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    let mut config = config.clone();
    if slots.iter().all(|slot| already_answered(*slot, &config)) {
        return Ok(config);
    }
    let signals = detect::signals(paths.root());
    // Only ever called for a compose file pando's own reader could not
    // follow — `extends:` or a top-level `include:` — so a plain project
    // costs no process spawn. `config` prints; it creates nothing.
    let program = services::docker_program(paths);
    let resolve = |file: &Path| -> Option<crate::compose::ComposeFile> {
        let dir = file.parent()?;
        services::Compose::new(
            &program,
            crate::compose::project_name(paths.project_id(), "detect"),
            vec![file.to_path_buf()],
            dir,
        )
        .config()
        .ok()
    };
    let proposals = detect::propose_with(paths.root(), &signals, Some(&resolve));
    // Asked of the config as it was loaded: once detection has written
    // `[dev].cmd`, the file is indistinguishable from one a developer
    // wrote by hand, and a `[dev]` they wrote is an answer about its ports
    // too. The only thing that changes it mid-run is an answer to the
    // shape question itself.
    let mut may_fill_dev = detect::may_fill_dev(&config);

    for slot in slots {
        if matches!(slot, Slot::DevCmd | Slot::PortEnv) && !may_fill_dev {
            continue;
        }
        // Just in time, and once: a slot the developer has already filled
        // in, by hand or by answering before, is never asked about again —
        // and a config that declares its processes has answered every
        // question about them, including the ones detection could only
        // write into `[dev]`.
        //
        // Re-read from `config` on every pass, because an earlier slot in
        // this same run may have answered a later one: taking the
        // per-app form settles the dev command and its ports with it.
        if already_answered(*slot, &config) || !detect::still_needed(*slot, &config) {
            continue;
        }
        let Some(proposal) = proposals.iter().find(|p| p.slot == *slot) else {
            continue;
        };
        if !proposal.decided && silent.contains(slot) {
            continue;
        }
        // The one slot whose answer is a set. It never takes the
        // single-candidate path below, because "these three" is not one of
        // the options — it is a subset of them.
        if slot.is_multi() {
            let (chosen, note): (Vec<detect::Candidate>, config::Note) = if proposal.decided {
                let taken: Vec<detect::Candidate> =
                    proposal.preferred_set().into_iter().cloned().collect();
                let why = taken.first().map(|c| c.why.clone()).unwrap_or_default();
                if !taken.is_empty() {
                    // Not "running private copies of": a start that is not
                    // isolating runs none, and this line is written on
                    // every start that fills the slot.
                    progress(&format!(
                        "using {} as this project's services (detected: {why})",
                        taken
                            .iter()
                            .map(|c| c.value.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ));
                }
                (taken, config::Note::Detected(why))
            } else {
                let question = question_for(proposal);
                let offered = question.options.len();
                match ask(&question)? {
                    Answer::Many(indexes) => (
                        indexes
                            .iter()
                            .map(|index| pick(proposal, *index))
                            .collect::<Result<Vec<_>>>()?,
                        config::Note::Answered,
                    ),
                    // `--yes`. Written down as what it is: a flag took the
                    // options the rules had resolved. A config that claims
                    // a human chose them is one nobody can review.
                    Answer::Auto(_) => {
                        let taken: Vec<detect::Candidate> =
                            proposal.preferred_set().into_iter().cloned().collect();
                        let note = config::Note::TookRuled {
                            taken: taken.len(),
                            offered,
                        };
                        (taken, note)
                    }
                    Answer::None => (Vec::new(), config::Note::Answered),
                    _ => bail!(
                        "{} is answered with a set of the {offered} options",
                        slot_label(*slot)
                    ),
                }
            };
            apply_service_answer(paths, &mut config, proposal, &chosen, note)?;
            continue;
        }
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
                // Only the multi-select slot has a set for an answer, and
                // it never reaches here.
                Answer::Many(_) => bail!(
                    "{} takes one of its {offered} options, not several",
                    slot_label(*slot)
                ),
                // Written down as an empty list rather than left out: "this
                // process has no ports" and "nobody has said yet" have to
                // be different states, or the question returns on every
                // start and the answer is a port nothing will ever bind.
                Answer::None if *slot == Slot::PortEnv => {
                    let (table, key) = Slot::PortEnv.key().expect("the port slot writes one key");
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
        // A slot whose answer is a whole `[[table]]` entry: appended, with
        // the note on the entry's own header rather than on each key.
        if let Some((array, entries)) = detect::array_edits(*slot, &[&candidate]) {
            config::set_detected_array_entry(paths, array, entries, note.clone())?;
            detect::apply(*slot, &candidate, &mut config);
            continue;
        }
        let edits = detect::edits(*slot, &candidate);
        if *slot == Slot::Processes {
            // A whole process table is one answer to one question, so the
            // note goes on the table's header rather than on each of its
            // five keys.
            for (table, entries) in group_by_table(edits) {
                let table: Vec<&str> = table.iter().map(String::as_str).collect();
                config::set_detected_table(paths, &table, entries, note.clone())?;
            }
        } else {
            for edit in edits {
                let table: Vec<&str> = edit.table.iter().map(String::as_str).collect();
                config::set_detected(paths, &table, &edit.key, edit.value, note.clone())?;
            }
        }
        detect::apply(*slot, &candidate, &mut config);
        if *slot == Slot::Processes {
            // The answer decided the shape. The per-app form leaves the
            // single-process slots nothing to fill; the root-script form
            // leaves them the port.
            may_fill_dev = detect::fills_one_dev_process(&config);
        }
    }
    Ok(config)
}

/// Writes the answer to the one multi-select slot: a `[[services]]` entry
/// listing the services chosen and the env keys that point at them.
///
/// An empty set is a real answer — "none of them" — and it is written down
/// as an entry with an empty `include`. An answer nothing records is asked
/// again on every start: a prompt the developer already declined, and exit
/// 3 for a script, with nowhere to put the answer but the TOML by hand.
/// `Slot::PortEnv` writes `ports = []` for exactly this reason.
fn apply_service_answer(
    paths: &PandoPaths,
    config: &mut Config,
    proposal: &detect::Proposal,
    chosen: &[detect::Candidate],
    note: config::Note,
) -> Result<()> {
    // The compose file the question was about. Without one there is
    // nothing to write an entry for, and nothing was asked either.
    let Some(file) = proposal.service_file().map(str::to_string) else {
        return Ok(());
    };
    let refs: Vec<&detect::Candidate> = chosen.iter().collect();
    let (array, entries) = detect::service_entry(&file, &refs);
    config::set_detected_array_entry(paths, array, entries, note)?;
    detect::apply_services(&file, &refs, config);
    Ok(())
}

/// Edits gathered per table, keeping the order they were produced in.
///
/// The keys of one table are written together so the note explaining them
/// can sit on the table rather than on every key.
type TableEdits = Vec<(Vec<String>, Vec<(String, toml_edit::Value)>)>;

fn group_by_table(edits: Vec<detect::Edit>) -> TableEdits {
    let mut out: TableEdits = Vec::new();
    for edit in edits {
        match out.iter_mut().find(|(table, _)| *table == edit.table) {
            Some((_, entries)) => entries.push((edit.key, edit.value)),
            None => out.push((edit.table, vec![(edit.key, edit.value)])),
        }
    }
    out
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
        // A set answer takes the options as they are; there is no command
        // to type in place of "which of these containers".
        allow_custom: !proposal.slot.is_multi(),
        allow_none: proposal.slot == Slot::PortEnv || proposal.slot.is_multi(),
        multi: proposal.slot.is_multi(),
        checked: proposal.preselected(),
    }
}

fn slot_label(slot: Slot) -> &'static str {
    match slot {
        Slot::Install => "install command",
        Slot::VersionFiles => "runtime version file",
        Slot::Processes => "process list",
        Slot::DevCmd => "dev command",
        Slot::PortEnv => "port variable",
        Slot::Services => "service list",
        Slot::SchemaHook => "schema command",
        Slot::Provision => "provision list",
    }
}

/// Whether config already says what this slot needs, from any layer.
fn already_answered(slot: Slot, config: &Config) -> bool {
    match slot {
        Slot::Install => config.project.install.is_some(),
        Slot::VersionFiles => !config.runtime.version_files.is_empty(),
        Slot::Provision => !config.project.provision.is_empty(),
        Slot::Services => !config.services.is_empty(),
        Slot::SchemaHook => !config.hooks.is_empty(),
        // Both of these now live in one place, because they are the same
        // question asked twice: has anything already said what this
        // project's processes are?
        Slot::Processes | Slot::DevCmd | Slot::PortEnv => !detect::still_needed(slot, config),
    }
}

// ---- start, stop, restart -------------------------------------------------

/// How long a process group gets to exit on its own before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// The role `share` and the browser-open key default to, and the role a
/// readiness rule watches when none is named.
const DEFAULT_READY_ROLE: &str = "web";

/// One process a start brought up, or found already up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedProcess {
    /// The process's name in config — `dev` for the `[dev]` shorthand.
    pub process: String,
    pub record: ProcessRecord,
}

/// What one `start` did to a worktree.
///
/// A worktree has as many processes as its config declares, so a start is
/// never one thing: some come up, some were already running, and the ports
/// and the URL belong to the worktree rather than to any one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartReport {
    pub worktree: String,
    /// Spawned by this call, in the order they were spawned.
    pub started: Vec<StartedProcess>,
    /// Already up, and left exactly as they were.
    pub already_running: Vec<StartedProcess>,
    /// Every role of the worktree, whatever `--only` asked for: ports are
    /// reserved for the whole worktree at once, so starting one process
    /// never moves another one's port.
    pub ports: BTreeMap<String, u16>,
    /// The `web` role's URL when any process owns that role, else the
    /// first role of the alphabetically first process that owns one — the
    /// same rule, through the same function, that `status`, `ls` and the
    /// TUI use. See [`worktree_url`].
    pub url: Option<String>,
    /// Ports this worktree owned had been taken, so it moved. Worth saying
    /// out loud: a URL the developer had bookmarked just changed.
    pub reassigned: bool,
}

impl StartReport {
    /// Every process this call has something to say about.
    pub fn processes(&self) -> impl Iterator<Item = &StartedProcess> {
        self.started.iter().chain(self.already_running.iter())
    }

    /// Nothing was spawned: everything asked for was already up.
    pub fn started_nothing(&self) -> bool {
        self.started.is_empty()
    }
}

/// The names of a list of processes, for a message.
pub fn process_names(list: &[StartedProcess]) -> String {
    list.iter()
        .map(|p| p.process.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The processes whose groups were signalled.
    Stopped(Vec<String>),
    /// Nothing was running. Not an error: `stop` is how you make sure.
    NotRunning,
}

/// Everything a start needs to know about one process before anything is
/// spawned.
///
/// Planned in full first, and spawned only afterwards: a second process
/// with an unrenderable command or a missing `cwd` must not leave the first
/// one running behind a failed start.
struct Planned {
    process: String,
    shell_cmd: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    log_file: PathBuf,
    ready_port: Option<u16>,
    ready_timeout_s: Option<u64>,
}

/// Starts a worktree's processes — every one its config declares, or the
/// one `only` names.
///
/// The shape is `new`'s: take the lock, decide, act, record, save. A
/// process that is already running is reported rather than started twice; a
/// record left over from one that died is signalled and cleared first,
/// because a dead leader does not mean a dead process group.
///
/// Every process is spawned before any of them is waited on. Dependencies
/// between them are the app's problem: a web server that needs its api up
/// first retries, as every dev server does.
pub fn start(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    isolated: bool,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    let worktree = find_worktree(paths, name)?;
    // Before anything is installed, signalled or spawned: a `--only` naming
    // a process that does not exist must have no side effects at all.
    let selection = selected_processes(config, only)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());

    // A project with nothing to isolate is not an error: `--isolated` on a
    // Go service with no compose file runs shared and says so, because the
    // flag is a wish about this project and the project has no services.
    let isolatable = !service_roles(config).is_empty();
    if isolated && !isolatable {
        progress("no services are configured for this project — starting in shared mode");
    }
    // A start that will only report what is already up must not install
    // first: `npm ci` inside a worktree whose dev server is live is a
    // surprise nobody asked for. Starting one process beside a live one is
    // not that case — it is about to run code. Read without the lock, like
    // the hook's own fingerprint: the decision it guards is "can this step
    // be skipped", and the authoritative one is made under the lock below.
    let everything_up = every_process_running(paths, name, &selection);

    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;

    // A record only vouches for the worktree it was written for; a stale
    // one at another path is replaced rather than inherited. Every group it
    // recorded is signalled first: those processes are running somewhere
    // else entirely, and dropping the record would leave nothing able to
    // find them again.
    let stale = store.worktrees.get(name).is_some_and(|record| {
        crate::paths::resolve_for_compare(&record.path)
            != crate::paths::resolve_for_compare(&canonical)
    });
    if stale {
        let groups: Vec<i32> = store.worktrees[name]
            .processes
            .values()
            .map(|p| p.pgid)
            .collect();
        for pgid in groups {
            proc::stop(pgid, STOP_GRACE)?;
        }
        store.worktrees.remove(name);
    }

    // Decided before anything is touched. A process that is alive is
    // reported and left exactly as it is; one whose leader is gone is
    // signalled before its record goes, because a dead leader is not a dead
    // process group and its child may still hold the port.
    let mut already: Vec<String> = Vec::new();
    let mut clear: Vec<(String, i32)> = Vec::new();
    if let Some(record) = store.worktrees.get(name) {
        for (process, existing) in &record.processes {
            let selected = selection.iter().any(|(n, _)| n == process);
            let live = matches!(
                existing.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            ) && proc::is_alive(existing.pid);
            if selected && live {
                already.push(process.clone());
            } else if selected || only.is_none() {
                // Selected and not live: this start replaces it. Not
                // selected, with nothing asking for a subset: a name config
                // no longer has — state a newer pando wrote, or a process
                // the developer renamed — whose live child would otherwise
                // be left holding a port nothing could find again.
                //
                // This is also where a `Failed` record goes: it survives
                // `reconcile` on purpose, so the mutation that acts on that
                // process is the one that has to clear it.
                //
                // A `--only` start leaves every other record alone. One
                // whose leader is dead is still signalled, by the sweep
                // below, before `reconcile` drops it.
                clear.push((process.clone(), existing.pgid));
            }
        }
    }
    for process in &already {
        progress(&format!("{process} is already running"));
    }
    if !clear.is_empty() {
        progress("clearing what is left of the last run");
    }
    for (_, pgid) in &clear {
        proc::stop(*pgid, STOP_GRACE)?;
    }
    if let Some(record) = store.worktrees.get_mut(name) {
        for (process, _) in &clear {
            record.processes.remove(process);
        }
    }
    // And every *other* worktree's dead-leader group, because `reconcile`
    // drops those records too — a half-dead share among them.
    for notice in sweep_orphaned_groups(&mut store)? {
        progress(&notice);
    }
    advance_before_reconcile(&mut store);
    state::reconcile(&mut store, proc::is_alive);

    let record = store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(canonical.clone(), false));
    // A whole-worktree start replaces everything that was observed; a
    // `--only` start leaves the sibling's ports alone and lets the next
    // refresh say what is really listening.
    if only.is_none() {
        record.observed_ports.clear();
    }
    // Isolation is per start and remembered per worktree: `--isolated`
    // turns it on, and a later plain `start` of the same worktree keeps
    // the services it already has rather than quietly pointing its
    // processes back at the shared database.
    let isolate = isolatable && (isolated || record.isolated);
    // Turned *on* only once the containers exist — see `remember_isolated`
    // below. Turned off here and now, because a worktree pando can no
    // longer isolate is one whose next start is a shared one, and there is
    // no container to contradict that.
    if !isolate {
        record.isolated = false;
    }
    // Service roles are reserved with the process roles, in one window, so
    // `{port:postgres}` resolves in any template and the number is the
    // same on every restart.
    let roles = worktree_roles(config, isolate);
    // Ports this worktree's own surviving processes are holding. They will
    // not pass a freeness probe, and they are not somebody else's either.
    let mut keep: Vec<u16> = record
        .processes
        .keys()
        .filter_map(|process| config.processes.get(process))
        .flat_map(|process| process.roles())
        .filter_map(|role| record.ports.get(&role).copied())
        .collect();
    // And the ports its own *containers* are holding, for exactly the same
    // reason. Without these, a second isolated start of a running worktree
    // reads its own database as somebody else's listener, decides the
    // window was taken, and moves every port — web included — leaving the
    // live application pointed at ports nothing is on.
    if isolate {
        keep.extend(record.services.iter().filter_map(|service| service.port));
    }

    let assignment = ports::assign_keeping(paths, &mut store, name, &roles, &keep)?;

    // Which process owns which role, for the whole worktree and whatever
    // `--only` asked for, recorded beside the ports themselves. The URL
    // rule has to be answerable from the record alone — `status` and the
    // TUI never see config — and it has to give the same answer after a
    // stop as `start` gave, so this outlives the processes exactly as the
    // ports do.
    let owners: BTreeMap<String, Vec<String>> = config
        .processes
        .iter()
        .map(|(process, config)| (process.clone(), config.roles()))
        .filter(|(_, roles)| !roles.is_empty())
        .collect();
    if let Some(record) = store.worktrees.get_mut(name) {
        record.roles = owners;
        if isolate {
            record.services = planned_services(config, paths, name, &assignment.ports, record);
        }
    }
    // Written down before anything is brought up: a start that fails
    // halfway must still leave `rm` able to name the compose project and
    // take its volumes with it.
    state::save(&paths.state_file(), &store)?;
    // And unlocked from here to the spawn. An install takes minutes, a
    // database takes seconds to become ready, and a migration takes as
    // long as it takes; holding the state lock through any of them would
    // freeze `pando ls` and the TUI's tick.
    drop(_lock);

    // Everything the app is told about where its services are. Computed
    // from the allocated ports alone, so a hook that runs before the
    // containers exist sees exactly what the processes will.
    let service_env = if isolate {
        resolve_service_env(config, &canonical, &assignment.ports)?
    } else {
        BTreeMap::new()
    };

    // The lifecycle in order: create (which is where the install step
    // lives), install, the services coming up, then services. Every hook
    // is gated by its own fingerprint, so a start that changes nothing
    // runs none of them.
    //
    // A worktree that is already running every process it was asked for
    // is not starting anything, so nothing is re-run for it either: `npm
    // ci` inside a live worktree is a surprise nobody asked for.
    let hook_ctx = HookContext {
        name,
        branch: worktree.branch.as_deref(),
        worktree: &canonical,
        ports: &assignment.ports,
        service_env: &service_env,
    };
    if !everything_up {
        run_hooks(
            paths,
            config,
            config::HookPoint::Create,
            &hook_ctx,
            progress,
        )?;
        run_hooks(
            paths,
            config,
            config::HookPoint::Install,
            &hook_ctx,
            progress,
        )?;
    }

    if isolate {
        bring_up_services(paths, config, name, &canonical, &assignment.ports, progress)?;
        remember_isolated(paths, name)?;
    }

    if !everything_up {
        run_hooks(
            paths,
            config,
            config::HookPoint::Services,
            &hook_ctx,
            progress,
        )?;
        // The last gate before anything is spawned: a probe that
        // recognises the failure stops the start and says how to fix it,
        // rather than letting the dev server die of it thirty seconds
        // later with the reason buried in a log.
        run_probes(paths, config, &hook_ctx, progress)?;
    }

    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // The record this call left behind, unless something removed the
    // worktree while the services were coming up.
    store
        .worktrees
        .entry(name.to_string())
        .or_insert_with(|| WorktreeRecord::new(canonical.clone(), false));

    let mut planned: Vec<Planned> = Vec::new();
    for (process_name, process) in &selection {
        if already.contains(process_name) {
            continue;
        }
        let process_roles = process.roles();
        let ready_role = ready_role(process, &process_roles)
            .with_context(|| format!("in process {process_name}"))?;
        let ready_port = ready_role
            .as_deref()
            .and_then(|r| assignment.ports.get(r))
            .copied();
        let log_file = paths.log_file(name, process_name);
        let ctx = template::Context {
            name,
            branch: worktree.branch.as_deref(),
            worktree: &canonical,
            root: paths.root(),
            project: paths.project_id(),
            // Every role of the worktree, not only this process's own:
            // `{port:api}` inside the web process's env is how one process
            // is told where another one is listening.
            ports: &assignment.ports,
            default_role: ready_role.as_deref(),
            log: Some(&log_file),
        };
        let cmd = template::render(&process.cmd, &ctx)
            .with_context(|| format!("in the command for process {process_name}"))?;
        let cwd = process_cwd(&canonical, process_name, process, &ctx)?;
        let env = process_env(paths, name, &worktree, process, &service_env, &ctx)?;
        planned.push(Planned {
            process: process_name.clone(),
            shell_cmd: with_prelude(config, &cmd),
            cwd,
            env,
            log_file,
            ready_port,
            ready_timeout_s: process.ready.as_ref().and_then(|r| r.timeout_s),
        });
    }

    let mut started: Vec<StartedProcess> = Vec::new();
    let mut failure: Option<anyhow::Error> = None;
    for plan in planned {
        progress(&format!("starting {}", plan.process));
        // Truncated, not appended: the classifier reads the tail of this
        // file to explain a failure, and the closing lines of the
        // *previous* run would be a confident wrong answer.
        let spawned = reset_log(&plan.log_file).and_then(|()| {
            proc::spawn_detached(SpawnOptions {
                shell_cmd: &plan.shell_cmd,
                cwd: &plan.cwd,
                log_file: &plan.log_file,
                env: &plan.env,
            })
        });
        let spawn = match spawned {
            Ok(spawn) => spawn,
            // Whatever came up before this one is already running and
            // already in the map. The state is saved below either way, so
            // the failure never leaves a process nothing can stop.
            Err(e) => {
                failure = Some(e.context(format!("starting process {}", plan.process)));
                break;
            }
        };
        let now = Utc::now();
        let record = ProcessRecord {
            pid: spawn.pid,
            pgid: spawn.pgid,
            started_at: now,
            log_path: plan.log_file,
            ready_port: plan.ready_port,
            ready_timeout_s: plan.ready_timeout_s,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Starting { since: now },
        };
        store
            .worktrees
            .get_mut(name)
            .expect("the record was just inserted")
            .processes
            .insert(plan.process.clone(), record.clone());
        started.push(StartedProcess {
            process: plan.process,
            record,
        });
    }
    state::save(&paths.state_file(), &store)?;
    if let Some(e) = failure {
        return Err(e);
    }

    let already_running: Vec<StartedProcess> = already
        .iter()
        .filter_map(|process| {
            let record = store.worktrees.get(name)?.processes.get(process)?;
            Some(StartedProcess {
                process: process.clone(),
                record: record.clone(),
            })
        })
        .collect();
    // The last point, and the only one that runs with the processes up.
    // Outside the lock, because a `dev` hook is a command like any other
    // and holding the lock through it would freeze the TUI's tick.
    drop(_lock);
    if !everything_up {
        run_hooks(paths, config, config::HookPoint::Dev, &hook_ctx, progress)?;
    }
    // From the record, through the one function every read path uses, so
    // that `pando start` and a `pando status` a second later cannot
    // disagree about the URL of the same worktree.
    let url = store.worktrees.get(name).and_then(worktree_url);
    Ok(StartReport {
        worktree: name.to_string(),
        started,
        already_running,
        url,
        ports: assignment.ports,
        reassigned: assignment.reassigned,
    })
}

/// Whether every process a start was asked for is already up. Best effort
/// and lock-free: every caller re-decides under the lock.
fn every_process_running(
    paths: &PandoPaths,
    name: &str,
    selection: &[(String, &ProcessConfig)],
) -> bool {
    let Ok(store) = state::load(&paths.state_file()) else {
        return false;
    };
    let Some(record) = store.worktrees.get(name) else {
        return false;
    };
    selection.iter().all(|(process, _)| {
        record.processes.get(process).is_some_and(|p| {
            matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. })
                && proc::is_alive(p.pid)
        })
    })
}

/// What an `--only` that names nothing the worktree is running means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingOnly {
    /// An error naming what the worktree *is* running. `stop` cannot see
    /// config — it has to work when `pando.toml` is broken — so the record
    /// is the only thing it can check a name against, and "not running"
    /// would read as "nothing to do" for a typo.
    IsAnError,
    /// Nothing to do. `restart` has already checked the name against
    /// config, and bringing a process that is down back up is exactly what
    /// `restart --only` is for.
    IsNothingToDo,
}

/// Stops a worktree's processes — every one it is running, or the one
/// `only` names. The worktree, its ports, and its logs survive.
pub fn stop(
    paths: &PandoPaths,
    name: &str,
    only: Option<&str>,
    progress: &dyn Fn(&str),
) -> Result<StopOutcome> {
    stop_missing(paths, name, only, MissingOnly::IsAnError, progress)
}

fn stop_missing(
    paths: &PandoPaths,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    progress: &dyn Fn(&str),
) -> Result<StopOutcome> {
    paths.ensure_home()?;
    let mut projects: Vec<String> = Vec::new();
    let outcome = {
        let _lock = state::lock(&paths.lock_file())?;
        let mut store = state::load(&paths.state_file())?;
        let outcome = stop_recorded(&mut store, name, only, missing, &mut projects)?;
        // `reconcile` drops dead-leader records for every worktree in the
        // project, not only this one, so every one is signalled first — and
        // a sibling's half-dead share along with them.
        for notice in sweep_orphaned_groups(&mut store)? {
            progress(&notice);
        }
        advance_before_reconcile(&mut store);
        state::reconcile(&mut store, proc::is_alive);
        state::save(&paths.state_file(), &store)?;
        outcome
    };
    // Outside the lock: `docker compose stop` takes as long as the
    // containers take to shut down, and the records that name the project
    // are already saved, so a failure here is recoverable by running
    // `stop` again.
    stop_compose_projects(paths, &projects, |compose| compose.stop())?;
    Ok(outcome)
}

/// Runs one compose verb against every project a worktree owns, reporting
/// the failures together.
fn stop_compose_projects(
    paths: &PandoPaths,
    projects: &[String],
    run: impl Fn(&services::Compose) -> Result<()>,
) -> Result<()> {
    if projects.is_empty() {
        return Ok(());
    }
    let program = services::docker_program(paths);
    let mut failures = Vec::new();
    for project in projects {
        let compose = services::Compose::by_project(&program, project.as_str());
        if let Err(e) = run(&compose) {
            failures.push(format!("{project}: {e:#}"));
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    bail!("could not reach the services of {}", failures.join("; "))
}

/// Stops every worktree pando has a process for, returning their names.
pub fn stop_all(paths: &PandoPaths, progress: &dyn Fn(&str)) -> Result<Vec<String>> {
    stop_all_with(paths, |pgid| proc::stop(pgid, STOP_GRACE), progress)
}

/// [`stop_all`] with the signal injected, so a test can drive the path
/// where a group refuses to die without needing one that really does.
pub fn stop_all_with(
    paths: &PandoPaths,
    stop: impl Fn(i32) -> Result<()>,
    progress: &dyn Fn(&str),
) -> Result<Vec<String>> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    // Services as well as processes: a worktree whose dev server crashed
    // still has a database up, and `stop` with no name is how you make
    // sure nothing of pando's is left running.
    let names: Vec<String> = store
        .worktrees
        .iter()
        .filter(|(_, r)| !r.processes.is_empty() || !r.services.is_empty())
        .map(|(name, _)| name.clone())
        .collect();
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    for name in names {
        // One worktree that will not die must not leave the rest running —
        // and must not lose its record either. The failures are collected
        // and reported once every other group has been signalled.
        match stop_recorded_with(
            &mut store,
            &name,
            None,
            MissingOnly::IsAnError,
            &stop,
            &mut projects,
        ) {
            Ok(StopOutcome::Stopped(_)) => stopped.push(name),
            Ok(StopOutcome::NotRunning) => {}
            Err(e) => failures.push(format!("stopping {name}: {e:#}")),
        }
    }
    // Nothing is dropped while a group is still unaccounted for: the pgid
    // in that record is the only way back to it.
    let mut sweep_failed = None;
    if failures.is_empty() {
        match sweep_orphaned_groups_with(&mut store, &stop) {
            Ok(notices) => {
                for notice in notices {
                    progress(&notice);
                }
                advance_before_reconcile(&mut store);
                state::reconcile(&mut store, proc::is_alive);
            }
            Err(e) => sweep_failed = Some(e),
        }
    }
    // Saved either way, so the groups that *were* signalled do not come
    // back as phantom records on the next read.
    state::save(&paths.state_file(), &store)?;
    drop(_lock);
    if !failures.is_empty() {
        bail!("{}", failures.join("; "));
    }
    if let Some(e) = sweep_failed {
        return Err(e);
    }
    stop_compose_projects(paths, &projects, |compose| compose.stop())?;
    Ok(stopped)
}

/// Signals the process groups recorded for `name` and drops their records.
/// The caller holds the lock and saves.
///
/// The signal is unconditional, Failed records included. pando is not the
/// process's parent by then, so "failed" only ever meant "its leader is
/// gone" — the group can still be serving. Clearing them is this path's job
/// too: a `Failed` record survives `reconcile` on purpose, and a stop of
/// the process it belongs to is one of the three things that ends it (the
/// others being a start of it and an `rm` of its worktree).
fn stop_recorded(
    store: &mut state::State,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    services_to_stop: &mut Vec<String>,
) -> Result<StopOutcome> {
    stop_recorded_with(
        store,
        name,
        only,
        missing,
        |pgid| proc::stop(pgid, STOP_GRACE),
        services_to_stop,
    )
}

/// What `--only <name>` gets when the worktree is not running that
/// process: an error naming what it *is* running, because "not running"
/// would read as "nothing to do" for a typo — and a typo must never be
/// answered by taking the database down.
fn missing_only(
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    record: &WorktreeRecord,
) -> Result<StopOutcome> {
    if missing == MissingOnly::IsNothingToDo {
        return Ok(StopOutcome::NotRunning);
    }
    let running: Vec<&str> = record.processes.keys().map(String::as_str).collect();
    bail!(
        "{name} is not running a process named {:?} — it is running: {}",
        only.unwrap_or_default(),
        if running.is_empty() {
            "nothing".to_string()
        } else {
            running.join(", ")
        }
    )
}

fn stop_recorded_with(
    store: &mut state::State,
    name: &str,
    only: Option<&str>,
    missing: MissingOnly,
    stop: impl Fn(i32) -> Result<()>,
    services_to_stop: &mut Vec<String>,
) -> Result<StopOutcome> {
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(StopOutcome::NotRunning);
    };
    // A worktree whose processes are all down may still have containers
    // up: it was started isolated and then every process crashed. `stop`
    // is how you make sure, so the services are taken down either way —
    // but only when nothing asked for a subset. `--only dev` is about one
    // process, and the database its siblings use is not that process; a
    // name the worktree is not running is the same error it is when
    // something *is* running, not a silent whole-worktree stop.
    if record.processes.is_empty() {
        if only.is_some() {
            return missing_only(name, only, missing, record);
        }
        let mut failures = stop_service_pumps(record, &stop);
        // A worktree whose every process crashed can still be shared: the
        // tunnel outlives them, and a public URL onto nothing is the worst
        // of both worlds.
        let was_shared = take_share_down(record, &stop, &mut failures);
        let projects = compose_projects(record);
        if !failures.is_empty() {
            bail!("{name}: {}", failures.join("; "));
        }
        if projects.is_empty() && !was_shared {
            return Ok(StopOutcome::NotRunning);
        }
        services_to_stop.extend(projects);
        return Ok(StopOutcome::Stopped(Vec::new()));
    }
    let groups: Vec<(String, i32)> = record
        .processes
        .iter()
        .filter(|(process, _)| only.is_none_or(|wanted| process.as_str() == wanted))
        .map(|(process, p)| (process.clone(), p.pgid))
        .collect();
    if groups.is_empty() {
        return missing_only(name, only, missing, record);
    }
    let mut stopped = Vec::new();
    let mut failures = Vec::new();
    for (process, pgid) in groups {
        // Signal first, drop second. A record cleared for a group that was
        // never signalled is a process nothing can find again.
        match stop(pgid) {
            Ok(()) => {
                record.processes.remove(&process);
                stopped.push(process);
            }
            Err(e) => failures.push(format!("{process} (group {pgid}): {e:#}")),
        }
    }
    if record.processes.is_empty() {
        record.observed_ports.clear();
    }
    // A worktree-wide stop takes its services with it; `--only` is about
    // one process and leaves the database its siblings are still using.
    if only.is_none() {
        failures.extend(stop_service_pumps(record, &stop));
        services_to_stop.extend(compose_projects(record));
    }
    // And the public URL, once nothing is left for it to point at. A
    // `--only` stop of one process of several leaves the share up, because
    // its siblings are still serving; a `--only` stop of the last one does
    // not, because a tunnel onto nothing is worse than no tunnel.
    let still_serving = record
        .processes
        .values()
        .any(|p| matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }));
    if only.is_none() || !still_serving {
        take_share_down(record, &stop, &mut failures);
    }
    if !failures.is_empty() {
        bail!("{name}: {}", failures.join("; "));
    }
    Ok(StopOutcome::Stopped(stopped))
}

/// Signals both halves of a worktree's share and drops the record, as part
/// of stopping it. Returns whether there was one.
///
/// Signal first, drop second, exactly as the process records are handled: a
/// share record cleared for a tunnel that was never signalled is a
/// cloudflared nothing can find again.
fn take_share_down(
    record: &mut WorktreeRecord,
    stop: &impl Fn(i32) -> Result<()>,
    failures: &mut Vec<String>,
) -> bool {
    let Some(share) = record.share.clone() else {
        return false;
    };
    match tunnel::stop_share_with(&share, stop) {
        Ok(()) => {
            record.share = None;
            true
        }
        Err(e) => {
            failures.push(format!(
                "its share (tunnel group {}): {e:#}",
                share.tunnel_pgid
            ));
            true
        }
    }
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
/// A half-dead share is the same failure with a public URL attached, so it
/// is swept here too: [`sweep_dead_shares`] is the only thing that can
/// signal one, and `reconcile` would otherwise drop the record holding the
/// surviving half's pgid. Its notices come back to the caller, which is the
/// only place that knows where to print them.
///
/// One group that will not die does not stop the sweep: the rest are still
/// signalled and the failures are reported together. A caller that gets an
/// error must not go on to drop records.
fn sweep_orphaned_groups(store: &mut state::State) -> Result<Vec<String>> {
    sweep_orphaned_groups_with(store, |pgid| proc::stop(pgid, STOP_GRACE))
}

/// [`sweep_orphaned_groups`] with the signal injected, so a test can watch
/// which groups it decides to signal without needing real ones.
fn sweep_orphaned_groups_with(
    store: &mut state::State,
    stop: impl Fn(i32) -> Result<()>,
) -> Result<Vec<String>> {
    // First, because a share is the one record whose survivor is a public
    // door: a tunnel nobody can name again is worse than a dev server
    // nobody can name again.
    let notices = sweep_dead_shares_with(store, proc::is_alive, &stop);
    let mut failures = Vec::new();
    for (name, record) in &mut store.worktrees {
        for (process, p) in &mut record.processes {
            // Once, not on every mutation. A dead leader is not a dead
            // group, so the group is signalled — but a `Failed` record now
            // survives `reconcile` until its own worktree is started,
            // stopped or removed (each of which signals and clears it on
            // its own path), and re-sending SIGTERM/SIGKILL to that pgid
            // on every later mutation in the project is how a pid that has
            // since wrapped around onto an unrelated session leader gets
            // killed. One signal per record bounds that to the window
            // between the leader dying and the first mutation after it.
            if p.swept || proc::is_alive(p.pid) {
                continue;
            }
            match stop(p.pgid) {
                // Recorded only once the signal actually went out: a group
                // that could not be signalled has to be tried again.
                Ok(()) => p.swept = true,
                Err(e) => failures.push(format!("{name}/{process} (group {}): {e:#}", p.pgid)),
            }
        }
        // A log pump is a process group like any other, and `reconcile`
        // forgets its pid the moment its leader dies — so it is signalled
        // here first, or a `docker compose logs -f` whose leader exited
        // keeps a child attached to the daemon with nothing able to name
        // it again.
        for service in record.services.iter_mut() {
            let (Some(pid), Some(pgid)) = (service.pid, service.pgid) else {
                continue;
            };
            if proc::is_alive(pid) {
                continue;
            }
            match stop(pgid) {
                Ok(()) => {
                    service.pid = None;
                    service.pgid = None;
                }
                Err(e) => failures.push(format!(
                    "{name}/{} log pump (group {pgid}): {e:#}",
                    service.name
                )),
            }
        }
    }
    if failures.is_empty() {
        return Ok(notices);
    }
    bail!(
        "could not signal {} process group(s) before dropping their records: {}",
        failures.len(),
        failures.join("; ")
    )
}

// ---- share ----------------------------------------------------------------

/// How long `[share].auth_cmd` may take before the share gives up on it.
///
/// A script that waits on something that never comes would otherwise hold
/// a share open forever, and in the TUI that is a pending slot nothing can
/// clear.
const AUTH_CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// How `auth_cmd` is told which port the proxy will listen on, in case it
/// wants to mint a session scoped to it.
pub const ENV_SHARE_PORT: &str = "PANDO_SHARE_PORT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareOutcome {
    pub name: String,
    pub public_url: String,
    /// Whether a proxy is injecting a header in front of the application.
    pub pre_authed: bool,
    /// Whether the worktree was already shared when this was asked. A
    /// second `share` prints the URL rather than opening a second tunnel.
    pub already: bool,
}

/// Publishes a running worktree at a public URL.
///
/// The lock is held for the refusals and for recording the result, and
/// dropped for everything slow in between — the auth command, the proxy,
/// and a tunnel that takes up to thirty seconds to publish. That window is
/// why the second half re-checks: a `stop` in the meantime must not leave a
/// tunnel open onto nothing with no record of it.
pub fn share(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    progress: &dyn Fn(&str),
) -> Result<ShareOutcome> {
    let provider = tunnel::provider_for(config.share.provider.as_deref())?;
    share_with(
        paths,
        config,
        name,
        provider.as_ref(),
        &|paths, name, listen, upstream, cookie| {
            share_proxy::spawn(paths, name, listen, upstream, cookie)
        },
        progress,
    )
}

/// How a proxy is started. Injected so a test can drive the path where a
/// tunnel fails with a proxy already running: the real one re-execs the
/// running binary, which inside a library test is the test harness.
pub type SpawnProxy<'a> =
    &'a dyn Fn(&PandoPaths, &str, u16, u16, &str) -> Result<share_proxy::ProxySpawn>;

/// [`share`] with the provider and the proxy injected, so a test can drive
/// a provider that is missing, and one whose tunnel fails after a proxy is
/// already running.
pub fn share_with(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    provider: &dyn tunnel::Provider,
    spawn_proxy: SpawnProxy<'_>,
    progress: &dyn Fn(&str),
) -> Result<ShareOutcome> {
    paths.ensure_home()?;
    let worktree = find_worktree(paths, name)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());

    // Every refusal first, and the proxy's port, under the lock.
    let (target_port, share_port) = {
        let _lock = state::lock(&paths.lock_file())?;
        let mut store = state::load(&paths.state_file())?;
        for notice in sweep_orphaned_groups(&mut store)? {
            progress(&notice);
        }
        advance_before_reconcile(&mut store);
        state::reconcile(&mut store, proc::is_alive);

        let record = store
            .worktrees
            .get(name)
            .with_context(|| format!("pando has no record of {name} — start it first"))?;
        if let Some(existing) = &record.share {
            return Ok(ShareOutcome {
                name: name.to_string(),
                public_url: existing.public_url.clone(),
                pre_authed: existing.proxy_pid.is_some(),
                already: true,
            });
        }
        let target_port = share_target_port(name, record)?;
        // Only when something will actually listen on it. A worktree that
        // is shared without an auth command needs no proxy and no port.
        let share_port = match config.share.auth_cmd {
            Some(_) => Some(ports::assign_share_port(paths, &mut store, name)?),
            None => None,
        };
        state::save(&paths.state_file(), &store)?;
        (target_port, share_port)
    };

    // Only now: a worktree that was never going to be shared must not be
    // told to install anything.
    provider.ensure_present(paths)?;

    // The cookie before anything is spawned, so a script that fails leaves
    // nothing behind to clean up.
    let cookie = match config.share.auth_cmd.as_deref() {
        Some(cmd) => {
            progress("running the auth command");
            Some(run_auth_cmd(
                paths,
                config,
                name,
                &worktree,
                &canonical,
                cmd,
                share_port.unwrap_or(target_port),
            )?)
        }
        None => None,
    };

    let proxy = match (&cookie, share_port) {
        (Some(cookie), Some(port)) => {
            progress("starting the share proxy");
            Some(spawn_proxy(paths, name, port, target_port, cookie)?)
        }
        _ => None,
    };
    let upstream = proxy.as_ref().map_or(target_port, |p| p.listen_port);

    progress(&format!("opening a {} tunnel", provider.name()));
    let spawn = match provider.start(paths, name, upstream) {
        Ok(spawn) => spawn,
        Err(e) => {
            // Nothing has recorded the proxy yet, so this is the last
            // moment anything knows its process group.
            if let Some(proxy) = &proxy {
                let _ = proc::stop(proxy.pgid, STOP_GRACE);
            }
            return Err(e);
        }
    };

    let record = ShareRecord {
        tunnel_pid: spawn.pid,
        tunnel_pgid: spawn.pgid,
        public_url: spawn.public_url.clone(),
        local_port: target_port,
        started_at: Utc::now(),
        log_path: spawn.log_path,
        proxy_pid: proxy.as_ref().map(|p| p.pid),
        proxy_pgid: proxy.as_ref().map(|p| p.pgid),
        proxy_port: proxy.as_ref().map(|p| p.listen_port),
    };

    // The worktree may have been stopped, removed, or shared by somebody
    // else while the tunnel was coming up. Anything that was spawned goes
    // down here rather than becoming a process with no record.
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(existing_record) = store.worktrees.get(name) else {
        let _ = tunnel::stop_share(&record);
        bail!("{name} was removed while its tunnel was starting; the tunnel was closed again");
    };
    if let Some(won) = &existing_record.share {
        let public_url = won.public_url.clone();
        let pre_authed = won.proxy_pid.is_some();
        let _ = tunnel::stop_share(&record);
        return Ok(ShareOutcome {
            name: name.to_string(),
            public_url,
            pre_authed,
            already: true,
        });
    }
    if let Err(e) = share_target_port(name, existing_record) {
        let _ = tunnel::stop_share(&record);
        return Err(e.context(format!(
            "{name} stopped while its tunnel was starting; the tunnel was closed again"
        )));
    }
    store.worktrees.get_mut(name).expect("just read").share = Some(record.clone());
    if let Err(e) = state::save(&paths.state_file(), &store) {
        // A tunnel nothing has a record of is a tunnel nothing can close.
        let _ = tunnel::stop_share(&record);
        return Err(e);
    }
    Ok(ShareOutcome {
        name: name.to_string(),
        public_url: record.public_url,
        pre_authed: proxy.is_some(),
        already: false,
    })
}

/// Takes a worktree's public URL down.
pub fn unshare(paths: &PandoPaths, name: &str) -> Result<()> {
    paths.ensure_home()?;
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let record = store
        .worktrees
        .get_mut(name)
        .with_context(|| format!("pando has no record of {name}"))?;
    let Some(share) = record.share.take() else {
        bail!("{name} is not shared");
    };
    if let Err(e) = tunnel::stop_share(&share) {
        // Still alive, so the record goes back: state has to match what is
        // really running, or a retry has no pgid to signal.
        record.share = Some(share);
        state::save(&paths.state_file(), &store)?;
        return Err(e.context(format!("could not take {name}'s share down")));
    }
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

/// Clears a share whose tunnel or proxy has died, signalling whatever is
/// left of it *before* the record that names it is dropped.
///
/// The only place that signals. `state::reconcile` and
/// `state::advance_phases` also drop a dead share, and neither can signal
/// anything — `state` knows nothing about processes — so this runs first on
/// every path that reaches them, and they find the record already gone.
///
/// A share is only useful while both halves live: a tunnel whose proxy died
/// serves the wrong thing, and a proxy whose tunnel died is unreachable.
/// Either way both groups are signalled, because a dead leader is not a
/// dead group.
fn sweep_dead_shares(store: &mut state::State) -> Vec<String> {
    sweep_dead_shares_with(store, proc::is_alive, |pgid| proc::stop(pgid, STOP_GRACE))
}

/// [`sweep_dead_shares`] with liveness and the signal injected, so a test
/// can drive it without real process groups.
fn sweep_dead_shares_with(
    store: &mut state::State,
    is_alive: impl Fn(u32) -> bool,
    stop: impl Fn(i32) -> Result<()>,
) -> Vec<String> {
    let mut notices = Vec::new();
    for (name, record) in store.worktrees.iter_mut() {
        let Some(share) = record.share.clone() else {
            continue;
        };
        let tunnel_dead = !is_alive(share.tunnel_pid);
        let proxy_dead = share.proxy_pid.is_some_and(|pid| !is_alive(pid));
        if !tunnel_dead && !proxy_dead {
            continue;
        }
        let half = if tunnel_dead { "tunnel" } else { "proxy" };
        match tunnel::stop_share_with(&share, &stop) {
            Ok(()) => {
                record.share = None;
                notices.push(format!(
                    "{name}: the share's {half} exited, so the public URL is closed"
                ));
            }
            // Not cleared: the record holds the only pgid anything can use
            // to try again.
            Err(e) => notices.push(format!(
                "{name}: the share's {half} exited and the rest of it would not stop ({e:#}) — \
                 `pando unshare {name}` to try again"
            )),
        }
    }
    notices
}

/// The port a share points at: the one the worktree's own URL uses, and
/// only while the process that owns it is running.
///
/// The same rule and the same words as the TUI's open key, because
/// "shareable" and "openable" have to mean the same thing.
fn share_target_port(name: &str, record: &WorktreeRecord) -> Result<u16> {
    let Some(role) = url_role(record) else {
        bail!("{name} has no port yet — start it first");
    };
    let Some(assigned) = record.ports.get(&role).copied() else {
        bail!("{name} has no port yet — start it first");
    };
    let owner = record
        .roles
        .iter()
        .find(|(_, roles)| roles.contains(&role))
        .map(|(process, _)| process.clone());
    let running = match &owner {
        Some(process) => record
            .processes
            .get(process)
            .is_some_and(|p| matches!(p.phase, Phase::Running { .. })),
        // A record written before pando tracked who owns what: anything up
        // is as much as it can say.
        None => record
            .processes
            .values()
            .any(|p| matches!(p.phase, Phase::Running { .. })),
    };
    if !running {
        bail!("{name} is not running — start it first, then share it");
    }
    // What is really serving, not what pando asked for.
    Ok(observed_port_for_role(record, &role).unwrap_or(assigned))
}

/// Runs `[share].auth_cmd` and returns the `Cookie` header value it printed.
///
/// It runs in the worktree, with the same environment the processes get
/// plus the proxy's port, so a script can mint a session against the very
/// database the application is using.
fn run_auth_cmd(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Worktree,
    canonical: &Path,
    cmd: &str,
    share_port: u16,
) -> Result<String> {
    let mut env = pando_env(paths, name, worktree.branch.as_deref(), canonical);
    // Best effort: a worktree with no ports never reaches here, and a
    // template that will not render is not a reason to refuse a share the
    // script may not even need it for.
    if let Ok(resolved) = resolved_env(paths, config, name) {
        env.extend(resolved);
    }
    env.push((ENV_SHARE_PORT.to_string(), share_port.to_string()));

    let captured = proc::run_captured(
        &with_prelude(config, cmd),
        canonical,
        &env,
        AUTH_CMD_TIMEOUT,
    )
    .with_context(|| format!("the [share].auth_cmd of {name}"))?;
    if !captured.success() {
        let reason = match captured.last_stderr_line() {
            Some(line) => format!(" — {line}"),
            None => String::new(),
        };
        bail!(
            "[share].auth_cmd exited {}{reason} — it runs in {}, and its stdout is the Cookie \
             header pando injects",
            captured.code.unwrap_or(-1),
            canonical.display()
        );
    }
    let cookie = captured.stdout.trim().to_string();
    if cookie.is_empty() {
        bail!(
            "[share].auth_cmd printed nothing — its stdout is the Cookie header value pando \
             injects, so an empty one has nothing to inject"
        );
    }
    // A header is one line. A value carrying a newline would let a script
    // append headers of its own to every proxied request.
    if cookie.contains(|c: char| c.is_control()) {
        bail!(
            "[share].auth_cmd printed a value with a control character in it — a Cookie header \
             is a single line"
        );
    }
    Ok(cookie)
}

/// Stop, then start. The ports come back from the record `stop` left
/// behind, so a restart keeps the URL — and `--only` restarts one process
/// while the rest keep serving on the ports they already have.
pub fn restart(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    isolated: bool,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    // Against config, and before anything is signalled: `stop` can only
    // check a name against what is running, so a typo at `--only` used to
    // get an error about the record — a different message depending on
    // unrelated state, and never the one that lists the names config
    // declares.
    selected_processes(config, only)?;
    // And a name config does know, whose process is simply not up, is a
    // no-op to stop rather than a refusal. Bringing a stopped process back
    // is the one thing `restart --only` exists for, and it used to be the
    // one thing it could not do — but only while a sibling was still
    // running, which made the failure look random.
    stop_missing(paths, name, only, MissingOnly::IsNothingToDo, progress)?;
    start(paths, config, name, only, isolated, progress)
}

/// The processes a start, stop or restart acts on: every one config
/// declares, or the one `only` names.
///
/// Alphabetically by process name, which is the order they are spawned in:
/// nothing in the loader preserves the file's own order.
fn selected_processes<'a>(
    config: &'a Config,
    only: Option<&str>,
) -> Result<Vec<(String, &'a ProcessConfig)>> {
    if config.processes.is_empty() {
        bail!("no processes configured; add [dev] to pando.toml");
    }
    let chosen: Vec<(String, &ProcessConfig)> = match only {
        Some(wanted) => {
            let process = config.processes.get(wanted).with_context(|| {
                let known: Vec<&str> = config.processes.keys().map(String::as_str).collect();
                format!(
                    "no process named {wanted:?} in pando.toml — it configures: {}",
                    known.join(", ")
                )
            })?;
            vec![(wanted.to_string(), process)]
        }
        None => config
            .processes
            .iter()
            .map(|(name, process)| (name.clone(), process))
            .collect(),
    };
    // `cmd` is optional so that a half-written process table does not take
    // every other command down with it; this is where it has to be there.
    // Refused before anything starts: half a worktree is worse than none.
    for (name, process) in &chosen {
        if process.cmd.trim().is_empty() {
            let table = if name == detect::DEV && config.processes.len() == 1 {
                "[dev]".to_string()
            } else {
                format!("[processes.{name}]")
            };
            bail!("{table} in pando.toml has no cmd — add the command that starts this process");
        }
    }
    Ok(chosen)
}

/// Every role every process of the worktree owns, alphabetically by
/// process name — which is the order `Config.processes` keeps them in, and
/// the order they are spawned in.
///
/// Ports are reserved for all of them at once, whatever `--only` asked
/// for: a role belongs to the worktree, and starting one process must
/// never move another one's port. `config::validate` has already refused
/// two processes claiming one role, so this only deduplicates defensively.
/// Every role the worktree needs a port for: the processes' first, then
/// the services' when this worktree runs private copies of them.
///
/// Services last, deliberately. Ports are handed out in role order from
/// one window, so putting them after the processes means a worktree that
/// switches from shared to isolated keeps the web port it already had and
/// simply grows the window — no bookmarked URL changes for turning
/// isolation on.
fn worktree_roles(config: &Config, isolated: bool) -> Vec<String> {
    let mut roles: Vec<String> = Vec::new();
    for process in config.processes.values() {
        for role in process.roles() {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
    }
    if isolated {
        for role in service_roles(config) {
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
    }
    roles
}

/// One `[[services]]` entry of `kind = "compose"`, flattened.
struct ComposeEntry<'a> {
    file: &'a str,
    include: &'a [String],
    env: &'a BTreeMap<String, String>,
    ready_timeout_s: Option<u64>,
}

fn compose_entries(config: &Config) -> Vec<ComposeEntry<'_>> {
    config
        .services
        .iter()
        .filter_map(|service| match service {
            config::ServiceConfig::Compose {
                file,
                include,
                env,
                ready_timeout_s,
            } => Some(ComposeEntry {
                file,
                include,
                env,
                ready_timeout_s: *ready_timeout_s,
            }),
            // Native services are Phase 6.
            config::ServiceConfig::Native { .. } => None,
        })
        .collect()
}

/// Every service this project can run a private copy of, in config order.
/// Each one is a role, so `{port:postgres}` resolves like any other.
fn service_roles(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in compose_entries(config) {
        for name in entry.include {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    out
}

/// The service records an isolated start writes, before anything is up.
///
/// A record that already exists keeps its log pump, so a plain `start`
/// beside live services does not lose the pid that stops it. A record for
/// a service config no longer includes is kept too: it still names the
/// compose project, and dropping it would leave a container and its volume
/// with nothing in pando able to take them down — but its *port* is
/// blanked, because the window has moved on and another service has that
/// number now. `status` would otherwise show two services on one port, and
/// only one of them would be telling the truth.
fn planned_services(
    config: &Config,
    paths: &PandoPaths,
    name: &str,
    ports: &BTreeMap<String, u16>,
    record: &WorktreeRecord,
) -> Vec<state::ServiceRecord> {
    let project = crate::compose::project_name(paths.project_id(), name);
    let mut out: Vec<state::ServiceRecord> = Vec::new();
    for service in service_roles(config) {
        let existing = record.services.iter().find(|s| s.name == service);
        out.push(state::ServiceRecord {
            name: service.clone(),
            kind: state::ServiceKind::Compose,
            port: ports.get(&service).copied(),
            pid: existing.and_then(|s| s.pid),
            pgid: existing.and_then(|s| s.pgid),
            compose_project: Some(project.clone()),
        });
    }
    for service in &record.services {
        if !out.iter().any(|kept| kept.name == service.name) {
            out.push(state::ServiceRecord {
                port: None,
                ..service.clone()
            });
        }
    }
    out
}

/// The env every process and hook of an isolated worktree is given, so it
/// talks to its own services rather than the shared ones.
fn resolve_service_env(
    config: &Config,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for entry in compose_entries(config) {
        out.extend(services::app_env(worktree, entry.env, ports)?);
    }
    Ok(out)
}

/// Brings this worktree's private services up, waits for them, and starts
/// a log pump in front of each one.
///
/// The override that remaps the ports is regenerated every time: the
/// ports can move, the compose file can change under a rebase, and a
/// stale override would publish a port nothing is on.
fn bring_up_services(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    worktree: &Path,
    ports: &BTreeMap<String, u16>,
    progress: &dyn Fn(&str),
) -> Result<()> {
    let entries = compose_entries(config);
    if entries.is_empty() {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut published: Vec<crate::compose::Published> = Vec::new();
    let mut wanted: Vec<services::Wanted> = Vec::new();
    let mut include: Vec<String> = Vec::new();
    let mut timeout = services::DEFAULT_READY_TIMEOUT_S;
    // Everywhere a bind mount must not land: the main checkout, the
    // directory every worktree lives under, and this worktree itself for
    // the adopted case, where it is somewhere else entirely.
    let repository = vec![
        paths.root().to_path_buf(),
        config.worktrees_dir(paths),
        worktree.to_path_buf(),
    ];
    let project = crate::compose::project_name(paths.project_id(), name);
    let program = services::docker_program(paths);
    for entry in &entries {
        let file = crate::compose::file_in(worktree, entry.file)?;
        let mut parsed = crate::compose::read(&file)?;
        // `extends:` and a top-level `include:` put the real definition in
        // a file this reader does not follow, so what it read is not what
        // compose would run. Compose can say; it is already the thing
        // about to bring the services up.
        if parsed.unresolved.any()
            && let Ok(resolved) =
                services::Compose::new(&program, &project, vec![file.clone()], worktree).config()
        {
            parsed = resolved;
        }
        for (service, container) in
            crate::compose::resolve_included(&parsed, entry.include, &repository)?
        {
            let host = *ports
                .get(&service)
                .with_context(|| format!("no port was allocated for the service {service:?}"))?;
            wanted.push(services::Wanted {
                service: service.clone(),
                port: host,
                healthcheck: parsed.services[&service].healthcheck,
            });
            published.push(crate::compose::Published {
                service: service.clone(),
                container,
                host,
            });
            include.push(service);
        }
        files.push(file);
    }
    for entry in &entries {
        if let Some(configured) = entry.ready_timeout_s {
            timeout = configured;
        }
    }
    // A `[[services]]` entry with an empty `include` is the written-down
    // answer "none of them". `docker compose up -d` with no service named
    // brings up *everything* in the file, on the ports the project
    // hardcoded, which is the opposite of what was asked for.
    if include.is_empty() {
        return Ok(());
    }

    let override_file = paths.compose_override_file(name);
    if let Some(parent) = override_file.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(
        &override_file,
        crate::compose::render_override(name, &published),
    )
    .with_context(|| format!("write {}", override_file.display()))?;
    files.push(override_file);

    let compose = services::Compose::new(&program, &project, files, worktree);
    progress(&format!("starting services: {}", include.join(", ")));
    compose.up(&include)?;

    // A service that never comes up leaves nothing running: the ones that
    // did are stopped again, so a failed start does not leave half an
    // environment holding ports.
    //
    // By project, not by files. `up -d <name>` enables that service's
    // profile implicitly; a `stop` with the same `-f` files does not, and
    // leaves a profiled container running on the port it was allocated.
    // Compose finds every container it created by label, which is why
    // `stop` and `rm` use this form too.
    if let Err(e) = services::wait_ready(&compose, &wanted, Duration::from_secs(timeout), progress)
    {
        let _ = services::Compose::by_project(&program, &project).stop();
        return Err(e);
    }

    pump_service_logs(paths, name, &compose, &include, worktree)
}

/// Remembers that this worktree runs private services, once they are up.
///
/// Written *after* `bring_up_services` rather than with the rest of the
/// record, because the flag is what a later plain `start` reads to keep
/// using them: a start that failed before any container existed — a
/// mapping the env rewriter cannot satisfy, a service the compose file
/// cannot isolate — would otherwise leave the worktree unable to start at
/// all, isolated or shared, until `pando.toml` was edited by hand.
///
/// A worktree that was already isolated keeps the flag through such a
/// failure, because its containers are real and nothing here clears it.
fn remember_isolated(paths: &PandoPaths, name: &str) -> Result<()> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let Some(record) = store.worktrees.get_mut(name) else {
        return Ok(());
    };
    if record.isolated {
        return Ok(());
    }
    record.isolated = true;
    state::save(&paths.state_file(), &store)
}

/// One detached `docker compose logs -f` per service, writing into the
/// worktree's log directory so the viewer has a tab for it.
///
/// Recorded as the service's pid and pgid, which is what makes it a
/// process `stop` signals and the orphan sweep covers.
fn pump_service_logs(
    paths: &PandoPaths,
    name: &str,
    compose: &services::Compose,
    include: &[String],
    worktree: &Path,
) -> Result<()> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    for service in include {
        let running = store
            .worktrees
            .get(name)
            .and_then(|r| r.services.iter().find(|s| &s.name == service))
            .and_then(|s| s.pid)
            .is_some_and(proc::is_alive);
        if running {
            continue;
        }
        let log_file = paths.log_file(name, service);
        reset_log(&log_file)?;
        let spawned = proc::spawn_detached(SpawnOptions {
            shell_cmd: &compose.logs_shell_cmd(service),
            cwd: worktree,
            log_file: &log_file,
            env: &[],
        })
        .with_context(|| format!("start the log pump for the service {service:?}"))?;
        if let Some(record) = store
            .worktrees
            .get_mut(name)
            .and_then(|r| r.services.iter_mut().find(|s| &s.name == service))
        {
            record.pid = Some(spawned.pid);
            record.pgid = Some(spawned.pgid);
        }
    }
    state::save(&paths.state_file(), &store)
}

/// Signals every log pump of a worktree and forgets it, leaving the rest
/// of the service record — the port and the compose project — in place.
fn stop_service_pumps(
    record: &mut WorktreeRecord,
    stop: &impl Fn(i32) -> Result<()>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for service in record.services.iter_mut() {
        let Some(pgid) = service.pgid else { continue };
        match stop(pgid) {
            Ok(()) => {
                service.pid = None;
                service.pgid = None;
            }
            Err(e) => failures.push(format!(
                "the log pump for {} (group {pgid}): {e:#}",
                service.name
            )),
        }
    }
    failures
}

/// The compose projects a worktree's records name, each once.
fn compose_projects(record: &WorktreeRecord) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for service in &record.services {
        if service.kind != state::ServiceKind::Compose {
            continue;
        }
        if let Some(project) = &service.compose_project
            && !out.contains(project)
        {
            out.push(project.clone());
        }
    }
    out
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

/// One of a worktree's private services, as `status` and the TUI show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub port: Option<u16>,
    /// Whether something answers on that port right now.
    pub up: bool,
}

/// Every service a worktree owns, with whether it is answering.
///
/// A TCP connect, never a bind — the same rule readiness follows, for the
/// same reason: a bind probe would take the port from the container that
/// owns it. It says nothing about *whose* listener answered, which is
/// exactly as much as a status line needs to claim.
pub fn service_statuses(record: &WorktreeRecord) -> Vec<ServiceStatus> {
    record
        .services
        .iter()
        .map(|service| ServiceStatus {
            name: service.name.clone(),
            port: service.port,
            up: service.port.map(ports::something_is_listening) == Some(true),
        })
        .collect()
}

/// The project's *shared* services and whether each is answering.
///
/// Shared mode runs no containers of pando's, so there is nothing in state
/// to look at: the port comes from the main checkout's own env files,
/// through the key the `[[services]]` entry maps to that service. That is
/// the number the developer's own `docker compose up` published, which is
/// exactly what the header chip is claiming to know about.
pub fn shared_service_statuses(paths: &PandoPaths, config: &Config) -> Vec<ServiceStatus> {
    let mut out: Vec<ServiceStatus> = Vec::new();
    for entry in compose_entries(config) {
        for (key, service) in entry.env {
            if out.iter().any(|status| &status.name == service) {
                continue;
            }
            let port = services::port_in_env(paths.root(), key);
            out.push(ServiceStatus {
                name: service.clone(),
                port,
                up: port.map(ports::something_is_listening) == Some(true),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The environment a command run by hand inside a worktree needs, so that
/// it reaches the same database and the same ports the dev processes do.
///
/// This is what `status --env` prints. It replaces materialising a
/// rewritten `.env` inside the worktree, which Invariant 1 forbids unless
/// the project already ignores that path — and which would be wrong the
/// moment two worktrees disagreed about it.
///
/// Merged in one fixed order — the services, then each process by name,
/// then pando's own variables — so two runs of the same command print the
/// same thing.
pub fn resolved_env(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let worktree = find_worktree(paths, name)?;
    let canonical = std::fs::canonicalize(&worktree.path).unwrap_or_else(|_| worktree.path.clone());
    let store = state::load(&paths.state_file())?;
    let record = store
        .worktrees
        .get(name)
        .with_context(|| format!("pando has no record of {name}"))?;
    if record.ports.is_empty() {
        bail!(
            "{name} has no ports yet — start it once, and `pando status --env {name}` can say \
             where everything is"
        );
    }
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    if record.isolated {
        out.extend(resolve_service_env(config, &canonical, &record.ports)?);
    }
    for (process_name, process) in &config.processes {
        let log_file = paths.log_file(name, process_name);
        let ctx = template::Context {
            name,
            branch: worktree.branch.as_deref(),
            worktree: &canonical,
            root: paths.root(),
            project: paths.project_id(),
            ports: &record.ports,
            default_role: None,
            log: Some(&log_file),
        };
        // Through the same function the processes are started with, so a
        // developer who evals this gets exactly what the dev server got.
        for (var, value) in process_env(paths, name, &worktree, process, &out.clone(), &ctx)
            .with_context(|| format!("in process {process_name}"))?
        {
            out.insert(var, value);
        }
    }
    Ok(out)
}

/// `export KEY='value'` lines a shell can `eval`.
///
/// Single quotes with the close-escape-reopen trick, because a value can
/// hold anything: a password with a `$` in it must not be expanded, and a
/// value with a quote in it must not end the string early.
pub fn export_lines(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push_str(&format!(
            "export {key}='{}'\n",
            value.replace('\'', "'\\''")
        ));
    }
    out
}

/// The URL a worktree serves on. One worktree, one URL, however many
/// processes it runs — and one function, because `start`, `status`, `ls`,
/// the TUI's row and the TUI's `o` key all have to hand out the same one.
///
/// The role is `web` whenever any process owns it, because that is the role
/// everything else defaults to; otherwise the first role of the
/// alphabetically first process that owns one. The port is what that
/// process is really listening on when that is known, and the port pando
/// assigned it otherwise.
///
/// From the record alone: `status` and the TUI never see config, and the
/// answer has to survive a stop unchanged.
pub fn worktree_url(record: &WorktreeRecord) -> Option<String> {
    let role = url_role(record)?;
    let assigned = *record.ports.get(&role)?;
    Some(format!(
        "http://localhost:{}",
        observed_port_for_role(record, &role).unwrap_or(assigned)
    ))
}

/// The role a worktree's URL points at: `web` wherever anything owns it,
/// else the first role of the alphabetically first process that owns one.
fn url_role(record: &WorktreeRecord) -> Option<String> {
    if record.ports.contains_key(DEFAULT_READY_ROLE) {
        return Some(DEFAULT_READY_ROLE.to_string());
    }
    record
        .roles
        .values()
        .find_map(|roles| roles.first())
        .filter(|role| record.ports.contains_key(*role))
        .cloned()
        // A record written before pando kept track of who owns what: the
        // ports are all there is to go on.
        .or_else(|| record.ports.keys().next().cloned())
}

/// The port the process that owns `role` is really listening on.
///
/// A framework that ignores `PORT`, or one that picked the next free
/// number, is followed: the URL is what is really serving, not what pando
/// asked for. But only within the owning process's own group. An observed
/// port belongs to whichever group opened it, and with several processes a
/// port nobody claimed is far more often the *other* process's HMR socket
/// or debugger than this role's server — which is how a worktree whose web
/// server was stopped handed out the api's second port as its URL.
fn observed_port_for_role(record: &WorktreeRecord, role: &str) -> Option<u16> {
    let assigned = *record.ports.get(role)?;
    let owner = record
        .roles
        .iter()
        .find(|(_, roles)| roles.iter().any(|owned| owned == role))
        .map(|(process, _)| process)?;
    let process = record.processes.get(owner)?;
    // A process that is not up is not listening on anything, and what it
    // was last seen holding says nothing about now.
    if !matches!(
        process.phase,
        Phase::Starting { .. } | Phase::Running { .. }
    ) {
        return None;
    }
    if process.observed_ports.contains(&assigned) {
        return Some(assigned);
    }
    process
        .observed_ports
        .iter()
        .copied()
        .find(|observed| !record.ports.values().any(|port| port == observed))
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
    service_env: &BTreeMap<String, String>,
    ctx: &template::Context<'_>,
) -> Result<Vec<(String, String)>> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (var, tmpl) in process.port_env() {
        env.insert(
            var.clone(),
            template::render(&tmpl, ctx).with_context(|| format!("in ports.{var}"))?,
        );
    }
    // Between the port sugar and the process's own `env`, so a developer
    // who spells a service URL out by hand still wins: pando's rewrite is
    // the default, not the law.
    for (var, value) in service_env {
        env.insert(var.clone(), value.clone());
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
    /// What the refresh itself did, one line each. A share whose tunnel
    /// died is closed here rather than in silence: the URL a developer had
    /// open stops working, and they should be told once rather than
    /// discover it from a browser.
    pub notices: Vec<String>,
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
            notices: Vec::new(),
        };
    }
    let _lock = match state::lock(&paths.lock_file()) {
        Ok(lock) => lock,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
                notices: Vec::new(),
            };
        }
    };
    let mut store = match state::load(&paths.state_file()) {
        Ok(store) => store,
        Err(e) => {
            return Refreshed {
                state: state::State::new(),
                warning: Some(format!("{e:#}")),
                notices: Vec::new(),
            };
        }
    };

    // Before anything advances or reconciles: both of those drop a share
    // whose tunnel has died, and neither can signal what is left of it.
    // Signalling is not spawning — a read path must not start a process,
    // but a tunnel nothing can reach again is exactly what it must not
    // leave behind either.
    let notices = sweep_dead_shares(&mut store);

    // One scan of every live group, used for both questions this pass
    // answers: whether a starting process has opened its port yet, and what
    // every group is really listening on.
    let scans = scan_groups(&store);
    let mut changed = !notices.is_empty();
    changed |= advance_with(&mut store, &scans);
    changed |= capture_observed_ports(&mut store, &scans);
    if changed {
        // A read path that cannot write is still a read path: the phases are
        // right in memory either way, so a save that fails is not worth
        // failing the command the user actually ran.
        if let Err(e) = state::save(&paths.state_file(), &store) {
            return Refreshed {
                state: store,
                warning: Some(format!("{e:#}")),
                notices,
            };
        }
    }
    Refreshed {
        state: store,
        warning: None,
        notices,
    }
}

/// Moves every process to the phase it is really in, and writes the reason
/// for each failure that is new. The one implementation of "advance", used
/// by the read path and by every mutation that is about to `reconcile`.
fn advance_with(store: &mut state::State, scans: &BTreeMap<i32, Option<Vec<u16>>>) -> bool {
    let failed_before = failed_processes(store);
    let mut changed = state::advance_phases(store, proc::is_alive, |pgid, port| {
        port_is_bound(scans, pgid, port)
    });
    changed |= explain_new_failures(store, &failed_before);
    changed
}

/// Advances phases before `reconcile` drops anything.
///
/// `reconcile` keeps a record that is already `Failed`, because a crash has
/// to stay visible until the developer acts on it. That only works if
/// something marked it failed first — and between a process dying and the
/// next read path, nothing has. Without this, a `start`, a `stop`, or a
/// `stop --only` of a *sibling* silently drops the crash it was about to
/// make visible. So every mutation advances with the same inputs the read
/// path uses, and only then reconciles.
///
/// The observed ports the same scan could capture are deliberately not
/// recorded here: `start` clears them for the processes it replaces, and
/// the next refresh says what is really listening.
fn advance_before_reconcile(store: &mut state::State) {
    let scans = scan_groups(store);
    advance_with(store, &scans);
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

/// Records the ports each live group is really listening on, per process.
///
/// Configured ports are what pando asked for; these are what happened. A
/// framework that ignores `PORT`, or one that opens a second socket for hot
/// reload, shows up here and nowhere else.
///
/// Per process, because a worktree-wide list cannot say which group opened
/// which socket: an `--inspect` port the api opened reads exactly like a
/// port the web server opened, and the worktree's URL then follows it.
/// The worktree's own list stays as the union of them, which is the shape
/// `status --json` publishes.
fn capture_observed_ports(
    store: &mut state::State,
    scans: &BTreeMap<i32, Option<Vec<u16>>>,
) -> bool {
    let mut changed = false;
    for record in store.worktrees.values_mut() {
        let mut union: Vec<u16> = Vec::new();
        let mut groups = 0usize;
        let mut scanned = false;
        for proc in record.processes.values_mut() {
            if !matches!(proc.phase, Phase::Starting { .. } | Phase::Running { .. }) {
                // Not up, so listening on nothing: its last sighting went
                // stale the moment it stopped.
                if !proc.observed_ports.is_empty() {
                    proc.observed_ports.clear();
                    changed = true;
                }
                continue;
            }
            groups += 1;
            let Some(Some(ports)) = scans.get(&proc.pgid) else {
                // This group could not be scanned: its last good answer
                // stands rather than being cleared by a missing `lsof`.
                union.extend(proc.observed_ports.iter().copied());
                continue;
            };
            scanned = true;
            let mut observed = ports.clone();
            observed.sort_unstable();
            observed.dedup();
            if proc.observed_ports != observed {
                proc.observed_ports = observed.clone();
                changed = true;
            }
            union.extend(observed);
        }
        union.sort_unstable();
        union.dedup();
        // Nothing could be scanned at all, so there is nothing to say.
        if groups > 0 && !scanned {
            continue;
        }
        if record.observed_ports != union {
            record.observed_ports = union;
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

    /// The shared-mode `start`, which is what every test written before
    /// isolation existed means. Shadowing the real one keeps those tests
    /// reading as they did — `--isolated` is a separate question, and they
    /// were never asking it.
    fn start(
        paths: &PandoPaths,
        config: &Config,
        name: &str,
        only: Option<&str>,
        progress: &dyn Fn(&str),
    ) -> Result<StartReport> {
        super::start(paths, config, name, only, false, progress)
    }

    fn restart(
        paths: &PandoPaths,
        config: &Config,
        name: &str,
        only: Option<&str>,
        progress: &dyn Fn(&str),
    ) -> Result<StartReport> {
        super::restart(paths, config, name, only, false, progress)
    }

    /// `stop`, `stop_all` and `rm` as every test written before the sweep
    /// narrated anything means them: with nobody listening. Shadowing them
    /// keeps those tests reading as they did — the notices a sweep returns
    /// are a separate question, asked by the tests that ask it, which call
    /// `super::` directly.
    fn stop(paths: &PandoPaths, name: &str, only: Option<&str>) -> Result<StopOutcome> {
        super::stop(paths, name, only, &noop)
    }

    fn stop_all(paths: &PandoPaths) -> Result<Vec<String>> {
        super::stop_all(paths, &noop)
    }

    fn stop_all_with(paths: &PandoPaths, stop: impl Fn(i32) -> Result<()>) -> Result<Vec<String>> {
        super::stop_all_with(paths, stop, &noop)
    }

    fn rm(paths: &PandoPaths, name: &str, yes: bool, force: bool) -> Result<()> {
        super::rm(paths, name, yes, force, &noop)
    }

    /// Detection for a shared-mode start, which is what every test written
    /// before isolation existed means.
    fn resolve_process(
        paths: &PandoPaths,
        config: &Config,
        ask: Ask<'_>,
        progress: &dyn Fn(&str),
    ) -> Result<Config> {
        super::resolve_process(paths, config, false, ask, progress)
    }

    // The two lists have to stay in step: a process named `install` writes
    // the install hook's log file, and `reset_log` truncates it on every
    // start.
    #[test]
    fn the_install_hooks_log_name_is_one_no_process_may_take() {
        assert!(
            crate::paths::RESERVED_LOG_SOURCES.contains(&INSTALL_HOOK),
            "{INSTALL_HOOK} must be reserved, or a process can take its log"
        );
    }

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
    fn guard(report: &StartReport) -> Vec<Detached> {
        report
            .started
            .iter()
            .map(|p| Detached {
                pid: p.record.pid,
                pgid: p.record.pgid,
            })
            .collect()
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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let started = &outcome.started[0];
        assert!(!outcome.started_nothing());
        assert_eq!(outcome.worktree, name);
        assert_eq!(started.process, "dev");
        assert!(outcome.already_running.is_empty());
        assert_eq!(outcome.ports.len(), 1, "one role, one port");
        let port = outcome.ports["web"];
        assert_eq!(
            outcome.url.as_deref(),
            Some(&*format!("http://localhost:{port}"))
        );
        assert!(!outcome.reassigned);

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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];
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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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
        let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
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

        let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
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
        let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "no processes configured; add [dev] to pando.toml"
        );
    }

    #[test]
    fn starting_a_worktree_that_does_not_exist_says_so() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let err = start(&fx.paths, &fx.config, "nope", None, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("no worktree named"));
        let err = start(&fx.paths, &fx.config, "acme-shop", None, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("main checkout"));
    }

    #[test]
    fn a_second_start_while_running_reports_the_process_that_is_already_up() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");

        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&first);
        let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        assert!(
            second.started_nothing(),
            "a running process is reported, not started twice"
        );
        assert_eq!(
            second
                .already_running
                .iter()
                .map(|p| p.process.as_str())
                .collect::<Vec<_>>(),
            vec!["dev"],
            "and it says which process that was"
        );
        assert_eq!(
            second.already_running[0].record.pid,
            first.started[0].record.pid
        );
        assert_eq!(second.ports, first.ports);
    }

    // A failure is sticky for display; starting is the user acting on it.
    #[test]
    fn start_clears_a_failed_record_and_starts_fresh() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let first_pid = first.started[0].record.pid;
        let first_ports = first.ports.clone();
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

        let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&second);
        assert!(!second.started_nothing());
        assert_ne!(second.started[0].record.pid, first_pid);
        assert_eq!(
            second.ports, first_ports,
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(outcome.ports.is_empty());
        assert_eq!(outcome.started[0].record.ready_port, None);
        assert_eq!(outcome.url, None);

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
        let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];

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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];

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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];

        // Taking the port is this test's setup, not what it is about: on a
        // busy run another test's listener can hold it for a moment, and a
        // failure there says nothing about readiness.
        let squatter = bind_when_free(port);
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

    /// Takes `port`, waiting for whatever else on this machine has it.
    fn bind_when_free(port: u16) -> std::net::TcpListener {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => return listener,
                Err(e) if std::time::Instant::now() >= deadline => {
                    panic!("could not take port {port} to squat on: {e}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

    // ---- several processes ------------------------------------------------

    /// The workspace shape: a web process and an api process, each in its
    /// own directory, with the web one told the api's port through a
    /// template. Fixture 5, in miniature.
    fn with_web_and_api(fx: &mut Fx) {
        fx.config.processes.insert(
            "web".to_string(),
            ProcessConfig {
                cmd: "pwd && env && sleep 30".to_string(),
                cwd: Some("apps/web".to_string()),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                env: BTreeMap::from([(
                    "VITE_API_URL".to_string(),
                    "http://localhost:{port:api}".to_string(),
                )]),
                ..Default::default()
            },
        );
        fx.config.processes.insert(
            "api".to_string(),
            ProcessConfig {
                cmd: "pwd && env && sleep 30".to_string(),
                cwd: Some("apps/api".to_string()),
                ports: Some(PortsSpec::Map(BTreeMap::from([(
                    "PORT".to_string(),
                    "api".to_string(),
                )]))),
                ..Default::default()
            },
        );
    }

    /// The worktree both processes need, with the directories they run in.
    fn workspace_worktree(fx: &Fx, branch: &str) -> String {
        let name = worktree_named(fx, branch);
        let worktree = fx.worktrees_dir().join(&name);
        std::fs::create_dir_all(worktree.join("apps/web")).unwrap();
        std::fs::create_dir_all(worktree.join("apps/api")).unwrap();
        name
    }

    fn log_source(fx: &Fx, name: &str, source: &str) -> String {
        std::fs::read_to_string(fx.paths.log_file(name, source)).unwrap_or_default()
    }

    fn names_of(list: &[StartedProcess]) -> Vec<&str> {
        list.iter().map(|p| p.process.as_str()).collect()
    }

    fn live_processes(fx: &Fx, name: &str) -> Vec<String> {
        fx.state().worktrees[name]
            .processes
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn start_spawns_every_configured_process_with_its_own_log_cwd_and_port() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");

        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);
        assert_eq!(
            names_of(&report.started),
            vec!["api", "web"],
            "config order, spawned one after the other"
        );
        assert_eq!(report.ports.len(), 2, "a port for every role");
        assert_eq!(
            report.url,
            Some(format!("http://localhost:{}", report.ports["web"])),
            "one URL for the worktree, and it is the web role's"
        );

        let record = fx.state().worktrees[&name].clone();
        assert_eq!(
            record.processes.keys().cloned().collect::<Vec<_>>(),
            vec!["api", "web"],
            "the worktree record holds both"
        );
        assert_eq!(
            record.processes["web"].ready_port,
            Some(report.ports["web"]),
            "each process waits on its own role's port"
        );
        assert_eq!(
            record.processes["api"].ready_port,
            Some(report.ports["api"])
        );
        assert_eq!(
            record.processes["web"].log_path,
            fx.paths.log_file(&name, "web")
        );
        assert_eq!(
            record.processes["api"].log_path,
            fx.paths.log_file(&name, "api")
        );
        assert_ne!(
            record.processes["web"].pgid, record.processes["api"].pgid,
            "each gets its own process group, so one can be stopped alone"
        );

        // Each in its own directory, and the web one carrying the api's
        // real port: `{port:api}` is why the template language has roles.
        let api_port = report.ports["api"];
        // Patient: `bash -lc` reads a login profile, and a full parallel
        // test run has a dozen of them starting at once.
        assert!(
            wait_until(Duration::from_secs(30), || {
                log_source(&fx, &name, "web")
                    .contains(&format!("VITE_API_URL=http://localhost:{api_port}"))
            }),
            "the web log should carry the api's port: {:?}",
            log_source(&fx, &name, "web")
        );
        assert!(
            wait_until(Duration::from_secs(30), || {
                log_source(&fx, &name, "web").contains("apps/web")
                    && log_source(&fx, &name, "api").contains("apps/api")
            }),
            "each process runs in its own cwd: {:?} / {:?}",
            log_source(&fx, &name, "web"),
            log_source(&fx, &name, "api")
        );
        assert!(
            wait_until(Duration::from_secs(30), || {
                log_source(&fx, &name, "api").contains(&format!("PORT={api_port}"))
            }),
            "the map form of ports reaches the process it belongs to: {:?}",
            log_source(&fx, &name, "api")
        );
    }

    #[test]
    fn only_starts_the_process_it_names_and_leaves_the_other_alone() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");

        let first = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
        let _ga = guard(&first);
        assert_eq!(names_of(&first.started), vec!["api"]);
        assert_eq!(live_processes(&fx, &name), vec!["api"]);
        assert_eq!(
            first.ports.len(),
            2,
            "ports are reserved for every role of the worktree, not only the one started"
        );

        let api_pid = fx.state().worktrees[&name].processes["api"].pid;
        let second = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
        let _gb = guard(&second);
        assert_eq!(names_of(&second.started), vec!["web"]);
        assert_eq!(
            second.ports, first.ports,
            "a second --only start never moves the ports the first one handed out"
        );
        assert!(!second.reassigned);
        assert_eq!(live_processes(&fx, &name), vec!["api", "web"]);
        assert_eq!(
            fx.state().worktrees[&name].processes["api"].pid,
            api_pid,
            "the process it did not name was not touched"
        );
    }

    // The port a worktree's *own* listener holds is not a port somebody
    // took. Read that way, starting the second of a pair would move both
    // ports — while the first process is still serving on the old one.
    #[test]
    fn starting_one_process_beside_a_listening_one_keeps_every_port() {
        if !python3_available() {
            eprintln!("skipping: python3 is not installed");
            return;
        }
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let api = fx.config.processes.get_mut("api").expect("the api process");
        api.cmd = crate::testutil::python_listener_for_role("api");
        api.cwd = None;
        let web = fx.config.processes.get_mut("web").expect("the web process");
        web.cwd = None;
        let name = worktree_named(&fx, "feat/one");

        let first = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
        let _ga = guard(&first);
        let api_port = first.ports["api"];
        assert!(
            wait_until(Duration::from_secs(30), || ports::something_is_listening(
                api_port
            )),
            "the api never came up on {api_port}: {:?}",
            log_source(&fx, &name, "api")
        );

        let second = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
        let _gb = guard(&second);
        assert_eq!(
            second.ports, first.ports,
            "the worktree's own listener must not look like a squatter"
        );
        assert!(!second.reassigned);
        assert_eq!(
            fx.state().worktrees[&name].ports["api"],
            api_port,
            "and the api is still recorded on the port it is really serving"
        );
    }

    #[test]
    fn a_second_start_says_which_processes_were_already_running() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");

        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&first);
        let said = std::sync::Mutex::new(Vec::<String>::new());
        let report = start(&fx.paths, &fx.config, &name, None, &|m: &str| {
            said.lock().expect("the progress lock").push(m.to_string())
        })
        .unwrap();
        let said = said.into_inner().expect("the progress lock");
        assert!(report.started_nothing());
        assert_eq!(names_of(&report.already_running), vec!["api", "web"]);
        assert!(
            said.contains(&"api is already running".to_string())
                && said.contains(&"web is already running".to_string()),
            "and it says so on the way: {said:?}"
        );
        assert_eq!(
            fx.state().worktrees[&name].processes["web"].pid,
            first.started[1].record.pid,
            "nothing was started twice"
        );
    }

    #[test]
    fn an_only_that_names_nothing_says_what_there_is_and_starts_nothing() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");

        let err = start(&fx.paths, &fx.config, &name, Some("worker"), &noop).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("worker"), "{msg}");
        assert!(msg.contains("api, web"), "the names there are: {msg}");
        assert!(
            fx.state().worktrees[&name].processes.is_empty(),
            "a refused --only starts nothing"
        );
    }

    // Planned in full before anything is spawned: the first process must
    // not be left running behind a start that failed on the second.
    #[test]
    fn a_process_that_cannot_start_leaves_none_of_the_others_running() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        fx.config.processes.get_mut("web").unwrap().cwd = Some("apps/nope".to_string());
        let name = workspace_worktree(&fx, "feat/one");

        let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("apps/nope"));
        assert!(
            fx.state().worktrees[&name].processes.is_empty(),
            "the api process must not have been spawned either"
        );
    }

    #[test]
    fn stop_signals_every_process_group_and_keeps_the_ports() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);
        let groups: Vec<i32> = report.started.iter().map(|p| p.record.pgid).collect();

        assert_eq!(
            stop(&fx.paths, &name, None).unwrap(),
            StopOutcome::Stopped(vec!["api".to_string(), "web".to_string()])
        );
        for pgid in groups {
            assert!(
                !crate::process::group_alive(pgid),
                "group {pgid} survived the stop"
            );
        }
        let record = fx.state().worktrees[&name].clone();
        assert!(record.processes.is_empty());
        assert_eq!(
            record.ports, report.ports,
            "a stopped worktree keeps its ports"
        );
    }

    #[test]
    fn stop_only_signals_one_group_and_leaves_the_other_running() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);
        let web = report.started.iter().find(|p| p.process == "web").unwrap();
        let api = report.started.iter().find(|p| p.process == "api").unwrap();

        assert_eq!(
            stop(&fx.paths, &name, Some("web")).unwrap(),
            StopOutcome::Stopped(vec!["web".to_string()])
        );
        assert!(!crate::process::group_alive(web.record.pgid));
        assert!(
            crate::process::group_alive(api.record.pgid),
            "the process it did not name keeps serving"
        );
        assert_eq!(live_processes(&fx, &name), vec!["api"]);
    }

    #[test]
    fn stopping_a_process_a_worktree_is_not_running_names_the_ones_it_is() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
        let _guard = guard(&report);

        let err = stop(&fx.paths, &name, Some("web")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("web"), "{msg}");
        assert!(msg.contains("it is running: api"), "{msg}");
        assert_eq!(live_processes(&fx, &name), vec!["api"], "and stops nothing");
    }

    #[test]
    fn restart_only_replaces_one_process_and_keeps_every_port() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&first);
        let web_pid = fx.state().worktrees[&name].processes["web"].pid;
        let api_pid = fx.state().worktrees[&name].processes["api"].pid;

        let report = restart(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
        let _g2 = guard(&report);
        assert_eq!(names_of(&report.started), vec!["api"]);
        assert_eq!(
            report.ports, first.ports,
            "a restart keeps the URL, and --only keeps the other process's too"
        );
        let after = fx.state().worktrees[&name].processes.clone();
        assert_ne!(after["api"].pid, api_pid, "the api is a new process");
        assert_eq!(after["web"].pid, web_pid, "the web process never stopped");
        assert!(crate::process::is_alive(web_pid));
    }

    // Phase 2b review, finding 5. `restart` was `stop` then `start`, and a
    // `--only` stop of something that is not running is an error — so the
    // one command a developer reaches for to bring a stopped process back
    // refused to do it, but only when a *sibling* was still up.
    #[test]
    fn restart_only_brings_back_a_process_that_is_not_running() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&first);
        stop(&fx.paths, &name, Some("api")).unwrap();
        let web_pid = fx.state().worktrees[&name].processes["web"].pid;
        assert!(
            !fx.state().worktrees[&name].processes.contains_key("api"),
            "the api is down and the web process is still serving"
        );

        let report = restart(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
        let _g2 = guard(&report);
        assert_eq!(names_of(&report.started), vec!["api"]);
        assert_eq!(
            report.ports, first.ports,
            "and on the ports the worktree already had"
        );
        assert_eq!(
            fx.state().worktrees[&name].processes["web"].pid,
            web_pid,
            "the process it did not name was never touched"
        );
    }

    // The name is checked against config, not against what happens to be
    // running: the same typo used to produce two different messages
    // depending on unrelated state, and only one of them listed the names
    // config declares.
    #[test]
    fn restart_only_answers_a_name_config_never_heard_of_with_the_names_it_did() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&first);

        let err = format!(
            "{:#}",
            restart(&fx.paths, &fx.config, &name, Some("typo"), &noop).unwrap_err()
        );
        assert!(err.contains("no process named \"typo\""), "{err}");
        assert!(err.contains("api, web"), "{err}");
        assert_eq!(
            live_processes(&fx, &name),
            vec!["api", "web"],
            "and a refused restart stops nothing"
        );
    }

    // `stop` itself keeps the stricter message: it cannot see config — it
    // has to work when `pando.toml` is broken — so the record is the only
    // thing it can check a name against.
    #[test]
    fn stop_only_still_refuses_a_name_the_worktree_is_not_running() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
        let _guard = guard(&first);
        let err = format!("{:#}", stop(&fx.paths, &name, Some("api")).unwrap_err());
        assert!(
            err.contains("is not running a process named \"api\""),
            "{err}"
        );
    }

    #[test]
    fn restart_replaces_every_process_and_keeps_every_port() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        drop(guard(&first));
        let before: Vec<u32> = first.started.iter().map(|p| p.record.pid).collect();

        let report = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);
        assert_eq!(names_of(&report.started), vec!["api", "web"]);
        assert_eq!(report.ports, first.ports);
        let after: Vec<u32> = report.started.iter().map(|p| p.record.pid).collect();
        assert_ne!(after, before);
        for pid in before {
            assert!(!crate::process::is_alive(pid), "pid {pid} survived");
        }
    }

    // ---- stop ------------------------------------------------------------

    #[test]
    fn stop_ends_the_process_and_keeps_the_ports() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.started[0].record.pgid;
        let ports = outcome.ports.clone();

        assert_eq!(
            stop(&fx.paths, &name, None).unwrap(),
            StopOutcome::Stopped(vec!["dev".to_string()])
        );
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let (pid, pgid) = (
            outcome.started[0].record.pid,
            outcome.started[0].record.pgid,
        );

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

        assert_eq!(
            stop(&fx.paths, &name, None).unwrap(),
            StopOutcome::Stopped(vec!["dev".to_string()])
        );
        assert!(
            !crate::process::group_alive(pgid),
            "a failed record's group must still be killed"
        );
    }

    #[test]
    fn stopping_something_that_is_not_running_is_not_an_error() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        assert_eq!(
            stop(&fx.paths, "nope", None).unwrap(),
            StopOutcome::NotRunning
        );
        let name = worktree_named(&fx, "feat/one");
        assert_eq!(
            stop(&fx.paths, &name, None).unwrap(),
            StopOutcome::NotRunning
        );
    }

    #[test]
    fn stop_all_stops_every_worktree_that_is_running() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let one = worktree_named(&fx, "feat/one");
        let two = worktree_named(&fx, "feat/two");
        let a = start(&fx.paths, &fx.config, &one, None, &noop).unwrap();
        let _ga = guard(&a);
        let b = start(&fx.paths, &fx.config, &two, None, &noop).unwrap();
        let _gb = guard(&b);
        assert_ne!(
            a.ports["web"], b.ports["web"],
            "two worktrees never share a port"
        );

        let mut stopped = stop_all(&fx.paths).unwrap();
        stopped.sort();
        assert_eq!(stopped, vec![one.clone(), two.clone()]);
        assert!(!crate::process::group_alive(a.started[0].record.pgid));
        assert!(!crate::process::group_alive(b.started[0].record.pgid));
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
                observed_ports: Vec::new(),
                swept: false,
                phase: Phase::Running { since: Utc::now() },
            },
        );
        state::save(&fx.paths.state_file(), &store).unwrap();

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let orphan = guard(&outcome)
            .pop()
            .expect("the start spawned exactly one process");
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

    // Phase 2b review, finding 7. A `Failed` record now outlives
    // `reconcile`, and it is exactly the record most likely to still have a
    // live child behind its pgid — so the sweep has to keep signalling it,
    // and only its own worktree's stop may clear it.
    #[test]
    fn a_failed_records_group_is_signalled_and_the_record_kept_until_its_own_stop() {
        let mut fx = fixture();
        let (orphan_name, orphan) = orphaned_sibling(&mut fx);
        // A refresh is what turns a dead leader into a Failed record; the
        // child it backgrounded is still holding the group open.
        let state = refresh(&fx.paths).state;
        assert!(
            matches!(
                state.worktrees[&orphan_name].processes["dev"].phase,
                Phase::Failed { .. }
            ),
            "{:?}",
            state.worktrees[&orphan_name].processes["dev"].phase
        );

        let other = worktree_named(&fx, "feat/other");
        stop(&fx.paths, &other, None).unwrap();
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "a Failed record's group is signalled like every other"
        );
        assert!(
            matches!(
                fx.state().worktrees[&orphan_name].processes["dev"].phase,
                Phase::Failed { .. }
            ),
            "but stopping another worktree must not erase the crash"
        );

        // Its own stop is what clears it.
        stop(&fx.paths, &orphan_name, None).unwrap();
        assert!(fx.state().worktrees[&orphan_name].processes.is_empty());
    }

    // Finding 7 on the path `--only` promises to leave alone: `stop` ran
    // `reconcile` over the whole state file, so stopping the web process
    // threw away the record that said the api had crashed.
    #[test]
    fn stopping_one_process_leaves_a_siblings_failed_record_where_status_can_see_it() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);

        // The api dies, and a read is what notices.
        let api = fx.state().worktrees[&name].processes["api"].clone();
        crate::process::stop(api.pgid, Duration::from_secs(5)).unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || matches!(
                refresh(&fx.paths).state.worktrees[&name]
                    .processes
                    .get("api")
                    .map(|p| &p.phase),
                Some(Phase::Failed { .. })
            )),
            "the api never reached failed"
        );

        stop(&fx.paths, &name, Some("web")).unwrap();

        let state = refresh(&fx.paths).state;
        let record = &state.worktrees[&name];
        assert!(
            !record.processes.contains_key("web"),
            "the process that was stopped is gone"
        );
        assert!(
            matches!(
                record.processes.get("api").map(|p| &p.phase),
                Some(Phase::Failed { .. })
            ),
            "and the one that crashed is still saying so: {:?}",
            record.processes
        );
        assert_eq!(
            state::aggregate_phase(record).map(|a| a.word()),
            Some("failed"),
            "which is what the worktree reads as"
        );

        // A whole stop of its own worktree is what clears it.
        stop(&fx.paths, &name, None).unwrap();
        assert!(fx.state().worktrees[&name].processes.is_empty());
    }

    // The half finding 7 left open. `reconcile` keeps a record that is
    // *already* `Failed`, but a process that died since the last read path
    // is still recorded as `Running` — and `reconcile` drops it before
    // anything has had the chance to mark it failed. So a mutation path has
    // to advance phases first, exactly as the read path does.
    #[test]
    fn a_crash_no_read_path_has_seen_yet_survives_a_stop_of_its_sibling() {
        let mut fx = fixture();
        with_web_and_api(&mut fx);
        let name = workspace_worktree(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&report);

        // kill -9, and then *nothing reads state*: no `status`, no `ls`, no
        // TUI tick. What is on disk still says the api is running.
        let api = fx.state().worktrees[&name].processes["api"].clone();
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(api.pgid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || !crate::process::is_alive(
                api.pid
            )),
            "the api should be gone after a SIGKILL"
        );
        assert!(
            matches!(
                fx.state().worktrees[&name].processes["api"].phase,
                Phase::Running { .. } | Phase::Starting { .. }
            ),
            "the premise: nothing has marked it failed yet"
        );

        stop(&fx.paths, &name, Some("web")).unwrap();

        // On disk, without a read path having run since.
        let record = &fx.state().worktrees[&name];
        let Some(api_record) = record.processes.get("api") else {
            panic!("the crash was reconciled away: {:?}", record.processes);
        };
        match &api_record.phase {
            Phase::Failed { reason, .. } => assert!(
                reason.contains("process exited"),
                "the reason should say what happened: {reason}"
            ),
            other => panic!("a crash has to be recorded as failed, not {other:?}"),
        }
        // And the first read path after it agrees, which is what `status`
        // and the TUI row show.
        let state = refresh(&fx.paths).state;
        assert_eq!(
            state::aggregate_phase(&state.worktrees[&name]).map(|a| a.word()),
            Some("failed"),
            "status has to be able to see it"
        );

        stop(&fx.paths, &name, None).unwrap();
        assert!(fx.state().worktrees[&name].processes.is_empty());
    }

    #[test]
    fn stopping_one_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (orphan_name, orphan) = orphaned_sibling(&mut fx);
        let other = worktree_named(&fx, "feat/other");

        assert_eq!(
            stop(&fx.paths, &other, None).unwrap(),
            StopOutcome::NotRunning
        );
        assert!(
            !crate::process::group_alive(orphan.pgid),
            "a record reconcile could drop must have been signalled first"
        );
        // Signalled, and *kept*: the mutation advances phases before it
        // reconciles, so a leader that died without a read path noticing is
        // a crash the developer still gets to see. Its own worktree's stop
        // is what clears it.
        assert!(
            matches!(
                fx.state().worktrees[&orphan_name].processes["dev"].phase,
                Phase::Failed { .. }
            ),
            "the crash stays visible: {:?}",
            fx.state().worktrees[&orphan_name].processes["dev"].phase
        );
        stop(&fx.paths, &orphan_name, None).unwrap();
        assert!(
            fx.state().worktrees[&orphan_name].processes.is_empty(),
            "and only its own stop drops it"
        );
    }

    #[test]
    fn starting_one_worktree_signals_another_ones_orphan() {
        let mut fx = fixture();
        let (_orphan_name, orphan) = orphaned_sibling(&mut fx);
        let other = worktree_named(&fx, "feat/other");

        let outcome = start(&fx.paths, &fx.config, &other, None, &noop).unwrap();
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

    // Phase 2c review, finding 2. A `Failed` record survives `reconcile`
    // until its own worktree is acted on, and the sweep used to re-signal
    // its pgid on every mutation anywhere in the project — which, once
    // that pid has wrapped around, is an unrelated session leader being
    // SIGTERMed and then SIGKILLed, over and over.
    #[test]
    fn a_dead_groups_pgid_is_signalled_once_and_not_on_every_later_mutation() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        // Far above `kern.maxproc`, so `is_alive` is certainly false and
        // no real process can be behind either number.
        record
            .processes
            .insert("dev".to_string(), failed_record(4_000_001));
        // A process that is still alive is never signalled by the sweep,
        // swept flag or not.
        let mut live = fake_record(4_000_002);
        live.pid = std::process::id();
        record.processes.insert("web".to_string(), live);
        store.worktrees.insert("feat+one".to_string(), record);

        let signalled = std::cell::RefCell::new(Vec::new());
        let watch = |pgid: i32| {
            signalled.borrow_mut().push(pgid);
            Ok(())
        };

        sweep_orphaned_groups_with(&mut store, watch).unwrap();
        assert_eq!(
            *signalled.borrow(),
            vec![4_000_001],
            "the dead leader's group is signalled, the live one's is not"
        );

        sweep_orphaned_groups_with(&mut store, watch).unwrap();
        assert_eq!(
            *signalled.borrow(),
            vec![4_000_001],
            "a second mutation must not signal that pgid again"
        );
        assert!(
            store.worktrees["feat+one"].processes["dev"].swept,
            "and the record is what remembers it"
        );

        // A leader that died since is a different matter: it has never
        // been swept, so its group is signalled on the next mutation.
        store
            .worktrees
            .get_mut("feat+one")
            .unwrap()
            .processes
            .insert("api".to_string(), failed_record(4_000_003));
        sweep_orphaned_groups_with(&mut store, watch).unwrap();
        assert_eq!(
            *signalled.borrow(),
            vec![4_000_001, 4_000_003],
            "a freshly dead leader is still signalled"
        );
    }

    // A signal that did not go out has to be tried again: the flag records
    // that the group *was* signalled, not that it was looked at.
    #[test]
    fn a_group_that_could_not_be_signalled_is_not_recorded_as_swept() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record
            .processes
            .insert("dev".to_string(), failed_record(4_000_001));
        store.worktrees.insert("feat+one".to_string(), record);

        let err = sweep_orphaned_groups_with(&mut store, |_| anyhow::bail!("killpg refused"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("feat+one/dev"), "{err:#}");
        assert!(!store.worktrees["feat+one"].processes["dev"].swept);
    }

    // The flag lives in the state file, because the mutation that must not
    // re-signal the group is a later run of pando, not a later line of
    // this one. A state file written before the flag existed reads as
    // "never swept", which signals once more and is the safe direction.
    #[test]
    fn the_swept_flag_survives_a_real_mutation_and_defaults_to_false() {
        let fx = fixture();
        let crashed = worktree_named(&fx, "feat/crashed");
        let other = worktree_named(&fx, "feat/other");
        let mut store = state::load(&fx.paths.state_file()).unwrap();
        store
            .worktrees
            .entry(crashed.clone())
            .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(&crashed), true))
            .processes
            .insert("dev".to_string(), failed_record(4_000_001));
        state::save(&fx.paths.state_file(), &store).unwrap();
        let written = std::fs::read_to_string(fx.paths.state_file()).unwrap();
        assert!(
            !written.contains("swept"),
            "a record that was never swept writes nothing: {written}"
        );

        // A mutation on a *different* worktree: the sweep is what acts on
        // the crashed record, and the record itself has to survive it.
        stop(&fx.paths, &other, None).unwrap();
        let saved = fx.state();
        let record = &saved.worktrees[&crashed].processes["dev"];
        assert!(matches!(record.phase, Phase::Failed { .. }), "{record:?}");
        assert!(
            record.swept,
            "without this in the file, the next mutation signals that pgid all \
             over again — and every one after it"
        );

        // And a second mutation leaves it exactly as it is.
        stop(&fx.paths, &other, None).unwrap();
        assert!(fx.state().worktrees[&crashed].processes["dev"].swept);
    }

    /// A `Failed` record for a group that does not exist and a pid that
    /// cannot: the shape the sweep is about.
    fn failed_record(pgid: i32) -> ProcessRecord {
        ProcessRecord {
            phase: Phase::Failed {
                at: Utc::now(),
                reason: "process exited".to_string(),
            },
            ..fake_record(pgid)
        }
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
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        }
    }

    // Phase 2b review, finding 4. One flat list per worktree cannot say
    // which group opened which socket, and the URL rule needs exactly that.
    #[test]
    fn observed_ports_are_recorded_per_process_and_the_worktrees_list_is_their_union() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/trees/feat+one", true);
        record.processes.insert("web".to_string(), fake_record(101));
        record.processes.insert("api".to_string(), fake_record(102));
        store.worktrees.insert("feat+one".to_string(), record);

        let scans = BTreeMap::from([
            (101, Some(vec![17_342])),
            (102, Some(vec![17_343, 9876, 17_343])),
        ]);
        assert!(capture_observed_ports(&mut store, &scans));

        let record = &store.worktrees["feat+one"];
        assert_eq!(record.processes["web"].observed_ports, vec![17_342]);
        assert_eq!(
            record.processes["api"].observed_ports,
            vec![9876, 17_343],
            "sorted, and each port once"
        );
        assert_eq!(
            record.observed_ports,
            vec![9876, 17_342, 17_343],
            "the worktree's own list is the union, which is what the JSON shape publishes"
        );

        // A scan that could not run says nothing at all: the last good
        // answer stands rather than being cleared by a missing `lsof`.
        let unscannable = BTreeMap::from([(101, None), (102, None)]);
        assert!(!capture_observed_ports(&mut store, &unscannable));
        assert_eq!(
            store.worktrees["feat+one"].observed_ports,
            vec![9876, 17_342, 17_343]
        );

        // A process that is no longer up is listening on nothing, and its
        // last sighting is stale the moment it stops.
        store
            .worktrees
            .get_mut("feat+one")
            .unwrap()
            .processes
            .get_mut("web")
            .unwrap()
            .phase = Phase::Failed {
            at: Utc::now(),
            reason: "process exited".to_string(),
        };
        assert!(capture_observed_ports(&mut store, &scans));
        let record = &store.worktrees["feat+one"];
        assert!(record.processes["web"].observed_ports.is_empty());
        assert_eq!(record.observed_ports, vec![9876, 17_343]);
    }

    // Phase 2b review, finding 6. `start` and every read path have to hand
    // out the same URL for the same worktree, in every state it can be in.
    #[test]
    fn start_and_the_read_paths_agree_on_the_url_when_nothing_owns_web() {
        let mut fx = fixture();
        // `alpha` owns `srv` and `beta` owns `admin`: the alphabetically
        // first *process* and the alphabetically first *role* are different
        // answers, which is what made two commands disagree.
        for (process, role) in [("alpha", "srv"), ("beta", "admin")] {
            fx.config.processes.insert(
                process.to_string(),
                ProcessConfig {
                    cmd: "sleep 30".to_string(),
                    ports: Some(PortsSpec::List(vec![role.to_string()])),
                    ..Default::default()
                },
            );
        }
        let name = worktree_named(&fx, "feat/url2");

        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _g = guard(&report);
        let expected = format!("http://localhost:{}", report.ports["srv"]);
        assert_eq!(
            report.url.as_deref(),
            Some(expected.as_str()),
            "alpha comes first, so its first role is the worktree's URL"
        );
        assert_eq!(
            worktree_url(&fx.state().worktrees[&name]),
            report.url,
            "and the record every read path works from says the same"
        );

        // The ports survive a stop, so the URL does too — and it is still
        // the same one.
        stop(&fx.paths, &name, None).unwrap();
        assert_eq!(worktree_url(&fx.state().worktrees[&name]), report.url);
    }

    // ---- share -----------------------------------------------------------

    use crate::testutil::{FAKE_TUNNEL_URL, fake_cloudflared_failing, fake_cloudflared_publishing};

    /// Stops whatever a share started, even when an assertion panics first.
    struct ShareGuard(Option<ShareRecord>);

    impl Drop for ShareGuard {
        fn drop(&mut self) {
            if let Some(record) = &self.0 {
                let _ = tunnel::stop_share(record);
            }
        }
    }

    fn share_guard(fx: &Fx, name: &str) -> ShareGuard {
        ShareGuard(fx.state().worktrees.get(name).and_then(|r| r.share.clone()))
    }

    /// A worktree running a real listener on its `web` port, with a fake
    /// provider installed — the state every share test starts from.
    fn shared_fixture() -> Option<(Fx, String, Vec<Detached>, StartReport)> {
        if !python3_available() {
            eprintln!("skipping: python3 is needed for a process that really holds a port");
            return None;
        }
        let mut fx = fixture();
        with_dev(
            &mut fx,
            ProcessConfig {
                cmd: python_listener_template(),
                ports: Some(PortsSpec::List(vec!["web".to_string()])),
                ready: Some(ReadySpec {
                    role: Some("web".to_string()),
                    timeout_s: None,
                }),
                ..Default::default()
            },
        );
        fake_cloudflared_publishing(&fx.paths.home);
        let name = worktree_named(&fx, "feat/one");
        let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let guards = guard(&report);
        let port = report.ports["web"];
        assert!(
            wait_until(Duration::from_secs(20), || {
                refresh(&fx.paths);
                crate::ports::something_is_listening(port)
            }),
            "the listener never bound {port}"
        );
        // One refresh so the process is Running rather than Starting: a
        // share of something still coming up is refused on purpose.
        refresh(&fx.paths);
        Some((fx, name, guards, report))
    }

    /// A stand-in for the real proxy, which re-execs the running binary —
    /// inside a library test that is the test harness, which exits at once.
    /// This one is a detached process group with a pid, which is all the
    /// assertions here are about: that it is started, recorded, and taken
    /// down again when the share it belongs to fails.
    fn stub_proxy(
        paths: &PandoPaths,
        name: &str,
        listen: u16,
        _upstream: u16,
        _cookie: &str,
    ) -> Result<share_proxy::ProxySpawn> {
        let log_path = paths.log_file(name, share_proxy::PROXY_LOG);
        let spawn = proc::spawn_detached(SpawnOptions {
            shell_cmd: "exec sleep 300",
            cwd: &std::env::temp_dir(),
            log_file: &log_path,
            env: &[],
        })?;
        Ok(share_proxy::ProxySpawn {
            pid: spawn.pid,
            pgid: spawn.pgid,
            listen_port: listen,
            log_path,
        })
    }

    /// `share` as the CLI calls it, but with the proxy stubbed.
    fn share_stubbed(fx: &Fx, config: &Config, name: &str) -> Result<ShareOutcome> {
        let provider = tunnel::provider_for(config.share.provider.as_deref())?;
        share_with(
            &fx.paths,
            config,
            name,
            provider.as_ref(),
            &stub_proxy,
            &noop,
        )
    }

    /// A provider that is not installed.
    struct MissingProvider;

    impl tunnel::Provider for MissingProvider {
        fn name(&self) -> &'static str {
            "cloudflared"
        }
        fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
            bail!("cloudflared is not installed — `brew install cloudflared`")
        }
        fn start(&self, _: &PandoPaths, _: &str, _: u16) -> Result<tunnel::TunnelSpawn> {
            panic!("a missing provider must never be asked to start anything")
        }
    }

    /// A provider that is installed and whose tunnel fails anyway, the way a
    /// rate-limited quick tunnel does.
    struct FailingProvider;

    impl tunnel::Provider for FailingProvider {
        fn name(&self) -> &'static str {
            "cloudflared"
        }
        fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
            Ok(())
        }
        fn start(&self, _: &PandoPaths, _: &str, _: u16) -> Result<tunnel::TunnelSpawn> {
            bail!("cloudflared published no URL within 30s — tail: 429 Too Many Requests")
        }
    }

    #[test]
    fn share_refuses_a_worktree_that_is_not_running() {
        let fx = fixture();
        fake_cloudflared_publishing(&fx.paths.home);
        let name = worktree_named(&fx, "feat/one");

        let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("start it first"), "{message}");
        assert!(
            fx.state()
                .worktrees
                .get(&name)
                .is_none_or(|r| r.share.is_none()),
            "nothing may be recorded for a refused share"
        );
    }

    #[test]
    fn share_refuses_a_worktree_whose_process_has_stopped() {
        let Some((fx, name, guards, _)) = shared_fixture() else {
            return;
        };
        drop(guards);
        stop(&fx.paths, &name, None).unwrap();

        let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("not running"), "{err:#}");
    }

    #[test]
    fn share_without_an_auth_command_tunnels_straight_to_the_web_port() {
        let Some((fx, name, _guards, report)) = shared_fixture() else {
            return;
        };
        let outcome = share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _share = share_guard(&fx, &name);

        assert_eq!(outcome.public_url, FAKE_TUNNEL_URL);
        assert!(!outcome.pre_authed);
        assert!(!outcome.already);

        let record = fx.state().worktrees[&name].share.clone().unwrap();
        assert!(record.proxy_pid.is_none(), "no auth command, no proxy");
        assert!(record.proxy_port.is_none());
        assert_eq!(record.local_port, report.ports["web"]);
        assert!(crate::process::is_alive(record.tunnel_pid));
        assert_eq!(record.log_path, fx.paths.log_file(&name, "tunnel"));

        let log = std::fs::read_to_string(fx.paths.log_file(&name, "tunnel")).unwrap();
        assert!(
            log.contains(&format!("--url http://127.0.0.1:{}", report.ports["web"])),
            "the tunnel must point at the application itself: {log}"
        );
    }

    #[test]
    fn share_with_an_auth_command_runs_it_and_puts_a_proxy_in_front() {
        let Some((fx, name, _guards, report)) = shared_fixture() else {
            return;
        };
        // Written outside the worktree: a share must never make the
        // repository dirty, not even from a test's own script.
        let seen = fx.paths.home.join("auth-env.txt");
        let mut config = fx.config.clone();
        // Something only the process environment carries, so "it runs with
        // the process env" is a claim this test can really check.
        config.processes.get_mut("dev").unwrap().env =
            BTreeMap::from([("APP_SECRET".to_string(), "from-the-process-env".to_string())]);
        config.share.auth_cmd = Some(format!("env > {}; printf 'session=abc123'", seen.display()));

        let outcome = share_stubbed(&fx, &config, &name).unwrap();
        let _share = share_guard(&fx, &name);
        assert!(outcome.pre_authed);

        let record = fx.state().worktrees[&name].share.clone().unwrap();
        let proxy_port = record.proxy_port.expect("a proxy port");
        assert_eq!(
            fx.state().worktrees[&name].share_port,
            Some(proxy_port),
            "the proxy's port is remembered, so a later share reuses it"
        );
        assert!(crate::process::is_alive(record.proxy_pid.unwrap()));
        assert_eq!(
            record.local_port, report.ports["web"],
            "the record still says what is being shared, not what is in front of it"
        );

        let log = std::fs::read_to_string(fx.paths.log_file(&name, "tunnel")).unwrap();
        assert!(
            log.contains(&format!("--url http://127.0.0.1:{proxy_port}")),
            "the tunnel must point at the proxy, not the application: {log}"
        );

        // The process environment, plus the port.
        let env = std::fs::read_to_string(&seen).unwrap();
        assert!(
            env.contains(&format!("{ENV_SHARE_PORT}={proxy_port}")),
            "the auth command must be told the proxy's port: {env}"
        );
        assert!(env.contains(&format!("PANDO_NAME={name}")), "{env}");
        assert!(
            env.contains("APP_SECRET=from-the-process-env"),
            "the auth command runs with the same environment the process got: {env}"
        );
        assert_eq!(
            porcelain_status(&fx.worktrees_dir().join(&name)),
            Vec::<String>::new(),
            "the auth command must leave the worktree clean"
        );
    }

    #[test]
    fn a_failing_auth_command_fails_the_share_with_its_own_complaint() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("echo 'no session for you' >&2; exit 3".to_string());

        let err = share_stubbed(&fx, &config, &name).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("exited 3"), "{message}");
        assert!(message.contains("no session for you"), "{message}");
        assert!(
            fx.state().worktrees[&name].share.is_none(),
            "a share that failed before it started anything records nothing"
        );
        assert!(
            !fx.paths.log_file(&name, "tunnel").exists(),
            "nothing may have been spawned"
        );
    }

    #[test]
    fn an_auth_command_that_prints_nothing_usable_is_refused() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        for (cmd, expected) in [
            ("true", "printed nothing"),
            ("printf 'a\\nb'", "control character"),
        ] {
            let mut config = fx.config.clone();
            config.share.auth_cmd = Some(cmd.to_string());
            let err = share_stubbed(&fx, &config, &name).unwrap_err();
            assert!(
                format!("{err:#}").contains(expected),
                "{cmd:?} should be refused with {expected:?}: {err:#}"
            );
        }
    }

    #[test]
    fn a_second_share_hands_back_the_url_it_already_has() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let first = share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let _share = share_guard(&fx, &name);
        let pid = fx.state().worktrees[&name]
            .share
            .clone()
            .unwrap()
            .tunnel_pid;

        let second = share(&fx.paths, &fx.config, &name, &noop).unwrap();
        assert_eq!(second.public_url, first.public_url);
        assert!(second.already, "the second call opened nothing");
        assert_eq!(
            fx.state().worktrees[&name]
                .share
                .clone()
                .unwrap()
                .tunnel_pid,
            pid,
            "and the tunnel is the same one"
        );
    }

    #[test]
    fn unshare_stops_both_halves_and_clears_the_record() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("printf 'session=abc'".to_string());
        share_stubbed(&fx, &config, &name).unwrap();
        let record = fx.state().worktrees[&name].share.clone().unwrap();
        let (tunnel_pid, proxy_pid) = (record.tunnel_pid, record.proxy_pid.unwrap());

        unshare(&fx.paths, &name).unwrap();

        assert!(fx.state().worktrees[&name].share.is_none());
        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(tunnel_pid) && !crate::process::is_alive(proxy_pid)
        }));
        assert!(
            fx.state().worktrees[&name].share_port.is_some(),
            "the proxy's port is kept, so the next share reuses it"
        );
    }

    #[test]
    fn unshare_refuses_a_worktree_that_is_not_shared() {
        let fx = fixture();
        let name = worktree_named(&fx, "feat/one");
        let err = unshare(&fx.paths, &name).unwrap_err();
        assert!(format!("{err:#}").contains("not shared"), "{err:#}");
    }

    // A tunnel whose process is already gone must still unshare: the record
    // is the only thing holding a URL nobody can reach.
    #[test]
    fn unshare_clears_a_share_whose_tunnel_already_died() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let record = fx.state().worktrees[&name].share.clone().unwrap();
        crate::process::stop(record.tunnel_pgid, Duration::from_secs(5)).unwrap();

        unshare(&fx.paths, &name).unwrap();
        assert!(fx.state().worktrees[&name].share.is_none());
    }

    #[test]
    fn share_refuses_with_the_install_hint_when_the_provider_is_missing() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let err = share_with(
            &fx.paths,
            &fx.config,
            &name,
            &MissingProvider,
            &stub_proxy,
            &noop,
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("not installed"), "{message}");
        assert!(message.contains("brew install cloudflared"), "{message}");
        assert!(fx.state().worktrees[&name].share.is_none());
    }

    #[test]
    fn share_refuses_a_provider_pando_does_not_speak() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let mut config = fx.config.clone();
        config.share.provider = Some("ngrok".to_string());
        let err = share(&fx.paths, &config, &name, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("ngrok"), "{err:#}");
    }

    // The leak this guards: the proxy is spawned before the tunnel, and a
    // tunnel that never comes up leaves nothing recorded that could ever
    // find it again.
    #[test]
    fn a_tunnel_that_never_opens_takes_the_proxy_down_with_it() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("printf 'session=abc'".to_string());

        // The pid the proxy was given, captured as it is spawned: once the
        // share has failed, nothing records it, and that is the whole
        // point of this test.
        let spawned: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        let watched = |paths: &PandoPaths,
                       name: &str,
                       listen: u16,
                       upstream: u16,
                       cookie: &str|
         -> Result<share_proxy::ProxySpawn> {
            let spawn = stub_proxy(paths, name, listen, upstream, cookie)?;
            spawned.lock().unwrap().push(spawn.pid);
            Ok(spawn)
        };

        let err =
            share_with(&fx.paths, &config, &name, &FailingProvider, &watched, &noop).unwrap_err();
        assert!(format!("{err:#}").contains("no URL"), "{err:#}");

        assert!(fx.state().worktrees[&name].share.is_none());
        let pids = spawned.into_inner().unwrap();
        assert_eq!(pids.len(), 1, "a proxy was started before the tunnel");
        assert!(
            wait_until(Duration::from_secs(5), || !crate::process::is_alive(
                pids[0]
            )),
            "the proxy outlived the share that spawned it, with nothing left to find it"
        );
    }

    // The real cloudflared fails the same way, through the same path.
    #[test]
    fn a_provider_that_exits_fails_the_share_and_records_nothing() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        fake_cloudflared_failing(&fx.paths.home);
        let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
        assert!(
            format!("{err:#}").contains("Too Many Requests"),
            "the provider's own complaint is the diagnosis: {err:#}"
        );
        assert!(fx.state().worktrees[&name].share.is_none());
    }

    // Finding 2 end to end: a cloudflared that cannot even *reach*
    // Cloudflare logs the quick-tunnel API's own URL, and used to be
    // reported as a successful share at `https://api.trycloudflare.com`,
    // with a proxy left running behind a tunnel that was already dead.
    #[test]
    fn a_provider_that_cannot_reach_cloudflare_fails_the_share_and_leaves_no_proxy() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        crate::testutil::fake_cloudflared_api_error(&fx.paths.home);
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("printf 'session=abc'".to_string());

        let spawned: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        let watched = |paths: &PandoPaths,
                       name: &str,
                       listen: u16,
                       upstream: u16,
                       cookie: &str|
         -> Result<share_proxy::ProxySpawn> {
            let spawn = stub_proxy(paths, name, listen, upstream, cookie)?;
            spawned.lock().unwrap().push(spawn.pid);
            Ok(spawn)
        };
        let provider = tunnel::provider_for(None).unwrap();

        let err = share_with(
            &fx.paths,
            &config,
            &name,
            provider.as_ref(),
            &watched,
            &noop,
        )
        .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains("before publishing a URL"),
            "a request that failed is not a published URL: {message}"
        );
        assert!(
            message.contains("failed to request quick Tunnel"),
            "and the provider's own complaint is the diagnosis: {message}"
        );
        assert!(
            fx.state().worktrees[&name].share.is_none(),
            "nothing may be recorded for a share that never published"
        );
        let pids = spawned.into_inner().unwrap();
        assert_eq!(pids.len(), 1, "a proxy was started before the tunnel");
        assert!(
            wait_until(Duration::from_secs(5), || !crate::process::is_alive(
                pids[0]
            )),
            "a proxy was left behind a tunnel that never opened"
        );
    }

    // The Phase 3 critical, at this level: sharing must never move a port
    // the running application is being reached on.
    #[test]
    fn sharing_and_restarting_leave_every_port_where_it_was() {
        let Some((fx, name, guards, report)) = shared_fixture() else {
            return;
        };
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("printf 'session=abc'".to_string());
        share(&fx.paths, &config, &name, &noop).unwrap();
        let _share = share_guard(&fx, &name);

        assert_eq!(
            fx.state().worktrees[&name].ports,
            report.ports,
            "a share must not touch the application's ports"
        );

        drop(guards);
        stop(&fx.paths, &name, None).unwrap();
        let again = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _again = guard(&again);
        assert_eq!(
            again.ports, report.ports,
            "and neither must the start after it"
        );
        assert!(!again.reassigned);
    }

    // ---- a share that outlives what it points at -------------------------

    /// A share record whose two halves have the given pids.
    fn share_record_of(tunnel_pid: u32, proxy_pid: Option<u32>) -> ShareRecord {
        ShareRecord {
            tunnel_pid,
            tunnel_pgid: tunnel_pid as i32,
            public_url: "https://x.trycloudflare.com".into(),
            local_port: 17000,
            started_at: Utc::now(),
            log_path: PathBuf::from("tunnel.log"),
            proxy_pid,
            proxy_pgid: proxy_pid.map(|p| p as i32),
            proxy_port: proxy_pid.map(|_| 17005),
        }
    }

    fn state_with_share(share: ShareRecord) -> state::State {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/tmp/feat+one", true);
        record.share = Some(share);
        store.worktrees.insert("feat+one".to_string(), record);
        store
    }

    #[test]
    fn a_dead_tunnel_closes_the_share_and_signals_the_proxy_that_is_left() {
        let mut store = state_with_share(share_record_of(4242, Some(8484)));
        let signalled = std::sync::Mutex::new(Vec::new());

        let notices = sweep_dead_shares_with(
            &mut store,
            // The tunnel died; the proxy is still up.
            |pid| pid == 8484,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
        );

        assert_eq!(
            signalled.into_inner().unwrap(),
            vec![4242, 8484],
            "both groups are signalled before the record that names them is dropped"
        );
        assert!(store.worktrees["feat+one"].share.is_none());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("tunnel"), "{:?}", notices[0]);
        assert!(notices[0].contains("feat+one"), "{:?}", notices[0]);
    }

    #[test]
    fn a_dead_proxy_takes_the_tunnel_in_front_of_it_down() {
        let mut store = state_with_share(share_record_of(4242, Some(8484)));
        let signalled = std::sync::Mutex::new(Vec::new());

        let notices = sweep_dead_shares_with(
            &mut store,
            // The proxy died; the tunnel is still up, serving the login
            // screen the proxy existed to skip.
            |pid| pid == 4242,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
        );

        assert_eq!(signalled.into_inner().unwrap(), vec![4242, 8484]);
        assert!(store.worktrees["feat+one"].share.is_none());
        assert!(notices[0].contains("proxy"), "{:?}", notices[0]);
    }

    #[test]
    fn a_live_share_is_left_alone() {
        let mut store = state_with_share(share_record_of(4242, Some(8484)));
        let signalled = std::sync::Mutex::new(Vec::new());
        let notices = sweep_dead_shares_with(
            &mut store,
            |_| true,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
        );
        assert!(signalled.into_inner().unwrap().is_empty());
        assert!(notices.is_empty());
        assert!(store.worktrees["feat+one"].share.is_some());
    }

    // The record holds the only pgid anything can use to try again, so a
    // half that will not die keeps it.
    #[test]
    fn a_share_that_will_not_die_keeps_its_record_and_says_so() {
        let mut store = state_with_share(share_record_of(4242, Some(8484)));
        let notices =
            sweep_dead_shares_with(&mut store, |pid| pid == 8484, |_| bail!("would not stop"));

        assert!(
            store.worktrees["feat+one"].share.is_some(),
            "dropping it would leave a tunnel nothing can name"
        );
        assert!(notices[0].contains("unshare"), "{:?}", notices[0]);
    }

    #[test]
    fn a_share_without_a_proxy_is_swept_on_its_tunnel_alone() {
        let mut store = state_with_share(share_record_of(4242, None));
        let signalled = std::sync::Mutex::new(Vec::new());
        sweep_dead_shares_with(
            &mut store,
            |_| false,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
        );
        assert_eq!(signalled.into_inner().unwrap(), vec![4242]);
        assert!(store.worktrees["feat+one"].share.is_none());
    }

    #[test]
    fn refresh_closes_a_share_whose_tunnel_died_and_says_so_once() {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let record = fx.state().worktrees[&name].share.clone().unwrap();
        crate::process::stop(record.tunnel_pgid, Duration::from_secs(5)).unwrap();

        let refreshed = refresh(&fx.paths);
        assert!(refreshed.state.worktrees[&name].share.is_none());
        assert_eq!(refreshed.notices.len(), 1, "{:?}", refreshed.notices);
        assert!(refreshed.notices[0].contains("public URL is closed"));

        // And the record is gone from disk, so the next refresh has
        // nothing to repeat.
        assert!(refresh(&fx.paths).notices.is_empty());
    }

    // ---- a dead half of a share, on every path that drops records --------
    //
    // `reconcile` drops the record that holds the surviving half's pgid, and
    // it cannot signal anything. So every path that reaches it has to signal
    // first. `refresh` and `share` did; `start`, `stop <name>` and `stop`
    // did not, and a live cloudflared with a public URL — or a live proxy
    // holding the injected cookie in its environment — was left running with
    // nothing in pando able to name it again.

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum DeadHalf {
        Tunnel,
        Proxy,
    }

    /// Shares `name` with a proxy in front of it, then kills `dead` outright,
    /// leaving the other half running and only the record naming it.
    fn share_with_a_dead_half(fx: &Fx, name: &str, dead: DeadHalf) -> ShareRecord {
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some("printf 'session=abc'".to_string());
        share_stubbed(fx, &config, name).unwrap();
        let record = fx.state().worktrees[name].share.clone().unwrap();
        let (pid, pgid) = match dead {
            DeadHalf::Tunnel => (record.tunnel_pid, record.tunnel_pgid),
            DeadHalf::Proxy => (record.proxy_pid.unwrap(), record.proxy_pgid.unwrap()),
        };
        crate::process::stop(pgid, Duration::from_secs(5)).unwrap();
        assert!(
            !crate::process::is_alive(pid),
            "the {dead:?} half should be dead"
        );
        assert!(
            crate::process::group_alive(surviving_pgid(&record, dead)),
            "the other half has to still be running, or this test proves nothing"
        );
        record
    }

    fn surviving_pgid(record: &ShareRecord, dead: DeadHalf) -> i32 {
        match dead {
            DeadHalf::Tunnel => record.proxy_pgid.unwrap(),
            DeadHalf::Proxy => record.tunnel_pgid,
        }
    }

    fn assert_nothing_of_the_share_is_left(record: &ShareRecord, dead: DeadHalf) {
        let pgid = surviving_pgid(record, dead);
        assert!(
            wait_until(Duration::from_secs(5), || !crate::process::group_alive(
                pgid
            )),
            "the {dead:?} half died and the other one was left running in group {pgid}, \
             with the record that named it dropped"
        );
    }

    fn assert_the_caller_was_told(notices: &[String], name: &str) {
        assert!(
            notices
                .iter()
                .any(|n| n.contains(name) && n.contains("public URL is closed")),
            "the caller must be told the URL it had is gone: {notices:?}"
        );
    }

    #[test]
    fn a_start_of_another_worktree_signals_the_survivor_of_a_dead_tunnel() {
        a_start_of_another_worktree_signals_the_survivor(DeadHalf::Tunnel);
    }

    #[test]
    fn a_start_of_another_worktree_signals_the_survivor_of_a_dead_proxy() {
        a_start_of_another_worktree_signals_the_survivor(DeadHalf::Proxy);
    }

    fn a_start_of_another_worktree_signals_the_survivor(dead: DeadHalf) {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        // Created before the share dies: `new` reaches the same chokepoint,
        // and this test is about what `start` does.
        let other = worktree_named(&fx, "feat/two");
        let record = share_with_a_dead_half(&fx, &name, dead);
        let _cleanup = ShareGuard(Some(record.clone()));

        let said = std::sync::Mutex::new(Vec::<String>::new());
        let report = start(&fx.paths, &fx.config, &other, None, &|line| {
            said.lock().unwrap().push(line.to_string())
        })
        .unwrap();
        let _others = guard(&report);

        assert!(fx.state().worktrees[&name].share.is_none());
        assert_nothing_of_the_share_is_left(&record, dead);
        assert_the_caller_was_told(&said.into_inner().unwrap(), &name);
    }

    #[test]
    fn a_stop_of_another_worktree_signals_the_survivor_of_a_dead_tunnel() {
        a_stop_of_another_worktree_signals_the_survivor(DeadHalf::Tunnel);
    }

    #[test]
    fn a_stop_of_another_worktree_signals_the_survivor_of_a_dead_proxy() {
        a_stop_of_another_worktree_signals_the_survivor(DeadHalf::Proxy);
    }

    fn a_stop_of_another_worktree_signals_the_survivor(dead: DeadHalf) {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let other = worktree_named(&fx, "feat/two");
        let record = share_with_a_dead_half(&fx, &name, dead);
        let _cleanup = ShareGuard(Some(record.clone()));

        let said = std::sync::Mutex::new(Vec::<String>::new());
        super::stop(&fx.paths, &other, None, &|line| {
            said.lock().unwrap().push(line.to_string())
        })
        .unwrap();

        assert!(fx.state().worktrees[&name].share.is_none());
        assert_nothing_of_the_share_is_left(&record, dead);
        assert_the_caller_was_told(&said.into_inner().unwrap(), &name);
    }

    #[test]
    fn a_bare_stop_signals_the_survivor_of_a_dead_tunnel() {
        a_bare_stop_signals_the_survivor(DeadHalf::Tunnel);
    }

    #[test]
    fn a_bare_stop_signals_the_survivor_of_a_dead_proxy() {
        a_bare_stop_signals_the_survivor(DeadHalf::Proxy);
    }

    // No notice is asserted here: a bare `stop` stops the shared worktree
    // itself, so its share comes down as part of stopping it — which is the
    // documented behaviour and not news. What has to hold either way is
    // that neither half is left running.
    fn a_bare_stop_signals_the_survivor(dead: DeadHalf) {
        let Some((fx, name, _guards, _)) = shared_fixture() else {
            return;
        };
        let record = share_with_a_dead_half(&fx, &name, dead);
        let _cleanup = ShareGuard(Some(record.clone()));

        stop_all(&fx.paths).unwrap();

        assert!(fx.state().worktrees[&name].share.is_none());
        assert_nothing_of_the_share_is_left(&record, dead);
    }

    #[test]
    fn stop_takes_the_public_url_down_with_the_worktree() {
        let Some((fx, name, guards, _)) = shared_fixture() else {
            return;
        };
        share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let record = fx.state().worktrees[&name].share.clone().unwrap();

        drop(guards);
        stop(&fx.paths, &name, None).unwrap();

        assert!(
            fx.state().worktrees[&name].share.is_none(),
            "a stopped worktree never keeps a public URL"
        );
        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(record.tunnel_pid)
        }));
    }

    #[test]
    fn rm_takes_the_public_url_down_with_the_worktree() {
        let Some((fx, name, guards, _)) = shared_fixture() else {
            return;
        };
        share(&fx.paths, &fx.config, &name, &noop).unwrap();
        let record = fx.state().worktrees[&name].share.clone().unwrap();

        drop(guards);
        rm(&fx.paths, &name, false, true).unwrap();

        assert!(!fx.state().worktrees.contains_key(&name));
        assert!(wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(record.tunnel_pid)
        }));
    }

    // `--only` is about one process. Its siblings are still serving, so the
    // URL still points at something.
    #[test]
    fn stopping_one_process_of_several_leaves_the_share_up() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/tmp/feat+one", true);
        record.processes.insert("web".into(), fake_record(100));
        record.processes.insert("api".into(), fake_record(200));
        record.share = Some(share_record_of(4242, None));
        store.worktrees.insert("feat+one".into(), record);

        let mut projects = Vec::new();
        stop_recorded_with(
            &mut store,
            "feat+one",
            Some("api"),
            MissingOnly::IsAnError,
            |_| Ok(()),
            &mut projects,
        )
        .unwrap();

        assert!(
            store.worktrees["feat+one"].share.is_some(),
            "the web process is still serving what the URL points at"
        );
    }

    // …and a `--only` stop of the last one does take it down: a tunnel onto
    // nothing is worse than no tunnel.
    #[test]
    fn stopping_the_last_process_takes_the_share_down_even_with_only() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/tmp/feat+one", true);
        record.processes.insert("web".into(), fake_record(100));
        record.share = Some(share_record_of(4242, Some(8484)));
        store.worktrees.insert("feat+one".into(), record);

        let signalled = std::sync::Mutex::new(Vec::new());
        let mut projects = Vec::new();
        stop_recorded_with(
            &mut store,
            "feat+one",
            Some("web"),
            MissingOnly::IsAnError,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
            &mut projects,
        )
        .unwrap();

        assert!(store.worktrees["feat+one"].share.is_none());
        let signalled = signalled.into_inner().unwrap();
        assert!(
            signalled.contains(&4242) && signalled.contains(&8484),
            "{signalled:?}"
        );
    }

    // A worktree whose every process crashed still has a tunnel up.
    #[test]
    fn stopping_a_worktree_with_nothing_running_still_closes_its_share() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/tmp/feat+one", true);
        record.share = Some(share_record_of(4242, None));
        store.worktrees.insert("feat+one".into(), record);

        let signalled = std::sync::Mutex::new(Vec::new());
        let mut projects = Vec::new();
        let outcome = stop_recorded_with(
            &mut store,
            "feat+one",
            None,
            MissingOnly::IsAnError,
            |pgid| {
                signalled.lock().unwrap().push(pgid);
                Ok(())
            },
            &mut projects,
        )
        .unwrap();

        assert!(matches!(outcome, StopOutcome::Stopped(_)), "{outcome:?}");
        assert_eq!(signalled.into_inner().unwrap(), vec![4242]);
        assert!(store.worktrees["feat+one"].share.is_none());
    }

    #[test]
    fn a_share_that_will_not_stop_fails_the_stop_and_keeps_its_record() {
        let mut store = state::State::new();
        let mut record = WorktreeRecord::new("/tmp/feat+one", true);
        record.share = Some(share_record_of(4242, None));
        store.worktrees.insert("feat+one".into(), record);

        let mut projects = Vec::new();
        let err = stop_recorded_with(
            &mut store,
            "feat+one",
            None,
            MissingOnly::IsAnError,
            |_| bail!("would not stop"),
            &mut projects,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("share"), "{err:#}");
        assert!(store.worktrees["feat+one"].share.is_some());
    }

    // ---- restart ---------------------------------------------------------

    #[test]
    fn restart_stops_the_old_process_and_keeps_the_ports() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let first_pgid = first.started[0].record.pgid;
        let ports = first.ports.clone();
        drop(guard(&first));

        let second = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&second);
        assert!(!second.started_nothing());
        assert_ne!(second.started[0].record.pid, first.started[0].record.pid);
        assert!(!crate::process::group_alive(first_pgid));
        assert_eq!(
            second.ports, ports,
            "a restart keeps the URL the developer had open"
        );
        assert!(!second.reassigned);
    }

    #[test]
    fn restarting_something_that_was_never_started_just_starts_it() {
        let mut fx = fixture();
        with_dev(&mut fx, dev("sleep 30"));
        let name = worktree_named(&fx, "feat/one");
        let outcome = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        assert!(!outcome.started_nothing());
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

        let outcome = start(&fx.paths, &fx.config, "feat+one", None, &noop).unwrap();
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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

        let first = start(&fx.paths, &fx.config, "adopted", None, &noop).unwrap();
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

        stop(&fx.paths, "adopted", None).unwrap();
        let second = start(&fx.paths, &fx.config, "adopted", None, &noop).unwrap();
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

        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

    /// A workspace fixture: two apps, each with its own dev script, and an
    /// env example in which one points at the other.
    fn workspace_fixture() -> Fx {
        let fx = detectable_fixture(
            r#"{ "dev": "pnpm -r --parallel dev" }"#,
            "WEB_PORT=5173\nAPI_PORT=4000\nVITE_API_URL=http://localhost:4000\n",
        );
        std::fs::write(
            fx.root.join("package.json"),
            "{\n  \"workspaces\": [\"apps/*\"],\n  \"scripts\": { \"dev\": \"pnpm -r --parallel dev\" }\n}\n",
        )
        .unwrap();
        for (dir, manifest) in [
            ("apps/web", r#"{ "scripts": { "dev": "vite" } }"#),
            ("apps/api", r#"{ "scripts": { "dev": "node server.js" } }"#),
        ] {
            std::fs::create_dir_all(fx.root.join(dir)).unwrap();
            std::fs::write(fx.root.join(dir).join("package.json"), manifest).unwrap();
        }
        std::fs::write(
            fx.root.join("apps/web/vite.config.ts"),
            "export default {}\n",
        )
        .unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "workspace"]);
        fx
    }

    #[test]
    fn a_workspace_is_asked_about_once_and_yes_takes_the_per_app_form() {
        let fx = workspace_fixture();
        // What `--yes` does: the first option, recorded as a flag's choice
        // rather than a rule's.
        let (ask, asked) = scripted(vec![Answer::Auto(0)]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

        let questions = asked.borrow();
        assert_eq!(questions.len(), 1, "one question, not one per app");
        assert_eq!(questions[0].slot, Slot::Processes);
        assert_eq!(questions[0].preselect, Some(0));
        assert_eq!(
            questions[0].options.len(),
            2,
            "the per-app form and the root script"
        );

        assert_eq!(
            config.processes.keys().cloned().collect::<Vec<_>>(),
            vec!["api", "web"]
        );
        assert_eq!(config.processes["web"].cwd.as_deref(), Some("apps/web"));
        assert_eq!(
            config.processes["web"].env["VITE_API_URL"],
            "http://localhost:{port:api}"
        );

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(written.contains("[processes.web]"), "{written}");
        assert!(written.contains("[processes.api]"), "{written}");
        assert!(
            !written.contains("\n[dev]"),
            "nothing may write [dev] beside [processes]: {written}"
        );
        assert!(
            written.contains("--yes took the first of 2 options"),
            "a flag's choice says so: {written}"
        );
        // And the file pando wrote is one pando reads back.
        let loaded = crate::config::load(&fx.paths).unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.processes.len(), 2);

        // Second run: nothing left to ask.
        let again = resolve_process(&fx.paths, &loaded.config, &refuse, &noop).unwrap();
        assert_eq!(again.processes.len(), 2);
    }

    /// The workspace above plus a third app whose `dev` script is not a
    /// server. A `packages/*` library with `dev: "tsc -w"`, a codegen
    /// watcher, a queue consumer: it has a dev script, it has no framework
    /// rule, and the env example has no `WORKER_PORT` for it.
    fn workspace_with_worker_fixture() -> Fx {
        let fx = workspace_fixture();
        std::fs::create_dir_all(fx.root.join("apps/worker")).unwrap();
        std::fs::write(
            fx.root.join("apps/worker/package.json"),
            "{ \"name\": \"worker\", \"scripts\": { \"dev\": \"tsc -w\" } }\n",
        )
        .unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "worker"]);
        fx
    }

    // Phase 2b review, finding 2. Detection wrote a role and a readiness
    // rule for every app, including one with no way to be told which port
    // the role stands for. The process was then handed a reserved port it
    // never heard of, `advance_phases` waited thirty seconds for it, and a
    // worktree whose every process was healthy read `failed`.
    #[test]
    fn a_workspace_app_with_no_way_to_be_told_a_port_gets_neither_a_role_nor_readiness() {
        let fx = workspace_with_worker_fixture();
        let (ask, _asked) = scripted(vec![Answer::Auto(0)]);
        let mut config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

        assert_eq!(
            config.processes.keys().cloned().collect::<Vec<_>>(),
            vec!["api", "web", "worker"]
        );
        let worker = &config.processes["worker"];
        assert_eq!(
            worker.ports,
            Some(PortsSpec::List(Vec::new())),
            "no framework rule, no flag and no WORKER_PORT key: it cannot be told a port"
        );
        assert!(
            worker.ready.is_none(),
            "and there is nothing for readiness to wait for: {:?}",
            worker.ready
        );
        assert!(
            !worker.env.keys().any(|key| key.ends_with("PORT")),
            "{:?}",
            worker.env
        );
        // The apps that *can* be told one still are.
        assert_eq!(config.processes["web"].roles(), vec!["web"]);
        assert_eq!(config.processes["api"].roles(), vec!["api"]);
        assert_eq!(
            config.processes["api"]
                .ready
                .as_ref()
                .and_then(|r| r.role.as_deref()),
            Some("api")
        );
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            written.contains("ports = []"),
            "`no ports` is an answer written down, not a gap: {written}"
        );

        // And a process with no port to wait for is Running as soon as it
        // is alive, rather than failed thirty seconds later for not binding
        // one it was never told about.
        for process in config.processes.values_mut() {
            process.cmd = "sleep 30".to_string();
        }
        let name = worktree_named(&fx, "feat/one");
        let report = start(&fx.paths, &config, &name, None, &noop).unwrap();
        let _g = guard(&report);
        assert_eq!(
            fx.state().worktrees[&name].processes["worker"].ready_port,
            None,
            "a process that owns no role has no port to wait for"
        );
        assert!(
            wait_until(Duration::from_secs(10), || matches!(
                refresh(&fx.paths).state.worktrees[&name]
                    .processes
                    .get("worker")
                    .map(|p| &p.phase),
                Some(Phase::Running { .. })
            )),
            "the worker never reached running: {:?}",
            fx.state().worktrees[&name].processes["worker"].phase
        );
        stop(&fx.paths, &name, None).unwrap();
    }

    // The two cosmetic gaps the notes list, and the shape of the file as a
    // whole: what detection writes has to read like something a human would
    // have written, because the whole point is that a wrong guess is one
    // visible edit away.
    #[test]
    fn the_config_a_workspace_answer_writes_reads_as_a_human_would_write_it() {
        let fx = workspace_fixture();
        let (ask, _asked) = scripted(vec![Answer::Auto(0)]);
        resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            !written.lines().any(|line| line.trim() == "[processes]"),
            "a bare [processes] header is a line nobody would write: {written}"
        );
        // The file's own header mentions the marker, so only lines that
        // are not themselves comments count.
        assert_eq!(
            written
                .lines()
                .filter(|line| !line.starts_with('#') && line.contains("# answered:"))
                .count(),
            2,
            "one note per table answered, not one per key: {written}"
        );
        for header in ["[processes.api]", "[processes.web]"] {
            assert!(
                written
                    .lines()
                    .any(|line| line.starts_with(header) && line.contains("# answered:")),
                "{header} carries the note for its own keys: {written}"
            );
        }
        let loaded = crate::config::load(&fx.paths).expect("the file pando wrote must load");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.processes.len(), 2);
    }

    #[test]
    fn declining_the_workspace_question_keeps_the_root_script() {
        let fx = workspace_fixture();
        // The second option is the root script; the port question follows
        // it, because one process still needs a port.
        let (ask, asked) = scripted(vec![Answer::Choice(1), Answer::Choice(0)]);
        let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

        let questions = asked.borrow();
        assert_eq!(
            questions.iter().map(|q| q.slot).collect::<Vec<_>>(),
            vec![Slot::Processes, Slot::PortEnv]
        );
        assert_eq!(
            config.processes.keys().cloned().collect::<Vec<_>>(),
            vec!["dev"]
        );
        assert_eq!(config.processes["dev"].cmd, "pnpm dev");
        assert_eq!(config.processes["dev"].port_env()["WEB_PORT"], "{port:web}");

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(written.contains("[dev]"), "one process is [dev]: {written}");
        assert!(!written.contains("[processes."), "{written}");
    }

    #[test]
    fn a_lone_dev_with_no_command_is_filled_in_and_its_own_keys_are_kept() {
        let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
        fx.config.processes.insert(
            "dev".to_string(),
            ProcessConfig {
                cwd: Some(".".to_string()),
                env: BTreeMap::from([("GREETING".to_string(), "hello".to_string())]),
                ..Default::default()
            },
        );
        let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert_eq!(config.processes["dev"].cmd, "pnpm dev");
        assert_eq!(config.processes["dev"].roles(), vec!["web"]);
        assert_eq!(config.processes["dev"].cwd.as_deref(), Some("."));
        assert_eq!(config.processes["dev"].env["GREETING"], "hello");
    }

    // A `ports` the developer wrote is an answer, including the empty one.
    #[test]
    fn a_lone_dev_that_says_it_has_no_ports_keeps_none() {
        let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
        fx.config.processes.insert(
            "dev".to_string(),
            ProcessConfig {
                ports: Some(PortsSpec::List(Vec::new())),
                ..Default::default()
            },
        );
        let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
        assert_eq!(
            config.processes["dev"].cmd, "pnpm dev",
            "the command is filled"
        );
        assert!(
            config.processes["dev"].roles().is_empty(),
            "and the ports are left exactly as they were"
        );
    }

    // The workspace question is only for a workspace. Anywhere else the
    // dev-command question is the one asked, as it was before.
    #[test]
    fn a_project_that_is_not_a_workspace_is_never_asked_about_processes() {
        let fx = detectable_fixture(
            r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
            "PORT=3000\n",
        );
        let (ask, asked) = scripted(vec![Answer::Choice(1)]);
        resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert_eq!(
            asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
            vec![Slot::DevCmd],
            "a wrapper script with no workspace behind it is still one process"
        );
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

    // ---- the services slot -----------------------------------------------

    /// [`detectable_fixture`] with a compose file, which is what the
    /// services slot is proposed from.
    fn compose_fixture(compose: &str, env_example: &str) -> Fx {
        let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, env_example);
        std::fs::write(fx.root.join("docker-compose.yml"), compose).unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "compose"]);
        fx
    }

    /// The isolating form of the resolver: on a plain start the services
    /// question is silent, and these are all about what it asks.
    fn resolve_isolating(paths: &PandoPaths, config: &Config, ask: Ask<'_>) -> Result<Config> {
        super::resolve_process(paths, config, true, ask, &noop)
    }

    const TWO_DATABASES: &str = "services:\n  \
         db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n  \
         db_test:\n    image: postgres:16\n    ports: [\"5433:5432\"]\n";

    // A dev database beside a test one is an ordinary layout, and both of
    // them resolve to `DATABASE_URL` through the image's prefixes. Taking
    // that as decided pointed the app — and the migration hook — at the
    // test database, and left the real one with nothing addressing it.
    #[test]
    fn two_services_that_would_claim_one_env_key_are_asked_about_rather_than_guessed() {
        let fx = compose_fixture(
            TWO_DATABASES,
            "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n\
             TEST_DATABASE_URL=postgres://acme:acme@localhost:5433/acme_test\n",
        );
        let (ask, asked) = scripted(vec![Answer::Many(vec![0, 1])]);
        let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

        assert_eq!(
            asked.borrow().len(),
            1,
            "a second database pando cannot address is a question, not a guess"
        );
        let question = &asked.borrow()[0];
        assert_eq!(question.slot, Slot::Services);
        assert_eq!(
            question.checked,
            vec![0],
            "only the one a rule really resolved starts ticked"
        );
        assert!(
            question.options[1].1.contains("DATABASE_URL"),
            "and the other says whose key it would have taken: {}",
            question.options[1].1
        );

        let crate::config::ServiceConfig::Compose { env, include, .. } = &config.services[0] else {
            panic!("a compose entry");
        };
        assert_eq!(include, &vec!["db".to_string(), "db_test".to_string()]);
        assert_eq!(
            env.get("DATABASE_URL").map(String::as_str),
            Some("db"),
            "the key belongs to the service that claimed it first: {env:?}"
        );
        assert_eq!(env.len(), 1, "and no key is written twice: {env:?}");
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(!written.contains("DATABASE_URL = \"db_test\""), "{written}");
    }

    /// A compose file with one service nothing in the env example names.
    const ONE_UNNAMED_CACHE: &str =
        "services:\n  cache:\n    image: redis:7\n    ports: [\"6379:6379\"]\n";

    // "None of them" is an answer, and an answer that is not written down
    // is asked again on every start — exit 3 for a script, with no way to
    // answer it except editing TOML by hand.
    #[test]
    fn answering_none_to_the_services_question_is_written_down_as_no_services() {
        let fx = compose_fixture(ONE_UNNAMED_CACHE, "PORT=3000\n");
        let (ask, asked) = scripted(vec![Answer::None]);
        let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();
        assert_eq!(asked.borrow().len(), 1);

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(written.contains("[[services]]"), "{written}");
        assert!(
            written.contains("include = []"),
            "an empty list, so it reads as answered rather than missing: {written}"
        );
        assert_eq!(config.services.len(), 1);

        // And asked once: a second resolve has nothing left to ask about.
        let again = resolve_isolating(&fx.paths, &config, &refuse).unwrap();
        assert_eq!(again.services.len(), 1);
    }

    #[test]
    fn yes_with_nothing_the_rules_resolved_is_written_down_the_same_way() {
        let fx = compose_fixture(ONE_UNNAMED_CACHE, "PORT=3000\n");
        let (ask, _) = scripted(vec![Answer::Auto(0)]);
        let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            written.contains("include = []"),
            "--yes that takes nothing still answers the question: {written}"
        );
        assert!(
            written.contains("--yes took the 0 of 1"),
            "and says a flag did it, not a human: {written}"
        );
        let again = resolve_isolating(&fx.paths, &config, &refuse).unwrap();
        assert_eq!(again.services.len(), 1);
    }

    /// A docker that answers nothing, which is what a machine with no
    /// daemon looks like to `docker compose config`. Installed so the test
    /// below exercises the fallback rather than this machine's Docker.
    fn docker_that_cannot_answer(paths: &PandoPaths) {
        use std::os::unix::fs::PermissionsExt;
        let bin = paths.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("docker");
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // `extends:` and a top-level `include:` are common in real
    // repositories, and pando's own reader follows neither. Reading half a
    // file and concluding "this project has no services pando can address"
    // is the one answer that is certainly wrong.
    #[test]
    fn a_compose_file_pando_cannot_read_whole_is_asked_about_rather_than_passed_over() {
        let fx = compose_fixture(
            "services:\n  db:\n    extends:\n      file: base.yml\n      service: template\n",
            "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
        );
        fx.paths.ensure_home().unwrap();
        docker_that_cannot_answer(&fx.paths);

        let (ask, asked) = scripted(vec![Answer::None]);
        resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();
        assert_eq!(
            asked.borrow().len(),
            1,
            "a file pando could not read whole is a question, not silence"
        );
        let question = &asked.borrow()[0];
        assert_eq!(question.slot, Slot::Services);
        assert_eq!(question.options.len(), 1);
        assert!(
            question.options[0].1.contains("extends"),
            "and it says why it could not decide: {}",
            question.options[0].1
        );
        assert!(
            question.checked.is_empty(),
            "nothing pando read through an unfollowed key starts ticked"
        );
    }

    // `rewrite` can put a port into a URL or replace a bare number. A bare
    // host name has nowhere to put one, so proposing a `_HOST` key wrote a
    // mapping that could never be satisfied — and the failed start left the
    // worktree recorded as isolated, so it could not be started at all.
    #[test]
    fn a_host_key_is_never_proposed_because_a_port_cannot_be_put_into_one() {
        let fx = compose_fixture(
            "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
            "PORT=3000\nDB_HOST=db\n",
        );
        let (ask, _) = scripted(vec![Answer::Auto(0)]);
        let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            !written.contains("DB_HOST"),
            "a key pando cannot rewrite is not a mapping it may write: {written}"
        );
        let crate::config::ServiceConfig::Compose { env, .. } = &config.services[0] else {
            panic!("a compose entry");
        };
        assert!(env.is_empty(), "{env:?}");

        // And the project still starts, which is the whole point.
        let name = super::new(&fx.paths, &config, "feat/h", None, &noop).unwrap();
        let report = start(&fx.paths, &config, &name, None, &noop).unwrap();
        assert!(report.ports.contains_key("web"));
        let _ = stop(&fx.paths, &name, None);
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.started[0].record.pgid;
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.started[0].record.pgid;
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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

        let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        assert!(second.started_nothing());
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let pgid = outcome.started[0].record.pgid;

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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        let _guard = guard(&outcome);
        let port = outcome.ports["web"];

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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
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
