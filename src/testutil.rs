//! Helpers shared by the unit tests of several modules. Compiled only under
//! `cfg(test)`; integration tests use `tests/common/mod.rs` instead, since
//! they cannot see a test-gated module of the library.

use std::path::Path;
use std::process::{Command, Stdio};

/// Runs git with a fixture identity so committing works on any machine and
/// never picks up (or depends on) the operator's own git config. These flags
/// belong to generated fixture repositories only.
const FIXTURE_IDENTITY: [&str; 10] = [
    "-c",
    "user.name=t",
    "-c",
    "user.email=t@t",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "init.defaultBranch=main",
];

pub fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(FIXTURE_IDENTITY)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

/// A repository with one commit on `main`, for tests that need a real repo
/// but no particular contents.
pub fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["commit", "--quiet", "--allow-empty", "-m", "root"]);
}

/// A detached child that is always stopped, even when a test fails partway
/// through. Every test in this crate that forks a real process holds one of
/// these: an assertion that panics mid-test must not leave a `sleep` or a
/// listener behind.
pub struct Detached {
    pub pid: u32,
    pub pgid: i32,
}

impl Drop for Detached {
    fn drop(&mut self) {
        let _ = crate::process::stop(self.pgid, std::time::Duration::from_secs(5));
    }
}

/// Spawns a detached shell command under a guard, as `actions::start` does.
pub fn spawn_guarded(shell_cmd: &str, cwd: &Path, log_file: &Path) -> Detached {
    let r = crate::process::spawn_detached(crate::process::SpawnOptions {
        shell_cmd,
        cwd,
        log_file,
        env: &[],
    })
    .expect("spawn detached");
    Detached {
        pid: r.pid,
        pgid: r.pgid,
    }
}

/// Whether `python3` is on PATH. Tests that need a process which binds a
/// port skip with a message rather than failing on a machine without it.
pub fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A process that binds `port` and then does nothing, for readiness and
/// observed-port tests.
///
/// Deliberately brace-free: `{` is pando's template syntax, and a command
/// carrying one would have to be escaped everywhere this string is used.
pub fn python_listener(port: u16) -> String {
    format!(
        "python3 -u -c \"import socket,time;s=socket.socket();\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
         s.bind(('127.0.0.1',{port}));s.listen(5);print('listening on {port}');time.sleep(300)\""
    )
}

/// A listener that binds whatever port the `{port:<role>}` template
/// resolves to, for tests that start a process which really holds a port.
pub fn python_listener_for_role(role: &str) -> String {
    format!(
        "python3 -u -c \"import socket,time;s=socket.socket();\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
         s.bind(('127.0.0.1',{{port:{role}}}));s.listen(5);\
         print('listening');time.sleep(300)\""
    )
}

/// A process that binds `port` on the IPv6 loopback and nowhere else.
///
/// Not exotic: `listen(port, "localhost")` in Node on macOS resolves to
/// `::1` first, and `runserver [::1]:8000` does the same. Both IPv4
/// addresses stay bindable, so a probe that only tries those never sees it.
pub fn python_listener_v6(port: u16) -> String {
    format!(
        "python3 -u -c \"import socket,time;s=socket.socket(socket.AF_INET6,socket.SOCK_STREAM);\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
         s.bind(('::1',{port}));s.listen(5);print('listening');time.sleep(300)\""
    )
}

/// Whether this machine has an IPv6 loopback to bind at all. A test about
/// IPv6 behaviour skips with a message rather than failing where there is
/// none.
pub fn ipv6_loopback_available() -> bool {
    std::net::TcpListener::bind(("::1", 0)).is_ok()
}

/// Polls `ready` until it is true or the deadline passes. Returns whether it
/// became true. Fixed sleeps make process tests flaky on a loaded machine;
/// this makes them fast when the machine is idle and patient when it is not.
pub fn wait_until(timeout: std::time::Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
