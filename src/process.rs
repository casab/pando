use anyhow::{Context, Result, bail};
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct SpawnOptions<'a> {
    pub shell_cmd: &'a str,
    pub cwd: &'a Path,
    pub log_file: &'a Path,
    pub env: &'a [(String, String)],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnResult {
    pub pid: u32,
    pub pgid: i32,
}

pub fn spawn_detached(opts: SpawnOptions<'_>) -> Result<SpawnResult> {
    if let Some(parent) = opts.log_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(opts.log_file)
        .with_context(|| format!("open log file {}", opts.log_file.display()))?;
    let err = out
        .try_clone()
        .context("clone log file handle for stderr")?;

    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(opts.shell_cmd)
        .current_dir(opts.cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);

    for (k, v) in opts.env {
        cmd.env(k, v);
    }

    // Full detachment: new session, new process group, no controlling tty.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        });
    }

    let child = cmd.spawn().context("spawn detached bash")?;
    let pid = child.id();
    // After setsid() the child's pgid equals its pid (it becomes session leader).
    let pgid = pid as i32;
    // Drop child handle without waiting — the process runs on independently.
    std::mem::drop(child);

    Ok(SpawnResult { pid, pgid })
}

pub fn is_alive(pid: u32) -> bool {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    // Reap zombie first if we're the parent (TUI session). Returns
    // ECHILD when pid isn't our child — fall through to signal probe.
    match waitpid(Pid::from_raw(pid as i32), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => return false,
        Ok(WaitStatus::StillAlive) => return true,
        _ => {}
    }
    match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => false,
    }
}

/// Whether any process is still in the group — the leader, a child it
/// forked, or a grandchild.
///
/// `kill(-pgid, 0)` and not `kill(leader, 0)`: a dev server's group leader is
/// a `bash -lc` that very often exits while the server it backgrounded keeps
/// running and keeps the port. Asking after the leader alone is how a stop
/// reports success over a process that is still serving.
pub fn group_alive(pgid: i32) -> bool {
    // 0 is "our own group" and 1 is "every process we may signal". Neither
    // is ever a worktree's process group, and both would be catastrophic to
    // pass to kill(2).
    if pgid <= 1 {
        return false;
    }
    match nix::sys::signal::kill(Pid::from_raw(-pgid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        // The group exists but holds something we may not signal.
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// What a captured command printed, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Captured {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The last non-empty line of stderr — what a failing script actually
    /// complained about, without the build noise above it.
    pub fn last_stderr_line(&self) -> Option<&str> {
        self.stderr
            .lines()
            .map(str::trim)
            .rev()
            .find(|l| !l.is_empty())
    }
}

/// Runs a shell command to completion with its output captured and a hard
/// deadline.
///
/// Two differences from `Command::output()`, both of which matter for a
/// command a project supplied:
///
/// - **It leads its own process group.** A command that times out is killed
///   with everything it started, not just the shell that started them.
/// - **It cannot hang forever.** A script that waits on something that
///   never comes would otherwise block whatever asked for its answer, and
///   in the TUI that is a slot nothing can clear.
///
/// The pipes are drained on their own threads, so a command that prints
/// more than a pipe buffer holds does not deadlock against the wait.
pub fn run_captured(
    shell_cmd: &str,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<Captured> {
    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(shell_cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("run {shell_cmd:?}"))?;
    // After `setsid` the child leads a group whose id is its pid.
    let pgid = child.id() as i32;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || drain(out_pipe.as_mut()));
    let err_reader = std::thread::spawn(move || drain(err_pipe.as_mut()));

    let deadline = Instant::now() + timeout;
    let code = loop {
        match child.try_wait().context("wait for the command")? {
            Some(status) => break status.code(),
            None if Instant::now() >= deadline => {
                // The group, not the child: the shell may have started
                // something that is the actual reason this is still here.
                let _ = stop(pgid, Duration::from_secs(1));
                let _ = child.wait();
                let stderr = err_reader.join().unwrap_or_default();
                bail!(
                    "{shell_cmd:?} was still running after {}s{}",
                    timeout.as_secs(),
                    match stderr.lines().map(str::trim).rev().find(|l| !l.is_empty()) {
                        Some(line) => format!(" — its last output was: {line}"),
                        None => String::new(),
                    }
                );
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    Ok(Captured {
        code,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

fn drain(pipe: Option<&mut impl std::io::Read>) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Single-quotes one word of a `bash -lc` command line, with any single
/// quote inside closed, escaped, and reopened.
///
/// Here rather than beside a caller because every command pando builds ends
/// up in [`SpawnOptions::shell_cmd`], and a home directory or a worktree
/// path with a space in it is ordinary: a word that is not quoted is a
/// command that runs something else.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// SIGTERM to the whole group, then SIGKILL to whatever is left after
/// `grace`. Returns once the group is empty, or once it has been SIGKILLed.
///
/// Signalling is unconditional, including for a process pando has already
/// written off as failed: a leader that exited is not a group that exited,
/// and skipping the signal is exactly how a backgrounded child outlives the
/// tool that started it.
pub fn stop(pgid: i32, grace: Duration) -> Result<()> {
    if pgid <= 1 {
        return Ok(());
    }
    let group = Pid::from_raw(pgid);
    let any_in_group = Pid::from_raw(-pgid);

    match killpg(group, Signal::SIGTERM) {
        Ok(()) => {}
        // Nothing left in the group; there is nothing to wait for.
        Err(Errno::ESRCH) => {
            reap_group(any_in_group);
            return Ok(());
        }
        // macOS returns EPERM for a group whose only member is a zombie
        // leader. Falling through reaps it.
        Err(Errno::EPERM) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGTERM")),
    }

    let deadline = Instant::now() + grace;
    loop {
        // Reap first: a zombie child of ours still counts as a group member
        // to kill(2), so an unreaped leader would look alive forever.
        reap_group(any_in_group);
        if !group_alive(pgid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    match killpg(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGKILL")),
    }
    // SIGKILL is not instantaneous; the caller is about to reuse this
    // worktree's ports, so it is worth the few milliseconds to see it land.
    let hard_deadline = Instant::now() + KILL_SETTLE;
    loop {
        reap_group(any_in_group);
        if !group_alive(pgid) || Instant::now() >= hard_deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How long to wait for SIGKILL to be delivered before giving up and
/// returning anyway.
const KILL_SETTLE: Duration = Duration::from_millis(500);

fn reap_group(any_in_group: Pid) {
    loop {
        match waitpid(Some(any_in_group), Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn is_alive_returns_false_for_nonexistent_pid() {
        assert!(!is_alive(999_999));
    }

    #[test]
    fn a_captured_command_reports_what_it_printed_and_how_it_ended() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "echo out; echo err >&2",
            dir.path(),
            &[],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(captured.success());
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "out");
        assert_eq!(captured.stderr.trim(), "err");
    }

    #[test]
    fn a_captured_command_that_fails_keeps_its_code_and_last_complaint() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "echo noise >&2; echo 'the real reason' >&2; exit 7",
            dir.path(),
            &[],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(!captured.success());
        assert_eq!(captured.code, Some(7));
        assert_eq!(captured.last_stderr_line(), Some("the real reason"));
    }

    #[test]
    fn a_captured_command_runs_where_it_was_told_with_the_environment_it_was_given() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "pwd; printf 'SECRET=%s\\n' \"$SECRET\"",
            dir.path(),
            &[("SECRET".to_string(), "abc".to_string())],
            Duration::from_secs(10),
        )
        .unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert!(
            captured.stdout.contains(&canonical.display().to_string()),
            "{}",
            captured.stdout
        );
        assert!(
            captured.stdout.contains("SECRET=abc"),
            "{}",
            captured.stdout
        );
    }

    // More than a pipe buffer holds. A wait that does not drain the pipes
    // deadlocks here rather than returning.
    #[test]
    fn a_captured_command_that_prints_a_lot_does_not_deadlock() {
        let dir = tempdir().unwrap();
        let captured = run_captured(
            "for i in $(seq 1 5000); do echo 'a line of output that is not especially short'; done",
            dir.path(),
            &[],
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(captured.success());
        assert_eq!(captured.stdout.lines().count(), 5000);
    }

    // The whole reason this is not `Command::output()`: a script that hangs
    // must not hang the share that asked it a question — and killing the
    // shell alone would leave whatever it started behind.
    #[test]
    fn a_captured_command_that_hangs_is_killed_with_everything_it_started() {
        let dir = tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        // Generous, because `bash -lc` sources a login profile first: a
        // command killed before it ran anything at all would prove nothing.
        let err = run_captured(
            &format!("sleep 300 & echo $! > {}; wait", pidfile.display()),
            dir.path(),
            &[],
            Duration::from_secs(3),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("still running after"),
            "{err:#}"
        );

        let child: u32 = std::fs::read_to_string(&pidfile)
            .expect("the script wrote its child's pid")
            .trim()
            .parse()
            .expect("a pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_alive(child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            !is_alive(child),
            "the grandchild outlived the command that started it"
        );
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("/tmp/x y/pando"), "'/tmp/x y/pando'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn stop_is_idempotent_when_group_already_gone() {
        stop(999_999, Duration::from_millis(100)).unwrap();
        assert!(!group_alive(999_999));
    }

    // `kill(-0, …)` signals our own process group and `kill(-1, …)` signals
    // every process we are allowed to signal. Neither is ever a worktree.
    #[test]
    fn stop_refuses_the_process_group_ids_that_would_signal_ourselves() {
        assert!(!group_alive(0));
        assert!(!group_alive(1));
        assert!(!group_alive(-5));
        stop(0, Duration::from_millis(10)).unwrap();
        stop(1, Duration::from_millis(10)).unwrap();
        stop(-1, Duration::from_millis(10)).unwrap();
        // Still here, and still runnable: we did not signal ourselves.
        assert!(is_alive(std::process::id()));
    }

    // The failure the origin tool had: a leader that exits leaves its
    // backgrounded child holding the port, and a stop that watches only the
    // leader reports success over a process that is still serving.
    #[test]
    fn stop_kills_a_child_whose_leader_already_exited() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            // bash backgrounds the sleep and exits immediately.
            shell_cmd: "sleep 30 & exit 0",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_alive(r.pid), "the leader should have exited on its own");
        assert!(
            group_alive(r.pgid),
            "the backgrounded child keeps the group alive"
        );

        stop(r.pgid, Duration::from_secs(5)).unwrap();
        assert!(
            !group_alive(r.pgid),
            "stop must empty the group, not just outlive its leader"
        );
    }

    #[test]
    fn spawn_puts_child_in_its_own_process_group() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "sleep 30",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
        })
        .unwrap();

        let our_pgid = nix::unistd::getpgrp().as_raw();
        assert_ne!(
            r.pgid, our_pgid,
            "child pgid ({}) must differ from parent pgid ({})",
            r.pgid, our_pgid
        );

        assert!(is_alive(r.pid));
        stop(r.pgid, Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!is_alive(r.pid), "child should be dead after stop");
    }

    #[test]
    fn spawn_redirects_stdout_to_log_file() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "echo hello-from-child && echo err-line >&2",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));

        let content = std::fs::read_to_string(&log).unwrap();
        assert!(
            content.contains("hello-from-child"),
            "log missing stdout: {content:?}"
        );
        assert!(
            content.contains("err-line"),
            "log missing stderr: {content:?}"
        );
    }

    #[test]
    fn stop_terminates_quickly_with_sigterm_when_child_cooperates() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "sleep 30",
            cwd: dir.path(),
            log_file: &log,
            env: &[],
        })
        .unwrap();

        let start = Instant::now();
        stop(r.pgid, Duration::from_secs(5)).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "sleep should die on SIGTERM well under the grace window; took {elapsed:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(!is_alive(r.pid));
    }

    #[test]
    fn spawn_writes_env_vars_into_child_environment() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("log.txt");
        let r = spawn_detached(SpawnOptions {
            shell_cmd: "printf 'WEB=%s API=%s' \"$WEB_PORT\" \"$API_PORT\"",
            cwd: dir.path(),
            log_file: &log,
            env: &[
                ("WEB_PORT".into(), "17224".into()),
                ("API_PORT".into(), "17225".into()),
            ],
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        while is_alive(r.pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));

        let content = std::fs::read_to_string(&log).unwrap();
        assert!(
            content.contains("WEB=17224") && content.contains("API=17225"),
            "env not passed through: {content:?}"
        );
    }
}
