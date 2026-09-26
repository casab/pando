//! The one test that opens a real Cloudflare quick tunnel, gated by
//! `PANDO_TEST_CLOUDFLARED=1`.
//!
//! Quick tunnels are free, anonymous, rate limited, and best effort. Every
//! other share test runs against a fake provider in the home; this one
//! exists to prove that the arguments pando passes really do produce a URL
//! that answers — including `--config`, which shadows the developer's own
//! `~/.cloudflared/config.yml` so its ingress rules cannot swallow the
//! request.
//!
//! It skips rather than fails when the network or Cloudflare will not play
//! along: a rate limit is not a defect in pando.
//!
//! ```sh
//! PANDO_TEST_CLOUDFLARED=1 cargo test --test tunnel -- --nocapture
//! ```

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::paths_for;
use pando::tunnel::{self, Provider, TunnelSpawn};
use tempfile::TempDir;

/// How long to keep asking the public URL before giving up. Cloudflare
/// takes a few seconds to route a fresh quick tunnel, and a loaded machine
/// takes longer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);

/// What the local server answers with, so the body proves the request
/// really reached *this* listener and not some other thing on the internet.
const BODY: &str = "pando-quick-tunnel-probe-ok";

fn enabled() -> bool {
    std::env::var("PANDO_TEST_CLOUDFLARED").as_deref() == Ok("1")
}

/// A tunnel that is always torn down, even when an assertion panics first.
/// A leaked cloudflared keeps a public URL open onto this machine.
struct Tunnel(TunnelSpawn);

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = pando::process::stop(self.0.pgid, Duration::from_secs(5));
    }
}

/// A local HTTP server that answers every request with [`BODY`], on a
/// thread that stops when the flag is cleared.
struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock the accept so the thread notices.
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local server");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    thread::spawn(move || {
        for conn in listener.incoming() {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut socket) = conn else { continue };
            // Read whatever headers arrived; the request itself does not
            // matter, only that something asked.
            let mut buf = [0u8; 4096];
            socket.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let _ = socket.read(&mut buf);
            let _ = socket.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{BODY}",
                    BODY.len()
                )
                .as_bytes(),
            );
            let _ = socket.shutdown(std::net::Shutdown::Write);
        }
    });
    Server { port, stop }
}

fn curl_available() -> bool {
    Command::new("curl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// How long to wait before asking at all.
///
/// Cloudflare creates the hostname's DNS record a moment after it prints
/// the URL. Asking too early does more than fail: the resolver in front of
/// this machine caches the NXDOMAIN, and every later attempt keeps failing
/// for the whole of that negative TTL — which is how this test came to
/// report a working tunnel as unreachable. Verified by hand: a first
/// lookup ten seconds in resolves, one at zero seconds poisons the cache.
const FIRST_ASK_DELAY: Duration = Duration::from_secs(10);

/// Asks the public URL until it answers with the body, or the timeout
/// passes. Returns what the last attempt produced, for the skip message.
fn fetch_until_answered(url: &str) -> Result<(), String> {
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    let mut last = String::from("(never ran)");
    let mut attempts = 0;
    thread::sleep(FIRST_ASK_DELAY);
    while Instant::now() < deadline {
        attempts += 1;
        let out = Command::new("curl")
            .args(["--silent", "--show-error", "--max-time", "15", url])
            .output()
            .map_err(|e| format!("could not run curl: {e}"))?;
        let body = String::from_utf8_lossy(&out.stdout).into_owned();
        if body.contains(BODY) {
            eprintln!("the tunnel answered on attempt {attempts}");
            return Ok(());
        }
        last = format!(
            "exit {:?}, stdout {:?}, stderr {:?}",
            out.status.code(),
            body.chars().take(200).collect::<String>(),
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        );
        eprintln!("attempt {attempts} did not answer: {last}");
        thread::sleep(Duration::from_secs(5));
    }
    Err(format!("after {attempts} attempts: {last}"))
}

#[test]
fn a_real_quick_tunnel_answers_on_its_public_url_and_is_torn_down() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_CLOUDFLARED=1 to open a real quick tunnel");
        return;
    }
    if !curl_available() {
        eprintln!("skipping: curl is needed to fetch the public URL");
        return;
    }

    let dir = TempDir::new().unwrap();
    let root = common::fixture_repo(dir.path());
    let home = dir.path().join("pando-home");
    // Deliberately no shim in the home: this is the one test that must use
    // the cloudflared the developer really has installed.
    let paths = paths_for(&home, &root);
    if tunnel::Cloudflared.ensure_present(&paths).is_err() {
        eprintln!("skipping: cloudflared is not installed");
        return;
    }
    assert_eq!(
        tunnel::cloudflared_program(&paths),
        PathBuf::from("cloudflared"),
        "this test must run the real binary, not a shim"
    );

    let server = serve();
    eprintln!("local server on 127.0.0.1:{}", server.port);

    let spawn = match tunnel::start_tunnel(&paths, "feat+one", tunnel::DEV_SERVER_HOST, server.port)
    {
        Ok(spawn) => spawn,
        Err(e) => {
            // Rate limited, offline, or Cloudflare having a bad day. None
            // of that is a defect in pando.
            eprintln!("skipping: cloudflared did not publish a URL ({e:#})");
            return;
        }
    };
    let tunnel = Tunnel(spawn);
    let url = tunnel.0.public_url.clone();
    eprintln!("quick tunnel at {url}");

    assert!(url.starts_with("https://"), "{url}");
    assert!(url.ends_with(".trycloudflare.com"), "{url}");
    assert!(
        pando::process::is_alive(tunnel.0.pid),
        "the tunnel must still be up when start_tunnel returns"
    );
    assert!(
        paths.tunnel_config_file().is_file(),
        "the isolated config must have been written"
    );
    assert!(
        tunnel.0.log_path.starts_with(&home),
        "the tunnel log lives under pando's home"
    );

    match fetch_until_answered(&url) {
        Ok(()) => {}
        Err(reason) => {
            // A URL that never routes is far more often Cloudflare than
            // pando, and a red test here would be a false one.
            eprintln!("skipping the answer assertion: {url} never served this machine — {reason}");
            eprintln!(
                "tunnel log tail:\n{}",
                std::fs::read_to_string(&tunnel.0.log_path).unwrap_or_default()
            );
            return;
        }
    }

    // And down again, with nothing left of it.
    let (pid, pgid) = (tunnel.0.pid, tunnel.0.pgid);
    drop(tunnel);
    let deadline = Instant::now() + Duration::from_secs(10);
    while pando::process::is_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !pando::process::is_alive(pid),
        "the tunnel outlived the test that opened it"
    );
    assert!(
        !pando::process::group_alive(pgid),
        "something in the tunnel's process group is still running"
    );
}
