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

/// The quick-tunnel URL the fake provider publishes.
pub const FAKE_TUNNEL_URL: &str = "https://fake-tunnel-for-tests.trycloudflare.com";

/// Installs a fake `cloudflared` at `<home>/bin/cloudflared`.
///
/// The same hook a developer would use for a real shim, so no test has to
/// put anything on PATH: `std::env::set_var` is unsafe in this edition and
/// racy across parallel tests, and a child's `PATH` is not reliably what
/// program lookup uses.
///
/// Every fake echoes its own arguments first, so a test can assert what
/// pando asked the provider for — which port it tunnelled, and that it
/// shadowed the user's config.
pub fn fake_cloudflared(home: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).expect("create the shim directory");
    let path = bin.join("cloudflared");
    std::fs::write(&path, format!("#!/bin/sh\necho \"ARGS: $*\"\n{body}"))
        .expect("write the fake cloudflared");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the fake cloudflared executable");
}

/// A fake that publishes a URL in cloudflared's own bordered format and
/// then stays up, as a tunnel does. `exec` so the pid pando records is the
/// one that has to be killed.
pub fn fake_cloudflared_publishing(home: &Path) {
    fake_cloudflared(
        home,
        &format!(
            "echo 'INF Requesting new quick Tunnel on trycloudflare.com...'\n\
             echo 'INF +----------------------------------------------------+'\n\
             echo 'INF |  {FAKE_TUNNEL_URL}  |'\n\
             echo 'INF +----------------------------------------------------+'\n\
             exec sleep 300\n"
        ),
    );
}

/// A fake that publishes the same URL the way `--output json` logs it: one
/// JSON object per line, with the bordered banner inside `message`, which
/// is where cloudflared 2025.11.1 really puts it.
pub fn fake_cloudflared_json_publishing(home: &Path) {
    fake_cloudflared(
        home,
        &format!(
            "echo '{{\"level\":\"info\",\"message\":\"Requesting new quick Tunnel on \
             trycloudflare.com...\"}}'\n\
             echo '{{\"level\":\"info\",\"message\":\"+---------------------+\"}}'\n\
             echo '{{\"level\":\"info\",\"message\":\"|  {FAKE_TUNNEL_URL}  |\"}}'\n\
             echo '{{\"level\":\"info\",\"message\":\"+---------------------+\"}}'\n\
             exec sleep 300\n"
        ),
    );
}

/// A fake that fails the way an offline or rate-limited cloudflared does:
/// a Go `*url.Error` naming the quick-tunnel API host, and then a shutdown
/// that is not instantaneous, because a real binary's is not either.
pub fn fake_cloudflared_api_error(home: &Path) {
    fake_cloudflared(
        home,
        "echo 'INF Requesting new quick Tunnel on trycloudflare.com...'\n\
         echo 'ERR failed to request quick Tunnel: Post \
         \"https://api.trycloudflare.com/tunnel\": dial tcp: lookup api.trycloudflare.com: \
         no such host' >&2\n\
         sleep 0.4\nexit 1\n",
    );
}

/// A fake that starts, says so, and never publishes anything.
pub fn fake_cloudflared_silent(home: &Path) {
    fake_cloudflared(
        home,
        "echo 'INF Requesting new quick Tunnel on trycloudflare.com...'\nexec sleep 300\n",
    );
}

/// A fake that fails the way a rate-limited cloudflared does.
pub fn fake_cloudflared_failing(home: &Path) {
    fake_cloudflared(
        home,
        "echo 'ERR failed to request quick Tunnel: 429 Too Many Requests' >&2\nexit 1\n",
    );
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
