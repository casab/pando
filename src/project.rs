//! The repository pando is looking at, and the identity it derives from it.
//!
//! Discovery is from `git worktree list --porcelain`, whose first entry is
//! always the main checkout — from any cwd, including inside a linked
//! worktree. `git rev-parse --git-common-dir` is deliberately not used: from
//! a subdirectory it prints a relative path such as `../.git`.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    output_within(command, timeout)
}

/// `Command::output`, with a deadline.
///
/// The child leads its own process group, so a timeout kills whatever it
/// started as well; the pipes are read on their own threads, so a child
/// that fills one cannot deadlock against the wait, and the reads are
/// bounded too, because anything the child left behind can hold a pipe
/// open after it exits.
pub(crate) fn output_within(mut command: Command, timeout: Duration) -> std::io::Result<Output> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let stdout = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let kill_group = || {
        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
    };

    // The leader is left unreaped until the group has been dealt with. A
    // zombie still holds its pid, and so the group id: once it is reaped
    // and its group is empty, the number can go to an unrelated process
    // group, and a `killpg` after that point is a signal to a stranger.
    // So the exit is *seen* here without being collected, and the child is
    // only reaped below, after the last `killpg`.
    let pid = child.id() as i32;
    let deadline = Instant::now() + timeout;
    let mut reaped: Option<std::process::ExitStatus> = None;
    loop {
        match exited_unreaped(pid) {
            Some(true) => break,
            Some(false) => {}
            // `waitid` not available: the old way, reaping to find out.
            None => {
                if let Some(status) = child.try_wait()? {
                    reaped = Some(status);
                    break;
                }
            }
        }
        if Instant::now() >= deadline {
            kill_group();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer in {}s", timeout.as_secs_f32()),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        let left = deadline.saturating_duration_since(Instant::now());
        rx.recv_timeout(left.max(Duration::from_millis(100)))
    };
    let mut out = collect(&stdout);
    let mut err = collect(&stderr);
    if out.is_err() || err.is_err() {
        // Exited, but something it started is still holding a pipe. That
        // group is this call's own, so it goes — which closes the pipe, and
        // what was already written is still the answer. Only while the
        // leader is unreaped, which is what makes the group still ours.
        if reaped.is_none() {
            kill_group();
        }
        let settle = Duration::from_millis(500);
        if out.is_err() {
            out = stdout.recv_timeout(settle);
        }
        if err.is_err() {
            err = stderr.recv_timeout(settle);
        }
    }
    let (out, err) = (out.unwrap_or_default(), err.unwrap_or_default());
    // Reaped last: nothing signals the group after this.
    let status = match reaped {
        Some(status) => status,
        None => child.wait()?,
    };
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Whether the child `pid` has exited, *without* reaping it: `Some(true)`
/// once it has, `Some(false)` while it runs, `None` where `waitid` cannot
/// say.
fn exited_unreaped(pid: i32) -> Option<bool> {
    // SAFETY: `waitid` writes only into `info`, which is zeroed and owned
    // here; `WNOWAIT` leaves the child waitable for the `wait` that reaps it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return None;
    }
    // With `WNOHANG`, a child that has not changed state leaves `si_pid`
    // zero.
    Some(siginfo_pid(&info) != 0)
}

#[cfg(target_os = "linux")]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: filled by `waitid`, for which `si_pid` is the valid member.
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
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
    use std::os::unix::ffi::OsStrExt;
    let digest = md5::compute(root.as_os_str().as_bytes());
    format!("{digest:x}")[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, init_repo};
    use tempfile::tempdir;

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

    // git is the one external call every command makes first, so a git
    // that never answers must not be a pando that never answers.
    #[test]
    fn a_command_that_does_not_answer_is_killed_at_the_deadline() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & sleep 30"]);
        let started = Instant::now();
        let err = output_within(command, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "bounded, not waited out: {:?}",
            started.elapsed()
        );

        // And one that answers is read exactly as `output` would read it.
        let mut command = Command::new("sh");
        command.args(["-c", "echo out; echo err >&2; exit 3"]);
        let out = output_within(command, Duration::from_secs(10)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
    }

    // Exited, with a child it backgrounded still holding stdout: the call
    // returns rather than waiting on the child.
    #[test]
    fn a_background_child_holding_the_pipe_does_not_hold_the_call() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & echo done"]);
        let started = Instant::now();
        let out = output_within(command, Duration::from_secs(1)).unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("done"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    // The group is only signalled while its leader is unreaped: a reaped
    // leader of an empty group frees the number for a stranger. So the
    // exit is seen without being collected, and the status still comes
    // back whole once the child is reaped afterwards.
    #[test]
    fn an_exit_is_seen_without_reaping_the_child() {
        let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn().unwrap();
        let pid = child.id() as i32;
        let started = Instant::now();
        while exited_unreaped(pid) != Some(true) {
            assert!(started.elapsed() < Duration::from_secs(10), "never seen");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Still ours to reap, with its status intact.
        assert_eq!(child.wait().unwrap().code(), Some(7));

        let mut running = Command::new("sleep").arg("30").spawn().unwrap();
        assert_eq!(exited_unreaped(running.id() as i32), Some(false));
        let _ = running.kill();
        let _ = running.wait();
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
