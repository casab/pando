//! Private copies of the project's services, one set per worktree.
//!
//! The compose adapter, and for now the only one: a project that already
//! has a compose file gets isolation with no new configuration beyond
//! which services to include. Native recipes are Phase 6 and land beside
//! this, behind the same shape.
//!
//! Everything here goes through one compose *project name* per worktree,
//! `pando-<project id>-<worktree>`, which is what isolates containers, the
//! network, and — because compose prefixes named volumes with it — the
//! data. Nothing is ever written into the repository: the override that
//! remaps the ports lives under pando's home and is passed with `-f`.
//!
//! Readiness never binds a port. A bind probe would take the port from the
//! server being waited for; a service with a compose `healthcheck` is
//! asked through `docker compose ps`, and one without is asked with a
//! connect.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::paths::PandoPaths;
use crate::ports;

/// How long one service gets to become ready before the start is failed.
pub const DEFAULT_READY_TIMEOUT_S: u64 = 60;

/// How often readiness is re-checked. Short enough that a redis that comes
/// up in half a second is not waited on for two.
const POLL: Duration = Duration::from_millis(250);

/// How many poll rounds between `docker compose ps` calls when nothing is
/// healthchecked. Every round would be a process spawn four times a
/// second for a whole minute; this is often enough to notice a container
/// that died without making the wait itself expensive.
const CHECK_EVERY: u32 = 8;

/// What docker says when the binary is present and the daemon is not. Two
/// spellings, because the classic Unix-socket message and the newer one
/// differ, and both mean the same thing to the developer.
const DAEMON_DOWN: [&str; 3] = [
    "Cannot connect to the Docker daemon",
    "docker daemon is not running",
    // Newer clients, and OrbStack's socket: "failed to connect to the
    // docker API at unix://…; check if the path is correct and if the
    // daemon is running".
    "failed to connect to the docker API",
];

/// Docker answered, and the answer was that its daemon is not running.
///
/// Its own type because the right response depends on who is asking. A
/// start that needs containers has to stop and say so; a `stop` or an
/// `rm` of containers a daemon that is down cannot be running has nothing
/// left to do, and failing there turns "Docker is off" into "pando cannot
/// stop my worktree".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonDown;

impl std::fmt::Display for DaemonDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "isolated mode needs Docker, and the Docker daemon is not running — start Docker, \
             or {}",
            crate::remedy::SHARED
        )
    }
}

impl std::error::Error for DaemonDown {}

/// How long a compose question that only reads may take: `ps`, and
/// `config`. Docker Desktop can wedge with its socket still accepting, and
/// then `docker compose ps` waits for ever — while `rm` holds the state
/// lock around it, which freezes `pando ls` and the TUI's tick with it.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long `stop` and `down -v` may take. Generous, because compose gives
/// each container its own grace period and a project can have several;
/// bounded, because `rm` runs them under the state lock too.
pub const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(300);

/// Docker was asked and did not answer in time: a daemon that is up but
/// wedged, which is not the same as one that is not running. Its
/// containers may well be running, so nothing that acts on "Docker is
/// off" may act on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonHung {
    pub verb: String,
    pub timeout: Duration,
}

impl std::fmt::Display for DaemonHung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Docker did not answer `docker compose {}` in {}s — it looks hung; restarting \
             Docker usually clears it",
            self.verb,
            self.timeout.as_secs()
        )
    }
}

impl std::error::Error for DaemonHung {}

/// Whether an error is, underneath its context, a daemon that did not
/// answer in time.
pub fn is_daemon_hung(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DaemonHung>().is_some()
}

/// Whether an error is, underneath whatever context it gathered, a
/// daemon that is not running.
pub fn is_daemon_down(e: &anyhow::Error) -> bool {
    e.downcast_ref::<DaemonDown>().is_some()
}

/// The docker executable pando runs.
///
/// `<home>/bin/docker` when it is there and executable, else whatever
/// `docker` resolves to on PATH. Two reasons for the hook: a developer
/// whose docker is not on the PATH pando inherits has somewhere to put a
/// shim, and the tests drive a fake docker per test home without mutating
/// the process environment — `std::env::set_var` is unsafe and racy with
/// tests running in parallel, and a child `PATH` is not reliably what
/// program lookup uses.
pub fn docker_program(paths: &PandoPaths) -> PathBuf {
    let shim = paths.home.join("bin").join("docker");
    if is_executable(&shim) {
        return shim;
    }
    PathBuf::from("docker")
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// One worktree's compose project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compose {
    program: PathBuf,
    project: String,
    /// The project's own file first, pando's override second. Empty for
    /// the by-project form.
    files: Vec<PathBuf>,
    /// The directory compose resolves relative paths against, which is the
    /// worktree. `None` for the by-project form.
    dir: Option<PathBuf>,
}

impl Compose {
    pub fn new(
        program: impl Into<PathBuf>,
        project: impl Into<String>,
        files: Vec<PathBuf>,
        dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            project: project.into(),
            files,
            dir: Some(dir.into()),
        }
    }

    /// The form `stop` and `rm` use: they never load config, so they have
    /// no compose file to name — and compose does not need one, because it
    /// labels every container it created with its project.
    pub fn by_project(program: impl Into<PathBuf>, project: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            project: project.into(),
            files: Vec::new(),
            dir: None,
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    /// `compose -p <project> [-f <file>…] <rest…>`, which every invocation
    /// starts with.
    pub fn args(&self, rest: &[&str]) -> Vec<String> {
        let mut args = vec![
            "compose".to_string(),
            "-p".to_string(),
            self.project.clone(),
        ];
        for file in &self.files {
            args.push("-f".to_string());
            args.push(file.display().to_string());
        }
        args.extend(rest.iter().map(|a| (*a).to_string()));
        args
    }

    /// Runs one compose verb, bounded by `timeout` when there is one. `up`
    /// has none: pulling an image takes as long as the network does, and
    /// a start is never run under the state lock.
    fn run(&self, rest: &[&str], timeout: Option<Duration>) -> Result<String> {
        let args = self.args(rest);
        let mut command = Command::new(&self.program);
        command.args(&args).stdin(Stdio::null());
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        let out = match timeout {
            Some(timeout) => crate::project::output_within(command, timeout),
            None => command.output(),
        };
        if let (Err(e), Some(timeout)) = (&out, timeout)
            && e.kind() == std::io::ErrorKind::TimedOut
        {
            return Err(anyhow::Error::new(DaemonHung {
                verb: rest.first().copied().unwrap_or_default().to_string(),
                timeout,
            }));
        }
        let out = out.with_context(|| {
            format!(
                "run {} {} — isolated mode needs Docker; install it, or {}",
                self.program.display(),
                args.join(" "),
                crate::remedy::SHARED
            )
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // The binary is there and the daemon is not. That is one thing
            // to do about it, and a compose command line with two `-f`
            // paths in it is not how to say so.
            if DAEMON_DOWN.iter().any(|needle| stderr.contains(needle)) {
                return Err(anyhow::Error::new(DaemonDown));
            }
            let reason = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no output")
                .trim();
            bail!(
                "`{} {}` exited {}: {reason}",
                self.program.display(),
                args.join(" "),
                out.status.code().unwrap_or(-1)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The fully resolved file, as compose itself reads it.
    ///
    /// `extends:` and a top-level `include:` are followed and every default
    /// is filled in — none of which pando's own reader does. Nothing is
    /// created or started: `config` only prints.
    pub fn config(&self) -> Result<crate::compose::ComposeFile> {
        let text = self.run(&["config", "--format", "json"], Some(PROBE_TIMEOUT))?;
        crate::compose::parse_config_json(&text)
    }

    /// Brings the included services up in the background. Idempotent:
    /// compose leaves a container that is already running and matches its
    /// configuration exactly as it is.
    pub fn up(&self, services: &[String]) -> Result<()> {
        let mut rest = vec!["up", "-d"];
        rest.extend(services.iter().map(String::as_str));
        self.run(&rest, None)?;
        Ok(())
    }

    /// Whether the daemon answers at all, asked the cheapest way compose
    /// has: `ps` of this project, which creates and starts nothing.
    ///
    /// What an isolated start asks *before* it stops anything, so "Docker
    /// is not running" is a refusal rather than the end of a start that
    /// already took the running environment down.
    pub fn reachable(&self) -> Result<()> {
        self.run(&["ps", "--all", "--format", "json"], Some(PROBE_TIMEOUT))?;
        Ok(())
    }

    /// What compose says about every container of this project.
    pub fn ps(&self) -> Result<Vec<Status>> {
        let text = self.run(&["ps", "--all", "--format", "json"], Some(PROBE_TIMEOUT))?;
        parse_ps(&text)
    }

    /// Stops the containers and leaves the volumes. What `stop` does: the
    /// data survives, and the next start brings the same database back.
    pub fn stop(&self) -> Result<()> {
        self.run(&["stop"], Some(TEARDOWN_TIMEOUT))?;
        Ok(())
    }

    /// Removes the containers, the network, and the named volumes. What
    /// `rm` does: the worktree is going, and its database goes with it.
    pub fn down_with_volumes(&self) -> Result<()> {
        self.run(&["down", "-v"], Some(TEARDOWN_TIMEOUT))?;
        Ok(())
    }

    /// The shell command that pumps one service's container log into a
    /// file, run detached through [`crate::process::spawn_detached`] so it
    /// is a process group pando can sweep like any other.
    pub fn logs_shell_cmd(&self, service: &str) -> String {
        let mut parts = vec![shell_quote(&self.program.display().to_string())];
        for arg in self.args(&["logs", "-f", "--no-color", service]) {
            parts.push(shell_quote(&arg));
        }
        parts.join(" ")
    }
}

/// Single quotes, with any single quote inside closed, escaped, reopened.
/// A worktree path can hold a space, and a docker shim path can hold both.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// One container of the compose project, as `docker compose ps` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub service: String,
    /// `running`, `exited`, `created`, …
    pub state: String,
    /// `starting`, `healthy`, `unhealthy`, or empty when the service
    /// declares no healthcheck.
    pub health: String,
    /// Published host ports, as `(host, container)`.
    pub published: Vec<(u16, u16)>,
}

impl Status {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// A container that will never become ready on its own.
    pub fn dead(&self) -> bool {
        matches!(self.state.as_str(), "exited" | "dead" | "removing")
    }
}

/// Compose 5 emits one JSON object per line. Older ones emit a single JSON
/// array, so both are accepted: a developer on an older compose should get
/// a working pando, not a parse error with a version number in it.
pub fn parse_ps(text: &str) -> Result<Vec<Status>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let values: Vec<serde_json::Value> =
            serde_json::from_str(trimmed).context("parse `docker compose ps --format json`")?;
        return Ok(values.iter().map(status_from).collect());
    }
    let mut out = Vec::new();
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parse a `docker compose ps` line: {line}"))?;
        out.push(status_from(&value));
    }
    Ok(out)
}

fn status_from(value: &serde_json::Value) -> Status {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let published = value
        .get("Publishers")
        .and_then(|v| v.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let host = entry.get("PublishedPort")?.as_u64()? as u16;
                    let container = entry.get("TargetPort")?.as_u64()? as u16;
                    (host != 0).then_some((host, container))
                })
                .collect()
        })
        .unwrap_or_default();
    Status {
        service: string("Service"),
        state: string("State"),
        health: string("Health"),
        published,
    }
}

// ---- telling the app where its services are -------------------------------

/// Where a value is looked up, in order. `.env` first because it holds the
/// real credentials for this project; the example files are the fallback
/// for a worktree that has none.
const ENV_FILES: [&str; 4] = [".env", ".env.example", ".env.sample", ".env.template"];

/// The environment that points an app at *this* worktree's services.
///
/// For each `ENV_KEY = "service"` in a `[[services]]` entry, the value the
/// project already uses is read from the worktree's own env files and its
/// port replaced with the one pando allocated. A URL keeps its
/// credentials, its database name, and its query string; a bare number
/// becomes the port.
///
/// This is what replaces materialising a rewritten `.env` inside the
/// worktree, which Invariant 1 forbids unless the project ignores it. The
/// same map reaches the processes, the hooks, and `pando status --env`.
pub fn app_env(
    worktree: &Path,
    mapping: &std::collections::BTreeMap<String, String>,
    ports: &std::collections::BTreeMap<String, u16>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let files = read_env_files(worktree);
    let mut out = std::collections::BTreeMap::new();
    for (key, service) in mapping {
        let port = *ports.get(service).with_context(|| {
            format!("no port was allocated for the service {service:?} (env.{key})")
        })?;
        let Some((source, value)) = files
            .iter()
            .find_map(|(name, map)| map.get(key).map(|value| (name.as_str(), value.as_str())))
        else {
            bail!(
                "env.{key} points at the service {service:?}, but nothing in this worktree says \
                 what {key} normally looks like — add it to .env.example (or .env), or drop it \
                 from the [[services]] env map"
            );
        };
        out.insert(key.clone(), rewrite(key, value, source, service, port)?);
    }
    Ok(out)
}

fn rewrite(key: &str, value: &str, source: &str, service: &str, port: u16) -> Result<String> {
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Ok(port.to_string());
    }
    if let Some(rewritten) = rewrite_url(trimmed, service, port) {
        return Ok(rewritten);
    }
    bail!(
        "{key}={trimmed:?} in {source} is neither a URL nor a port number, so pando cannot point \
         it at {service:?} — make it a URL or a bare port, or drop {key} from the [[services]] \
         env map"
    )
}

/// A URL with its port replaced, and its host replaced too when the host
/// is the compose service's own name: inside the compose network a service
/// is reachable as `postgres`, and from the host it is `localhost`.
fn rewrite_url(value: &str, service: &str, port: u16) -> Option<String> {
    let after_scheme = value.find("://")? + 3;
    let (head, rest) = value.split_at(after_scheme);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(at) => authority.split_at(at + 1),
        None => ("", authority),
    };
    // An IPv6 host is bracketed and full of colons; only the one after the
    // closing bracket separates the port.
    let host = if hostport.starts_with('[') {
        match hostport.find(']') {
            Some(close) => &hostport[..=close],
            None => hostport,
        }
    } else {
        match hostport.rfind(':') {
            Some(colon) => &hostport[..colon],
            None => hostport,
        }
    };
    if host.is_empty() {
        return None;
    }
    let host = if host == service { "localhost" } else { host };
    Some(format!("{head}{userinfo}{host}:{port}{tail}"))
}

/// The user and the database name a connection URL carries, if it carries
/// them.
///
/// A native service has to *create* what the app's own URL asks for: a
/// Postgres cluster that initdb made has one database called `postgres`
/// and nothing called `acme_dev`. The port is rewritten by [`app_env`];
/// these two are what a recipe's `create` command needs on top of it.
///
/// Percent-escapes are not decoded. A user name with a `%40` in it is
/// vanishingly rare in a development URL, and a wrong guess here would be
/// spliced into a SQL statement — so the caller checks the shape of what
/// comes back and refuses anything that is not a plain identifier.
pub fn url_identity(value: &str) -> (Option<String>, Option<String>) {
    let Some(after_scheme) = value.find("://").map(|at| at + 3) else {
        return (None, None);
    };
    let rest = &value[after_scheme..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let user = authority
        .rfind('@')
        .map(|at| &authority[..at])
        .map(|userinfo| match userinfo.find(':') {
            Some(colon) => &userinfo[..colon],
            None => userinfo,
        })
        .filter(|user| !user.is_empty())
        .map(str::to_string);
    let database = tail
        .strip_prefix('/')
        .map(|path| path.split(['?', '#']).next().unwrap_or_default())
        .filter(|database| !database.is_empty())
        .map(str::to_string);
    (user, database)
}

/// Who the app connects as, and to which database, from keys beside the
/// one that addresses the service: `(user, database)`.
///
/// Many projects address a database as parts — `DB_HOST`, `DB_PORT`,
/// `DB_NAME`, `DB_USER` — rather than as one URL, and only a URL carries a
/// database name. So for each addressing key `<P>_PORT`, `<P>_HOST` or
/// `<P>_URL`, the siblings `<P>_NAME`, `<P>_DATABASE` or `<P>_DB` and
/// `<P>_USER` or `<P>_USERNAME` are read, first found wins. A database
/// "name" that is all digits is a numbered database — redis's `REDIS_DB=2`
/// — and not a name anything creates.
pub fn sibling_identity<'a>(
    worktree: &Path,
    keys: impl IntoIterator<Item = &'a str>,
) -> (Option<String>, Option<String>) {
    let files = read_env_files(worktree);
    let lookup = |key: &str| {
        files
            .iter()
            .find_map(|(_, map)| map.get(key))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let mut user = None;
    let mut database = None;
    for key in keys {
        let Some(prefix) = ["_PORT", "_HOST", "_URL"]
            .iter()
            .find_map(|suffix| key.strip_suffix(suffix))
        else {
            continue;
        };
        if database.is_none() {
            database = ["_NAME", "_DATABASE", "_DB"]
                .iter()
                .filter_map(|suffix| lookup(&format!("{prefix}{suffix}")))
                .find(|value| !value.chars().all(|c| c.is_ascii_digit()));
        }
        if user.is_none() {
            user = ["_USER", "_USERNAME"]
                .iter()
                .find_map(|suffix| lookup(&format!("{prefix}{suffix}")));
        }
    }
    (user, database)
}

/// Something an app keeps beside a service's address, by the same rule
/// [`sibling_identity`] reads a database's name by: for each addressing
/// key `<P>_PORT`, `<P>_HOST` or `<P>_URL` among `keys`, the first of
/// `<P><suffix>` this directory's env files set to anything — as the key
/// it was found under, and its value.
///
/// `DATABASE_PASSWORD` beside `DATABASE_PORT`, `REDIS_DB` beside
/// `REDIS_PORT`.
pub fn sibling_value<'a>(
    dir: &Path,
    keys: impl IntoIterator<Item = &'a str>,
    suffixes: &[&str],
) -> Option<(String, String)> {
    let files = read_env_files(dir);
    keys.into_iter().find_map(|key| {
        let prefix = ["_PORT", "_HOST", "_URL"]
            .iter()
            .find_map(|suffix| key.strip_suffix(suffix))?;
        suffixes.iter().find_map(|suffix| {
            let sibling = format!("{prefix}{suffix}");
            let value = files.iter().find_map(|(_, map)| map.get(&sibling))?;
            let value = value.trim();
            (!value.is_empty()).then(|| (sibling, value.to_string()))
        })
    })
}

/// The user and the password a connection URL carries before its `@`,
/// percent-escapes decoded: a password with an `@` or a `:` in it can only
/// be written in a URL escaped, and the client that is handed it wants
/// the characters, not the escapes.
pub fn url_userinfo(value: &str) -> (Option<String>, Option<String>) {
    let Some(after_scheme) = value.find("://").map(|at| at + 3) else {
        return (None, None);
    };
    let rest = &value[after_scheme..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let Some(at) = authority.rfind('@') else {
        return (None, None);
    };
    let userinfo = &authority[..at];
    let (user, password) = match userinfo.split_once(':') {
        Some((user, password)) => (user, Some(password)),
        None => (userinfo, None),
    };
    let decoded = |text: &str| Some(percent_decoded(text)).filter(|t| !t.is_empty());
    (decoded(user), password.and_then(decoded))
}

/// `%40` as `@`; anything that is not a valid escape stays as written.
fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|pair| pair.iter().all(u8::is_ascii_hexdigit))
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The port an env key names in this directory's env files: the port of a
/// URL, or a bare number.
///
/// What shared mode probes. The services there are the ones the developer
/// already runs, on the ports the project's own files say, so pando reads
/// rather than assigns.
pub fn port_in_env(dir: &Path, key: &str) -> Option<u16> {
    port_of_value(&value_in_env(dir, key)?)
}

/// The value an env key has in this directory's env files, in lookup order.
pub fn value_in_env(dir: &Path, key: &str) -> Option<String> {
    read_env_files(dir)
        .iter()
        .find_map(|(_, map)| map.get(key).cloned())
}

fn port_of_value(value: &str) -> Option<u16> {
    let value = value.trim();
    if let Ok(port) = value.parse::<u16>() {
        return Some(port);
    }
    let after_scheme = value.find("://")? + 3;
    let rest = &value[after_scheme..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let hostport = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    // An IPv6 host is bracketed; only the colon after `]` is the port's.
    let colon = if hostport.starts_with('[') {
        hostport.find(']').map(|close| close + 1)?
    } else {
        hostport.rfind(':')?
    };
    hostport.get(colon + 1..)?.parse().ok()
}

/// Every env file the worktree has, in lookup order, each as a key map.
fn read_env_files(worktree: &Path) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    let mut out = Vec::new();
    for name in ENV_FILES {
        let Ok(text) = std::fs::read_to_string(worktree.join(name)) else {
            continue;
        };
        out.push((name.to_string(), parse_env(&text)));
    }
    out
}

/// `KEY=value` lines, with `export` and surrounding quotes dropped. Not a
/// dotenv implementation: it reads what a key looks like, and the only
/// thing pando does with the answer is swap a number inside it.
pub fn parse_env(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = match (value.starts_with('"'), value.starts_with('\'')) {
            (true, _) if value.len() >= 2 && value.ends_with('"') => &value[1..value.len() - 1],
            (_, true) if value.len() >= 2 && value.ends_with('\'') => &value[1..value.len() - 1],
            _ => value,
        };
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// A service being waited on: which port it was given, and whether its
/// compose entry declares a healthcheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub service: String,
    pub port: u16,
    pub healthcheck: bool,
}

/// Waits until every service is ready, or fails naming the first one that
/// was not and how long it was given.
///
/// Health when the service declares it, because a connect succeeding says
/// the socket is open and not that the database will answer a query —
/// Postgres in particular accepts connections while it is still
/// initialising. A connect otherwise, and never a bind.
pub fn wait_ready(
    compose: &Compose,
    wanted: &[Wanted],
    timeout: Duration,
    progress: &dyn Fn(&str),
) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    let mut pending: Vec<&Wanted> = wanted.iter().collect();
    let watched = pending.iter().any(|w| w.healthcheck);
    for service in &pending {
        progress(&format!("waiting for {}", service.service));
    }
    let mut round = 0u32;
    loop {
        // One `ps` per round, not one per service: it is a process spawn,
        // and it answers for every container of the project at once.
        //
        // Every round when a healthcheck is the answer; otherwise on a
        // slower beat, purely so a container that *exited* is noticed at
        // once. A postgres that refuses to start without a password dies
        // in a second, and waiting the whole minute to say "did not
        // become ready" hides the one line that explains it.
        let statuses = if watched || round.is_multiple_of(CHECK_EVERY) {
            compose.ps()?
        } else {
            Vec::new()
        };
        round += 1;
        pending.retain(|want| !is_ready(want, &statuses));
        if pending.is_empty() {
            return Ok(());
        }
        // A container that has exited is never going to be ready, and
        // waiting the whole timeout to say so wastes a minute of the
        // developer's time for an answer already on disk.
        if let Some(want) = pending.iter().find(|want| {
            statuses
                .iter()
                .any(|s| s.service == want.service && s.dead())
        }) {
            bail!(
                "the service {:?} exited before it was ready — `docker compose -p {} logs {}` \
                 says why",
                want.service,
                compose.project(),
                want.service
            );
        }
        if Instant::now() >= deadline {
            let names: Vec<&str> = pending.iter().map(|w| w.service.as_str()).collect();
            bail!(
                "the service{} {} did not become ready in {}s",
                if names.len() == 1 { "" } else { "s" },
                names.join(", "),
                // Rounded up, never down: "did not become ready in 0s" is
                // a sentence that makes pando look broken.
                timeout.as_millis().div_ceil(1000)
            );
        }
        std::thread::sleep(POLL);
    }
}

/// Compose health when the service declares a healthcheck, because that is
/// the project's own answer to "is this up" and nothing pando can do from
/// outside beats it.
///
/// Otherwise a connect that also proves something is *behind* the port.
/// Docker's published port is a proxy that completes the handshake as soon
/// as the container is running, so a plain connect would declare a postgres
/// ready while it is still running `initdb` — and the detected `migrate`
/// hook would then run against a database refusing connections. See
/// [`ports::something_is_serving`].
fn is_ready(want: &Wanted, statuses: &[Status]) -> bool {
    if want.healthcheck {
        return statuses
            .iter()
            .any(|s| s.service == want.service && s.health == "healthy");
    }
    ports::something_is_serving(want.port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn home_with_shim(body: &str) -> (TempDir, PandoPaths) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let home = dir.path().join("pando-home");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let shim = home.join("bin").join("docker");
        std::fs::write(&shim, body).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = PandoPaths::new(home, ProjectRef::from_root(&root).unwrap());
        (dir, paths)
    }

    #[test]
    fn a_service_addressed_in_parts_names_its_database_and_user_in_siblings() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".env.example"),
            "DB_HOST=localhost\nDB_PORT=3306\nDB_NAME=shop\nDB_USERNAME=shop_user\n\
             REDIS_PORT=6379\nREDIS_DB=2\n",
        )
        .unwrap();
        assert_eq!(
            sibling_identity(dir.path(), ["DB_PORT"]),
            (Some("shop_user".to_string()), Some("shop".to_string()))
        );
        assert_eq!(
            sibling_identity(dir.path(), ["REDIS_PORT"]),
            (None, None),
            "a numbered database is not a name anything creates"
        );
        assert_eq!(sibling_identity(dir.path(), ["PORT"]), (None, None));
        // `.env` is read before the example, as everywhere else.
        std::fs::write(dir.path().join(".env"), "DB_NAME=shop_local\n").unwrap();
        assert_eq!(
            sibling_identity(dir.path(), ["DB_PORT"]).1.as_deref(),
            Some("shop_local")
        );
    }

    #[test]
    fn a_urls_login_is_read_with_only_real_escapes_undone() {
        assert_eq!(
            url_userinfo("mysql://a%40b:c%3Ad@h:1/x"),
            (Some("a@b".to_string()), Some("c:d".to_string()))
        );
        // Not escapes: left exactly as written.
        assert_eq!(
            url_userinfo("mysql://u:%zz%+1%@h/x").1.as_deref(),
            Some("%zz%+1%")
        );
        assert_eq!(url_userinfo("mysql://h:1/x"), (None, None));
        assert_eq!(url_userinfo("mysql://:@h:1/x"), (None, None));
        assert_eq!(url_userinfo("not a url"), (None, None));
        // The `@` that ends the login is the last one before the host.
        assert_eq!(
            url_userinfo("redis://:p@ss@h:6379/0").1.as_deref(),
            Some("p@ss")
        );
    }

    #[test]
    fn a_value_beside_an_address_is_found_by_its_prefix() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".env.example"),
            "DB_PORT=1\nDB_PASS=example\nREDIS_URL=redis://h\nREDIS_DB=2\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(".env"), "DB_PASSWORD=\nDB_PWD=real\n").unwrap();
        // An empty `.env` value is nothing, and the next spelling is read.
        assert_eq!(
            sibling_value(dir.path(), ["DB_PORT"], &["_PASSWORD", "_PWD", "_PASS"]),
            Some(("DB_PWD".to_string(), "real".to_string()))
        );
        assert_eq!(
            sibling_value(dir.path(), ["REDIS_URL"], &["_DB"]),
            Some(("REDIS_DB".to_string(), "2".to_string()))
        );
        assert_eq!(sibling_value(dir.path(), ["PORT"], &["_DB"]), None);
    }

    #[test]
    fn docker_comes_from_the_path_unless_a_shim_is_in_pandos_home() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("h"), ProjectRef::from_root(&root).unwrap());
        assert_eq!(docker_program(&paths), PathBuf::from("docker"));

        // Present but not executable is not a shim; it is a file somebody
        // left there, and running it would fail in a confusing way.
        std::fs::create_dir_all(paths.home.join("bin")).unwrap();
        std::fs::write(paths.home.join("bin").join("docker"), "x").unwrap();
        assert_eq!(docker_program(&paths), PathBuf::from("docker"));

        let shim = paths.home.join("bin").join("docker");
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(docker_program(&paths), shim);
    }

    #[test]
    fn every_invocation_names_the_project_and_both_files() {
        let compose = Compose::new(
            "docker",
            "pando-acme-feat-one",
            vec![
                PathBuf::from("/wt/docker-compose.yml"),
                PathBuf::from("/h/o.yml"),
            ],
            "/wt",
        );
        assert_eq!(
            compose.args(&["up", "-d", "postgres"]),
            vec![
                "compose",
                "-p",
                "pando-acme-feat-one",
                "-f",
                "/wt/docker-compose.yml",
                "-f",
                "/h/o.yml",
                "up",
                "-d",
                "postgres"
            ]
        );
    }

    #[test]
    fn the_by_project_form_names_no_file_because_stop_has_no_config() {
        let compose = Compose::by_project("docker", "pando-acme-feat-one");
        assert_eq!(
            compose.args(&["down", "-v"]),
            vec!["compose", "-p", "pando-acme-feat-one", "down", "-v"]
        );
    }

    #[test]
    fn the_log_pump_command_quotes_every_path_it_names() {
        let compose = Compose::new(
            "/pando home/bin/docker",
            "pando-p-feat-one",
            vec![PathBuf::from("/a b/docker-compose.yml")],
            "/a b",
        );
        assert_eq!(
            compose.logs_shell_cmd("postgres"),
            "'/pando home/bin/docker' 'compose' '-p' 'pando-p-feat-one' '-f' \
             '/a b/docker-compose.yml' 'logs' '-f' '--no-color' 'postgres'"
        );
    }

    // Compose 5.0.1 on the development machine: one object per line.
    #[test]
    fn ps_reads_the_ndjson_compose_five_emits() {
        let text = concat!(
            r#"{"Service":"postgres","State":"running","Health":"healthy","Publishers":[{"URL":"127.0.0.1","TargetPort":5432,"PublishedPort":17004,"Protocol":"tcp"}]}"#,
            "\n",
            r#"{"Service":"redis","State":"running","Health":"","Publishers":[{"URL":"","TargetPort":6379,"PublishedPort":0,"Protocol":"tcp"}]}"#,
            "\n"
        );
        let statuses = parse_ps(text).unwrap();
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].service, "postgres");
        assert_eq!(statuses[0].health, "healthy");
        assert_eq!(statuses[0].published, vec![(17_004, 5432)]);
        assert!(statuses[0].running());
        assert!(!statuses[0].dead());
        // An unpublished port is not a port anything on the host can reach.
        assert!(statuses[1].published.is_empty());
    }

    #[test]
    fn ps_also_reads_the_json_array_an_older_compose_emits() {
        let text = r#"[{"Service":"db","State":"exited","Health":""}]"#;
        let statuses = parse_ps(text).unwrap();
        assert_eq!(statuses[0].service, "db");
        assert!(statuses[0].dead());
        assert!(!statuses[0].running());
        assert!(parse_ps("").unwrap().is_empty());
        assert!(parse_ps("  \n ").unwrap().is_empty());
    }

    #[test]
    fn a_docker_that_fails_is_reported_with_its_last_line() {
        let (_dir, paths) =
            home_with_shim("#!/bin/sh\necho 'no such service: nope' >&2\nexit 14\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let err = format!("{:#}", compose.up(&["nope".to_string()]).unwrap_err());
        assert!(err.contains("exited 14"), "{err}");
        assert!(err.contains("no such service: nope"), "{err}");
        assert!(err.contains("compose -p pando-x-y up -d nope"), "{err}");
    }

    // A daemon that is down is its own error, so a `stop` can tell it from
    // a failure without reading the sentence.
    #[test]
    fn a_daemon_that_is_down_is_recognisable_through_any_context() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\necho 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.' >&2\nexit 1\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let err = compose.stop().unwrap_err().context("stopping x");
        assert!(is_daemon_down(&err), "{err:#}");
        assert!(format!("{err:#}").contains("Docker daemon is not running"));
        let err = compose.reachable().unwrap_err();
        assert!(is_daemon_down(&err), "{err:#}");
    }

    // OrbStack's socket, and newer docker clients, say it another way; read
    // as a generic failure it made `stop` and `rm` fail where a daemon that
    // is down has nothing to stop.
    #[test]
    fn a_daemon_down_in_the_newer_wording_is_recognised() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\necho 'failed to connect to the docker API at unix:///x/docker.sock; check if the path is correct and if the daemon is running: dial unix /x/docker.sock: connect: no such file or directory' >&2\nexit 1\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let err = compose.reachable().unwrap_err();
        assert!(is_daemon_down(&err), "{err:#}");
    }

    // A wedged Docker Desktop accepts the connection and never answers, and
    // `rm` asked it under the state lock: every other pando froze with it.
    // The read-only probe is bounded, and a timeout is its own error, told
    // apart from a daemon that is down because its containers may be up.
    #[test]
    fn a_docker_that_never_answers_is_given_up_on() {
        let (_dir, paths) = home_with_shim("#!/bin/sh\nexec sleep 30\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let began = Instant::now();
        let err = compose
            .run(
                &["ps", "--all", "--format", "json"],
                Some(Duration::from_millis(300)),
            )
            .unwrap_err()
            .context("asking about x");
        assert!(began.elapsed() < Duration::from_secs(10), "it waited");
        assert!(is_daemon_hung(&err), "{err:#}");
        assert!(!is_daemon_down(&err), "{err:#}");
        assert!(format!("{err:#}").contains("docker compose ps"), "{err:#}");
    }

    #[test]
    fn a_docker_that_is_not_there_says_isolated_mode_needs_it() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("h"), ProjectRef::from_root(&root).unwrap());
        let compose = Compose::by_project(paths.home.join("bin").join("docker"), "pando-x-y");
        let err = format!("{:#}", compose.stop().unwrap_err());
        assert!(err.contains("needs Docker"), "{err}");
    }

    #[test]
    fn readiness_by_health_waits_for_healthy_and_not_merely_running() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\n\
             f=$(dirname \"$0\")/round\n\
             n=$(cat \"$f\" 2>/dev/null || echo 0)\n\
             echo $((n + 1)) > \"$f\"\n\
             if [ \"$n\" -lt 2 ]; then h=starting; else h=healthy; fi\n\
             echo \"{\\\"Service\\\":\\\"db\\\",\\\"State\\\":\\\"running\\\",\\\"Health\\\":\\\"$h\\\"}\"\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let wanted = vec![Wanted {
            service: "db".into(),
            port: 1,
            healthcheck: true,
        }];
        wait_ready(&compose, &wanted, Duration::from_secs(10), &|_| {}).unwrap();
    }

    #[test]
    fn a_service_that_exits_fails_the_start_at_once_rather_than_at_the_timeout() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\necho '{\"Service\":\"db\",\"State\":\"exited\",\"Health\":\"\"}'\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let wanted = vec![Wanted {
            service: "db".into(),
            port: 1,
            healthcheck: true,
        }];
        let started = Instant::now();
        let err = format!(
            "{:#}",
            wait_ready(&compose, &wanted, Duration::from_secs(30), &|_| {}).unwrap_err()
        );
        assert!(err.contains("\"db\""), "{err}");
        assert!(err.contains("exited"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait"
        );
    }

    // Without a healthcheck the answer is a connect, and a connect to a
    // container that died looks exactly like one to a container still
    // starting. Docker is asked on a slow beat so the difference is
    // noticed in a second rather than at the end of the timeout.
    #[test]
    fn a_service_with_no_healthcheck_that_exits_is_noticed_without_waiting() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\necho '{\"Service\":\"db\",\"State\":\"exited\",\"Health\":\"\"}'\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let wanted = vec![Wanted {
            service: "db".into(),
            port: 1,
            healthcheck: false,
        }];
        let started = Instant::now();
        let err = format!(
            "{:#}",
            wait_ready(&compose, &wanted, Duration::from_secs(30), &|_| {}).unwrap_err()
        );
        assert!(err.contains("exited before it was ready"), "{err}");
        assert!(err.contains("logs db"), "and where to read why: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it did not wait"
        );
    }

    #[test]
    fn a_service_that_never_becomes_ready_fails_with_its_name_and_the_timeout() {
        let (_dir, paths) = home_with_shim(
            "#!/bin/sh\necho '{\"Service\":\"db\",\"State\":\"running\",\"Health\":\"starting\"}'\n",
        );
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let wanted = vec![Wanted {
            service: "db".into(),
            port: 1,
            healthcheck: true,
        }];
        let err = format!(
            "{:#}",
            wait_ready(&compose, &wanted, Duration::from_millis(400), &|_| {}).unwrap_err()
        );
        assert!(err.contains("db"), "{err}");
        assert!(err.contains("did not become ready"), "{err}");
    }

    // Readiness by connect, against a real listener rather than a mock:
    // the one thing it must never do is bind the port itself.
    #[test]
    fn readiness_by_connect_waits_for_something_to_be_listening() {
        let (_dir, paths) = home_with_shim("#!/bin/sh\nexit 0\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let wanted = vec![Wanted {
            service: "cache".into(),
            port,
            healthcheck: false,
        }];
        wait_ready(&compose, &wanted, Duration::from_secs(5), &|_| {}).unwrap();
        // Held to the end on purpose. A port freed mid-test is a port
        // another test running in parallel can bind, and then "nothing is
        // listening" is not a thing this process can assert.
        drop(listener);
    }

    #[test]
    fn readiness_by_connect_gives_up_when_nothing_ever_answers() {
        let (_dir, paths) = home_with_shim("#!/bin/sh\nexit 0\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        // Port 1 is privileged and unbindable by a test, so a refused
        // connection here is a fact rather than a race.
        let wanted = vec![Wanted {
            service: "cache".into(),
            port: 1,
            healthcheck: false,
        }];
        let err = format!(
            "{:#}",
            wait_ready(&compose, &wanted, Duration::from_millis(400), &|_| {}).unwrap_err()
        );
        assert!(err.contains("cache"), "{err}");
        assert!(err.contains("did not become ready"), "{err}");
    }

    // ---- env rewriting ---------------------------------------------------

    fn worktree_with(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    fn map(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn ports(pairs: &[(&str, u16)]) -> std::collections::BTreeMap<String, u16> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn a_url_keeps_everything_but_its_port() {
        let dir = worktree_with(&[(
            ".env",
            "DATABASE_URL=postgres://acme:secret@localhost:5432/acme?sslmode=disable\n\
             REDIS_URL=redis://localhost:6379\n",
        )]);
        let env = app_env(
            dir.path(),
            &map(&[("DATABASE_URL", "postgres"), ("REDIS_URL", "redis")]),
            &ports(&[("postgres", 17_004), ("redis", 17_006)]),
        )
        .unwrap();
        assert_eq!(
            env["DATABASE_URL"],
            "postgres://acme:secret@localhost:17004/acme?sslmode=disable"
        );
        assert_eq!(env["REDIS_URL"], "redis://localhost:17006");
    }

    #[test]
    fn a_url_that_names_the_compose_service_as_its_host_is_pointed_at_localhost() {
        let dir = worktree_with(&[(
            ".env.example",
            "DATABASE_URL=postgres://app@postgres:5432/app\nOTHER_URL=redis://cache-a:6379\n",
        )]);
        let env = app_env(
            dir.path(),
            &map(&[("DATABASE_URL", "postgres"), ("OTHER_URL", "redis")]),
            &ports(&[("postgres", 17_004), ("redis", 17_006)]),
        )
        .unwrap();
        assert_eq!(env["DATABASE_URL"], "postgres://app@localhost:17004/app");
        assert_eq!(
            env["OTHER_URL"], "redis://cache-a:17006",
            "a host that is not the service's name is the developer's and is kept"
        );
    }

    #[test]
    fn a_url_with_no_port_gains_one() {
        let dir = worktree_with(&[(".env", "REDIS_URL=redis://localhost\n")]);
        let env = app_env(
            dir.path(),
            &map(&[("REDIS_URL", "redis")]),
            &ports(&[("redis", 17_006)]),
        )
        .unwrap();
        assert_eq!(env["REDIS_URL"], "redis://localhost:17006");
    }

    #[test]
    fn an_ipv6_host_keeps_its_brackets() {
        let dir = worktree_with(&[(".env", "DB=postgres://[::1]:5432/app\n")]);
        let env = app_env(
            dir.path(),
            &map(&[("DB", "postgres")]),
            &ports(&[("postgres", 17_004)]),
        )
        .unwrap();
        assert_eq!(env["DB"], "postgres://[::1]:17004/app");
    }

    #[test]
    fn a_bare_number_becomes_the_port() {
        let dir = worktree_with(&[(".env", "DB_HOST=localhost\nDB_PORT=5432\n")]);
        let env = app_env(
            dir.path(),
            &map(&[("DB_PORT", "db")]),
            &ports(&[("db", 17_004)]),
        )
        .unwrap();
        assert_eq!(env["DB_PORT"], "17004");
    }

    // The real credentials live in `.env`; the example is the fallback.
    #[test]
    fn the_worktrees_own_env_wins_over_the_example() {
        let dir = worktree_with(&[
            (
                ".env",
                "DATABASE_URL=postgres://real:pw@localhost:5432/real\n",
            ),
            (
                ".env.example",
                "DATABASE_URL=postgres://user:pass@localhost:5432/db\nEXTRA_URL=redis://localhost:6379\n",
            ),
        ]);
        let env = app_env(
            dir.path(),
            &map(&[("DATABASE_URL", "postgres"), ("EXTRA_URL", "redis")]),
            &ports(&[("postgres", 17_004), ("redis", 17_006)]),
        )
        .unwrap();
        assert_eq!(
            env["DATABASE_URL"],
            "postgres://real:pw@localhost:17004/real"
        );
        assert_eq!(
            env["EXTRA_URL"], "redis://localhost:17006",
            "a key only the example has still resolves"
        );
    }

    #[test]
    fn a_key_nothing_in_the_worktree_sets_is_an_error_naming_it() {
        let dir = worktree_with(&[(".env", "SOMETHING_ELSE=1\n")]);
        let err = format!(
            "{:#}",
            app_env(
                dir.path(),
                &map(&[("DATABASE_URL", "postgres")]),
                &ports(&[("postgres", 17_004)]),
            )
            .unwrap_err()
        );
        assert!(err.contains("DATABASE_URL"), "{err}");
        assert!(
            err.contains(".env.example"),
            "it says where to put it: {err}"
        );
    }

    #[test]
    fn a_value_that_is_neither_a_url_nor_a_port_is_an_error_naming_it() {
        let dir = worktree_with(&[(".env", "DB=localhost\n")]);
        let err = format!(
            "{:#}",
            app_env(
                dir.path(),
                &map(&[("DB", "postgres")]),
                &ports(&[("postgres", 17_004)]),
            )
            .unwrap_err()
        );
        assert!(err.contains("DB=\"localhost\""), "{err}");
        assert!(err.contains(".env"), "{err}");
    }

    #[test]
    fn env_files_are_read_the_way_a_shell_would_read_them() {
        let parsed = parse_env(
            "# a comment\n\nexport A=1\nB = \"two\"\nC='three'\nD=four=five\nbroken\n=nokey\n",
        );
        assert_eq!(parsed["A"], "1");
        assert_eq!(parsed["B"], "two");
        assert_eq!(parsed["C"], "three");
        assert_eq!(parsed["D"], "four=five");
        assert_eq!(parsed.len(), 4);
    }

    #[test]
    fn waiting_for_nothing_asks_docker_nothing() {
        // The shim would fail the test if it ran at all.
        let (_dir, paths) = home_with_shim("#!/bin/sh\nexit 3\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        wait_ready(&compose, &[], Duration::from_secs(1), &|_| {}).unwrap();
    }
}
