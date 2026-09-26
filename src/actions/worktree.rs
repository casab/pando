//! Worktree creation and removal: `new`, `rm`, `ls`, `path`, the files
//! provisioned into a new worktree, and the git questions asked along the
//! way.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{self, Config, ProvisionMode};
use crate::paths::PandoPaths;
use crate::state::{self, WorktreeRecord};
use crate::worktree::{self, PrInfo, Worktree};

use super::hooks::{HookContext, run_hooks};
use super::lifecycle::{StopOutcome, stop_recorded, sweep_orphaned_groups};
use super::namespaced::drop_namespaces;
use super::refresh::refresh;
use super::services::{clear_native_sockets, compose_projects, docker_down_for, remove_containers};
use crate::remedy;

/// What `new` says after the branch when the worktree was made and only
/// its install step failed. The worktree is kept, so a caller may treat
/// it as made.
pub const CREATED_BUT_INSTALL_FAILED: &str = "was created, but its install step failed";

/// What `new` says when another command recorded the worktree while git
/// checked it out. The worktree is kept here too, so a caller may treat
/// it as made.
pub const KEPT_OVER_RACED_RECORD: &str = "the worktree and that record are kept";

pub use crate::worktree::sanitize_branch_to_dir;

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
/// created and say so in the error. The one refusal that keeps the
/// worktree is over a record of it another command wrote while git
/// checked it out: that command may be running something in it. That
/// record is marked as pando's, and the error says what the worktree did
/// not get.
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
        // A prunable entry is one whose directory is gone, and "already
        // exists at" a path that is not there says nothing about the one
        // command that clears it.
        if found.prunable {
            bail!(
                "git still lists a worktree named {dir_name:?} at {}, but a prunable one ({}) — \
                 `pando rm {}` clears that entry, and then this can run again",
                found.path.display(),
                found
                    .prunable_reason
                    .as_deref()
                    .unwrap_or("its directory is gone"),
                found.display_name()
            );
        }
        bail!(
            "a worktree named {dir_name:?} already exists at {}",
            found.path.display()
        );
    }

    // The cheap early refusal: a project that needs an untracked,
    // non-ignored file is refused with the path named, not fixed up. The
    // worktree gets asked again once it exists, because it may have a
    // different `.gitignore` checked out.
    for rel in config.project.provision_paths() {
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
    // Not held through the checkout, which runs the repository's filters
    // and hooks — an LFS download takes minutes, and every `ls` and every
    // TUI worker would wait on it.
    let left = {
        let _lock = state::lock(&paths.lock_file())?;
        let mut store = state::load(&paths.state_file())?;
        let loaded = store.clone();
        // Under the lock, so the porcelain read cannot race a concurrent
        // `new` whose record is already saved but whose worktree this
        // process has not seen yet. The sweep signals only groups whose
        // leader is dead: a record of a worktree removed outside pando while
        // its processes still run is dropped with them unsignalled. A known
        // gap, left because a path comparison that went wrong here would
        // kill a healthy server.
        for notice in sweep_orphaned_groups(&mut store)? {
            progress(&notice);
        }
        drop_stale_worktree_records(&mut store, &root);
        if store != loaded {
            state::save(&paths.state_file(), &store)?;
        }
        // What the record under this name is as the lock goes: none, unless
        // a porcelain that could not be read kept a stale one.
        store.worktrees.get(&dir_name).cloned()
    };

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
    // stderr, which would paint over the TUI's alternate screen. And never
    // prompting, like the fetches: the checkout runs the LFS filter, whose
    // download can ask for a password — over the TUI, whose keys then went
    // nowhere, and for ever.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = cmd.output().context("spawn git worktree add")?;
    if !out.status.success() {
        bail!("git worktree add failed: {}", git_failure_reason(&out));
    }

    // Past this point the worktree exists, so every failure has something to
    // undo before it is reported. The state is read again under the lock:
    // other commands have saved theirs while git checked out. And the undo
    // runs with the lock still held: let go first, a `start` waiting on it
    // could record a dev server in the worktree the undo then removes, and
    // the next sweep drops that record as stale with the server running.
    let undo = |e| unwind_new(&root, &target, branch, &source, e);
    let lock = state::lock(&paths.lock_file()).map_err(undo)?;
    let mut store = state::load(&paths.state_file()).map_err(undo)?;
    // A `start` from another terminal can find the worktree while git is
    // still checking it out, and record its dev server and ports under
    // this name. Written over, they drop out of pando's state while they
    // run; unwound, the worktree goes from under them. So neither: the
    // worktree is kept, and so is what that command recorded in it.
    if store.worktrees.get(&dir_name) != left.as_ref() {
        return Err(refuse_over_raced_record(
            paths, config, &mut store, &dir_name, branch, &target,
        ));
    }
    if !config.project.provision_paths().is_empty() {
        progress("provisioning");
    }
    provision_worktree_files(paths, config, &target, progress).map_err(undo)?;
    let canonical = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    store
        .worktrees
        .insert(dir_name.clone(), WorktreeRecord::new(canonical, true));
    state::save(&paths.state_file(), &store).map_err(undo)?;
    // The lock goes before the install runs: `npm ci` takes minutes, and
    // holding the state lock through it would stall every `ls` and freeze
    // the TUI's tick.
    drop(lock);

    // A failed install keeps the worktree. The branch is checked out, the
    // files are provisioned, and the next `start` tries the install again —
    // so the error is worth an exit code, but not an unwind.
    //
    // No ports yet: they are allocated at `start`, so a create hook that
    // names one fails here by name rather than silently rendering the
    // wrong number. Almost none do; the install step never does.
    // Said only when there is something to install: a project with no
    // install command and no create hook used to print it anyway.
    let installs = has_install_step(config);
    for line in uninitialised_submodules(&target) {
        progress(&line);
    }
    if installs {
        progress("installing");
    }
    let no_ports: BTreeMap<String, u16> = BTreeMap::new();
    let no_services: BTreeMap<String, String> = BTreeMap::new();
    let ctx = HookContext {
        name: &dir_name,
        branch: Some(branch),
        worktree: &target,
        ports: &no_ports,
        service_env: &no_services,
        own_data: false,
        not_own: None,
    };
    run_hooks(paths, config, config::HookPoint::Create, &ctx, progress)
        .with_context(|| format!("{branch} {CREATED_BUT_INSTALL_FAILED}"))?;
    Ok(dir_name)
}

/// Whether `new` has an install step to run: the project's install
/// command, or a hook after create.
fn has_install_step(config: &Config) -> bool {
    config
        .project
        .install
        .as_deref()
        .is_some_and(|cmd| !cmd.trim().is_empty())
        || config
            .hooks
            .iter()
            .any(|hook| hook.after == config::HookPoint::Create)
}

/// The refusal `new` gives over a record another command wrote while git
/// checked the worktree out, having marked that record as pando's.
///
/// It did: the worktree is the one `new` just created. Left as a start
/// writes it, the record is an adopted worktree's, and `rm` and the TUI
/// would treat it as not pando's to remove. Only a record of this
/// directory is marked — one under the same name for another path is not
/// this worktree's. The error names what `new` did not give the worktree,
/// and the commands that do.
fn refuse_over_raced_record(
    paths: &PandoPaths,
    config: &Config,
    store: &mut state::State,
    dir_name: &str,
    branch: &str,
    target: &Path,
) -> anyhow::Error {
    let head = format!(
        "{branch} was checked out at {}, but another pando command recorded {dir_name:?} \
         while git was checking it out — {KEPT_OVER_RACED_RECORD}",
        target.display()
    );
    if let Some(record) = store.worktrees.get_mut(dir_name)
        && !record.created_by_pando
        && crate::paths::resolve_for_compare(&record.path)
            == crate::paths::resolve_for_compare(target)
    {
        record.created_by_pando = true;
        if let Err(e) = state::save(&paths.state_file(), store) {
            return e.context(head);
        }
    }
    // What provisioning would have written: the paths not there yet that
    // have something to come from.
    let missing: Vec<&str> = config
        .project
        .provision_paths()
        .iter()
        .filter(|rel| !target.join(rel).exists() && provision_source(paths, config, rel).is_some())
        .map(String::as_str)
        .collect();
    let lacks = match (missing.is_empty(), has_install_step(config)) {
        (false, true) => format!(
            "its provisioned files ({}) or its install step",
            missing.join(", ")
        ),
        (false, false) => format!("its provisioned files ({})", missing.join(", ")),
        (true, true) => "its install step".to_string(),
        (true, false) => {
            return anyhow::anyhow!(
                "{head}, and there was nothing to provision or install into it"
            );
        }
    };
    anyhow::anyhow!(
        "{head}, but it did not get {lacks}; `pando rm {branch}` and then `pando new {branch}` \
         make it again with nothing missing"
    )
}

/// What `new` says about submodules it left empty, one line, or nothing.
///
/// `git worktree add` checks out the superproject only, so every submodule
/// of a new worktree is an empty directory — and a build that needs one
/// fails far from the cause. pando does not fill them: `submodule update`
/// clones into `.git`, beyond what `worktree add` records, which Invariant
/// 1 leaves to the developer. So it says which, and the command that does.
pub(super) fn uninitialised_submodules(worktree: &Path) -> Vec<String> {
    if !worktree.join(".gitmodules").is_file() {
        return Vec::new();
    }
    let Ok(out) = crate::project::git(worktree, ["submodule", "status"]) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    // `-<sha> <path>` is a submodule that was never initialised here.
    let empty: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('-'))
        .filter_map(|rest| rest.split_whitespace().nth(1))
        .map(str::to_string)
        .collect();
    if empty.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "submodules left empty: {} — pando does not fill them; `git -C {} submodule update \
         --init --recursive` does, if this worktree needs them",
        empty.join(", "),
        worktree.display()
    )]
}

/// The first thing `git worktree remove` would refuse over, when there is
/// one: a tracked file that differs, or an untracked file the project does
/// not ignore. `None` means git has no objection.
///
/// A prunable worktree has no directory left to look in, and clearing its
/// entry removes nothing, so there is nothing to refuse.
///
/// A git that could not answer — timed out, failed, never ran — is an
/// error, never "clean": `rm` acts on this answer by stopping processes
/// and removing volumes, and only then finds out from `git worktree
/// remove` that the tree was dirty after all.
///
/// `--no-optional-locks`, as `worktree::is_dirty` has: a plain status
/// refreshes a stale index, which takes `index.lock` and rewrites the
/// index inside `.git` — of a worktree `rm --yes` may not own, and one a
/// refused `rm` leaves in place.
fn dirty_entry(worktree: &Worktree) -> Result<Option<String>> {
    if worktree.prunable {
        return Ok(None);
    }
    let out = crate::project::git(
        &worktree.path,
        ["--no-optional-locks", "status", "--porcelain"],
    )?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|entry| entry.trim().to_string()))
}

/// The submodule `git worktree remove` would refuse over, when there is
/// one: without `--force`, git removes no worktree with a submodule checked
/// out in it, or with a `modules` directory in its git directory, however
/// clean its status is. `new` itself says how to fill a worktree's
/// submodules, so a clean worktree that has them is an ordinary one.
///
/// A prunable worktree has nothing to refuse, and a git that could not
/// answer is an error, both as in [`dirty_entry`].
fn submodule_in(worktree: &Worktree) -> Result<Option<String>> {
    if worktree.prunable {
        return Ok(None);
    }
    let answer = |args: &[&str]| -> Result<String> {
        let out = crate::project::git(&worktree.path, args)?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // `<mode> <object> <stage>\t<path>`, and a submodule's mode is 160000.
    // git counts one as checked out when its directory has a `.git`.
    let index = answer(&["ls-files", "--stage", "-z"])?;
    let checked_out = index
        .split('\0')
        .filter_map(|entry| entry.split_once('\t'))
        .filter(|(meta, _)| meta.starts_with("160000 "))
        .map(|(_, path)| path)
        .find(|path| worktree.path.join(path).join(".git").exists());
    if let Some(path) = checked_out {
        return Ok(Some(path.to_string()));
    }
    let modules =
        PathBuf::from(answer(&["rev-parse", "--absolute-git-dir"])?.trim()).join("modules");
    Ok(modules.is_dir().then(|| modules.display().to_string()))
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
///
/// A worktree git no longer lists — its directory deleted and `git
/// worktree prune` run — that pando still has a record for is removed the
/// same way, less the git removal: what the record names is taken down
/// and the record forgotten. Otherwise the record would wait for an
/// unrelated `new` to drop it, and nothing would take down what it named.
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
    let target = discovery.worktrees.iter().find(|w| w.name == name);
    // Read without the lock, and before the home is made: this only asks
    // whether there is anything to remove at all.
    if target.is_none()
        && !state::load(&paths.state_file())
            .is_ok_and(|store| forgotten_record(&discovery, &store, name).is_some())
    {
        bail!("no worktree named {name:?}");
    }

    // Locked worktrees are always refused. pando never unlocks, and never
    // passes --force twice to talk git out of it.
    if let Some(target) = target
        && target.locked
    {
        let reason = target
            .lock_reason
            .as_deref()
            .unwrap_or("no reason recorded");
        bail!(
            "{} is locked ({reason}) — unlock it with `git worktree unlock` first",
            target.display_name()
        );
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
    // And saved before any of the refusals below: the sweep marked every
    // group it signalled, and a refused `rm` that forgot that signalled
    // them all again on the next mutation.
    state::save(&paths.state_file(), &store)?;
    let (shown, recorded_at) = match target {
        Some(target) => {
            drop_stale_worktree_records(&mut store, paths.root());
            (target.display_name(), target.path.clone())
        }
        // Not dropped as stale: that record is the one this removes.
        None => match forgotten_record(&discovery, &store, name) {
            Some(record) => {
                progress(&format!(
                    "git no longer lists {name} — taking down what pando still has for it"
                ));
                (name.to_string(), record.path.clone())
            }
            None => bail!("no worktree named {name:?}"),
        },
    };
    let created_by_pando = store.worktrees.get(name).is_some_and(|r| {
        r.created_by_pando && target.is_none_or(|target| record_is_for(r, target))
    });
    if !created_by_pando && !yes {
        bail!(
            "pando did not create {shown} ({}) — {}",
            recorded_at.display(),
            remedy::REMOVE_ANYWAY
        );
    }

    // git's own refusal is the last one, so it is asked first: stopping the
    // dev server and *then* being told the worktree stays leaves a process
    // that is gone, a worktree that is not, and a record blaming the
    // process for pando's kill. The questions are the two git asks without
    // `--force`, in its order; ignored files never block a removal and do
    // not show here.
    if !force && let Some(target) = target {
        match submodule_in(target) {
            Ok(None) => {}
            Ok(Some(what)) => bail!(
                "{shown} has submodules in it ({what}), and git removes such a worktree only \
                 when forced — nothing was stopped or removed; {}",
                remedy::DISCARD_CHANGES
            ),
            Err(e) => bail!(
                "could not tell whether {shown} has submodules in it: {e:#} — nothing was \
                 stopped or removed; check it with `git submodule status`, or {}",
                remedy::DISCARD_CHANGES
            ),
        }
        match dirty_entry(target) {
            Ok(None) => {}
            Ok(Some(entry)) => bail!(
                "{shown} contains modified or untracked files ({entry}) — commit or remove \
                 them, or {}",
                remedy::DISCARD_CHANGES
            ),
            // Before anything destructive: not knowing is not "clean".
            Err(e) => bail!(
                "could not tell whether {shown} has changes: {e:#} — nothing was stopped or \
                 removed; check it with `git status`, or {}",
                remedy::DISCARD_CHANGES
            ),
        }
    }

    // And Docker's, for the same reason. `rm` is what takes a worktree's
    // compose volumes with it, and the record naming the project is about
    // to go: removed while the daemon is down, the volumes survive with
    // nothing in pando able to find them again. So that is refused unless
    // forced, before anything is stopped — and forced, it goes ahead and
    // says how to remove them by hand.
    if !force
        && let Some(record) = store.worktrees.get(name)
        && let Some((project, how)) = docker_down_for(paths, &compose_projects(record))
    {
        bail!(
            "Docker {how}, so the services of {shown} cannot be removed with it — {}; \
             its data volumes then stay until `docker compose -p {project} down -v`",
            remedy::REMOVE_WITHOUT_DOCKER
        );
    }

    // Whatever is running goes next. A dev server whose working directory
    // has just been deleted is not a process anyone can do anything with,
    // and `rm` removes the record that is the only way to find it again.
    let mut projects: Vec<String> = Vec::new();
    // Said before it happens: `rm` of a running worktree takes its dev
    // server down, and a bare "removed" afterwards hid that it had.
    let running: Vec<String> = store
        .worktrees
        .get(name)
        .map(|record| record.processes.keys().cloned().collect())
        .unwrap_or_default();
    if !running.is_empty() {
        progress(&format!("stopping {}", running.join(", ")));
    }
    let stopped = stop_recorded(&mut store, name, None, &mut projects)?;

    // The containers, the network, and the volumes, before git is asked to
    // remove anything: the record that names the compose project is about
    // to be dropped, and a `down -v` that failed after the worktree was
    // gone would leave a database nothing could ever find. Saved first, so
    // a retry of `rm` has the record it needs.
    //
    // Forced past a Docker that cannot be asked, it is not asked again: a
    // wedged daemon would hold `down -v` — under the state lock — for the
    // whole teardown deadline and then fail the removal `--force` was
    // passed to get. Said instead, with the command that finishes it.
    if !projects.is_empty() {
        state::save(&paths.state_file(), &store)?;
        match force.then(|| docker_down_for(paths, &projects)).flatten() {
            Some((_, how)) => {
                for project in &projects {
                    progress(&format!(
                        "Docker {how}, so the services of {project} could not be removed — \
                         their data volumes survive; once Docker is up, `docker compose -p \
                         {project} down -v` removes them"
                    ));
                }
            }
            None => remove_containers(paths, &projects, progress).with_context(|| {
                format!("{shown} was left in place; its services could not be taken down")
            })?,
        }
    }

    // Nothing is unlinked first. Verified against git 2.51: an ignored file
    // does not block `git worktree remove`, and `--force` does not follow a
    // symlink out of the worktree — so unlinking bought nothing, and a
    // removal git then refused (a dirty tree without `--force`) left the
    // worktree alive without the `.env` pando had provisioned for it.
    //
    // Always run, even when the directory is already gone: the same command
    // clears a prunable entry, and only that one. `git worktree prune` is
    // global and would sweep entries pando has no business touching. A
    // worktree git no longer lists has nothing left for it to remove.
    if let Some(target) = target {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(paths.root()).args(["worktree", "remove"]);
        if force {
            cmd.arg("--force");
        }
        cmd.arg(target.path.to_str().context("worktree path is not utf-8")?);
        let out = cmd.output().context("spawn git worktree remove")?;
        if !out.status.success() {
            // Anything that was running has already been stopped by now
            // and that cannot be taken back, so the cleared record is saved
            // rather than left behind to resurface as a phantom failure —
            // and the message says what actually happened.
            state::save(&paths.state_file(), &store)?;
            let reason = git_failure_reason(&out);
            if matches!(stopped, StopOutcome::Stopped(_)) {
                bail!(
                    "git worktree remove failed: {reason} — what was running was stopped, and \
                     any public URL closed; the worktree was kept"
                );
            }
            bail!("git worktree remove failed: {reason}");
        }
    }

    let _ = std::fs::remove_dir_all(paths.logs_dir(name));
    // Every native service's data directory is under this one, so this is
    // also what makes "`rm` wipes the database" true for them.
    let _ = std::fs::remove_dir_all(paths.data_dir(name));
    if let Some(record) = store.worktrees.get(name) {
        clear_native_sockets(paths, name, record);
    }
    // And the compose override, which is regenerated on every isolated
    // start and would otherwise outlive every worktree that ever had one.
    let _ = std::fs::remove_file(paths.compose_override_file(name));
    // Its namespaces last, once the worktree is gone for certain: a
    // removal git refused must not have cost the worktree its database.
    // Each goes through the guard, on the server it was made on; one pando
    // cannot drop is said, with the command that does.
    drop_namespaces(paths, &store, name, progress);
    store.worktrees.remove(name);
    state::save(&paths.state_file(), &store)?;
    Ok(())
}

/// The managed worktree called `name`, or an error naming what is there.
pub(super) fn find_worktree(paths: &PandoPaths, name: &str) -> Result<Worktree> {
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

/// [`find_worktree`] for a start or restart, which runs things in the
/// worktree: see [`refuse_a_gone_directory`].
pub(super) fn find_live_worktree(paths: &PandoPaths, name: &str) -> Result<Worktree> {
    let worktree = find_worktree(paths, name)?;
    refuse_a_gone_directory(&worktree)?;
    Ok(worktree)
}

/// Refuses a worktree git still lists but whose directory is gone, naming
/// the `rm` that clears its entry.
///
/// Nothing else would say so. A start made its namespaces, assigned its
/// ports and saved them, and then failed in the first hook or spawn with a
/// bare "No such file or directory" that read as the install command or
/// bash being missing. `rm` keeps the plain lookup: clearing such an entry
/// is what it is for.
pub(super) fn refuse_a_gone_directory(worktree: &Worktree) -> Result<()> {
    if !worktree.prunable && worktree.path.is_dir() {
        return Ok(());
    }
    let why = worktree
        .prunable_reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    bail!(
        "the directory of {} is gone{why}: git still lists it at {}, but nothing can run there \
         — `pando rm {}` clears that entry",
        worktree.display_name(),
        worktree.path.display(),
        worktree.display_name()
    )
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
    path_in(worktree::discover_all(&paths.project)?, name)
}

/// [`path`], from a listing already made: the one the name was resolved
/// against, so `pando path` lists the worktrees once for both.
pub fn path_in(discovery: worktree::Discovery, name: &str) -> Result<PathBuf> {
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

/// Creates a worktree for a pull request, returning its directory name.
///
/// One opened from a branch on `origin` is `new` of that branch, which
/// fetches it when it has not been yet. One from a fork has no branch on
/// `origin`, only `refs/pull/<number>/head`, so that is fetched into
/// [`PrInfo::local_branch`] first — once: a local branch of that name
/// already is somebody's work on it, and is checked out as it stands.
pub fn new_for_pr(
    paths: &PandoPaths,
    config: &Config,
    pr: &PrInfo,
    progress: &dyn Fn(&str),
) -> Result<String> {
    let branch = pr.local_branch();
    let root = paths.root().to_path_buf();
    validate_branch_name(&root, &branch)?;
    if ref_exists(&root, &format!("refs/heads/{branch}")) {
        return new(paths, config, &branch, None, progress);
    }
    if !pr.cross_repository {
        // `new` of a name it cannot find forks a new branch of that name,
        // which here would be an empty worktree that looks like the pull
        // request. `gh` may be answering for a remote other than `origin`
        // — `upstream`, with `origin` somebody's fork — so it is looked
        // for first, and its absence said.
        let tracking = format!("refs/remotes/origin/{branch}");
        if has_origin(&root) && !ref_exists(&root, &tracking) {
            progress(&format!("looking for origin/{branch}"));
            fetch_branch(&root, &branch, crate::project::GIT_TIMEOUT)?;
        }
        if !ref_exists(&root, &tracking) {
            bail!(
                "#{}'s branch {branch} is not on origin — gh may list pull requests from \
                 another remote; fetch the branch, then try again",
                pr.number
            );
        }
        return new(paths, config, &branch, None, progress);
    }
    if !has_origin(&root) {
        bail!(
            "#{} is from a fork, and pando fetches it from a remote named origin, which this \
             repository does not have",
            pr.number
        );
    }
    progress(&format!("fetching #{}", pr.number));
    fetch_pr_head(&root, pr.number, &branch, crate::project::GIT_TIMEOUT)?;
    // A rejected `new` leaves no branch behind, and the one fetched here
    // is part of what it made.
    new(paths, config, &branch, None, progress).inspect_err(|_| {
        let _ = crate::project::git(&root, ["branch", "-D", branch.as_str()]);
    })
}

/// `git fetch origin refs/pull/<number>/head` into a new local branch.
/// Bounded and never prompting, as [`fetch_branch`] is; unlike it, a
/// failure is an error, because there is no other place the branch could be.
fn fetch_pr_head(
    root: &Path,
    number: u32,
    branch: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    let refspec = format!("refs/pull/{number}/head:refs/heads/{branch}");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["fetch", "--quiet", "origin", &refspec])
        .env("GIT_TERMINAL_PROMPT", "0");
    match crate::project::output_within(command, timeout) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!(
            "could not fetch #{number} from origin: {}",
            git_failure_reason(&out)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => bail!(
            "`git fetch origin {refspec}` did not answer in {}s",
            timeout.as_secs()
        ),
        Err(e) => Err(e).context("spawn git fetch"),
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

/// The record called `name` when it is for a worktree git no longer lists
/// at all, under that name or any other: the one thing `rm` of a name git
/// does not know removes.
fn forgotten_record<'a>(
    discovery: &worktree::Discovery,
    store: &'a state::State,
    name: &str,
) -> Option<&'a WorktreeRecord> {
    let record = store.worktrees.get(name)?;
    let path = crate::paths::resolve_for_compare(&record.path);
    let listed = std::iter::once(&discovery.main)
        .chain(&discovery.worktrees)
        .any(|w| crate::paths::resolve_for_compare(&w.path) == path);
    (!listed).then_some(record)
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
        let fetched = fetch_branch(root, branch, crate::project::GIT_TIMEOUT)?;
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
        None => worktree::resolve_base_branch(root).context(format!(
            "cannot work out a base branch (no origin/HEAD, main, or master) — {}",
            remedy::NAME_A_BASE
        ))?,
    };
    Ok(CreateSource::Fork { base })
}

/// `git fetch origin <branch>`: whether it fetched.
///
/// Bounded, and never allowed to ask for anything. A remote that wants a
/// password used to prompt on the terminal — over the TUI, whose keys
/// then went nowhere — and one that never answered held `new` for ever.
/// A fetch that *fails* is still "not on the remote", as before; one that
/// runs out of time is not an answer, and forking a new branch under a
/// name the remote may well have is worse than saying so.
pub(super) fn fetch_branch(
    root: &Path,
    branch: &str,
    timeout: std::time::Duration,
) -> Result<bool> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["fetch", "--quiet", "origin", branch])
        .env("GIT_TERMINAL_PROMPT", "0");
    match crate::project::output_within(command, timeout) {
        Ok(out) => Ok(out.status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => bail!(
            "`git fetch origin {branch}` did not answer in {}s, so pando cannot tell whether \
             origin already has {branch} — fetch it yourself, then run this again",
            timeout.as_secs()
        ),
        Err(_) => Ok(false),
    }
}

/// A bare base name would fork from the possibly stale local branch, so a
/// worktree created weeks after the last fetch would silently miss
/// everything merged since. A name already qualified with its remote is
/// used untouched; that is told from the refs, not from a slash, because
/// `release/1.2` is a branch name too.
fn resolve_create_base(root: &Path, base: &str) -> String {
    if base.contains('/') && ref_exists(root, &format!("refs/remotes/{base}")) {
        return base.to_string();
    }
    if ref_exists(root, &format!("refs/remotes/origin/{base}")) {
        return format!("origin/{base}");
    }
    base.to_string()
}

fn validate_branch_name(root: &Path, branch: &str) -> Result<()> {
    if branch.trim().is_empty() {
        bail!("a branch name is required");
    }
    let ok = crate::project::git(root, ["check-ref-format", "--branch", branch])
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
    let out = crate::project::git(dir, ["check-ignore", "-q", "--", rel])
        .context("run git check-ignore")?;
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
fn provision_worktree_files(
    paths: &PandoPaths,
    config: &Config,
    worktree: &Path,
    progress: &dyn Fn(&str),
) -> Result<()> {
    for rel in config.project.provision_paths() {
        let dst = worktree.join(rel);
        if dst.exists() {
            continue;
        }
        let Some((src, seeded)) = provision_source(paths, config, rel) else {
            continue;
        };
        // Invariant 1, checked in the worktree the file lands in and
        // immediately before the write — a seeded file is no different, and
        // the example it comes from being tracked buys it nothing.
        ensure_gitignored(worktree, rel)?;
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        // A seed is always copied, whatever the mode says. A symlink to the
        // tracked example would make every edit inside the worktree a write
        // into the repository, which is the invariant this whole path
        // exists to keep.
        let mode = match seeded {
            true => {
                // Every guess is visible, and this one is a file being
                // created out of contents pando did not write: the notice
                // names the source, so the developer can go and read it.
                let from = config
                    .project
                    .provision_from
                    .get(rel)
                    .map(String::as_str)
                    .unwrap_or_default();
                progress(&format!("seeding {rel} from {from}"));
                ProvisionMode::Copy
            }
            false => config.project.provision_mode,
        };
        match mode {
            ProvisionMode::Link => {
                std::os::unix::fs::symlink(&src, &dst)
                    .with_context(|| format!("symlink {} → {}", src.display(), dst.display()))?;
                // Said per file, and said to be a link: an edit in the
                // worktree edits the main checkout's file.
                progress(&format!(
                    "linked {rel} → {} (symlink; `provision_mode = \"copy\"` gives each \
                     worktree its own)",
                    src.display()
                ));
            }
            ProvisionMode::Copy => {
                std::fs::copy(&src, &dst)
                    .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
                if !seeded {
                    progress(&format!("copied {rel} from {}", src.display()));
                }
            }
        }
    }
    Ok(())
}

/// Where a provisioned path's contents come from, and whether that is the
/// project's own example rather than a local file.
///
/// The main checkout's own file first: an example is the fallback for a
/// clone that has none, and the moment the developer writes their real one
/// it is what every new worktree gets. `None` when there is nothing to copy
/// from, which is skipped rather than invented.
///
/// A source that is itself a gitignored local file — the root `.env` given
/// to a workspace app — is not a seed: it is the developer's own file under
/// another path, and it is linked or copied as the mode says, like the
/// root one is.
fn provision_source(paths: &PandoPaths, config: &Config, rel: &str) -> Option<(PathBuf, bool)> {
    let src = paths.root().join(rel);
    if src.exists() {
        return Some((src, false));
    }
    let from = config.project.provision_from.get(rel)?;
    let seed = paths.root().join(from);
    if !seed.exists() {
        return None;
    }
    let seeded = !crate::detect::is_gitignored(paths.root(), from);
    Some((seed, seeded))
}

pub(super) fn ref_exists(root: &Path, refname: &str) -> bool {
    crate::project::git(root, ["rev-parse", "--verify", "--quiet", refname])
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
    crate::project::git(root, ["remote"])
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
