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
    loop {
        // One `ps` per round, not one per service: it is a process spawn,
        // and it answers for every container of the project at once.
        let statuses = if watched { compose.ps()? } else { Vec::new() };
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

    #[test]
    fn waiting_for_nothing_asks_docker_nothing() {
        // The shim would fail the test if it ran at all.
        let (_dir, paths) = home_with_shim("#!/bin/sh\nexit 3\n");
        let compose = Compose::by_project(docker_program(&paths), "pando-x-y");
        wait_ready(&compose, &[], Duration::from_secs(1), &|_| {}).unwrap();
    }
}
