//! The repository pando is looking at, and the identity it derives from it.
//!
//! Discovery is from `git worktree list --porcelain`, whose first entry is
//! always the main checkout — from any cwd, including inside a linked
//! worktree. `git rev-parse --git-common-dir` is deliberately not used: from
//! a subdirectory it prints a relative path such as `../.git`.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

/// How long one git question may take before pando stops waiting for it.
///
/// git is the one external call every command makes before it reaches
/// dispatch — [`discover`] runs first — so a git that hangs (waiting on
/// a wedged process, or on a network mount that stopped
/// answering) is a pando that hangs, with no line on the screen saying
/// why. Generous, because a status on a very large tree is slow and still
/// an answer; bounded, because forever is not.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// `git -C <dir> <args…>` to completion, output captured, bounded by
/// [`GIT_TIMEOUT`].
///
/// An [`std::io::ErrorKind::TimedOut`] error when git did not answer in
/// time, with everything it started killed; every other failure is the
/// same `io::Error` `Command::output` would have given.
pub fn git<I, S>(dir: &Path, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_within(dir, args, GIT_TIMEOUT)
}

/// [`git`] with the deadline supplied.
pub fn git_within<I, S>(dir: &Path, args: I, timeout: Duration) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    crate::platform::process::output_within(command, timeout)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// `<display_name>-<8 hex chars of a hash of the canonical root path>`.
    pub id: String,
    /// Canonical path of the main checkout.
    pub root: PathBuf,
    /// The main checkout's directory name.
    pub display_name: String,
}

impl ProjectRef {
    /// Builds a reference from a main-checkout path. Public so tests and
    /// later phases (doctor adopting a moved project) can construct one
    /// without shelling out.
    pub fn from_root(root: impl AsRef<Path>) -> Result<Self> {
        let root = canonicalize(root.as_ref());
        let display_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let id = format!("{display_name}-{}", path_hash8(&root));
        Ok(Self {
            id,
            root,
            display_name,
        })
    }
}

/// The repository containing `cwd`, or an error naming why not.
pub fn discover(cwd: &Path) -> Result<ProjectRef> {
    let out = git(cwd, ["worktree", "list", "--porcelain"]);
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(_) => bail!("not inside a git repository: {}", cwd.display()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => bail!(
            "`git worktree list` did not answer in {}s in {} — a hung git process or an \
             unresponsive disk is the usual cause; run it yourself to see what git is waiting on",
            GIT_TIMEOUT.as_secs(),
            cwd.display()
        ),
        Err(e) => bail!("could not run git ({e}) — is git installed?"),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let (root, bare) = first_entry(&text)
        .with_context(|| format!("git listed no worktrees for {}", cwd.display()))?;
    if bare {
        bail!(
            "bare repositories are not supported: {} has no working tree to manage",
            root.display()
        );
    }
    ProjectRef::from_root(&root)
}

/// Path and bare-ness of the porcelain output's first entry — the main
/// checkout. Entries are separated by a blank line; unknown lines are
/// ignored, as new git versions add them.
fn first_entry(porcelain: &str) -> Option<(PathBuf, bool)> {
    let mut path: Option<PathBuf> = None;
    let mut bare = false;
    for line in porcelain.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if path.is_some() {
                break;
            }
            path = Some(PathBuf::from(rest));
        } else if line == "bare" {
            bare = true;
        }
    }
    path.map(|p| (p, bare))
}

/// macOS prints `/var/...` from the shell and `/private/var/...` from git, so
/// every path is canonicalised before it is hashed or compared. Falls back to
/// the input when the path does not exist, so error messages still name it.
fn canonicalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// First eight hex characters of md5 over the canonical root path. Not
/// security — just a short, stable discriminator so two checkouts of the same
/// repository name do not share a project directory.
fn path_hash8(root: &Path) -> String {
    let digest = md5::compute(crate::platform::files::path_bytes(root));
    format!("{digest:x}")[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, init_repo};
    use tempfile::tempdir;

    // A project's id names its directory under the pando home: a hash that
    // changed with a refactor would orphan every project's state, config
    // and logs. Pinned to the bytes of the path as the OS stores them.
    #[test]
    fn the_path_hash_of_a_project_never_changes() {
        assert_eq!(path_hash8(Path::new("/home/me/code/app")), "1fb718a9");
        assert_eq!(
            path_hash8(Path::new("/Users/me/Code/ünïcode app")),
            "30d92acf"
        );
    }

    #[test]
    fn discovers_the_root_from_the_repository_root() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("acme-shop");
        init_repo(&repo);

        let project = discover(&repo).unwrap();
        assert_eq!(project.root, std::fs::canonicalize(&repo).unwrap());
        assert_eq!(project.display_name, "acme-shop");
        assert!(project.id.starts_with("acme-shop-"));
        assert_eq!(project.id.len(), "acme-shop-".len() + 8);
    }

    #[test]
    fn subdirectory_and_linked_worktree_resolve_to_the_same_project() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("acme-shop");
        init_repo(&repo);
        let nested = repo.join("apps").join("web");
        std::fs::create_dir_all(&nested).unwrap();
        let linked = dir.path().join("elsewhere").join("feat+x");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        git(
            &repo,
            &["worktree", "add", "-b", "feat/x", linked.to_str().unwrap()],
        );

        let from_root = discover(&repo).unwrap();
        let from_sub = discover(&nested).unwrap();
        let from_linked = discover(&linked).unwrap();

        assert_eq!(from_root, from_sub);
        assert_eq!(
            from_root, from_linked,
            "a linked worktree must resolve to the main checkout's project"
        );
    }

    #[test]
    fn the_id_is_stable_and_path_dependent() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("one").join("shop");
        let b = dir.path().join("two").join("shop");
        init_repo(&a);
        init_repo(&b);

        let first = discover(&a).unwrap();
        assert_eq!(first, discover(&a).unwrap(), "id must be deterministic");
        let second = discover(&b).unwrap();
        assert_eq!(first.display_name, second.display_name);
        assert_ne!(
            first.id, second.id,
            "same directory name at different paths must not share a project id"
        );
    }

    #[test]
    fn outside_a_repository_is_an_error() {
        let dir = tempdir().unwrap();
        let err = discover(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a git repository"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn a_bare_repository_is_refused() {
        let dir = tempdir().unwrap();
        let bare = dir.path().join("bare.git");
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

        let err = discover(&bare).unwrap_err();
        assert!(
            format!("{err:#}").contains("bare repositories are not supported"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn first_entry_reads_the_main_checkout_only() {
        let text = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /elsewhere/feat+x\nHEAD def\nbranch refs/heads/feat/x\n\n";
        assert_eq!(
            first_entry(text),
            Some((PathBuf::from("/repo"), false)),
            "only the first entry describes the main checkout"
        );
    }

    #[test]
    fn first_entry_reports_a_bare_main_checkout() {
        assert_eq!(
            first_entry("worktree /repo.git\nbare\n\n"),
            Some((PathBuf::from("/repo.git"), true))
        );
    }

    #[test]
    fn first_entry_ignores_unknown_lines_and_empty_output() {
        assert_eq!(
            first_entry("worktree /repo\nsomething-new-in-git 1\nHEAD abc\n\n"),
            Some((PathBuf::from("/repo"), false))
        );
        assert_eq!(first_entry(""), None);
    }
}
