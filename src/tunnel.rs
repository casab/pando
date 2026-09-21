//! Publishing one local port at a public URL.
//!
//! One provider in v1, `cloudflared` quick tunnels, behind a trait so a
//! second one can be added without touching `actions`.
//!
//! The tunnel is a process like any other: detached, its own process group,
//! logged to `logs/<worktree>/tunnel.log`, recorded in state, and swept.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::paths::PandoPaths;
use crate::process::{self, SpawnOptions};
use crate::state::ShareRecord;

/// The log source the tunnel writes to. Reserved in [`crate::paths`], so no
/// process, hook, or service can be given the same name.
pub const TUNNEL_LOG: &str = "tunnel";

/// How long a provider may take to publish a URL before the share fails.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Matches `actions`' own grace: a tunnel is not special enough to wait
/// longer for.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Lines of the log an error carries. Enough to see the provider's own
/// complaint, short enough to read on one screen.
const TAIL_LINES: usize = 5;

const URL_HOST: &str = ".trycloudflare.com";

/// The only value `[share].provider` takes in v1.
pub const DEFAULT_PROVIDER: &str = "cloudflared";

/// Without `--config <path>`, cloudflared reads `~/.cloudflared/config.yml`
/// and applies its `ingress:` rules to every request — even to a quick
/// tunnel created with `--url`. A developer who already runs a named tunnel
/// with a catch-all `service: http_status:404` would get empty 404s instead
/// of their dev server. Pointing cloudflared at this file, which pando owns
/// and which defines no ingress, makes it fall back to `--url` for
/// everything.
const EMPTY_TUNNEL_CONFIG: &str = "\
# Written by pando. An empty cloudflared config, whose whole purpose is to
# shadow ~/.cloudflared/config.yml so its ingress rules are not inherited by
# a quick tunnel. Edit it if you want to (pando never rewrites it); just do
# not add ingress rules, or shared worktrees will stop answering.
no-autoupdate: true
";

/// A running tunnel, before it is written to state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpawn {
    pub pid: u32,
    pub pgid: i32,
    pub public_url: String,
    pub log_path: PathBuf,
}

/// One way of publishing a local port.
///
/// v1 has exactly one implementation. The trait is here so the second one
/// arrives as a new file rather than as a branch inside `actions`.
pub trait Provider {
    /// What `[share].provider` calls this one.
    fn name(&self) -> &'static str;

    /// Whether the machine can run it at all, with an install hint when it
    /// cannot. Asked after every refusal, so a worktree that was never
    /// going to be shared does not get told to install anything.
    fn ensure_present(&self, paths: &PandoPaths) -> Result<()>;

    /// Publishes `local_port` and returns once a public URL exists.
    fn start(&self, paths: &PandoPaths, name: &str, local_port: u16) -> Result<TunnelSpawn>;
}

/// The provider `[share].provider` names, or the default when it names
/// none. An unknown name is refused rather than silently defaulted: a typo
/// that quietly shares through something else is worse than a failure.
pub fn provider_for(configured: Option<&str>) -> Result<Box<dyn Provider>> {
    match configured.unwrap_or(DEFAULT_PROVIDER) {
        DEFAULT_PROVIDER => Ok(Box::new(Cloudflared)),
        other => bail!(
            "[share].provider is {other:?}, and pando only speaks {DEFAULT_PROVIDER:?} — remove \
             the line to use it"
        ),
    }
}

pub struct Cloudflared;

impl Provider for Cloudflared {
    fn name(&self) -> &'static str {
        DEFAULT_PROVIDER
    }

    fn ensure_present(&self, paths: &PandoPaths) -> Result<()> {
        ensure_runnable(
            &cloudflared_program(paths),
            &paths.home.join("bin").join(DEFAULT_PROVIDER),
        )
    }

    fn start(&self, paths: &PandoPaths, name: &str, local_port: u16) -> Result<TunnelSpawn> {
        start_tunnel(paths, name, local_port)
    }
}

/// The cloudflared executable pando runs.
///
/// `<home>/bin/cloudflared` when it is there and executable, else whatever
/// `cloudflared` resolves to on PATH — the same hook `docker` has, for the
/// same two reasons: a developer whose cloudflared is not on the PATH pando
/// inherits has somewhere to put a shim, and the tests drive a fake one per
/// test home without mutating the process environment.
pub fn cloudflared_program(paths: &PandoPaths) -> PathBuf {
    let shim = paths.home.join("bin").join(DEFAULT_PROVIDER);
    if is_executable(&shim) {
        return shim;
    }
    PathBuf::from(DEFAULT_PROVIDER)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Refuses when the provider cannot be run, naming both ways of fixing it.
///
/// Split from [`Provider::ensure_present`] so the refusal is testable on a
/// machine that does have cloudflared installed.
fn ensure_runnable(program: &Path, shim: &Path) -> Result<()> {
    if program_is_runnable(program) {
        return Ok(());
    }
    bail!(
        "cloudflared is not installed — `brew install cloudflared`, or put a shim at {}",
        shim.display()
    )
}

/// Whether the program can be run: a shim is checked on disk, a bare name
/// is looked up the way the shell would.
fn program_is_runnable(program: &Path) -> bool {
    if program.components().count() > 1 {
        return is_executable(program);
    }
    Command::new("sh")
        .arg("-c")
        .arg(format!(
            "command -v {} >/dev/null 2>&1",
            shell_single_quote(&program.to_string_lossy())
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawns a quick tunnel onto `local_port` and waits for its public URL.
pub fn start_tunnel(paths: &PandoPaths, name: &str, local_port: u16) -> Result<TunnelSpawn> {
    let log_path = paths.log_file(name, TUNNEL_LOG);
    // `spawn_detached` opens the log with O_APPEND, so after
    // share → unshare → share the dead session's URL is still in the file
    // and the parser would hand it back instead of the live one.
    truncate_log(&log_path)?;
    let config_path = paths.tunnel_config_file();
    ensure_tunnel_config(&config_path)?;

    // `--no-autoupdate` keeps the background updater from restarting a
    // detached child mid-session. `--output default` forces the bordered
    // log format the parser expects even where `TUNNEL_LOG_OUTPUT=json` is
    // in the inherited environment. `--config` shadows the user's own.
    let shell_cmd = format!(
        "exec {program} tunnel --no-autoupdate --output default --config {config} \
         --url http://127.0.0.1:{local_port}",
        program = shell_single_quote(&cloudflared_program(paths).to_string_lossy()),
        config = shell_single_quote(&config_path.to_string_lossy()),
    );
    // Not the worktree: a tunnel holding a directory open is one more
    // reason `rm` cannot remove it, and the tunnel needs nothing from there.
    let cwd = std::env::temp_dir();
    let spawn = process::spawn_detached(SpawnOptions {
        shell_cmd: &shell_cmd,
        cwd: &cwd,
        log_file: &log_path,
        env: &[],
    })
    .context("spawn cloudflared")?;

    match await_url(spawn.pid, &log_path) {
        Ok(public_url) => Ok(TunnelSpawn {
            pid: spawn.pid,
            pgid: spawn.pgid,
            public_url,
            log_path,
        }),
        Err(e) => {
            // Never leave the child behind on the way out: nothing has
            // recorded it yet, so this is the last moment anything knows
            // its process group.
            let _ = process::stop(spawn.pgid, STOP_GRACE);
            Err(e)
        }
    }
}

/// Polls the log until a URL appears, the process dies, or the timeout
/// passes.
fn await_url(pid: u32, log_path: &Path) -> Result<String> {
    await_url_until(pid, log_path, Instant::now() + READY_TIMEOUT)
}

/// [`await_url`] with the deadline given, so a test can drive the timeout
/// branch without sitting through it.
fn await_url_until(pid: u32, log_path: &Path, deadline: Instant) -> Result<String> {
    loop {
        // Read the log before testing liveness, not after: a provider that
        // published a URL and exited in the same breath has still told us
        // what we asked, and checking liveness first would throw it away.
        let found = parse_url_from_log(log_path);
        let alive = process::is_alive(pid);
        if let Some(url) = found {
            if !alive {
                bail!(
                    "cloudflared published {url} and then exited — tail: {}",
                    tail_log(log_path)
                );
            }
            return Ok(url);
        }
        if !alive {
            bail!(
                "cloudflared exited before publishing a URL — tail: {}",
                tail_log(log_path)
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "cloudflared published no URL within {}s — tail: {}",
                READY_TIMEOUT.as_secs(),
                tail_log(log_path)
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Takes both halves of a share down: the tunnel first, so no new request
/// reaches a proxy that is about to go, then the proxy.
///
/// Both are always signalled, whatever the first one did — a stuck
/// cloudflared must not be the reason a proxy is orphaned. The first error
/// is what the caller sees.
pub fn stop_share(record: &ShareRecord) -> Result<()> {
    stop_share_with(record, |pgid| process::stop(pgid, STOP_GRACE))
}

/// [`stop_share`] with the signal injected, so a test can watch the order
/// without real process groups.
pub fn stop_share_with(record: &ShareRecord, stop: impl Fn(i32) -> Result<()>) -> Result<()> {
    let tunnel = stop(record.tunnel_pgid);
    let proxy = match record.proxy_pgid {
        Some(pgid) => stop(pgid),
        None => Ok(()),
    };
    tunnel.and(proxy)
}

/// Empties the log before a share, creating nothing that was not there.
fn truncate_log(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    if path.exists() {
        std::fs::write(path, b"")
            .with_context(|| format!("truncate tunnel log {}", path.display()))?;
    }
    Ok(())
}

/// Writes pando's own cloudflared config once, and never again: a developer
/// who edited it — to add a metrics endpoint, say — keeps their edit.
fn ensure_tunnel_config(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(path, EMPTY_TUNNEL_CONFIG)
            .with_context(|| format!("write tunnel config {}", path.display()))?;
    }
    Ok(())
}

fn parse_url_from_log(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().find_map(extract_trycloudflare_url)
}

/// The quick-tunnel URL on a log line, host only.
///
/// Everything past the host is dropped, so a future cloudflared that prints
/// a path or a query string still yields a URL that can be opened.
fn extract_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let host_end = rest.find(URL_HOST)? + URL_HOST.len();
    Some(rest[..host_end].to_string())
}

fn tail_log(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return "(no log)".to_string();
    };
    let last: Vec<&str> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(TAIL_LINES)
        .collect();
    if last.is_empty() {
        return "(empty log)".to_string();
    }
    last.into_iter().rev().collect::<Vec<_>>().join(" | ")
}

/// Single-quotes a string for a `bash -lc` command line. A home directory
/// with a space in it is ordinary; a shim path that is not quoted is a
/// command that runs the wrong thing.
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::testutil::wait_until;
    use chrono::Utc;
    use std::sync::Mutex;
    use tempfile::{TempDir, tempdir};

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

    /// Writes a fake cloudflared at `<home>/bin/cloudflared`, the same hook
    /// a developer would use for a real shim.
    fn fake_cloudflared(paths: &PandoPaths, body: &str) {
        let bin = paths.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join(DEFAULT_PROVIDER);
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Prints the URL in the format cloudflared really uses, then stays
    /// alive as a tunnel does. `exec` so the pid pando records is the one
    /// that has to be killed.
    const FAKE_PUBLISHES: &str = "#!/bin/sh\n\
         echo \"$(date -u +%Y-%m-%dT%H:%M:%SZ) INF Requesting new quick Tunnel...\"\n\
         echo \"INF +--------------------------------------------------------+\"\n\
         echo \"INF |  https://fake-tunnel-for-tests.trycloudflare.com        |\"\n\
         echo \"INF +--------------------------------------------------------+\"\n\
         exec sleep 300\n";

    const FAKE_SILENT: &str =
        "#!/bin/sh\necho 'INF Requesting new quick Tunnel...'\nexec sleep 300\n";

    const FAKE_EXITS: &str = "#!/bin/sh\necho 'ERR failed to request quick Tunnel'\nexit 7\n";

    fn share_record(tunnel_pgid: i32, proxy_pgid: Option<i32>) -> ShareRecord {
        ShareRecord {
            tunnel_pid: tunnel_pgid as u32,
            tunnel_pgid,
            public_url: "https://x.trycloudflare.com".into(),
            local_port: 17000,
            started_at: Utc::now(),
            log_path: PathBuf::from("tunnel.log"),
            proxy_pid: proxy_pgid.map(|p| p as u32),
            proxy_pgid,
            proxy_port: proxy_pgid.map(|_| 17001),
        }
    }

    #[test]
    fn the_isolated_config_is_written_under_the_home_and_defines_no_ingress() {
        let fx = fixture();
        let config = fx.paths.tunnel_config_file();
        assert!(config.starts_with(&fx.paths.home));
        assert!(!config.exists());

        ensure_tunnel_config(&config).unwrap();

        let content = std::fs::read_to_string(&config).unwrap();
        assert!(content.contains("no-autoupdate: true"), "{content}");
        assert!(
            !content.contains("ingress:"),
            "the whole point of this file is that it defines no ingress: {content}"
        );
    }

    #[test]
    fn the_isolated_config_is_never_rewritten() {
        let fx = fixture();
        let config = fx.paths.tunnel_config_file();
        let edited = "# mine\nno-autoupdate: true\nmetrics: 127.0.0.1:8081\n";
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, edited).unwrap();

        ensure_tunnel_config(&config).unwrap();
        ensure_tunnel_config(&config).unwrap();

        assert_eq!(std::fs::read_to_string(&config).unwrap(), edited);
    }

    #[test]
    fn a_url_is_extracted_from_the_line_cloudflared_really_prints() {
        assert_eq!(
            extract_trycloudflare_url(
                "2026-09-21T12:00:00Z INF |  https://threaded-fathers-explore-supplier.trycloudflare.com  |"
            ),
            Some("https://threaded-fathers-explore-supplier.trycloudflare.com".into())
        );
        assert_eq!(
            extract_trycloudflare_url("Visit https://abc-123-xyz.trycloudflare.com to try it"),
            Some("https://abc-123-xyz.trycloudflare.com".into())
        );
    }

    #[test]
    fn a_url_is_cut_at_the_host_so_nothing_after_it_is_carried() {
        assert_eq!(
            extract_trycloudflare_url("INF | https://named.trycloudflare.com/foo?token=secret |"),
            Some("https://named.trycloudflare.com".into()),
            "a path or query must never travel with the URL pando hands out"
        );
    }

    #[test]
    fn lines_without_a_quick_tunnel_url_yield_nothing() {
        assert_eq!(extract_trycloudflare_url("INF Starting tunnel..."), None);
        assert_eq!(
            extract_trycloudflare_url("INF | https://example.com |"),
            None
        );
        assert_eq!(extract_trycloudflare_url(""), None);
    }

    #[test]
    fn the_log_is_parsed_and_a_missing_or_urlless_file_says_so() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("tunnel.log");
        assert_eq!(parse_url_from_log(&log), None, "missing file");

        std::fs::write(&log, "INF Requesting new quick Tunnel...\n").unwrap();
        assert_eq!(parse_url_from_log(&log), None, "no URL yet");

        std::fs::write(
            &log,
            "INF requesting...\nINF |  https://aaa-bbb.trycloudflare.com  |\n",
        )
        .unwrap();
        assert_eq!(
            parse_url_from_log(&log),
            Some("https://aaa-bbb.trycloudflare.com".into())
        );
    }

    #[test]
    fn the_log_is_truncated_before_a_share_and_never_created() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("logs").join("tunnel.log");
        truncate_log(&log).unwrap();
        assert!(!log.exists(), "truncate must not create the file");

        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, b"INF | https://stale.trycloudflare.com |\n").unwrap();
        truncate_log(&log).unwrap();

        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        assert_eq!(
            parse_url_from_log(&log),
            None,
            "a stale URL must not survive into the next share"
        );
    }

    #[test]
    fn a_tunnel_publishes_its_url_and_is_stoppable() {
        let fx = fixture();
        fake_cloudflared(&fx.paths, FAKE_PUBLISHES);

        let spawn = start_tunnel(&fx.paths, "feat+one", 17000).unwrap();
        assert_eq!(
            spawn.public_url,
            "https://fake-tunnel-for-tests.trycloudflare.com"
        );
        assert_eq!(spawn.log_path, fx.paths.log_file("feat+one", TUNNEL_LOG));
        assert!(process::is_alive(spawn.pid), "the tunnel must still be up");

        process::stop(spawn.pgid, STOP_GRACE).unwrap();
        assert!(wait_until(Duration::from_secs(5), || !process::is_alive(
            spawn.pid
        )));
    }

    #[test]
    fn the_log_written_is_the_worktrees_own_tunnel_log() {
        let fx = fixture();
        fake_cloudflared(&fx.paths, FAKE_PUBLISHES);
        let spawn = start_tunnel(&fx.paths, "feat+one", 17000).unwrap();
        let _ = process::stop(spawn.pgid, STOP_GRACE);

        let log = std::fs::read_to_string(fx.paths.log_file("feat+one", TUNNEL_LOG)).unwrap();
        assert!(log.contains("trycloudflare.com"), "{log}");
        assert!(
            fx.paths
                .log_file("feat+one", TUNNEL_LOG)
                .starts_with(&fx.paths.home),
            "logs live under the home, never in the repository"
        );
    }

    // The timeout is 30s, which is far too long for a test to sit through,
    // so the wait is driven directly with a child that never publishes.
    #[test]
    fn a_provider_that_never_publishes_fails_with_the_log_tail() {
        let fx = fixture();
        fake_cloudflared(&fx.paths, FAKE_SILENT);
        let log = fx.paths.log_file("feat+one", TUNNEL_LOG);
        truncate_log(&log).unwrap();

        let spawn = process::spawn_detached(SpawnOptions {
            shell_cmd: &format!(
                "exec {}",
                shell_single_quote(&cloudflared_program(&fx.paths).to_string_lossy())
            ),
            cwd: &std::env::temp_dir(),
            log_file: &log,
            env: &[],
        })
        .unwrap();
        assert!(wait_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&log)
                .map(|s| s.contains("Requesting"))
                .unwrap_or(false)
        }));

        // A deadline in the past, so the loop reports the timeout it would
        // report thirty seconds from now.
        let err = await_url_until(spawn.pid, &log, Instant::now()).unwrap_err();
        let _ = process::stop(spawn.pgid, STOP_GRACE);

        let message = format!("{err:#}");
        assert!(message.contains("no URL"), "{message}");
        assert!(
            message.contains("Requesting new quick Tunnel"),
            "the tail of the log is the whole diagnosis: {message}"
        );
    }

    #[test]
    fn a_provider_that_exits_before_publishing_fails_with_the_log_tail() {
        let fx = fixture();
        fake_cloudflared(&fx.paths, FAKE_EXITS);
        let err = start_tunnel(&fx.paths, "feat+one", 17000).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("exited before publishing"), "{message}");
        assert!(message.contains("failed to request"), "{message}");
    }

    #[test]
    fn stop_share_signals_the_tunnel_before_the_proxy() {
        let order = Mutex::new(Vec::new());
        let record = share_record(4242, Some(8484));
        stop_share_with(&record, |pgid| {
            order.lock().unwrap().push(pgid);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            order.into_inner().unwrap(),
            vec![4242, 8484],
            "the tunnel goes first so no new request reaches a proxy that is about to die"
        );
    }

    #[test]
    fn stop_share_signals_the_proxy_even_when_the_tunnel_refuses_to_die() {
        let signalled = Mutex::new(Vec::new());
        let record = share_record(4242, Some(8484));
        let result = stop_share_with(&record, |pgid| {
            signalled.lock().unwrap().push(pgid);
            if pgid == 4242 { bail!("stuck") } else { Ok(()) }
        });
        assert!(result.is_err(), "the caller still learns the tunnel stuck");
        assert_eq!(
            signalled.into_inner().unwrap(),
            vec![4242, 8484],
            "a stuck tunnel must never be the reason a proxy is orphaned"
        );
    }

    #[test]
    fn stop_share_without_a_proxy_signals_only_the_tunnel() {
        let signalled = Mutex::new(Vec::new());
        let record = share_record(4242, None);
        stop_share_with(&record, |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        })
        .unwrap();
        assert_eq!(signalled.into_inner().unwrap(), vec![4242]);
    }

    #[test]
    fn the_program_is_the_home_shim_when_there_is_one() {
        let fx = fixture();
        assert_eq!(
            cloudflared_program(&fx.paths),
            PathBuf::from(DEFAULT_PROVIDER),
            "with no shim, whatever the shell would run"
        );
        fake_cloudflared(&fx.paths, FAKE_PUBLISHES);
        assert_eq!(
            cloudflared_program(&fx.paths),
            fx.paths.home.join("bin").join(DEFAULT_PROVIDER)
        );
    }

    // Not through `ensure_present`: this machine has cloudflared installed,
    // so the only honest way to test the refusal is to hand it a program
    // that is definitely not there.
    #[test]
    fn a_missing_provider_is_refused_with_an_install_hint() {
        let fx = fixture();
        let shim = fx.paths.home.join("bin").join(DEFAULT_PROVIDER);
        let err = ensure_runnable(Path::new("/nonexistent/cloudflared"), &shim).unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("not installed"), "{message}");
        assert!(message.contains("brew install cloudflared"), "{message}");
        assert!(
            message.contains(&shim.display().to_string()),
            "the message must say where a shim would go: {message}"
        );
    }

    #[test]
    fn a_shim_that_is_not_executable_is_not_installed() {
        let fx = fixture();
        let bin = fx.paths.home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let shim = bin.join(DEFAULT_PROVIDER);
        std::fs::write(&shim, "#!/bin/sh\n").unwrap();
        assert!(!program_is_runnable(&shim), "a file is not a program");
        assert!(ensure_runnable(&shim, &shim).is_err());

        fake_cloudflared(&fx.paths, FAKE_PUBLISHES);
        Cloudflared.ensure_present(&fx.paths).unwrap();
    }

    #[test]
    fn only_cloudflared_is_a_provider_and_a_typo_is_refused() {
        assert_eq!(provider_for(None).unwrap().name(), DEFAULT_PROVIDER);
        assert_eq!(
            provider_for(Some("cloudflared")).unwrap().name(),
            DEFAULT_PROVIDER
        );
        // `map` first: a boxed trait object has no `Debug` for `unwrap_err`.
        let err = provider_for(Some("ngrok")).map(|_| ()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("ngrok"), "{message}");
        assert!(message.contains("cloudflared"), "{message}");
    }

    #[test]
    fn a_shim_path_with_a_space_in_it_is_quoted() {
        assert_eq!(
            shell_single_quote("/tmp/x y/cloudflared"),
            "'/tmp/x y/cloudflared'"
        );
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn the_tail_says_so_when_there_is_nothing_to_say() {
        let dir = tempdir().unwrap();
        assert_eq!(tail_log(&dir.path().join("nope.log")), "(no log)");
        let empty = dir.path().join("empty.log");
        std::fs::write(&empty, "\n\n").unwrap();
        assert_eq!(tail_log(&empty), "(empty log)");
    }
}
