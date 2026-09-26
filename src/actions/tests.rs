use super::*;
use super::{hooks::*, lifecycle::*, questions::*, refresh::*, share::*, worktree::*};
use crate::config::{Config, ProcessConfig, ProvisionMode};
use crate::paths::PandoPaths;
use crate::ports;
use crate::process::{self as proc, SpawnOptions};
use crate::project::ProjectRef;
use crate::share_proxy;
use crate::state::{self, PendingShare, Phase, ProcessRecord, ShareRecord, WorktreeRecord};
use crate::testutil::git;
use crate::tunnel;
use crate::worktree;
use anyhow::{Result, bail};
use chrono::Utc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tempfile::{TempDir, tempdir};

/// The shared-mode `start`, which is what every test written before
/// isolation existed means. Shadowing the real one keeps those tests
/// reading as they did — `--isolated` is a separate question, and they
/// were never asking it.
fn start(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    super::start(paths, config, name, only, Mode::Remembered, progress)
}

fn restart(
    paths: &PandoPaths,
    config: &Config,
    name: &str,
    only: Option<&str>,
    progress: &dyn Fn(&str),
) -> Result<StartReport> {
    super::restart(paths, config, name, only, Mode::Remembered, progress)
}

/// `stop`, `stop_all` and `rm` as every test written before the sweep
/// narrated anything means them: with nobody listening. Shadowing them
/// keeps those tests reading as they did — the notices a sweep returns
/// are a separate question, asked by the tests that ask it, which call
/// `super::` directly.
fn stop(paths: &PandoPaths, name: &str, only: Option<&str>) -> Result<StopOutcome> {
    super::stop(paths, name, only, &noop)
}

fn stop_all(paths: &PandoPaths) -> Result<Vec<String>> {
    super::stop_all(paths, &noop)
}

fn stop_all_with(paths: &PandoPaths, stop: impl Fn(i32) -> Result<()>) -> Result<Vec<String>> {
    super::stop_all_with(paths, None, stop, &noop).map(|report| report.stopped)
}

fn rm(paths: &PandoPaths, name: &str, yes: bool, force: bool) -> Result<()> {
    super::rm(paths, name, yes, force, &noop)
}

/// Detection for a shared-mode start, which is what every test written
/// before isolation existed means.
fn resolve_process(
    paths: &PandoPaths,
    config: &Config,
    ask: Ask<'_>,
    progress: &dyn Fn(&str),
) -> Result<Config> {
    super::resolve_process(paths, config, Mode::Remembered, ask, progress)
}

// The two lists have to stay in step: a process named `install` writes
// the install hook's log file, and `reset_log` truncates it on every
// start.
#[test]
fn the_install_hooks_log_name_is_one_no_process_may_take() {
    assert!(
        crate::paths::RESERVED_LOG_SOURCES.contains(&INSTALL_HOOK),
        "{INSTALL_HOOK} must be reserved, or a process can take its log"
    );
}

struct Fx {
    _dir: TempDir,
    root: PathBuf,
    paths: PandoPaths,
    config: Config,
}

impl Fx {
    fn worktrees_dir(&self) -> PathBuf {
        self.config.worktrees_dir(&self.paths)
    }

    fn names(&self) -> Vec<String> {
        worktree::discover(&self.paths.project)
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect()
    }

    fn state(&self) -> state::State {
        state::load(&self.paths.state_file()).unwrap()
    }
}

fn noop(_: &str) {}

// ---- start, stop, restart helpers ------------------------------------

use crate::config::{PortsSpec, ReadySpec};
use crate::testutil::{Detached, python_listener, python3_available, wait_until};
use std::time::Duration;

/// A dev process with one `web` role exposed as `PORT`, the shape almost
/// every JavaScript project has.
fn dev(cmd: &str) -> ProcessConfig {
    ProcessConfig {
        cmd: cmd.to_string(),
        ports: Some(PortsSpec::Map(BTreeMap::from([(
            "PORT".to_string(),
            "web".to_string(),
        )]))),
        ..Default::default()
    }
}

fn with_dev(fx: &mut Fx, process: ProcessConfig) {
    fx.config.processes.insert("dev".to_string(), process);
}

/// Stops whatever a test started even when an assertion panics first. No
/// test in this crate may leave a process behind.
fn guard(report: &StartReport) -> Vec<Detached> {
    report
        .started
        .iter()
        .map(|p| Detached {
            pid: p.record.pid,
            pgid: p.record.pgid,
        })
        .collect()
}

fn log_of(fx: &Fx, name: &str) -> String {
    std::fs::read_to_string(fx.paths.log_file(name, "dev")).unwrap_or_default()
}

/// Creates a worktree and returns its directory name.
fn worktree_named(fx: &Fx, branch: &str) -> String {
    new(&fx.paths, &fx.config, branch, None, &noop).unwrap()
}

// ---- start -----------------------------------------------------------

#[test]
fn start_spawns_the_dev_process_and_records_it() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let started = &outcome.started[0];
    assert!(!outcome.started_nothing());
    assert_eq!(outcome.worktree, name);
    assert_eq!(started.process, "dev");
    assert!(outcome.already_running.is_empty());
    assert_eq!(outcome.ports.len(), 1, "one role, one port");
    let port = outcome.ports["web"];
    assert_eq!(
        outcome.url.as_deref(),
        Some(&*format!("http://localhost:{port}"))
    );
    assert!(!outcome.reassigned);

    let record = &fx.state().worktrees[&name].processes["dev"];
    assert_eq!(record.ready_port, Some(port));
    assert!(matches!(record.phase, Phase::Starting { .. }));
    assert!(crate::process::is_alive(record.pid));
    assert_eq!(
        record.log_path,
        fx.paths.log_file(&name, "dev"),
        "the log lives under pando's home, one file per source"
    );
    assert_eq!(
        fx.state().worktrees[&name].ports["web"],
        port,
        "the port is recorded, so a stopped worktree keeps it"
    );
}

#[test]
fn start_runs_in_the_worktree_with_the_ports_and_pando_variables_in_the_environment() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("pwd && env | sort && sleep 30"));
    fx.config.runtime.prelude = Some("echo prelude-ran".to_string());
    let name = worktree_named(&fx, "feat/one");

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];
    let worktree = fx.worktrees_dir().join(&name).canonicalize().unwrap();

    assert!(
        wait_until(Duration::from_secs(10), || log_of(&fx, &name)
            .contains("PANDO_PROJECT")),
        "the environment never reached the log: {:?}",
        log_of(&fx, &name)
    );
    let log = log_of(&fx, &name);
    assert!(
        log.contains("prelude-ran"),
        "the prelude did not run: {log}"
    );
    assert!(
        log.contains(&worktree.display().to_string()),
        "the process must run in its worktree: {log}"
    );
    for expected in [
        format!("PORT={port}"),
        format!("PANDO_NAME={name}"),
        "PANDO_BRANCH=feat/one".to_string(),
        format!("PANDO_WORKTREE={}", worktree.display()),
        format!("PANDO_ROOT={}", fx.root.display()),
        format!("PANDO_PROJECT={}", fx.paths.project_id()),
    ] {
        assert!(log.contains(&expected), "missing {expected} in:\n{log}");
    }
}

#[test]
fn start_renders_the_port_into_the_command_for_a_positional_framework() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "echo serving on 127.0.0.1:{port:web} && sleep 30".to_string(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];
    assert!(
        wait_until(Duration::from_secs(10), || log_of(&fx, &name)
            .contains(&format!("127.0.0.1:{port}"))),
        "{:?}",
        log_of(&fx, &name)
    );
}

#[test]
fn start_runs_in_the_configured_subdirectory() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "pwd && sleep 30".to_string(),
            cwd: Some("apps/web".to_string()),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let worktree = fx.worktrees_dir().join(&name);
    std::fs::create_dir_all(worktree.join("apps/web")).unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        wait_until(Duration::from_secs(10), || log_of(&fx, &name)
            .contains("apps/web")),
        "{:?}",
        log_of(&fx, &name)
    );
}

#[test]
fn a_cwd_that_does_not_exist_is_refused_by_name() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30".to_string(),
            cwd: Some("apps/nope".to_string()),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("apps/nope"), "{msg}");
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "a refused start records nothing"
    );
}

// `config::validate` refuses a literal `..` or an absolute path, but a
// symlink resolves only when the process is about to be started, and a
// process running outside its worktree writes into a repository.
#[test]
fn a_cwd_that_is_a_symlink_out_of_the_worktree_is_refused() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30".to_string(),
            cwd: Some("escape".to_string()),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let worktree = fx.worktrees_dir().join(&name);
    let outside = fx.root.parent().expect("a parent").join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, worktree.join("escape")).unwrap();

    let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("outside the worktree"), "{msg}");
    assert!(msg.contains("\"dev\""), "the process is named: {msg}");
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "a refused start records nothing"
    );
}

#[test]
fn a_project_with_no_processes_says_what_to_add() {
    let fx = fixture();
    let name = worktree_named(&fx, "feat/one");
    let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "no processes configured; add [dev] to pando.toml"
    );
}

#[test]
fn starting_a_worktree_that_does_not_exist_says_so() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let err = start(&fx.paths, &fx.config, "nope", None, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("no worktree named"));
    let err = start(&fx.paths, &fx.config, "acme-shop", None, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("main checkout"));
}

#[test]
fn a_second_start_while_running_reports_the_process_that_is_already_up() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");

    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    assert!(
        second.started_nothing(),
        "a running process is reported, not started twice"
    );
    assert_eq!(
        second
            .already_running
            .iter()
            .map(|p| p.process.as_str())
            .collect::<Vec<_>>(),
        vec!["dev"],
        "and it says which process that was"
    );
    assert_eq!(
        second.already_running[0].record.pid,
        first.started[0].record.pid
    );
    assert_eq!(second.ports, first.ports);
}

// A failure is sticky for display; starting is the user acting on it.
#[test]
fn start_clears_a_failed_record_and_starts_fresh() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let first_pid = first.started[0].record.pid;
    let first_ports = first.ports.clone();
    drop(guard(&first));

    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(first_pid)
    }));
    // Mark it Failed the way a read path would.
    let mut store = fx.state();
    store
        .worktrees
        .get_mut(&name)
        .unwrap()
        .processes
        .get_mut("dev")
        .unwrap()
        .phase = Phase::Failed {
        at: Utc::now(),
        reason: "process exited".into(),
    };
    state::save(&fx.paths.state_file(), &store).unwrap();

    let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&second);
    assert!(!second.started_nothing());
    assert_ne!(second.started[0].record.pid, first_pid);
    assert_eq!(
        second.ports, first_ports,
        "ports are stable across a failure"
    );
    assert!(matches!(
        fx.state().worktrees[&name].processes["dev"].phase,
        Phase::Starting { .. }
    ));
}

#[test]
fn a_process_with_no_ports_gets_no_ready_port() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30".to_string(),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(outcome.ports.is_empty());
    assert_eq!(outcome.started[0].record.ready_port, None);
    assert_eq!(outcome.url, None);

    // Which is what makes it Running as soon as it is alive: nothing
    // is watching a port, so no probe can have an opinion.
    let mut store = fx.state();
    assert!(state::advance_phases(
        &mut store,
        crate::process::is_alive,
        crate::process::group_alive,
        |_, _| false
    ));
    assert!(matches!(
        store.worktrees[&name].processes["dev"].phase,
        Phase::Running { .. }
    ));
}

/// A dev command that backgrounds its server and returns.
///
/// `swift build && ./app &`, `npm run dev &`, any recipe line ending
/// in `&`: the `bash -lc` pando spawned is the group leader, and it
/// exits the moment it has started the thing it was asked to start.
/// Asking after the leader alone calls that worktree Failed while the
/// application is serving — confidently wrong in the opposite
/// direction from the truth, which is the worst shape a report can
/// have. Found the first time pando was run on a repository it had
/// not generated, whose Makefile did exactly this.
///
/// Two failures, not one: the phase flips to Failed, and `reconcile`
/// then drops the record outright, so the worktree stops listing the
/// process it is still running. Both read liveness of the group now,
/// which is what `stop` has always asked.
#[test]
fn a_process_that_backgrounds_its_server_is_not_called_dead() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            // The leader exits at once; the group keeps a child.
            cmd: "sleep 30 &".to_string(),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    // The leader does not go at once: measured, a `bash -lc` that
    // backgrounds something lives about half a second before it
    // exits. A shorter wait than this passes for the wrong reason —
    // it reads the window where the leader is still alive and never
    // exercises the group probe at all.
    let leader = outcome.started[0].record.pid;
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::is_alive(leader) {
        assert!(
            Instant::now() < deadline,
            "the leader never exited, so this test would prove nothing"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    for read in 0..3 {
        let state = refresh(&fx.paths).state;
        let record = state.worktrees[&name]
            .processes
            .get("dev")
            .unwrap_or_else(|| {
                panic!("read {read}: reconcile dropped a process whose group is alive")
            });
        assert!(
            matches!(record.phase, Phase::Running { .. }),
            "read {read}: the leader exited but its group is serving: {:?}",
            record.phase
        );
    }
    assert_eq!(
        state::aggregate_phase(&fx.state().worktrees[&name])
            .expect("a phase")
            .word(),
        "running",
        "and the worktree a list shows reads as running"
    );
}

/// The same shape, from the side that acts on it. The read path kept
/// that process Running, but the orphan sweep every mutation runs still
/// asked the leader alone: a `stop` of a sibling SIGKILLed the app, and
/// the record then read Failed as "exited", blaming the developer's
/// command for a kill pando did. And a second `start` of the worktree
/// called it not running, stopped it, and ran it again.
#[test]
fn a_process_that_backgrounds_itself_survives_a_mutation_elsewhere() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30 &".to_string(),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let sibling = worktree_named(&fx, "feat/two");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let started = outcome.started[0].record.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::process::is_alive(started.pid) {
        assert!(
            Instant::now() < deadline,
            "the leader never exited, so this test would prove nothing"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Every mutation sweeps the whole project, not only its own worktree.
    stop(&fx.paths, &sibling, None).unwrap();
    assert!(
        crate::process::group_alive(started.pgid),
        "a stop of another worktree killed this one's app"
    );
    let record = fx.state().worktrees[&name].processes["dev"].clone();
    assert!(
        matches!(record.phase, Phase::Running { .. }),
        "{:?}",
        record.phase
    );
    assert!(!record.swept, "nothing was signalled: {record:?}");

    let again = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _again = guard(&again);
    assert!(again.started_nothing(), "it was started twice: {again:?}");
    assert_eq!(again.already_running[0].record.pgid, started.pgid);
    assert!(crate::process::group_alive(started.pgid));
}

/// A desktop application, a worker, a watcher: something that owns no
/// port and never will. `ports = []` is a written answer, and it has
/// to be a workable one — a readiness rule that waits for a socket
/// nobody ever opens would leave every such project hanging until the
/// start timeout failed it.
///
/// Through `refresh`, so the liveness check and the port scan are the
/// real ones rather than closures saying what this test would like to
/// hear.
// A worktree whose own `.env` another tool wrote for an isolated run
// points at a database nobody runs. Started shared, the app is told the
// main checkout's port in its environment, which a dotenv file that does
// not override leaves alone.
#[test]
fn a_shared_start_tells_the_app_the_main_checkouts_service_ports() {
    let mut fx = fixture();
    std::fs::write(fx.root.join(".env"), "DATABASE_PORT=3306\n").unwrap();
    let services: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n",
    )
    .unwrap();
    fx.config.services = services.services;
    let seen = fx.root.parent().unwrap().join("seen-env");
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: format!("env > {}; sleep 30", seen.display()),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    std::fs::write(
        fx.worktrees_dir().join(&name).join(".env"),
        "DATABASE_PORT=52434\n",
    )
    .unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        wait_until(Duration::from_secs(5), || std::fs::read_to_string(&seen)
            .is_ok_and(|env| env.contains("DATABASE_PORT="))),
        "the process never wrote its environment"
    );
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(
        env.lines().any(|line| line == "DATABASE_PORT=3306"),
        "{env}"
    );
}

// A URL the main checkout's `.env` builds from its own keys reaches the
// app as the app's loader would have read it. A loader that does not
// override keeps what pando set, so a literal `${…}` there was the login.
#[test]
fn a_shared_start_hands_the_app_its_env_files_references_expanded() {
    let mut fx = fixture();
    std::fs::write(
        fx.root.join(".env"),
        "PANDO_TEST_DB_USER=app\nPANDO_TEST_DB_NAME=shop\n\
         DATABASE_URL=mysql://${PANDO_TEST_DB_USER}@localhost:3306/${PANDO_TEST_DB_NAME}\n",
    )
    .unwrap();
    let services: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_URL = \"mariadb\" }\n",
    )
    .unwrap();
    fx.config.services = services.services;
    let env = super::services::shared_service_env(&fx.paths, &fx.config);
    assert_eq!(
        env.get("DATABASE_URL").map(String::as_str),
        Some("mysql://app@localhost:3306/shop")
    );
}

// A reference nothing pando reads sets may be one the app's own loader
// knows, from a file pando does not read. Handed on as written, ahead of
// that loader, it was the app's login; a namespaced start rewrote the
// database around it.
#[test]
fn a_key_pando_cannot_expand_is_left_to_the_apps_loader_and_its_service_stays_shared() {
    let mut fx = fixture();
    std::fs::write(
        fx.root.join(".env"),
        "DATABASE_PORT=3306\n\
         DATABASE_URL=mysql://${PANDO_TEST_UNSET_USER}@localhost:3306/shop\n",
    )
    .unwrap();
    let services: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\n\
         env = { DATABASE_URL = \"mariadb\", DATABASE_PORT = \"mariadb\" }\n",
    )
    .unwrap();
    fx.config.services = services.services;
    let env = super::services::shared_service_env(&fx.paths, &fx.config);
    assert_eq!(env.get("DATABASE_URL"), None);
    assert_eq!(env.get("DATABASE_PORT").map(String::as_str), Some("3306"));

    let plan = super::namespaced::plan(&fx.paths, &fx.config);
    assert!(plan.targets.is_empty(), "{:?}", plan.targets);
    assert_eq!(
        plan.shared_lines(),
        vec![
            "mariadb: shared — DATABASE_URL in .env holds ${PANDO_TEST_UNSET_USER}, which \
             neither pando's environment nor an earlier line of .env sets"
                .to_string()
        ]
    );
    assert_eq!(plan.shared_data, vec!["mariadb".to_string()]);
}

// A key a recipe supplied is dropped when the project never wrote it. One
// the project wrote and pando cannot expand is not: dropped, the app's own
// loader read the main checkout's port from it.
#[test]
fn an_isolated_start_refuses_a_service_key_it_cannot_expand_whoever_named_it() {
    let mut fx = fixture();
    let services: Config =
        toml::from_str("[[services]]\nkind = \"native\"\nname = \"postgres\"\n").unwrap();
    fx.config.services = services.services;
    let ports = BTreeMap::from([("postgres".to_string(), 17_004)]);
    std::fs::write(fx.root.join(".env"), "OTHER=1\n").unwrap();
    assert_eq!(
        super::services::resolve_service_env(&fx.paths, &fx.config, &fx.root, &ports).unwrap(),
        BTreeMap::new()
    );
    std::fs::write(
        fx.root.join(".env"),
        "DATABASE_URL=postgres://${PANDO_TEST_UNSET_USER}@localhost:5432/shop\n",
    )
    .unwrap();
    let e =
        super::services::resolve_service_env(&fx.paths, &fx.config, &fx.root, &ports).unwrap_err();
    assert!(
        e.downcast_ref::<crate::services::Unresolved>().is_some(),
        "{e:#}"
    );
    assert!(
        format!("{e:#}").contains("DATABASE_URL in .env holds ${PANDO_TEST_UNSET_USER}"),
        "{e:#}"
    );
}

#[test]
fn a_process_that_owns_no_ports_starts_and_reaches_running() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30".to_string(),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");

    let began = Instant::now();
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "start never waits on a readiness that cannot come: {:?}",
        began.elapsed()
    );
    assert!(outcome.ports.is_empty(), "no roles, so no ports");
    assert_eq!(outcome.started[0].record.ready_port, None);
    assert_eq!(outcome.url, None, "and nothing to point a browser at");

    let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
        .phase
        .clone();
    assert!(
        matches!(phase, Phase::Running { .. }),
        "alive is the whole of ready for a process with no port: {phase:?}"
    );

    // And it stays there. Reaching Running is what takes it out of the
    // reach of the start timeout, so a second read is the proof that
    // nothing pulls it back.
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(100));
        let again = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
            .phase
            .clone();
        assert!(matches!(again, Phase::Running { .. }), "{again:?}");
    }
    assert_eq!(
        state::aggregate_phase(&fx.state().worktrees[&name])
            .expect("a phase")
            .word(),
        "running",
        "and the worktree reads as running, which is what a list shows"
    );
}

#[test]
fn a_ready_role_the_process_does_not_own_is_refused() {
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: "sleep 30".to_string(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ready: Some(ReadySpec {
                role: Some("api".to_string()),
                timeout_s: None,
            }),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("api") && msg.contains("web"), "{msg}");
}

#[test]
fn the_readiness_port_is_the_one_a_listener_binds() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: python_listener_template(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];

    assert!(
        wait_until(Duration::from_secs(20), || !crate::ports::is_port_free(
            port
        )),
        "the listener never bound {port}: {:?}",
        log_of(&fx, &name)
    );
    let mut store = fx.state();
    // Through the readiness probe the read path really uses: the
    // group's own listening sockets, never a bind of the port.
    let scans = scan_groups(&store);
    assert!(state::advance_phases(
        &mut store,
        crate::process::is_alive,
        crate::process::group_alive,
        |pgid, port| port_is_bound(&scans, pgid, port)
    ));
    assert!(
        matches!(
            store.worktrees[&name].processes["dev"].phase,
            Phase::Running { .. }
        ),
        "a bound ready port is what running means"
    );
}

/// The listener, with its port coming from a template rather than the
/// environment — the positional shape Django and friends use.
fn python_listener_template() -> String {
    python_listener(0).replace("',0)", "',{port:web})")
}

fn python_listener_v6_template() -> String {
    crate::testutil::python_listener_v6(0).replace("',0)", "',{port:web})")
}

// A server on `[::1]` leaves both IPv4 addresses bindable, so a probe
// that decides readiness by binding says "not up" forever — and the
// observed-port scan never ran for a process that had not reached
// Running, so the two mechanisms deadlocked each other.
#[test]
fn a_dev_server_on_ipv6_loopback_alone_still_becomes_running() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    if !crate::testutil::ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback on this machine");
        return;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: python_listener_v6_template(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ready: Some(ReadySpec {
                role: None,
                timeout_s: Some(20),
            }),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];

    assert!(
        wait_until(Duration::from_secs(20), || matches!(
            refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
            Phase::Running { .. }
        )),
        "a server that is serving must not read as failed: {:?}",
        log_of(&fx, &name)
    );
    assert!(
        refresh(&fx.paths).state.worktrees[&name]
            .observed_ports
            .contains(&port),
        "and the port it really bound is recorded"
    );
}

// Readiness used to be answered by binding the port: "free" meant not
// up yet. That says nothing about *which* process is listening, so an
// unrelated squatter on the port made a dev server that had not even
// opened a socket read as running.
#[test]
fn a_port_something_else_holds_does_not_make_this_process_ready() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 300"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];

    // Taking the port is this test's setup, not what it is about: on a
    // busy run another test's listener can hold it for a moment, and a
    // failure there says nothing about readiness.
    let squatter = bind_when_free(port);
    let refreshed = refresh(&fx.paths);
    assert!(
        matches!(
            refreshed.state.worktrees[&name].processes["dev"].phase,
            Phase::Starting { .. }
        ),
        "readiness is about this group's own sockets, not about the port"
    );
    drop(squatter);
}

/// Takes `port`, waiting for whatever else on this machine has it.
fn bind_when_free(port: u16) -> std::net::TcpListener {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return listener,
            Err(e) if std::time::Instant::now() >= deadline => {
                panic!("could not take port {port} to squat on: {e}")
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// A "dev server" that tries to bind the IPv4 wildcard over and over and
/// counts how often it was refused — the class Django's
/// `runserver 0.0.0.0:P`, `vite --host 0.0.0.0` and Rails' `-b 0.0.0.0`
/// all belong to. A probe holding `0.0.0.0:P` locks every one of them
/// out, `SO_REUSEADDR` or not.
fn wildcard_bind_loop() -> String {
    "python3 -u -c \"
import os,socket,time
p=int(os.environ['PORT'])
f=0
for i in range(1500):
    s=socket.socket()
    s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
    try:
        s.bind(('0.0.0.0',p))
        s.listen(5)
    except OSError:
        f+=1
    s.close()
    time.sleep(0.001)
print('bind failures',f)
time.sleep(300)
\""
    .to_string()
}

// Deciding "is it up yet?" by *taking* the port meant nothing else
// could take it for the length of every probe — including the server
// pando was waiting for, which then died with EADDRINUSE and a
// classifier pointing at the wrong culprit.
#[test]
fn polling_readiness_never_refuses_the_server_its_own_port() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: wildcard_bind_loop(),
            ports: Some(PortsSpec::Map(BTreeMap::from([(
                "PORT".to_string(),
                "web".to_string(),
            )]))),
            ready: Some(ReadySpec {
                role: None,
                timeout_s: Some(120),
            }),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    // Polled the whole time the server is coming up, which is what
    // `ls`, `status` and the TUI's tick each do.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline && !log_of(&fx, &name).contains("bind failures") {
        refresh(&fx.paths);
    }
    let log = log_of(&fx, &name);
    assert!(
        log.contains("bind failures 0"),
        "the server was refused its own port while pando was checking on it: {log:?}"
    );
}

// ---- several processes ------------------------------------------------

/// The workspace shape: a web process and an api process, each in its
/// own directory, with the web one told the api's port through a
/// template. Fixture 5, in miniature.
fn with_web_and_api(fx: &mut Fx) {
    fx.config.processes.insert(
        "web".to_string(),
        ProcessConfig {
            cmd: "pwd && env && sleep 30".to_string(),
            cwd: Some("apps/web".to_string()),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            env: BTreeMap::from([(
                "VITE_API_URL".to_string(),
                "http://localhost:{port:api}".to_string(),
            )]),
            ..Default::default()
        },
    );
    fx.config.processes.insert(
        "api".to_string(),
        ProcessConfig {
            cmd: "pwd && env && sleep 30".to_string(),
            cwd: Some("apps/api".to_string()),
            ports: Some(PortsSpec::Map(BTreeMap::from([(
                "PORT".to_string(),
                "api".to_string(),
            )]))),
            ..Default::default()
        },
    );
}

/// The worktree both processes need, with the directories they run in.
fn workspace_worktree(fx: &Fx, branch: &str) -> String {
    let name = worktree_named(fx, branch);
    let worktree = fx.worktrees_dir().join(&name);
    std::fs::create_dir_all(worktree.join("apps/web")).unwrap();
    std::fs::create_dir_all(worktree.join("apps/api")).unwrap();
    name
}

fn log_source(fx: &Fx, name: &str, source: &str) -> String {
    std::fs::read_to_string(fx.paths.log_file(name, source)).unwrap_or_default()
}

fn names_of(list: &[StartedProcess]) -> Vec<&str> {
    list.iter().map(|p| p.process.as_str()).collect()
}

fn live_processes(fx: &Fx, name: &str) -> Vec<String> {
    fx.state().worktrees[name]
        .processes
        .keys()
        .cloned()
        .collect()
}

#[test]
fn start_spawns_every_configured_process_with_its_own_log_cwd_and_port() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");

    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    assert_eq!(
        names_of(&report.started),
        vec!["api", "web"],
        "config order, spawned one after the other"
    );
    assert_eq!(report.ports.len(), 2, "a port for every role");
    assert_eq!(
        report.url,
        Some(format!("http://localhost:{}", report.ports["web"])),
        "one URL for the worktree, and it is the web role's"
    );

    let record = fx.state().worktrees[&name].clone();
    assert_eq!(
        record.processes.keys().cloned().collect::<Vec<_>>(),
        vec!["api", "web"],
        "the worktree record holds both"
    );
    assert_eq!(
        record.processes["web"].ready_port,
        Some(report.ports["web"]),
        "each process waits on its own role's port"
    );
    assert_eq!(
        record.processes["api"].ready_port,
        Some(report.ports["api"])
    );
    assert_eq!(
        record.processes["web"].log_path,
        fx.paths.log_file(&name, "web")
    );
    assert_eq!(
        record.processes["api"].log_path,
        fx.paths.log_file(&name, "api")
    );
    assert_ne!(
        record.processes["web"].pgid, record.processes["api"].pgid,
        "each gets its own process group, so one can be stopped alone"
    );

    // Each in its own directory, and the web one carrying the api's
    // real port: `{port:api}` is why the template language has roles.
    let api_port = report.ports["api"];
    // Patient: `bash -lc` reads a login profile, and a full parallel
    // test run has a dozen of them starting at once.
    assert!(
        wait_until(Duration::from_secs(30), || {
            log_source(&fx, &name, "web")
                .contains(&format!("VITE_API_URL=http://localhost:{api_port}"))
        }),
        "the web log should carry the api's port: {:?}",
        log_source(&fx, &name, "web")
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            log_source(&fx, &name, "web").contains("apps/web")
                && log_source(&fx, &name, "api").contains("apps/api")
        }),
        "each process runs in its own cwd: {:?} / {:?}",
        log_source(&fx, &name, "web"),
        log_source(&fx, &name, "api")
    );
    assert!(
        wait_until(Duration::from_secs(30), || {
            log_source(&fx, &name, "api").contains(&format!("PORT={api_port}"))
        }),
        "the map form of ports reaches the process it belongs to: {:?}",
        log_source(&fx, &name, "api")
    );
}

#[test]
fn only_starts_the_process_it_names_and_leaves_the_other_alone() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");

    let first = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
    let _ga = guard(&first);
    assert_eq!(names_of(&first.started), vec!["api"]);
    assert_eq!(live_processes(&fx, &name), vec!["api"]);
    assert_eq!(
        first.ports.len(),
        2,
        "ports are reserved for every role of the worktree, not only the one started"
    );

    let api_pid = fx.state().worktrees[&name].processes["api"].pid;
    let second = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
    let _gb = guard(&second);
    assert_eq!(names_of(&second.started), vec!["web"]);
    assert_eq!(
        second.ports, first.ports,
        "a second --only start never moves the ports the first one handed out"
    );
    assert!(!second.reassigned);
    assert_eq!(live_processes(&fx, &name), vec!["api", "web"]);
    assert_eq!(
        fx.state().worktrees[&name].processes["api"].pid,
        api_pid,
        "the process it did not name was not touched"
    );
}

// The port a worktree's *own* listener holds is not a port somebody
// took. Read that way, starting the second of a pair would move both
// ports — while the first process is still serving on the old one.
#[test]
fn starting_one_process_beside_a_listening_one_keeps_every_port() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let api = fx.config.processes.get_mut("api").expect("the api process");
    api.cmd = crate::testutil::python_listener_for_role("api");
    api.cwd = None;
    let web = fx.config.processes.get_mut("web").expect("the web process");
    web.cwd = None;
    let name = worktree_named(&fx, "feat/one");

    let first = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
    let _ga = guard(&first);
    let api_port = first.ports["api"];
    assert!(
        wait_until(Duration::from_secs(30), || ports::something_is_listening(
            api_port
        )),
        "the api never came up on {api_port}: {:?}",
        log_source(&fx, &name, "api")
    );

    let second = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
    let _gb = guard(&second);
    assert_eq!(
        second.ports, first.ports,
        "the worktree's own listener must not look like a squatter"
    );
    assert!(!second.reassigned);
    assert_eq!(
        fx.state().worktrees[&name].ports["api"],
        api_port,
        "and the api is still recorded on the port it is really serving"
    );
}

#[test]
fn a_second_start_says_which_processes_were_already_running() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");

    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    let said = std::sync::Mutex::new(Vec::<String>::new());
    let report = start(&fx.paths, &fx.config, &name, None, &|m: &str| {
        said.lock().expect("the progress lock").push(m.to_string())
    })
    .unwrap();
    let said = said.into_inner().expect("the progress lock");
    assert!(report.started_nothing());
    assert_eq!(names_of(&report.already_running), vec!["api", "web"]);
    assert!(
        said.contains(&"api is already running".to_string())
            && said.contains(&"web is already running".to_string()),
        "and it says so on the way: {said:?}"
    );
    assert_eq!(
        fx.state().worktrees[&name].processes["web"].pid,
        first.started[1].record.pid,
        "nothing was started twice"
    );
}

#[test]
fn an_only_that_names_nothing_says_what_there_is_and_starts_nothing() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");

    let err = start(&fx.paths, &fx.config, &name, Some("worker"), &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("worker"), "{msg}");
    assert!(msg.contains("api, web"), "the names there are: {msg}");
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "a refused --only starts nothing"
    );
}

// Planned in full before anything is spawned: the first process must
// not be left running behind a start that failed on the second.
#[test]
fn a_process_that_cannot_start_leaves_none_of_the_others_running() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    fx.config.processes.get_mut("web").unwrap().cwd = Some("apps/nope".to_string());
    let name = workspace_worktree(&fx, "feat/one");

    let err = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("apps/nope"));
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "the api process must not have been spawned either"
    );
}

#[test]
fn stop_signals_every_process_group_and_keeps_the_ports() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    let groups: Vec<i32> = report.started.iter().map(|p| p.record.pgid).collect();

    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::Stopped(vec!["api".to_string(), "web".to_string()])
    );
    for pgid in groups {
        assert!(
            !crate::process::group_alive(pgid),
            "group {pgid} survived the stop"
        );
    }
    let record = fx.state().worktrees[&name].clone();
    assert!(record.processes.is_empty());
    assert_eq!(
        record.ports, report.ports,
        "a stopped worktree keeps its ports"
    );
}

#[test]
fn stop_only_signals_one_group_and_leaves_the_other_running() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    let web = report.started.iter().find(|p| p.process == "web").unwrap();
    let api = report.started.iter().find(|p| p.process == "api").unwrap();

    assert_eq!(
        stop(&fx.paths, &name, Some("web")).unwrap(),
        StopOutcome::Stopped(vec!["web".to_string()])
    );
    assert!(!crate::process::group_alive(web.record.pgid));
    assert!(
        crate::process::group_alive(api.record.pgid),
        "the process it did not name keeps serving"
    );
    assert_eq!(live_processes(&fx, &name), vec!["api"]);
}

#[test]
fn stopping_a_process_a_worktree_is_not_running_names_the_ones_it_is() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
    let _guard = guard(&report);

    let err = stop(&fx.paths, &name, Some("web")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("web"), "{msg}");
    assert!(msg.contains("it is running: api"), "{msg}");
    assert_eq!(live_processes(&fx, &name), vec!["api"], "and stops nothing");
}

#[test]
fn restart_only_replaces_one_process_and_keeps_every_port() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    let web_pid = fx.state().worktrees[&name].processes["web"].pid;
    let api_pid = fx.state().worktrees[&name].processes["api"].pid;

    let report = restart(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
    let _g2 = guard(&report);
    assert_eq!(names_of(&report.started), vec!["api"]);
    assert_eq!(
        report.ports, first.ports,
        "a restart keeps the URL, and --only keeps the other process's too"
    );
    let after = fx.state().worktrees[&name].processes.clone();
    assert_ne!(after["api"].pid, api_pid, "the api is a new process");
    assert_eq!(after["web"].pid, web_pid, "the web process never stopped");
    assert!(crate::process::is_alive(web_pid));
}

// Phase 2b review, finding 5. `restart` was `stop` then `start`, and a
// `--only` stop of something that is not running is an error — so the
// one command a developer reaches for to bring a stopped process back
// refused to do it, but only when a *sibling* was still up.
#[test]
fn restart_only_brings_back_a_process_that_is_not_running() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    stop(&fx.paths, &name, Some("api")).unwrap();
    let web_pid = fx.state().worktrees[&name].processes["web"].pid;
    assert!(
        !fx.state().worktrees[&name].processes.contains_key("api"),
        "the api is down and the web process is still serving"
    );

    let report = restart(&fx.paths, &fx.config, &name, Some("api"), &noop).unwrap();
    let _g2 = guard(&report);
    assert_eq!(names_of(&report.started), vec!["api"]);
    assert_eq!(
        report.ports, first.ports,
        "and on the ports the worktree already had"
    );
    assert_eq!(
        fx.state().worktrees[&name].processes["web"].pid,
        web_pid,
        "the process it did not name was never touched"
    );
}

/// Shares a started worktree through a stand-in tunnel that really runs,
/// so a sweep reads it as alive and a stop has a group to signal.
fn share_through_a_live_tunnel(fx: &Fx, name: &str) -> Detached {
    let tunnel = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file(name, "tunnel"),
    );
    let mut store = fx.state();
    store.worktrees.get_mut(name).expect("started").share = Some(ShareRecord {
        tunnel_pid: tunnel.pid,
        tunnel_pgid: tunnel.pgid,
        ..share_record_of(tunnel.pid, None)
    });
    state::save(&fx.paths.state_file(), &store).unwrap();
    tunnel
}

// A `--only` stop of the process a share points at takes the share down,
// and `restart --only` was that stop and then a start. The URL a
// developer had handed out closed for a process that came back on the
// same port a moment later — and a stop that kept it would not have
// helped, because the start's own sweep found the share with nothing
// behind it and closed it anyway.
#[test]
fn restart_only_of_the_process_a_share_points_at_keeps_the_share() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    let tunnel = share_through_a_live_tunnel(&fx, &name);
    let web_pid = fx.state().worktrees[&name].processes["web"].pid;

    let report = restart(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
    let _g2 = guard(&report);
    assert_eq!(names_of(&report.started), vec!["web"]);
    assert_eq!(report.ports, first.ports, "web comes back on its own port");
    assert!(
        !crate::process::is_alive(web_pid),
        "the old web process was replaced"
    );
    let share = fx.state().worktrees[&name].share.clone();
    assert_eq!(
        share.map(|s| s.tunnel_pid),
        Some(tunnel.pid),
        "the public URL is the one the developer handed out"
    );
    assert!(crate::process::is_alive(tunnel.pid), "and its tunnel is up");
}

// A whole restart stops everything first, its share included, and did so
// in silence: the row lost its URL, and whoever the URL was sent to found
// out before the developer did.
#[test]
fn a_restart_that_closes_a_share_says_which_url_and_how_to_get_a_new_one() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);
    let _tunnel = share_through_a_live_tunnel(&fx, &name);

    let said = std::cell::RefCell::new(Vec::<String>::new());
    let report = restart(&fx.paths, &fx.config, &name, None, &|line| {
        said.borrow_mut().push(line.to_string())
    })
    .unwrap();
    let _g2 = guard(&report);
    assert!(fx.state().worktrees[&name].share.is_none());
    let said = said.into_inner();
    assert!(
        said.contains(&share_closed(&name, "https://x.trycloudflare.com")),
        "{said:?}"
    );
    assert!(
        said.iter()
            .any(|line| line.contains(&format!("`pando share {name}`"))),
        "{said:?}"
    );
}

#[test]
fn stopping_everything_says_which_public_url_it_closed() {
    let fx = fixture();
    let name = worktree_named(&fx, "feat/shared");
    let mut store = fx.state();
    store
        .worktrees
        .get_mut(&name)
        .expect("new wrote a record")
        .share = Some(share_record_of(4_000_001, None));
    state::save(&fx.paths.state_file(), &store).unwrap();

    let said = std::cell::RefCell::new(Vec::<String>::new());
    super::stop_all_with(&fx.paths, None, |_| Ok(()), &|line| {
        said.borrow_mut().push(line.to_string())
    })
    .unwrap();
    let said = said.into_inner();
    assert!(
        said.contains(&share_closed(&name, "https://x.trycloudflare.com")),
        "{said:?}"
    );
}

// The name is checked against config, not against what happens to be
// running: the same typo used to produce two different messages
// depending on unrelated state, and only one of them listed the names
// config declares.
#[test]
fn restart_only_answers_a_name_config_never_heard_of_with_the_names_it_did() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&first);

    let err = format!(
        "{:#}",
        restart(&fx.paths, &fx.config, &name, Some("typo"), &noop).unwrap_err()
    );
    assert!(err.contains("no process named \"typo\""), "{err}");
    assert!(err.contains("api, web"), "{err}");
    assert_eq!(
        live_processes(&fx, &name),
        vec!["api", "web"],
        "and a refused restart stops nothing"
    );
}

// `stop` itself keeps the stricter message: it cannot see config — it
// has to work when `pando.toml` is broken — so the record is the only
// thing it can check a name against.
#[test]
fn stop_only_still_refuses_a_name_the_worktree_is_not_running() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, Some("web"), &noop).unwrap();
    let _guard = guard(&first);
    let err = format!("{:#}", stop(&fx.paths, &name, Some("api")).unwrap_err());
    assert!(
        err.contains("is not running a process named \"api\""),
        "{err}"
    );
}

#[test]
fn restart_replaces_every_process_and_keeps_every_port() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    drop(guard(&first));
    let before: Vec<u32> = first.started.iter().map(|p| p.record.pid).collect();

    let report = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    assert_eq!(names_of(&report.started), vec!["api", "web"]);
    assert_eq!(report.ports, first.ports);
    let after: Vec<u32> = report.started.iter().map(|p| p.record.pid).collect();
    assert_ne!(after, before);
    for pid in before {
        assert!(!crate::process::is_alive(pid), "pid {pid} survived");
    }
}

// `status --env` merges the processes in name order, each with the env
// it started with: two processes that both map `PORT` leave the last
// one's port, not the first one's pushed onto every later one.
#[test]
fn the_env_for_a_command_run_by_hand_takes_a_shared_key_from_the_last_process_by_name() {
    let mut fx = fixture();
    for role in ["api", "web"] {
        fx.config.processes.insert(
            role.to_string(),
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                ports: Some(PortsSpec::Map(BTreeMap::from([(
                    "PORT".to_string(),
                    role.to_string(),
                )]))),
                ..Default::default()
            },
        );
    }
    let name = worktree_named(&fx, "feat/one");
    let mut store = fx.state();
    store
        .worktrees
        .entry(name.clone())
        .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(&name), true))
        .ports = BTreeMap::from([("api".to_string(), 17009), ("web".to_string(), 17008)]);
    state::save(&fx.paths.state_file(), &store).unwrap();

    let env = resolved_env(&fx.paths, &fx.config, &name).unwrap();
    assert_eq!(env.get("PORT").map(String::as_str), Some("17008"));
}

// A shared start tells its processes the main checkout's service values.
// `status --env` left them out, so a migration run by hand after the eval
// read the worktree's own `.env` and reached a database the app was not on.
#[test]
fn the_env_for_a_command_run_by_hand_in_a_shared_worktree_names_the_main_checkouts_services() {
    let mut fx = fixture();
    std::fs::write(fx.root.join(".env"), "DATABASE_PORT=3306\n").unwrap();
    let services: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n",
    )
    .unwrap();
    fx.config.services = services.services;
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    std::fs::write(
        fx.worktrees_dir().join(&name).join(".env"),
        "DATABASE_PORT=52434\n",
    )
    .unwrap();
    let mut store = fx.state();
    store
        .worktrees
        .entry(name.clone())
        .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(&name), true))
        .ports = BTreeMap::from([("web".to_string(), 17008)]);
    state::save(&fx.paths.state_file(), &store).unwrap();

    let env = resolved_env(&fx.paths, &fx.config, &name).unwrap();
    assert_eq!(env.get("DATABASE_PORT").map(String::as_str), Some("3306"));
    assert_eq!(env.get("PORT").map(String::as_str), Some("17008"));
}

// ---- stop ------------------------------------------------------------

#[test]
fn stop_ends_the_process_and_keeps_the_ports() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pgid = outcome.started[0].record.pgid;
    let ports = outcome.ports.clone();

    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::Stopped(vec!["dev".to_string()])
    );
    assert!(
        !crate::process::group_alive(pgid),
        "the group must be empty"
    );
    let record = &fx.state().worktrees[&name];
    assert!(
        record.processes.is_empty(),
        "the record goes with the process"
    );
    assert_eq!(
        record.ports, ports,
        "a stopped worktree still owns its ports"
    );
    assert!(record.created_by_pando, "and is still ours");
}

// The origin tool skipped the signal for a record it had written off,
// and leaked every child whose shell had already exited.
#[test]
fn stop_signals_the_group_even_when_the_leader_is_already_gone() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30 & exit 0"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let (pid, pgid) = (
        outcome.started[0].record.pid,
        outcome.started[0].record.pgid,
    );

    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(pid)
    }));
    assert!(
        crate::process::group_alive(pgid),
        "the backgrounded child is still there"
    );
    // Written off as failed, which is exactly when the signal used to be
    // skipped.
    let mut store = fx.state();
    store
        .worktrees
        .get_mut(&name)
        .unwrap()
        .processes
        .get_mut("dev")
        .unwrap()
        .phase = Phase::Failed {
        at: Utc::now(),
        reason: "process exited".into(),
    };
    state::save(&fx.paths.state_file(), &store).unwrap();

    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::Stopped(vec!["dev".to_string()])
    );
    assert!(
        !crate::process::group_alive(pgid),
        "a failed record's group must still be killed"
    );
}

#[test]
fn stopping_something_that_is_not_running_is_not_an_error() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    assert_eq!(
        stop(&fx.paths, "nope", None).unwrap(),
        StopOutcome::NotRunning
    );
    let name = worktree_named(&fx, "feat/one");
    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::NotRunning
    );
}

#[test]
fn stop_all_stops_every_worktree_that_is_running() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let one = worktree_named(&fx, "feat/one");
    let two = worktree_named(&fx, "feat/two");
    let a = start(&fx.paths, &fx.config, &one, None, &noop).unwrap();
    let _ga = guard(&a);
    let b = start(&fx.paths, &fx.config, &two, None, &noop).unwrap();
    let _gb = guard(&b);
    assert_ne!(
        a.ports["web"], b.ports["web"],
        "two worktrees never share a port"
    );

    let mut stopped = stop_all(&fx.paths).unwrap();
    stopped.sort();
    assert_eq!(stopped, vec![one.clone(), two.clone()]);
    assert!(!crate::process::group_alive(a.started[0].record.pgid));
    assert!(!crate::process::group_alive(b.started[0].record.pgid));
    assert!(fx.state().worktrees[&one].processes.is_empty());
    assert!(fx.state().worktrees[&two].processes.is_empty());
    assert!(
        stop_all(&fx.paths).unwrap().is_empty(),
        "and it is idempotent"
    );
}

// A record under a name this pando does not start — state a newer one
// wrote, or a process since renamed in config — is still a process
// group, and `reconcile` is about to drop it.
#[test]
fn start_signals_every_group_recorded_for_the_worktree() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");

    // A leader that exits and leaves its child behind, recorded under
    // another process name.
    let log = fx.paths.log_file(&name, "worker");
    let stray = crate::testutil::spawn_guarded("sleep 30 & exit 0", &fx.root, &log);
    let (stray_pid, stray_pgid) = (stray.pid, stray.pgid);
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(stray_pid)
    }));
    assert!(crate::process::group_alive(stray_pgid));

    let mut store = fx.state();
    store.worktrees.get_mut(&name).unwrap().processes.insert(
        "worker".to_string(),
        ProcessRecord {
            pid: stray_pid,
            pgid: stray_pgid,
            started_at: Utc::now(),
            log_path: log,
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    state::save(&fx.paths.state_file(), &store).unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        !crate::process::group_alive(stray_pgid),
        "a group nothing would record again must not be left running"
    );
    assert_eq!(
        fx.state().worktrees[&name].processes.len(),
        1,
        "and its record goes with it"
    );
}

// ---- orphans in *another* worktree ------------------------------------

/// A worktree whose dev process has already lost its leader: the shell
/// exits at once and the child it backgrounded keeps the group — and its
/// port — alive. This is the record every `reconcile` is about to drop,
/// and dropping it unsignalled is how the child becomes unfindable.
///
/// The fixture's config is left holding a plain `sleep`, so a command
/// run against *another* worktree afterwards starts something ordinary.
fn orphaned_sibling(fx: &mut Fx) -> (String, Detached) {
    with_dev(fx, dev("sleep 300 & exit 0"));
    let name = worktree_named(fx, "feat/orphan");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let orphan = guard(&outcome)
        .pop()
        .expect("the start spawned exactly one process");
    assert!(
        wait_until(Duration::from_secs(5), || {
            !crate::process::is_alive(orphan.pid)
        }),
        "the shell that backgrounded the child should have exited"
    );
    assert!(
        crate::process::group_alive(orphan.pgid),
        "the child holds the group open"
    );
    with_dev(fx, dev("sleep 30"));
    (name, orphan)
}

// Phase 2b review, finding 7. A `Failed` record now outlives
// `reconcile`, and it is exactly the record most likely to still have a
// live child behind its pgid — so the sweep has to keep signalling it,
// and only its own worktree's stop may clear it.
#[test]
fn a_failed_records_group_is_signalled_and_the_record_kept_until_its_own_stop() {
    let mut fx = fixture();
    let (orphan_name, orphan) = orphaned_sibling(&mut fx);
    // A refresh is what turns a dead leader into a Failed record; the
    // child it backgrounded is still holding the group open.
    let state = refresh(&fx.paths).state;
    assert!(
        matches!(
            state.worktrees[&orphan_name].processes["dev"].phase,
            Phase::Failed { .. }
        ),
        "{:?}",
        state.worktrees[&orphan_name].processes["dev"].phase
    );

    let other = worktree_named(&fx, "feat/other");
    stop(&fx.paths, &other, None).unwrap();
    assert!(
        !crate::process::group_alive(orphan.pgid),
        "a Failed record's group is signalled like every other"
    );
    assert!(
        matches!(
            fx.state().worktrees[&orphan_name].processes["dev"].phase,
            Phase::Failed { .. }
        ),
        "but stopping another worktree must not erase the crash"
    );

    // Its own stop is what clears it.
    stop(&fx.paths, &orphan_name, None).unwrap();
    assert!(fx.state().worktrees[&orphan_name].processes.is_empty());
}

// Finding 7 on the path `--only` promises to leave alone: `stop` ran
// `reconcile` over the whole state file, so stopping the web process
// threw away the record that said the api had crashed.
#[test]
fn stopping_one_process_leaves_a_siblings_failed_record_where_status_can_see_it() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);

    // The api dies, and a read is what notices.
    let api = fx.state().worktrees[&name].processes["api"].clone();
    crate::process::stop(api.pgid, Duration::from_secs(5)).unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || matches!(
            refresh(&fx.paths).state.worktrees[&name]
                .processes
                .get("api")
                .map(|p| &p.phase),
            Some(Phase::Failed { .. })
        )),
        "the api never reached failed"
    );

    stop(&fx.paths, &name, Some("web")).unwrap();

    let state = refresh(&fx.paths).state;
    let record = &state.worktrees[&name];
    assert!(
        !record.processes.contains_key("web"),
        "the process that was stopped is gone"
    );
    assert!(
        matches!(
            record.processes.get("api").map(|p| &p.phase),
            Some(Phase::Failed { .. })
        ),
        "and the one that crashed is still saying so: {:?}",
        record.processes
    );
    assert_eq!(
        state::aggregate_phase(record).map(|a| a.word()),
        Some("failed"),
        "which is what the worktree reads as"
    );

    // A whole stop of its own worktree is what clears it.
    stop(&fx.paths, &name, None).unwrap();
    assert!(fx.state().worktrees[&name].processes.is_empty());
}

// The half finding 7 left open. `reconcile` keeps a record that is
// *already* `Failed`, but a process that died since the last read path
// is still recorded as `Running` — and `reconcile` drops it before
// anything has had the chance to mark it failed. So a mutation path has
// to advance phases first, exactly as the read path does.
#[test]
fn a_crash_no_read_path_has_seen_yet_survives_a_stop_of_its_sibling() {
    let mut fx = fixture();
    with_web_and_api(&mut fx);
    let name = workspace_worktree(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);

    // kill -9, and then *nothing reads state*: no `status`, no `ls`, no
    // TUI tick. What is on disk still says the api is running.
    let api = fx.state().worktrees[&name].processes["api"].clone();
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(api.pgid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || !crate::process::is_alive(
            api.pid
        )),
        "the api should be gone after a SIGKILL"
    );
    assert!(
        matches!(
            fx.state().worktrees[&name].processes["api"].phase,
            Phase::Running { .. } | Phase::Starting { .. }
        ),
        "the premise: nothing has marked it failed yet"
    );

    stop(&fx.paths, &name, Some("web")).unwrap();

    // On disk, without a read path having run since.
    let record = &fx.state().worktrees[&name];
    let Some(api_record) = record.processes.get("api") else {
        panic!("the crash was reconciled away: {:?}", record.processes);
    };
    match &api_record.phase {
        Phase::Failed { reason, .. } => assert!(
            reason.contains("process exited"),
            "the reason should say what happened: {reason}"
        ),
        other => panic!("a crash has to be recorded as failed, not {other:?}"),
    }
    // And the first read path after it agrees, which is what `status`
    // and the TUI row show.
    let state = refresh(&fx.paths).state;
    assert_eq!(
        state::aggregate_phase(&state.worktrees[&name]).map(|a| a.word()),
        Some("failed"),
        "status has to be able to see it"
    );

    stop(&fx.paths, &name, None).unwrap();
    assert!(fx.state().worktrees[&name].processes.is_empty());
}

#[test]
fn stopping_one_worktree_signals_another_ones_orphan() {
    let mut fx = fixture();
    let (orphan_name, orphan) = orphaned_sibling(&mut fx);
    let other = worktree_named(&fx, "feat/other");

    assert_eq!(
        stop(&fx.paths, &other, None).unwrap(),
        StopOutcome::NotRunning
    );
    assert!(
        !crate::process::group_alive(orphan.pgid),
        "a record reconcile could drop must have been signalled first"
    );
    // Signalled, and *kept*: the mutation advances phases before it
    // reconciles, so a leader that died without a read path noticing is
    // a crash the developer still gets to see. Its own worktree's stop
    // is what clears it.
    assert!(
        matches!(
            fx.state().worktrees[&orphan_name].processes["dev"].phase,
            Phase::Failed { .. }
        ),
        "the crash stays visible: {:?}",
        fx.state().worktrees[&orphan_name].processes["dev"].phase
    );
    stop(&fx.paths, &orphan_name, None).unwrap();
    assert!(
        fx.state().worktrees[&orphan_name].processes.is_empty(),
        "and only its own stop drops it"
    );
}

#[test]
fn starting_one_worktree_signals_another_ones_orphan() {
    let mut fx = fixture();
    let (_orphan_name, orphan) = orphaned_sibling(&mut fx);
    let other = worktree_named(&fx, "feat/other");

    let outcome = start(&fx.paths, &fx.config, &other, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        !crate::process::group_alive(orphan.pgid),
        "starting one worktree must not orphan another one's child"
    );
}

#[test]
fn removing_one_worktree_signals_another_ones_orphan() {
    let mut fx = fixture();
    let (_orphan_name, orphan) = orphaned_sibling(&mut fx);
    let other = worktree_named(&fx, "feat/other");

    rm(&fx.paths, &other, false, false).unwrap();
    assert!(
        !crate::process::group_alive(orphan.pgid),
        "removing one worktree must not orphan another one's child"
    );
}

#[test]
fn creating_a_worktree_signals_another_ones_orphan() {
    let mut fx = fixture();
    let (_orphan_name, orphan) = orphaned_sibling(&mut fx);

    worktree_named(&fx, "feat/other");
    assert!(
        !crate::process::group_alive(orphan.pgid),
        "creating a worktree must not orphan another one's child"
    );
}

// A group that would not die must not have its record cleared: the pgid
// is the only way back to it.
#[test]
fn stop_all_keeps_a_record_whose_group_it_could_not_signal() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let stubborn = worktree_named(&fx, "feat/stubborn");
    let willing = worktree_named(&fx, "feat/willing");
    let mut store = fx.state();
    for (name, pgid) in [(&stubborn, 4242), (&willing, 4243)] {
        store
            .worktrees
            .entry(name.clone())
            .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(name), true))
            .processes
            .insert("dev".to_string(), fake_record(pgid));
    }
    state::save(&fx.paths.state_file(), &store).unwrap();

    let err = stop_all_with(&fx.paths, |pgid| {
        if pgid == 4242 {
            anyhow::bail!("killpg refused");
        }
        Ok(())
    })
    .unwrap_err();
    assert!(
        format!("{err:#}").contains(&stubborn),
        "the failure names the worktree: {err:#}"
    );

    let saved = fx.state();
    assert!(
        saved.worktrees[&stubborn].processes.contains_key("dev"),
        "a group that was not signalled keeps its record"
    );
    assert!(
        saved.worktrees[&willing].processes.is_empty(),
        "the ones that were signalled are cleared, and saved"
    );
}

// A worktree whose processes are all gone can still be shared: the tunnel
// outlives them. `stop` with no name is "nothing of pando's left running",
// and it chose the worktrees to stop by their processes and services alone
// — so a public URL onto nothing stayed up through it.
#[test]
fn stop_all_takes_down_a_share_whose_processes_are_all_gone() {
    let fx = fixture();
    let name = worktree_named(&fx, "feat/shared");
    let tunnel = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file(&name, "tunnel"),
    );
    let mut store = fx.state();
    let record = store.worktrees.get_mut(&name).expect("new wrote a record");
    assert!(record.processes.is_empty() && record.services.is_empty());
    record.share = Some(state::ShareRecord {
        tunnel_pid: tunnel.pid,
        tunnel_pgid: tunnel.pgid,
        public_url: "https://x.trycloudflare.com".into(),
        local_port: 17_000,
        started_at: chrono::Utc::now(),
        log_path: fx.paths.log_file(&name, "tunnel"),
        proxy_pid: None,
        proxy_pgid: None,
        proxy_port: None,
    });
    state::save(&fx.paths.state_file(), &store).unwrap();

    let signalled = std::cell::RefCell::new(Vec::<i32>::new());
    let stopped = stop_all_with(&fx.paths, |pgid| {
        signalled.borrow_mut().push(pgid);
        Ok(())
    })
    .unwrap();
    assert_eq!(stopped, vec![name.clone()]);
    assert!(signalled.borrow().contains(&tunnel.pgid), "{signalled:?}");
    assert!(fx.state().worktrees[&name].share.is_none());
}

// The TUI's `X` lists what is up and stops on `y`, which can come long
// after: a worktree an agent started or shared in between was stopped,
// and its public URL closed, though the list never named it.
#[test]
fn a_stop_all_after_a_list_leaves_running_what_came_up_since() {
    let fx = fixture();
    let listed = worktree_named(&fx, "feat/listed");
    let since = worktree_named(&fx, "feat/since");
    let crashed = worktree_named(&fx, "feat/crashed");
    let mut store = fx.state();
    // Live leaders are this test's own pid, so no sweep signals them.
    let mut up = fake_record(4_000_201);
    up.pid = std::process::id();
    let mut came_up = fake_record(4_000_202);
    came_up.pid = std::process::id();
    for (name, record) in [
        (&listed, up),
        (&since, came_up),
        (&crashed, failed_record(4_000_203)),
    ] {
        store
            .worktrees
            .get_mut(name)
            .expect("new wrote a record")
            .processes
            .insert("dev".to_string(), record);
    }
    state::save(&fx.paths.state_file(), &store).unwrap();

    let said = std::cell::RefCell::new(Vec::<String>::new());
    let signalled = std::cell::RefCell::new(Vec::<i32>::new());
    let report = super::stop_all_with(
        &fx.paths,
        Some(std::slice::from_ref(&listed)),
        |pgid| {
            signalled.borrow_mut().push(pgid);
            Ok(())
        },
        &|line| said.borrow_mut().push(line.to_string()),
    )
    .unwrap();
    let mut stopped = report.stopped;
    stopped.sort();
    // Not up, so never listed: a crashed one goes as it always did.
    assert_eq!(stopped, vec![crashed.clone(), listed.clone()]);
    // And the one kept is returned, for the TUI's header to name.
    assert_eq!(report.kept, vec![since.clone()]);
    assert!(!signalled.borrow().contains(&4_000_202), "{signalled:?}");
    assert!(fx.state().worktrees[&since].processes.contains_key("dev"));
    assert!(
        said.borrow()
            .iter()
            .any(|line| line.contains(&since) && line.contains("left running")),
        "{:?}",
        said.borrow()
    );
}

// Phase 2c review, finding 2. A `Failed` record survives `reconcile`
// until its own worktree is acted on, and the sweep used to re-signal
// its pgid on every mutation anywhere in the project — which, once
// that pid has wrapped around, is an unrelated session leader being
// SIGTERMed and then SIGKILLed, over and over.
#[test]
fn a_dead_groups_pgid_is_signalled_once_and_not_on_every_later_mutation() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    // Far above `kern.maxproc`, so `is_alive` is certainly false and
    // no real process can be behind either number.
    record
        .processes
        .insert("dev".to_string(), failed_record(4_000_001));
    // A process that is still alive is never signalled by the sweep,
    // swept flag or not.
    let mut live = fake_record(4_000_002);
    live.pid = std::process::id();
    record.processes.insert("web".to_string(), live);
    store.worktrees.insert("feat+one".to_string(), record);

    let signalled = std::cell::RefCell::new(Vec::new());
    let watch = |pgid: i32| {
        signalled.borrow_mut().push(pgid);
        Ok(())
    };

    sweep_orphaned_groups_with(&mut store, watch).unwrap();
    assert_eq!(
        *signalled.borrow(),
        vec![4_000_001],
        "the dead leader's group is signalled, the live one's is not"
    );

    sweep_orphaned_groups_with(&mut store, watch).unwrap();
    assert_eq!(
        *signalled.borrow(),
        vec![4_000_001],
        "a second mutation must not signal that pgid again"
    );
    assert!(
        store.worktrees["feat+one"].processes["dev"].swept,
        "and the record is what remembers it"
    );

    // A leader that died since is a different matter: it has never
    // been swept, so its group is signalled on the next mutation.
    store
        .worktrees
        .get_mut("feat+one")
        .unwrap()
        .processes
        .insert("api".to_string(), failed_record(4_000_003));
    sweep_orphaned_groups_with(&mut store, watch).unwrap();
    assert_eq!(
        *signalled.borrow(),
        vec![4_000_001, 4_000_003],
        "a freshly dead leader is still signalled"
    );
}

// A signal that did not go out has to be tried again: the flag records
// that the group *was* signalled, not that it was looked at.
#[test]
fn a_group_that_could_not_be_signalled_is_not_recorded_as_swept() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record
        .processes
        .insert("dev".to_string(), failed_record(4_000_001));
    store.worktrees.insert("feat+one".to_string(), record);

    let err =
        sweep_orphaned_groups_with(&mut store, |_| anyhow::bail!("killpg refused")).unwrap_err();
    assert!(format!("{err:#}").contains("feat+one/dev"), "{err:#}");
    assert!(!store.worktrees["feat+one"].processes["dev"].swept);
}

// The flag lives in the state file, because the mutation that must not
// re-signal the group is a later run of pando, not a later line of
// this one. A state file written before the flag existed reads as
// "never swept", which signals once more and is the safe direction.
#[test]
fn the_swept_flag_survives_a_real_mutation_and_defaults_to_false() {
    let fx = fixture();
    let crashed = worktree_named(&fx, "feat/crashed");
    let other = worktree_named(&fx, "feat/other");
    let mut store = state::load(&fx.paths.state_file()).unwrap();
    store
        .worktrees
        .entry(crashed.clone())
        .or_insert_with(|| WorktreeRecord::new(fx.worktrees_dir().join(&crashed), true))
        .processes
        .insert("dev".to_string(), failed_record(4_000_001));
    state::save(&fx.paths.state_file(), &store).unwrap();
    let written = std::fs::read_to_string(fx.paths.state_file()).unwrap();
    assert!(
        !written.contains("swept"),
        "a record that was never swept writes nothing: {written}"
    );

    // A mutation on a *different* worktree: the sweep is what acts on
    // the crashed record, and the record itself has to survive it.
    stop(&fx.paths, &other, None).unwrap();
    let saved = fx.state();
    let record = &saved.worktrees[&crashed].processes["dev"];
    assert!(matches!(record.phase, Phase::Failed { .. }), "{record:?}");
    assert!(
        record.swept,
        "without this in the file, the next mutation signals that pgid all \
         over again — and every one after it"
    );

    // And a second mutation leaves it exactly as it is.
    stop(&fx.paths, &other, None).unwrap();
    assert!(fx.state().worktrees[&crashed].processes["dev"].swept);
}

// The same finding, on the paths that act on the record's own worktree.
// The sweep signals a dead leader's group once and marks it, but a
// `start` or a `stop` of that worktree, days later, signalled the same
// pgid again — which by then can be anybody's process group.
#[test]
fn a_swept_groups_pgid_is_not_signalled_again_by_its_own_worktree() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    // What that pgid names by the time its worktree is acted on.
    let stranger = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file(&name, "stranger"),
    );
    let plant = || {
        let mut store = fx.state();
        let record = ProcessRecord {
            pid: 4_000_001,
            swept: true,
            ..failed_record(stranger.pgid)
        };
        store
            .worktrees
            .get_mut(&name)
            .expect("new wrote a record")
            .processes
            .insert("dev".to_string(), record);
        state::save(&fx.paths.state_file(), &store).unwrap();
    };

    plant();
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        crate::process::group_alive(stranger.pgid),
        "start signalled a pgid the sweep had already signalled"
    );
    assert_eq!(outcome.started.len(), 1, "and still replaced the record");

    stop(&fx.paths, &name, None).unwrap();
    plant();
    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::Stopped(vec!["dev".to_string()])
    );
    assert!(
        crate::process::group_alive(stranger.pgid),
        "stop signalled a pgid the sweep had already signalled"
    );
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "and still dropped the record"
    );
}

// And the flag has to reach the file. `share` and `rm` sweep and then
// refuse without saving, so the marks the sweep made were lost with the
// refusal: an agent polling `pando share` for a worktree that is not up
// yet signalled a crashed sibling's pgid on every call.
#[test]
fn a_mutation_that_sweeps_and_then_refuses_still_saves_the_sweep() {
    let fx = fixture();
    let crashed = worktree_named(&fx, "feat/crashed");
    let idle = worktree_named(&fx, "feat/idle");
    let plant = || {
        let mut store = fx.state();
        store
            .worktrees
            .get_mut(&crashed)
            .expect("new wrote a record")
            .processes
            .insert("dev".to_string(), failed_record(4_000_001));
        state::save(&fx.paths.state_file(), &store).unwrap();
    };
    let swept = || fx.state().worktrees[&crashed].processes["dev"].swept;

    plant();
    let err = share_with(
        &fx.paths,
        &fx.config,
        &idle,
        &MissingProvider,
        &stub_proxy,
        &noop,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("start it first"), "{err:#}");
    assert!(swept(), "a refused share forgot the group it signalled");

    plant();
    std::fs::write(fx.worktrees_dir().join(&idle).join("scratch.txt"), "wip").unwrap();
    let err = rm(&fx.paths, &idle, false, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("modified or untracked"),
        "{err:#}"
    );
    assert!(swept(), "a refused rm forgot the group it signalled");

    // A branch another worktree has checked out passes every check `new`
    // makes itself, and git refuses it after the sweep.
    plant();
    let elsewhere = fx.root.parent().unwrap().join("elsewhere");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "taken",
            elsewhere.to_str().unwrap(),
        ],
    );
    let err = new(&fx.paths, &fx.config, "taken", None, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("git worktree add failed"),
        "{err:#}"
    );
    assert!(swept(), "a refused new forgot the group it signalled");
}

/// A `Failed` record for a group that does not exist and a pid that
/// cannot: the shape the sweep is about.
fn failed_record(pgid: i32) -> ProcessRecord {
    ProcessRecord {
        phase: Phase::Failed {
            at: Utc::now(),
            reason: "process exited".to_string(),
        },
        ..fake_record(pgid)
    }
}

/// A process record for a group that does not exist, for tests about
/// bookkeeping rather than about signals.
fn fake_record(pgid: i32) -> ProcessRecord {
    ProcessRecord {
        pid: pgid as u32,
        pgid,
        started_at: Utc::now(),
        log_path: PathBuf::from("/does/not/exist/dev.log"),
        ready_port: None,
        ready_timeout_s: None,
        observed_ports: Vec::new(),
        swept: false,
        phase: Phase::Running { since: Utc::now() },
    }
}

// A readiness timeout over a process that is serving — on a port it
// chose itself — is not a crash, and "nothing bound port N" alone sends
// the developer looking for one.
#[test]
fn a_timeout_names_the_ports_the_process_opened_instead() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_000);
    record.ports.insert("api".to_string(), 17_001);
    let mut web = fake_record(999_901);
    web.phase = Phase::Failed {
        at: Utc::now(),
        reason: "timeout: nothing bound port 17000 in 30s".to_string(),
    };
    let mut api = fake_record(999_902);
    api.phase = Phase::Failed {
        at: Utc::now(),
        reason: "timeout: nothing bound port 17001 in 30s".to_string(),
    };
    record.processes.insert("web".to_string(), web);
    record.processes.insert("api".to_string(), api);
    store.worktrees.insert("feat+one".to_string(), record);

    let scans = BTreeMap::from([
        (999_901, Some(vec![3000, 3000])),
        // Listening only on a port the worktree assigned to a role — its
        // own second role, say — is not listening elsewhere.
        (999_902, Some(vec![17_000])),
    ]);
    assert!(explain_new_failures(&mut store, &[], &scans));
    let record = &store.worktrees["feat+one"];
    let Phase::Failed { reason, .. } = &record.processes["web"].phase else {
        panic!("still failed");
    };
    assert!(
        reason.starts_with("timeout: nothing bound port 17000"),
        "{reason}"
    );
    assert!(
        reason.contains("listening on 3000 instead"),
        "the port it did open is the diagnosis: {reason}"
    );
    let Phase::Failed { reason, .. } = &record.processes["api"].phase else {
        panic!("still failed");
    };
    assert!(!reason.contains("instead"), "{reason}");
}

#[test]
fn only_a_timeout_is_explained_by_where_the_process_listens() {
    assert_eq!(listening_elsewhere(state::EXITED, &[3000], &[17_000]), None);
    assert_eq!(listening_elsewhere("timeout: x", &[], &[17_000]), None);
    let note = listening_elsewhere("timeout: x", &[5180, 3001, 17_000], &[17_000]).unwrap();
    assert!(
        note.starts_with("it is listening on 3001, 5180 instead"),
        "{note}"
    );
}

// Phase 2b review, finding 4. One flat list per worktree cannot say
// which group opened which socket, and the URL rule needs exactly that.
#[test]
fn observed_ports_are_recorded_per_process_and_the_worktrees_list_is_their_union() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.processes.insert("web".to_string(), fake_record(101));
    record.processes.insert("api".to_string(), fake_record(102));
    store.worktrees.insert("feat+one".to_string(), record);

    let scans = BTreeMap::from([
        (101, Some(vec![17_342])),
        (102, Some(vec![17_343, 9876, 17_343])),
    ]);
    assert!(capture_observed_ports(&mut store, &scans));

    let record = &store.worktrees["feat+one"];
    assert_eq!(record.processes["web"].observed_ports, vec![17_342]);
    assert_eq!(
        record.processes["api"].observed_ports,
        vec![9876, 17_343],
        "sorted, and each port once"
    );
    assert_eq!(
        record.observed_ports,
        vec![9876, 17_342, 17_343],
        "the worktree's own list is the union, which is what the JSON shape publishes"
    );

    // A scan that could not run says nothing at all: the last good
    // answer stands rather than being cleared by a missing `lsof`.
    let unscannable = BTreeMap::from([(101, None), (102, None)]);
    assert!(!capture_observed_ports(&mut store, &unscannable));
    assert_eq!(
        store.worktrees["feat+one"].observed_ports,
        vec![9876, 17_342, 17_343]
    );

    // A process that is no longer up is listening on nothing, and its
    // last sighting is stale the moment it stops.
    store
        .worktrees
        .get_mut("feat+one")
        .unwrap()
        .processes
        .get_mut("web")
        .unwrap()
        .phase = Phase::Failed {
        at: Utc::now(),
        reason: "process exited".to_string(),
    };
    assert!(capture_observed_ports(&mut store, &scans));
    let record = &store.worktrees["feat+one"];
    assert!(record.processes["web"].observed_ports.is_empty());
    assert_eq!(record.observed_ports, vec![9876, 17_343]);
}

// Phase 2b review, finding 6. `start` and every read path have to hand
// out the same URL for the same worktree, in every state it can be in.
#[test]
fn start_and_the_read_paths_agree_on_the_url_when_nothing_owns_web() {
    let mut fx = fixture();
    // `alpha` owns `srv` and `beta` owns `admin`: the alphabetically
    // first *process* and the alphabetically first *role* are different
    // answers, which is what made two commands disagree.
    for (process, role) in [("alpha", "srv"), ("beta", "admin")] {
        fx.config.processes.insert(
            process.to_string(),
            ProcessConfig {
                cmd: "sleep 30".to_string(),
                ports: Some(PortsSpec::List(vec![role.to_string()])),
                ..Default::default()
            },
        );
    }
    let name = worktree_named(&fx, "feat/url2");

    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _g = guard(&report);
    let expected = format!("http://localhost:{}", report.ports["srv"]);
    assert_eq!(
        report.url.as_deref(),
        Some(expected.as_str()),
        "alpha comes first, so its first role is the worktree's URL"
    );
    assert_eq!(
        worktree_url(&fx.state().worktrees[&name]),
        report.url,
        "and the record every read path works from says the same"
    );

    // The ports survive a stop, so the URL does too — and it is still
    // the same one.
    stop(&fx.paths, &name, None).unwrap();
    assert_eq!(worktree_url(&fx.state().worktrees[&name]), report.url);
}

// ---- share -----------------------------------------------------------

use crate::testutil::{FAKE_TUNNEL_URL, fake_cloudflared_failing, fake_cloudflared_publishing};

/// Stops whatever a share started, even when an assertion panics first.
struct ShareGuard(Option<ShareRecord>);

impl Drop for ShareGuard {
    fn drop(&mut self) {
        if let Some(record) = &self.0 {
            let _ = tunnel::stop_share(record);
        }
    }
}

fn share_guard(fx: &Fx, name: &str) -> ShareGuard {
    ShareGuard(fx.state().worktrees.get(name).and_then(|r| r.share.clone()))
}

/// A worktree running a real listener on its `web` port, with a fake
/// provider installed — the state every share test starts from.
fn shared_fixture() -> Option<(Fx, String, Vec<Detached>, StartReport)> {
    shared_fixture_running(python_listener_template())
}

/// [`shared_fixture`], with `cmd` as the process on the `web` port.
fn shared_fixture_running(cmd: String) -> Option<(Fx, String, Vec<Detached>, StartReport)> {
    if !python3_available() {
        eprintln!("skipping: python3 is needed for a process that really holds a port");
        return None;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd,
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ready: Some(ReadySpec {
                role: Some("web".to_string()),
                timeout_s: None,
            }),
            ..Default::default()
        },
    );
    fake_cloudflared_publishing(&fx.paths.home);
    let name = worktree_named(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let guards = guard(&report);
    let port = report.ports["web"];
    assert!(
        wait_until(Duration::from_secs(20), || {
            refresh(&fx.paths);
            crate::ports::something_is_listening(port)
        }),
        "the listener never bound {port}"
    );
    // One refresh so the process is Running rather than Starting: a
    // share of something still coming up is refused on purpose.
    refresh(&fx.paths);
    Some((fx, name, guards, report))
}

/// A stand-in for the real proxy, which re-execs the running binary —
/// inside a library test that is the test harness, which exits at once.
/// This one is a detached process group with a pid that listens on the
/// proxy's port, which is all the assertions here are about: that it is
/// started, waited for, recorded, and taken down again when the share it
/// belongs to fails.
fn stub_proxy(
    paths: &PandoPaths,
    name: &str,
    listen: u16,
    _upstream: u16,
    _cookie: &str,
) -> Result<share_proxy::ProxySpawn> {
    stub_proxy_running(
        paths,
        name,
        listen,
        &format!("exec {}", python_listener(listen)),
    )
}

/// [`stub_proxy`], running whatever the test needs its proxy to do.
fn stub_proxy_running(
    paths: &PandoPaths,
    name: &str,
    listen: u16,
    shell_cmd: &str,
) -> Result<share_proxy::ProxySpawn> {
    let log_path = paths.log_file(name, share_proxy::PROXY_LOG);
    let spawn = proc::spawn_detached(SpawnOptions {
        shell_cmd,
        cwd: &std::env::temp_dir(),
        log_file: &log_path,
        env: &[],
        status_file: None,
    })?;
    Ok(share_proxy::ProxySpawn {
        pid: spawn.pid,
        pgid: spawn.pgid,
        listen_port: listen,
        log_path,
    })
}

/// `share` as the CLI calls it, but with the proxy stubbed.
fn share_stubbed(fx: &Fx, config: &Config, name: &str) -> Result<ShareOutcome> {
    let provider = tunnel::provider_for(config.share.provider.as_deref())?;
    share_with(
        &fx.paths,
        config,
        name,
        provider.as_ref(),
        &stub_proxy,
        &noop,
    )
}

/// A provider that is not installed.
struct MissingProvider;

impl tunnel::Provider for MissingProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        bail!("cloudflared is not installed — `brew install cloudflared`")
    }
    fn start(
        &self,
        _: &PandoPaths,
        _: &str,
        _: &str,
        _: u16,
        _: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        panic!("a missing provider must never be asked to start anything")
    }
}

/// A provider that is installed and whose tunnel fails anyway, the way a
/// rate-limited quick tunnel does.
struct FailingProvider;

impl tunnel::Provider for FailingProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        Ok(())
    }
    fn start(
        &self,
        _: &PandoPaths,
        _: &str,
        _: &str,
        _: u16,
        _: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        bail!("cloudflared published no URL within 30s — tail: 429 Too Many Requests")
    }
}

#[test]
fn share_refuses_a_worktree_that_is_not_running() {
    let fx = fixture();
    fake_cloudflared_publishing(&fx.paths.home);
    let name = worktree_named(&fx, "feat/one");

    let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("start it first"), "{message}");
    assert!(
        fx.state()
            .worktrees
            .get(&name)
            .is_none_or(|r| r.share.is_none()),
        "nothing may be recorded for a refused share"
    );
}

// Finding 8. `start` records `Starting` and returns; the phase advances
// on the next read path, once the port is really bound. So
// `pando start x && pando share x` — the first thing anyone types —
// answered "x is not running — start it first". Waiting is what a
// developer does by hand, and `share` already blocks on the auth
// command and on the tunnel, both narrated.
#[test]
fn share_waits_for_a_worktree_that_start_has_only_just_returned_from() {
    if !python3_available() {
        eprintln!("skipping: python3 is needed for a process that really holds a port");
        return;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            // A server that takes a moment to bind, so `share` meets it
            // still starting however quickly the shell itself comes up.
            cmd: format!("sleep 1 && {}", python_listener_template()),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ready: Some(ReadySpec {
                role: Some("web".to_string()),
                timeout_s: None,
            }),
            ..Default::default()
        },
    );
    fake_cloudflared_publishing(&fx.paths.home);
    let name = worktree_named(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guards = guard(&report);
    assert!(
        matches!(
            fx.state().worktrees[&name].processes["dev"].phase,
            Phase::Starting { .. }
        ),
        "the whole point of this test is that `start` returns before readiness"
    );

    let said = std::sync::Mutex::new(Vec::<String>::new());
    let outcome = share(&fx.paths, &fx.config, &name, &|line| {
        said.lock().unwrap().push(line.to_string())
    })
    .unwrap();
    let _share = share_guard(&fx, &name);

    assert_eq!(outcome.public_url, FAKE_TUNNEL_URL);
    let said = said.into_inner().unwrap();
    assert!(
        said.iter().any(|line| line.contains("waiting")),
        "a wait the developer cannot see is a hang: {said:?}"
    );
}

#[test]
fn share_refuses_a_worktree_whose_process_has_stopped() {
    let Some((fx, name, guards, _)) = shared_fixture() else {
        return;
    };
    drop(guards);
    stop(&fx.paths, &name, None).unwrap();

    let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("not running"), "{err:#}");
}

#[test]
fn share_without_an_auth_command_tunnels_straight_to_the_web_port() {
    let Some((fx, name, _guards, report)) = shared_fixture() else {
        return;
    };
    let outcome = share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let _share = share_guard(&fx, &name);

    assert_eq!(outcome.public_url, FAKE_TUNNEL_URL);
    assert!(!outcome.pre_authed);
    assert!(!outcome.already);

    let record = fx.state().worktrees[&name].share.clone().unwrap();
    assert!(record.proxy_pid.is_none(), "no auth command, no proxy");
    assert!(record.proxy_port.is_none());
    assert_eq!(record.local_port, report.ports["web"]);
    assert!(crate::process::is_alive(record.tunnel_pid));
    assert_eq!(record.log_path, fx.paths.log_file(&name, "tunnel"));

    let log = std::fs::read_to_string(fx.paths.log_file(&name, "tunnel")).unwrap();
    assert!(
        log.contains(&format!("--url http://localhost:{}", report.ports["web"])),
        "the tunnel must point at the application itself: {log}"
    );
}

#[test]
fn share_with_an_auth_command_runs_it_and_puts_a_proxy_in_front() {
    let Some((fx, name, _guards, report)) = shared_fixture() else {
        return;
    };
    // Written outside the worktree: a share must never make the
    // repository dirty, not even from a test's own script.
    let seen = fx.paths.home.join("auth-env.txt");
    let mut config = fx.config.clone();
    // Something only the process environment carries, so "it runs with
    // the process env" is a claim this test can really check.
    config.processes.get_mut("dev").unwrap().env =
        BTreeMap::from([("APP_SECRET".to_string(), "from-the-process-env".to_string())]);
    config.share.auth_cmd = Some(format!("env > {}; printf 'session=abc123'", seen.display()));

    let outcome = share_stubbed(&fx, &config, &name).unwrap();
    let _share = share_guard(&fx, &name);
    assert!(outcome.pre_authed);

    let record = fx.state().worktrees[&name].share.clone().unwrap();
    let proxy_port = record.proxy_port.expect("a proxy port");
    assert_eq!(
        fx.state().worktrees[&name].share_port,
        Some(proxy_port),
        "the proxy's port is remembered, so a later share reuses it"
    );
    assert!(crate::process::is_alive(record.proxy_pid.unwrap()));
    assert_eq!(
        record.local_port, report.ports["web"],
        "the record still says what is being shared, not what is in front of it"
    );

    // By the one address the proxy binds. Told `localhost`, cloudflared
    // tries `[::1]` first, and anything that bound `[::1]` on the proxy's
    // port — which `ps` shows — was handed every visitor.
    let log = std::fs::read_to_string(fx.paths.log_file(&name, "tunnel")).unwrap();
    assert!(
        log.contains(&format!("--url http://127.0.0.1:{proxy_port}")),
        "the tunnel must point at the proxy, not the application: {log}"
    );

    // The process environment, plus the port.
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(
        env.contains(&format!("{ENV_SHARE_PORT}={proxy_port}")),
        "the auth command must be told the proxy's port: {env}"
    );
    assert!(env.contains(&format!("PANDO_NAME={name}")), "{env}");
    assert!(
        env.contains("APP_SECRET=from-the-process-env"),
        "the auth command runs with the same environment the process got: {env}"
    );
    assert_eq!(
        porcelain_status(&fx.worktrees_dir().join(&name)),
        Vec::<String>::new(),
        "the auth command must leave the worktree clean"
    );
}

#[test]
fn a_failing_auth_command_fails_the_share_with_its_own_complaint() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("echo 'no session for you' >&2; exit 3".to_string());

    let err = share_stubbed(&fx, &config, &name).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("exited 3"), "{message}");
    assert!(message.contains("no session for you"), "{message}");
    assert!(
        fx.state().worktrees[&name].share.is_none(),
        "a share that failed before it started anything records nothing"
    );
    assert!(
        !fx.paths.log_file(&name, "tunnel").exists(),
        "nothing may have been spawned"
    );
}

#[test]
fn an_auth_command_that_prints_nothing_usable_is_refused() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    for (cmd, expected) in [
        ("true", "printed nothing"),
        ("printf 'a\\nb'", "control character"),
    ] {
        let mut config = fx.config.clone();
        config.share.auth_cmd = Some(cmd.to_string());
        let err = share_stubbed(&fx, &config, &name).unwrap_err();
        assert!(
            format!("{err:#}").contains(expected),
            "{cmd:?} should be refused with {expected:?}: {err:#}"
        );
    }
}

#[test]
fn a_second_share_hands_back_the_url_it_already_has() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let first = share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let _share = share_guard(&fx, &name);
    let pid = fx.state().worktrees[&name]
        .share
        .clone()
        .unwrap()
        .tunnel_pid;

    let second = share(&fx.paths, &fx.config, &name, &noop).unwrap();
    assert_eq!(second.public_url, first.public_url);
    assert!(second.already, "the second call opened nothing");
    assert_eq!(
        fx.state().worktrees[&name]
            .share
            .clone()
            .unwrap()
            .tunnel_pid,
        pid,
        "and the tunnel is the same one"
    );
}

#[test]
fn unshare_stops_both_halves_and_clears_the_record() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    share_stubbed(&fx, &config, &name).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();
    let (tunnel_pid, proxy_pid) = (record.tunnel_pid, record.proxy_pid.unwrap());

    unshare(&fx.paths, &name).unwrap();

    assert!(fx.state().worktrees[&name].share.is_none());
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(tunnel_pid) && !crate::process::is_alive(proxy_pid)
    }));
    assert!(
        fx.state().worktrees[&name].share_port.is_some(),
        "the proxy's port is kept, so the next share reuses it"
    );
}

#[test]
fn unshare_refuses_a_worktree_that_is_not_shared() {
    let fx = fixture();
    let name = worktree_named(&fx, "feat/one");
    let err = unshare(&fx.paths, &name).unwrap_err();
    assert!(format!("{err:#}").contains("not shared"), "{err:#}");
}

// A tunnel whose process is already gone must still unshare: the record
// is the only thing holding a URL nobody can reach.
#[test]
fn unshare_clears_a_share_whose_tunnel_already_died() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();
    crate::process::stop(record.tunnel_pgid, Duration::from_secs(5)).unwrap();

    unshare(&fx.paths, &name).unwrap();
    assert!(fx.state().worktrees[&name].share.is_none());
}

#[test]
fn share_refuses_with_the_install_hint_when_the_provider_is_missing() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let err = share_with(
        &fx.paths,
        &fx.config,
        &name,
        &MissingProvider,
        &stub_proxy,
        &noop,
    )
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("not installed"), "{message}");
    assert!(message.contains("brew install cloudflared"), "{message}");
    assert!(fx.state().worktrees[&name].share.is_none());
}

#[test]
fn share_refuses_a_provider_pando_does_not_speak() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.provider = Some("ngrok".to_string());
    let err = share(&fx.paths, &config, &name, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("ngrok"), "{err:#}");
}

// The leak this guards: the proxy is spawned before the tunnel, and a
// tunnel that never comes up leaves nothing recorded that could ever
// find it again.
#[test]
fn a_tunnel_that_never_opens_takes_the_proxy_down_with_it() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());

    // The pid the proxy was given, captured as it is spawned: once the
    // share has failed, nothing records it, and that is the whole
    // point of this test.
    let spawned: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    let watched = |paths: &PandoPaths,
                   name: &str,
                   listen: u16,
                   upstream: u16,
                   cookie: &str|
     -> Result<share_proxy::ProxySpawn> {
        let spawn = stub_proxy(paths, name, listen, upstream, cookie)?;
        spawned.lock().unwrap().push(spawn.pid);
        Ok(spawn)
    };

    let err = share_with(&fx.paths, &config, &name, &FailingProvider, &watched, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("no URL"), "{err:#}");

    assert!(fx.state().worktrees[&name].share.is_none());
    let pids = spawned.into_inner().unwrap();
    assert_eq!(pids.len(), 1, "a proxy was started before the tunnel");
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::is_alive(
            pids[0]
        )),
        "the proxy outlived the share that spawned it, with nothing left to find it"
    );
}

/// A provider that is installed and must never be asked for a tunnel.
struct UnreachedProvider;

impl tunnel::Provider for UnreachedProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        Ok(())
    }
    fn start(
        &self,
        _: &PandoPaths,
        _: &str,
        _: &str,
        _: u16,
        _: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        panic!("a tunnel was opened onto a proxy that never listened")
    }
}

// `share_proxy::spawn` returns before the child binds anything, so a
// proxy that could not bind was found by the first visitor, through a
// tunnel already published onto it.
#[test]
fn a_proxy_that_cannot_bind_fails_the_share_before_a_tunnel_is_opened() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    let failing = |paths: &PandoPaths, name: &str, listen: u16, _: u16, _: &str| {
        stub_proxy_running(
            paths,
            name,
            listen,
            "echo 'pando: bind the share proxy: Address already in use' >&2; exit 1",
        )
    };

    let err = share_with(
        &fx.paths,
        &config,
        &name,
        &UnreachedProvider,
        &failing,
        &noop,
    )
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("before it was listening"), "{message}");
    assert!(
        message.contains("Address already in use"),
        "the proxy's own complaint is the diagnosis: {message}"
    );
    assert!(fx.state().worktrees[&name].share.is_none());
}

// Two shares of one worktree get the same proxy port. The one whose
// proxy lost the bind saw the other's proxy listening, published a
// tunnel, and recorded a share with a dead proxy; the other then gave
// way to it by stopping the only proxy that worked.
#[test]
fn a_share_whose_proxy_died_while_its_tunnel_opened_is_never_recorded() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    crate::testutil::fake_cloudflared(
        &fx.paths.home,
        &format!(
            "sleep 2\necho 'INF |  {FAKE_TUNNEL_URL}  |'\necho '{}'\nexec sleep 300\n",
            crate::testutil::FAKE_REGISTERED
        ),
    );
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    // The port answers for another share's proxy; this one gives up on
    // it a moment later, the way a lost bind does.
    let held = std::sync::Mutex::new(Vec::new());
    let losing = |paths: &PandoPaths, name: &str, listen: u16, _: u16, _: &str| {
        held.lock()
            .unwrap()
            .push(std::net::TcpListener::bind(("127.0.0.1", listen))?);
        stub_proxy_running(paths, name, listen, "exec sleep 0.2")
    };
    let provider = tunnel::provider_for(None).unwrap();

    let err = share_with(&fx.paths, &config, &name, provider.as_ref(), &losing, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("exited while its tunnel was starting"),
        "{err:#}"
    );
    assert!(
        fx.state().worktrees[&name].share.is_none(),
        "a share whose proxy is dead is a public URL onto nothing"
    );
}

/// A provider whose tunnel published its URL and was still dialling its
/// edge when the deadline passed.
struct DiallingProvider;

impl tunnel::Provider for DiallingProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        Ok(())
    }
    fn start(
        &self,
        paths: &PandoPaths,
        name: &str,
        _: &str,
        _: u16,
        spawned: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        let log_path = paths.log_file(name, tunnel::TUNNEL_LOG);
        let spawn = proc::spawn_detached(SpawnOptions {
            shell_cmd: "exec sleep 300",
            cwd: &std::env::temp_dir(),
            log_file: &log_path,
            env: &[],
            status_file: None,
        })?;
        spawned(spawn.pgid);
        Ok(tunnel::TunnelSpawn {
            pid: spawn.pid,
            pgid: spawn.pgid,
            public_url: FAKE_TUNNEL_URL.to_string(),
            log_path,
            unconnected: Some("ERR Failed to dial a quic connection".to_string()),
        })
    }
}

#[test]
fn a_share_whose_tunnel_has_not_reached_its_edge_says_so() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let said = std::sync::Mutex::new(Vec::<String>::new());
    let outcome = share_with(
        &fx.paths,
        &fx.config,
        &name,
        &DiallingProvider,
        &stub_proxy,
        &|m| said.lock().unwrap().push(m.to_string()),
    )
    .unwrap();
    let _share = share_guard(&fx, &name);

    assert_eq!(outcome.public_url, FAKE_TUNNEL_URL);
    let said = said.into_inner().unwrap();
    assert!(
        said.iter()
            .any(|m| m.contains("has not connected") && m.contains("Failed to dial")),
        "a URL that may not answer is handed out with the reason: {said:?}"
    );
}

/// A dev server on the `web` port that answers every request the way
/// Vite refuses a host it does not know.
fn vite_refusing_hosts_template() -> String {
    "python3 -u -c \"import socket;s=socket.socket();\
     s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
     s.bind(('127.0.0.1',{port:web}));s.listen(5)\nwhile True:\n c,_=s.accept()\n try:\n  \
     c.recv(65536);c.sendall(b'HTTP/1.1 403 Forbidden\\r\\nContent-Length: 51\\r\\n\
     Connection: close\\r\\n\\r\\nBlocked request. Add it to server.allowedHosts now.')\n \
     except Exception:\n  pass\n c.close()\""
        .to_string()
}

/// A server that reads one request, hands its head to the test, and
/// answers with `response`.
fn answering_once(response: &'static [u8]) -> (u16, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let mut head = Vec::new();
        let mut chunk = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
        tx.send(String::from_utf8_lossy(&head).into_owned()).ok();
        let _ = socket.write_all(response);
    });
    (port, rx)
}

// A stock Vite 6, Rails or Django dev server refuses every host that is
// not localhost or an address, and the tunnel hands it the public one:
// the share "worked", and every visitor got the framework's blocked-host
// page with nothing in pando pointing at the one-line fix.
#[test]
fn a_dev_server_that_refuses_the_public_host_is_named_with_its_fix() {
    let (port, seen) = answering_once(
        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 56\r\nConnection: close\r\n\r\n\
          Blocked request. This host is not allowed. allowedHosts.",
    );
    let refusal = host_refusal(port, "https://abc-def.trycloudflare.com").expect("a refusal");
    assert!(refusal.contains("Vite"), "{refusal}");
    assert!(
        refusal.contains("'.trycloudflare.com'") && refusal.contains("server.allowedHosts"),
        "every host the provider hands out, and where it goes: {refusal}"
    );
    let head = seen.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        head.contains("Host: abc-def.trycloudflare.com\r\n"),
        "asked as the tunnel will ask it: {head}"
    );
}

#[test]
fn a_dev_server_that_lets_the_public_host_in_says_nothing() {
    let (port, _) = answering_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
    assert_eq!(host_refusal(port, "https://abc.trycloudflare.com"), None);

    // One that never answers is building the page, which means it let
    // the host in.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    assert_eq!(host_refusal(port, "https://abc.trycloudflare.com"), None);
}

#[test]
fn a_share_of_a_dev_server_that_refuses_its_host_says_how_to_let_it_in() {
    let Some((fx, name, _guards, _)) = shared_fixture_running(vite_refusing_hosts_template())
    else {
        return;
    };
    let said = std::sync::Mutex::new(Vec::<String>::new());
    let provider = tunnel::provider_for(None).unwrap();
    let outcome = share_with(
        &fx.paths,
        &fx.config,
        &name,
        provider.as_ref(),
        &stub_proxy,
        &|m| said.lock().unwrap().push(m.to_string()),
    )
    .unwrap();
    let _share = share_guard(&fx, &name);

    assert_eq!(
        outcome.public_url, FAKE_TUNNEL_URL,
        "the share still stands"
    );
    let said = said.into_inner().unwrap();
    assert!(
        said.iter()
            .any(|m| m.contains("refuses the public URL's host") && m.contains("allowedHosts")),
        "{said:?}"
    );
}

// A share records its tunnel only once it has published, up to thirty
// seconds after spawning it. A pando that died in between — a Ctrl-C, a
// TUI quit mid-share — left the tunnel and the proxy running with nothing
// able to name them, so they are written down before the wait.
#[test]
fn a_share_names_what_it_has_spawned_while_its_tunnel_comes_up() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    let proxies = std::sync::Mutex::new(Vec::new());
    let watched = |paths: &PandoPaths, name: &str, listen: u16, upstream: u16, cookie: &str| {
        let spawn = stub_proxy(paths, name, listen, upstream, cookie)?;
        proxies.lock().unwrap().push(spawn.pgid);
        Ok(spawn)
    };
    let seen = std::sync::Mutex::new(Vec::new());
    let provider = PendingWatcher {
        paths: &fx.paths,
        seen: &seen,
    };

    share_with(&fx.paths, &config, &name, &provider, &watched, &noop).unwrap();
    let _share = share_guard(&fx, &name);

    let seen = seen.into_inner().unwrap();
    let tunnel = fx.state().worktrees[&name]
        .share
        .clone()
        .unwrap()
        .tunnel_pgid;
    let seen: Vec<(u32, Vec<i32>)> = seen.into_iter().map(|p| (p.owner_pid, p.pgids)).collect();
    assert_eq!(
        seen,
        vec![(
            std::process::id(),
            vec![proxies.into_inner().unwrap()[0], tunnel]
        )],
        "the proxy and the tunnel, before the tunnel's wait"
    );
    assert!(
        fx.state().worktrees[&name].pending_shares.is_empty(),
        "and nothing once the share is recorded"
    );
}

/// A provider that publishes at once, and reads back what state says is
/// pending for the worktree the moment its tunnel exists.
struct PendingWatcher<'a> {
    paths: &'a PandoPaths,
    seen: &'a std::sync::Mutex<Vec<PendingShare>>,
}

impl tunnel::Provider for PendingWatcher<'_> {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        Ok(())
    }
    fn start(
        &self,
        paths: &PandoPaths,
        name: &str,
        _: &str,
        _: u16,
        spawned: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        let log_path = paths.log_file(name, tunnel::TUNNEL_LOG);
        let spawn = proc::spawn_detached(SpawnOptions {
            shell_cmd: "exec sleep 300",
            cwd: &std::env::temp_dir(),
            log_file: &log_path,
            env: &[],
            status_file: None,
        })?;
        spawned(spawn.pgid);
        let store = state::load(&self.paths.state_file())?;
        *self.seen.lock().unwrap() = store.worktrees[name].pending_shares.clone();
        Ok(tunnel::TunnelSpawn {
            pid: spawn.pid,
            pgid: spawn.pgid,
            public_url: FAKE_TUNNEL_URL.to_string(),
            log_path,
            unconnected: None,
        })
    }
}

#[test]
fn a_share_that_fails_leaves_nothing_pending() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    share_with(
        &fx.paths,
        &config,
        &name,
        &FailingProvider,
        &stub_proxy,
        &noop,
    )
    .unwrap_err();
    assert!(fx.state().worktrees[&name].pending_shares.is_empty());
}

/// A provider whose tunnels publish only once the test opens the gate,
/// each at a URL of its own, and which counts how many it was asked for.
struct GatedProvider {
    started: std::sync::atomic::AtomicUsize,
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl tunnel::Provider for GatedProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }
    fn ensure_present(&self, _: &PandoPaths) -> Result<()> {
        Ok(())
    }
    fn start(
        &self,
        paths: &PandoPaths,
        name: &str,
        _: &str,
        _: u16,
        spawned: &dyn Fn(i32),
    ) -> Result<tunnel::TunnelSpawn> {
        let n = self
            .started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let open = self.open.lock().unwrap();
        drop(self.opened.wait_while(open, |open| !*open).unwrap());
        let log_path = paths.log_file(name, tunnel::TUNNEL_LOG);
        let spawn = proc::spawn_detached(SpawnOptions {
            shell_cmd: "exec sleep 300",
            cwd: &std::env::temp_dir(),
            log_file: &log_path,
            env: &[],
            status_file: None,
        })?;
        spawned(spawn.pgid);
        Ok(tunnel::TunnelSpawn {
            pid: spawn.pid,
            pgid: spawn.pgid,
            public_url: format!("https://tunnel-{n}.trycloudflare.com"),
            log_path,
            unconnected: None,
        })
    }
}

// Every tunnel of a worktree writes one log, and each wait takes the
// first URL in it: two shares at once could both read one tunnel's URL,
// and the one that recorded second closed the tunnel serving it. The
// TUI's `t` and an agent's `pando share` of one worktree is all it takes.
#[test]
fn a_share_while_another_of_the_worktree_is_opening_waits_and_answers_with_its_url() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let provider = GatedProvider {
        started: std::sync::atomic::AtomicUsize::new(0),
        open: std::sync::Mutex::new(false),
        opened: std::sync::Condvar::new(),
    };
    let said = std::sync::Mutex::new(Vec::<String>::new());
    let share_of = |progress: &(dyn Fn(&str) + Sync)| {
        share_with(
            &fx.paths,
            &fx.config,
            &name,
            &provider,
            &stub_proxy,
            progress,
        )
    };
    let started = || provider.started.load(std::sync::atomic::Ordering::SeqCst);
    // The gate opens whatever was seen, so a failed wait is a failed
    // assertion rather than two threads blocked for good.
    let (asked, reached, first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| share_of(&noop));
        let asked = wait_until(Duration::from_secs(20), || started() == 1);
        let second = scope.spawn(|| share_of(&|m: &str| said.lock().unwrap().push(m.to_string())));
        // Until the second share is either waiting or has asked for a
        // tunnel of its own, whichever this code does.
        let reached = asked
            && wait_until(Duration::from_secs(20), || {
                started() > 1 || said.lock().unwrap().iter().any(|m| m.contains("waiting"))
            });
        *provider.open.lock().unwrap() = true;
        provider.opened.notify_all();
        (
            asked,
            reached,
            first.join().unwrap(),
            second.join().unwrap(),
        )
    });
    let _share = share_guard(&fx, &name);
    assert!(asked, "the first share never asked for its tunnel");
    assert!(
        reached,
        "the second share neither waited nor opened a tunnel"
    );

    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(
        provider.started.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one tunnel for one worktree"
    );
    assert!(!first.already);
    assert!(second.already, "{second:?}");
    assert_eq!(second.public_url, first.public_url);
    let record = fx.state().worktrees[&name].share.clone().unwrap();
    assert_eq!(record.public_url, first.public_url);
    assert!(
        crate::process::is_alive(record.tunnel_pid),
        "the recorded tunnel is the one still serving its URL"
    );
}

/// A worktree with a share of `owner`'s still coming up, which has
/// spawned the groups `pgids`.
fn state_with_a_pending_share(owner: u32, pgids: Vec<i32>) -> state::State {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.pending_shares.push(PendingShare {
        owner_pid: owner,
        since: Utc::now(),
        pgids,
    });
    store.worktrees.insert("feat+one".to_string(), record);
    store
}

#[test]
fn a_share_whose_pando_died_before_its_tunnel_was_up_is_stopped_by_the_sweep() {
    let mut store = state_with_a_pending_share(4_000_001, vec![4242, 8484]);
    let signalled = std::sync::Mutex::new(Vec::new());
    let notices = sweep_dead_shares_with(
        &mut store,
        |pid| pid != 4_000_001,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );
    assert_eq!(signalled.into_inner().unwrap(), vec![4242, 8484]);
    assert!(store.worktrees["feat+one"].pending_shares.is_empty());
    assert!(notices[0].contains("interrupted"), "{notices:?}");
}

#[test]
fn a_share_still_coming_up_in_a_live_pando_is_left_to_it() {
    let mut store = state_with_a_pending_share(4_000_001, vec![4242]);
    let notices =
        sweep_dead_shares_with(&mut store, |_| true, |_| true, |_| bail!("must not signal"));
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(store.worktrees["feat+one"].pending_shares.len(), 1);
}

// A pid alone is no proof its pando is still waiting: once that pando is
// gone the system can hand the pid to anything, and a long-lived process
// that got it kept an orphaned tunnel, and a proxy holding the auth
// cookie, up for good.
#[test]
fn a_share_pending_for_longer_than_any_share_waits_is_stopped_whoever_has_its_pid() {
    let mut store = state_with_a_pending_share(4_000_001, vec![4242, 8484]);
    store.worktrees.get_mut("feat+one").unwrap().pending_shares[0].since =
        Utc::now() - chrono::TimeDelta::minutes(10);
    let signalled = std::sync::Mutex::new(Vec::new());
    let notices = sweep_dead_shares_with(
        &mut store,
        |_| true,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );
    assert_eq!(signalled.into_inner().unwrap(), vec![4242, 8484]);
    assert!(store.worktrees["feat+one"].pending_shares.is_empty());
    assert!(notices[0].contains("interrupted"), "{notices:?}");
}

// Nothing is left to stop, and the pgids may name somebody else's groups
// by the time a sweep comes round.
#[test]
fn a_pending_share_whose_groups_have_all_exited_is_dropped_without_a_signal() {
    let mut store = state_with_a_pending_share(4_000_001, vec![4242, 8484]);
    let notices = sweep_dead_shares_with(
        &mut store,
        |pid| pid != 4_000_001,
        |_| false,
        |_| bail!("must not signal"),
    );
    assert!(store.worktrees["feat+one"].pending_shares.is_empty());
    assert!(notices.is_empty(), "{notices:?}");

    // And only the groups still alive are signalled.
    let mut store = state_with_a_pending_share(4_000_001, vec![4242, 8484]);
    let signalled = std::sync::Mutex::new(Vec::new());
    sweep_dead_shares_with(
        &mut store,
        |pid| pid != 4_000_001,
        |pgid| pgid == 8484,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );
    assert_eq!(signalled.into_inner().unwrap(), vec![8484]);
}

#[test]
fn an_interrupted_share_that_will_not_stop_keeps_its_groups_named() {
    let mut store = state_with_a_pending_share(4_000_001, vec![4242]);
    let notices = sweep_dead_shares_with(&mut store, |_| false, |_| true, |_| bail!("stuck"));
    assert_eq!(store.worktrees["feat+one"].pending_shares.len(), 1);
    assert!(notices[0].contains("would not stop"), "{notices:?}");
}

// The real cloudflared fails the same way, through the same path.
#[test]
fn a_provider_that_exits_fails_the_share_and_records_nothing() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    fake_cloudflared_failing(&fx.paths.home);
    let err = share(&fx.paths, &fx.config, &name, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("Too Many Requests"),
        "the provider's own complaint is the diagnosis: {err:#}"
    );
    assert!(fx.state().worktrees[&name].share.is_none());
}

// Finding 2 end to end: a cloudflared that cannot even *reach*
// Cloudflare logs the quick-tunnel API's own URL, and used to be
// reported as a successful share at `https://api.trycloudflare.com`,
// with a proxy left running behind a tunnel that was already dead.
#[test]
fn a_provider_that_cannot_reach_cloudflare_fails_the_share_and_leaves_no_proxy() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    crate::testutil::fake_cloudflared_api_error(&fx.paths.home);
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());

    let spawned: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
    let watched = |paths: &PandoPaths,
                   name: &str,
                   listen: u16,
                   upstream: u16,
                   cookie: &str|
     -> Result<share_proxy::ProxySpawn> {
        let spawn = stub_proxy(paths, name, listen, upstream, cookie)?;
        spawned.lock().unwrap().push(spawn.pid);
        Ok(spawn)
    };
    let provider = tunnel::provider_for(None).unwrap();

    let err = share_with(
        &fx.paths,
        &config,
        &name,
        provider.as_ref(),
        &watched,
        &noop,
    )
    .unwrap_err();

    let message = format!("{err:#}");
    assert!(
        message.contains("before publishing a URL"),
        "a request that failed is not a published URL: {message}"
    );
    assert!(
        message.contains("failed to request quick Tunnel"),
        "and the provider's own complaint is the diagnosis: {message}"
    );
    assert!(
        fx.state().worktrees[&name].share.is_none(),
        "nothing may be recorded for a share that never published"
    );
    let pids = spawned.into_inner().unwrap();
    assert_eq!(pids.len(), 1, "a proxy was started before the tunnel");
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::is_alive(
            pids[0]
        )),
        "a proxy was left behind a tunnel that never opened"
    );
}

// Finding 4 through the wiring rather than the primitive: an auth
// command that backgrounds a helper — an ordinary thing for a session
// minting script to do — left its stdout open after the shell exited,
// and `share` waited for that helper rather than for its own timeout.
// `sleep 45 &` cost 47s; `sleep 3600 &` cost an hour, in the TUI as a
// pending slot nothing could clear.
#[test]
fn an_auth_command_that_backgrounds_a_helper_is_bounded_by_its_own_timeout() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let pidfile = fx.paths.home.join("auth-child.pid");
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some(format!(
        "printf 'session=abc'; sleep 45 & echo $! > {}",
        pidfile.display()
    ));

    let started = std::time::Instant::now();
    let outcome = share_stubbed(&fx, &config, &name).unwrap();
    let elapsed = started.elapsed();
    let _share = share_guard(&fx, &name);

    assert!(
        elapsed < AUTH_CMD_TIMEOUT + Duration::from_secs(10),
        "the share waited for a helper the script backgrounded: {elapsed:?}"
    );
    assert!(
        outcome.pre_authed,
        "the cookie it printed is still the cookie"
    );

    let child: u32 = std::fs::read_to_string(&pidfile)
        .expect("the script wrote its child's pid")
        .trim()
        .parse()
        .expect("a pid");
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::is_alive(child)),
        "the helper outlived the auth command that started it"
    );
}

// The Phase 3 critical, at this level: sharing must never move a port
// the running application is being reached on.
#[test]
fn sharing_and_restarting_leave_every_port_where_it_was() {
    let Some((fx, name, guards, report)) = shared_fixture() else {
        return;
    };
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    share_stubbed(&fx, &config, &name).unwrap();
    let _share = share_guard(&fx, &name);

    assert_eq!(
        fx.state().worktrees[&name].ports,
        report.ports,
        "a share must not touch the application's ports"
    );

    drop(guards);
    stop(&fx.paths, &name, None).unwrap();
    let again = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _again = guard(&again);
    assert_eq!(
        again.ports, report.ports,
        "and neither must the start after it"
    );
    assert!(!again.reassigned);
}

// ---- a share that outlives what it points at -------------------------

/// A share record whose two halves have the given pids.
fn share_record_of(tunnel_pid: u32, proxy_pid: Option<u32>) -> ShareRecord {
    ShareRecord {
        tunnel_pid,
        tunnel_pgid: tunnel_pid as i32,
        public_url: "https://x.trycloudflare.com".into(),
        local_port: 17000,
        started_at: Utc::now(),
        log_path: PathBuf::from("tunnel.log"),
        proxy_pid,
        proxy_pgid: proxy_pid.map(|p| p as i32),
        proxy_port: proxy_pid.map(|_| 17005),
    }
}

fn state_with_share(share: ShareRecord) -> state::State {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.share = Some(share);
    store.worktrees.insert("feat+one".to_string(), record);
    store
}

#[test]
fn a_dead_tunnel_closes_the_share_and_signals_the_proxy_that_is_left() {
    let mut store = state_with_share(share_record_of(4242, Some(8484)));
    let signalled = std::sync::Mutex::new(Vec::new());

    let notices = sweep_dead_shares_with(
        &mut store,
        // The tunnel died; the proxy is still up.
        |pid| pid == 8484,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );

    assert_eq!(
        signalled.into_inner().unwrap(),
        vec![4242, 8484],
        "both groups are signalled before the record that names them is dropped"
    );
    assert!(store.worktrees["feat+one"].share.is_none());
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("tunnel"), "{:?}", notices[0]);
    assert!(notices[0].contains("feat+one"), "{:?}", notices[0]);
}

#[test]
fn a_dead_proxy_takes_the_tunnel_in_front_of_it_down() {
    let mut store = state_with_share(share_record_of(4242, Some(8484)));
    let signalled = std::sync::Mutex::new(Vec::new());

    let notices = sweep_dead_shares_with(
        &mut store,
        // The proxy died; the tunnel is still up, serving the login
        // screen the proxy existed to skip.
        |pid| pid == 4242,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );

    assert_eq!(signalled.into_inner().unwrap(), vec![4242, 8484]);
    assert!(store.worktrees["feat+one"].share.is_none());
    assert!(notices[0].contains("proxy"), "{:?}", notices[0]);
}

#[test]
fn a_live_share_is_left_alone() {
    // With the application it publishes still up: a share pointing at
    // a worktree that is running nothing is closed, which is its own
    // test further down.
    let mut store = state_with_a_shared_application(Phase::Running { since: Utc::now() }, 777);
    let signalled = std::sync::Mutex::new(Vec::new());
    let notices = sweep_dead_shares_with(
        &mut store,
        |_| true,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );
    assert!(signalled.into_inner().unwrap().is_empty());
    assert!(notices.is_empty());
    assert!(store.worktrees["feat+one"].share.is_some());
}

// The record holds the only pgid anything can use to try again, so a
// half that will not die keeps it.
#[test]
fn a_share_that_will_not_die_keeps_its_record_and_says_so() {
    let mut store = state_with_share(share_record_of(4242, Some(8484)));
    let notices = sweep_dead_shares_with(
        &mut store,
        |pid| pid == 8484,
        |_| true,
        |_| bail!("would not stop"),
    );

    assert!(
        store.worktrees["feat+one"].share.is_some(),
        "dropping it would leave a tunnel nothing can name"
    );
    assert!(notices[0].contains("unshare"), "{:?}", notices[0]);
}

/// A worktree with one `dev` process owning `web`, shared, whose
/// process is in `phase`.
fn state_with_a_shared_application(phase: Phase, pid: u32) -> state::State {
    let mut store = state_with_share(share_record_of(4242, Some(8484)));
    let record = store.worktrees.get_mut("feat+one").unwrap();
    record.ports.insert("web".to_string(), 17000);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    let mut process = fake_record(pid as i32);
    process.phase = phase;
    record.processes.insert("dev".to_string(), process);
    store
}

// Finding 5. `stop` and `rm` unshare first, so a *stopped* worktree
// never keeps a public URL — but a crashed one did, and the proxy in
// front of it kept injecting the auth cookie into every request aimed
// at a port whose owner was gone.
#[test]
fn a_share_whose_application_crashed_is_closed_and_both_halves_signalled() {
    let mut store = state_with_a_shared_application(Phase::Running { since: Utc::now() }, 777);
    let signalled = std::sync::Mutex::new(Vec::new());

    let notices = sweep_dead_shares_with(
        &mut store,
        // Both halves of the share are up; the application is not.
        |pid| pid != 777,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );

    assert_eq!(
        signalled.into_inner().unwrap(),
        vec![4242, 8484],
        "a public URL onto nothing is worse than no public URL"
    );
    assert!(store.worktrees["feat+one"].share.is_none());
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("public URL is closed"),
        "{:?}",
        notices[0]
    );
}

#[test]
fn a_share_whose_application_is_still_up_is_left_alone() {
    let mut store = state_with_a_shared_application(Phase::Running { since: Utc::now() }, 777);
    let notices = sweep_dead_shares_with(&mut store, |_| true, |_| true, |_| Ok(()));
    assert!(notices.is_empty(), "{notices:?}");
    assert!(store.worktrees["feat+one"].share.is_some());
}

// A worktree that is still coming up is not a worktree with nothing
// serving: tearing its share down would be the same mistake inverted.
#[test]
fn a_share_of_something_still_starting_is_left_alone() {
    let mut store = state_with_a_shared_application(Phase::Starting { since: Utc::now() }, 777);
    let notices = sweep_dead_shares_with(&mut store, |_| true, |_| true, |_| Ok(()));
    assert!(notices.is_empty(), "{notices:?}");
    assert!(store.worktrees["feat+one"].share.is_some());
}

#[test]
fn refresh_closes_a_share_whose_application_crashed_rather_than_stopped() {
    let Some((fx, name, guards, _)) = shared_fixture() else {
        return;
    };
    share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();
    let _cleanup = ShareGuard(Some(record.clone()));

    // A crash, not a `stop`: nothing tells pando, and nothing unshares.
    drop(guards);

    let refreshed = refresh(&fx.paths);
    assert!(
        refreshed.state.worktrees[&name].share.is_none(),
        "a crashed application left its public URL open"
    );
    assert!(
        refreshed
            .notices
            .iter()
            .any(|n| n.contains("public URL is closed")),
        "{:?}",
        refreshed.notices
    );
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::group_alive(
            record.tunnel_pgid
        )),
        "the tunnel onto nothing was left running"
    );
}

// Finding 8. One message for three different states, and for the one
// that matters most — a worktree `start` returned from a moment ago —
// it was advice the developer had just followed.
#[test]
fn a_share_refusal_names_the_state_it_found() {
    let mut base = WorktreeRecord::new("/tmp/feat+one", true);
    base.ports.insert("web".to_string(), 17000);
    base.roles
        .insert("dev".to_string(), vec!["web".to_string()]);

    let nothing = format!("{:#}", share_target_port("feat+one", &base).unwrap_err());
    assert!(nothing.contains("start it first"), "{nothing}");

    let mut starting = base.clone();
    let mut process = fake_record(900);
    process.phase = Phase::Starting { since: Utc::now() };
    starting.processes.insert("dev".to_string(), process);
    let message = format!(
        "{:#}",
        share_target_port("feat+one", &starting).unwrap_err()
    );
    assert!(
        message.contains("still starting"),
        "a worktree that is coming up is not one that was never started: {message}"
    );

    let mut failed = base.clone();
    let mut process = fake_record(900);
    process.phase = Phase::Failed {
        at: Utc::now(),
        reason: "timeout: nothing bound port 17000 in 30s".to_string(),
    };
    failed.processes.insert("dev".to_string(), process);
    let message = format!("{:#}", share_target_port("feat+one", &failed).unwrap_err());
    assert!(
        message.contains("nothing bound port 17000"),
        "a failure says what failed: {message}"
    );
}

#[test]
fn a_share_without_a_proxy_is_swept_on_its_tunnel_alone() {
    let mut store = state_with_share(share_record_of(4242, None));
    let signalled = std::sync::Mutex::new(Vec::new());
    sweep_dead_shares_with(
        &mut store,
        |_| false,
        |_| true,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
    );
    assert_eq!(signalled.into_inner().unwrap(), vec![4242]);
    assert!(store.worktrees["feat+one"].share.is_none());
}

#[test]
fn refresh_closes_a_share_whose_tunnel_died_and_says_so_once() {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();
    crate::process::stop(record.tunnel_pgid, Duration::from_secs(5)).unwrap();

    let refreshed = refresh(&fx.paths);
    assert!(refreshed.state.worktrees[&name].share.is_none());
    assert_eq!(refreshed.notices.len(), 1, "{:?}", refreshed.notices);
    assert!(refreshed.notices[0].contains("public URL is closed"));

    // And the record is gone from disk, so the next refresh has
    // nothing to repeat.
    assert!(refresh(&fx.paths).notices.is_empty());
}

// ---- a dead half of a share, on every path that drops records --------
//
// `reconcile` drops the record that holds the surviving half's pgid, and
// it cannot signal anything. So every path that reaches it has to signal
// first. `refresh` and `share` did; `start`, `stop <name>` and `stop`
// did not, and a live cloudflared with a public URL — or a live proxy
// holding the injected cookie in its environment — was left running with
// nothing in pando able to name it again.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeadHalf {
    Tunnel,
    Proxy,
}

/// Shares `name` with a proxy in front of it, then kills `dead` outright,
/// leaving the other half running and only the record naming it.
fn share_with_a_dead_half(fx: &Fx, name: &str, dead: DeadHalf) -> ShareRecord {
    let mut config = fx.config.clone();
    config.share.auth_cmd = Some("printf 'session=abc'".to_string());
    share_stubbed(fx, &config, name).unwrap();
    let record = fx.state().worktrees[name].share.clone().unwrap();
    let (pid, pgid) = match dead {
        DeadHalf::Tunnel => (record.tunnel_pid, record.tunnel_pgid),
        DeadHalf::Proxy => (record.proxy_pid.unwrap(), record.proxy_pgid.unwrap()),
    };
    crate::process::stop(pgid, Duration::from_secs(5)).unwrap();
    assert!(
        !crate::process::is_alive(pid),
        "the {dead:?} half should be dead"
    );
    assert!(
        crate::process::group_alive(surviving_pgid(&record, dead)),
        "the other half has to still be running, or this test proves nothing"
    );
    record
}

fn surviving_pgid(record: &ShareRecord, dead: DeadHalf) -> i32 {
    match dead {
        DeadHalf::Tunnel => record.proxy_pgid.unwrap(),
        DeadHalf::Proxy => record.tunnel_pgid,
    }
}

fn assert_nothing_of_the_share_is_left(record: &ShareRecord, dead: DeadHalf) {
    let pgid = surviving_pgid(record, dead);
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::group_alive(
            pgid
        )),
        "the {dead:?} half died and the other one was left running in group {pgid}, \
         with the record that named it dropped"
    );
}

fn assert_the_caller_was_told(notices: &[String], name: &str) {
    assert!(
        notices
            .iter()
            .any(|n| n.contains(name) && n.contains("public URL is closed")),
        "the caller must be told the URL it had is gone: {notices:?}"
    );
}

#[test]
fn a_start_of_another_worktree_signals_the_survivor_of_a_dead_tunnel() {
    a_start_of_another_worktree_signals_the_survivor(DeadHalf::Tunnel);
}

#[test]
fn a_start_of_another_worktree_signals_the_survivor_of_a_dead_proxy() {
    a_start_of_another_worktree_signals_the_survivor(DeadHalf::Proxy);
}

fn a_start_of_another_worktree_signals_the_survivor(dead: DeadHalf) {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    // Created before the share dies: `new` reaches the same chokepoint,
    // and this test is about what `start` does.
    let other = worktree_named(&fx, "feat/two");
    let record = share_with_a_dead_half(&fx, &name, dead);
    let _cleanup = ShareGuard(Some(record.clone()));

    let said = std::sync::Mutex::new(Vec::<String>::new());
    let report = start(&fx.paths, &fx.config, &other, None, &|line| {
        said.lock().unwrap().push(line.to_string())
    })
    .unwrap();
    let _others = guard(&report);

    assert!(fx.state().worktrees[&name].share.is_none());
    assert_nothing_of_the_share_is_left(&record, dead);
    assert_the_caller_was_told(&said.into_inner().unwrap(), &name);
}

#[test]
fn a_stop_of_another_worktree_signals_the_survivor_of_a_dead_tunnel() {
    a_stop_of_another_worktree_signals_the_survivor(DeadHalf::Tunnel);
}

#[test]
fn a_stop_of_another_worktree_signals_the_survivor_of_a_dead_proxy() {
    a_stop_of_another_worktree_signals_the_survivor(DeadHalf::Proxy);
}

fn a_stop_of_another_worktree_signals_the_survivor(dead: DeadHalf) {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let other = worktree_named(&fx, "feat/two");
    let record = share_with_a_dead_half(&fx, &name, dead);
    let _cleanup = ShareGuard(Some(record.clone()));

    let said = std::sync::Mutex::new(Vec::<String>::new());
    super::stop(&fx.paths, &other, None, &|line| {
        said.lock().unwrap().push(line.to_string())
    })
    .unwrap();

    assert!(fx.state().worktrees[&name].share.is_none());
    assert_nothing_of_the_share_is_left(&record, dead);
    assert_the_caller_was_told(&said.into_inner().unwrap(), &name);
}

#[test]
fn a_bare_stop_signals_the_survivor_of_a_dead_tunnel() {
    a_bare_stop_signals_the_survivor(DeadHalf::Tunnel);
}

#[test]
fn a_bare_stop_signals_the_survivor_of_a_dead_proxy() {
    a_bare_stop_signals_the_survivor(DeadHalf::Proxy);
}

// No notice is asserted here: a bare `stop` stops the shared worktree
// itself, so its share comes down as part of stopping it — which is the
// documented behaviour and not news. What has to hold either way is
// that neither half is left running.
fn a_bare_stop_signals_the_survivor(dead: DeadHalf) {
    let Some((fx, name, _guards, _)) = shared_fixture() else {
        return;
    };
    let record = share_with_a_dead_half(&fx, &name, dead);
    let _cleanup = ShareGuard(Some(record.clone()));

    stop_all(&fx.paths).unwrap();

    assert!(fx.state().worktrees[&name].share.is_none());
    assert_nothing_of_the_share_is_left(&record, dead);
}

#[test]
fn stop_takes_the_public_url_down_with_the_worktree() {
    let Some((fx, name, guards, _)) = shared_fixture() else {
        return;
    };
    share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();

    drop(guards);
    stop(&fx.paths, &name, None).unwrap();

    assert!(
        fx.state().worktrees[&name].share.is_none(),
        "a stopped worktree never keeps a public URL"
    );
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(record.tunnel_pid)
    }));
}

#[test]
fn rm_takes_the_public_url_down_with_the_worktree() {
    let Some((fx, name, guards, _)) = shared_fixture() else {
        return;
    };
    share(&fx.paths, &fx.config, &name, &noop).unwrap();
    let record = fx.state().worktrees[&name].share.clone().unwrap();

    drop(guards);
    rm(&fx.paths, &name, false, true).unwrap();

    assert!(!fx.state().worktrees.contains_key(&name));
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(record.tunnel_pid)
    }));
}

// `--only` is about one process. Its siblings are still serving, so the
// URL still points at something.
#[test]
fn stopping_one_process_of_several_leaves_the_share_up() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.processes.insert("web".into(), fake_record(100));
    record.processes.insert("api".into(), fake_record(200));
    record.share = Some(share_record_of(4242, None));
    store.worktrees.insert("feat+one".into(), record);

    let mut projects = Vec::new();
    stop_recorded_with(
        &mut store,
        "feat+one",
        Some("api"),
        |_| Ok(()),
        &mut projects,
    )
    .unwrap();

    assert!(
        store.worktrees["feat+one"].share.is_some(),
        "the web process is still serving what the URL points at"
    );
}

// …and a `--only` stop of the last one does take it down: a tunnel onto
// nothing is worse than no tunnel.
#[test]
fn stopping_the_last_process_takes_the_share_down_even_with_only() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.processes.insert("web".into(), fake_record(100));
    record.share = Some(share_record_of(4242, Some(8484)));
    store.worktrees.insert("feat+one".into(), record);

    let signalled = std::sync::Mutex::new(Vec::new());
    let mut projects = Vec::new();
    stop_recorded_with(
        &mut store,
        "feat+one",
        Some("web"),
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
        &mut projects,
    )
    .unwrap();

    assert!(store.worktrees["feat+one"].share.is_none());
    let signalled = signalled.into_inner().unwrap();
    assert!(
        signalled.contains(&4242) && signalled.contains(&8484),
        "{signalled:?}"
    );
}

/// A shared worktree whose `web` process owns the URL and whose `api`
/// process owns a port of its own, with only the processes named up.
fn shared_web_and_api(running: &[&str]) -> state::State {
    let mut store = state_with_share(share_record_of(4242, Some(8484)));
    let record = store.worktrees.get_mut("feat+one").unwrap();
    for (process, port) in [("web", 17000), ("api", 17001)] {
        record.ports.insert(process.to_string(), port);
        record
            .roles
            .insert(process.to_string(), vec![process.to_string()]);
    }
    for (pid, process) in running.iter().enumerate() {
        record
            .processes
            .insert(process.to_string(), fake_record(100 + pid as i32));
    }
    store
}

// `roles` and `ports` are written for every process at every start, and
// a stopped process leaves `processes`. So a URL whose owner was stopped
// on its own looked like a record that names no owner, and the api still
// running kept a public URL up onto a port nothing listened on.
#[test]
fn stopping_the_process_the_url_points_at_takes_the_share_down_even_with_only() {
    let mut store = shared_web_and_api(&["web", "api"]);
    let mut projects = Vec::new();
    stop_recorded_with(
        &mut store,
        "feat+one",
        Some("web"),
        |_| Ok(()),
        &mut projects,
    )
    .unwrap();
    assert!(
        store.worktrees["feat+one"].share.is_none(),
        "the api is up, but the URL is the web process's port"
    );

    let mut store = shared_web_and_api(&["web", "api"]);
    stop_recorded_with(
        &mut store,
        "feat+one",
        Some("api"),
        |_| Ok(()),
        &mut projects,
    )
    .unwrap();
    assert!(
        store.worktrees["feat+one"].share.is_some(),
        "and stopping what the URL does not point at leaves it up"
    );
}

#[test]
fn a_share_of_a_worktree_not_running_the_urls_owner_is_refused_by_name() {
    let store = shared_web_and_api(&["api"]);
    let record = &store.worktrees["feat+one"];
    let message = format!("{:#}", share_target_port("feat+one", record).unwrap_err());
    assert!(
        message.contains("not running web"),
        "a `start --only api` has nothing on the URL's port: {message}"
    );
}

#[test]
fn a_share_whose_urls_owner_was_stopped_on_its_own_is_closed() {
    let mut store = shared_web_and_api(&["api"]);
    let notices = sweep_dead_shares_with(&mut store, |_| true, |_| true, |_| Ok(()));
    assert!(store.worktrees["feat+one"].share.is_none());
    assert!(
        notices[0].contains("nothing is serving"),
        "{:?}",
        notices[0]
    );
}

#[test]
fn a_share_does_not_wait_on_a_sibling_of_the_urls_owner() {
    let mut store = shared_web_and_api(&[]);
    let mut api = fake_record(200);
    api.phase = Phase::Starting { since: Utc::now() };
    store
        .worktrees
        .get_mut("feat+one")
        .unwrap()
        .processes
        .insert("api".to_string(), api);
    assert_eq!(
        share_ready_budget(&store, "feat+one"),
        None,
        "the api coming up is not the web process coming up"
    );
}

// A worktree whose every process crashed still has a tunnel up.
#[test]
fn stopping_a_worktree_with_nothing_running_still_closes_its_share() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.share = Some(share_record_of(4242, None));
    store.worktrees.insert("feat+one".into(), record);

    let signalled = std::sync::Mutex::new(Vec::new());
    let mut projects = Vec::new();
    let outcome = stop_recorded_with(
        &mut store,
        "feat+one",
        None,
        |pgid| {
            signalled.lock().unwrap().push(pgid);
            Ok(())
        },
        &mut projects,
    )
    .unwrap();

    assert!(matches!(outcome, StopOutcome::Stopped(_)), "{outcome:?}");
    assert_eq!(signalled.into_inner().unwrap(), vec![4242]);
    assert!(store.worktrees["feat+one"].share.is_none());
}

#[test]
fn a_share_that_will_not_stop_fails_the_stop_and_keeps_its_record() {
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new("/tmp/feat+one", true);
    record.share = Some(share_record_of(4242, None));
    store.worktrees.insert("feat+one".into(), record);

    let mut projects = Vec::new();
    let err = stop_recorded_with(
        &mut store,
        "feat+one",
        None,
        |_| bail!("would not stop"),
        &mut projects,
    )
    .unwrap_err();

    assert!(format!("{err:#}").contains("share"), "{err:#}");
    assert!(store.worktrees["feat+one"].share.is_some());
}

// ---- restart ---------------------------------------------------------

#[test]
fn restart_stops_the_old_process_and_keeps_the_ports() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let first = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let first_pgid = first.started[0].record.pgid;
    let ports = first.ports.clone();
    drop(guard(&first));

    let second = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&second);
    assert!(!second.started_nothing());
    assert_ne!(second.started[0].record.pid, first.started[0].record.pid);
    assert!(!crate::process::group_alive(first_pgid));
    assert_eq!(
        second.ports, ports,
        "a restart keeps the URL the developer had open"
    );
    assert!(!second.reassigned);
}

#[test]
fn restarting_something_that_was_never_started_just_starts_it() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(!outcome.started_nothing());
}

// ---- the install hook ------------------------------------------------

/// A fixture with a lockfile, so the install hook has a fingerprint.
fn installable_fixture(install: &str) -> Fx {
    let mut fx = fixture();
    std::fs::write(fx.root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "lock"]);
    fx.config.project.install = Some(install.to_string());
    fx
}

fn install_log(fx: &Fx, name: &str) -> String {
    std::fs::read_to_string(fx.paths.log_file(name, INSTALL_HOOK)).unwrap_or_default()
}

fn install_fingerprint(fx: &Fx, name: &str) -> Option<String> {
    fx.state().worktrees[name]
        .hooks
        .get(INSTALL_HOOK)?
        .fingerprint
        .clone()
}

#[test]
fn the_install_hook_runs_after_new_and_records_its_fingerprint() {
    let fx = installable_fixture("echo installed-once");
    let name = worktree_named(&fx, "feat/one");
    assert!(install_log(&fx, &name).contains("installed-once"));
    let recorded = install_fingerprint(&fx, &name).expect("a fingerprint");
    assert!(recorded.starts_with("md5:"), "{recorded}");
}

// A plain install writes the gitignored lockfile itself, so the lockfile
// says nothing about whether the dependencies changed: the manifests do.
#[test]
fn a_plain_install_is_keyed_on_the_manifests_and_a_frozen_one_on_the_lockfiles() {
    let mut config = Config::default();
    config.project.install = Some("npm install".to_string());
    let plain = install_hook(&config).unwrap();
    assert_eq!(plain.fingerprint, vec!["**/package.json".to_string()]);
    // One that writes no lockfile at all has none to say so either.
    config.project.install = Some("bun install --no-save".to_string());
    let unsaved = install_hook(&config).unwrap();
    assert_eq!(unsaved.fingerprint, vec!["**/package.json".to_string()]);
    config.project.install = Some("npm ci".to_string());
    let frozen = install_hook(&config).unwrap();
    assert!(
        frozen
            .fingerprint
            .contains(&"package-lock.json".to_string())
    );
}

#[test]
fn the_install_hook_is_skipped_while_the_lockfile_is_unchanged() {
    let mut fx = installable_fixture("echo run");
    let name = worktree_named(&fx, "feat/one");
    assert_eq!(install_log(&fx, &name).matches("run").count(), 1);

    with_dev(&mut fx, dev("sleep 30"));
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(
        install_log(&fx, &name).matches("run").count(),
        1,
        "nothing changed, so nothing to install"
    );
}

// The case the fingerprint exists for: a branch with different
// dependencies, or a rebase that moved the lockfile under a worktree.
#[test]
fn the_install_hook_runs_again_when_the_lockfile_changes() {
    let mut fx = installable_fixture("echo run");
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let first = install_fingerprint(&fx, &name).unwrap();

    let worktree = fx.worktrees_dir().join(&name);
    std::fs::write(worktree.join("pnpm-lock.yaml"), "lockfileVersion: '10.0'\n").unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(
        install_log(&fx, &name).matches("run").count(),
        2,
        "a changed lockfile is what makes it run again"
    );
    assert_ne!(install_fingerprint(&fx, &name).unwrap(), first);
}

// The worktree is the expensive thing; the install is retryable. A
// failure reports itself and leaves everything else alone.
#[test]
fn a_failed_install_keeps_the_worktree_and_names_its_log() {
    let fx = installable_fixture("echo could-not-resolve >&2 && exit 1");
    let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("install"), "{msg}");
    assert!(msg.contains("could-not-resolve"), "{msg}");
    // What the TUI reads it by, and what gets the way past it appended.
    assert!(
        msg.starts_with(&format!("feat/one {CREATED_BUT_INSTALL_FAILED}")),
        "{msg}"
    );
    assert!(install_remedy(&fx.paths, &msg).is_some(), "{msg}");
    assert!(
        msg.contains("install.log"),
        "the log path is in the message: {msg}"
    );

    assert_eq!(
        fx.names(),
        vec!["feat+one".to_string()],
        "the worktree stays"
    );
    assert!(fx.worktrees_dir().join("feat+one").is_dir());
    assert!(
        install_fingerprint(&fx, "feat+one").is_none(),
        "a failed install records no fingerprint, so the next start retries"
    );
}

#[test]
fn a_failed_install_is_retried_by_the_next_start() {
    let mut fx = installable_fixture("exit 1");
    assert!(new(&fx.paths, &fx.config, "feat/one", None, &noop).is_err());
    fx.config.project.install = Some("echo recovered".to_string());
    with_dev(&mut fx, dev("sleep 30"));

    let outcome = start(&fx.paths, &fx.config, "feat+one", None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(install_log(&fx, "feat+one").contains("recovered"));
    assert!(install_fingerprint(&fx, "feat+one").is_some());
}

// Verified by hand against pnpm 9: `pnpm install --frozen-lockfile` on a
// lockfile it considers malformed rewrites it anyway. pando cannot stop
// that, but it must not then re-install on every start forever.
#[test]
fn an_install_that_rewrites_its_own_lockfile_still_settles() {
    let mut fx = installable_fixture("echo rewriting && echo changed >> pnpm-lock.yaml");
    with_dev(&mut fx, dev("sleep 30"));
    let notices = std::cell::RefCell::new(Vec::<String>::new());
    let record_notice = |m: &str| notices.borrow_mut().push(m.to_string());

    let name = new(&fx.paths, &fx.config, "feat/one", None, &record_notice).unwrap();
    assert_eq!(install_log(&fx, &name).matches("rewriting").count(), 1);
    assert!(
        notices.borrow().iter().any(|n| n.contains("not as frozen")),
        "a lockfile changing under a worktree is worth saying out loud: {:?}",
        notices.borrow()
    );

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(
        install_log(&fx, &name).matches("rewriting").count(),
        1,
        "the fingerprint recorded is the one the install left behind, so it settles"
    );
}

// On `start` for a worktree pando did not create there is no record yet,
// so a hook result written through `get_mut` would be dropped and the
// install would run again on every single start.
#[test]
fn the_install_hook_settles_on_an_adopted_worktree_too() {
    let mut fx = installable_fixture("echo run");
    with_dev(&mut fx, dev("sleep 30"));
    // Created by git, not by pando: no state record exists.
    let adopted = fx.worktrees_dir().join("adopted");
    std::fs::create_dir_all(fx.worktrees_dir()).unwrap();
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );
    assert!(!fx.state().worktrees.contains_key("adopted"));

    let first = start(&fx.paths, &fx.config, "adopted", None, &noop).unwrap();
    drop(guard(&first));
    assert_eq!(install_log(&fx, "adopted").matches("run").count(), 1);
    assert!(
        install_fingerprint(&fx, "adopted").is_some(),
        "the fingerprint is recorded even with no record to hang it on yet"
    );
    assert!(
        !fx.state().worktrees["adopted"].created_by_pando,
        "and pando does not claim a worktree it found"
    );

    stop(&fx.paths, "adopted", None).unwrap();
    let second = start(&fx.paths, &fx.config, "adopted", None, &noop).unwrap();
    let _guard = guard(&second);
    assert_eq!(
        install_log(&fx, "adopted").matches("run").count(),
        1,
        "nothing changed, so the install does not run again"
    );
}

// The hook's `PANDO_BRANCH` was the directory name and the dev
// process's was the git branch, so `git checkout "$PANDO_BRANCH"` in a
// hook checked out the wrong thing — or nothing at all.
#[test]
fn pando_branch_is_the_git_branch_for_hooks_as_well_as_processes() {
    let mut fx = fixture();
    fx.config.project.install = Some("echo INSTALL PANDO_BRANCH=$PANDO_BRANCH".to_string());
    with_dev(
        &mut fx,
        dev("echo DEV PANDO_BRANCH=$PANDO_BRANCH && sleep 30"),
    );
    let name = worktree_named(&fx, "feat/one");
    assert_eq!(name, "feat+one", "the directory name is the sanitised one");

    assert!(
        install_log(&fx, &name).contains("PANDO_BRANCH=feat/one"),
        "the hook gets the branch, not the directory: {:?}",
        install_log(&fx, &name)
    );

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        wait_until(Duration::from_secs(10), || log_of(&fx, &name)
            .contains("PANDO_BRANCH=")),
        "the process never logged its environment: {:?}",
        log_of(&fx, &name)
    );
    assert!(
        log_of(&fx, &name).contains("PANDO_BRANCH=feat/one"),
        "and both agree: {:?}",
        log_of(&fx, &name)
    );
}

#[test]
fn a_project_with_no_install_step_runs_no_hook() {
    let fx = fixture();
    let name = worktree_named(&fx, "feat/one");
    assert!(!fx.paths.log_file(&name, INSTALL_HOOK).exists());
    assert!(fx.state().worktrees[&name].hooks.is_empty());
}

// No lockfile means nothing can say the dependencies are unchanged, so
// the hook has to run every time rather than guess.
#[test]
fn an_install_with_nothing_to_fingerprint_runs_every_time() {
    let mut fx = fixture();
    fx.config.project.install = Some("echo run".to_string());
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(install_log(&fx, &name).matches("run").count(), 2);
}

// A version manager reads the worktree's own `.nvmrc`, so a branch that
// moves it runs everything under another runtime with the lockfile
// unchanged, and native modules built for the old one fail to load until
// the install runs again. A corrected prelude does the same.
#[test]
fn the_install_hook_runs_again_when_the_runtime_it_builds_for_changes() {
    let mut fx = installable_fixture("echo run");
    std::fs::write(fx.root.join(".nvmrc"), "20\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "pin"]);
    fx.config.runtime.version_files = vec![".nvmrc".to_string()];
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let runs = |fx: &Fx| install_log(fx, &name).matches("run").count();
    let start_and_stop = |fx: &Fx| {
        let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
        drop(guard(&outcome));
        stop(&fx.paths, &name, None).unwrap();
    };
    assert_eq!(runs(&fx), 1);

    start_and_stop(&fx);
    assert_eq!(runs(&fx), 1, "an unchanged pin is no reason to install");

    let worktree = fx.worktrees_dir().join(&name);
    std::fs::write(worktree.join(".nvmrc"), "22\n").unwrap();
    start_and_stop(&fx);
    assert_eq!(runs(&fx), 2, "a moved pin is");

    fx.config.runtime.prelude = Some("true".to_string());
    start_and_stop(&fx);
    assert_eq!(runs(&fx), 3, "and so is another prelude");
}

// The pins join a fingerprint and never make one: with no lockfile,
// nothing says the dependencies are unchanged, `.nvmrc` or not.
#[test]
fn a_runtime_pin_alone_does_not_let_an_install_be_skipped() {
    let mut fx = fixture();
    std::fs::write(fx.root.join(".nvmrc"), "20\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "pin"]);
    fx.config.project.install = Some("echo run".to_string());
    fx.config.runtime.version_files = vec![".nvmrc".to_string()];
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    assert!(install_fingerprint(&fx, &name).is_none());
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(install_log(&fx, &name).matches("run").count(), 2);
}

// `dir/` names the directory as surely as `dir` does, and the advice for
// it was `dir//**`: an empty segment matches no name, so following the
// advice kept the warning it answered.
#[test]
fn a_directory_written_with_a_trailing_slash_gets_advice_that_matches() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("prisma/migrations")).unwrap();
    std::fs::write(dir.path().join("prisma/migrations/0001.sql"), "select 1;\n").unwrap();
    let hook = crate::config::HookConfig {
        name: "migrate".to_string(),
        after: crate::config::HookPoint::Services,
        fingerprint: vec!["prisma/migrations/".to_string()],
        cmd: "true".to_string(),
        cwd: None,
        fallback: None,
        on: None,
    };
    let said = matched_nothing(dir.path(), &hook);
    assert!(said.contains("try prisma/migrations/**"), "{said}");
    assert!(!said.contains("//"), "{said}");
    assert!(
        !crate::hooks::matched(dir.path(), &["prisma/migrations/**".to_string()]).is_empty(),
        "and the advice is a glob that matches"
    );
}

// ---- questions -------------------------------------------------------

use crate::detect::Slot;

/// Questions a scripted `ask` was asked, shared with the test.
type AskedQuestions = std::rc::Rc<std::cell::RefCell<Vec<Question>>>;

/// An `ask` that answers from a script and records what it was asked.
fn scripted(answers: Vec<Answer>) -> (impl Fn(&Question) -> Result<Answer>, AskedQuestions) {
    let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = asked.clone();
    let answers = std::cell::RefCell::new(answers.into_iter());
    let ask = move |q: &Question| -> Result<Answer> {
        seen.borrow_mut().push(q.clone());
        answers
            .borrow_mut()
            .next()
            .ok_or_else(|| anyhow::anyhow!("asked more questions than the test scripted"))
    };
    (ask, asked)
}

fn refuse(_: &Question) -> Result<Answer> {
    panic!("nothing should have been asked")
}

/// A fixture with a package.json, a lockfile, and an env example — the
/// shape detection is built for.
fn detectable_fixture(scripts: &str, env_example: &str) -> Fx {
    let fx = fixture();
    std::fs::write(
        fx.root.join("package.json"),
        format!("{{\n  \"name\": \"x\",\n  \"scripts\": {scripts}\n}}\n"),
    )
    .unwrap();
    std::fs::write(fx.root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    std::fs::write(fx.root.join(".env.example"), env_example).unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "app"]);
    fx
}

/// A workspace fixture: two apps, each with its own dev script, and an
/// env example in which one points at the other.
fn workspace_fixture() -> Fx {
    let fx = detectable_fixture(
        r#"{ "dev": "pnpm -r --parallel dev" }"#,
        "WEB_PORT=5173\nAPI_PORT=4000\nVITE_API_URL=http://localhost:4000\n",
    );
    std::fs::write(
        fx.root.join("package.json"),
        "{\n  \"workspaces\": [\"apps/*\"],\n  \"scripts\": { \"dev\": \"pnpm -r --parallel dev\" }\n}\n",
    )
    .unwrap();
    for (dir, manifest) in [
        ("apps/web", r#"{ "scripts": { "dev": "vite" } }"#),
        ("apps/api", r#"{ "scripts": { "dev": "node server.js" } }"#),
    ] {
        std::fs::create_dir_all(fx.root.join(dir)).unwrap();
        std::fs::write(fx.root.join(dir).join("package.json"), manifest).unwrap();
    }
    std::fs::write(
        fx.root.join("apps/web/vite.config.ts"),
        "export default {}\n",
    )
    .unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "workspace"]);
    fx
}

#[test]
fn a_workspace_is_asked_about_once_and_yes_takes_the_per_app_form() {
    let fx = workspace_fixture();
    // What `--yes` does: the first option, recorded as a flag's choice
    // rather than a rule's.
    let (ask, asked) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

    let questions = asked.borrow();
    assert_eq!(questions.len(), 1, "one question, not one per app");
    assert_eq!(questions[0].slot, Slot::Processes);
    assert_eq!(questions[0].preselect, Some(0));
    assert_eq!(
        questions[0].options.len(),
        2,
        "the per-app form and the root script"
    );

    assert_eq!(
        config.processes.keys().cloned().collect::<Vec<_>>(),
        vec!["api", "web"]
    );
    assert_eq!(config.processes["web"].cwd.as_deref(), Some("apps/web"));
    assert_eq!(
        config.processes["web"].env["VITE_API_URL"],
        "http://localhost:{port:api}"
    );

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("[processes.web]"), "{written}");
    assert!(written.contains("[processes.api]"), "{written}");
    assert!(
        !written.contains("\n[dev]"),
        "nothing may write [dev] beside [processes]: {written}"
    );
    assert!(
        written.contains("--yes took the first of 2 options"),
        "a flag's choice says so: {written}"
    );
    // And the file pando wrote is one pando reads back.
    let loaded = crate::config::load(&fx.paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.config.processes.len(), 2);

    // Second run: nothing left to ask.
    let again = resolve_process(&fx.paths, &loaded.config, &refuse, &noop).unwrap();
    assert_eq!(again.processes.len(), 2);
}

/// The workspace above plus a third app whose `dev` script is not a
/// server. A `packages/*` library with `dev: "tsc -w"`, a codegen
/// watcher, a queue consumer: it has a dev script, it has no framework
/// rule, and the env example has no `WORKER_PORT` for it.
fn workspace_with_worker_fixture() -> Fx {
    let fx = workspace_fixture();
    std::fs::create_dir_all(fx.root.join("apps/worker")).unwrap();
    std::fs::write(
        fx.root.join("apps/worker/package.json"),
        "{ \"name\": \"worker\", \"scripts\": { \"dev\": \"tsc -w\" } }\n",
    )
    .unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "worker"]);
    fx
}

// Phase 2b review, finding 2. Detection wrote a role and a readiness
// rule for every app, including one with no way to be told which port
// the role stands for. The process was then handed a reserved port it
// never heard of, `advance_phases` waited thirty seconds for it, and a
// worktree whose every process was healthy read `failed`.
#[test]
fn a_workspace_app_with_no_way_to_be_told_a_port_gets_neither_a_role_nor_readiness() {
    let fx = workspace_with_worker_fixture();
    let (ask, _asked) = scripted(vec![Answer::Auto(0)]);
    let mut config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

    assert_eq!(
        config.processes.keys().cloned().collect::<Vec<_>>(),
        vec!["api", "web", "worker"]
    );
    let worker = &config.processes["worker"];
    assert_eq!(
        worker.ports,
        Some(PortsSpec::List(Vec::new())),
        "no framework rule, no flag and no WORKER_PORT key: it cannot be told a port"
    );
    assert!(
        worker.ready.is_none(),
        "and there is nothing for readiness to wait for: {:?}",
        worker.ready
    );
    // No port of its own. It is told where its siblings listen — a worker
    // that calls the api reads `API_PORT` like anything else does — and
    // every such variable carries another app's role, never one of its own.
    assert!(
        !worker.env.contains_key("PORT") && !worker.env.contains_key("WORKER_PORT"),
        "{:?}",
        worker.env
    );
    for (key, value) in worker.env.iter().filter(|(key, _)| key.ends_with("PORT")) {
        assert!(
            value == "{port:api}" || value == "{port:web}",
            "{key} = {value} is not a sibling's port"
        );
    }
    // The apps that *can* be told one still are.
    assert_eq!(config.processes["web"].roles(), vec!["web"]);
    assert_eq!(config.processes["api"].roles(), vec!["api"]);
    assert_eq!(
        config.processes["api"]
            .ready
            .as_ref()
            .and_then(|r| r.role.as_deref()),
        Some("api")
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("ports = []"),
        "`no ports` is an answer written down, not a gap: {written}"
    );

    // And a process with no port to wait for is Running as soon as it
    // is alive, rather than failed thirty seconds later for not binding
    // one it was never told about.
    for process in config.processes.values_mut() {
        process.cmd = "sleep 30".to_string();
    }
    let name = worktree_named(&fx, "feat/one");
    let report = start(&fx.paths, &config, &name, None, &noop).unwrap();
    let _g = guard(&report);
    assert_eq!(
        fx.state().worktrees[&name].processes["worker"].ready_port,
        None,
        "a process that owns no role has no port to wait for"
    );
    assert!(
        wait_until(Duration::from_secs(10), || matches!(
            refresh(&fx.paths).state.worktrees[&name]
                .processes
                .get("worker")
                .map(|p| &p.phase),
            Some(Phase::Running { .. })
        )),
        "the worker never reached running: {:?}",
        fx.state().worktrees[&name].processes["worker"].phase
    );
    stop(&fx.paths, &name, None).unwrap();
}

// The two cosmetic gaps the notes list, and the shape of the file as a
// whole: what detection writes has to read like something a human would
// have written, because the whole point is that a wrong guess is one
// visible edit away.
#[test]
fn the_config_a_workspace_answer_writes_reads_as_a_human_would_write_it() {
    let fx = workspace_fixture();
    let (ask, _asked) = scripted(vec![Answer::Auto(0)]);
    resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        !written.lines().any(|line| line.trim() == "[processes]"),
        "a bare [processes] header is a line nobody would write: {written}"
    );
    // The file's own header mentions the marker, so only lines that
    // are not themselves comments count.
    assert_eq!(
        written
            .lines()
            .filter(|line| !line.starts_with('#') && line.contains("# answered:"))
            .count(),
        2,
        "one note per table answered, not one per key: {written}"
    );
    for header in ["[processes.api]", "[processes.web]"] {
        assert!(
            written
                .lines()
                .any(|line| line.starts_with(header) && line.contains("# answered:")),
            "{header} carries the note for its own keys: {written}"
        );
    }
    let loaded = crate::config::load(&fx.paths).expect("the file pando wrote must load");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.config.processes.len(), 2);
}

#[test]
fn declining_the_workspace_question_keeps_the_root_script() {
    let fx = workspace_fixture();
    // The second option is the root script; the port question follows
    // it, because one process still needs a port.
    let (ask, asked) = scripted(vec![Answer::Choice(1), Answer::Choice(0)]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

    let questions = asked.borrow();
    assert_eq!(
        questions.iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::Processes, Slot::PortEnv]
    );
    assert_eq!(
        config.processes.keys().cloned().collect::<Vec<_>>(),
        vec!["dev"]
    );
    assert_eq!(config.processes["dev"].cmd, "pnpm dev");
    assert_eq!(config.processes["dev"].port_env()["WEB_PORT"], "{port:web}");

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("[dev]"), "one process is [dev]: {written}");
    assert!(!written.contains("[processes."), "{written}");
}

#[test]
fn a_lone_dev_with_no_command_is_filled_in_and_its_own_keys_are_kept() {
    let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    fx.config.processes.insert(
        "dev".to_string(),
        ProcessConfig {
            cwd: Some(".".to_string()),
            env: BTreeMap::from([("GREETING".to_string(), "hello".to_string())]),
            ..Default::default()
        },
    );
    let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert_eq!(config.processes["dev"].cmd, "pnpm dev");
    assert_eq!(config.processes["dev"].roles(), vec!["web"]);
    assert_eq!(config.processes["dev"].cwd.as_deref(), Some("."));
    assert_eq!(config.processes["dev"].env["GREETING"], "hello");
}

// A `ports` the developer wrote is an answer, including the empty one.
#[test]
fn a_lone_dev_that_says_it_has_no_ports_keeps_none() {
    let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    fx.config.processes.insert(
        "dev".to_string(),
        ProcessConfig {
            ports: Some(PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert_eq!(
        config.processes["dev"].cmd, "pnpm dev",
        "the command is filled"
    );
    assert!(
        config.processes["dev"].roles().is_empty(),
        "and the ports are left exactly as they were"
    );
}

// The workspace question is only for a workspace. Anywhere else the
// dev-command question is the one asked, as it was before.
#[test]
fn a_project_that_is_not_a_workspace_is_never_asked_about_processes() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\n",
    );
    let (ask, asked) = scripted(vec![Answer::Choice(1)]);
    resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::DevCmd],
        "a wrapper script with no workspace behind it is still one process"
    );
}

#[test]
fn an_unambiguous_project_is_resolved_without_asking_anything() {
    let fx = detectable_fixture(
        r#"{ "dev": "next dev", "build": "next build" }"#,
        "PORT=3000\n",
    );
    let notices = std::cell::RefCell::new(Vec::<String>::new());
    let config = resolve_process(&fx.paths, &fx.config, &refuse, &|m| {
        notices.borrow_mut().push(m.to_string())
    })
    .unwrap();
    let notices = notices.into_inner();
    assert_eq!(config.processes["dev"].cmd, "pnpm dev");
    assert_eq!(config.processes["dev"].roles(), vec!["web"]);
    assert!(
        notices.iter().any(|n| n.contains("pnpm dev")),
        "every guess is visible: {notices:?}"
    );
}

/// The shape a first run met: a repository with no manifest pando
/// reads and a `Makefile` whose `dev` target is several lines, the
/// first of them a guard. What is written has to be runnable, and what
/// the developer is offered has to be something they can recognise.
#[test]
fn a_multi_line_make_target_is_written_as_make_dev() {
    let fx = fixture();
    std::fs::write(
        fx.root.join("Makefile"),
        "APP := demo\nBIN := build\n\n.PHONY: dev\n\
         dev:\n\
         \t@command -v watcher >/dev/null || { echo \"install watcher\"; exit 1; }\n\
         \t@echo \"watching\"\n\
         \t@killall $(APP) 2>/dev/null; $(BIN)/$(APP) &\n",
    )
    .unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "make"]);

    let (ask, asked) = scripted(vec![Answer::None]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(
        config.processes["dev"].cmd, "make dev",
        "the target is run by make, not by one line lifted out of it"
    );
    assert!(
        asked.borrow().iter().all(|q| q.slot != Slot::DevCmd),
        "one candidate is a decision, not a question: {:?}",
        asked.borrow()
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("cmd = \"make dev\"  # detected: the dev target"),
        "{written}"
    );
    assert!(
        !written.contains("command -v"),
        "a guard that exits 0 is never what a developer is offered: {written}"
    );
}

/// And the narrow case the old behaviour was right about is kept: one
/// plain line, nothing make would expand, so `make` in the middle would
/// only be a process between pando and the server.
#[test]
fn a_one_line_make_target_is_still_written_as_the_line_itself() {
    let fx = fixture();
    std::fs::write(fx.root.join("Makefile"), "dev:\n\t@./serve --dev\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "make"]);

    let (ask, _asked) = scripted(vec![Answer::None]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(config.processes["dev"].cmd, "./serve --dev");
}

/// What the trailing comment means, which `docs/05-config-spec.md`
/// had wrong until 2026-09-22: it splits on **where the value came
/// from, not who typed it**.
///
/// A developer picking one of pando's options is accepting a value
/// pando's rules produced, and the `why` written beside it is the
/// rule's reasoning — so it is `# detected:`, the same as a value
/// pando took without asking. Only an answer with no rule behind it
/// is `# answered:`.
///
/// It is load-bearing, not cosmetic. `doctor` measures a
/// `# detected:` value against what the rules offer now and says so
/// when they no longer offer it; a typed answer has no rule behind it
/// to have changed and is never second-guessed. Writing a chosen
/// option as `# answered:` would silence exactly the case that check
/// exists for.
#[test]
fn a_chosen_option_is_detected_and_a_typed_answer_is_answered() {
    for (answer, expected) in [
        (
            Answer::Choice(1),
            "cmd = \"./scripts/run.sh\"  # detected: the run target",
        ),
        (
            Answer::Custom("./serve --dev".to_string()),
            "cmd = \"./serve --dev\"  # answered:",
        ),
    ] {
        let fx = fixture();
        // Two candidates and no certainty, so the question is really
        // asked: `make dev` for a target with a prerequisite, and the
        // one-line `run` target lifted as itself.
        std::fs::write(
            fx.root.join("Makefile"),
            "build:\n\t./scripts/build.sh\n\ndev: build\n\t./scripts/serve.sh\n\t./scripts/watch.sh\n\nrun:\n\t./scripts/run.sh\n",
        )
        .unwrap();
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "make"]);

        let (ask, asked) = scripted(vec![answer, Answer::None]);
        resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
        assert!(
            asked.borrow().iter().any(|q| q.slot == Slot::DevCmd),
            "the question has to have been asked for this to mean anything"
        );
        let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
        assert!(
            written.contains(expected),
            "want {expected:?} in:\n{written}"
        );
    }
}

#[test]
fn an_answer_is_written_to_the_config_with_a_comment() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("cmd = \"pnpm dev\"  # detected: package.json scripts.dev"),
        "{written}"
    );
    assert!(written.contains("[dev]"), "{written}");
    assert!(
        written.contains("ports = { PORT = \"web\" }"),
        "the map form is what a developer would have written: {written}"
    );
    // And the file pando wrote is one pando reads back.
    let loaded = crate::config::load(&fx.paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.config.processes["dev"].cmd, "pnpm dev");
}

#[test]
fn an_ambiguous_project_asks_once_and_never_again() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\nAPI_PORT=3001\n",
    );
    let (ask, asked) = scripted(vec![Answer::Choice(1), Answer::Choice(0)]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(config.processes["dev"].cmd, "pnpm dev:web");
    assert_eq!(config.processes["dev"].port_env()["PORT"], "{port:web}");
    let questions = asked.borrow();
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].slot, Slot::DevCmd);
    assert_eq!(questions[0].preselect, Some(0));
    assert!(questions[0].allow_custom);
    assert_eq!(questions[1].slot, Slot::PortEnv);

    // Second run, reading the config that was just written: nothing left
    // to ask.
    let loaded = crate::config::load(&fx.paths).unwrap().config;
    let again = resolve_process(&fx.paths, &loaded, &refuse, &noop).unwrap();
    assert_eq!(again.processes["dev"].cmd, "pnpm dev:web");
}

#[test]
fn a_typed_answer_is_taken_as_written_and_dated() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\nAPI_PORT=3001\n",
    );
    let (ask, _) = scripted(vec![Answer::Custom(
        "./scripts/serve.sh --port {port:web}".to_string(),
    )]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(
        config.processes["dev"].cmd,
        "./scripts/serve.sh --port {port:web}"
    );
    assert_eq!(
        config.processes["dev"].roles(),
        vec!["web"],
        "a command carrying {{port:web}} has answered the port question"
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("# answered:"), "{written}");
    assert!(written.contains("ports = [\"web\"]"), "{written}");
}

// A lone `[dev]` holding the developer's own `ports` is the shape pando
// fills a command into. A typed command carrying `{port:web}` kept their
// map for the start that answered it, then wrote `ports = ["web"]` over
// it in the file: `HMR_PORT` and its role were gone from the next load,
// and a `ready.role` naming it made the file refuse to load at all.
#[test]
fn a_typed_command_carrying_a_port_keeps_the_ports_the_developer_wrote() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\n",
    );
    let file = fx.paths.config_file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "[dev]\nports = { PORT = \"web\", HMR_PORT = \"hmr\" }\nready = { role = \"hmr\" }\n",
    )
    .unwrap();
    let loaded = crate::config::load(&fx.paths).unwrap().config;

    let (ask, asked) = scripted(vec![Answer::Custom(
        "npm run dev -- --port {port:web}".to_string(),
    )]);
    let config = resolve_process(&fx.paths, &loaded, &ask, &noop).unwrap();
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::DevCmd]
    );
    assert_eq!(config.processes["dev"].roles(), vec!["hmr", "web"]);

    let written = std::fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("ports = { PORT = \"web\", HMR_PORT = \"hmr\" }"),
        "{written}"
    );
    let reloaded = crate::config::load(&fx.paths)
        .unwrap_or_else(|e| panic!("the file still loads: {e:#}\n{written}"))
        .config;
    assert_eq!(
        reloaded.processes["dev"].cmd,
        "npm run dev -- --port {port:web}"
    );
    assert_eq!(reloaded.processes["dev"].roles(), vec!["hmr", "web"]);
}

// "no ports" and "not answered yet" used to be the same state, so a
// process that really has none — a worker, a watcher, a queue consumer
// — was asked again on every start, and given a port it would never
// bind if anything answered for it.
#[test]
fn answering_none_to_the_port_question_is_written_down_as_no_ports() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\nAPI_PORT=3001\n",
    );
    let (ask, asked) = scripted(vec![
        Answer::Custom("./worker.sh".to_string()),
        Answer::None,
    ]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(asked.borrow().len(), 2, "the command, then the port");
    assert!(
        asked.borrow()[1].allow_none,
        "the port question has to offer \"none\" for this to be answerable"
    );
    assert!(
        config.processes["dev"].roles().is_empty(),
        "a process with no ports has no roles"
    );

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("ports = []"),
        "an empty list, so it reads as answered rather than missing: {written}"
    );

    // And asked once: a second resolve has nothing left to ask about.
    let again = resolve_process(&fx.paths, &config, &refuse, &noop).unwrap();
    assert!(again.processes["dev"].roles().is_empty());
}

// A project whose env example names its ports by role gets them as
// roles, all of them, in one answer — and the framework convention
// underneath is still on offer rather than thrown away.
#[test]
fn the_env_examples_own_port_variables_become_this_projects_roles() {
    let fx = detectable_fixture(
        r#"{ "dev": "next dev" }"#,
        "WEB_PORT=5173\nAPI_PORT=4000\nDATABASE_PORT=5432\n",
    );
    let (ask, asked) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();

    let questions = asked.borrow();
    assert_eq!(
        questions.iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::PortEnv],
        "the dev command resolves on its own; only the port is a question"
    );
    assert_eq!(
        questions[0]
            .options
            .iter()
            .map(|(value, _)| value.as_str())
            .collect::<Vec<_>>(),
        vec!["WEB_PORT, API_PORT", "PORT", "WEB_PORT", "API_PORT"],
        "the project's own declaration leads; Next.js's PORT is still offered"
    );
    assert_eq!(
        config.processes["dev"].roles(),
        vec!["api", "web"],
        "two variables named by role are two roles, not two guesses at one"
    );
    assert_eq!(config.processes["dev"].port_env()["WEB_PORT"], "{port:web}");
    assert_eq!(config.processes["dev"].port_env()["API_PORT"], "{port:api}");

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains(r#"ports = { API_PORT = "api", WEB_PORT = "web" }"#),
        "{written}"
    );
    // The file pando wrote has to load, and to have answered the slot:
    // a second start must not ask again.
    let loaded = crate::config::load(&fx.paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let again = resolve_process(&fx.paths, &loaded.config, &refuse, &noop).unwrap();
    assert_eq!(again.processes["dev"].roles(), vec!["api", "web"]);
}

// ---- init ------------------------------------------------------------

// The batch form asks the same questions, through the same resolver:
// a project whose rules all decide is configured without a prompt.
// A preview promises to write nothing. It used to leave the project's
// own directory behind in pando's home — empty, but a write all the same.
#[test]
fn a_dry_run_leaves_no_directory_behind_in_pandos_home() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    assert!(!fx.paths.home.exists(), "the fixture starts with no home");
    let (_, preview) =
        init_dry_run(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    assert!(!preview.is_empty(), "it previewed something");
    assert!(
        !fx.paths.project_dir().exists(),
        "no project directory: {}",
        fx.paths.project_dir().display()
    );
    assert!(!fx.paths.home.exists(), "and no home either");

    // A home that already existed is left exactly where it was.
    fx.paths.ensure_home().unwrap();
    init_dry_run(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    assert!(fx.paths.project_dir().is_dir());
    assert!(!fx.paths.config_file().exists());
}

// The preview ran in a scratch home with none of the real one's recipes
// or shims, so it proposed services from the built-in recipes alone: a
// developer whose recipe points an engine at a shim in pando's own `bin`
// was shown one answer and then had `init` write another.
#[test]
fn a_dry_run_sees_the_developers_own_recipes_and_shims() {
    use std::os::unix::fs::PermissionsExt;
    let fx = detectable_fixture(
        r#"{ "dev": "next dev" }"#,
        "PORT=3000\nMONGODB_URL=mongodb://localhost:27017/app\n",
    );
    // An engine only pando's own `bin` has, so what either run finds does
    // not depend on what this host has installed.
    let recipes = fx.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(
        recipes.join("mongodb.toml"),
        "kind = \"service\"\nname = \"mongodb\"\nbinaries = [\"pando-fake-mongod\"]\n\n\
         [service]\ncmd = \"exec pando-fake-mongod --port {port}\"\n",
    )
    .unwrap();
    let bin = fx.paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("pando-fake-mongod"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(
        bin.join("pando-fake-mongod"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let services = std::cell::RefCell::new(Vec::<Question>::new());
    let take_the_rules = |q: &Question| -> Result<Answer> {
        if q.slot == Slot::Services {
            services.borrow_mut().push(q.clone());
            return Ok(Answer::Many(q.checked.clone()));
        }
        match recommended(q) {
            Some((answer, _)) => Ok(answer),
            None => Err(NeedsAnswer {
                question: q.clone(),
            }
            .into()),
        }
    };
    let (_, preview) = init_dry_run(
        &fx.paths,
        &fx.config,
        &Answering::asking(&take_the_rules),
        &noop,
    )
    .unwrap();
    init(
        &fx.paths,
        &fx.config,
        &Answering::asking(&take_the_rules),
        &noop,
    )
    .unwrap();

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("mongodb"), "{written}");
    let previewed = preview
        .iter()
        .find(|(path, _)| *path == fx.paths.config_file())
        .map(|(_, body)| body.as_str());
    assert_eq!(previewed, Some(written.as_str()), "{services:?}");
    // And the links went with the scratch, leaving what they pointed at.
    assert!(recipes.join("mongodb.toml").is_file());
    assert!(bin.join("pando-fake-mongod").is_file());
    assert!(!fx.paths.home.join("preview").exists());
}

// The preview prints pando's own file for the project whole, and that is
// where a login typed at a namespaced start is kept. It printed the
// password; doctor, reading the same file, never has.
#[test]
fn a_dry_run_prints_a_namespace_login_without_its_password() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    let kept = "[namespaced]\ncache = { password = \"t0ps3cret\" }\n\n\
                [namespaced.db]  # answered: 2026-09-26\nuser = \"root\"\npassword = \"s3cret\"\n";
    std::fs::create_dir_all(fx.paths.project_dir()).unwrap();
    std::fs::write(fx.paths.config_file(), kept).unwrap();

    let (_, preview) =
        init_dry_run(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    let body = preview
        .iter()
        .find(|(path, _)| *path == fx.paths.config_file())
        .map(|(_, body)| body.as_str())
        .expect("the pass changes the project file, so it is previewed");
    assert!(body.contains(r#"cmd = "pnpm dev""#), "{body}");
    assert!(!body.contains("s3cret"), "{body}");
    assert!(!body.contains("t0ps3cret"), "{body}");
    assert!(
        body.contains(
            "[namespaced.db]  # answered: 2026-09-26\nuser = \"root\"\npassword = \"(hidden)\"\n"
        ),
        "{body}"
    );
    assert!(
        body.contains("cache = { password = \"(hidden)\" }"),
        "{body}"
    );
    // Only what is printed: the file itself keeps the login it holds.
    assert_eq!(
        std::fs::read_to_string(fx.paths.config_file()).unwrap(),
        kept
    );
}

#[test]
fn init_takes_every_slot_a_rule_decided_and_asks_nothing() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    let report = init(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();

    assert!(report.answered_anything());
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains(r#"install = "pnpm install --frozen-lockfile""#),
        "{written}"
    );
    assert!(written.contains(r#"cmd = "pnpm dev""#), "{written}");
    assert!(written.contains("provision = [\".env\"]"), "{written}");
}

// `null` at `schema_hook` is an answers file's documented "no", and on a
// project where the rules found no schema step it has nothing to switch
// off. It used to reach the writer for `ports = []` and `provision = []`,
// which has no key for a hook, and panic with the slots after it unasked.
#[test]
fn a_program_saying_no_to_a_schema_step_nobody_found_writes_nothing_and_carries_on() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    let offered = std::cell::RefCell::new(Vec::<Slot>::new());
    let program = |q: &Question| -> Option<Result<Answer>> {
        offered.borrow_mut().push(q.slot);
        (q.slot == Slot::SchemaHook).then(|| Ok(Answer::Program(Box::new(Answer::None))))
    };
    let said = std::cell::RefCell::new(Vec::<String>::new());
    let progress = |line: &str| said.borrow_mut().push(line.to_string());
    init(
        &fx.paths,
        &fx.config,
        &Answering::by_program(&refuse, &program),
        &progress,
    )
    .unwrap();

    assert!(
        offered.borrow().contains(&Slot::SchemaHook),
        "the program was offered the slot the rules were silent about"
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(!written.contains("[[hooks]]"), "{written}");
    assert!(
        written.contains("provision = [\".env\"]"),
        "the slot after it was still reached: {written}"
    );
    assert!(
        said.borrow()
            .iter()
            .any(|line| line.contains("no schema step") && line.contains("nothing was written")),
        "{:?}",
        said.borrow()
    );
}

// What `signals` publishes as `answered`, and what `init` reports, is
// what the resolver really asks. A config that declares two processes
// is never asked for a dev command or its ports, and was published as
// open on both forever; so was the services slot after a native project
// answered "none of them", which is written as `[isolation] none`.
#[test]
fn a_slot_the_resolver_will_never_ask_is_settled() {
    let mut two = Config::default();
    two.processes.insert("web".to_string(), dev("pnpm dev:web"));
    two.processes.insert("api".to_string(), dev("pnpm dev:api"));
    assert!(settled(Slot::DevCmd, &two));
    assert!(settled(Slot::PortEnv, &two));

    // A `[dev]` written by hand with no ports is an answer about them.
    let mut by_hand = Config::default();
    by_hand.processes.insert(
        "dev".to_string(),
        ProcessConfig {
            cmd: "./serve".to_string(),
            ..Default::default()
        },
    );
    assert!(settled(Slot::PortEnv, &by_hand));

    // A lone `[dev]` with no command is the shape detection fills.
    let mut lone = Config::default();
    lone.processes
        .insert("dev".to_string(), ProcessConfig::default());
    assert!(!settled(Slot::DevCmd, &lone));
    assert!(!settled(Slot::PortEnv, &lone));
    assert!(!settled(Slot::DevCmd, &Config::default()));

    let mut none = Config::default();
    assert!(!settled(Slot::Services, &none));
    none.isolation.none = true;
    assert!(settled(Slot::Services, &none));
    assert_eq!(
        super::init::slot_value(&none, Slot::Services).as_deref(),
        Some("none"),
        "and `init` says what the answer was"
    );
}

// Ask just in time, once: the second pass has nothing left to ask,
// which is what writing the first one down was for.
#[test]
fn a_second_init_asks_nothing_and_answers_nothing() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    init(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    let first = std::fs::read_to_string(fx.paths.config_file()).unwrap();

    let loaded = crate::config::load(&fx.paths).unwrap();
    let report = init(
        &fx.paths,
        &loaded.config,
        &Answering::asking(&refuse),
        &noop,
    )
    .unwrap();
    assert!(!report.answered_anything());
    assert_eq!(
        std::fs::read_to_string(fx.paths.config_file()).unwrap(),
        first,
        "a second init rewrites nothing at all"
    );
}

// One question per undecided slot, in one pass, and every answer on
// disk when it ends.
#[test]
fn init_asks_one_question_per_undecided_slot() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently 'next dev' 'node worker.js'", "dev:web": "next dev" }"#,
        "PORT=3000\n",
    );
    let (ask, asked) = scripted(vec![Answer::Custom("pnpm dev".to_string())]);
    let report = init(&fx.paths, &fx.config, &Answering::asking(&ask), &noop).unwrap();

    let slots: Vec<Slot> = asked.borrow().iter().map(|q| q.slot).collect();
    assert_eq!(
        slots,
        vec![Slot::DevCmd],
        "the dev command is the only thing the rules could not settle here"
    );
    assert_eq!(
        report
            .slots
            .iter()
            .find(|s| s.slot == Slot::DevCmd)
            .and_then(|s| s.value.clone()),
        Some("pnpm dev".to_string())
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains(r#"cmd = "pnpm dev""#), "{written}");
}

// `init` is the batch form of the questions and nothing else. Answering
// them must not bring a worktree up.
#[test]
fn init_starts_nothing() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    let name = worktree_named(&fx, "feat/one");
    init(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    assert!(
        fx.state().worktrees[&name].processes.is_empty(),
        "init answered questions and started something"
    );
}

// The summary is about the file, not about the run: it is read back
// from disk, so a config pando could not load again is a failure `init`
// reports rather than one the next command discovers.
#[test]
fn the_summary_says_what_the_written_config_holds() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    let report = init(&fx.paths, &fx.config, &Answering::asking(&refuse), &noop).unwrap();
    let value = |slot: Slot| {
        report
            .slots
            .iter()
            .find(|s| s.slot == slot)
            .and_then(|s| s.value.clone())
    };
    assert_eq!(
        value(Slot::Install),
        Some("pnpm install --frozen-lockfile".to_string())
    );
    assert_eq!(value(Slot::DevCmd), Some("pnpm dev".to_string()));
    assert_eq!(value(Slot::PortEnv), Some("PORT = web".to_string()));
    assert_eq!(value(Slot::Provision), Some(".env".to_string()));
    assert_eq!(value(Slot::Services), None, "there is no compose file here");
    assert_eq!(report.config_file, fx.paths.config_file());
    assert_eq!(report.user_file, None, "nothing here was about the machine");
}

// The prelude is the one answer that belongs to the laptop, so it is
// the one that makes `init` name a second file.
#[test]
fn a_machine_answer_names_the_machine_wide_file() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::write(fx.root.join(".nvmrc"), "99\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "pin node 99"]);

    // No machine has node 99, so the probe is a mismatch on any host,
    // and "this machine needs nothing" is the answer given.
    let (ask, asked) = scripted(vec![
        Answer::None,
        Answer::Choice(0),
        Answer::Choice(0),
        Answer::Choice(0),
    ]);
    let report = init(&fx.paths, &fx.config, &Answering::asking(&ask), &noop).unwrap();
    assert_eq!(
        asked.borrow().first().map(|q| q.slot),
        Some(Slot::Prelude),
        "the runtime is asked about before anything else: {:?}",
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>()
    );
    assert_eq!(report.user_file, Some(fx.paths.user_config_file()));
    let written = std::fs::read_to_string(fx.paths.user_config_file()).unwrap();
    assert!(written.contains(r#"prelude = """#), "{written}");
}

// ---- the services slot -----------------------------------------------

/// [`detectable_fixture`] with a compose file, which is what the
/// services slot is proposed from.
fn compose_fixture(compose: &str, env_example: &str) -> Fx {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, env_example);
    std::fs::write(fx.root.join("docker-compose.yml"), compose).unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "compose"]);
    fx
}

/// The isolating form of the resolver: on a plain start the services
/// question is silent, and these are all about what it asks.
fn resolve_isolating(paths: &PandoPaths, config: &Config, ask: Ask<'_>) -> Result<Config> {
    super::resolve_process(paths, config, Mode::Isolated, ask, &noop)
}

// ---- an answer pando could not load back ------------------------

/// The pair the detection notes routed here: an env example naming
/// `API_PORT` gives the application a role called `api`, and a compose
/// file with a service called `api` gives it that name too. Each
/// answer is reasonable; together they are a config pando's own loader
/// refuses — and it used to refuse it at the *next* load, in the
/// project layer, which fails hard.
#[test]
fn an_answer_that_would_make_the_config_refuse_to_load_is_refused_before_it_is_written() {
    let fx = compose_fixture(
        "services:\n  api:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    let mut config = fx.config.clone();
    config.processes.insert(
        "dev".to_string(),
        ProcessConfig {
            cmd: "next dev".to_string(),
            ports: Some(crate::config::PortsSpec::Map(BTreeMap::from([(
                "API_PORT".to_string(),
                "api".to_string(),
            )]))),
            ..Default::default()
        },
    );

    let err = resolve_isolating(&fx.paths, &config, &refuse).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("both claim the role"), "{text}");
    assert!(text.contains("nothing was written"), "{text}");

    // And nothing was: the file pando writes has no services entry in
    // it, and it still loads.
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap_or_default();
    assert!(!written.contains("[[services]]"), "{written}");
    assert!(
        crate::config::load(&fx.paths).is_ok(),
        "the project layer still loads"
    );
}

#[test]
fn the_same_answer_is_written_when_nothing_else_claims_the_name() {
    let fx = compose_fixture(
        "services:\n  api:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    let mut config = fx.config.clone();
    config.processes.insert(
        "dev".to_string(),
        ProcessConfig {
            cmd: "next dev".to_string(),
            ports: Some(crate::config::PortsSpec::Map(BTreeMap::from([(
                "PORT".to_string(),
                "web".to_string(),
            )]))),
            ..Default::default()
        },
    );
    let resolved = resolve_isolating(&fx.paths, &config, &refuse).unwrap();
    let crate::config::ServiceConfig::Compose { include, .. } = &resolved.services[0] else {
        panic!("a compose entry");
    };
    assert_eq!(include, &vec!["api".to_string()]);
    assert!(crate::config::load(&fx.paths).is_ok());
}

const TWO_DATABASES: &str = "services:\n  \
     db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n  \
     db_test:\n    image: postgres:16\n    ports: [\"5433:5432\"]\n";

// A dev database beside a test one is an ordinary layout, and both of
// them resolve to `DATABASE_URL` through the image's prefixes. Taking
// that as decided pointed the app — and the migration hook — at the
// test database, and left the real one with nothing addressing it.
#[test]
fn two_services_that_would_claim_one_env_key_are_asked_about_rather_than_guessed() {
    let fx = compose_fixture(
        TWO_DATABASES,
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n\
         TEST_DATABASE_URL=postgres://acme:acme@localhost:5433/acme_test\n",
    );
    let (ask, asked) = scripted(vec![Answer::Many(vec![0, 1])]);
    let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

    assert_eq!(
        asked.borrow().len(),
        1,
        "a second database pando cannot address is a question, not a guess"
    );
    let question = &asked.borrow()[0];
    assert_eq!(question.slot, Slot::Services);
    assert_eq!(
        question.checked,
        vec![0],
        "only the one a rule really resolved starts ticked"
    );
    assert!(
        question.options[1].1.contains("DATABASE_URL"),
        "and the other says whose key it would have taken: {}",
        question.options[1].1
    );

    let crate::config::ServiceConfig::Compose { env, include, .. } = &config.services[0] else {
        panic!("a compose entry");
    };
    assert_eq!(include, &vec!["db".to_string(), "db_test".to_string()]);
    assert_eq!(
        env.get("DATABASE_URL").map(String::as_str),
        Some("db"),
        "the key belongs to the service that claimed it first: {env:?}"
    );
    assert_eq!(env.len(), 1, "and no key is written twice: {env:?}");
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(!written.contains("DATABASE_URL = \"db_test\""), "{written}");
}

/// A compose file with one service nothing in the env example names.
const ONE_UNNAMED_CACHE: &str =
    "services:\n  cache:\n    image: redis:7\n    ports: [\"6379:6379\"]\n";

// "None of them" is an answer, and an answer that is not written down
// is asked again on every start — exit 3 for a script, with no way to
// answer it except editing TOML by hand.
#[test]
fn answering_none_to_the_services_question_is_written_down_as_no_services() {
    let fx = compose_fixture(ONE_UNNAMED_CACHE, "PORT=3000\n");
    let (ask, asked) = scripted(vec![Answer::None]);
    let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();
    assert_eq!(asked.borrow().len(), 1);

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("[[services]]"), "{written}");
    assert!(
        written.contains("include = []"),
        "an empty list, so it reads as answered rather than missing: {written}"
    );
    assert_eq!(config.services.len(), 1);

    // And asked once: a second resolve has nothing left to ask about.
    let again = resolve_isolating(&fx.paths, &config, &refuse).unwrap();
    assert_eq!(again.services.len(), 1);
}

#[test]
fn yes_with_nothing_the_rules_resolved_is_written_down_the_same_way() {
    let fx = compose_fixture(ONE_UNNAMED_CACHE, "PORT=3000\n");
    let (ask, _) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("include = []"),
        "--yes that takes nothing still answers the question: {written}"
    );
    assert!(
        written.contains("--yes took the 0 of 1"),
        "and says a flag did it, not a human: {written}"
    );
    let again = resolve_isolating(&fx.paths, &config, &refuse).unwrap();
    assert_eq!(again.services.len(), 1);
}

/// A compose file whose every service is built out of this repository:
/// the app, and a worker sharing its image.
const ONLY_THIS_PROJECT: &str = "services:\n  \
     web:\n    build: .\n    ports: [\"3000:3000\"]\n  \
     worker:\n    build:\n      context: ./services/worker\n";

// The services question offered the app itself, because a compose file
// that builds it declares a service like any other. The only answer on
// offer was "run a second copy of the thing you are developing", and a
// question whose only answer is wrong is worse than silence.
#[test]
fn a_compose_file_of_only_this_project_is_never_a_question() {
    let fx = compose_fixture(ONLY_THIS_PROJECT, "PORT=3000\n");
    let notices = std::cell::RefCell::new(Vec::new());
    let config = super::resolve_process(&fx.paths, &fx.config, Mode::Isolated, &refuse, &|line| {
        notices.borrow_mut().push(line.to_string())
    })
    .unwrap();

    // Asked nothing — `refuse` panics on a question — and still
    // answered: the empty answer is written down, so the next start
    // does not work it out again.
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("[[services]]"), "{written}");
    assert!(written.contains("include = []"), "{written}");
    assert!(
        written.contains("# detected:") && written.contains("built from this repository"),
        "the file says why pando decided this on its own: {written}"
    );
    assert_eq!(config.services.len(), 1);
    assert!(
        notices
            .borrow()
            .iter()
            .any(|line| line.contains("no services") && line.contains("built from this")),
        "every guess is visible, the empty one included: {:?}",
        notices.borrow()
    );

    // The file pando wrote has to load, and to have answered the slot.
    let loaded = crate::config::load(&fx.paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let again =
        super::resolve_process(&fx.paths, &loaded.config, Mode::Isolated, &refuse, &noop).unwrap();
    assert_eq!(again.services.len(), 1);
    assert!(
        super::service_roles(&again).is_empty(),
        "an entry with nothing included brings no roles, so an isolated \
         start runs shared"
    );
}

// The app is filtered out; everything it really depends on is not.
#[test]
fn the_dependencies_beside_the_app_are_still_offered() {
    let fx = compose_fixture(
        "services:\n  \
         web:\n    build: .\n  \
         postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    let config =
        super::resolve_process(&fx.paths, &fx.config, Mode::Isolated, &refuse, &noop).unwrap();
    let crate::config::ServiceConfig::Compose { include, env, .. } = &config.services[0] else {
        panic!("a compose entry");
    };
    assert_eq!(include, &vec!["postgres".to_string()]);
    assert_eq!(
        env.get("DATABASE_URL").map(String::as_str),
        Some("postgres")
    );
}

/// A docker that answers nothing, which is what a machine with no
/// daemon looks like to `docker compose config`. Installed so the test
/// below exercises the fallback rather than this machine's Docker.
fn docker_that_cannot_answer(paths: &PandoPaths) {
    use std::os::unix::fs::PermissionsExt;
    let bin = paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("docker");
    std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// `extends:` and a top-level `include:` are common in real
// repositories, and pando's own reader follows neither. Reading half a
// file and concluding "this project has no services pando can address"
// is the one answer that is certainly wrong.
#[test]
fn a_compose_file_pando_cannot_read_whole_is_asked_about_rather_than_passed_over() {
    let fx = compose_fixture(
        "services:\n  db:\n    extends:\n      file: base.yml\n      service: template\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    fx.paths.ensure_home().unwrap();
    docker_that_cannot_answer(&fx.paths);

    let (ask, asked) = scripted(vec![Answer::None]);
    resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();
    assert_eq!(
        asked.borrow().len(),
        1,
        "a file pando could not read whole is a question, not silence"
    );
    let question = &asked.borrow()[0];
    assert_eq!(question.slot, Slot::Services);
    assert_eq!(question.options.len(), 1);
    assert!(
        question.options[0].1.contains("extends"),
        "and it says why it could not decide: {}",
        question.options[0].1
    );
    assert!(
        question.checked.is_empty(),
        "nothing pando read through an unfollowed key starts ticked"
    );
}

// `rewrite` can put a port into a URL or replace a bare number. A bare
// host name has nowhere to put one, so proposing a `_HOST` key wrote a
// mapping that could never be satisfied — and the failed start left the
// worktree recorded as isolated, so it could not be started at all.
#[test]
fn a_host_key_is_never_proposed_because_a_port_cannot_be_put_into_one() {
    let fx = compose_fixture(
        "services:\n  db:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDB_HOST=db\n",
    );
    let (ask, _) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_isolating(&fx.paths, &fx.config, &ask).unwrap();

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        !written.contains("DB_HOST"),
        "a key pando cannot rewrite is not a mapping it may write: {written}"
    );
    let crate::config::ServiceConfig::Compose { env, .. } = &config.services[0] else {
        panic!("a compose entry");
    };
    assert!(env.is_empty(), "{env:?}");

    // And the project still starts, which is the whole point.
    let name = super::new(&fx.paths, &config, "feat/h", None, &noop).unwrap();
    let report = start(&fx.paths, &config, &name, None, &noop).unwrap();
    assert!(report.ports.contains_key("web"));
    let _ = stop(&fx.paths, &name, None);
}

#[test]
fn a_config_the_developer_already_wrote_is_never_questioned() {
    let mut fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\nAPI_PORT=3001\n",
    );
    with_dev(&mut fx, dev("./my-own-server"));
    let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert_eq!(config.processes["dev"].cmd, "./my-own-server");
    assert!(
        !fx.paths.config_file().exists(),
        "resolving nothing writes nothing"
    );
}

// Detection may only ever write `[dev]`, and a file holding both `[dev]`
// and `[processes]` is one pando's own loader refuses — which used to
// brick every later command, `stop` included.
#[test]
fn detection_never_writes_a_dev_table_next_to_a_configured_process() {
    let mut fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    fx.config
        .processes
        .insert("web".to_string(), dev("sleep 30"));

    let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert!(
        !config.processes.contains_key("dev"),
        "a project that declares its processes has answered both slots"
    );
    assert!(
        !fx.paths.config_file().exists(),
        "and nothing at all was written: {:?}",
        std::fs::read_to_string(fx.paths.config_file()).ok()
    );
}

#[test]
fn a_library_is_resolved_to_nothing_at_all() {
    let fx = fixture();
    let config = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert!(
        config.processes.is_empty(),
        "a repository with no server gets no process and no question"
    );
    assert!(!fx.paths.config_file().exists());
}

#[test]
fn new_resolves_install_version_files_and_provision() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::write(fx.root.join(".nvmrc"), "22\n").unwrap();
    git(&fx.root, &["add", ".nvmrc"]);
    git(&fx.root, &["commit", "--quiet", "-m", "nvmrc"]);

    // A machine that already resolves what `.nvmrc` asks for: with an
    // install to run, `new` checks the runtime, and this one needs nothing.
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("22.11.0", "", "");
    let m = Machine {
        shell: &shell,
        home: machine.home.path().to_path_buf(),
    };
    let config =
        super::questions::resolve_for_new_on(&fx.paths, &fx.config, &refuse, &noop, &m).unwrap();
    assert_eq!(
        config.project.install.as_deref(),
        Some("pnpm install --frozen-lockfile")
    );
    assert_eq!(config.runtime.version_files, vec![".nvmrc"]);
    assert_eq!(
        config.project.provision.as_deref(),
        Some(&[".env".to_string()][..])
    );
    assert!(
        config.processes.is_empty(),
        "new does not need the dev command yet"
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("# detected: pnpm-lock.yaml"), "{written}");
}

// The case `--yes` cannot rescue: pando knows there is a server here and
// has no candidate to offer.
#[test]
fn a_framework_with_no_command_shape_asks_with_no_options() {
    let fx = fixture();
    std::fs::write(
        fx.root.join("package.json"),
        "{\n  \"scripts\": { \"build\": \"node build.js\" }\n}\n",
    )
    .unwrap();
    let (ask, asked) = scripted(vec![Answer::Custom("node server.js".to_string())]);
    let config = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(config.processes["dev"].cmd, "node server.js");
    let questions = asked.borrow();
    assert_eq!(questions[0].slot, Slot::DevCmd);
    assert!(questions[0].options.is_empty());
    assert_eq!(
        questions[0].preselect, None,
        "there is nothing to recommend, so --yes has nothing to take"
    );
    assert!(questions[0].allow_custom);
}

#[test]
fn an_empty_answer_is_refused() {
    let fx = detectable_fixture(
        r#"{ "dev": "concurrently \"npm:dev:*\"", "dev:web": "next dev" }"#,
        "PORT=3000\n",
    );
    let (ask, _) = scripted(vec![Answer::Custom("   ".to_string())]);
    let err = resolve_process(&fx.paths, &fx.config, &ask, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("empty answer"), "{err:#}");
}

// A dev server whose working directory has just been deleted is not a
// process anyone can do anything with — and `rm` removes the record
// that is the only way to find it again.
#[test]
fn rm_stops_what_is_running_before_it_removes_the_worktree() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pgid = outcome.started[0].record.pgid;
    assert!(crate::process::group_alive(pgid));

    rm(&fx.paths, &name, false, false).unwrap();
    assert!(
        !crate::process::group_alive(pgid),
        "rm must not leave a process behind with no record of it"
    );
    assert!(!fx.state().worktrees.contains_key(&name));
    assert!(fx.names().is_empty());
}

// git's refusal is the last one, and it used to come *after* the kill:
// the dev server was stopped, git then kept the worktree, and the next
// read blamed the process for pando's own kill.
#[test]
fn rm_refuses_a_dirty_worktree_before_it_stops_anything() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 300"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pgid = outcome.started[0].record.pgid;
    std::fs::write(
        fx.worktrees_dir().join(&name).join("README.md"),
        "edited in the worktree\n",
    )
    .unwrap();

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("modified or untracked"),
        "{err:#}"
    );
    assert!(
        crate::process::group_alive(pgid),
        "a removal pando declined has not touched what is running"
    );
    let record = &fx.state().worktrees[&name].processes["dev"];
    assert!(
        matches!(record.phase, Phase::Starting { .. } | Phase::Running { .. }),
        "and the process is not written off for a kill that never happened: {:?}",
        record.phase
    );
}

// A start that will only report what is already up must not run an
// install first: `npm ci` inside a worktree whose dev server is live is
// a surprise nobody asked for.
#[test]
fn a_start_that_reports_a_running_process_runs_no_install() {
    let mut fx = fixture();
    // No lockfile here, so the hook has no fingerprint and runs on
    // every start — which is what makes this visible at all.
    let marker = fx.paths.project_dir().join("install-ran");
    fx.config.project.install = Some(format!("echo ran >> {}", marker.display()));
    with_dev(&mut fx, dev("sleep 300"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let runs = || {
        std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = runs();
    assert!(
        before > 0,
        "the hook has to have run at all for this to mean anything"
    );

    let second = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    assert!(second.started_nothing());
    assert_eq!(
        runs(),
        before,
        "nothing was started, so nothing was installed"
    );
}

// A refusal must still leave the worktree usable: it is only stopped
// once every reason to refuse has been checked.
#[test]
fn a_refused_rm_leaves_the_process_running() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pgid = outcome.started[0].record.pgid;

    // Locked worktrees are always refused, before anything is touched.
    git(
        &fx.root,
        &[
            "worktree",
            "lock",
            fx.worktrees_dir().join(&name).to_str().unwrap(),
        ],
    );
    assert!(rm(&fx.paths, &name, true, true).is_err());
    assert!(
        crate::process::group_alive(pgid),
        "a removal pando declined has not touched what is running"
    );
    git(
        &fx.root,
        &[
            "worktree",
            "unlock",
            fx.worktrees_dir().join(&name).to_str().unwrap(),
        ],
    );
}

// ---- refresh ---------------------------------------------------------

#[test]
fn refresh_on_a_project_that_never_started_anything_writes_nothing() {
    let fx = fixture();
    let refreshed = refresh(&fx.paths);
    assert!(refreshed.state.worktrees.is_empty());
    assert!(refreshed.warning.is_none());
    assert!(
        !fx.paths.state_file().exists(),
        "a read path must not create state"
    );
}

#[test]
fn refresh_moves_a_starting_process_to_running_once_its_port_binds() {
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut fx = fixture();
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: python_listener_template(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let port = outcome.ports["web"];

    assert!(
        wait_until(Duration::from_secs(20), || matches!(
            refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
            Phase::Running { .. }
        )),
        "never reached Running: {:?}",
        log_of(&fx, &name)
    );
    // Saved, not just computed: the next command must see it too.
    assert!(matches!(
        fx.state().worktrees[&name].processes["dev"].phase,
        Phase::Running { .. }
    ));
    assert!(
        wait_until(Duration::from_secs(10), || refresh(&fx.paths)
            .state
            .worktrees[&name]
            .observed_ports
            .contains(&port)),
        "the port it is really listening on is recorded"
    );
}

// The whole reason read paths advance instead of reconciling: a crashed
// dev server has to stay on screen until the developer acts on it.
#[test]
fn a_crashed_process_stays_failed_across_reads() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("echo boom && exit 1"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    assert!(wait_until(Duration::from_secs(10), || matches!(
        refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
        Phase::Failed { .. }
    )));
    for _ in 0..3 {
        let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
            .phase
            .clone();
        assert!(
            matches!(&phase, Phase::Failed { reason, .. } if reason.starts_with("process exited")),
            "a failure must not be swept away by the next read: {phase:?}"
        );
    }
    // And the worktree keeps its ports, so a restart reuses them.
    assert!(!fx.state().worktrees[&name].ports.is_empty());
}

/// The report that started this: a dev command that succeeded and
/// returned, recorded as "failed — process exited" over an empty log.
/// Now the exit status is in the line every read path shows.
#[test]
fn a_dev_command_that_exits_at_once_reports_the_status_it_exited_with() {
    let mut fx = fixture();
    // The generic guard shape: a prerequisite check that exits 0 when
    // the tool it checks for is there, which is the usual case.
    with_dev(
        &mut fx,
        dev("command -v sh >/dev/null || { echo 'sh not found'; exit 1; }"),
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    assert!(wait_until(Duration::from_secs(10), || matches!(
        refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
        Phase::Failed { .. }
    )));
    let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
        .phase
        .clone();
    let Phase::Failed { reason, .. } = &phase else {
        panic!("expected a failure, got {phase:?}");
    };
    assert!(
        reason.contains("status 0"),
        "the exit status is the whole difference between a crash and a \
         command that did its job and returned: {reason}"
    );
    assert!(
        !log_of(&fx, &name).contains("sh not found"),
        "the guard took its success branch, which is the case being tested"
    );
}

/// A status file outlives the run that wrote it unless something
/// removes it, and the next run of the same process would then be
/// explained by the previous one's exit. `reset_log` takes both.
#[test]
fn resetting_a_log_takes_the_exit_status_beside_it() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("logs").join("dev.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "the previous run\n").unwrap();
    let status = crate::paths::exit_status_file(&log);
    std::fs::write(&status, "0").unwrap();

    reset_log(&log).unwrap();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
    assert!(
        !status.exists(),
        "the previous run's status may not explain the next one"
    );
    // And it is fine with nothing to remove.
    reset_log(&log).unwrap();
}

/// The shape that would have been accused wrongly: a command that
/// backgrounds the server and returns. Its leader exits 0 having
/// printed nothing, exactly like the guard — and the server is fine.
#[test]
fn a_command_that_backgrounds_the_server_is_not_called_the_wrong_one() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30 & exit 0"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    assert!(wait_until(Duration::from_secs(10), || matches!(
        refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
        Phase::Failed { .. }
    )));
    let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
        .phase
        .clone();
    let Phase::Failed { reason, .. } = &phase else {
        panic!("expected a failure, got {phase:?}");
    };
    assert!(
        !reason.contains("not the one that starts it"),
        "the server it started is up; nothing here is evidence against the \
         command: {reason}"
    );
}

/// The other half, without depending on what a login shell on this
/// machine prints: an empty log and a recorded status, explained.
#[test]
fn a_failure_with_an_empty_log_says_that_it_is_empty() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("dev.log");
    std::fs::write(&log, b"").unwrap();
    let status = crate::paths::exit_status_file(&log);

    std::fs::write(&status, "0").unwrap();
    let silent = explain_failure(state::EXITED, &log, false);
    assert!(silent.starts_with("process exited"), "{silent}");
    assert!(silent.contains("status 0"), "{silent}");
    assert!(
        silent.contains("printed nothing"),
        "an empty log is a fact to state, not an absence to skip over: {silent}"
    );
    assert!(
        silent.contains("not the one that starts it"),
        "and it means something worth saying: {silent}"
    );

    // A timeout is a process that is still up, so no status belongs to
    // it — and none is invented.
    let timeout = "timeout: nothing bound port 17000 in 30s";
    let waited = explain_failure(timeout, &log, false);
    assert!(
        !waited.contains("status 0"),
        "only the phase's own \"it is gone\" takes a status: {waited}"
    );
    assert!(waited.starts_with(timeout), "{waited}");

    // And a log with something in it is explained by the log.
    std::fs::write(&log, "Error: listen EADDRINUSE :::17342\n").unwrap();
    std::fs::write(&status, "1").unwrap();
    let loud = explain_failure(state::EXITED, &log, false);
    assert!(loud.contains("status 1"), "{loud}");
    assert!(
        !loud.contains("printed nothing"),
        "it printed something: {loud}"
    );
    assert!(
        loud.contains("17342"),
        "and that is what explains it: {loud}"
    );
}

#[test]
fn a_failure_the_log_explains_carries_the_hint() {
    let mut fx = fixture();
    // A port that is already taken, reported the way Node reports it.
    with_dev(
        &mut fx,
        dev("echo 'Error: listen EADDRINUSE: address already in use :::3000' && exit 1"),
    );
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);

    assert!(wait_until(Duration::from_secs(10), || matches!(
        refresh(&fx.paths).state.worktrees[&name].processes["dev"].phase,
        Phase::Failed { .. }
    )));
    let phase = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
        .phase
        .clone();
    let Phase::Failed { reason, .. } = &phase else {
        panic!("expected a failure, got {phase:?}");
    };
    assert!(reason.starts_with("process exited"), "{reason}");
    assert!(reason.contains("3000"), "the hint names the port: {reason}");
    let again = refresh(&fx.paths).state.worktrees[&name].processes["dev"]
        .phase
        .clone();
    let Phase::Failed { reason: again, .. } = &again else {
        panic!("still failed: {again:?}");
    };
    assert_eq!(
        again, reason,
        "the hint is written once, not appended again on every read"
    );
}

#[test]
fn refresh_reports_a_state_file_it_cannot_use_instead_of_failing() {
    let fx = fixture();
    fx.paths.ensure_home().unwrap();
    std::fs::write(fx.paths.state_file(), "{ not json").unwrap();
    let refreshed = refresh(&fx.paths);
    assert!(refreshed.state.worktrees.is_empty());
    let warning = refreshed.warning.expect("a broken state file is reported");
    assert!(warning.contains("state"), "{warning}");
    assert_eq!(
        std::fs::read_to_string(fx.paths.state_file()).unwrap(),
        "{ not json",
        "and it is never overwritten"
    );
}

#[test]
fn ownership_still_reads_through_refresh() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let worktrees = ls(&fx.paths).unwrap();
    let owned = created_by_pando(&fx.paths, &worktrees);
    assert_eq!(owned.by_name.get(&name), Some(&true));
    assert!(owned.warning.is_none());
}

#[test]
fn the_prelude_is_prefixed_to_the_command() {
    let mut config = Config::default();
    assert_eq!(with_prelude(&config, "pnpm dev"), "pnpm dev");
    config.runtime.prelude = Some("  ".to_string());
    assert_eq!(
        with_prelude(&config, "pnpm dev"),
        "pnpm dev",
        "a blank prelude adds nothing"
    );
    config.runtime.prelude = Some("nvm use 22".to_string());
    assert_eq!(
        with_prelude(&config, "pnpm dev"),
        "nvm use 22 && {\npnpm dev\n}"
    );
}

// `prelude && a; b` ran `b` when the prelude failed: the `&&` bound only
// the first command of the line.
#[test]
fn a_failed_prelude_runs_no_part_of_the_command() {
    let mut config = Config::default();
    config.runtime.prelude = Some("false".to_string());
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(with_prelude(
            &config,
            "echo one; echo two # trailing comment",
        ))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    config.runtime.prelude = Some("true".to_string());
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(with_prelude(
            &config,
            "echo one; exec echo two # trailing comment",
        ))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one\ntwo\n");
}

/// A repo with one commit, a gitignore listing `.env`, and an untracked
/// ignored `.env` present so provisioning has something to link.
fn fixture() -> Fx {
    let dir = tempdir().unwrap();
    let root = dir.path().join("acme-shop");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(root.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
    std::fs::write(root.join("README.md"), "# acme\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "root"]);
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();

    let project = ProjectRef::from_root(&root).unwrap();
    let paths = PandoPaths::new(dir.path().join("pando-home"), project);
    Fx {
        root: paths.root().to_path_buf(),
        paths,
        config: Config::default(),
        _dir: dir,
    }
}

/// The same fixture, cloned from a bare origin so remote-tracking refs
/// exist. `remote_branches` are pushed to origin and not checked out.
fn fixture_with_origin(remote_branches: &[&str]) -> Fx {
    let dir = tempdir().unwrap();
    let bare = dir.path().join("origin.git");
    git(
        dir.path(),
        &[
            "init",
            "--bare",
            "--quiet",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );
    let seed = dir.path().join("seed");
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    std::fs::write(seed.join(".gitignore"), ".env\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "--quiet", "-m", "root"]);
    git(&seed, &["push", "--quiet", "origin", "main"]);
    for branch in remote_branches {
        git(&seed, &["checkout", "--quiet", "-b", branch]);
        git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "remote work"],
        );
        git(&seed, &["push", "--quiet", "origin", branch]);
    }
    let root = dir.path().join("acme-shop");
    git(
        dir.path(),
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();

    let project = ProjectRef::from_root(&root).unwrap();
    let paths = PandoPaths::new(dir.path().join("pando-home"), project);
    Fx {
        root: paths.root().to_path_buf(),
        paths,
        config: Config::default(),
        _dir: dir,
    }
}

fn upstream_of(root: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn sanitize_turns_slashes_into_plus_signs() {
    assert_eq!(sanitize_branch_to_dir("feat/checkout"), "feat+checkout");
    assert_eq!(sanitize_branch_to_dir("a/b/c"), "a+b+c");
    assert_eq!(sanitize_branch_to_dir("plain"), "plain");
}

#[test]
fn new_creates_the_branch_and_worktree_under_pando_home() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();

    assert_eq!(name, "feat+one");
    let target = fx.worktrees_dir().join("feat+one");
    assert!(
        target.is_dir(),
        "worktree not created at {}",
        target.display()
    );
    assert!(
        target.starts_with(&fx.paths.home),
        "worktrees must live under pando's home by default"
    );
    assert_eq!(fx.names(), vec!["feat+one"]);
}

#[test]
fn new_records_created_by_pando_in_state() {
    let fx = fixture();
    new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let rec = fx.state().worktrees.get("feat+one").cloned().unwrap();
    assert!(rec.created_by_pando);
    assert_eq!(
        rec.path,
        fx.worktrees_dir().join("feat+one").canonicalize().unwrap()
    );
}

// A tracked new branch would turn a later `git pull` into "merge main
// into my feature branch".
#[test]
fn a_new_branch_has_no_upstream() {
    let fx = fixture_with_origin(&[]);
    new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    assert_eq!(
        upstream_of(&fx.root, "feat/one"),
        None,
        "a forked branch must not track its base"
    );
}

#[test]
fn new_with_an_existing_local_branch_checks_it_out() {
    let fx = fixture();
    git(&fx.root, &["branch", "feat/existing"]);
    git(
        &fx.root,
        &["commit", "--quiet", "--allow-empty", "-m", "main moves on"],
    );

    let name = new(&fx.paths, &fx.config, "feat/existing", None, &noop).unwrap();
    let head = Command::new("git")
        .arg("-C")
        .arg(fx.worktrees_dir().join(&name))
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "feat/existing",
        "an existing branch is checked out, not recreated"
    );
}

#[test]
fn new_with_a_remote_only_branch_tracks_the_remote() {
    let fx = fixture_with_origin(&["feat/remote"]);
    new(&fx.paths, &fx.config, "feat/remote", None, &noop).unwrap();
    assert_eq!(
        upstream_of(&fx.root, "feat/remote").as_deref(),
        Some("origin/feat/remote"),
        "checking out a remote branch should track it"
    );
}

// The remote-tracking ref does not exist locally yet: pando has to fetch
// before it can tell a remote branch from a brand new one.
#[test]
fn new_fetches_a_remote_branch_that_has_not_been_fetched_yet() {
    let fx = fixture_with_origin(&[]);
    let bare = fx.root.parent().unwrap().join("origin.git");
    let seed = fx.root.parent().unwrap().join("seed");
    git(&seed, &["checkout", "--quiet", "-b", "feat/late"]);
    git(&seed, &["commit", "--quiet", "--allow-empty", "-m", "late"]);
    git(&seed, &["push", "--quiet", "origin", "feat/late"]);
    assert!(bare.exists());
    assert!(!ref_exists(&fx.root, "refs/remotes/origin/feat/late"));

    new(&fx.paths, &fx.config, "feat/late", None, &noop).unwrap();
    assert_eq!(
        upstream_of(&fx.root, "feat/late").as_deref(),
        Some("origin/feat/late")
    );
}

fn open_pr(number: u32, branch: &str, cross_repository: bool) -> worktree::PrInfo {
    worktree::PrInfo {
        number,
        title: format!("PR {number}"),
        branch: branch.into(),
        author: "someone".into(),
        draft: false,
        state: worktree::PrState::Open,
        url: String::new(),
        cross_repository,
    }
}

fn head_of(dir: &Path, rev: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", rev])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_pull_request_from_origin_checks_out_its_own_branch() {
    let fx = fixture_with_origin(&["feat/pr"]);
    let name = new_for_pr(&fx.paths, &fx.config, &open_pr(3, "feat/pr", false), &noop).unwrap();
    assert_eq!(name, "feat+pr");
    assert_eq!(
        upstream_of(&fx.root, "feat/pr").as_deref(),
        Some("origin/feat/pr")
    );
}

// A fork's branch is on origin only as `refs/pull/<n>/head`, and its name —
// here `main` — is one the main checkout already has.
#[test]
fn a_pull_request_from_a_fork_is_fetched_into_a_branch_of_its_own() {
    let fx = fixture_with_origin(&[]);
    let seed = fx.root.parent().unwrap().join("seed");
    git(
        &seed,
        &["commit", "--quiet", "--allow-empty", "-m", "fork work"],
    );
    git(
        &seed,
        &["push", "--quiet", "origin", "HEAD:refs/pull/7/head"],
    );
    let fork_head = head_of(&seed, "HEAD");

    let name = new_for_pr(&fx.paths, &fx.config, &open_pr(7, "main", true), &noop).unwrap();
    assert_eq!(name, "pr-7+main");
    assert_eq!(head_of(&fx.worktrees_dir().join(&name), "HEAD"), fork_head);
    assert_eq!(fx.names(), vec!["pr-7+main"]);
}

// Not an empty branch of the same name, which `new` would fork: a
// worktree that looks like the pull request and holds none of it.
#[test]
fn a_pull_request_whose_branch_is_not_on_origin_is_refused() {
    let fx = fixture_with_origin(&[]);
    let err = new_for_pr(
        &fx.paths,
        &fx.config,
        &open_pr(4, "feat/elsewhere", false),
        &noop,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("is not on origin"), "{err:#}");
    assert!(!ref_exists(&fx.root, "refs/heads/feat/elsewhere"));
    assert!(fx.names().is_empty());
}

#[test]
fn a_fork_pull_request_origin_does_not_have_leaves_no_branch_behind() {
    let fx = fixture_with_origin(&[]);
    let err = new_for_pr(&fx.paths, &fx.config, &open_pr(9, "main", true), &noop).unwrap_err();
    assert!(format!("{err:#}").contains("could not fetch #9"), "{err:#}");
    assert!(!ref_exists(&fx.root, "refs/heads/pr-9/main"));
    assert!(fx.names().is_empty());
}

#[test]
fn new_forks_from_the_requested_base() {
    let fx = fixture();
    git(&fx.root, &["checkout", "--quiet", "-b", "release"]);
    git(
        &fx.root,
        &["commit", "--quiet", "--allow-empty", "-m", "release only"],
    );
    git(&fx.root, &["checkout", "--quiet", "main"]);

    let name = new(&fx.paths, &fx.config, "fix/one", Some("release"), &noop).unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(fx.worktrees_dir().join(&name))
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "release only");
}

#[test]
fn branch_rules_choose_the_base_when_no_argument_is_given() {
    let mut fx = fixture();
    git(&fx.root, &["checkout", "--quiet", "-b", "beta"]);
    git(
        &fx.root,
        &["commit", "--quiet", "--allow-empty", "-m", "beta only"],
    );
    git(&fx.root, &["checkout", "--quiet", "main"]);
    fx.config.branches.rules = vec![crate::config::BranchRule {
        match_: "*-beta".into(),
        base: "beta".into(),
    }];

    let name = new(&fx.paths, &fx.config, "fix/thing-beta", None, &noop).unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(fx.worktrees_dir().join(&name))
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "beta only");
}

// A bare base name must mean "current origin state", not a local branch
// that has not been pulled in weeks.
#[test]
fn a_bare_base_name_prefers_the_remote_tracking_ref() {
    let fx = fixture_with_origin(&[]);
    let seed = fx.root.parent().unwrap().join("seed");
    git(&seed, &["checkout", "--quiet", "main"]);
    git(
        &seed,
        &["commit", "--quiet", "--allow-empty", "-m", "origin moved"],
    );
    git(&seed, &["push", "--quiet", "origin", "main"]);
    git(&fx.root, &["fetch", "--quiet", "origin"]);

    let name = new(&fx.paths, &fx.config, "feat/fresh", Some("main"), &noop).unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(fx.worktrees_dir().join(&name))
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "origin moved",
        "a bare base should fork from origin/main, not the stale local main"
    );
}

// A base with a slash in its name — `release/1.2`, as a `[branches].rules`
// base often is — was taken for one already qualified with its remote:
// refused when only origin had it, and forked from a stale local copy when
// there was one.
#[test]
fn a_base_with_a_slash_in_its_name_prefers_the_remote_tracking_ref_too() {
    let fx = fixture_with_origin(&["release/1.2"]);
    let last_commit = |name: &str| {
        let out = Command::new("git")
            .arg("-C")
            .arg(fx.worktrees_dir().join(name))
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let name = new(&fx.paths, &fx.config, "fix/one", Some("release/1.2"), &noop).unwrap();
    assert_eq!(last_commit(&name), "remote work", "only origin has it");

    git(&fx.root, &["branch", "release/1.2", "origin/release/1.2"]);
    let seed = fx.root.parent().unwrap().join("seed");
    git(
        &seed,
        &["commit", "--quiet", "--allow-empty", "-m", "origin moved"],
    );
    git(&seed, &["push", "--quiet", "origin", "release/1.2"]);
    git(&fx.root, &["fetch", "--quiet", "origin"]);
    let name = new(&fx.paths, &fx.config, "fix/two", Some("release/1.2"), &noop).unwrap();
    assert_eq!(
        last_commit(&name),
        "origin moved",
        "the stale local copy is not the fork point"
    );

    let name = new(
        &fx.paths,
        &fx.config,
        "fix/three",
        Some("origin/main"),
        &noop,
    )
    .unwrap();
    assert_eq!(
        last_commit(&name),
        "root",
        "a qualified name is used as it is"
    );
}

#[test]
fn new_refuses_an_invalid_branch_name_before_creating_anything() {
    let fx = fixture();
    for bad in ["feat//two", "-leading-dash", "has space", "", "ends.lock"] {
        assert!(
            new(&fx.paths, &fx.config, bad, None, &noop).is_err(),
            "{bad:?} should be refused"
        );
    }
    assert!(
        !fx.worktrees_dir().exists(),
        "a refused create must not even make the worktrees directory"
    );
}

#[test]
fn new_refuses_a_name_already_checked_out_in_another_worktree() {
    let fx = fixture();
    new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("already exists"),
        "unexpected error: {err:#}"
    );
    assert_eq!(fx.names().len(), 1);
}

#[test]
fn new_refuses_a_branch_checked_out_elsewhere_and_surfaces_gits_reason() {
    let fx = fixture();
    // Adopt a worktree in another location holding the branch, then ask
    // for the same branch under a different directory name.
    let elsewhere = fx.root.parent().unwrap().join("elsewhere");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "taken",
            elsewhere.to_str().unwrap(),
        ],
    );
    let err = new(&fx.paths, &fx.config, "taken", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("git worktree add failed"), "{msg}");
    assert!(msg.contains("taken"), "{msg}");
}

#[test]
fn new_refuses_a_provision_path_that_is_not_gitignored() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec!["README.md".into()]);
    let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(msg.contains("README.md"), "{msg}");
    assert!(
        fx.names().is_empty(),
        "the refusal must happen before git is asked to do anything"
    );
}

#[test]
fn new_refuses_a_provision_path_outside_the_repository() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec!["../escape.env".into()]);
    let err = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("check-ignore"),
        "git's own refusal should be surfaced: {err:#}"
    );
}

#[test]
fn provisioned_files_are_symlinked_by_default_and_copied_on_request() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec![".env".into()]);
    let name = new(&fx.paths, &fx.config, "feat/link", None, &noop).unwrap();
    let linked = fx.worktrees_dir().join(&name).join(".env");
    assert!(
        std::fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink(),
        "link mode must produce a symlink"
    );
    assert_eq!(std::fs::read_to_string(&linked).unwrap(), "SECRET=1\n");

    fx.config.project.provision_mode = ProvisionMode::Copy;
    let name = new(&fx.paths, &fx.config, "feat/copy", None, &noop).unwrap();
    let copied = fx.worktrees_dir().join(&name).join(".env");
    assert!(
        !std::fs::symlink_metadata(&copied)
            .unwrap()
            .file_type()
            .is_symlink(),
        "copy mode must produce a real file"
    );
    assert_eq!(std::fs::read_to_string(&copied).unwrap(), "SECRET=1\n");
}

// A workspace app given the root `.env` is given the developer's own
// file under another path — linked like the root one, not seeded like an
// example, and still only because the repository ignores that path.
#[test]
fn the_root_env_given_to_a_workspace_app_is_linked_not_seeded() {
    let mut fx = fixture();
    std::fs::create_dir_all(fx.root.join("apps/api")).unwrap();
    std::fs::write(fx.root.join("apps/api/package.json"), "{}\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "an app"]);
    fx.config.project.provision = Some(vec![".env".into(), "apps/api/.env".into()]);
    fx.config.project.provision_from =
        BTreeMap::from([("apps/api/.env".to_string(), ".env".to_string())]);
    let said = std::cell::RefCell::new(Vec::<String>::new());
    let name = new(
        &fx.paths,
        &fx.config,
        "feat/app-env",
        None,
        &|line: &str| said.borrow_mut().push(line.to_string()),
    )
    .unwrap();
    let linked = fx.worktrees_dir().join(&name).join("apps/api/.env");
    assert!(
        std::fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the developer's own file is linked, as the mode says"
    );
    assert_eq!(std::fs::read_to_string(&linked).unwrap(), "SECRET=1\n");
    assert!(
        !said.borrow().iter().any(|line| line.starts_with("seeding")),
        "it is not a seed: {:?}",
        said.borrow()
    );
    // Nothing shows in the worktree's status: the path is ignored.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(fx.worktrees_dir().join(&name))
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.trim().is_empty(), "{status}");
}

// `git worktree add` leaves every submodule empty, and a build that needs
// one fails far from the cause. pando does not fill them — that clones
// into `.git` — but it says which, and how.
#[test]
fn new_names_the_submodules_it_left_empty() {
    let fx = fixture();
    let _library = with_submodule(&fx);

    let said = std::cell::RefCell::new(Vec::<String>::new());
    let name = new(&fx.paths, &fx.config, "feat/sub", None, &|line: &str| {
        said.borrow_mut().push(line.to_string())
    })
    .unwrap();
    let said = said.borrow();
    let line = said
        .iter()
        .find(|line| line.starts_with("submodules left empty"))
        .unwrap_or_else(|| panic!("nothing said about the submodule: {said:?}"));
    assert!(line.contains("vendor/lib"), "{line}");
    assert!(line.contains("submodule update --init"), "{line}");
    assert!(
        !fx.worktrees_dir()
            .join(&name)
            .join("vendor/lib/lib.txt")
            .exists(),
        "and pando did not fill it"
    );

    // A project with no submodules hears nothing about them.
    let plain = fixture();
    let quiet = std::cell::RefCell::new(Vec::<String>::new());
    new(
        &plain.paths,
        &plain.config,
        "feat/plain",
        None,
        &|line: &str| quiet.borrow_mut().push(line.to_string()),
    )
    .unwrap();
    assert!(!quiet.borrow().iter().any(|l| l.contains("submodule")));
}

/// A submodule at `vendor/lib` committed to the fixture's main branch,
/// cloned from the repository in the returned directory.
fn with_submodule(fx: &Fx) -> TempDir {
    let library = tempdir().unwrap();
    git(
        library.path(),
        &["init", "--quiet", "--initial-branch=main"],
    );
    std::fs::write(library.path().join("lib.txt"), "x\n").unwrap();
    git(library.path(), &["add", "."]);
    git(library.path(), &["commit", "--quiet", "-m", "lib"]);
    git(
        &fx.root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &library.path().display().to_string(),
            "vendor/lib",
        ],
    );
    git(&fx.root, &["commit", "--quiet", "-m", "a submodule"]);
    library
}

// A clean worktree with its submodules filled — which `new` says how to
// do — passes `git status`, and `git worktree remove` still refuses it
// without `--force`. Asked only then, `rm` had already stopped the dev
// server and taken the worktree's volumes down.
#[test]
fn rm_refuses_a_worktree_with_a_submodule_before_stopping_anything() {
    let mut fx = fixture();
    let _library = with_submodule(&fx);
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/sub");
    git(
        &fx.worktrees_dir().join(&name),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--quiet",
        ],
    );
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pid = outcome.started[0].record.pid;

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("submodules in it (vendor/lib)"), "{msg}");
    assert!(msg.contains("nothing was stopped or removed"), "{msg}");
    assert!(crate::remedy::for_cli(&msg).contains("--force"), "{msg}");
    assert!(proc::is_alive(pid), "the dev server was stopped first");
    assert!(fx.state().worktrees[&name].processes.contains_key("dev"));
    assert_eq!(fx.names(), vec![name.clone()]);

    rm(&fx.paths, &name, false, true).unwrap();
    assert!(fx.names().is_empty());

    // Left empty, as `new` leaves them, they are nothing git objects to.
    let empty = worktree_named(&fx, "feat/empty");
    rm(&fx.paths, &empty, false, false).unwrap();
    assert!(fx.names().is_empty());
}

#[test]
fn a_missing_provision_source_is_skipped_rather_than_invented() {
    let mut fx = fixture();
    std::fs::remove_file(fx.root.join(".env")).unwrap();
    fx.config.project.provision = Some(vec![".env".into()]);
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    assert!(!fx.worktrees_dir().join(&name).join(".env").exists());
}

/// The fixture as a fresh clone leaves it: the example is tracked and
/// here, the local file it is an example of is gitignored and never
/// arrived.
fn fresh_clone_fixture() -> Fx {
    let fx = fixture();
    std::fs::remove_file(fx.root.join(".env")).unwrap();
    std::fs::write(fx.root.join(".env.example"), EXAMPLE).unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "ship an example"]);
    fx
}

const EXAMPLE: &str = "PORT=3000\nDATABASE_URL=postgres://acme@localhost:5432/acme\n";

fn seeded(fx: &Fx) -> Config {
    let mut config = fx.config.clone();
    config.project.provision = Some(vec![".env".to_string()]);
    config.project.provision_from =
        BTreeMap::from([(".env".to_string(), ".env.example".to_string())]);
    config
}

// A fresh clone has nothing gitignored and present, so there was
// nothing to provision and no question either — and every worktree came
// out without the file the app reads.
#[test]
fn a_clone_with_no_local_files_is_offered_the_example_beside_them() {
    let fx = fresh_clone_fixture();
    let (ask, asked) = scripted(vec![Answer::Choice(0)]);
    let config = resolve_for_new(&fx.paths, &fx.config, &ask, &noop).unwrap();

    let questions = asked.borrow();
    assert_eq!(
        questions.iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::Provision],
        "copying a tracked example into a worktree is asked, never assumed"
    );
    assert!(
        questions[0].allow_none,
        "and it has to be declinable, or it is asked on every new"
    );
    assert_eq!(config.project.provision_paths(), [".env".to_string()]);
    assert_eq!(config.project.provision_from[".env"], ".env.example");

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains(r#"provision = [".env"]"#), "{written}");
    assert!(
        written.contains(r#"provision_from = { ".env" = ".env.example" }"#),
        "{written}"
    );

    // Asked once: the file pando wrote loads and answers the slot.
    let loaded = crate::config::load(&fx.paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    resolve_for_new(&fx.paths, &loaded.config, &refuse, &noop).unwrap();
}

// `--yes` must not take a seed. Every other slot's options are a
// command or a name pando authored and can vouch for; this one makes
// pando create a file out of contents it did not write and cannot
// read. On a clone with nothing local at all there is no safe option
// to fall back to, so the question is what an unattended run gets —
// loudly, with the source named, rather than a file copied blind.
#[test]
fn yes_refuses_to_seed_a_file_on_a_developers_behalf() {
    let fx = fresh_clone_fixture();
    let (ask, asked) = scripted(vec![Answer::Auto(0)]);
    let question = asked.clone();
    let refuse_auto = move |q: &Question| -> Result<Answer> {
        question.borrow_mut().push(q.clone());
        assert_eq!(
            q.preselect, None,
            "nothing here is an option --yes may take: {:?}",
            q.options
        );
        Err(anyhow::anyhow!("--yes would have printed this question"))
    };
    let err = resolve_for_new(&fx.paths, &fx.config, &refuse_auto, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("--yes"), "{err:#}");
    drop(ask);

    // And nothing was written: a refused answer is not an answer.
    assert!(!fx.paths.config_file().exists());
}

// With a local file already here, that is the answer `--yes` takes —
// and the seed beside it is left alone.
#[test]
fn yes_takes_the_files_that_are_already_here_and_never_the_seed() {
    let fx = fresh_clone_fixture();
    std::fs::write(fx.root.join(".env.local"), "FLAG=1\n").unwrap();
    std::fs::write(
        fx.root.join(".gitignore"),
        ".env\n.env.local\nnode_modules/\n",
    )
    .unwrap();
    git(
        &fx.root,
        &["commit", "--quiet", "-am", "ignore .env.local too"],
    );

    let (ask, asked) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_for_new(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(asked.borrow()[0].preselect, Some(0));
    assert_eq!(
        config.project.provision_paths(),
        [".env.local".to_string()],
        "--yes took what is already here"
    );
    assert!(
        config.project.provision_from.is_empty(),
        "and copied nothing from an example"
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(!written.contains("provision_from"), "{written}");
}

// The human path still gets the offer, and taking it says out loud
// which file was copied from where.
#[test]
fn choosing_the_seed_says_which_source_it_copied() {
    let fx = fresh_clone_fixture();
    let (ask, _) = scripted(vec![Answer::Choice(0)]);
    let config = resolve_for_new(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(config.project.provision_from[".env"], ".env.example");

    let said = std::cell::RefCell::new(Vec::new());
    new(&fx.paths, &config, "feat/one", None, &|line| {
        said.borrow_mut().push(line.to_string())
    })
    .unwrap();
    assert!(
        said.borrow()
            .iter()
            .any(|line| line == "seeding .env from .env.example"),
        "the notice names the source, so it can be read: {:?}",
        said.borrow()
    );
}

// "No thanks" is an answer. Without somewhere to put it the question
// came back on every `new`, which is the thing "ask once" is about.
#[test]
fn declining_the_provision_question_is_written_down_as_no_files() {
    let fx = fresh_clone_fixture();
    let (ask, _) = scripted(vec![Answer::None]);
    let config = resolve_for_new(&fx.paths, &fx.config, &ask, &noop).unwrap();
    assert_eq!(config.project.provision, Some(Vec::new()));

    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(
        written.contains("provision = []"),
        "an empty list, so it reads as answered rather than missing: {written}"
    );
    resolve_for_new(&fx.paths, &config, &refuse, &noop).unwrap();
}

// Invariant 1: the file lands in the worktree, so the worktree's own
// gitignore authorises it — and it is a copy, because a symlink to the
// tracked example would make every edit in the worktree a write into
// the repository.
#[test]
fn a_seeded_file_is_a_copy_of_the_example_and_never_a_link_to_it() {
    let fx = fresh_clone_fixture();
    let config = seeded(&fx);
    let name = new(&fx.paths, &config, "feat/one", None, &noop).unwrap();
    let seeded_file = fx.worktrees_dir().join(&name).join(".env");
    assert!(
        !std::fs::symlink_metadata(&seeded_file)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a link would put the worktree's edits inside the repository"
    );
    assert_eq!(std::fs::read_to_string(&seeded_file).unwrap(), EXAMPLE);
    assert_eq!(
        std::fs::read_to_string(fx.root.join(".env.example")).unwrap(),
        EXAMPLE,
        "and the example itself is untouched"
    );
    assert!(!fx.root.join(".env").exists(), "nothing was written here");
}

// The example is the fallback, not the source. The moment the developer
// writes their own file, that is what every new worktree gets — and it
// is linked, as it always was.
#[test]
fn the_checkouts_own_file_beats_the_example_it_would_have_been_seeded_from() {
    let fx = fresh_clone_fixture();
    std::fs::write(fx.root.join(".env"), "SECRET=real\n").unwrap();
    let config = seeded(&fx);
    let name = new(&fx.paths, &config, "feat/one", None, &noop).unwrap();
    let provisioned = fx.worktrees_dir().join(&name).join(".env");
    assert!(
        std::fs::symlink_metadata(&provisioned)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a real local file is linked, exactly as before"
    );
    assert_eq!(
        std::fs::read_to_string(&provisioned).unwrap(),
        "SECRET=real\n"
    );
}

// The gitignore that authorises the write is the worktree's, and a
// branch can carry an older one. A seeded file is no exception, and the
// refusal has to unwind the worktree it was discovered in.
#[test]
fn a_branch_that_does_not_ignore_the_seeded_file_is_refused_and_unwound() {
    let fx = fresh_clone_fixture();
    git(&fx.root, &["checkout", "--quiet", "-b", "legacy"]);
    std::fs::write(fx.root.join(".gitignore"), "node_modules/\n").unwrap();
    git(
        &fx.root,
        &["commit", "--quiet", "-am", "an older gitignore"],
    );
    git(&fx.root, &["checkout", "--quiet", "main"]);

    let config = seeded(&fx);
    let err = new(&fx.paths, &config, "legacy", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(msg.contains(".env"), "{msg}");
    assert!(
        fx.names().is_empty(),
        "the half-created worktree must have been unwound"
    );
}

#[test]
fn rm_removes_a_pando_worktree_with_its_logs_and_data() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    std::fs::create_dir_all(fx.paths.logs_dir(&name)).unwrap();
    std::fs::write(fx.paths.log_file(&name, "dev"), "log line\n").unwrap();
    std::fs::create_dir_all(fx.paths.data_dir(&name)).unwrap();

    rm(&fx.paths, &name, false, false).unwrap();

    assert!(fx.names().is_empty());
    assert!(!fx.worktrees_dir().join(&name).exists());
    assert!(!fx.paths.logs_dir(&name).exists());
    assert!(!fx.paths.data_dir(&name).exists());
    assert!(!fx.state().worktrees.contains_key(&name));
}

// Any command that creates a directory under pando's home must make the
// home itself 0700 first: later phases copy env files in there, and a
// home created by a stray `create_dir_all` would carry the umask.
#[test]
fn a_first_run_rm_still_creates_a_private_home() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let adopted = fx.root.parent().unwrap().join("adopted");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );
    assert!(!fx.paths.home.exists(), "nothing has written the home yet");

    rm(&fx.paths, "adopted", true, false).unwrap();

    let mode = std::fs::metadata(&fx.paths.home)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o700,
        "pando home must be private from the first write"
    );
}

#[test]
fn rm_keeps_the_branch() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    rm(&fx.paths, &name, false, false).unwrap();
    assert!(
        ref_exists(&fx.root, "refs/heads/feat/one"),
        "rm removes the worktree, not the work"
    );
}

#[test]
fn rm_refuses_an_adopted_worktree_without_yes() {
    let fx = fixture();
    let adopted = fx.root.parent().unwrap().join("adopted");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );

    let err = rm(&fx.paths, "adopted", false, false).unwrap_err();
    assert!(
        crate::remedy::for_cli(&format!("{err:#}")).contains("--yes"),
        "unexpected error: {err:#}"
    );
    assert_eq!(fx.names(), vec!["adopted"]);

    rm(&fx.paths, "adopted", true, false).unwrap();
    assert!(fx.names().is_empty());
}

#[test]
fn rm_refuses_a_dirty_worktree_without_force() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    std::fs::write(fx.worktrees_dir().join(&name).join("scratch.txt"), "wip").unwrap();

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("modified or untracked"), "{msg}");
    assert!(
        crate::remedy::for_cli(&msg).contains("--force") && !msg.contains("--"),
        "{msg}"
    );
    assert!(
        msg.contains("scratch.txt"),
        "it names what is in the way: {msg}"
    );
    assert_eq!(fx.names(), vec![name.clone()]);

    rm(&fx.paths, &name, false, true).unwrap();
    assert!(fx.names().is_empty());
}

// A plain `git status` refreshes a stale index: it takes `index.lock` and
// rewrites the index inside `.git`, which a `git commit` in the worktree
// can trip over and a refused `rm` left behind.
#[test]
fn rm_and_the_hook_check_leave_a_worktrees_index_alone() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let worktree = fx.worktrees_dir().join(&name);
    let out = Command::new("git")
        .arg("-C")
        .arg(&worktree)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .unwrap();
    let index = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("index");
    let before = std::fs::read(&index).unwrap();
    // Stale stat data for a tracked file, with its content unchanged.
    std::fs::File::options()
        .write(true)
        .open(worktree.join("README.md"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    std::fs::write(worktree.join("scratch.txt"), "wip").unwrap();

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("modified or untracked"),
        "{err:#}"
    );
    assert_eq!(porcelain_status(&worktree), vec!["?? scratch.txt"]);
    assert!(
        std::fs::read(&index).unwrap() == before,
        "a status probe rewrote the worktree's index"
    );
}

// A `git status` that cannot answer (a timeout, a broken gitfile) read as
// "clean": `rm` stopped the dev server and took the volumes down, and only
// then did `git worktree remove` refuse a dirty tree. Not knowing has to
// stop it before anything is touched.
#[test]
fn rm_refuses_before_stopping_anything_when_git_cannot_say_whether_it_is_dirty() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pid = outcome.started[0].record.pid;
    let gitfile = fx.worktrees_dir().join(&name).join(".git");
    let saved = std::fs::read_to_string(&gitfile).unwrap();
    std::fs::write(&gitfile, "gitdir: /does/not/exist\n").unwrap();

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("could not tell whether"), "{msg}");
    assert!(crate::remedy::for_cli(&msg).contains("--force"), "{msg}");
    assert!(proc::is_alive(pid), "the dev server was stopped first");
    std::fs::write(&gitfile, saved).unwrap();
    assert_eq!(fx.names(), vec![name.clone()]);
}

// An ignored, provisioned file is pando's own doing and must never be
// the reason a removal needs --force.
#[test]
fn a_provisioned_env_file_does_not_block_removal() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec![".env".into()]);
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    rm(&fx.paths, &name, false, false).unwrap();
    assert!(fx.names().is_empty());
    assert_eq!(
        std::fs::read_to_string(fx.root.join(".env")).unwrap(),
        "SECRET=1\n",
        "the main checkout's file must survive its symlink being removed"
    );
}

#[test]
fn rm_always_refuses_a_locked_worktree_and_shows_the_reason() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    git(
        &fx.root,
        &[
            "worktree",
            "lock",
            "--reason",
            "benchmark running",
            fx.worktrees_dir().join(&name).to_str().unwrap(),
        ],
    );

    for (yes, force) in [(false, false), (true, false), (true, true)] {
        let err = rm(&fx.paths, &name, yes, force).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("locked"), "{msg}");
        assert!(msg.contains("benchmark running"), "{msg}");
    }
    assert_eq!(fx.names(), vec![name]);
}

#[test]
fn rm_clears_one_prunable_entry_and_leaves_the_others_alone() {
    let fx = fixture();
    let gone = new(&fx.paths, &fx.config, "feat/gone", None, &noop).unwrap();
    let other = new(&fx.paths, &fx.config, "feat/other", None, &noop).unwrap();
    std::fs::remove_dir_all(fx.worktrees_dir().join(&gone)).unwrap();
    std::fs::remove_dir_all(fx.worktrees_dir().join(&other)).unwrap();

    rm(&fx.paths, &gone, false, false).unwrap();

    let left = fx.names();
    assert_eq!(
        left,
        vec![other],
        "removing one prunable entry must not sweep the others"
    );
}

// Once `git worktree prune` has run over a deleted worktree, git no longer
// lists it, and `rm` looked the name up in git's list alone: "no worktree
// named", while doctor named that very command as the way to forget it.
// The record stayed, and so did the process, logs and data it named.
#[test]
fn rm_of_a_worktree_git_has_pruned_takes_down_what_its_record_names() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let pgid = outcome.started[0].record.pgid;
    std::fs::create_dir_all(fx.paths.data_dir(&name)).unwrap();
    std::fs::remove_dir_all(fx.worktrees_dir().join(&name)).unwrap();
    git(&fx.root, &["worktree", "prune"]);
    assert!(fx.names().is_empty(), "git has forgotten it");

    rm(&fx.paths, &name, false, false).unwrap();
    assert!(
        !crate::process::group_alive(pgid),
        "rm must not leave a process behind with no record of it"
    );
    assert!(!fx.state().worktrees.contains_key(&name));
    assert!(!fx.paths.logs_dir(&name).exists());
    assert!(!fx.paths.data_dir(&name).exists());

    // And a name with no record either is still nothing to remove.
    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    assert!(format!("{err:#}").contains("no worktree named"), "{err:#}");
}

// The checkout runs the repository's filters and hooks, an LFS download
// among them. Held through it, the state lock stalled every `ls` and TUI
// worker for as long as it took; and a download that wanted a password
// asked for it on the terminal, over the TUI.
#[test]
fn new_holds_no_state_lock_through_the_checkout_and_lets_it_prompt_for_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let fx = fixture();
    let marks = tempdir().unwrap();
    let hooks = marks.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf '%s' \"$GIT_TERMINAL_PROMPT\" > '{m}/prompt'\ntouch '{m}/started'\n\
             i=0\nwhile [ ! -e '{m}/release' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n",
            m = marks.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &fx.root,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );

    std::thread::scope(|scope| {
        let creating = scope.spawn(|| new(&fx.paths, &fx.config, "feat/slow", None, &noop));
        let started = wait_until(Duration::from_secs(20), || {
            marks.path().join("started").exists()
        });
        let free = started && state::try_lock(&fx.paths.lock_file()).unwrap().is_some();
        std::fs::write(marks.path().join("release"), "").unwrap();
        let name = creating.join().unwrap().unwrap();
        assert!(started, "the checkout's hook never ran");
        assert!(free, "the state lock was held through the checkout");
        assert!(fx.state().worktrees[&name].created_by_pando);
    });
    assert_eq!(
        std::fs::read_to_string(marks.path().join("prompt")).unwrap(),
        "0"
    );
}

// With the lock let go through the checkout, a `start` in another
// terminal can find the worktree while git is still checking it out, and
// record its dev server and ports. `new` then wrote its own record over
// that one, and the running server dropped out of pando's state: nothing
// could stop it, and its ports went to the next worktree. Kept as the
// start wrote it, the record then called the worktree adopted — `rm`
// demanded `--yes` for one pando made — and the error named neither the
// `.env` nor the install it never got, nor a way to get them.
#[test]
fn new_keeps_a_record_another_command_wrote_while_git_checked_out() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = fixture();
    fx.config.project.provision = Some(vec![".env".into()]);
    fx.config.project.install = Some("true".into());
    let marks = tempdir().unwrap();
    let hooks = marks.path().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\ntouch '{m}/started'\n\
             i=0\nwhile [ ! -e '{m}/release' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n",
            m = marks.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        &fx.root,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    let name = sanitize_branch_to_dir("feat/slow");
    let target = fx.worktrees_dir().join(&name);

    let err = std::thread::scope(|scope| {
        let creating = scope.spawn(|| new(&fx.paths, &fx.config, "feat/slow", None, &noop));
        let started = wait_until(Duration::from_secs(20), || {
            marks.path().join("started").exists()
        });
        if started {
            // What a start's record is: adopted, with a live process and
            // its port. The pid is this test's, so no sweep signals it.
            let _lock = state::lock(&fx.paths.lock_file()).unwrap();
            let mut store = state::load(&fx.paths.state_file()).unwrap();
            let mut record = WorktreeRecord::new(std::fs::canonicalize(&target).unwrap(), false);
            let mut live = fake_record(4_000_003);
            live.pid = std::process::id();
            record.processes.insert("dev".to_string(), live);
            record.ports.insert("web".to_string(), 17_000);
            store.worktrees.insert(name.clone(), record);
            state::save(&fx.paths.state_file(), &store).unwrap();
        }
        std::fs::write(marks.path().join("release"), "").unwrap();
        let made = creating.join().unwrap();
        assert!(started, "the checkout's hook never ran");
        made.unwrap_err()
    });
    let msg = format!("{err:#}");
    assert!(msg.contains("another pando command recorded"), "{msg}");
    // What the TUI reads to go to the kept worktree, not "could not create".
    assert!(msg.contains(KEPT_OVER_RACED_RECORD), "{msg}");
    assert!(msg.contains("provisioned files (.env)"), "{msg}");
    assert!(msg.contains("install step"), "{msg}");
    assert!(
        msg.contains("`pando rm feat/slow` and then `pando new feat/slow`"),
        "{msg}"
    );
    assert!(!target.join(".env").exists(), "the refusal provisioned");

    let record = &fx.state().worktrees[&name];
    assert!(
        record.processes.contains_key("dev"),
        "new wrote over the start's record"
    );
    assert_eq!(record.ports.get("web"), Some(&17_000));
    assert!(
        record.created_by_pando,
        "a worktree new made is recorded as adopted"
    );
    assert!(
        fx.names().contains(&name),
        "the worktree went from under the start's dev server"
    );
    let listed = worktree::discover_all(&fx.paths.project).unwrap().worktrees;
    assert_eq!(ownership(&fx.state(), &listed).get(&name), Some(&true));

    // Still listed, so the next mutation's sweep keeps its record.
    worktree_named(&fx, "feat/after");
    assert!(fx.state().worktrees[&name].processes.contains_key("dev"));
}

// git still lists a worktree whose directory was deleted, and `new` said
// it "already exists at" a path that was not there, with nothing about
// the `rm` that clears the entry.
#[test]
fn new_over_a_prunable_entry_names_the_rm_that_clears_it() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/gone", None, &noop).unwrap();
    std::fs::remove_dir_all(fx.worktrees_dir().join(&name)).unwrap();

    let err = new(&fx.paths, &fx.config, "feat/gone", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("prunable"), "{msg}");
    assert!(msg.contains("`pando rm feat/gone`"), "{msg}");
    assert!(!msg.contains("already exists"), "{msg}");

    rm(&fx.paths, &name, false, false).unwrap();
    assert_eq!(
        new(&fx.paths, &fx.config, "feat/gone", None, &noop).unwrap(),
        name
    );
}

// A start of a worktree whose directory was deleted assigned its ports,
// saved them, and failed in the install hook or the spawn with a bare "No
// such file or directory" — read by the CLI as the install command being
// wrong — having asked for a login and made a namespace first.
#[test]
fn a_start_of_a_worktree_whose_directory_is_gone_names_the_rm_that_clears_it() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/gone");
    std::fs::remove_dir_all(fx.worktrees_dir().join(&name)).unwrap();

    let asked = resolve_for_start(
        &fx.paths,
        &fx.config,
        &name,
        Mode::Namespaced,
        &refuse,
        &noop,
    )
    .map(|_| ())
    .unwrap_err();
    let started = start(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    for err in [asked, started] {
        let msg = format!("{err:#}");
        assert!(msg.contains("feat/gone is gone"), "{msg}");
        assert!(msg.contains("`pando rm feat/gone`"), "{msg}");
        assert!(!msg.contains("No such file"), "{msg}");
    }
    let state = fx.state();
    let record = state.worktrees.get(&name);
    assert!(
        record.is_none_or(|r| r.ports.is_empty() && r.processes.is_empty()),
        "{record:?}"
    );
}

#[test]
fn a_restart_of_a_worktree_whose_directory_is_gone_stops_nothing() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/gone");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    std::fs::remove_dir_all(fx.worktrees_dir().join(&name)).unwrap();

    let err = restart(&fx.paths, &fx.config, &name, None, &noop).unwrap_err();
    assert!(
        format!("{err:#}").contains("`pando rm feat/gone`"),
        "{err:#}"
    );
    assert!(
        crate::process::group_alive(report.started[0].record.pgid),
        "a restart that cannot start must not stop"
    );
    assert_eq!(live_processes(&fx, &name), vec!["dev"]);
}

#[test]
fn rm_refuses_the_main_checkout_and_an_unknown_name() {
    let fx = fixture();
    let main_name = worktree::discover_all(&fx.paths.project).unwrap().main.name;
    let err = rm(&fx.paths, &main_name, true, true).unwrap_err();
    assert!(format!("{err:#}").contains("main checkout"), "{err:#}");

    let err = rm(&fx.paths, "nope", true, true).unwrap_err();
    assert!(format!("{err:#}").contains("no worktree named"), "{err:#}");
}

#[test]
fn ls_lists_managed_worktrees_with_enrichment() {
    let fx = fixture();
    new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let listed = ls(&fx.paths).unwrap();
    assert_eq!(listed.len(), 1);
    let w = &listed[0];
    assert_eq!(w.name, "feat+one");
    assert_eq!(w.branch.as_deref(), Some("feat/one"));
    assert!(w.head_sha.is_some());
    assert_eq!(w.dirty, Some(false));
    assert_eq!(w.ahead_behind, Some((0, 0)));
}

#[test]
fn path_prints_the_absolute_canonical_path() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let p = path(&fx.paths, &name).unwrap();
    assert!(p.is_absolute());
    assert_eq!(p, fx.worktrees_dir().join(&name).canonicalize().unwrap());
    assert!(path(&fx.paths, "nope").is_err());
}

#[test]
fn created_by_pando_distinguishes_adopted_worktrees() {
    let fx = fixture();
    new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    let adopted = fx.root.parent().unwrap().join("adopted");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );

    let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
    assert_eq!(owned.by_name.get("feat+one"), Some(&true));
    assert_eq!(
        owned.by_name.get("adopted"),
        None,
        "an adopted worktree has no record"
    );
}

#[test]
fn a_configured_worktrees_dir_outside_the_repository_is_honoured() {
    let mut fx = fixture();
    let elsewhere = fx.root.parent().unwrap().join("custom-trees");
    fx.config.project.worktrees_dir = Some(elsewhere.clone());

    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    assert!(elsewhere.join(&name).is_dir());
    assert_eq!(
        path(&fx.paths, &name).unwrap(),
        elsewhere.join(&name).canonicalize().unwrap()
    );

    rm(&fx.paths, &name, false, false).unwrap();
    assert!(!elsewhere.join(&name).exists());
}

// `ls` labels a worktree "adopted" from the same file `rm` keys its
// confirmation off. When that file cannot be read, both have to say the
// same thing rather than one shrugging and the other failing.
#[test]
fn created_by_pando_reports_a_state_file_it_cannot_use() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/one", None, &noop).unwrap();
    std::fs::write(fx.paths.state_file(), r#"{"version":3,"worktrees":{}}"#).unwrap();

    let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
    assert!(owned.by_name.is_empty());
    let warning = owned
        .warning
        .expect("a state file pando cannot use must be reported, not swallowed");
    assert!(warning.contains("version 3"), "{warning}");

    let err = rm(&fx.paths, &name, true, false).unwrap_err();
    assert_eq!(
        warning,
        format!("{err:#}"),
        "the listing and rm must give the same line"
    );
}

// A record is keyed by basename, and the worktree it was written for can
// be removed behind pando's back. The record that survives must not then
// vouch for a different worktree that happens to share the name — `rm`
// would delete a directory pando never created, without asking.
#[test]
fn a_stale_record_does_not_make_an_unrelated_worktree_ours() {
    let fx = fixture();
    let name = new(&fx.paths, &fx.config, "feat/x", None, &noop).unwrap();
    let ours = fx.worktrees_dir().join(&name);
    git(
        &fx.root,
        &["worktree", "remove", "--force", ours.to_str().unwrap()],
    );
    assert!(
        fx.state().worktrees.contains_key(&name),
        "the record outlives the worktree git forgot"
    );

    let elsewhere = fx.root.parent().unwrap().join("elsewhere").join(&name);
    std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            elsewhere.to_str().unwrap(),
            "feat/x",
        ],
    );

    let owned = created_by_pando(&fx.paths, &ls(&fx.paths).unwrap());
    assert_eq!(
        owned.by_name.get(&name),
        Some(&false),
        "a record for a directory that is gone must not vouch for another one"
    );
    assert!(owned.warning.is_none(), "{:?}", owned.warning);

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    assert!(
        crate::remedy::for_cli(&format!("{err:#}")).contains("--yes"),
        "{err:#}"
    );
    assert!(
        elsewhere.is_dir(),
        "the adopted worktree must still be there"
    );

    rm(&fx.paths, &name, true, false).unwrap();
    assert!(!elsewhere.exists());
}

// A refused `rm` must change nothing at all. Unlinking the provisioned
// files before asking git left the worktree alive and stripped of its
// `.env`, with nothing to re-provision it.
#[test]
fn a_refused_rm_leaves_the_provisioned_files_alone() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec![".env".into()]);
    let name = new(&fx.paths, &fx.config, "feat/p", None, &noop).unwrap();
    let worktree = fx.worktrees_dir().join(&name);
    let env = worktree.join(".env");
    std::fs::write(worktree.join("DIRTY.txt"), "wip").unwrap();

    let err = rm(&fx.paths, &name, false, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("modified or untracked"),
        "{err:#}"
    );
    assert_eq!(fx.names(), vec![name], "the worktree is still there");
    assert!(
        std::fs::symlink_metadata(&env)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "a refused rm must leave the provisioned symlink where it was"
    );
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "SECRET=1\n");
}

// Invariant 1 covers the whole repository, and a linked worktree is part
// of it. Neither pando's home nor the directory it creates worktrees in
// may sit inside any of them.
#[test]
fn write_locations_inside_the_repository_or_a_worktree_are_refused() {
    let mut fx = fixture();
    let linked = fx.root.parent().unwrap().join("linked");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    guard_write_locations(&fx.paths, &fx.config)
        .expect("a home beside the repository is what every test uses");

    fx.config.project.worktrees_dir = Some(linked.join("nested"));
    let err = guard_write_locations(&fx.paths, &fx.config).unwrap_err();
    assert!(
        format!("{err:#}").contains("inside the worktree"),
        "{err:#}"
    );

    fx.config.project.worktrees_dir = None;
    for (home, expected) in [
        (linked.join(".pando"), "inside the worktree"),
        (fx.root.join(".pando"), "inside the repository"),
    ] {
        let paths = PandoPaths::new(&home, fx.paths.project.clone());
        let err = guard_write_locations(&paths, &fx.config).unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{err:#}");
        assert!(!home.exists(), "nothing may be created for a refused home");
    }
}

/// Commits an ignore rule on `main` and a branch whose own committed
/// `.gitignore` predates it, so the main checkout authorises a write the
/// worktree would not.
fn with_a_branch_that_does_not_ignore(fx: &Fx, rel: &str, branch: &str) {
    std::fs::write(
        fx.root.join(".gitignore"),
        format!(".env\nnode_modules/\n{rel}\n"),
    )
    .unwrap();
    git(&fx.root, &["add", ".gitignore"]);
    git(&fx.root, &["commit", "--quiet", "-m", "ignore it"]);
    std::fs::write(fx.root.join(rel), "TOKEN=1\n").unwrap();

    git(&fx.root, &["checkout", "--quiet", "-b", branch]);
    std::fs::write(fx.root.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
    git(&fx.root, &["commit", "--quiet", "-am", "older gitignore"]);
    git(&fx.root, &["checkout", "--quiet", "main"]);
}

// State is read before git is asked to create anything, so a state file
// pando cannot parse refuses while there is still nothing to undo.
#[test]
fn a_broken_state_file_refuses_new_before_anything_is_created() {
    let fx = fixture();
    fx.paths.ensure_home().unwrap();
    std::fs::write(fx.paths.state_file(), "not json").unwrap();

    let err = new(&fx.paths, &fx.config, "feat/b", None, &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("parse state file"), "{msg}");
    assert!(fx.names().is_empty(), "no worktree may have been created");
    assert!(
        !ref_exists(&fx.root, "refs/heads/feat/b"),
        "no branch may have been created"
    );
    assert!(!fx.worktrees_dir().join("feat+b").exists());
}

// The worktree's own gitignore is the last word, so a refusal can happen
// after `git worktree add` — which makes the unwind what keeps `new`
// all-or-nothing.
#[test]
fn a_refusal_after_the_worktree_exists_unwinds_it() {
    let mut fx = fixture();
    with_a_branch_that_does_not_ignore(&fx, "local.pando", "legacy");
    fx.config.project.provision = Some(vec!["local.pando".into()]);

    // A forked branch is pando's own doing, so it goes too.
    let err = new(&fx.paths, &fx.config, "feat/new", Some("legacy"), &noop).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(
        msg.contains("removed"),
        "the error must say what it undid: {msg}"
    );
    assert!(fx.names().is_empty(), "the worktree must be gone");
    assert!(!fx.worktrees_dir().join("feat+new").exists());
    assert!(
        !ref_exists(&fx.root, "refs/heads/feat/new"),
        "a branch pando created must be deleted by the unwind"
    );
    assert!(!fx.state().worktrees.contains_key("feat+new"));

    // An existing branch was only checked out, so it survives.
    let err = new(&fx.paths, &fx.config, "legacy", None, &noop).unwrap_err();
    assert!(format!("{err:#}").contains("not ignored"), "{err:#}");
    assert!(fx.names().is_empty());
    assert!(
        ref_exists(&fx.root, "refs/heads/legacy"),
        "a branch pando did not create must survive the unwind"
    );
}

// ---- the runtime the project asks for --------------------------------

/// A machine a test decides entirely: a home holding the managers it
/// says are installed, and a shell that answers the way it says.
struct FakeMachine {
    home: TempDir,
    /// Every command the shell was asked to run.
    asked: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl FakeMachine {
    /// A machine with nvm installed under its own home.
    fn with_nvm() -> FakeMachine {
        let home = tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".nvm")).unwrap();
        std::fs::write(home.path().join(".nvm/nvm.sh"), "#!/bin/sh\n").unwrap();
        FakeMachine {
            home,
            asked: Default::default(),
        }
    }

    /// A shell that resolves `without` normally, and `with` once a
    /// prelude carrying `needle` is in front of it — which is what a
    /// version manager does.
    fn shell(
        &self,
        without: &'static str,
        needle: &'static str,
        with: &'static str,
    ) -> impl Fn(&str) -> Option<String> + use<'_> {
        let asked = self.asked.clone();
        move |command: &str| {
            asked.borrow_mut().push(command.to_string());
            let version = if !needle.is_empty() && command.contains(needle) {
                with
            } else {
                without
            };
            Some(crate::runtime::probe_reply(
                &format!("/usr/local/bin/node-{version}"),
                version,
            ))
        }
    }

    fn probes(&self) -> usize {
        self.asked.borrow().len()
    }
}

/// A fixture that pins node, which is what `.nvmrc` does.
fn fixture_pinning(file: &str, spec: &str) -> Fx {
    let fx = fixture();
    std::fs::write(fx.root.join(file), format!("{spec}\n")).unwrap();
    fx
}

/// Resolves the one slot these tests are about, on the machine they
/// describe.
fn resolve_runtime_slot(
    fx: &Fx,
    config: &Config,
    ask: Ask<'_>,
    shell: &dyn Fn(&str) -> Option<String>,
    home: &Path,
) -> Result<Config> {
    let machine = Machine {
        shell,
        home: home.to_path_buf(),
    };
    resolve_on(
        &fx.paths,
        config,
        &[Slot::Prelude],
        &[],
        &Answering::asking(ask),
        &noop,
        &machine,
    )
}

fn user_config(fx: &Fx) -> String {
    std::fs::read_to_string(fx.paths.user_config_file()).unwrap_or_default()
}

#[test]
fn a_project_that_pins_no_runtime_is_never_probed() {
    let fx = fixture();
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    resolve_runtime_slot(&fx, &fx.config, &refuse, &shell, machine.home.path()).unwrap();
    assert_eq!(
        machine.probes(),
        0,
        "nothing pinned, nothing to ask a shell"
    );
}

// A match says nothing at all, and is remembered: a start costs one
// extra spawn when the requirement changes, not one on every start.
#[test]
fn a_runtime_this_machine_meets_is_silent_and_probed_once() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("22.11.0", "", "");

    for _ in 0..3 {
        let config =
            resolve_runtime_slot(&fx, &fx.config, &refuse, &shell, machine.home.path()).unwrap();
        assert_eq!(config.runtime.prelude, None, "nothing is written");
    }
    assert_eq!(machine.probes(), 1, "the probe is cached against the pin");
    assert!(
        fx.paths.runtime_cache_file().exists(),
        "and the cache is under pando's home"
    );
}

// The cache only saves a probe. A start whose check passed goes on when
// the pass cannot be written down, and probes again next time.
#[test]
fn a_pass_that_cannot_be_saved_does_not_stop_the_start() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("22.11.0", "", "");
    let cache_dir = fx
        .paths
        .runtime_cache_file()
        .parent()
        .unwrap()
        .to_path_buf();
    fx.paths.ensure_home().unwrap();
    std::fs::create_dir_all(cache_dir.parent().unwrap()).unwrap();
    // A file where the cache's directory would be, so no save can land.
    std::fs::write(&cache_dir, "").unwrap();

    for _ in 0..2 {
        let config =
            resolve_runtime_slot(&fx, &fx.config, &refuse, &shell, machine.home.path()).unwrap();
        assert_eq!(config.runtime.prelude, None);
    }
    assert_eq!(
        machine.probes(),
        2,
        "nothing was remembered, so it asks again"
    );
}

// The case the whole work item exists for: the project pins one
// version, the shell pando spawns in resolves another, and nothing is
// started until that is settled.
#[test]
fn a_mismatch_asks_before_anything_is_started_and_names_the_path() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    // The nvm line works; nothing in front of it does.
    let shell = machine.shell("24.21.0", "nvm.sh", "22.11.0");
    // The nvm option by what it *is*, never by where it sits: the
    // table's order is a product decision, and this test would
    // otherwise start checking a different manager the day it changes.
    let asked: AskedQuestions = Default::default();
    let seen = asked.clone();
    let ask = move |question: &Question| -> Result<Answer> {
        seen.borrow_mut().push(question.clone());
        let index = question
            .options
            .iter()
            .position(|(line, _)| line.contains("nvm.sh"))
            .ok_or_else(|| anyhow::anyhow!("the nvm line was not on offer"))?;
        Ok(Answer::Choice(index))
    };

    let config = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap();

    let question = &asked.borrow()[0];
    assert_eq!(question.slot, Slot::Prelude);
    let report = question.details.join("\n");
    assert!(report.contains("node 22 (.nvmrc)"), "{report}");
    assert!(report.contains("24.21.0"), "{report}");
    assert!(
        report.contains("/usr/local/bin/node-24.21.0"),
        "the path it resolved from, not only the version: {report}"
    );
    assert!(report.contains("nvm"), "which managers are here: {report}");
    assert!(
        report.contains("nvm install 22"),
        "the install command is printed, never run: {report}"
    );
    assert!(
        question
            .options
            .iter()
            .any(|(line, _)| line.contains("nvm.sh")),
        "the fix is on offer: {:?}",
        question.options
    );
    assert!(question.allow_none, "and \"nothing is needed\" is sayable");

    // The answer is about this machine, so it is written to the user
    // layer — and the project's own file is not touched at all.
    assert!(user_config(&fx).contains("nvm.sh"), "{}", user_config(&fx));
    assert!(
        !fx.paths.config_file().exists(),
        "the project layer is for what the project needs, not what this laptop does"
    );
    assert!(config.runtime.prelude.unwrap().contains("nvm.sh"));
}

// Verified before it is written. This line lands in a file every
// project on the machine shares, and `--yes` must not be able to
// persist one that does not work.
#[test]
fn a_prelude_that_does_not_work_is_refused_rather_than_written() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    let (ask, _asked) = scripted(vec![Answer::Custom("true".to_string())]);

    let err = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("does not work"), "{message}");
    assert!(message.contains("24.21.0"), "{message}");
    assert!(
        !fx.paths.user_config_file().exists(),
        "a line that does not work is not written down"
    );
}

// A shim whose pinned version is not installed names that version in its
// error. Read as the version resolving, it passed the check, was cached,
// and let the prelude offered for it be written, and every process then
// died of the shim's error.
#[test]
fn a_shim_whose_version_is_not_installed_is_neither_passed_nor_remembered() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let error = "nodenv: version `22' is not installed (set by .nvmrc)";
    let shell = |_: &str| {
        Some(crate::runtime::probe_failure(
            "/home/dev/.nodenv/shims/node",
            error,
            1,
        ))
    };
    let (ask, asked) = scripted(vec![Answer::Auto(0)]);

    let err = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap_err();
    let report = asked.borrow()[0].details.join("\n");
    assert!(
        report.contains(&format!(
            "finds node at /home/dev/.nodenv/shims/node, and it fails: {error}"
        )),
        "the question says what the shim said: {report}"
    );
    assert!(
        format!("{err:#}").contains("does not work"),
        "the line offered for it is checked the same way: {err:#}"
    );
    assert!(!fx.paths.user_config_file().exists(), "nothing is written");
    assert!(
        !fx.paths.runtime_cache_file().exists(),
        "and nothing is remembered as a pass"
    );
}

// The same refusal, when a program handed the line in, is a mistake in
// its input rather than a failure: the usage-error shape, exit 2, so a
// program can tell "my answer was bad" from "the command broke". A person
// at a prompt still gets the ordinary error above.
#[test]
fn a_program_supplied_prelude_that_does_not_work_is_a_refused_answer() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    let (ask, _asked) = scripted(vec![Answer::Program(Box::new(Answer::Custom(
        "true".to_string(),
    )))]);

    let err = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap_err();
    assert!(
        err.downcast_ref::<RefusedAnswer>().is_some(),
        "a refused answer, not a plain failure: {err:#}"
    );
    assert!(format!("{err:#}").contains("does not work"), "{err:#}");
    assert!(!fx.paths.user_config_file().exists());

    let (ask, _asked) = scripted(vec![Answer::Custom("true".to_string())]);
    let err = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap_err();
    assert!(
        err.downcast_ref::<RefusedAnswer>().is_none(),
        "a person's typed answer is an ordinary error"
    );
}

// The case that is invisible without this check: a prelude is set, and
// it is not doing anything.
#[test]
fn a_prelude_that_is_set_and_still_wrong_stops_the_start() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    let mut config = fx.config.clone();
    config.runtime.prelude = Some("nvm use 22".to_string());
    // Written where the answer to this question goes, so the report
    // can say which file to fix.
    std::fs::create_dir_all(&fx.paths.home).unwrap();
    std::fs::write(
        fx.paths.user_config_file(),
        "[runtime]\nprelude = \"nvm use 22\"\n",
    )
    .unwrap();

    let err = resolve_runtime_slot(&fx, &config, &refuse, &shell, machine.home.path()).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("is not working"), "{message}");
    assert!(
        message.contains("nvm use 22"),
        "the prelude itself: {message}"
    );
    assert!(
        message.contains("24.21.0"),
        "and the version that still resolved: {message}"
    );
    assert!(
        message.contains(&fx.paths.user_config_file().display().to_string()),
        "and the file to change: {message}"
    );
}

// A prelude that fails outright is a different sentence from one that
// runs and resolves the wrong thing.
#[test]
fn a_prelude_that_fails_says_that_rather_than_blaming_the_runtime() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = |_: &str| Some("bash: nvm: command not found\n".to_string());
    let mut config = fx.config.clone();
    config.runtime.prelude = Some("nvm use 22".to_string());

    let err = resolve_runtime_slot(&fx, &config, &refuse, &shell, machine.home.path()).unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("the prelude itself failed: bash: nvm: command not found"),
        "{message}"
    );
}

// "This machine needs nothing" is an answer, recorded as an empty
// prelude so it is never asked twice — the shape `ports = []` has.
#[test]
fn nothing_needed_is_an_answer_and_is_only_asked_once() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    let (ask, _asked) = scripted(vec![Answer::None]);

    let config = resolve_runtime_slot(&fx, &fx.config, &ask, &shell, machine.home.path()).unwrap();
    assert_eq!(config.runtime.prelude.as_deref(), Some(""));
    assert!(
        user_config(&fx).contains("prelude = \"\""),
        "{}",
        user_config(&fx)
    );

    // And now nothing asks, and nothing probes.
    let probes = machine.probes();
    resolve_runtime_slot(&fx, &config, &refuse, &shell, machine.home.path()).unwrap();
    assert_eq!(
        machine.probes(),
        probes,
        "an answered question is not re-probed"
    );
}

// Unknown is not a mismatch: a spec this build cannot evaluate, or a
// shell that could not be run, never stops a start.
#[test]
fn a_requirement_pando_cannot_judge_blocks_nothing() {
    let fx = fixture_pinning(".nvmrc", "lts/hydrogen");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    resolve_runtime_slot(&fx, &fx.config, &refuse, &shell, machine.home.path()).unwrap();

    let dead = fixture_pinning(".nvmrc", "22");
    let no_shell = |_: &str| None;
    resolve_runtime_slot(&dead, &dead.config, &refuse, &no_shell, machine.home.path()).unwrap();
}

// The slot list decides whether the machine is asked about at all:
// `new` creates a worktree and spawns no process, so it has no
// business probing a runtime.
#[test]
fn the_slots_new_fills_never_probe_the_runtime() {
    let fx = fixture_pinning(".nvmrc", "22");
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "", "");
    let panicking = |_: &str| -> Option<String> { panic!("new must not probe a runtime") };
    let m = Machine {
        shell: &panicking,
        home: machine.home.path().to_path_buf(),
    };
    resolve_on(
        &fx.paths,
        &fx.config,
        &NEW_SLOTS,
        &[],
        &Answering::asking(&refuse),
        &noop,
        &m,
    )
    .unwrap();
    drop(shell);
}

// ---- which URL a native service's create step reads --------------------

// A service the app reaches through more than one variable used to hand
// its recipe whichever key sorted first — so a bare `APP_DB_PORT` beside
// `DATABASE_URL` meant the recipe created the default database, not the
// app's. The value that names the most wins, and ties go to the first key.
#[test]
fn the_url_a_native_service_is_created_from_is_the_one_that_names_the_most() {
    let url = "postgres://acme:secret@localhost:17001/acme_dev";
    let resolved = BTreeMap::from([
        ("APP_DB_PORT".to_string(), "17001".to_string()),
        ("DATABASE_URL".to_string(), url.to_string()),
        (
            "PG_ADMIN_URL".to_string(),
            "postgres://localhost:17001".to_string(),
        ),
    ]);
    assert_eq!(
        super::services::identity_url(&resolved).as_deref(),
        Some(url)
    );

    // Two that name as much as each other: the first key, every time.
    let tie = BTreeMap::from([
        (
            "B_URL".to_string(),
            "postgres://b@localhost:1/b".to_string(),
        ),
        (
            "A_URL".to_string(),
            "postgres://a@localhost:1/a".to_string(),
        ),
    ]);
    for _ in 0..3 {
        assert_eq!(
            super::services::identity_url(&tie).as_deref(),
            Some("postgres://a@localhost:1/a")
        );
    }
    assert_eq!(super::services::identity_url(&BTreeMap::new()), None);
}

// ---- two starts of one worktree at once --------------------------------

// The TUI's start key pressed twice, or the TUI and the CLI together: both
// starts decide "nothing is running" before either spawns, the lock is let
// go for the install, and each then spawned its own dev process — the
// second record overwrote the first, whose group went on running with
// nothing in pando able to find or stop it.
#[test]
fn two_concurrent_starts_of_one_worktree_spawn_one_process() {
    let mut fx = installable_fixture("sleep 1");
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");

    let (a, b) = std::thread::scope(|s| {
        let one = s.spawn(|| start(&fx.paths, &fx.config, &name, None, &noop));
        let two = s.spawn(|| start(&fx.paths, &fx.config, &name, None, &noop));
        (one.join().unwrap().unwrap(), two.join().unwrap().unwrap())
    });
    let _guards: Vec<Detached> = guard(&a).into_iter().chain(guard(&b)).collect();

    let spawned: Vec<u32> = a
        .started
        .iter()
        .chain(b.started.iter())
        .map(|p| p.record.pid)
        .collect();
    assert_eq!(
        spawned.len(),
        1,
        "one of them found the other's process and left it alone: {spawned:?}"
    );
    let recorded = fx.state().worktrees[&name].processes["dev"].pid;
    assert_eq!(recorded, spawned[0], "and the record is the one that runs");
    stop(&fx.paths, &name, None).unwrap();
}

// A record left by a worktree removed outside pando, now replaced by a
// new one of the same name, is dropped by the next start. Its processes
// were signalled first; its native server was not, and nothing could
// find it again once the record was gone.
#[test]
fn a_stale_records_native_server_is_signalled_before_the_record_goes() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");

    let log = fx.paths.log_file(&name, "db");
    let server = crate::testutil::spawn_guarded("sleep 30", &fx.root, &log);
    let mut store = fx.state();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.path = fx.root.join("somewhere-else");
    record.services.push(state::ServiceRecord {
        name: "db".to_string(),
        kind: state::ServiceKind::Native,
        port: Some(17_999),
        pid: Some(server.pid),
        pgid: Some(server.pgid),
        compose_project: None,
    });
    state::save(&fx.paths.state_file(), &store).unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        wait_until(Duration::from_secs(5), || !crate::process::group_alive(
            server.pgid
        )),
        "the stale record's server is left running with no record of it"
    );
    assert!(fx.state().worktrees[&name].services.is_empty());
}

// The same stale record, holding the things of pando's that outlive a
// process: the containers and volumes of its compose project, and a
// database it made in the main checkout's server. Both are named for the
// worktree's name, so they are this worktree's — and the start that
// dropped the record with them left the containers up on their old ports
// and gave `rm` nothing to take down or drop.
#[test]
fn a_stale_records_containers_and_namespaces_pass_to_the_record_that_replaces_it() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let calls = docker_that_records(&fx.paths);

    let namespace = state::NamespaceRecord {
        service: "mariadb".into(),
        recipe: "mariadb".into(),
        kind: state::NamespaceKind::Database,
        host: "127.0.0.1".into(),
        port: 3306,
        name: "app__feat_one".into(),
        main: "app".into(),
        mains: Vec::new(),
        keys: Vec::new(),
        used_at: Utc::now(),
    };
    let mut store = fx.state();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.path = fx.root.join("somewhere-else");
    record.mode = Some(state::ServiceMode::Isolated);
    record.services.push(state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Compose,
        port: Some(17_999),
        pid: None,
        pgid: None,
        compose_project: Some("pando-stale-feat_one".to_string()),
    });
    record.namespaces.push(namespace.clone());
    state::save(&fx.paths.state_file(), &store).unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    let record = fx.state().worktrees[&name].clone();
    assert_ne!(record.path, fx.root.join("somewhere-else"), "{record:?}");
    let postgres = record
        .services
        .iter()
        .find(|s| s.name == "postgres")
        .expect("the record `rm` takes the volumes down by");
    assert_eq!(
        postgres.compose_project.as_deref(),
        Some("pando-stale-feat_one")
    );
    assert_eq!(
        postgres.port, None,
        "but it claims no port it does not have"
    );
    assert_eq!(
        record.namespaces,
        vec![namespace],
        "and the record `rm` drops the database by"
    );
    assert_eq!(
        compose_stops(&calls, "pando-stale-feat_one"),
        1,
        "a shared start left the stale record's containers running"
    );
}

/// A docker that does nothing and writes down every command line it was
/// given, one per line, in the file this returns. Installed where
/// `services::docker_program` looks first, so nothing reaches this
/// machine's Docker.
fn docker_that_records(paths: &PandoPaths) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let calls = paths.home.join("docker-calls");
    let bin = paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("docker"),
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
    calls
}

/// The compose `stop`s a recording docker was asked for, of one project.
fn compose_stops(calls: &Path, project: &str) -> usize {
    std::fs::read_to_string(calls)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(project) && line.ends_with("stop"))
        .count()
}

// A stopped isolated worktree keeps its compose records, with no pump,
// until `rm` takes the volumes down by them. Every later `stop` of it
// said "stopped", and `stop` with no name listed every worktree that had
// ever run isolated among the ones it had just stopped.
#[test]
fn a_stopped_isolated_worktree_is_not_running_to_a_second_stop() {
    let fx = fixture();
    let calls = docker_that_records(&fx.paths);
    let name = worktree_named(&fx, "feat/one");
    let with_postgres = |pump: Option<&Detached>| {
        let mut store = fx.state();
        store.worktrees.get_mut(&name).unwrap().services = vec![state::ServiceRecord {
            name: "postgres".to_string(),
            kind: state::ServiceKind::Compose,
            port: None,
            pid: pump.map(|p| p.pid),
            pgid: pump.map(|p| p.pgid),
            compose_project: Some("pando-x-feat_one".to_string()),
        }];
        state::save(&fx.paths.state_file(), &store).unwrap();
    };

    with_postgres(None);
    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::NotRunning
    );
    assert!(stop_all(&fx.paths).unwrap().is_empty());
    assert_eq!(
        compose_stops(&calls, "pando-x-feat_one"),
        2,
        "Docker is still asked, since a container can outlive its pump"
    );

    // A pump that is up is a service that is up.
    let pump = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file(&name, "postgres"),
    );
    with_postgres(Some(&pump));
    assert_eq!(
        stop(&fx.paths, &name, None).unwrap(),
        StopOutcome::Stopped(Vec::new())
    );
    assert!(!crate::process::group_alive(pump.pgid));
}

// A start asks compose what is already running first, so a failure after
// it stops only what the start brought up. A `ps` that failed — a Docker
// too loaded to answer in time — read as "nothing is running", and the
// failure after it stopped the whole project, containers the worktree's
// live processes were using among them.
#[test]
fn a_compose_ps_that_fails_starts_nothing_and_stops_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let fx = compose_fixture(
        "services:\n  postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    let config: Config = toml::from_str(
        "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }\n",
    )
    .unwrap();
    fx.paths.ensure_home().unwrap();
    let calls = fx.paths.home.join("docker-calls");
    let bin = fx.paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("docker"),
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\n\
             case \" $* \" in *\" ps \"*) echo 'context deadline exceeded' >&2; exit 1;; esac\n",
            calls.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let ports = BTreeMap::from([("postgres".to_string(), 17010)]);

    let err =
        super::services::bring_up_services(&fx.paths, &config, "feat+one", &fx.root, &ports, &noop)
            .unwrap_err();

    assert!(
        format!("{err:#}").contains("context deadline exceeded"),
        "{err:#}"
    );
    let calls = std::fs::read_to_string(&calls).unwrap_or_default();
    assert!(
        !calls.lines().any(|line| line.contains(" up ")),
        "nothing was brought up: {calls}"
    );
    assert!(
        !calls.lines().any(|line| line.ends_with("stop")),
        "nothing was stopped: {calls}"
    );
}

// A service record whose kind config has changed is written fresh, so
// nothing would stop what the old one ran. A native server whose name is
// now a compose service's was left running with no record of it, on the
// port its container was about to be published on.
#[test]
fn a_native_server_whose_service_is_now_compose_is_stopped_and_forgotten() {
    let mut fx = fixture();
    let compose: Config = toml::from_str(
        "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }\n",
    )
    .unwrap();
    fx.config.services = compose.services;
    let server = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file("feat+one", "postgres"),
    );
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.services = vec![state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Native,
        port: Some(15432),
        pid: Some(server.pid),
        pgid: Some(server.pgid),
        compose_project: None,
    }];

    let containers =
        super::services::leave_changed_kinds(&fx.paths, &fx.config, "feat+one", &mut record)
            .unwrap();
    assert!(containers.is_empty(), "a native server has no container");
    assert!(!crate::process::group_alive(server.pgid));

    let ports = BTreeMap::from([("postgres".to_string(), 15432)]);
    let planned = super::services::planned_services(&fx.config, &ports, &record);
    assert_eq!(planned.len(), 1, "{planned:?}");
    assert_eq!(planned[0].kind, state::ServiceKind::Compose);
    assert_eq!(planned[0].pid, None, "the server's pid is not a log pump");
}

// The native record of a service that was compose was written over the
// compose one before its container was stopped, so a stop that failed
// left nothing in pando able to find that container again.
#[test]
fn a_compose_record_whose_service_is_now_native_stays_until_its_container_is_stopped() {
    let mut fx = fixture();
    let native: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
         env = { DATABASE_URL = \"postgres\" }\n",
    )
    .unwrap();
    fx.config.services = native.services;
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.services = vec![state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Compose,
        port: Some(15432),
        pid: None,
        pgid: None,
        compose_project: Some("pando-acme-feat-one".to_string()),
    }];
    let containers = vec![(
        "pando-acme-feat-one".to_string(),
        vec!["postgres".to_string()],
    )];

    let left = super::services::leave_changed_kinds(&fx.paths, &fx.config, "feat+one", &mut record)
        .unwrap();
    assert_eq!(left, containers);
    let ports = BTreeMap::from([("postgres".to_string(), 15433)]);
    record.services = super::services::planned_services(&fx.config, &ports, &record);
    assert_eq!(record.services.len(), 1, "{:?}", record.services);
    assert_eq!(record.services[0].kind, state::ServiceKind::Compose);
    assert_eq!(
        record.services[0].compose_project.as_deref(),
        Some("pando-acme-feat-one")
    );
    assert_eq!(record.services[0].port, Some(15433));
    let again =
        super::services::leave_changed_kinds(&fx.paths, &fx.config, "feat+one", &mut record)
            .unwrap();
    assert_eq!(again, containers, "a start that could not stop it retries");

    fx.paths.ensure_home().unwrap();
    let mut store = state::State::default();
    store.worktrees.insert("feat+one".to_string(), record);
    state::save(&fx.paths.state_file(), &store).unwrap();
    super::services::replace_stopped_containers(&fx.paths, "feat+one", &containers, &[]).unwrap();
    let services = &fx.state().worktrees["feat+one"].services;
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(services[0].kind, state::ServiceKind::Native);
    assert_eq!(services[0].compose_project, None);
    assert_eq!(services[0].port, Some(15433));
}

// A docker that cannot be run was taken to mean the container it would
// have stopped could not be running, and the compose record — the only
// thing that names the project — was written over with the native one.
// A docker only missing from this PATH still has its daemon, so that
// container could be up, with nothing left in pando to stop it or to
// take its volume.
#[test]
fn a_compose_record_docker_could_not_be_asked_about_is_kept_beside_the_native_one() {
    let mut fx = fixture();
    let native: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
         env = { DATABASE_URL = \"postgres\" }\n",
    )
    .unwrap();
    fx.config.services = native.services;
    let project = "pando-acme-feat-one".to_string();
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.mode = Some(crate::state::ServiceMode::Isolated);
    record.services = vec![state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Compose,
        port: Some(15432),
        pid: None,
        pgid: None,
        compose_project: Some(project.clone()),
    }];
    let containers = vec![(project.clone(), vec!["postgres".to_string()])];
    let save = |record: &WorktreeRecord| {
        fx.paths.ensure_home().unwrap();
        let mut store = state::State::default();
        store
            .worktrees
            .insert("feat+one".to_string(), record.clone());
        state::save(&fx.paths.state_file(), &store).unwrap();
    };

    save(&record);
    super::services::replace_stopped_containers(&fx.paths, "feat+one", &containers, &containers)
        .unwrap();
    let mut record = fx.state().worktrees["feat+one"].clone();
    assert_eq!(record.services.len(), 2, "{:?}", record.services);
    assert_eq!(record.services[0].kind, state::ServiceKind::Native);
    assert_eq!(record.services[0].port, Some(15432));
    assert_eq!(record.services[1].kind, state::ServiceKind::Compose);
    assert_eq!(record.services[1].port, None, "a leftover holds no port");
    assert_eq!(
        super::services::compose_projects(&record),
        vec![project.clone()],
        "stop and rm can still find the project"
    );
    let shown = service_statuses(&record);
    assert_eq!(shown.len(), 1, "one postgres, the native one: {shown:?}");
    assert_eq!(shown[0].port, Some(15432));

    // The next start asks about the container again, and keeps the
    // native server it may have running beside the leftover — which is
    // not a service moving to other data: the native one is up on its own.
    record.services[0].pid = Some(4242);
    record.services[0].pgid = Some(4242);
    assert_eq!(
        super::services::changed_kinds(&fx.config, &record),
        Vec::new(),
        "{:?}",
        record.services
    );
    let again =
        super::services::leave_changed_kinds(&fx.paths, &fx.config, "feat+one", &mut record)
            .unwrap();
    assert_eq!(again, containers);
    let ports = BTreeMap::from([("postgres".to_string(), 15432)]);
    record.services = super::services::planned_services(&fx.config, &ports, &record);
    assert_eq!(record.services.len(), 2, "{:?}", record.services);
    assert_eq!(record.services[0].kind, state::ServiceKind::Native);
    assert_eq!(record.services[0].pid, Some(4242), "the server is not lost");
    assert_eq!(record.services[1].kind, state::ServiceKind::Compose);
    assert_eq!(record.services[1].port, None);

    // And once docker can be asked, the leftover goes.
    save(&record);
    super::services::replace_stopped_containers(&fx.paths, "feat+one", &containers, &[]).unwrap();
    let services = &fx.state().worktrees["feat+one"].services;
    assert_eq!(services.len(), 1, "{services:?}");
    assert_eq!(services[0].kind, state::ServiceKind::Native);
    assert_eq!(services[0].pid, Some(4242));
}

// The compose leftover beside a native server was taken for a server of
// the kind config runs the service as, in either direction. Switched back
// to compose, the start stopped the native server and brought the
// leftover's old volume up as if nothing had moved: its hooks were not
// run again, and the app ran on data from before the switch.
#[test]
fn a_compose_leftover_does_not_hide_a_service_moving_back_to_compose() {
    let mut fx = fixture();
    let compose: Config = toml::from_str(
        "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }\n",
    )
    .unwrap();
    fx.config.services = compose.services;
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.mode = Some(crate::state::ServiceMode::Isolated);
    record.services = vec![
        state::ServiceRecord {
            name: "postgres".to_string(),
            kind: state::ServiceKind::Native,
            port: Some(15432),
            pid: Some(4242),
            pgid: Some(4242),
            compose_project: None,
        },
        state::ServiceRecord {
            name: "postgres".to_string(),
            kind: state::ServiceKind::Compose,
            port: None,
            pid: None,
            pgid: None,
            compose_project: Some("pando-acme-feat-one".to_string()),
        },
    ];
    assert_eq!(
        super::services::changed_kinds(&fx.config, &record),
        vec![("postgres".to_string(), state::ServiceKind::Compose)]
    );
}

// Two starts of one worktree at once — the TUI and an agent — both found
// no native server running and both spawned one on the same data
// directory, and the second pid was written over the first: a live server
// no record named, holding the port and the data directory from every
// start after it. A data directory neither had made yet, both initialised.
#[test]
fn two_starts_at_once_spawn_and_record_one_native_server() {
    use std::os::unix::fs::PermissionsExt;
    if !python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut fx = fixture();
    fx.paths.ensure_home().unwrap();
    let inits = fx.paths.home.join("slowdb-inits");
    let spawns = fx.paths.home.join("slowdb-spawns");
    let recipes = fx.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    // An init slow enough that both starts are inside it at once.
    std::fs::write(
        recipes.join("slowdb.toml"),
        format!(
            "kind = \"service\"\nname = \"slowdb\"\nbinaries = [\"pando-fake-slowdb\"]\n\n\
             [service]\ninit = \"echo ran >> '{}' && sleep 1 && mkdir -p {{datadir}}\"\n\
             cmd = \"exec pando-fake-slowdb {{port}}\"\n",
            inits.display()
        ),
    )
    .unwrap();
    let bin = fx.paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("pando-fake-slowdb"),
        format!(
            "#!/bin/sh\necho spawned >> '{}'\n\
             exec python3 -c \"import socket,sys,time;s=socket.socket();\
             s.bind(('127.0.0.1',int(sys.argv[1])));s.listen(5);time.sleep(60)\" \"$1\"\n",
            spawns.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("pando-fake-slowdb"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let native: Config =
        toml::from_str("[[services]]\nkind = \"native\"\nname = \"slowdb\"\n").unwrap();
    fx.config.services = native.services;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.services = vec![state::ServiceRecord {
        name: "slowdb".to_string(),
        kind: state::ServiceKind::Native,
        port: Some(port),
        pid: None,
        pgid: None,
        compose_project: None,
    }];
    let mut store = state::State::default();
    store.worktrees.insert("feat+one".to_string(), record);
    state::save(&fx.paths.state_file(), &store).unwrap();
    let ports = BTreeMap::from([("slowdb".to_string(), port)]);

    let bring_up = || {
        super::services::bring_up_services(
            &fx.paths, &fx.config, "feat+one", &fx.root, &ports, &noop,
        )
        .map(|_| ())
    };
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(bring_up);
        let b = scope.spawn(bring_up);
        (a.join().unwrap(), b.join().unwrap())
    });

    let record = fx.state().worktrees["feat+one"].services[0].clone();
    let _server = record.pgid.map(|pgid| crate::testutil::Detached {
        pid: record.pid.unwrap_or_default(),
        pgid,
    });
    let count = |path: &Path| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .count()
    };
    assert_eq!(count(&spawns), 1, "one server for one data directory");
    assert_eq!(count(&inits), 1, "one init for one data directory");
    a.unwrap();
    b.unwrap();
    assert!(
        record.pid.is_some_and(proc::is_alive),
        "the server that runs is the one recorded: {record:?}"
    );
}

// 0.2.0 could leave a compose record with a project and no port on a
// worktree that is not isolated — a failed isolated start. `status` and
// the TUI showed it as `postgres  no port` for ever. It is still what `rm`
// needs to take the volumes down, so it stays in state; it is not shown.
#[test]
fn a_shared_worktrees_leftover_compose_record_is_kept_but_not_shown() {
    let mut record = WorktreeRecord::new("/tmp/nowhere", true);
    record.services.push(state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Compose,
        port: None,
        pid: None,
        pgid: None,
        compose_project: Some("pando-acme-feat-one".to_string()),
    });
    assert!(service_statuses(&record).is_empty());
    assert_eq!(
        super::services::compose_projects(&record),
        vec!["pando-acme-feat-one".to_string()],
        "rm can still find the project"
    );

    // An isolated worktree's stopped service is a real row.
    record.mode = Some(crate::state::ServiceMode::Isolated);
    assert_eq!(service_statuses(&record).len(), 1);

    // And so is a service a switch to isolated is bringing up: it has its
    // port before the worktree is flagged.
    record.mode = Some(crate::state::ServiceMode::Shared);
    record.services[0].port = Some(17_010);
    assert_eq!(service_statuses(&record).len(), 1);
}

// `new` narrated "provisioning" and "installing" for a project with
// nothing to provision and nothing to install.
#[test]
fn new_says_nothing_about_steps_it_has_nothing_to_do_for() {
    let fx = fixture();
    let said = std::sync::Mutex::new(Vec::<String>::new());
    new(&fx.paths, &fx.config, "feat/one", None, &|m| {
        said.lock().unwrap().push(m.to_string())
    })
    .unwrap();
    let said = said.into_inner().unwrap();
    assert!(
        !said
            .iter()
            .any(|m| m == "provisioning" || m == "installing"),
        "{said:?}"
    );
}

// `rm` of a running worktree printed only "removed": that it stopped the
// dev server first went unsaid.
#[test]
fn rm_of_a_running_worktree_says_what_it_stops() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let report = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&report);
    let said = std::sync::Mutex::new(Vec::<String>::new());
    super::rm(&fx.paths, &name, false, false, &|m| {
        said.lock().unwrap().push(m.to_string())
    })
    .unwrap();
    let said = said.into_inner().unwrap();
    assert!(said.iter().any(|m| m == "stopping dev"), "{said:?}");
}

// `share` waits for a starting worktree. The phase machine keeps a
// process `Starting` past its window while the port scan cannot answer;
// a share that waited for the window alone gave up on a healthy server
// the phase machine was still, correctly, waiting on.
#[test]
fn share_waits_as_long_as_the_phase_can_stay_starting() {
    let mut record = WorktreeRecord::new("/tmp/nowhere", true);
    let mut dev = fake_record(1);
    dev.ready_timeout_s = Some(10);
    dev.phase = Phase::Starting { since: Utc::now() };
    record.processes.insert("dev".to_string(), dev);
    let mut store = state::State {
        version: state::STATE_VERSION,
        worktrees: BTreeMap::new(),
    };
    store.worktrees.insert("feat+one".to_string(), record);

    let budget = share_ready_budget(&store, "feat+one").unwrap();
    let longest = state::longest_starting_secs(10) as u64;
    assert!(
        budget.as_secs() + 1 >= longest && budget.as_secs() <= longest + 1,
        "{budget:?} against {longest}s"
    );
}

// An interrupted pass writes nothing about the dev process. A start with
// no terminal used to stop at the port question with `[dev].cmd` already
// on disk, and a `[dev]` on disk reads as a developer's own — so the port
// question was never asked again, and the next start "succeeded" with no
// port and no URL.
#[test]
fn a_pass_stopped_at_the_port_question_leaves_the_dev_command_unwritten() {
    let fx = detectable_fixture(
        r#"{ "dev": "node server.js" }"#,
        "WEB_PORT=3000\nADMIN_PORT=3001\n",
    );
    let stop_at_ports = |q: &Question| -> Result<Answer> {
        Err(NeedsAnswer {
            question: q.clone(),
        }
        .into())
    };
    let err = resolve_process(&fx.paths, &fx.config, &stop_at_ports, &noop).unwrap_err();
    assert_eq!(
        err.downcast_ref::<NeedsAnswer>().map(|n| n.question.slot),
        Some(Slot::PortEnv)
    );
    // And it says where its answer goes: this project's file, absolutely.
    assert_eq!(
        err.downcast_ref::<NeedsAnswer>()
            .and_then(|n| n.question.answer_file.clone()),
        Some(fx.paths.config_file())
    );
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap_or_default();
    assert!(
        !written.contains("[dev]"),
        "the dev command waits for its ports: {written}"
    );

    // The next pass asks the port question again, and writes both.
    let loaded = crate::config::load(&fx.paths).unwrap();
    let (ask, asked) = scripted(vec![Answer::Auto(0)]);
    let config = resolve_process(&fx.paths, &loaded.config, &ask, &noop).unwrap();
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::PortEnv]
    );
    assert_eq!(config.processes["dev"].roles(), vec!["admin", "web"]);
    let reloaded = crate::config::load(&fx.paths).unwrap().config;
    assert_eq!(reloaded.processes["dev"].cmd, "pnpm dev");
    assert_eq!(reloaded.processes["dev"].roles(), vec!["admin", "web"]);
}

// The schema step is the one question that touches data. A start that is
// not isolating neither asks it nor takes it — the hook would run against
// the developer's shared database — and an isolated one asks it even with
// a single candidate, with "no" recorded so it is never asked again.
#[test]
fn the_schema_step_is_asked_only_on_an_isolated_start_and_no_is_recorded() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::create_dir_all(fx.root.join("prisma")).unwrap();
    std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "prisma"]);

    let shared = resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    assert!(
        shared.hooks.is_empty(),
        "a shared start takes no schema step"
    );

    let loaded = crate::config::load(&fx.paths).unwrap().config;
    let (ask, asked) = scripted(vec![Answer::None]);
    let isolated = super::resolve_process(&fx.paths, &loaded, Mode::Isolated, &ask, &noop).unwrap();
    let asked = asked.borrow();
    assert_eq!(
        asked.iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::SchemaHook]
    );
    assert!(asked[0].allow_none, "\"no\" is an answer");
    assert_eq!(isolated.hooks.len(), 1);
    assert_eq!(isolated.hooks[0].on, Some(crate::config::HookScope::Never));
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("on = \"never\""), "{written}");

    // Answered, so asked never again.
    let reloaded = crate::config::load(&fx.paths).unwrap().config;
    super::resolve_process(&fx.paths, &reloaded, Mode::Isolated, &refuse, &noop).unwrap();
}

// A plain start of a worktree that is already isolated keeps its own
// services, so it is an isolating start too: the schema question is
// about the mode it is in, and silencing it there left the private
// database with no schema step ever proposed.
#[test]
fn a_plain_start_of_an_isolated_worktree_asks_the_schema_question() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::create_dir_all(fx.root.join("prisma")).unwrap();
    std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "prisma"]);
    resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    let mut text = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    text.push_str("\n[[services]]\nkind = \"native\"\nname = \"postgres\"\n");
    std::fs::write(fx.paths.config_file(), text).unwrap();
    let loaded = crate::config::load(&fx.paths).unwrap().config;
    let name = new(&fx.paths, &loaded, "feat/one", None, &noop).unwrap();

    // Not isolated yet: a plain start stays silent about it.
    super::resolve_for_start(&fx.paths, &loaded, &name, Mode::Remembered, &refuse, &noop).unwrap();

    let mut store = fx.state();
    store.worktrees.get_mut(&name).unwrap().mode = Some(crate::state::ServiceMode::Isolated);
    state::save(&fx.paths.state_file(), &store).unwrap();
    let (ask, asked) = scripted(vec![Answer::None]);
    super::resolve_for_start(&fx.paths, &loaded, &name, Mode::Remembered, &ask, &noop).unwrap();
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::SchemaHook]
    );
}

// A namespaced start runs the schema step only on a database of the
// worktree's own, so one that gets only a Redis slot is not asked what
// fills a fresh database — and one that gets its own database is.
#[test]
fn a_namespaced_start_asks_the_schema_question_only_when_it_gets_a_database_of_its_own() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::create_dir_all(fx.root.join("prisma")).unwrap();
    std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "prisma"]);
    std::fs::write(
        fx.root.join(".env"),
        "PORT=3000\nREDIS_URL=redis://localhost:6379/0\n",
    )
    .unwrap();
    resolve_process(&fx.paths, &fx.config, &refuse, &noop).unwrap();
    let mut text = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    text.push_str(
        "\n[[services]]\nkind = \"native\"\nname = \"redis\"\nenv = { REDIS_URL = \"redis\" }\n",
    );
    std::fs::write(fx.paths.config_file(), &text).unwrap();
    let loaded = crate::config::load(&fx.paths).unwrap().config;
    let only_a_slot =
        super::resolve_process(&fx.paths, &loaded, Mode::Namespaced, &refuse, &noop).unwrap();
    assert!(only_a_slot.hooks.is_empty(), "{:?}", only_a_slot.hooks);

    std::fs::write(
        fx.root.join(".env"),
        "PORT=3000\nREDIS_URL=redis://localhost:6379/0\n\
         DATABASE_URL=mysql://app:pw@localhost:3306/shop\n",
    )
    .unwrap();
    text.push_str(
        "\n[[services]]\nkind = \"native\"\nname = \"mariadb\"\n\
         env = { DATABASE_URL = \"mariadb\" }\n",
    );
    std::fs::write(fx.paths.config_file(), &text).unwrap();
    let loaded = crate::config::load(&fx.paths).unwrap().config;
    let (ask, asked) = scripted(vec![Answer::None]);
    super::resolve_process(&fx.paths, &loaded, Mode::Namespaced, &ask, &noop).unwrap();
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::SchemaHook]
    );
}

// The Docker-down offer said "delete the compose `[[services]]` entry"
// whatever the file held. Counted, and named by what each entry includes,
// so a developer with two knows there are two to find.
#[test]
fn the_compose_entries_to_delete_are_counted_and_named() {
    let entry = |file: &str, include: &[&str]| crate::config::ServiceConfig::Compose {
        file: file.to_string(),
        include: include.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
        ready_timeout_s: None,
    };
    let mut config = Config {
        services: vec![entry("docker-compose.yml", &["postgres", "redis"])],
        ..Default::default()
    };
    assert_eq!(
        super::services::compose_entries_named(&config),
        "the compose `[[services]]` entry (postgres, redis)"
    );
    config
        .services
        .push(entry("compose.search.yml", &["meilisearch"]));
    assert_eq!(
        super::services::compose_entries_named(&config),
        "the 2 compose `[[services]]` entries (postgres, redis; meilisearch)"
    );
}

// `--yes` takes the step pando found, scoped to isolated starts.
#[test]
fn yes_takes_the_schema_step_scoped_to_isolated_starts() {
    let fx = detectable_fixture(r#"{ "dev": "next dev" }"#, "PORT=3000\n");
    std::fs::create_dir_all(fx.root.join("prisma")).unwrap();
    std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "prisma"]);
    let (ask, _) = scripted(vec![Answer::Auto(0)]);
    let config =
        super::resolve_process(&fx.paths, &fx.config, Mode::Isolated, &ask, &noop).unwrap();
    assert_eq!(config.hooks[0].cmd, "pnpm prisma migrate deploy");
    assert_eq!(config.hooks[0].on, Some(crate::config::HookScope::Isolated));
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert!(written.contains("on = \"isolated\""), "{written}");
}

// A uv project runs every command through `uv run`, which reads
// `.python-version` and finds or installs that Python itself. What
// `bash -lc` resolves on its own is beside the point, and asking for a
// prelude line was a dead end on a machine with no pyenv — `--yes`
// included, because there was nothing to take.
#[test]
fn a_uv_project_needs_no_prelude_when_its_commands_go_through_uv_run() {
    let fx = fixture_pinning(".python-version", "3.12");
    std::fs::write(fx.root.join("uv.lock"), "version = 1\n").unwrap();
    let home = tempdir().unwrap();
    let shell = |command: &str| -> Option<String> {
        if command.contains("command -v uv") {
            return Some("pando-runner-ok\n".to_string());
        }
        Some(crate::runtime::probe_reply("/usr/bin/python3", "3.11.5"))
    };
    let mut config = fx.config.clone();
    config.processes.insert(
        "dev".to_string(),
        dev("uv run python manage.py runserver 127.0.0.1:{port:web}"),
    );
    let resolved = resolve_runtime_slot(&fx, &config, &refuse, &shell, home.path()).unwrap();
    assert_eq!(
        resolved.runtime.prelude, None,
        "nothing asked, nothing written"
    );

    // A process that does not go through the runner gets the interpreter
    // the shell resolves, so the mismatch is still a question.
    config
        .processes
        .insert("worker".to_string(), dev("python worker.py"));
    let err = resolve_runtime_slot(&fx, &config, &asks_nothing_answerable, &shell, home.path())
        .unwrap_err();
    assert!(err.downcast_ref::<NeedsAnswer>().is_some(), "{err:#}");

    // …and so is a machine with no uv at all.
    config.processes.remove("worker");
    let no_uv = |command: &str| -> Option<String> {
        if command.contains("command -v uv") {
            return Some(String::new());
        }
        Some(crate::runtime::probe_reply("/usr/bin/python3", "3.11.5"))
    };
    let err = resolve_runtime_slot(&fx, &config, &asks_nothing_answerable, &no_uv, home.path())
        .unwrap_err();
    assert!(err.downcast_ref::<NeedsAnswer>().is_some(), "{err:#}");
}

// A hook runs in the same shell a process does, and before it: a
// migration that runs `python` bare gets whatever interpreter the shell
// resolves. Only processes were checked, so the mismatch went unasked and
// the hook ran under the wrong Python. Its fallback counts as well.
#[test]
fn a_hook_that_does_not_go_through_uv_run_keeps_the_runtime_question() {
    let fx = fixture_pinning(".python-version", "3.12");
    std::fs::write(fx.root.join("uv.lock"), "version = 1\n").unwrap();
    let home = tempdir().unwrap();
    let shell = |command: &str| -> Option<String> {
        if command.contains("command -v uv") {
            return Some("pando-runner-ok\n".to_string());
        }
        Some(crate::runtime::probe_reply("/usr/bin/python3", "3.11.5"))
    };
    let mut config = fx.config.clone();
    config.processes.insert(
        "dev".to_string(),
        dev("uv run python manage.py runserver 127.0.0.1:{port:web}"),
    );
    let hook = |cmd: &str, fallback: Option<&str>| crate::config::HookConfig {
        name: "migrate".to_string(),
        after: crate::config::HookPoint::Services,
        fingerprint: Vec::new(),
        cmd: cmd.to_string(),
        cwd: None,
        fallback: fallback.map(str::to_string),
        on: None,
    };

    config.hooks = vec![hook("uv run python manage.py migrate", None)];
    let resolved = resolve_runtime_slot(&fx, &config, &refuse, &shell, home.path()).unwrap();
    assert_eq!(resolved.runtime.prelude, None, "through the runner");

    for (cmd, fallback) in [
        ("python manage.py migrate", None),
        (
            "uv run python manage.py migrate",
            Some("python manage.py migrate"),
        ),
    ] {
        config.hooks = vec![hook(cmd, fallback)];
        let err = resolve_runtime_slot(&fx, &config, &asks_nothing_answerable, &shell, home.path())
            .unwrap_err();
        assert!(
            err.downcast_ref::<NeedsAnswer>().is_some(),
            "{cmd} / {fallback:?}: {err:#}"
        );
    }

    // …but not one that is switched off. `on = "never"` is the schema
    // question's recorded "no", and a command that never runs needs no
    // interpreter at all — asking for a prelude line over it is a question
    // about nothing.
    let mut declined = hook("python manage.py migrate", None);
    declined.on = Some(crate::config::HookScope::Never);
    config.hooks = vec![declined];
    let resolved = resolve_runtime_slot(&fx, &config, &refuse, &shell, home.path()).unwrap();
    assert_eq!(resolved.runtime.prelude, None, "a hook that never runs");
}

fn asks_nothing_answerable(q: &Question) -> Result<Answer> {
    Err(NeedsAnswer {
        question: q.clone(),
    }
    .into())
}

// `new` runs the install step, and an install under the wrong runtime
// builds for the wrong one. With an install to run, the runtime question
// comes before it — not at the first `start`, after the damage.
#[test]
fn new_asks_the_runtime_question_before_an_install_runs() {
    let fx = fixture_pinning(".nvmrc", "22");
    std::fs::write(fx.root.join("package-lock.json"), "{}\n").unwrap();
    let machine = FakeMachine::with_nvm();
    let shell = machine.shell("24.21.0", "nvm use", "22.11.0");
    let m = Machine {
        shell: &shell,
        home: machine.home.path().to_path_buf(),
    };
    let (ask, asked) = scripted(vec![Answer::Choice(0)]);
    let config =
        super::questions::resolve_for_new_on(&fx.paths, &fx.config, &ask, &noop, &m).unwrap();
    assert_eq!(config.project.install.as_deref(), Some("npm ci"));
    assert_eq!(
        asked.borrow().iter().map(|q| q.slot).collect::<Vec<_>>(),
        vec![Slot::Prelude]
    );
    assert!(
        config
            .runtime
            .prelude
            .as_deref()
            .is_some_and(|p| p.contains("nvm use")),
        "{:?}",
        config.runtime.prelude
    );
}

// One `[[services]]` entry per native service, and each cites its own
// evidence. Redis found from `CACHE_URL` used to be written as "detected:
// DATABASE_URL…", because the first service's reason went on every entry.
#[test]
fn each_native_service_entry_cites_its_own_evidence() {
    let fx = fixture();
    let native = |name: &str, key: &str, why: &str| crate::detect::Candidate {
        value: name.to_string(),
        why: why.to_string(),
        service: Some(crate::detect::ServiceHint {
            source: crate::detect::ServiceSource::Native {
                recipe: name.to_string(),
            },
            env_key: Some(key.to_string()),
        }),
        preselected: true,
        ..Default::default()
    };
    let postgres = native("postgres", "DATABASE_URL", "from DATABASE_URL");
    let redis = native("redis", "CACHE_URL", "from CACHE_URL");
    let mut config = fx.config.clone();
    super::questions::apply_native_service_answer(
        &fx.paths,
        &mut config,
        &[&postgres, &redis],
        crate::config::Note::Detected(postgres.why.clone()),
    )
    .unwrap();
    let written = std::fs::read_to_string(fx.paths.config_file()).unwrap();
    assert_eq!(
        written.matches("# detected: from DATABASE_URL").count(),
        1,
        "{written}"
    );
    assert_eq!(
        written.matches("# detected: from CACHE_URL").count(),
        1,
        "{written}"
    );
}

// A preview is not a place to stop. A question nobody here can answer is
// part of what the dry run reports — that slot, unanswered — and the rest
// of the proposal is still rendered, with nothing written.
#[test]
fn a_dry_run_with_an_open_question_renders_it_unanswered() {
    let fx = detectable_fixture(
        r#"{ "dev": "node server.js" }"#,
        "WEB_PORT=3000\nADMIN_PORT=3001\n",
    );
    let (report, preview) = init_dry_run(
        &fx.paths,
        &fx.config,
        &Answering::asking(&asks_nothing_answerable),
        &noop,
    )
    .unwrap();
    let ports = report
        .slots
        .iter()
        .find(|s| s.slot == Slot::PortEnv)
        .unwrap();
    assert!(
        ports
            .value
            .as_deref()
            .is_some_and(|v| v.starts_with("(unanswered)")),
        "{ports:?}"
    );
    let dev = report
        .slots
        .iter()
        .find(|s| s.slot == Slot::DevCmd)
        .unwrap();
    assert_eq!(dev.value.as_deref(), Some("pnpm dev"));
    assert!(!preview.is_empty(), "the rest is still previewed");
    assert!(!fx.paths.config_file().exists(), "and nothing is written");
}

// The pass a preview runs again, after a question nobody here could
// answer, used to run over the copies the pass before had already
// written. A `[[services]]` entry is appended, not replaced, so the
// services the rules decided went in twice — and the preview failed on
// its own copy: two entries both running postgres.
#[test]
fn a_dry_run_run_again_after_an_open_question_writes_each_entry_once() {
    let fx = compose_fixture(
        "services:\n  postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n",
        "PORT=3000\nDATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    );
    // A schema step, so there is an open question after the services.
    std::fs::create_dir_all(fx.root.join("prisma")).unwrap();
    std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").unwrap();
    git(&fx.root, &["add", "."]);
    git(&fx.root, &["commit", "--quiet", "-m", "prisma"]);

    let (report, preview) = init_dry_run(
        &fx.paths,
        &fx.config,
        &Answering::asking(&asks_nothing_answerable),
        &noop,
    )
    .unwrap();
    let schema = report
        .slots
        .iter()
        .find(|s| s.slot == Slot::SchemaHook)
        .unwrap();
    assert!(
        schema
            .value
            .as_deref()
            .is_some_and(|v| v.starts_with("(unanswered)")),
        "the pass really was run again: {schema:?}"
    );
    let (_, body) = preview
        .iter()
        .find(|(path, _)| *path == fx.paths.config_file())
        .expect("the project's file is previewed");
    assert_eq!(body.matches("[[services]]").count(), 1, "{body}");
    assert!(body.contains("\"postgres\""), "{body}");
    assert!(!fx.paths.config_file().exists(), "and nothing is written");
}

// "provisioning" alone hid that `.env` in the worktree is a link to the
// main checkout's: an edit there edits main. Each file is named, with
// where it points and that it is a link.
#[test]
fn new_says_which_files_it_linked_and_where_they_point() {
    let mut fx = fixture();
    fx.config.project.provision = Some(vec![".env".into()]);
    let said = std::cell::RefCell::new(Vec::<String>::new());
    new(&fx.paths, &fx.config, "feat/link", None, &|m: &str| {
        said.borrow_mut().push(m.to_string())
    })
    .unwrap();
    let main_env = fx.paths.root().join(".env");
    assert!(
        said.borrow().iter().any(|m| m.starts_with("linked .env → ")
            && m.contains(&main_env.display().to_string())
            && m.contains("symlink")),
        "{:?}",
        said.borrow()
    );

    fx.config.project.provision_mode = ProvisionMode::Copy;
    said.borrow_mut().clear();
    new(&fx.paths, &fx.config, "feat/copy", None, &|m: &str| {
        said.borrow_mut().push(m.to_string())
    })
    .unwrap();
    assert!(
        said.borrow()
            .iter()
            .any(|m| m.starts_with("copied .env from ")),
        "{:?}",
        said.borrow()
    );
}

// Two starts switching the same worktree to isolated: one finishes the
// switch and spawns against the new services, the other fails late. The
// loser's undo must not tear those services down, or the winner's live
// application loses its database. A failing start never sets the flag —
// it is set right before the spawn — so an isolated record with a live
// process is always the other start's.
#[test]
fn a_failed_switchs_undo_leaves_a_switch_another_start_completed() {
    let fx = fixture();
    fx.paths.ensure_home().unwrap();
    let live = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file("feat+one", "dev"),
    );
    let mut store = state::State::new();
    let mut record = WorktreeRecord::new(fx.root.clone(), false);
    record.mode = Some(crate::state::ServiceMode::Isolated);
    record.processes.insert(
        "dev".to_string(),
        ProcessRecord {
            pid: live.pid,
            ..fake_record(live.pgid)
        },
    );
    record.services.push(state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Native,
        port: Some(17_001),
        pid: None,
        pgid: None,
        compose_project: None,
    });
    store
        .worktrees
        .insert("feat+one".to_string(), record.clone());
    state::save(&fx.paths.state_file(), &store).unwrap();

    super::services::undo_failed_isolation(&fx.paths, &fx.config, "feat+one", &BTreeMap::new());

    let after = &fx.state().worktrees["feat+one"];
    assert!(
        after.mode() == crate::state::ServiceMode::Isolated,
        "the other start's switch was undone"
    );
    assert_eq!(after.services, record.services);
}

// A switch to isolated interrupted between its services coming up and its
// second lock (Ctrl-C, a crash) never runs its undo: the record is not
// isolated, and its native server is live. The next plain start is a
// shared one, so it is the one that takes the orphan down, rather than
// blanking its port and leaving a server nothing can find.
#[test]
fn a_plain_start_takes_down_the_services_an_interrupted_switch_left_running() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let name = worktree_named(&fx, "feat/one");
    let orphan = crate::testutil::spawn_guarded(
        "exec sleep 300",
        &std::env::temp_dir(),
        &fx.paths.log_file(&name, "postgres"),
    );
    let mut store = fx.state();
    let record = store.worktrees.get_mut(&name).expect("new wrote a record");
    assert!(record.mode() != crate::state::ServiceMode::Isolated);
    record.services.push(state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Native,
        port: Some(17_001),
        pid: Some(orphan.pid),
        pgid: Some(orphan.pgid),
        compose_project: None,
    });
    state::save(&fx.paths.state_file(), &store).unwrap();

    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert!(
        wait_until(Duration::from_secs(5), || !proc::is_alive(orphan.pid)),
        "the orphaned server is still running"
    );
    assert!(fx.state().worktrees[&name].services.is_empty());
}

// The same switch interrupted earlier — during `up`, or while its
// containers were getting ready — has no pump recorded yet, only a compose
// record with its project and its port. The next shared start took the
// port away and left the containers running, holding theirs, where no
// status could show them.
#[test]
fn a_plain_start_takes_down_the_containers_a_switch_interrupted_before_its_pumps_left() {
    let mut fx = fixture();
    with_dev(&mut fx, dev("sleep 30"));
    let calls = docker_that_records(&fx.paths);
    let name = worktree_named(&fx, "feat/one");
    let mut store = fx.state();
    let record = store.worktrees.get_mut(&name).expect("new wrote a record");
    assert!(record.mode() != crate::state::ServiceMode::Isolated);
    record.services.push(state::ServiceRecord {
        name: "postgres".to_string(),
        kind: state::ServiceKind::Compose,
        port: Some(17_001),
        pid: None,
        pgid: None,
        compose_project: Some("pando-acme-feat_one".to_string()),
    });
    state::save(&fx.paths.state_file(), &store).unwrap();

    let (said, progress) = collecting();
    let outcome = super::start(
        &fx.paths,
        &fx.config,
        &name,
        None,
        Mode::Remembered,
        &progress,
    )
    .unwrap();
    let _guard = guard(&outcome);
    assert_eq!(compose_stops(&calls, "pando-acme-feat_one"), 1);
    assert!(
        said.borrow()
            .iter()
            .any(|l| l.contains("the private ones an interrupted start left running")),
        "{:?}",
        said.borrow()
    );
    let record = fx.state().worktrees[&name].clone();
    assert_eq!(
        record.services[0].compose_project.as_deref(),
        Some("pando-acme-feat_one"),
        "the record `rm` takes the volumes down by stays"
    );
    assert_eq!(record.services[0].port, None);

    // Once taken down, a shared start does not take it down again.
    stop(&fx.paths, &name, None).unwrap();
    let stopped = compose_stops(&calls, "pando-acme-feat_one");
    let outcome = start(&fx.paths, &fx.config, &name, None, &noop).unwrap();
    let _guard = guard(&outcome);
    assert_eq!(compose_stops(&calls, "pando-acme-feat_one"), stopped);
}

// `new` of a branch it cannot find locally asks origin for it. That fetch
// had no deadline: a remote that accepts the connection and never answers
// held `new` — and the TUI worker running it — for ever.
#[test]
fn a_fetch_from_an_origin_that_never_answers_is_bounded_and_says_so() {
    let fx = fixture();
    // Accepts, in the kernel's backlog, and never says a word: what a
    // wedged server looks like to the client.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    git(
        &fx.root,
        &[
            "remote",
            "add",
            "origin",
            &format!("git://127.0.0.1:{port}/acme.git"),
        ],
    );
    let began = std::time::Instant::now();
    let err = super::worktree::fetch_branch(&fx.root, "feat/x", Duration::from_secs(2))
        .expect_err("a fetch that never answered is not an answer");
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "{:?}",
        began.elapsed()
    );
    let err = format!("{err:#}");
    assert!(err.contains("did not answer"), "{err}");
    drop(silent);
}

// ---- the namespace login ------------------------------------------------------

/// Every line a call printed, for asserting what it did and did not say.
fn collecting() -> (std::rc::Rc<std::cell::RefCell<Vec<String>>>, impl Fn(&str)) {
    let lines = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = lines.clone();
    (lines, move |line: &str| {
        sink.borrow_mut().push(line.to_string())
    })
}

// The main checkout's own login is the one a namespaced start uses, and
// nobody is asked anything for it.
#[test]
fn a_namespace_login_the_main_checkout_has_is_used_without_asking() {
    let fx = fixture();
    std::fs::write(
        fx.root.join(".env"),
        "DATABASE_PORT=3306\nDATABASE_USER=app\nDATABASE_PASSWORD=hunter2\n",
    )
    .unwrap();
    let ask = |_: &Question| -> Result<Answer> { panic!("asked for a login the env files have") };
    let login = namespace_login(
        &fx.paths,
        &fx.config,
        "mariadb",
        &["DATABASE_PORT".to_string()],
        true,
        &ask,
        &noop,
    )
    .unwrap();
    assert_eq!(login.user.as_deref(), Some("app"));
    assert!(!fx.paths.config_file().exists(), "nothing was written");
}

// Decision 8: nothing says, so it asks — the same question every front
// end puts — and keeps the answer in pando's own file, 0600, where the
// next start finds it without asking again.
#[test]
fn a_namespace_login_nothing_says_is_asked_once_and_kept_where_only_pando_reads_it() {
    let fx = fixture();
    std::fs::write(fx.root.join(".env"), "DATABASE_PORT=3306\n").unwrap();
    let asked = std::cell::RefCell::new(Vec::<Question>::new());
    let ask = |q: &Question| -> Result<Answer> {
        asked.borrow_mut().push(q.clone());
        Ok(Answer::Custom("root:pa:ss word".to_string()))
    };
    let (said, progress) = collecting();
    let keys = ["DATABASE_PORT".to_string()];
    let login = namespace_login(
        &fx.paths, &fx.config, "mariadb", &keys, true, &ask, &progress,
    )
    .unwrap();
    assert_eq!(login.user.as_deref(), Some("root"));
    assert_eq!(
        login.env(Some("MYSQL_PWD")),
        vec![("MYSQL_PWD".to_string(), "pa:ss word".to_string())],
        "everything after the first colon is the password"
    );

    let question = asked.borrow()[0].clone();
    assert_eq!(question.slot, crate::detect::Slot::Login);
    assert!(question.slot.is_secret());
    assert!(question.options.is_empty() && question.allow_custom);
    assert!(question.prompt.contains("mariadb"), "{}", question.prompt);
    assert_eq!(question.answer_file.as_ref(), Some(&fx.paths.config_file()));
    assert!(
        question.snippet.contains("[namespaced.mariadb]"),
        "{}",
        question.snippet
    );
    assert!(
        recommended(&question).is_none(),
        "nothing to take on anyone's behalf"
    );

    let file = fx.paths.config_file();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("[namespaced.mariadb]"), "{text}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the file holding a password is its owner's alone"
    );
    for line in said.borrow().iter() {
        assert!(!line.contains("pa:ss word"), "printed the password: {line}");
    }

    // The next start reads it back and asks nothing.
    let config = crate::config::load(&fx.paths).unwrap().config;
    let never = |_: &Question| -> Result<Answer> { panic!("asked twice") };
    let again = namespace_login(&fx.paths, &config, "mariadb", &keys, true, &never, &noop).unwrap();
    assert_eq!(again.user.as_deref(), Some("root"));
    assert_eq!(again.env(Some("P")), login.env(Some("P")));
}

#[test]
fn a_namespace_login_with_no_user_is_refused_and_nothing_is_written() {
    let fx = fixture();
    let keys = ["DATABASE_PORT".to_string()];
    for answer in [
        Answer::Custom(":hunter2".to_string()),
        Answer::Custom("  ".to_string()),
        Answer::Choice(0),
        Answer::None,
    ] {
        let reply = answer.clone();
        let ask = move |_: &Question| -> Result<Answer> { Ok(reply.clone()) };
        let e = namespace_login(&fx.paths, &fx.config, "mariadb", &keys, true, &ask, &noop)
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(e.contains("nothing was written"), "{answer:?}: {e}");
        assert!(!e.contains("hunter2"), "{e}");
        assert!(!fx.paths.config_file().exists(), "{answer:?}");
    }
    // A user with no password is a login too.
    let ask = |_: &Question| -> Result<Answer> { Ok(Answer::Custom("root".to_string())) };
    let login =
        namespace_login(&fx.paths, &fx.config, "mariadb", &keys, true, &ask, &noop).unwrap();
    assert_eq!(login.user.as_deref(), Some("root"));
    assert!(!login.has_password());
}

// ---- namespaced starts ---------------------------------------------------------

/// A fake `mariadb` in pando's own `bin`, which every namespace command
/// finds first on PATH. Its databases are files under `dbs/`; `deny`
/// refuses every CREATE the way a login without the grant is refused; and
/// `argv` and `env` record what it was run with. A script at
/// `on-create-<name>` runs once, the first time `<name>` is to be made:
/// what another command does meanwhile.
fn fake_mariadb(paths: &PandoPaths) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let state = paths.home.join("fake-mariadb");
    std::fs::create_dir_all(state.join("dbs")).unwrap();
    let bin = paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
state='{state}'
printf '%s\n' "$*" >> "$state/argv"
printf '%s\n' "${{MYSQL_PWD-unset}}" >> "$state/env"
named() {{ printf '%s' "$*" | sed -n "s/.*$1[^a-zA-Z0-9_-]*\([a-zA-Z0-9_-]*\).*/\1/p"; }}
case "$*" in
  *"SELECT 1"*) echo 1 ;;
  *"CURRENT_USER()"*) echo "app@localhost" ;;
  *"CREATE DATABASE"*)
    db=$(printf '%s' "$*" | sed -n 's/.*CREATE DATABASE `\([^`]*\)`.*/\1/p')
    if [ -f "$state/on-create-$db" ]; then sh "$state/on-create-$db"; rm -f "$state/on-create-$db"; fi
    if [ -f "$state/deny" ]; then
      echo "ERROR 1044 (42000) at line 1: Access denied for user 'app'@'localhost' to database '$db'" >&2; exit 1
    fi
    if [ -f "$state/dbs/$db" ]; then
      echo "ERROR 1007 (HY000) at line 1: Can't create database '$db'; database exists" >&2; exit 1
    fi
    touch "$state/dbs/$db"; echo "$db" >> "$state/created" ;;
  *"LIKE"*)
    prefix=$(printf '%s' "$*" | sed -n "s/.*LIKE '\([^']*\)'.*/\1/p" | tr -d '\\' | sed 's/%$//')
    for f in "$state"/dbs/*; do
      n=$(basename "$f")
      case "$n" in "$prefix"*) echo "$n" ;; esac
    done ;;
  *"SCHEMATA"*)
    db=$(printf '%s' "$*" | sed -n "s/.*SCHEMA_NAME = '\([^']*\)'.*/\1/p")
    if [ -f "$state/dbs/$db" ]; then echo "$db"; fi ;;
  *"DROP DATABASE"*)
    db=$(printf '%s' "$*" | sed -n 's/.*DROP DATABASE IF EXISTS `\([^`]*\)`.*/\1/p')
    if [ -f "$state/deny-drop" ]; then
      echo "ERROR 1044 (42000) at line 1: Access denied for user 'app'@'localhost' to database '$db'" >&2; exit 1
    fi
    rm -f "$state/dbs/$db"; echo "$db" >> "$state/dropped" ;;
  *) echo "unexpected: $*" >&2; exit 9 ;;
esac
"#,
        state = state.display()
    );
    std::fs::write(bin.join("mariadb"), script).unwrap();
    std::fs::set_permissions(bin.join("mariadb"), std::fs::Permissions::from_mode(0o755)).unwrap();
    state
}

/// A project whose main checkout runs a MariaDB it addresses in parts, a
/// dev process that writes down its environment, and a schema step after
/// the services that writes down which database it was pointed at.
struct Namespaced {
    fx: Fx,
    name: String,
    fake: PathBuf,
    seen: PathBuf,
    schema: PathBuf,
}

fn namespaced_fixture(env: &str) -> Namespaced {
    let mut fx = fixture();
    std::fs::write(fx.root.join(".env"), env).unwrap();
    let seen = fx.root.parent().unwrap().join("seen-env");
    let schema = fx.root.parent().unwrap().join("schema-ran");
    let config: Config = toml::from_str(&format!(
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = {{ DATABASE_PORT = \"mariadb\" }}\n\n\
         [[hooks]]\nname = \"schema\"\nafter = \"services\"\n\
         cmd = \"echo \\\"$DATABASE_NAME\\\" >> '{}'\"\n",
        schema.display()
    ))
    .unwrap();
    fx.config.services = config.services;
    fx.config.hooks = config.hooks;
    with_dev(
        &mut fx,
        ProcessConfig {
            cmd: format!("env > {}; sleep 30", seen.display()),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let name = worktree_named(&fx, "feat/one");
    let fake = fake_mariadb(&fx.paths);
    Namespaced {
        fx,
        name,
        fake,
        seen,
        schema,
    }
}

const MAIN_ENV: &str = "DATABASE_HOST=localhost\nDATABASE_PORT=3306\nDATABASE_NAME=shop\n\
                        DATABASE_USER=app\nDATABASE_PASSWORD=s3cret-pw\n";

impl Namespaced {
    fn start(&self, mode: Mode) -> Result<(StartReport, Vec<String>)> {
        // What `env_line` reads is this start's environment, never the one
        // an earlier start in the same test wrote before it was stopped.
        let _ = std::fs::remove_file(&self.seen);
        let (said, progress) = collecting();
        let report = super::start(
            &self.fx.paths,
            &self.fx.config,
            &self.name,
            None,
            mode,
            &progress,
        )?;
        let said = said.borrow().clone();
        Ok((report, said))
    }

    fn env_line(&self, key: &str) -> Option<String> {
        let prefix = format!("{key}=");
        assert!(
            wait_until(Duration::from_secs(5), || std::fs::read_to_string(
                &self.seen
            )
            .is_ok_and(|env| env.contains("PANDO_NAME="))),
            "the process never wrote its environment"
        );
        let env = std::fs::read_to_string(&self.seen).unwrap();
        env.lines()
            .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
    }

    fn fake(&self, file: &str) -> String {
        std::fs::read_to_string(self.fake.join(file)).unwrap_or_default()
    }

    fn record(&self) -> WorktreeRecord {
        self.fx.state().worktrees[&self.name].clone()
    }
}

// The whole of a namespaced start: the worktree's own database made in the
// main checkout's server, recorded as pando's, handed to the app and to the
// schema step in place of main's — and the server's address left as it is.
#[test]
fn a_namespaced_start_makes_the_worktrees_own_database_and_points_the_app_at_it() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);

    assert_eq!(ns.fake("created"), "shop__feat_one\n");
    let record = ns.record();
    assert_eq!(record.mode, Some(crate::state::ServiceMode::Namespaced));
    assert_eq!(record.namespaces.len(), 1);
    let namespace = &record.namespaces[0];
    assert_eq!(namespace.name, "shop__feat_one");
    assert_eq!(namespace.main, "shop");
    assert_eq!(
        (namespace.host.as_str(), namespace.port),
        ("localhost", 3306)
    );
    assert_eq!(namespace.kind, crate::state::NamespaceKind::Database);

    assert_eq!(
        ns.env_line("DATABASE_NAME").as_deref(),
        Some("shop__feat_one")
    );
    assert_eq!(ns.env_line("DATABASE_PORT").as_deref(), Some("3306"));
    assert_eq!(
        std::fs::read_to_string(&ns.schema).unwrap(),
        "shop__feat_one\n",
        "the schema step ran, against the worktree's own database"
    );
    assert!(
        said.iter()
            .any(|l| l == "mariadb: own database shop__feat_one, made just now"),
        "{said:?}"
    );
    // The login went to the client in its environment, and nowhere a line
    // of output could carry it.
    assert!(ns.fake("env").lines().all(|l| l == "s3cret-pw"));
    assert!(!ns.fake("argv").contains("s3cret-pw"));
    assert!(said.iter().all(|l| !l.contains("s3cret-pw")), "{said:?}");
}

// A record left at another path by a worktree of the same name is dropped
// under the lock, so the start runs shared — but it was read as this
// worktree's mode first, and a plain start prepared namespaces in the
// developer's own server for a mode it would not run in, and failed
// outright when that server did not answer.
#[test]
fn a_stale_namespaced_record_is_not_this_worktrees_mode_and_nothing_is_asked_of_the_server() {
    let ns = namespaced_fixture(MAIN_ENV);
    let mut store = ns.fx.state();
    let record = store.worktrees.get_mut(&ns.name).unwrap();
    record.path = ns.fx.root.join("somewhere-else");
    record.mode = Some(crate::state::ServiceMode::Namespaced);
    state::save(&ns.fx.paths.state_file(), &store).unwrap();

    let (report, _) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.record().mode(), crate::state::ServiceMode::Shared);
    assert_eq!(ns.fake("argv"), "", "the server was asked something");
    assert_eq!(ns.fake("created"), "");
}

// A restart finds the database it made and makes nothing; one somebody
// dropped by hand is made again, empty — and then the schema step runs
// again whatever its fingerprint says, because an empty database is not
// "nothing changed".
#[test]
fn a_namespace_is_found_again_and_one_dropped_by_hand_is_made_again_and_filled() {
    let mut ns = namespaced_fixture(MAIN_ENV);
    ns.fx.config.hooks[0].fingerprint = vec!["README.md".to_string()];
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    drop(guard(&report));
    assert_eq!(
        ns.fake("created"),
        "shop__feat_one\n",
        "nothing new was made"
    );
    assert!(
        said.iter()
            .any(|l| l == "mariadb: own database shop__feat_one"),
        "{said:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&ns.schema).unwrap().lines().count(),
        1,
        "its fingerprint had not changed, and neither had its database"
    );
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    std::fs::remove_file(ns.fake.join("dbs/shop__feat_one")).unwrap();
    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.fake("created"), "shop__feat_one\nshop__feat_one\n");
    assert!(
        said.iter()
            .any(|l| l.contains("is gone from localhost:3306")),
        "{said:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&ns.schema).unwrap().lines().count(),
        2,
        "an empty database is filled again"
    );
}

// Decision 4: the login may not make it, so the start stops with nothing
// made, nothing recorded, nothing spawned, and the grant that fixes it.
#[test]
fn a_login_that_may_not_make_the_database_stops_the_start_with_the_grant() {
    let ns = namespaced_fixture(MAIN_ENV);
    std::fs::write(ns.fake.join("deny"), "").unwrap();
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(
        e.contains("GRANT ALL ON `shop\\_\\_%`.* TO 'app'@'localhost';"),
        "{e}"
    );
    assert!(e.contains("Nothing was made"), "{e}");
    assert!(!e.contains("s3cret-pw"), "{e}");
    let record = ns.fx.state().worktrees.get(&ns.name).cloned();
    assert!(
        record
            .as_ref()
            .is_none_or(|r| r.namespaces.is_empty() && r.processes.is_empty())
    );
    assert!(record.is_none_or(|r| r.mode != Some(crate::state::ServiceMode::Namespaced)));
    assert!(!ns.schema.exists(), "no hook ran");
}

// A database of that name already on the server was not made by pando, so
// it is left alone and the worktree gets its second name instead.
#[test]
fn a_database_already_there_is_left_alone_and_the_worktree_gets_its_other_name() {
    let ns = namespaced_fixture(MAIN_ENV);
    std::fs::write(ns.fake.join("dbs/shop__feat_one"), "").unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    let made = ns.fake("created");
    assert!(
        made.starts_with("shop__feat_one_") && made.lines().count() == 1,
        "{made}"
    );
    assert_eq!(ns.record().namespaces[0].name, made.trim());
    assert_eq!(ns.env_line("DATABASE_NAME").as_deref(), Some(made.trim()));
    assert!(
        said.iter().any(|l| l.contains("pando did not make it")),
        "{said:?}"
    );
}

/// Writes the state another start of the fixture's worktree leaves once it
/// has recorded `database` as the worktree's, and returns the command that
/// puts it in place: what that start does meanwhile.
fn recorded_by_another_start(ns: &Namespaced, database: &str) -> String {
    let mut raced = ns.fx.state();
    raced
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .push(crate::state::NamespaceRecord {
            service: "mariadb".into(),
            recipe: "mariadb".into(),
            kind: crate::state::NamespaceKind::Database,
            host: "localhost".into(),
            port: 3306,
            name: database.into(),
            main: "shop".into(),
            mains: vec!["shop".into()],
            keys: vec!["DATABASE_PORT".into()],
            used_at: Utc::now(),
        });
    let racing = ns.fx.paths.home.join("raced.json");
    state::save(&racing, &raced).unwrap();
    format!(
        "cp '{}' '{}'\n",
        racing.display(),
        ns.fx.paths.state_file().display()
    )
}

// Two starts of one worktree at once, the TUI's and the CLI's. The other
// made the database and recorded it after this one read state, so this
// one's CREATE found it there: it was said not to be pando's, and a second
// database was made and recorded beside it, for the next start to move the
// app back from. It is the worktree's own, and its only one.
#[test]
fn a_database_another_start_of_this_worktree_just_made_is_its_own_and_its_only_one() {
    let ns = namespaced_fixture(MAIN_ENV);
    let hook = ns.fake.join("on-create-shop__feat_one");
    std::fs::write(
        &hook,
        format!(
            "touch '{}'\n{}",
            ns.fake.join("dbs/shop__feat_one").display(),
            recorded_by_another_start(&ns, "shop__feat_one")
        ),
    )
    .unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert!(!hook.exists(), "the race never ran");
    assert_eq!(ns.fake("created"), "", "this start made a database");
    let names: Vec<String> = ns.record().namespaces.into_iter().map(|n| n.name).collect();
    assert_eq!(names, vec!["shop__feat_one"]);
    assert_eq!(
        ns.env_line("DATABASE_NAME").as_deref(),
        Some("shop__feat_one")
    );
    assert!(
        said.iter().all(|l| !l.contains("pando did not make it")),
        "{said:?}"
    );
}

// The same race, when this start had moved on to the other name before the
// other start recorded the first: the one recorded first is the worktree's,
// and the one this start made is left, empty, for doctor to list — never
// recorded beside it.
#[test]
fn a_database_made_while_another_start_of_this_worktree_recorded_one_is_left_unrecorded() {
    let ns = namespaced_fixture(MAIN_ENV);
    let [first, second] =
        crate::namespace::database_names("shop", ns.fx.paths.project_id(), &ns.name).unwrap();
    // The other start has made the first name, and not yet recorded it.
    std::fs::write(
        ns.fake.join(format!("on-create-{first}")),
        format!("touch '{}'\n", ns.fake.join("dbs").join(&first).display()),
    )
    .unwrap();
    // It records it while this one makes the second.
    std::fs::write(
        ns.fake.join(format!("on-create-{second}")),
        recorded_by_another_start(&ns, &first),
    )
    .unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.fake("created"), format!("{second}\n"));
    let names: Vec<String> = ns.record().namespaces.into_iter().map(|n| n.name).collect();
    assert_eq!(names, vec![first.clone()]);
    assert_eq!(ns.env_line("DATABASE_NAME"), Some(first.clone()));
    assert!(
        said.iter()
            .any(|l| l.contains(&format!("{second} is left on localhost:3306, empty"))),
        "{said:?}"
    );
    assert_eq!(ns.fake("dropped"), "", "nothing was dropped");
    let leftovers = namespace_leftovers(&ns.fx.paths, &ns.fx.config, &ns.fx.state());
    assert_eq!(
        leftovers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec![second.as_str()]
    );
}

// Decision 10: switching back to shared keeps the namespace until `rm`,
// and the app is back on the main checkout's own data — so switching to
// namespaced again finds what it had.
#[test]
fn a_worktree_switched_back_to_shared_keeps_its_namespace_for_the_way_back() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    assert_eq!(
        ns.env_line("DATABASE_NAME").as_deref(),
        Some("shop__feat_one")
    );
    drop(running);
    std::fs::remove_file(&ns.seen).unwrap();

    let (report, _) = ns.start(Mode::Shared).unwrap();
    let running = guard(&report);
    let record = ns.record();
    assert_eq!(record.mode, Some(crate::state::ServiceMode::Shared));
    assert_eq!(record.namespaces.len(), 1, "kept until rm");
    assert_eq!(
        ns.env_line("DATABASE_NAME"),
        None,
        "no longer the worktree's own"
    );
    assert_eq!(ns.fake("dropped"), "", "nothing was dropped");
    drop(running);
    std::fs::remove_file(&ns.seen).unwrap();

    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(
        ns.fake("created"),
        "shop__feat_one\n",
        "found again, not made again"
    );
    assert_eq!(
        ns.env_line("DATABASE_NAME").as_deref(),
        Some("shop__feat_one")
    );
}

// A start that asks — a terminal, the TUI — asks for the login nothing
// gives, and a start that cannot ask says where to write one.
#[test]
fn a_namespaced_start_with_no_login_asks_for_one_or_says_where_it_goes() {
    let ns = namespaced_fixture("DATABASE_PORT=3306\nDATABASE_NAME=shop\n");
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(e.contains("[namespaced.mariadb]"), "{e}");
    assert!(
        e.contains(&ns.fx.paths.config_file().display().to_string()),
        "{e}"
    );
    assert_eq!(ns.fake("created"), "", "nothing was made");

    let ask = |q: &Question| -> Result<Answer> {
        assert_eq!(q.slot, crate::detect::Slot::Login);
        Ok(Answer::Custom("root:typed-pw".to_string()))
    };
    let config = resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Namespaced,
        &ask,
        &noop,
    )
    .unwrap();
    let report = super::start(
        &ns.fx.paths,
        &config,
        &ns.name,
        None,
        Mode::Namespaced,
        &noop,
    )
    .unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.fake("created"), "shop__feat_one\n");
    assert!(ns.fake("env").lines().all(|l| l == "typed-pw"));
}

// A project none of whose services pando can give a namespace in starts
// on the main checkout's data, and says so for each.
#[test]
fn a_project_with_nothing_to_namespace_starts_shared_and_says_why() {
    let ns = namespaced_fixture("DATABASE_PORT=3306\n");
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter()
            .any(|l| l.contains("no service here can have a namespace of its own")),
        "{said:?}"
    );
    assert!(
        said.iter()
            .any(|l| l.contains("mariadb: shared") && l.contains("names its database")),
        "{said:?}"
    );
    assert_eq!(ns.record().mode, Some(crate::state::ServiceMode::Shared));
    assert!(!ns.schema.exists(), "a shared start runs no schema step");
}

// A start keeps a namespaced worktree on its namespaces. Once its plan lost
// every target — here the main checkout's env stopped naming its database
// — a plain start went on shared, on main's data, with nothing said, and
// wrote shared into the record so every later start stayed there; the
// TUI's quick start, a namespaced one, did the same with a progress line.
#[test]
fn a_start_of_a_namespaced_worktree_that_can_no_longer_have_its_namespaces_is_refused() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    std::fs::write(ns.fx.root.join(".env"), "DATABASE_PORT=3306\n").unwrap();

    let mut errors = Vec::new();
    for mode in [Mode::Remembered, Mode::Namespaced] {
        errors.push(
            resolve_for_start(&ns.fx.paths, &ns.fx.config, &ns.name, mode, &refuse, &noop)
                .map(|_| ())
                .unwrap_err(),
        );
        errors.push(ns.start(mode).map(|_| ()).unwrap_err());
        errors.push(
            super::restart(&ns.fx.paths, &ns.fx.config, &ns.name, None, mode, &noop)
                .map(|_| ())
                .unwrap_err(),
        );
    }
    for err in errors {
        let msg = format!("{err:#}");
        assert!(msg.contains("runs namespaced"), "{msg}");
        assert!(msg.contains("mariadb: shared"), "{msg}");
        assert!(
            crate::remedy::for_cli(&msg).contains("--shared"),
            "the way onto main's data on purpose is named: {msg}"
        );
    }
    let record = ns.record();
    assert_eq!(record.mode, Some(crate::state::ServiceMode::Namespaced));
    assert!(record.processes.is_empty(), "{:?}", record.processes);
    assert_eq!(record.namespaces.len(), 1, "its namespace is kept");

    // Asked for, it is what it always was.
    let (report, _) = ns.start(Mode::Shared).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.record().mode, Some(crate::state::ServiceMode::Shared));
}

// The schema step is scoped to starts with data of their own: a namespaced
// start runs it, and a shared one of the same worktree does not.
#[test]
fn the_schema_step_runs_on_a_namespaced_start_and_not_on_a_shared_one() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, said) = ns.start(Mode::Shared).unwrap();
    drop(guard(&report));
    assert!(!ns.schema.exists());
    assert!(
        said.iter()
            .any(|l| l.contains("schema: not run") && l.contains("namespaced")),
        "{said:?}"
    );
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert!(ns.schema.exists());
}

// `status --env` is what a command run by hand needs, and for a
// namespaced worktree that is its own database.
#[test]
fn the_env_a_namespaced_worktree_gives_a_command_run_by_hand_names_its_own_database() {
    let mut ns = namespaced_fixture(MAIN_ENV);
    ns.fx.config.processes.get_mut("dev").unwrap().ports =
        Some(PortsSpec::Map(BTreeMap::from([(
            "PORT".to_string(),
            "web".to_string(),
        )])));
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    let env = resolved_env(&ns.fx.paths, &ns.fx.config, &ns.name).unwrap();
    assert_eq!(
        env.get("DATABASE_NAME").map(String::as_str),
        Some("shop__feat_one")
    );
    assert_eq!(env.get("DATABASE_PORT").map(String::as_str), Some("3306"));
}

// A running shared worktree switched to namespaced keeps serving on the
// main checkout's data until its own is made and filled, and only then are
// its processes replaced — a namespace that cannot be made costs nothing.
#[test]
fn a_running_worktree_switched_to_namespaced_keeps_serving_until_its_data_is_ready() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (first, _) = ns.start(Mode::Shared).unwrap();
    let _first = guard(&first);
    let before = first.started[0].record.pid;

    let (second, said) = ns.start(Mode::Namespaced).unwrap();
    let _second = guard(&second);
    assert!(
        said.iter()
            .any(|l| l.contains("dev keeps running on the shared services")),
        "{said:?}"
    );
    assert!(
        said.iter()
            .any(|l| l.contains("its own namespaces are ready")),
        "{said:?}"
    );
    let after = ns.record().processes["dev"].pid;
    assert_ne!(before, after, "replaced, onto its own data");
    assert_eq!(
        ns.record().mode,
        Some(crate::state::ServiceMode::Namespaced)
    );

    // And the other way, a namespace that cannot be made leaves the
    // running process exactly where it was.
    let other = namespaced_fixture(MAIN_ENV);
    let (running, _) = other.start(Mode::Shared).unwrap();
    let _running = guard(&running);
    std::fs::write(other.fake.join("deny"), "").unwrap();
    assert!(other.start(Mode::Namespaced).is_err());
    let record = other.record();
    assert_eq!(record.processes["dev"].pid, running.started[0].record.pid);
    assert!(crate::process::is_alive(record.processes["dev"].pid));
    assert_eq!(record.mode, Some(crate::state::ServiceMode::Shared));
}

// `--only` through a switch would leave the other processes on the data
// the switch is leaving; it is refused in the words of where it goes.
#[test]
fn only_one_process_cannot_be_moved_onto_namespaces() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, _) = ns.start(Mode::Shared).unwrap();
    let _guard = guard(&report);
    let e = format!(
        "{:#}",
        super::restart(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Some("dev"),
            Mode::Namespaced,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("namespaces of its own"), "{e}");
    assert!(e.contains("--only dev"), "{e}");
    assert_eq!(ns.fake("created"), "", "refused before anything was made");
}

// `start --only` is refused the same way, and as early: before a database
// is made in the developer's server for a start that will not happen.
#[test]
fn only_one_process_cannot_be_started_onto_namespaces_and_nothing_is_made() {
    let ns = namespaced_fixture(MAIN_ENV);
    let (report, _) = ns.start(Mode::Shared).unwrap();
    let _guard = guard(&report);
    let e = format!(
        "{:#}",
        super::start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Some("dev"),
            Mode::Namespaced,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("namespaces of its own"), "{e}");
    assert!(e.contains("--only dev"), "{e}");
    assert_eq!(ns.fake("created"), "", "refused before anything was made");
    assert!(ns.record().namespaces.is_empty(), "and nothing recorded");
}

// ---- redis slots -----------------------------------------------------------------

/// A fake `redis-cli` beside the fake `mariadb`: slot `n` holds as many
/// keys as `slot-<n>` says, a slot past 15 is out of range, and `flushed`
/// records every slot emptied. A script at `on-size-<n>` runs once, the
/// first time slot `n` is sized: what another command does meanwhile.
/// Sizing slot `n` fails with what `fail-<n>` says, when there is one.
fn fake_redis(paths: &PandoPaths) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let state = paths.home.join("fake-redis");
    std::fs::create_dir_all(&state).unwrap();
    let bin = paths.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!(
        r#"#!/bin/sh
state='{state}'
printf '%s\n' "$*" >> "$state/argv"
for last; do :; done
case "$*" in
  *" ping") echo PONG ;;
  *DBSIZE*)
    if [ "$last" -ge "$(cat "$state/databases" 2>/dev/null || echo 16)" ]; then echo "ERR DB index is out of range" >&2; exit 1; fi
    if [ -f "$state/on-size-$last" ]; then sh "$state/on-size-$last"; rm -f "$state/on-size-$last"; fi
    if [ -f "$state/fail-$last" ]; then cat "$state/fail-$last" >&2; exit 1; fi
    cat "$state/slot-$last" 2>/dev/null || echo 0 ;;
  *FLUSHDB*) echo 0 > "$state/slot-$last"; echo "$last" >> "$state/flushed"; echo OK ;;
  *) echo "unexpected: $*" >&2; exit 9 ;;
esac
"#,
        state = state.display()
    );
    std::fs::write(bin.join("redis-cli"), script).unwrap();
    std::fs::set_permissions(
        bin.join("redis-cli"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    state
}

const MAIN_ENV_WITH_REDIS: &str = "DATABASE_HOST=localhost\nDATABASE_PORT=3306\n\
                                   DATABASE_NAME=shop\nDATABASE_USER=app\n\
                                   DATABASE_PASSWORD=s3cret-pw\nREDIS_HOST=\nREDIS_PORT=6379\n\
                                   REDIS_DB=0\n";

/// The namespaced fixture with a Redis beside the MariaDB, addressed by
/// `REDIS_PORT` with its slot in `REDIS_DB`, as the origin project does.
fn slots_fixture(env: &str) -> (Namespaced, PathBuf) {
    slots_fixture_keyed(env, &["REDIS_PORT"])
}

/// [`slots_fixture`] with the Redis found by `keys` instead.
fn slots_fixture_keyed(env: &str, keys: &[&str]) -> (Namespaced, PathBuf) {
    let mut ns = namespaced_fixture(env);
    let keys: Vec<String> = keys
        .iter()
        .map(|key| format!("{key} = \"redis\""))
        .collect();
    let redis: Config = toml::from_str(&format!(
        "[[services]]\nkind = \"native\"\nname = \"redis\"\nenv = {{ {} }}\n",
        keys.join(", ")
    ))
    .unwrap();
    ns.fx.config.services.extend(redis.services);
    let fake = fake_redis(&ns.fx.paths);
    (ns, fake)
}

/// A worktree record holding slot `n` of the fixture's Redis, stopped
/// unless `running`, last used `hours` ago.
fn slot_holder(n: u32, running: bool, hours: i64) -> WorktreeRecord {
    let mut record = WorktreeRecord::new(format!("/abs/w{n}"), true);
    record.mode = Some(crate::state::ServiceMode::Namespaced);
    record.namespaces.push(crate::state::NamespaceRecord {
        service: "redis".into(),
        recipe: "redis".into(),
        kind: crate::state::NamespaceKind::Slot,
        host: "127.0.0.1".into(),
        port: 6379,
        name: n.to_string(),
        main: "0".into(),
        mains: Vec::new(),
        keys: Vec::new(),
        used_at: Utc::now() - chrono::Duration::hours(hours),
    });
    if running {
        record.processes.insert(
            "dev".into(),
            ProcessRecord {
                pid: std::process::id(),
                pgid: 999_997,
                started_at: Utc::now(),
                log_path: PathBuf::from("/nowhere"),
                ready_port: None,
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                swept: false,
                phase: Phase::Running { since: Utc::now() },
            },
        );
    }
    record
}

/// Writes holders for `slots` into the fixture's state.
fn hold(ns: &Namespaced, slots: impl IntoIterator<Item = (u32, bool)>) {
    let mut store = ns.fx.state();
    for (n, running) in slots {
        store
            .worktrees
            .insert(format!("w{n}"), slot_holder(n, running, i64::from(n)));
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
}

// A slot is given out only when no worktree holds it and the server says
// it is empty: keys nobody here put there are somebody else's.
#[test]
fn a_namespaced_start_takes_the_first_empty_slot_nobody_holds_and_tells_the_app() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    std::fs::write(redis.join("slot-1"), "5\n").unwrap();
    hold(&ns, [(2, false)]);
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("3"));
    assert_eq!(ns.env_line("REDIS_PORT").as_deref(), Some("6379"));
    assert_eq!(
        ns.env_line("DATABASE_NAME").as_deref(),
        Some("shop__feat_one")
    );
    assert!(
        said.iter().any(|l| l == "redis: slot 3, made just now"),
        "{said:?}"
    );
    let slot = ns
        .record()
        .namespaces
        .into_iter()
        .find(|n| n.service == "redis")
        .unwrap();
    assert_eq!((slot.name.as_str(), slot.main.as_str()), ("3", "0"));
    assert_eq!(slot.host, "127.0.0.1", "an empty REDIS_HOST is loopback");
    let argv = std::fs::read_to_string(redis.join("argv")).unwrap();
    assert!(
        !argv.contains(" 0\n") && !argv.contains("FLUSHDB"),
        "{argv}"
    );

    // And a restart keeps it, asking the server nothing about other slots.
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    std::fs::remove_file(redis.join("argv")).unwrap();
    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert!(said.iter().any(|l| l == "redis: slot 3"), "{said:?}");
    let argv = std::fs::read_to_string(redis.join("argv")).unwrap();
    assert!(!argv.contains("DBSIZE"), "{argv}");
}

// Every slot the main checkout's env files name is main's, not only the
// first: a queue's slot beside a cache's is never given out, even while it
// is empty, and a record that claims it is never emptied.
#[test]
fn a_second_slot_the_main_checkout_names_is_never_given_out_or_emptied() {
    let (ns, redis) = slots_fixture_keyed(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\n\
         REDIS_URL=redis://localhost:6379/0\nSIDEKIQ_REDIS_URL=redis://localhost:6379/1\n",
        &["REDIS_URL", "SIDEKIQ_REDIS_URL"],
    );
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    assert_eq!(
        ns.env_line("SIDEKIQ_REDIS_URL").as_deref(),
        Some("redis://localhost:6379/2"),
        "slot 1 is main's queue, empty or not"
    );
    // Recorded, for another project on the same server to know them by.
    let slot = ns
        .record()
        .namespaces
        .into_iter()
        .find(|n| n.service == "redis")
        .unwrap();
    assert_eq!(slot.mains, vec!["0".to_string(), "1".to_string()]);
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.name = "1".into();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    // rm reads config from disk, as it always does.
    std::fs::write(
        ns.fx.paths.config_file(),
        "[dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n\n\
         [[services]]\nkind = \"native\"\nname = \"redis\"\n\
         env = { REDIS_URL = \"redis\", SIDEKIQ_REDIS_URL = \"redis\" }\n",
    )
    .unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 1 is left as it is")
                && l.contains("main checkout's own slot")),
        "{said:?}"
    );
    assert!(!redis.join("flushed").exists(), "main's queue was emptied");
}

// A URL with no path beside `REDIS_DB=10` names two slots, and neither is
// ever a worktree's.
#[test]
fn a_slot_a_key_names_beside_a_url_with_no_path_is_the_main_checkouts_too() {
    let (ns, _redis) = slots_fixture_keyed(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\n\
         REDIS_URL=redis://localhost:6379\nREDIS_DB=10\n",
        &["REDIS_URL"],
    );
    hold(&ns, (1..=9).map(|n| (n, false)));
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("11"));
    assert_eq!(
        ns.env_line("REDIS_URL").as_deref(),
        Some("redis://localhost:6379/11")
    );
}

// A URL can name its slot in its query, `?db=2`, which redis-py and the
// clients built on it read before the path. It was read as slot 0: slot 2
// was given out while it was empty, and the URL the worktree got still
// said `db=2`, so its app went on using main's slot whatever pando
// recorded. That slot is main's, never given out or emptied, and the
// worktree's URL names its own in both places.
#[test]
fn a_slot_a_urls_query_names_is_the_main_checkouts_and_the_worktree_is_told_its_own_there() {
    let (ns, redis) = slots_fixture_keyed(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\n\
         REDIS_URL=redis://localhost:6379?db=2\n",
        &["REDIS_URL"],
    );
    hold(&ns, [(1, false)]);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    assert_eq!(
        ns.env_line("REDIS_URL").as_deref(),
        Some("redis://localhost:6379/3?db=3")
    );
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.name = "2".into();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    // rm reads config from disk, as it always does.
    std::fs::write(
        ns.fx.paths.config_file(),
        "[dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n\n\
         [[services]]\nkind = \"native\"\nname = \"redis\"\nenv = { REDIS_URL = \"redis\" }\n",
    )
    .unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 2 is left as it is")
                && l.contains("main checkout's own slot")),
        "{said:?}"
    );
    assert!(!redis.join("flushed").exists(), "main's slot was emptied");
}

// A slot recorded before the query's `db` was read has main's as slot 0,
// and a start only marked it used: `status --env`, which finds the slot by
// the main checkout's today, found none and said to start it namespaced
// again, which changed nothing. A start records main's as it is now.
#[test]
fn a_start_records_the_slot_the_main_checkout_is_known_by_now() {
    let (mut ns, _redis) = slots_fixture_keyed(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\n\
         REDIS_URL=redis://localhost:6379?db=2\n",
        &["REDIS_URL"],
    );
    ns.fx.config.processes.get_mut("dev").unwrap().ports =
        Some(PortsSpec::Map(BTreeMap::from([(
            "PORT".to_string(),
            "web".to_string(),
        )])));
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.main = "0".into();
        slot.mains = Vec::new();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();

    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    let slot = ns
        .record()
        .namespaces
        .into_iter()
        .find(|n| n.service == "redis")
        .unwrap();
    assert_eq!((slot.name.as_str(), slot.main.as_str()), ("1", "2"));
    let env = resolved_env(&ns.fx.paths, &ns.fx.config, &ns.name).unwrap();
    assert_eq!(
        env.get("REDIS_URL").map(String::as_str),
        Some("redis://localhost:6379/1?db=1")
    );
}

/// Writes a state for another project under the same pando home, with
/// one worktree holding `record`.
fn hold_elsewhere(ns: &Namespaced, record: WorktreeRecord) {
    let mut theirs = state::State::new();
    theirs.worktrees.insert("feat+theirs".into(), record);
    let file = ns
        .fx
        .paths
        .projects_dir()
        .join("other-1a2b3c4d")
        .join("state.json");
    state::save(&file, &theirs).unwrap();
}

// A Redis on a port is the machine's. A slot another project's worktree
// holds is not given out here even while it is empty — its app may not
// have written yet — nor is the one its main checkout uses; and a record
// here that claims one of them is never emptied.
#[test]
fn a_slot_another_project_records_is_never_given_out_or_emptied() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let mut theirs = slot_holder(1, false, 1);
    theirs.namespaces[0].main = "2".into();
    hold_elsewhere(&ns, theirs);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("3"));
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.name = "1".into();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 1 is left as it is")
                && l.contains("feat+theirs of project other-1a2b3c4d")
                && !l.contains("will not mention it again")),
        "{said:?}"
    );
    assert!(
        !redis.join("flushed").exists(),
        "another project's slot was emptied"
    );
}

// Every slot another project's main checkout names is its own, not only
// the first: its records carry them all. Only the first was read, so its
// queue's slot beside its cache's was given out here while it was empty,
// and emptied by the `rm` of the worktree it was given to.
#[test]
fn a_second_slot_another_projects_main_checkout_names_is_never_given_out_or_emptied() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let mut theirs = slot_holder(3, false, 1);
    theirs.namespaces[0].main = "1".into();
    theirs.namespaces[0].mains = vec!["1".into(), "2".into()];
    hold_elsewhere(&ns, theirs);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("4"));
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.name = "2".into();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 2 is left as it is")
                && l.contains("main checkout's own in project other-1a2b3c4d")
                && !l.contains("will not mention it again")),
        "{said:?}"
    );
    assert!(
        !redis.join("flushed").exists(),
        "another project's main slot was emptied"
    );
}

// Every slot held counts another project's too, and offers only this
// project's own to free.
#[test]
fn every_slot_held_counts_another_projects_and_offers_only_this_ones() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=14).map(|n| (n, false)));
    hold_elsewhere(&ns, slot_holder(15, false, 1));
    let asked = std::cell::RefCell::new(None::<Question>);
    let ask = |q: &Question| -> Result<Answer> {
        asked.replace(Some(q.clone()));
        Ok(Answer::None)
    };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("no slot was freed"), "{e}");
    let question = asked.into_inner().expect("asked which one to free");
    assert_eq!(question.options.len(), 14);
    assert!(
        question
            .details
            .iter()
            .any(|d| d.contains("slot 15 (feat+theirs of project other-1a2b3c4d")),
        "{:?}",
        question.details
    );
    assert!(!redis.join("flushed").exists());
}

// A project deleted without `pando rm` still holds its slots, and nothing
// here can let them go: the error names its records' directory, and says
// its checkout is gone.
#[test]
fn every_slot_held_names_where_another_projects_records_are() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=14).map(|n| (n, true)));
    hold_elsewhere(&ns, slot_holder(15, false, 1));
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    let dir = ns.fx.paths.projects_dir().join("other-1a2b3c4d");
    assert!(
        e.contains("slot 15 (feat+theirs of project other-1a2b3c4d, whose checkout is gone)"),
        "{e}"
    );
    assert!(
        e.contains(&format!("held by its records in {}", dir.display()))
            && e.contains("until that directory is removed"),
        "{e}"
    );
    assert!(!redis.join("flushed").exists());
}

// git 2.48 and later can write a worktree's `gitdir:` relative to the
// worktree. It was resolved from pando's own directory instead, so a live
// project's slot read as one whose checkout is gone.
#[test]
fn a_relative_gitdir_is_resolved_from_the_worktree_that_holds_the_slot() {
    let (ns, _redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=14).map(|n| (n, true)));
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("shop/.git/worktrees/feat+theirs")).unwrap();
    let worktree = dir.path().join("worktrees/feat+theirs");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        "gitdir: ../../shop/.git/worktrees/feat+theirs\n",
    )
    .unwrap();
    let mut theirs = slot_holder(15, false, 1);
    theirs.path = worktree;
    hold_elsewhere(&ns, theirs);
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(
        e.contains("slot 15 (feat+theirs of project other-1a2b3c4d)"),
        "{e}"
    );
}

// A project whose state does not load may record the same slot: `rm`
// leaves it, says which project and why, and empties nothing.
#[test]
fn rm_empties_no_slot_while_another_projects_state_cannot_be_read() {
    let (ns, redis) = stopped_namespaced();
    let file = ns
        .fx
        .paths
        .projects_dir()
        .join("other-1a2b3c4d")
        .join("state.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{\"version\": 99, \"worktrees\": {}}").unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 1 is left as it is")
                && l.contains("project other-1a2b3c4d as well, whose state could not be read")),
        "{said:?}"
    );
    // That project may go on naming it, so this is not the last of it.
    assert!(
        said.iter()
            .all(|l| !l.contains("will not mention it again")),
        "{said:?}"
    );
    assert!(!redis.join("flushed").exists());
    assert_eq!(ns.fake("dropped"), "", "and no database was dropped");
}

// The same project may record any slot, as a worktree's or as its main
// checkout's: no slot is given out while its state cannot be read, and a
// start that could ask which stopped worktree frees one asks nothing.
#[test]
fn no_slot_is_given_out_while_another_projects_state_cannot_be_read() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let file = ns
        .fx
        .paths
        .projects_dir()
        .join("other-1a2b3c4d")
        .join("state.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{\"version\": 99, \"worktrees\": {}}").unwrap();
    let unread = "no slot of redis on 127.0.0.1:6379 is given out while project \
                  other-1a2b3c4d's state cannot be read";

    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(e.contains(unread) && e.contains("version 99"), "{e}");
    let store = ns.fx.state();
    assert!(
        store
            .worktrees
            .get(&ns.name)
            .into_iter()
            .flat_map(|record| &record.namespaces)
            .all(|n| n.service != "redis"),
        "{:?}",
        store.worktrees.get(&ns.name)
    );

    hold(&ns, (1..=15).map(|n| (n, false)));
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains(unread), "{e}");
    assert!(!redis.join("flushed").exists());
}

// Two namespaced starts at once: the slot each is given is taken under the
// lock, against state as it is by then, so another worktree's start that
// was given slot 1 first leaves this one slot 2 — never both on one.
#[test]
fn two_starts_at_once_never_record_the_same_slot() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    // The other start records slot 1 while this one is asking its size.
    let mut raced = ns.fx.state();
    raced
        .worktrees
        .insert("w-racer".into(), slot_holder(1, false, 0));
    let racing = ns.fx.paths.home.join("raced.json");
    state::save(&racing, &raced).unwrap();
    std::fs::write(
        redis.join("on-size-1"),
        format!(
            "cp '{}' '{}'\n",
            racing.display(),
            ns.fx.paths.state_file().display()
        ),
    )
    .unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert!(!redis.join("on-size-1").exists(), "the race never ran");
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"), "{said:?}");
    let store = ns.fx.state();
    assert_eq!(store.worktrees["w-racer"].namespaces[0].name, "1");
    let ours: Vec<&str> = store.worktrees[&ns.name]
        .namespaces
        .iter()
        .filter(|n| n.service == "redis")
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(ours, vec!["2"]);
}

// The same across projects on one Redis. Each project's lock is its own,
// and each start read the other projects' records before either wrote:
// two starts at once could both be given one empty slot, with nothing
// after to say so. A slot is handed out under a lock every project takes,
// held from reading the others' records to writing this one's.
#[test]
fn a_slot_is_handed_out_under_a_lock_every_project_takes() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let marks = tempdir().unwrap();
    std::fs::write(
        redis.join("on-size-1"),
        format!(
            "touch '{m}/sizing'\n\
             i=0\nwhile [ ! -e '{m}/release' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n",
            m = marks.path().display()
        ),
    )
    .unwrap();
    std::thread::scope(|scope| {
        let starting = scope.spawn(|| ns.start(Mode::Namespaced));
        let sizing = wait_until(Duration::from_secs(20), || {
            marks.path().join("sizing").exists()
        });
        let held = sizing
            && state::try_lock(&ns.fx.paths.slots_lock_file())
                .unwrap()
                .is_none();
        std::fs::write(marks.path().join("release"), "").unwrap();
        let (report, said) = starting.join().unwrap().unwrap();
        let _guard = guard(&report);
        assert!(sizing, "slot 1 was never sized");
        assert!(
            held,
            "slot 1 was sized outside the lock every project takes"
        );
        assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("1"), "{said:?}");
    });
    assert!(
        state::try_lock(&ns.fx.paths.slots_lock_file())
            .unwrap()
            .is_some(),
        "and let go once the slot was recorded"
    );
}

// A slot freed from this worktree while it was starting, and given to
// another, is not written back into its record: the start stops, and the
// slot stays the other one's alone.
#[test]
fn a_slot_given_to_another_worktree_while_this_one_started_is_not_written_back() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let paths = ns.fx.paths.clone();
    let name = ns.name.clone();
    let progress = |line: &str| {
        if line != "redis: slot 1, made just now" {
            return;
        }
        // What a start freeing this worktree's slot, and taking it, does.
        let mut store = state::load(&paths.state_file()).unwrap();
        store
            .worktrees
            .get_mut(&name)
            .unwrap()
            .namespaces
            .retain(|n| n.service != "redis");
        store
            .worktrees
            .insert("w-racer".into(), slot_holder(1, false, 0));
        state::save(&paths.state_file(), &store).unwrap();
    };
    let e = format!(
        "{:#}",
        super::start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            None,
            Mode::Namespaced,
            &progress
        )
        .unwrap_err()
    );
    assert!(
        e.contains("redis slot 1 was given to w-racer while this start was starting"),
        "{e}"
    );
    let store = ns.fx.state();
    assert!(
        store.worktrees[&ns.name]
            .namespaces
            .iter()
            .all(|n| n.service != "redis"),
        "{:?}",
        store.worktrees[&ns.name].namespaces
    );
    assert!(store.worktrees[&ns.name].processes.is_empty());
    assert!(!redis.join("flushed").exists());
}

// The same after the start has written the slot down and let the lock go
// for its hooks: its record names the slot and none of its processes is
// up, so another worktree's start can offer it as stopped, empty it and
// take it. The app was spawned onto that slot all the same, which only the
// other record named, for its `rm` to empty under it. The start stops.
#[test]
fn a_slot_freed_from_this_worktree_while_its_hooks_ran_is_not_spawned_onto() {
    let (ns, _redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let paths = ns.fx.paths.clone();
    let name = ns.name.clone();
    let progress = |line: &str| {
        if !line.starts_with("running the schema hook") {
            return;
        }
        // What a start freeing this worktree's slot, and taking it, does.
        let mut store = state::load(&paths.state_file()).unwrap();
        store
            .worktrees
            .get_mut(&name)
            .unwrap()
            .namespaces
            .retain(|n| n.service != "redis");
        store
            .worktrees
            .insert("w-racer".into(), slot_holder(1, false, 0));
        state::save(&paths.state_file(), &store).unwrap();
    };
    let e = format!(
        "{:#}",
        super::start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            None,
            Mode::Namespaced,
            &progress
        )
        .unwrap_err()
    );
    assert!(
        e.contains("redis slot 1 was freed from this worktree while this start was starting"),
        "{e}"
    );
    let store = ns.fx.state();
    assert!(store.worktrees[&ns.name].processes.is_empty());
    assert_ne!(
        store.worktrees[&ns.name].mode,
        Some(crate::state::ServiceMode::Namespaced)
    );
    assert!(!ns.seen.exists(), "the app was started");
}

// Two records naming one slot — a state written before slots were taken
// under the lock, or edited by hand — stop neither worktree for good: the
// one starting lets it go, unemptied, as the other's, and gets its own.
#[test]
fn a_slot_two_records_name_is_let_go_by_the_one_starting_and_emptied_by_neither() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    hold(&ns, [(1, false)]);

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter().any(|l| l
            == "redis: slot 1 is let go, not emptied — w1's record names it too — and this \
                worktree gets one of its own"),
        "{said:?}"
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"));
    let store = ns.fx.state();
    assert_eq!(store.worktrees["w1"].namespaces[0].name, "1");
    let ours: Vec<&str> = store.worktrees[&ns.name]
        .namespaces
        .iter()
        .filter(|n| n.service == "redis")
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(ours, vec!["2"]);
    assert!(!redis.join("flushed").exists());
}

// The same when the other record is another project's: a Redis on a port
// is the machine's, and a slot two projects' records name — one recorded
// before slots were handed out under a lock every project takes — was kept
// by both, shared for good with nothing to say so. Nor is a slot another
// project's main checkout uses kept.
#[test]
fn a_slot_another_projects_record_names_too_is_let_go_by_the_one_starting_and_emptied_by_neither() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    hold_elsewhere(&ns, slot_holder(1, false, 1));

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let running = guard(&report);
    assert!(
        said.iter().any(|l| l
            == "redis: slot 1 is let go, not emptied — feat+theirs of project other-1a2b3c4d's \
                record names it too — and this worktree gets one of its own"),
        "{said:?}"
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"));
    let ours = || -> Vec<String> {
        ns.record()
            .namespaces
            .into_iter()
            .filter(|n| n.service == "redis")
            .map(|n| n.name)
            .collect()
    };
    assert_eq!(ours(), vec!["2".to_string()]);
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let mut theirs = slot_holder(5, false, 1);
    theirs.namespaces[0].main = "2".into();
    hold_elsewhere(&ns, theirs);
    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter().any(|l| l
            == "redis: slot 2 is let go, not emptied — the main checkout of project \
                other-1a2b3c4d uses it — and this worktree gets one of its own"),
        "{said:?}"
    );
    assert_eq!(ours(), vec!["1".to_string()]);
    assert!(!redis.join("flushed").exists());
}

// A running worktree keeps a slot another record names too: a start that
// leaves its processes running, or replaces only one of them, cannot move
// them, and a record naming a new slot under them would leave the old one
// to the other record alone, for its `rm` to empty under the running app.
#[test]
fn a_running_worktree_keeps_a_slot_another_record_names_and_rm_of_the_other_empties_nothing() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _running = guard(&report);
    hold(&ns, [(1, false)]);
    let kept = "redis: slot 1 is kept while this worktree runs — w1's record names it too — stop \
                it and start it again, and it gets one of its own";

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _again = guard(&report);
    assert!(report.started.is_empty(), "{said:?}");
    assert!(said.iter().any(|l| l == kept), "{said:?}");

    std::fs::remove_file(&ns.seen).unwrap();
    let (said, progress) = collecting();
    let report = super::restart(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Some("dev"),
        Mode::Remembered,
        &progress,
    )
    .unwrap();
    let _restarted = guard(&report);
    assert!(
        said.borrow().iter().any(|l| l == kept),
        "{:?}",
        said.borrow()
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("1"));
    let ours: Vec<String> = ns
        .record()
        .namespaces
        .into_iter()
        .filter(|n| n.service == "redis")
        .map(|n| n.name)
        .collect();
    assert_eq!(ours, vec!["1".to_string()]);

    super::rm(&ns.fx.paths, "w1", false, false, &noop).unwrap();
    assert!(
        !redis.join("flushed").exists(),
        "the running app's slot was emptied"
    );
}

// The same for an app whose only process owns no port and outlived the
// shell that started it: the start leaves it running, as `status` calls
// it, so its slot is kept too. Its leader alone said it had stopped, and
// the record moved to a new slot while the app went on using the old one.
#[test]
fn a_portless_app_that_outlived_its_shell_keeps_a_slot_another_record_names() {
    let (mut ns, _redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    with_dev(
        &mut ns.fx,
        ProcessConfig {
            cmd: format!("env > {}; sleep 30 & exit 0", ns.seen.display()),
            ports: Some(crate::config::PortsSpec::List(Vec::new())),
            ..Default::default()
        },
    );
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _running = guard(&report);
    let leader = report.started[0].record.pid;
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(leader)
    }));
    hold(&ns, [(1, false)]);

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    assert!(report.started.is_empty(), "{said:?}");
    assert!(
        said.iter()
            .any(|l| l.starts_with("redis: slot 1 is kept while this worktree runs")),
        "{said:?}"
    );
    let ours: Vec<String> = ns
        .record()
        .namespaces
        .into_iter()
        .filter(|n| n.service == "redis")
        .map(|n| n.name)
        .collect();
    assert_eq!(ours, vec!["1".to_string()]);
}

// A worktree running shared is on the main checkout's data, not on the
// slot its record still names from a namespaced run, and the start that
// makes it namespaced replaces every process: a slot another record names
// too is let go then, and a terminal start asks for one first when every
// other slot is held, rather than keeping one its app was never on.
#[test]
fn a_worktree_running_shared_lets_a_slot_another_record_names_go_when_started_namespaced() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let namespaced = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("1"));
    drop(namespaced);
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    std::fs::remove_file(&ns.seen).unwrap();
    let (report, _) = ns.start(Mode::Shared).unwrap();
    let _shared = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB"), None);
    hold(&ns, (1..=15).map(|n| (n, false)));

    free_w2(&ns);
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "2\n"
    );

    std::fs::remove_file(&ns.seen).unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _namespaced = guard(&report);
    assert!(
        said.iter().any(|l| l
            == "redis: slot 1 is let go, not emptied — w1's record names it too — and this \
                worktree gets one of its own"),
        "{said:?}"
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"));
    let ours: Vec<String> = ns
        .record()
        .namespaces
        .into_iter()
        .filter(|n| n.service == "redis")
        .map(|n| n.name)
        .collect();
    assert_eq!(ours, vec!["2".to_string()]);
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "2\n",
        "w1's slot was emptied"
    );
}

// A record left at another path by a worktree of the same name has its
// processes stopped by the start that finds it, so they are not on its
// slot for the start to keep: one another record names too is let go.
#[test]
fn a_record_left_at_another_path_keeps_no_slot_another_record_names_for_its_processes() {
    let (ns, _redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let _running = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("1"));
    hold(&ns, [(1, false)]);
    let mut store = ns.fx.state();
    store.worktrees.get_mut(&ns.name).unwrap().path = PathBuf::from("/abs/elsewhere");
    state::save(&ns.fx.paths.state_file(), &store).unwrap();

    std::fs::remove_file(&ns.seen).unwrap();
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _again = guard(&report);
    assert!(
        said.iter()
            .any(|l| l.starts_with("redis: slot 1 is let go, not emptied")),
        "{said:?}"
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"));
}

// A worktree given main's second slot before main's every slot was known
// keeps it no longer: the next start lets it go, unemptied, and gives the
// worktree one of its own.
#[test]
fn a_recorded_slot_the_main_checkout_names_is_let_go_and_never_emptied() {
    let (ns, redis) = slots_fixture_keyed(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\n\
         REDIS_URL=redis://localhost:6379/0\nSIDEKIQ_REDIS_URL=redis://localhost:6379/1\n",
        &["REDIS_URL", "SIDEKIQ_REDIS_URL"],
    );
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    let mut store = ns.fx.state();
    for slot in store
        .worktrees
        .get_mut(&ns.name)
        .unwrap()
        .namespaces
        .iter_mut()
        .filter(|n| n.service == "redis")
    {
        slot.name = "1".into();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let _ = std::fs::remove_file(&ns.seen);

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter()
            .any(|l| l.starts_with("redis: slot 1 is let go, not emptied")
                && l.contains("the main checkout's env files name it")),
        "{said:?}"
    );
    assert_eq!(
        ns.env_line("SIDEKIQ_REDIS_URL").as_deref(),
        Some("redis://localhost:6379/2")
    );
    assert!(!redis.join("flushed").exists(), "main's queue was emptied");
}

// Decision 3: an app that reads no slot setting has nowhere to be told
// another slot, so its Redis stays shared — said in one line — and the
// database is still its own.
#[test]
fn a_redis_the_app_reads_no_slot_setting_for_stays_shared_and_says_so() {
    let (ns, redis) = slots_fixture(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\nREDIS_PORT=6379\n",
    );
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter()
            .any(|l| l == "redis: shared — the app reads no slot setting"),
        "{said:?}"
    );
    assert_eq!(ns.record().namespaces.len(), 1);
    assert!(
        !redis.join("argv").exists(),
        "Redis was never asked anything"
    );
    assert_eq!(
        std::fs::read_to_string(&ns.schema).unwrap(),
        "shop__feat_one\n",
        "a Redis left shared does not keep the schema step off the worktree's own database"
    );
}

// Tried as written, the worktree's database was made as a user called
// `${…}`. A login the main checkout's env builds from a variable nothing
// sets stops the start, naming it, with nothing made.
#[test]
fn a_namespaced_start_stops_on_a_login_it_cannot_expand_with_nothing_made() {
    let ns = namespaced_fixture(
        "DATABASE_HOST=localhost\nDATABASE_PORT=3306\nDATABASE_NAME=shop\n\
         DATABASE_USER=${PANDO_TEST_UNSET_USER}\nDATABASE_PASSWORD=s3cret-pw\n",
    );
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(
        e.contains("DATABASE_USER in .env holds ${PANDO_TEST_UNSET_USER}")
            && e.contains("[namespaced.mariadb]"),
        "{e}"
    );
    assert_eq!(ns.fake("created"), "", "nothing was made");
}

// A slot is not a database. A namespaced start that gave Redis a slot and
// left the database on the main checkout's runs no schema step: its
// migrations would run against main's database, which is what the step's
// scope exists to prevent.
#[test]
fn a_namespaced_start_whose_database_stays_shared_runs_no_schema_step() {
    let (ns, _redis) =
        slots_fixture("DATABASE_PORT=3306\nDATABASE_USER=app\nREDIS_PORT=6379\nREDIS_DB=0\n");
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(
        ns.record().mode,
        Some(crate::state::ServiceMode::Namespaced)
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("1"));
    assert!(
        !ns.schema.exists(),
        "the schema step ran against main's database"
    );
    assert!(
        said.iter().any(|l| l.starts_with("schema: not run")
            && l.contains("mariadb stays on the main checkout's data")),
        "{said:?}"
    );
    assert!(
        said.iter().all(|l| !l.contains("on = \"always\"")),
        "a setting that would run it against main's is not offered: {said:?}"
    );
}

// The same when the worktree has a database of its own and another one
// stays shared: the step would reach that one.
#[test]
fn a_database_pando_knows_no_namespace_for_keeps_the_schema_step_from_running() {
    let mut ns = namespaced_fixture(&format!("{MAIN_ENV}POSTGRES_PORT=5432\n"));
    let postgres: Config = toml::from_str(
        "[[services]]\nkind = \"native\"\nname = \"postgres\"\nenv = { POSTGRES_PORT = \"postgres\" }\n",
    )
    .unwrap();
    ns.fx.config.services.extend(postgres.services);
    let (report, said) = ns.start(Mode::Namespaced).unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.fake("created"), "shop__feat_one\n");
    assert!(
        !ns.schema.exists(),
        "the schema step ran beside a shared postgres"
    );
    assert!(
        said.iter()
            .any(|l| l.starts_with("schema: not run") && l.contains("postgres stays")),
        "{said:?}"
    );
}

// Which shared services a step could reach main's data through: a
// database pando cannot give a namespace in is one, a mail catcher is not.
#[test]
fn a_mail_catcher_left_shared_is_no_data_a_step_could_reach() {
    let mut fx = fixture();
    std::fs::write(
        fx.root.join("docker-compose.yml"),
        "services:\n  db:\n    image: mariadb:11\n  mail:\n    image: axllent/mailpit:latest\n  \
         pg:\n    image: postgres:16\n",
    )
    .unwrap();
    std::fs::write(
        fx.root.join(".env"),
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nSMTP_PORT=1025\nPG_PORT=5432\n",
    )
    .unwrap();
    let config: Config = toml::from_str(
        "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"db\", \"mail\", \"pg\"]\n\
         env = { DATABASE_PORT = \"db\", SMTP_PORT = \"mail\", PG_PORT = \"pg\" }\n",
    )
    .unwrap();
    fx.config.services = config.services;
    let plan = super::namespaced::plan(&fx.paths, &fx.config);
    assert_eq!(
        plan.targets
            .iter()
            .map(|t| t.service.as_str())
            .collect::<Vec<_>>(),
        vec!["db"]
    );
    assert_eq!(
        plan.shared
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>(),
        vec!["mail", "pg"]
    );
    assert_eq!(plan.shared_data, vec!["pg".to_string()]);
}

// Decision 9: every slot held, so a start that can ask asks which stopped
// worktree gives its slot up — never a running one, never on anyone's
// behalf — and the one chosen is emptied, freed, and taken.
#[test]
fn every_slot_held_asks_which_stopped_worktree_gives_up_its_slot() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=15).map(|n| (n, n == 7)));
    std::fs::write(redis.join("slot-4"), "12\n").unwrap();

    // A start that cannot ask says who holds them.
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(
        e.contains("every slot of redis") && e.contains("w7 (slot 7, running)"),
        "{e}"
    );
    assert!(e.contains("asks which stopped one to free"), "{e}");

    let asked = std::cell::RefCell::new(None::<Question>);
    let ask = |q: &Question| -> Result<Answer> {
        if q.slot == crate::detect::Slot::Login {
            panic!("the env files have the login");
        }
        asked.replace(Some(q.clone()));
        let index = q
            .options
            .iter()
            .position(|(value, _)| value == "w4")
            .unwrap();
        Ok(Answer::Choice(index))
    };
    let (said, progress) = collecting();
    let config = resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Namespaced,
        &ask,
        &progress,
    )
    .unwrap();
    let question = asked.into_inner().expect("asked");
    assert_eq!(question.slot, crate::detect::Slot::FreeSlot);
    assert_eq!(question.options.len(), 14, "the running one is not offered");
    assert!(question.options.iter().all(|(value, _)| value != "w7"));
    assert!(
        question
            .options
            .iter()
            .any(|(value, why)| value == "w4" && why.starts_with("slot 4, last ran 4 h ago")),
        "{:?}",
        question.options
    );
    assert!(
        question.details.iter().any(|d| d.contains("w7")),
        "{:?}",
        question.details
    );
    assert_eq!(
        question.preselect, None,
        "nothing is taken on anyone's behalf"
    );
    assert!(recommended(&question).is_none());
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "4\n"
    );
    assert!(
        ns.fx.state().worktrees["w4"].namespaces.is_empty(),
        "released"
    );
    assert!(
        said.borrow()
            .iter()
            .any(|l| l.contains("slot 4 emptied and freed")),
        "{:?}",
        said.borrow()
    );

    let report = super::start(
        &ns.fx.paths,
        &config,
        &ns.name,
        None,
        Mode::Namespaced,
        &noop,
    )
    .unwrap();
    let _guard = guard(&report);
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("4"));
}

#[test]
fn every_slot_held_by_a_running_worktree_stops_the_start_naming_them() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=15).map(|n| (n, true)));
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("held by a running worktree"), "{e}");
    assert!(e.contains("w15 (slot 15, running)"), "{e}");
    assert!(!redis.join("flushed").exists());
}

// A worktree whose app is still up is never offered to free: not one
// whose only process owns no port and outlived the shell that started it,
// which `status` calls running, nor one whose start failed its readiness
// wait while its app went on serving. Its leader alone said stopped, and
// the slot was emptied under the app.
#[test]
fn a_worktree_whose_app_is_still_up_is_never_offered_to_free_its_slot() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=15).map(|n| (n, false)));
    let log = ns.fx.paths.log_file("w2", "dev");
    let backgrounded = crate::testutil::spawn_guarded("sleep 30 & exit 0", &ns.fx.root, &log);
    assert!(wait_until(Duration::from_secs(5), || {
        !crate::process::is_alive(backgrounded.pid)
    }));
    assert!(crate::process::group_alive(backgrounded.pgid));
    let mut store = ns.fx.state();
    let mut portless = slot_holder(2, true, 2).processes.remove("dev").unwrap();
    portless.pid = backgrounded.pid;
    portless.pgid = backgrounded.pgid;
    store
        .worktrees
        .get_mut("w2")
        .unwrap()
        .processes
        .insert("dev".into(), portless);
    let mut failed = slot_holder(3, true, 3);
    failed.processes.get_mut("dev").unwrap().phase = Phase::Failed {
        at: Utc::now(),
        reason: "timeout: nothing bound port 3000 in 30s".into(),
    };
    store.worktrees.insert("w3".into(), failed);
    state::save(&ns.fx.paths.state_file(), &store).unwrap();

    let asked = std::cell::RefCell::new(None::<Question>);
    let ask = |q: &Question| -> Result<Answer> {
        asked.replace(Some(q.clone()));
        let index = q.options.iter().position(|(value, _)| value == "w4");
        Ok(Answer::Choice(index.expect("w4 is offered")))
    };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Namespaced,
        &ask,
        &noop,
    )
    .unwrap();
    let question = asked.into_inner().expect("asked which one to free");
    assert!(
        question
            .options
            .iter()
            .all(|(value, _)| value != "w2" && value != "w3"),
        "{:?}",
        question.options
    );
    assert_eq!(question.options.len(), 13);
    assert!(
        question
            .details
            .iter()
            .any(|d| d.starts_with("running, so not offered:")
                && d.contains("w2 (slot 2, running)")
                && d.contains("w3 (slot 3, running)")),
        "{:?}",
        question.details
    );
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "4\n"
    );
}

/// Asks which slot to free, answering `w2`; the question it put.
fn free_w2(ns: &Namespaced) -> Question {
    let asked = std::cell::RefCell::new(None::<Question>);
    let ask = |q: &Question| -> Result<Answer> {
        asked.replace(Some(q.clone()));
        let index = q.options.iter().position(|(value, _)| value == "w2");
        Ok(Answer::Choice(index.expect("w2 is offered")))
    };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Namespaced,
        &ask,
        &noop,
    )
    .unwrap();
    asked.into_inner().expect("asked which one to free")
}

// A Redis with fewer databases than the recipe says has no slot past its
// last: that is no free slot, so a start asks which stopped worktree
// gives one up rather than failing on the first it cannot size.
#[test]
fn a_redis_with_fewer_slots_than_the_recipe_asks_which_stopped_worktree_frees_one() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    std::fs::write(redis.join("databases"), "4\n").unwrap();
    hold(&ns, (1..=3).map(|n| (n, false)));

    // A start that cannot ask says why none is free, and how to free one.
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(
        e.contains("no slot of redis on 127.0.0.1:6379 is free")
            && e.contains("slot 4 could not be asked how full it is")
            && e.contains("out of range")
            && e.contains("w2 (slot 2, last ran")
            && e.contains("asks which stopped one to free"),
        "{e}"
    );

    let question = free_w2(&ns);
    assert!(
        question
            .prompt
            .starts_with("No slot of redis on 127.0.0.1:6379 is free."),
        "{}",
        question.prompt
    );
    assert!(
        question
            .details
            .iter()
            .any(|d| d.starts_with("slot 4 could not be asked")),
        "{:?}",
        question.details
    );
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "2\n"
    );
}

// Only a slot the server says is out of range ends the walk: one it could
// not be asked about for any other reason says nothing of the slots after
// it, so the start fails with what the server said, and asks nobody to
// give up their keys while an empty slot may be there.
#[test]
fn a_slot_the_server_could_not_be_asked_about_fails_the_start_and_frees_nothing() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=3).map(|n| (n, false)));
    std::fs::write(
        redis.join("fail-4"),
        "Could not connect to Redis at 127.0.0.1:6379: Connection reset by peer\n",
    )
    .unwrap();
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(
        e.contains("how full slot 4 is") && e.contains("Connection reset by peer"),
        "{e}"
    );

    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(e.contains("Connection reset by peer"), "{e}");
    assert!(!e.contains("none from it on is given out"), "{e}");
    assert!(!redis.join("flushed").exists());
}

// A recorded slot the start will let go is no slot of its own: with every
// other one held by a stopped worktree, a terminal start asks which one
// frees its slot the first time, rather than failing to say that it asks.
// The other record naming that slot is not offered, since the guard would
// not empty a slot two records name. While the worktree runs it keeps the
// slot, so nothing is asked then.
#[test]
fn a_start_that_will_let_its_slot_go_asks_which_stopped_worktree_frees_one() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    let running = guard(&report);
    hold(&ns, (1..=15).map(|n| (n, false)));
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Remembered,
        &ask,
        &noop,
    )
    .unwrap();
    drop(running);
    stop(&ns.fx.paths, &ns.name, None).unwrap();

    let question = free_w2(&ns);
    assert!(
        question
            .options
            .iter()
            .all(|(value, _)| *value != ns.name && value != "w1"),
        "{:?}",
        question.options
    );
    assert_eq!(question.options.len(), 14);
    assert!(
        question
            .details
            .iter()
            .any(|d| d.starts_with("not offered: w1 (slot 1, last ran")
                && d.contains(&format!("is recorded for {} as well", ns.name))),
        "{:?}",
        question.details
    );
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "2\n"
    );

    let (report, said) = ns.start(Mode::Remembered).unwrap();
    let _guard = guard(&report);
    assert!(
        said.iter()
            .any(|l| l.starts_with("redis: slot 1 is let go, not emptied")),
        "{said:?}"
    );
    assert_eq!(ns.env_line("REDIS_DB").as_deref(), Some("2"));
}

// A stopped worktree whose slot this one's record names too is no answer
// either, so with it the only stopped one nothing is asked: the start lets
// this worktree's claim go and says every slot is held, and the next one
// offers the other, whose slot is then its alone to give up.
#[test]
fn a_stopped_worktree_sharing_this_ones_slot_is_offered_once_this_one_lets_it_go() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    hold(
        &ns,
        std::iter::once((1, false)).chain((2..=15).map(|n| (n, true))),
    );
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.options) };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Remembered,
        &ask,
        &noop,
    )
    .unwrap();
    let e = format!("{:#}", ns.start(Mode::Remembered).unwrap_err());
    assert!(
        e.contains("every slot of redis") && e.contains("w1 (slot 1, last ran"),
        "{e}"
    );

    let asked = std::cell::RefCell::new(None::<Question>);
    let ask = |q: &Question| -> Result<Answer> {
        asked.replace(Some(q.clone()));
        Ok(Answer::Choice(0))
    };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Remembered,
        &ask,
        &noop,
    )
    .unwrap();
    let question = asked.into_inner().expect("asked which one to free");
    let offered: Vec<&str> = question.options.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(offered, vec!["w1"]);
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "1\n"
    );
}

// Slots nobody holds but full of somebody else's keys are no free slot
// either: the stopped worktrees holding the rest are offered.
#[test]
fn slots_full_of_somebody_elses_keys_ask_which_stopped_worktree_frees_one() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=14).map(|n| (n, false)));
    std::fs::write(redis.join("slot-15"), "3\n").unwrap();
    let question = free_w2(&ns);
    assert_eq!(question.options.len(), 14);
    assert!(
        question
            .details
            .iter()
            .any(|d| d.starts_with("slot 15 holds keys no worktree of this project records")),
        "{:?}",
        question.details
    );
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "2\n"
    );
}

// With a slot nobody holds and empty, nothing is asked: the start takes
// it.
#[test]
fn a_free_slot_beside_stopped_holders_asks_nothing() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=14).map(|n| (n, false)));
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.slot) };
    resolve_for_start(
        &ns.fx.paths,
        &ns.fx.config,
        &ns.name,
        Mode::Namespaced,
        &ask,
        &noop,
    )
    .unwrap();
    assert!(!redis.join("flushed").exists());
}

#[test]
fn freeing_no_slot_starts_nothing_and_empties_nothing() {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    hold(&ns, (1..=15).map(|n| (n, false)));
    let ask = |q: &Question| -> Result<Answer> {
        assert_eq!(q.slot, crate::detect::Slot::FreeSlot);
        assert!(q.allow_none && !q.allow_custom);
        Ok(Answer::None)
    };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("no slot was freed"), "{e}");
    assert!(!redis.join("flushed").exists());
    assert_eq!(ns.fake("created"), "", "nothing was made either");
}

// The guard stands before the question and behind it: a holder whose
// record claims the main checkout's own slot is not offered, and one whose
// slot another record comes to name while the question is asked is never
// emptied, whatever was answered.
#[test]
fn freeing_a_slot_goes_through_the_guard() {
    let (ns, redis) = slots_fixture(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\nREDIS_PORT=6379\nREDIS_DB=5\n",
    );
    let mut store = ns.fx.state();
    for n in (1..=15).filter(|n| *n != 5) {
        store
            .worktrees
            .insert(format!("w{n}"), slot_holder(n, false, 1));
    }
    // w1's record claims slot 1 but names 1 as main: the guard refuses it.
    store.worktrees.get_mut("w1").unwrap().namespaces[0].main = "1".into();
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let ask = |q: &Question| -> Result<Answer> {
        assert!(
            q.options.iter().all(|(value, _)| value != "w1"),
            "{:?}",
            q.options
        );
        assert!(
            q.details
                .iter()
                .any(|d| d.starts_with("not offered: w1 (slot 1")
                    && d.contains("main checkout's own slot")),
            "{:?}",
            q.details
        );
        let mut store = ns.fx.state();
        store
            .worktrees
            .insert("w16".into(), slot_holder(2, false, 1));
        state::save(&ns.fx.paths.state_file(), &store).unwrap();
        let index = q
            .options
            .iter()
            .position(|(value, _)| value == "w2")
            .unwrap();
        Ok(Answer::Choice(index))
    };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(e.contains("is recorded for w16 as well"), "{e}");
    assert!(!redis.join("flushed").exists());
    let store = ns.fx.state();
    assert_eq!(store.worktrees["w1"].namespaces.len(), 1, "still recorded");
    assert_eq!(store.worktrees["w2"].namespaces.len(), 1, "still recorded");
}

// A stopped worktree the guard refuses for good — its record claims the
// main checkout's slot — is nothing a question could free. Nothing was
// asked, the start went on, and its error said a terminal start asks which
// one to free, so every terminal start after it did the same. It stops
// now, saying why that one cannot be freed, and no start says it asks.
#[test]
fn a_stopped_worktree_the_guard_always_refuses_stops_the_start_and_is_never_said_to_be_asked_about()
{
    let (ns, redis) = slots_fixture(
        "DATABASE_PORT=3306\nDATABASE_NAME=shop\nDATABASE_USER=app\nREDIS_PORT=6379\nREDIS_DB=5\n",
    );
    let mut store = ns.fx.state();
    for n in (1..=15).filter(|n| *n != 5) {
        store
            .worktrees
            .insert(format!("w{n}"), slot_holder(n, n != 1, 1));
    }
    store.worktrees.get_mut("w1").unwrap().namespaces[0].main = "1".into();
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let ask = |q: &Question| -> Result<Answer> { panic!("asked {:?}", q.options) };
    let e = format!(
        "{:#}",
        resolve_for_start(
            &ns.fx.paths,
            &ns.fx.config,
            &ns.name,
            Mode::Namespaced,
            &ask,
            &noop
        )
        .unwrap_err()
    );
    assert!(
        e.contains("w1 (slot 1, last ran")
            && e.contains("main checkout's own slot")
            && e.contains("w2 (slot 2, running)")
            && e.contains("Stop one of this project's running ones"),
        "{e}"
    );
    assert!(!e.contains("on a terminal asks"), "{e}");

    // A start that does not ask finds every slot held, and does not say
    // that one which does would.
    let e = format!("{:#}", ns.start(Mode::Namespaced).unwrap_err());
    assert!(e.contains("every slot of redis"), "{e}");
    assert!(!e.contains("on a terminal asks"), "{e}");
    assert!(!redis.join("flushed").exists());
}

// ---- rm and doctor, for namespaces --------------------------------------------

/// A namespaced worktree with a database and a slot, started and stopped.
fn stopped_namespaced() -> (Namespaced, PathBuf) {
    let (ns, redis) = slots_fixture(MAIN_ENV_WITH_REDIS);
    let (report, _) = ns.start(Mode::Namespaced).unwrap();
    drop(guard(&report));
    stop(&ns.fx.paths, &ns.name, None).unwrap();
    (ns, redis)
}

// `rm` takes the worktree's own database and slot with it — through the
// guard, on the server they were made on — and nothing else.
#[test]
fn rm_drops_the_worktrees_database_and_empties_its_slot_and_nothing_else() {
    let (ns, redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("dbs/shop"), "").unwrap();
    std::fs::write(ns.fake.join("dbs/shop__feat_two"), "").unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l == "mariadb: dropped database shop__feat_one"),
        "{said:?}"
    );
    assert!(
        said.iter().any(|l| l == "redis: emptied slot 1"),
        "{said:?}"
    );
    assert_eq!(ns.fake("dropped"), "shop__feat_one\n");
    assert_eq!(
        std::fs::read_to_string(redis.join("flushed")).unwrap(),
        "1\n"
    );
    assert!(ns.fake.join("dbs/shop").exists(), "main's is where it was");
    assert!(
        ns.fake.join("dbs/shop__feat_two").exists(),
        "and so is another worktree's"
    );
    assert!(!ns.fx.state().worktrees.contains_key(&ns.name));
}

// Whatever the record says, the guard stands between it and the server: a
// record naming the main database is never dropped, and `rm` says so.
#[test]
fn rm_leaves_what_the_guard_refuses_and_says_why() {
    let (ns, redis) = stopped_namespaced();
    let mut store = ns.fx.state();
    let record = store.worktrees.get_mut(&ns.name).unwrap();
    for namespace in record.namespaces.iter_mut() {
        namespace.name = namespace.main.clone();
    }
    state::save(&ns.fx.paths.state_file(), &store).unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    assert!(
        said.iter()
            .any(|l| l.contains("database shop is left as it is")
                && l.contains("main checkout's own database")),
        "{said:?}"
    );
    assert!(said.iter().any(|l| l.contains("slot 0")), "{said:?}");
    // Nothing finds a slot again once its record is gone, and its line
    // says so; a database a recipe can list, doctor finds by its name.
    let forgotten = |l: &String| l.ends_with("so pando will not mention it again");
    assert!(
        said.iter()
            .any(|l| l.contains("redis slot 0 is left") && forgotten(l)),
        "{said:?}"
    );
    assert!(
        !said
            .iter()
            .any(|l| l.contains("database shop is left") && forgotten(l)),
        "{said:?}"
    );
    assert_eq!(ns.fake("dropped"), "", "nothing was dropped");
    assert!(!redis.join("flushed").exists(), "nothing was emptied");
}

// A slot `rm` leaves because another record names it too is still that
// one's, and pando goes on showing it: its line does not say this is the
// last pando says of it.
#[test]
fn a_slot_rm_leaves_to_another_record_is_not_said_to_be_the_last_of_it() {
    let (ns, redis) = stopped_namespaced();
    hold(&ns, [(1, false)]);
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, "w1", false, false, &progress).unwrap();
    let said = said.borrow().clone();
    let line = said
        .iter()
        .find(|l| l.contains("redis slot 1 is left as it is"))
        .unwrap_or_else(|| panic!("{said:?}"));
    assert!(
        line.contains(&format!("recorded for {} as well", ns.name)),
        "{line}"
    );
    assert!(!line.contains("will not mention it again"), "{line}");
    assert!(!redis.join("flushed").exists());
}

// A database the server will not let the login drop is said, with the
// command that drops it by hand — and never with the password in it.
#[test]
fn a_namespace_rm_cannot_drop_is_said_with_the_command_that_drops_it() {
    let (ns, _redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("deny-drop"), "").unwrap();
    let (said, progress) = collecting();
    super::rm(&ns.fx.paths, &ns.name, false, false, &progress).unwrap();
    let said = said.borrow().clone();
    let line = said
        .iter()
        .find(|l| l.contains("could not be dropped"))
        .unwrap_or_else(|| panic!("{said:?}"));
    assert!(
        line.contains("DROP DATABASE IF EXISTS `shop__feat_one`")
            && line.contains("with the password in MYSQL_PWD"),
        "{line}"
    );
    assert!(said.iter().all(|l| !l.contains("s3cret-pw")), "{said:?}");
    assert!(ns.fake.join("dbs/shop__feat_one").exists());
}

// A removal git refuses has cost nothing: the worktree is there, and so is
// its database.
#[test]
fn a_refused_removal_keeps_the_worktrees_namespaces() {
    let (ns, redis) = stopped_namespaced();
    let wt = ns.fx.worktrees_dir().join(&ns.name);
    std::fs::write(wt.join("scratch.txt"), "work in progress\n").unwrap();
    assert!(super::rm(&ns.fx.paths, &ns.name, false, false, &noop).is_err());
    assert_eq!(ns.fake("dropped"), "");
    assert!(!redis.join("flushed").exists());
    assert_eq!(ns.fx.state().worktrees[&ns.name].namespaces.len(), 2);
}

// doctor lists a database named for a worktree of this project that no
// record holds, with the command that drops it — and never drops it.
#[test]
fn doctor_lists_a_leftover_database_with_the_command_that_drops_it() {
    let (ns, _redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("dbs/shop__feat_gone"), "").unwrap();
    std::fs::write(ns.fake.join("dbs/shop"), "").unwrap();
    let leftovers = namespace_leftovers(&ns.fx.paths, &ns.fx.config, &ns.fx.state());
    assert_eq!(
        leftovers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["shop__feat_gone"],
        "the worktree's own is held, and main's is not a namespace"
    );
    assert!(
        leftovers[0]
            .by_hand
            .as_deref()
            .is_some_and(|c| c.contains("DROP DATABASE IF EXISTS `shop__feat_gone`")),
        "{leftovers:?}"
    );
    assert_eq!(ns.fake("dropped"), "", "listed, never dropped");

    // And a project that runs no namespaced worktree is never looked at.
    let plain = namespaced_fixture(MAIN_ENV);
    std::fs::write(plain.fake.join("dbs/shop__feat_gone"), "").unwrap();
    assert!(namespace_leftovers(&plain.fx.paths, &plain.fx.config, &plain.fx.state()).is_empty());
    assert_eq!(plain.fake("argv"), "", "no server was asked anything");
}

// A second clone of the repository on the same server names its
// worktrees' databases the same way: one its record holds is its own, and
// never listed here as this project's leftover.
#[test]
fn doctor_never_lists_a_database_another_projects_record_holds() {
    let (ns, _redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("dbs/shop__feat_theirs"), "").unwrap();
    std::fs::write(ns.fake.join("dbs/shop__feat_gone"), "").unwrap();
    let mut theirs = WorktreeRecord::new("/abs/clone/feat+theirs", true);
    theirs.namespaces.push(crate::state::NamespaceRecord {
        service: "db".into(),
        recipe: "mariadb".into(),
        kind: crate::state::NamespaceKind::Database,
        host: "127.0.0.1".into(),
        port: 3306,
        name: "SHOP__FEAT_THEIRS".into(),
        main: "shop".into(),
        mains: Vec::new(),
        keys: Vec::new(),
        used_at: Utc::now(),
    });
    hold_elsewhere(&ns, theirs);
    let leftovers = namespace_leftovers(&ns.fx.paths, &ns.fx.config, &ns.fx.state());
    assert_eq!(
        leftovers
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["shop__feat_gone"]
    );
}

// A project whose state does not load may hold any of them, as it may any
// slot. Its databases were counted as nobody's, and doctor said no pando
// project held one, with the command that drops that project's live
// worktree's database. It is said with that project now, and no command.
#[test]
fn doctor_offers_no_drop_while_another_projects_state_cannot_be_read() {
    let (ns, _redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("dbs/shop__feat_theirs"), "").unwrap();
    let file = ns
        .fx
        .paths
        .projects_dir()
        .join("other-1a2b3c4d")
        .join("state.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "{\"version\": 99, \"worktrees\": {}}").unwrap();
    let leftovers = namespace_leftovers(&ns.fx.paths, &ns.fx.config, &ns.fx.state());
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert_eq!(leftovers[0].name, "shop__feat_theirs");
    assert_eq!(leftovers[0].by_hand, None);
    assert!(
        leftovers[0]
            .unread
            .as_ref()
            .is_some_and(|(project, why)| project == "other-1a2b3c4d" && why.contains("99")),
        "{leftovers:?}"
    );

    // doctor reads config from disk, as it always does.
    std::fs::write(
        ns.fx.paths.config_file(),
        "[dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n",
    )
    .unwrap();
    let report = crate::doctor::run(&ns.fx.paths);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("shop__feat_theirs"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert!(
        finding
            .message
            .contains("project other-1a2b3c4d's state could not be read")
            && !finding
                .message
                .contains("no pando project's record holds it"),
        "{}",
        finding.message
    );
    assert_eq!(finding.fix, None);
    assert_eq!(ns.fake("dropped"), "");
}

// Anyone who may make a database under the prefix can put a statement in
// its name. It is still listed, with no command: the one printed for it
// dropped the main database as well.
#[test]
fn doctor_prints_no_drop_for_a_leftover_whose_name_is_not_plain() {
    let (ns, _redis) = stopped_namespaced();
    std::fs::write(ns.fake.join("dbs/shop__a`; DROP DATABASE `shop"), "").unwrap();
    let leftovers = namespace_leftovers(&ns.fx.paths, &ns.fx.config, &ns.fx.state());
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert_eq!(leftovers[0].name, "shop__a`; DROP DATABASE `shop");
    assert_eq!(leftovers[0].by_hand, None);
}

#[test]
fn doctor_reports_a_leftover_database_as_a_note_with_its_fix() {
    let (ns, _redis) = stopped_namespaced();
    // doctor reads config from disk, as it always does.
    std::fs::write(
        ns.fx.paths.config_file(),
        "[dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"mariadb\"\nenv = { DATABASE_PORT = \"mariadb\" }\n",
    )
    .unwrap();
    std::fs::write(ns.fake.join("dbs/shop__feat_gone"), "").unwrap();
    let report = crate::doctor::run(&ns.fx.paths);
    let finding = report
        .findings
        .iter()
        .find(|f| f.message.contains("shop__feat_gone"))
        .unwrap_or_else(|| panic!("{:?}", report.findings));
    assert_eq!(finding.severity, crate::doctor::Severity::Note);
    assert!(
        finding
            .message
            .contains("no pando project's record holds it")
            && finding
                .message
                .contains("or one that is not pando's at all"),
        "{}",
        finding.message
    );
    assert!(
        finding
            .fix
            .as_deref()
            .is_some_and(|fix| fix.contains("DROP DATABASE") && fix.contains("never drops")),
        "{:?}",
        finding.fix
    );
    assert_eq!(ns.fake("dropped"), "");
}
