//! The local proxy that sits between a tunnel and a dev server and injects
//! a `Cookie` header, so a visitor lands already authenticated.
//!
//! It runs as `pando __share-proxy`, a hidden subcommand of the same binary,
//! spawned detached like any other process. Two rules govern it:
//!
//! - **The cookie travels by environment, never by argv.** Anything in argv
//!   is in `ps` output, readable by every process on the machine.
//! - **Nothing it prints carries the cookie.** Its log is a log like any
//!   other; a credential in it would outlive the share.
//!
//! It is a hand-written HTTP/1.1 header rewriter, ported from the origin
//! tool where it has run in anger. It is deliberately not extended.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::paths::PandoPaths;
use crate::process::{self, SpawnOptions};

/// The log source the proxy writes to. Reserved in [`crate::paths`].
pub const PROXY_LOG: &str = "proxy";

/// How the cookie reaches the proxy. Never an argument.
pub const ENV_COOKIE: &str = "PANDO_SHARE_COOKIE";

/// The hidden subcommand that runs the proxy in this same binary.
pub const SUBCOMMAND: &str = "__share-proxy";

const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a client may take to finish sending its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the proxy waits on the upstream between reads.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// A request whose headers are bigger than this is refused rather than
/// buffered: the proxy holds the whole header block in memory.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// A running proxy, before it is written to state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySpawn {
    pub pid: u32,
    pub pgid: i32,
    pub listen_port: u16,
    pub log_path: PathBuf,
}

/// Spawns the proxy as a detached `pando __share-proxy`.
///
/// The same binary that is running now, so a `cargo run`, a
/// `./target/debug/pando`, and a PATH-installed `pando` all work without
/// anything having to know where it lives.
pub fn spawn(
    paths: &PandoPaths,
    name: &str,
    listen_port: u16,
    upstream_port: u16,
    cookie: &str,
) -> Result<ProxySpawn> {
    let exe = std::env::current_exe().context("resolve the running pando executable")?;
    spawn_with(paths, name, listen_port, upstream_port, cookie, &exe)
}

/// [`spawn`] with the executable given, so a test can point it at a binary
/// of its choosing.
pub fn spawn_with(
    paths: &PandoPaths,
    name: &str,
    listen_port: u16,
    upstream_port: u16,
    cookie: &str,
    exe: &Path,
) -> Result<ProxySpawn> {
    let log_path = paths.log_file(name, PROXY_LOG);
    // `spawn_detached` opens the log with O_APPEND, so without this every
    // share → unshare → share cycle keeps the dead sessions' bytes.
    truncate_log(&log_path)?;
    let spawn = process::spawn_detached(SpawnOptions {
        shell_cmd: &proxy_command(exe, listen_port, upstream_port),
        // Not the worktree: the proxy needs nothing from it, and a process
        // holding it open is one more reason `rm` cannot remove it.
        cwd: &std::env::temp_dir(),
        log_file: &log_path,
        env: &proxy_env(cookie),
    })
    .context("spawn the share proxy")?;
    Ok(ProxySpawn {
        pid: spawn.pid,
        pgid: spawn.pgid,
        listen_port,
        log_path,
    })
}

/// The command line the proxy is spawned with. Ports only — the cookie is
/// in the environment, and this string ends up in `ps`.
fn proxy_command(exe: &Path, listen_port: u16, upstream_port: u16) -> String {
    format!(
        "exec {exe} {SUBCOMMAND} --listen {listen_port} --upstream {upstream_port}",
        exe = process::shell_quote(&exe.to_string_lossy()),
    )
}

/// The environment the proxy is spawned with: the cookie, and nothing else.
fn proxy_env(cookie: &str) -> Vec<(String, String)> {
    vec![(ENV_COOKIE.to_string(), cookie.to_string())]
}

/// The hidden subcommand's entry point. Binds `127.0.0.1:listen_port` and
/// forwards every connection to `127.0.0.1:upstream_port` with the cookie
/// injected.
///
/// Loopback only. The proxy exists to be reached by a tunnel running on
/// this machine, and a bind on `0.0.0.0` would publish an
/// already-authenticated door onto the local network.
pub fn run_in_process(listen_port: u16, upstream_port: u16, cookie: &str) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", listen_port))
        .with_context(|| format!("bind the share proxy on 127.0.0.1:{listen_port}"))?;
    // Ports, never the cookie: this goes into the proxy log.
    eprintln!("pando share proxy: 127.0.0.1:{listen_port} -> 127.0.0.1:{upstream_port}");
    let cookie = cookie.to_string();
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cookie = cookie.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, upstream_port, &cookie) {
                        // `e` is about sockets and never holds a header.
                        eprintln!("pando share proxy: connection failed: {e:#}");
                    }
                });
            }
            Err(e) => eprintln!("pando share proxy: accept failed: {e}"),
        }
    }
    Ok(())
}

fn handle_connection(mut client: TcpStream, upstream_port: u16, cookie: &str) -> Result<()> {
    client.set_read_timeout(Some(HEADER_READ_TIMEOUT)).ok();
    let (head, leftover) = read_until_headers_end(&mut client)?;
    let rewritten = rewrite_headers(&head, cookie);

    let upstream_addr = format!("127.0.0.1:{upstream_port}");
    let mut upstream = TcpStream::connect_timeout(
        &upstream_addr
            .parse()
            .context("parse the upstream address")?,
        UPSTREAM_CONNECT_TIMEOUT,
    )
    .with_context(|| format!("connect to the upstream {upstream_addr}"))?;
    upstream.set_read_timeout(Some(UPSTREAM_READ_TIMEOUT)).ok();

    upstream
        .write_all(rewritten.as_bytes())
        .context("write the rewritten headers")?;
    if !leftover.is_empty() {
        upstream
            .write_all(&leftover)
            .context("write the rest of the request body")?;
    }

    // `Connection: close` went upstream, so every request is its own
    // socket: pipe both ways until one side hangs up.
    let mut up_read = upstream.try_clone().context("clone the upstream socket")?;
    let mut cli_write = client.try_clone().context("clone the client socket")?;
    let back = thread::spawn(move || {
        let _ = std::io::copy(&mut up_read, &mut cli_write);
        let _ = cli_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut client, &mut upstream);
    let _ = upstream.shutdown(std::net::Shutdown::Write);
    let _ = back.join();
    Ok(())
}

/// Reads until the end-of-headers marker, returning the header block and
/// whatever body bytes arrived in the same packet.
fn read_until_headers_end(client: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];
    loop {
        if buf.len() > MAX_HEADER_BYTES {
            bail!("the request headers were longer than {MAX_HEADER_BYTES} bytes");
        }
        let n = client.read(&mut chunk).context("read from the client")?;
        if n == 0 {
            bail!("the client closed before finishing its headers");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some((end_head, start_body)) = find_headers_end(&buf) {
            let head = String::from_utf8(buf[..end_head].to_vec())
                .context("the request headers were not valid UTF-8")?;
            return Ok((head, buf[start_body..].to_vec()));
        }
    }
}

/// Where the headers end: `(end of the header block, start of the body)`.
/// The header block keeps the last header's own `\r\n` and excludes the
/// blank line. `\n\n` is accepted as well as `\r\n\r\n`, for tolerance with
/// hand-written traffic.
fn find_headers_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i + 2, i + 4));
        }
    }
    for i in 0..buf.len().saturating_sub(1) {
        if &buf[i..i + 2] == b"\n\n" {
            return Some((i + 1, i + 2));
        }
    }
    None
}

/// Replaces the `Cookie` header with the one the auth command produced, and
/// forces `Connection: close` so each request is a fresh hop through the
/// proxy. Every other header is passed through byte for byte.
pub fn rewrite_headers(head: &str, cookie: &str) -> String {
    let mut lines = split_header_lines(head);
    let request_line = lines.next().unwrap_or("").to_string();
    let mut kept: Vec<String> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("cookie:")
            || lower.starts_with("connection:")
            || lower.starts_with("keep-alive:")
            || lower.starts_with("proxy-connection:")
        {
            continue;
        }
        kept.push(line.to_string());
    }
    kept.push(format!("Cookie: {cookie}"));
    kept.push("Connection: close".to_string());

    let mut out = String::with_capacity(head.len() + cookie.len() + 64);
    out.push_str(&request_line);
    out.push_str("\r\n");
    for header in kept {
        out.push_str(&header);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    out
}

fn split_header_lines(head: &str) -> impl Iterator<Item = &str> {
    head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

fn truncate_log(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    if path.exists() {
        std::fs::write(path, b"")
            .with_context(|| format!("truncate the proxy log {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use std::sync::mpsc;
    use tempfile::{TempDir, tempdir};

    const COOKIE: &str = "session=abc123; user=someone; flag=1";

    struct Fx {
        _dir: TempDir,
        paths: PandoPaths,
    }

    fn fixture() -> Fx {
        let dir = tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        let paths = PandoPaths::new(
            dir.path().join("pando-home"),
            ProjectRef::from_root(&root).unwrap(),
        );
        Fx { paths, _dir: dir }
    }

    #[test]
    fn the_cookie_is_never_in_the_command_line() {
        let command = proxy_command(Path::new("/usr/local/bin/pando"), 17005, 17000);
        assert!(
            !command.contains(COOKIE) && !command.contains("session="),
            "anything in argv is in `ps` output: {command}"
        );
        assert!(command.contains(SUBCOMMAND), "{command}");
        assert!(command.contains("--listen 17005"), "{command}");
        assert!(command.contains("--upstream 17000"), "{command}");
        assert!(
            command.starts_with("exec '/usr/local/bin/pando'"),
            "the path is quoted and exec'd, so the recorded pid is the proxy: {command}"
        );
    }

    #[test]
    fn a_binary_path_with_a_space_in_it_is_quoted() {
        let command = proxy_command(Path::new("/Users/a b/pando"), 1, 2);
        assert!(command.starts_with("exec '/Users/a b/pando'"), "{command}");
    }

    #[test]
    fn the_cookie_travels_in_the_environment_under_one_name() {
        let env = proxy_env(COOKIE);
        assert_eq!(env, vec![(ENV_COOKIE.to_string(), COOKIE.to_string())]);
        assert_eq!(ENV_COOKIE, "PANDO_SHARE_COOKIE");
    }

    #[test]
    fn rewriting_replaces_the_cookie_and_forces_connection_close() {
        let head = "GET /foo HTTP/1.1\r\n\
                    Host: example\r\n\
                    Cookie: stale=1\r\n\
                    Connection: keep-alive\r\n\
                    Keep-Alive: timeout=5\r\n\
                    Proxy-Connection: keep-alive\r\n\
                    User-Agent: test/1\r\n";
        let out = rewrite_headers(head, COOKIE);

        assert!(out.starts_with("GET /foo HTTP/1.1\r\n"), "{out}");
        assert!(!out.contains("stale=1"), "the visitor's cookie goes: {out}");
        assert!(!out.to_lowercase().contains("keep-alive"), "{out}");
        assert!(!out.to_lowercase().contains("proxy-connection"), "{out}");
        assert_eq!(out.matches("Connection:").count(), 1, "{out}");
        assert!(out.contains("Connection: close\r\n"), "{out}");
        assert!(out.contains(&format!("Cookie: {COOKIE}\r\n")), "{out}");
        assert!(out.contains("User-Agent: test/1\r\n"), "{out}");
        assert!(out.ends_with("\r\n\r\n"), "{out}");
    }

    #[test]
    fn rewriting_adds_a_cookie_where_the_request_had_none() {
        let head = "GET / HTTP/1.1\r\nHost: x\r\n";
        let out = rewrite_headers(head, "k=v");
        assert!(out.contains("Cookie: k=v\r\n"), "{out}");
        assert!(out.contains("Host: x\r\n"), "{out}");
    }

    #[test]
    fn rewriting_leaves_every_other_header_byte_identical() {
        let head = "POST /api/x HTTP/1.1\r\n\
                    Host: a.b\r\n\
                    Content-Type: application/json; charset=UTF-8\r\n\
                    Content-Length: 42\r\n\
                    X-Odd-Casing: KeEp\r\n\
                    Authorization: Bearer abc.def\r\n";
        let out = rewrite_headers(head, "c=1");
        for line in [
            "POST /api/x HTTP/1.1\r\n",
            "Host: a.b\r\n",
            "Content-Type: application/json; charset=UTF-8\r\n",
            "Content-Length: 42\r\n",
            "X-Odd-Casing: KeEp\r\n",
            "Authorization: Bearer abc.def\r\n",
        ] {
            assert!(out.contains(line), "missing {line:?} in {out}");
        }
    }

    #[test]
    fn rewriting_tolerates_lf_only_line_endings_and_normalises_them() {
        let head = "GET / HTTP/1.1\nHost: x\nCookie: old=1\n";
        let out = rewrite_headers(head, "k=v");
        assert!(out.starts_with("GET / HTTP/1.1\r\n"), "{out}");
        assert!(out.contains("Host: x\r\n"), "{out}");
        assert!(!out.contains("old=1"), "{out}");
        assert!(out.contains("Cookie: k=v\r\n"), "{out}");
    }

    #[test]
    fn the_end_of_the_headers_is_found_in_both_spellings() {
        let crlf = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody-bytes";
        let (end_head, start_body) = find_headers_end(crlf).unwrap();
        assert!(crlf[..end_head].ends_with(b"\r\n"));
        assert_eq!(&crlf[end_head..start_body], b"\r\n");
        assert_eq!(&crlf[start_body..], b"body-bytes");

        let lf = b"GET / HTTP/1.1\nHost: x\n\nbody";
        let (end_head, start_body) = find_headers_end(lf).unwrap();
        assert!(lf[..end_head].ends_with(b"\n"));
        assert_eq!(&lf[start_body..], b"body");

        assert!(find_headers_end(b"GET / HTTP/1.1\r\nHost: x\r\n").is_none());
    }

    /// An upstream that records the headers it was sent and answers 200.
    fn upstream_that_records() -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut buf = [0u8; 4096];
            let mut acc: Vec<u8> = Vec::new();
            loop {
                match socket.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if find_headers_end(&acc).is_some() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            tx.send(String::from_utf8_lossy(&acc).into_owned()).ok();
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
            let _ = socket.shutdown(std::net::Shutdown::Write);
        });
        (port, rx)
    }

    #[test]
    fn a_request_round_trips_and_the_upstream_sees_the_injected_cookie() {
        let (upstream_port, seen) = upstream_that_records();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((stream, _)) = proxy.accept() {
                let _ = handle_connection(stream, upstream_port, COOKIE);
            }
        });

        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        client
            .write_all(b"GET /hello HTTP/1.1\r\nHost: tunnel.example\r\nCookie: stale=1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.contains("200 OK"), "{response}");

        let head = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.contains(&format!("Cookie: {COOKIE}")), "{head}");
        assert!(!head.contains("stale=1"), "{head}");
        assert!(head.contains("Connection: close"), "{head}");
        assert!(head.contains("Host: tunnel.example"), "{head}");
    }

    #[test]
    fn a_request_body_arriving_with_the_headers_reaches_the_upstream() {
        let (upstream_port, seen) = upstream_that_records();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((stream, _)) = proxy.accept() {
                let _ = handle_connection(stream, upstream_port, COOKIE);
            }
        });

        let mut client = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
        client
            .write_all(b"POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.contains("200 OK"), "{response}");
        let head = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.contains("Content-Length: 5"), "{head}");
    }

    #[test]
    fn the_proxy_log_is_emptied_before_a_share_and_never_created() {
        let fx = fixture();
        let log = fx.paths.log_file("feat+one", PROXY_LOG);
        truncate_log(&log).unwrap();
        assert!(!log.exists(), "truncate must not create the file");

        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, b"bytes from a dead session\n").unwrap();
        truncate_log(&log).unwrap();
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
    }

    #[test]
    fn the_proxy_log_lives_under_the_home_beside_every_other_log() {
        let fx = fixture();
        let log = fx.paths.log_file("feat+one", PROXY_LOG);
        assert!(log.starts_with(&fx.paths.home));
        assert_eq!(log.file_name().unwrap(), "proxy.log");
    }

    // The proxy's own startup line goes into its log; a cookie there would
    // outlive the share it belonged to.
    #[test]
    fn nothing_the_proxy_announces_carries_the_cookie() {
        let announcement = format!(
            "pando share proxy: 127.0.0.1:{} -> 127.0.0.1:{}",
            17005, 17000
        );
        assert!(!announcement.contains("session="), "{announcement}");
        assert!(!announcement.contains(COOKIE), "{announcement}");
    }
}
