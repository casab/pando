use anyhow::{Context, Result};
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

pub fn stop(pgid: i32, grace: Duration) -> Result<()> {
    let group = Pid::from_raw(pgid);
    let any_in_group = Pid::from_raw(-pgid);
    let leader_pid = pgid as u32;

    match killpg(group, Signal::SIGTERM) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return Ok(()),
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGTERM")),
    }

    let deadline = Instant::now() + grace;
    loop {
        reap_group(any_in_group);
        if !is_alive(leader_pid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Tolerate EPERM on macOS: signaling a zombie group leader can return
    // EPERM even though the group has no live processes left.
    match killpg(group, Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) | Err(Errno::EPERM) => {}
        Err(e) => return Err(anyhow::Error::from(e).context("killpg SIGKILL")),
    }
    reap_group(any_in_group);
    Ok(())
}

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
    fn stop_is_idempotent_when_group_already_gone() {
        stop(999_999, Duration::from_millis(100)).unwrap();
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
