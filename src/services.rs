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

    fn run(&self, rest: &[&str]) -> Result<String> {
        let args = self.args(rest);
        let mut command = Command::new(&self.program);
        command.args(&args).stdin(Stdio::null());
        if let Some(dir) = &self.dir {
            command.current_dir(dir);
        }
        let out = command.output().with_context(|| {
            format!(
                "run {} {} — isolated mode needs Docker; install it, or start without --isolated",
                self.program.display(),
                args.join(" ")
            )
        })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
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

    /// Brings the included services up in the background. Idempotent:
    /// compose leaves a container that is already running and matches its
    /// configuration exactly as it is.
    pub fn up(&self, services: &[String]) -> Result<()> {
        let mut rest = vec!["up", "-d"];
        rest.extend(services.iter().map(String::as_str));
        self.run(&rest)?;
        Ok(())
    }

    /// What compose says about every container of this project.
    pub fn ps(&self) -> Result<Vec<Status>> {
        let text = self.run(&["ps", "--all", "--format", "json"])?;
        parse_ps(&text)
    }

    /// Stops the containers and leaves the volumes. What `stop` does: the
    /// data survives, and the next start brings the same database back.
    pub fn stop(&self) -> Result<()> {
        self.run(&["stop"])?;
        Ok(())
    }

    /// Removes the containers, the network, and the named volumes. What
    /// `rm` does: the worktree is going, and its database goes with it.
    pub fn down_with_volumes(&self) -> Result<()> {
        self.run(&["down", "-v"])?;
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

/// The port an env key names in this directory's env files: the port of a
/// URL, or a bare number.
///
/// What shared mode probes. The services there are the ones the developer
/// already runs, on the ports the project's own files say, so pando reads
/// rather than assigns.
pub fn port_in_env(dir: &Path, key: &str) -> Option<u16> {
    let value = read_env_files(dir)
        .iter()
        .find_map(|(_, map)| map.get(key).cloned())?;
    port_of_value(&value)
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

fn is_ready(want: &Wanted, statuses: &[Status]) -> bool {
    if want.healthcheck {
        return statuses
            .iter()
            .any(|s| s.service == want.service && s.health == "healthy");
    }
    ports::something_is_listening(want.port)
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
