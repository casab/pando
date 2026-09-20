//! The orchestration layer. Everything user-facing — the CLI and the TUI —
//! calls into here; those two stay thin wrappers.
//!
//! Invariant 1 is enforced at this level: the only things written inside a
//! worktree are paths the project's own gitignore already ignores, checked
//! with `git check-ignore` before anything is created.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, ProvisionMode};
use crate::paths::PandoPaths;
use crate::state::{self, WorktreeRecord};
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
    match finish {
        Ok(()) => Ok(dir_name),
        Err(e) => Err(unwind_new(&root, &target, branch, &source, e)),
    }
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
pub fn rm(paths: &PandoPaths, config: &Config, name: &str, yes: bool, force: bool) -> Result<()> {
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
    let created_by_pando = store
        .worktrees
        .get(name)
        .map(|r| r.created_by_pando)
        .unwrap_or(false);
    if !created_by_pando && !yes {
        bail!(
            "pando did not create {name} ({}) — pass --yes to remove it anyway",
            target.path.display()
        );
    }

    // Unlink provisioned symlinks before git touches the worktree, so the
    // main checkout's files are never followed during teardown.
    for rel in &config.project.provision {
        remove_symlink_if_link(&target.path.join(rel));
    }

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
        bail!("git worktree remove failed: {}", git_failure_reason(&out));
    }

    let _ = std::fs::remove_dir_all(paths.logs_dir(name));
    let _ = std::fs::remove_dir_all(paths.data_dir(name));
    store.worktrees.remove(name);
    state::save(&paths.state_file(), &store)?;
    Ok(())
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

/// Which worktrees pando created, from state. A read path, so it reconciles
/// first; it takes no lock, because it mutates nothing on disk.
pub fn created_by_pando(paths: &PandoPaths) -> std::collections::BTreeMap<String, bool> {
    let Ok(mut store) = state::load(&paths.state_file()) else {
        return Default::default();
    };
    state::reconcile(&mut store, crate::process::is_alive);
    store
        .worktrees
        .iter()
        .map(|(k, v)| (k.clone(), v.created_by_pando))
        .collect()
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

fn remove_symlink_if_link(path: &Path) {
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        std::fs::remove_file(path).ok();
    }
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

        rm(&fx.paths, &fx.config, &name, false, false).unwrap();

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

        rm(&fx.paths, &fx.config, "adopted", true, false).unwrap();

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
        rm(&fx.paths, &fx.config, &name, false, false).unwrap();
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

        let err = rm(&fx.paths, &fx.config, "adopted", false, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("--yes"),
            "unexpected error: {err:#}"
        );
        assert_eq!(fx.names(), vec!["adopted"]);

        rm(&fx.paths, &fx.config, "adopted", true, false).unwrap();
        assert!(fx.names().is_empty());
    }

    #[test]
    fn rm_refuses_a_dirty_worktree_without_force() {
        let fx = fixture();
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        std::fs::write(fx.worktrees_dir().join(&name).join("scratch.txt"), "wip").unwrap();

        let err = rm(&fx.paths, &fx.config, &name, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("git worktree remove failed"), "{msg}");
        assert!(
            msg.contains("--force") || msg.contains("modified or untracked"),
            "{msg}"
        );
        assert_eq!(fx.names(), vec![name.clone()]);

        rm(&fx.paths, &fx.config, &name, false, true).unwrap();
        assert!(fx.names().is_empty());
    }

    // An ignored, provisioned file is pando's own doing and must never be
    // the reason a removal needs --force.
    #[test]
    fn a_provisioned_env_file_does_not_block_removal() {
        let mut fx = fixture();
        fx.config.project.provision = vec![".env".into()];
        let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
        rm(&fx.paths, &fx.config, &name, false, false).unwrap();
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
            let err = rm(&fx.paths, &fx.config, &name, yes, force).unwrap_err();
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

        rm(&fx.paths, &fx.config, &gone, false, false).unwrap();

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
        let err = rm(&fx.paths, &fx.config, &main_name, true, true).unwrap_err();
        assert!(format!("{err:#}").contains("main checkout"), "{err:#}");

        let err = rm(&fx.paths, &fx.config, "nope", true, true).unwrap_err();
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

        let map = created_by_pando(&fx.paths);
        assert_eq!(map.get("feat+one"), Some(&true));
        assert_eq!(
            map.get("adopted"),
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

        rm(&fx.paths, &fx.config, &name, false, false).unwrap();
        assert!(!elsewhere.join(&name).exists());
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
