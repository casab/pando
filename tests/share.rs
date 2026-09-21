//! The share proxy as it really runs: a detached child of the pando binary,
//! reading its cookie from the environment.
//!
//! Everything here binds loopback ports and spawns the real binary, so each
//! test owns a guard that stops its child even when an assertion panics.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::paths_for;
use pando::share_proxy;
use tempfile::TempDir;

const COOKIE: &str = "pando_session=top-secret-value; who=dev";

/// A spawned proxy that is always stopped, even when a test fails partway
/// through. A leaked proxy holds a port and a cookie.
struct Proxy {
    spawn: share_proxy::ProxySpawn,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = pando::process::stop(self.spawn.pgid, Duration::from_secs(5));
    }
}

struct Env {
    _dir: TempDir,
    home: PathBuf,
    root: PathBuf,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let root = common::fixture_repo(dir.path());
    Env {
        home: dir.path().join("pando-home"),
        root,
        _dir: dir,
    }
}

fn pando_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pando"))
}

/// A port nothing is listening on. Bound to learn the number, then released
/// — the proxy takes it a moment later and the test waits until it has.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(timeout: Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// An upstream that records the request headers it was sent and answers.
/// Serves `requests` connections, then stops.
fn upstream(requests: usize) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for _ in 0..requests {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let mut seen: Vec<u8> = Vec::new();
            loop {
                match socket.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            tx.send(String::from_utf8_lossy(&seen).into_owned()).ok();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
            let _ = socket.shutdown(std::net::Shutdown::Write);
        }
    });
    (port, rx)
}

fn get_through(port: u16, path: &str) -> String {
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: tunnel.example\r\nCookie: stale=1\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    response
}

/// Every `.log` under a worktree's log directory, concatenated.
fn all_logs(home: &Path, root: &Path, name: &str) -> String {
    let paths = paths_for(home, root);
    let dir = paths.logs_dir(name);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return String::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_hidden_subcommand_proxies_a_request_and_injects_the_cookie() {
    let e = env();
    let paths = paths_for(&e.home, &e.root);
    let (upstream_port, seen) = upstream(1);
    let listen_port = free_port();

    let proxy = Proxy {
        spawn: share_proxy::spawn_with(
            &paths,
            "feat+one",
            listen_port,
            upstream_port,
            COOKIE,
            &pando_bin(),
        )
        .expect("spawn the proxy"),
    };
    assert!(
        wait_until(Duration::from_secs(10), || TcpStream::connect((
            "127.0.0.1",
            listen_port
        ))
        .is_ok()),
        "the proxy never started listening; log: {}",
        all_logs(&e.home, &e.root, "feat+one")
    );

    let response = get_through(listen_port, "/hello");
    assert!(response.contains("200 OK"), "{response}");

    let head = seen.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        head.contains(&format!("Cookie: {COOKIE}")),
        "the upstream did not see the injected cookie:\n{head}"
    );
    assert!(!head.contains("stale=1"), "{head}");
    assert_eq!(proxy.spawn.listen_port, listen_port);
}

// The whole reason the cookie goes by environment. `ps` output is readable
// by every process on the machine.
#[test]
fn the_cookie_is_in_no_command_line_and_in_no_log() {
    let e = env();
    let paths = paths_for(&e.home, &e.root);
    let (upstream_port, seen) = upstream(1);
    let listen_port = free_port();

    let proxy = Proxy {
        spawn: share_proxy::spawn_with(
            &paths,
            "feat+one",
            listen_port,
            upstream_port,
            COOKIE,
            &pando_bin(),
        )
        .unwrap(),
    };
    assert!(wait_until(Duration::from_secs(10), || TcpStream::connect(
        ("127.0.0.1", listen_port)
    )
    .is_ok()));
    get_through(listen_port, "/x");
    seen.recv_timeout(Duration::from_secs(10)).unwrap();

    let ps = Command::new("ps")
        .args(["-eo", "pid,pgid,command"])
        .output()
        .expect("run ps");
    let listing = String::from_utf8_lossy(&ps.stdout);
    assert!(
        listing.contains(&proxy.spawn.pid.to_string()),
        "the proxy should be in ps output at all"
    );
    assert!(
        !listing.contains("top-secret-value"),
        "the cookie reached a command line"
    );

    let logs = all_logs(&e.home, &e.root, "feat+one");
    assert!(
        !logs.is_empty(),
        "the proxy should have logged that it is listening"
    );
    assert!(
        !logs.contains("top-secret-value"),
        "the cookie reached a log that outlives the share:\n{logs}"
    );
    assert!(
        logs.contains(&listen_port.to_string()),
        "the proxy log should say where it is listening:\n{logs}"
    );
}

#[test]
fn the_hidden_subcommand_refuses_to_run_without_its_cookie() {
    let e = env();
    // Not a repository: the proxy must not need one, and the refusal must
    // not be "not inside a git repository".
    let elsewhere = e.root.parent().unwrap().join("not-a-repo");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = Command::new(pando_bin())
        .env_remove(share_proxy::ENV_COOKIE)
        .env("PANDO_HOME", &e.home)
        .current_dir(&elsewhere)
        .args([
            share_proxy::SUBCOMMAND,
            "--listen",
            "17005",
            "--upstream",
            "17000",
        ])
        .output()
        .expect("run pando");

    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(share_proxy::ENV_COOKIE), "{stderr}");
    assert!(stderr.contains("pando share"), "{stderr}");
    assert!(
        !stderr.contains("git repository"),
        "the proxy runs outside a repository on purpose: {stderr}"
    );
}

// It is hidden, not secret: it must not appear in `--help`, because nobody
// should ever type it.
#[test]
fn the_hidden_subcommand_is_not_advertised() {
    let out = Command::new(pando_bin())
        .arg("--help")
        .output()
        .expect("run pando --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("Commands:"), "{help}");
    assert!(
        !help.contains(share_proxy::SUBCOMMAND),
        "the proxy subcommand must stay out of help:\n{help}"
    );
}
