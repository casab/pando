//! Project hooks, and the probes that watch what they touch.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::hooks;
use crate::paths::PandoPaths;
use crate::state::{self, WorktreeRecord};
use crate::template;

use super::runtime::with_prelude;

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
        fingerprint: crate::catalog::package_managers::lockfiles()
            .iter()
            .map(|l| l.to_string())
            .collect(),
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
pub(super) fn run_probes(
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
pub(super) fn porcelain_status(worktree: &Path) -> Vec<String> {
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
///
/// Public because `doctor` reports the same thing at rest that a start
/// says while it happens, and two spellings of it would drift.
pub fn matched_nothing(worktree: &Path, hook: &config::HookConfig) -> String {
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
pub(super) fn pando_env(
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
