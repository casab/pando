//! `start --isolated` end to end, against the fake docker.
//!
//! The shape of the thing: the services get roles and ports beside the
//! processes, the app is told where they are through its own env keys, the
//! mode is remembered, `stop` takes both halves down, and `rm` takes the
//! volumes with it.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    Kind, build, build_fresh_clone, docker, listener_on_port_env, listener_printing, paths_for,
};
use pando::compose;
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::state::{self, ServiceKind};
use pando::{actions, process, services};
use tempfile::TempDir;

struct Iso {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    paths: PandoPaths,
    config: Config,
}

impl Iso {
    fn project(&self, name: &str) -> String {
        compose::project_name(self.paths.project_id(), name)
    }

    fn record(&self, name: &str) -> state::WorktreeRecord {
        let store = state::load(&self.paths.state_file()).unwrap();
        store.worktrees[name].clone()
    }

    fn service(&self, name: &str, service: &str) -> state::ServiceRecord {
        self.record(name)
            .services
            .into_iter()
            .find(|s| s.name == service)
            .unwrap_or_else(|| panic!("no record for the service {service}"))
    }
}

/// Nothing this test started outlives it, containers included.
impl Drop for Iso {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
        if let Ok(store) = state::load(&self.paths.state_file()) {
            for name in store.worktrees.keys() {
                let _ = actions::rm(&self.paths, name, true, true, &|_| {});
            }
        }
    }
}

/// The `[[services]]` block the fixture's compose file supports, plus a
/// dev process that prints one of the rewritten variables before it binds
/// its port, so what the process was told is visible in its own log.
fn config_toml(dev: &str) -> String {
    format!(
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [dev]\ncmd = '''{dev}'''\nports = {{ PORT = \"web\" }}\n\n\
         [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\", \"redis\"]\n\
         env = {{ DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }}\n"
    )
}

fn iso_with(config_text: &str) -> Iso {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    docker::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(paths.config_file(), config_text).unwrap();
    let config = config::load(&paths).unwrap().config;
    Iso {
        _dir: dir,
        root,
        home,
        paths,
        config,
    }
}

fn iso() -> Iso {
    iso_with(&config_toml(&listener_printing("DATABASE_URL")))
}

fn new_worktree(f: &Iso, branch: &str) -> String {
    actions::new(&f.paths, &f.config, branch, None, &|_| {}).unwrap()
}

fn start_isolated(f: &Iso, name: &str) -> actions::StartReport {
    actions::start(
        &f.paths,
        &f.config,
        name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .unwrap()
}

fn log_of(f: &Iso, name: &str, source: &str) -> String {
    let path = f.paths.log_file(name, source);
    for _ in 0..100 {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.trim().is_empty() {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::read_to_string(&path).unwrap_or_default()
}

fn skip_without_python() -> bool {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return true;
    }
    false
}

#[test]
fn services_get_roles_and_ports_beside_the_processes() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let report = start_isolated(&f, &name);

    let mut roles: Vec<&str> = report.ports.keys().map(String::as_str).collect();
    roles.sort();
    assert_eq!(roles, vec!["postgres", "redis", "web"]);
    // One window, consecutive, with the processes first — so turning
    // isolation on never moves the port the developer bookmarked.
    let web = report.ports["web"];
    assert_eq!(report.ports["postgres"], web + 1);
    assert_eq!(report.ports["redis"], web + 2);

    let record = f.record(&name);
    assert!(record.isolated, "the mode is remembered on the record");
    assert_eq!(
        record
            .services
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.port))
            .collect::<Vec<_>>(),
        vec![
            ("postgres", ServiceKind::Compose, Some(web + 1)),
            ("redis", ServiceKind::Compose, Some(web + 2)),
        ]
    );
    for service in &record.services {
        assert_eq!(
            service.compose_project.as_deref(),
            Some(f.project(&name).as_str())
        );
        assert!(service.pid.is_some(), "each service has a log pump");
    }
    assert_eq!(
        docker::services_up(&f.home, &f.project(&name)),
        vec![
            ("postgres".to_string(), web + 1),
            ("redis".to_string(), web + 2)
        ]
    );
}

#[test]
fn the_processes_are_told_where_their_own_services_are() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let report = start_isolated(&f, &name);
    let text = log_of(&f, &name, "dev");
    assert!(
        text.contains(&format!(
            "DATABASE_URL=postgres://acme:acme@localhost:{}/acme",
            report.ports["postgres"]
        )),
        "the dev process must see the rewritten URL, not the shared one: {text}"
    );
}

#[test]
fn a_service_log_tab_appears_for_each_private_copy() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    for service in ["postgres", "redis"] {
        let text = log_of(&f, &name, service);
        assert!(
            text.contains(&format!("fake docker log for {service}")),
            "{service}: {text}"
        );
    }
}

#[test]
fn a_later_plain_start_keeps_the_services_and_the_mode() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let first = start_isolated(&f, &name);
    let up = docker::services_up(&f.home, &f.project(&name));

    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    // A plain start, with no flag at all: the worktree remembers.
    let second = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert_eq!(second.ports, first.ports, "the ports do not move");
    assert!(f.record(&name).isolated);
    assert_eq!(
        docker::services_up(&f.home, &f.project(&name)),
        up,
        "the same private services, on the same ports"
    );
    assert!(
        !docker::was_downed(&f.home, &f.project(&name)),
        "a stop never takes the volumes"
    );
}

// A worktree's own containers hold its own service ports. A second start
// that reads them as "taken" re-derives the whole window — web included —
// and points a live application at ports nothing is on.
#[test]
fn a_second_isolated_start_of_a_running_worktree_keeps_every_port() {
    if skip_without_python() {
        return;
    }
    let f = iso_with(&config_toml(&listener_printing("DATABASE_URL")).replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }\nready_timeout_s = 3",
    ));
    let name = new_worktree(&f, "feat/one");
    let first = start_isolated(&f, &name);
    let up = docker::services_up(&f.home, &f.project(&name));
    let pids = docker::service_pids(&f.home, &f.project(&name));

    // Nothing has stopped, so nothing may move: not the web port the
    // developer bookmarked, and not the database the app is connected to.
    let second = start_isolated(&f, &name);
    assert_eq!(
        second.ports, first.ports,
        "every port the worktree owns, services included"
    );
    assert!(
        !second.reassigned,
        "the ports were held by this worktree, not taken by anybody"
    );
    assert_eq!(
        docker::services_up(&f.home, &f.project(&name)),
        up,
        "the same services on the same ports"
    );
    assert_eq!(
        docker::service_pids(&f.home, &f.project(&name)),
        pids,
        "and the containers that were already up were left alone"
    );
    // The map the app was given still agrees with the ports that exist.
    let env = actions::resolved_env(&f.paths, &f.config, &name).unwrap();
    assert_eq!(
        env["DATABASE_URL"],
        format!(
            "postgres://acme:acme@localhost:{}/acme",
            first.ports["postgres"]
        )
    );
}

#[test]
fn stop_takes_the_processes_and_the_services_down_together() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let pump = f.service(&name, "postgres").pid.unwrap();

    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    let record = f.record(&name);
    assert!(record.processes.is_empty(), "the dev process is gone");
    assert!(
        docker::services_up(&f.home, &f.project(&name)).is_empty(),
        "and so are the containers"
    );
    assert!(!process::is_alive(pump), "and the log pump with them");
    assert_eq!(
        record.services.len(),
        2,
        "the records survive: only they know which compose project to take down"
    );
    assert!(record.services.iter().all(|s| s.pid.is_none()));
    assert!(record.isolated, "and the worktree is still an isolated one");
}

// `--only dev` and then `--only dev` again — keep the database, restart
// the app — is exactly the workflow `--only` exists for. Once the process
// records were empty the name was ignored entirely, so the second call
// took the database down and reported success; a typo did the same.
#[test]
fn stop_only_a_process_that_is_not_running_leaves_the_services_alone() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let up = docker::services_up(&f.home, &f.project(&name));
    assert_eq!(up.len(), 2);

    // Correct today: one process down, the services left serving.
    actions::stop(&f.paths, &name, Some("dev"), &|_| {}).unwrap();
    assert_eq!(docker::services_up(&f.home, &f.project(&name)), up);

    // And a name the worktree is not running is an error, not a silent
    // whole-worktree stop.
    let err = format!(
        "{:#}",
        actions::stop(&f.paths, &name, Some("nosuchprocess"), &|_| {}).unwrap_err()
    );
    assert!(err.contains("nosuchprocess"), "{err}");
    assert_eq!(
        docker::services_up(&f.home, &f.project(&name)),
        up,
        "a typo must not take the database down"
    );

    // The whole-worktree form still does take them down.
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    assert!(docker::services_up(&f.home, &f.project(&name)).is_empty());
}

// `up -d <name>` enables a service's profile implicitly; `stop` with the
// same files does not, so the with-files form leaves a profiled container
// running and holding its port. The by-project form finds every container
// by label, which is what `stop` and `rm` already use.
#[test]
fn the_readiness_failure_cleanup_stops_the_project_rather_than_the_files() {
    if skip_without_python() {
        return;
    }
    let f = iso_with(&config_toml("sleep 30").replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }\nready_timeout_s = 1",
    ));
    let name = new_worktree(&f, "feat/one");
    let project = f.project(&name);
    docker::never_ready(&f.home, &project);

    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .unwrap_err();
    let seen = docker::invocations_for(&f.home, &project);
    assert!(
        seen.iter()
            .any(|line| line == &format!("compose -p {project} stop")),
        "the by-project form, so a service a profile started is stopped too: {seen:?}"
    );
}

#[test]
fn rm_takes_the_compose_project_down_with_its_volumes() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let project = f.project(&name);

    actions::rm(&f.paths, &name, false, true, &|_| {}).unwrap();
    assert!(
        docker::was_downed(&f.home, &project),
        "rm is the one that wipes the data"
    );
    let seen = docker::invocations_for(&f.home, &project);
    assert!(
        seen.iter().any(|line| line.ends_with("down -v")),
        "with the volumes, not without: {seen:?}"
    );
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(!store.worktrees.contains_key(&name));
}

#[test]
fn the_orphan_sweep_covers_a_log_pump_whose_leader_died() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let pump = f.service(&name, "postgres");
    let pgid = pump.pgid.unwrap();

    // Kill the leader only, the way a `bash -lc` exits while its child
    // lives on. The sweep has to find the group from the record.
    process::stop(pgid, Duration::from_secs(2)).unwrap();
    for _ in 0..40 {
        if !process::is_alive(pump.pid.unwrap()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Any mutation sweeps; `refresh` is the read path that notices.
    actions::refresh(&f.paths);
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    let after = f.service(&name, "postgres");
    assert_eq!(after.pid, None, "the dead pump is forgotten");
    assert_eq!(after.pgid, None);
    assert_eq!(
        after.compose_project.as_deref(),
        Some(f.project(&name).as_str()),
        "but the service is still there to take down"
    );
}

#[test]
fn two_worktrees_get_different_ports_and_different_compose_projects() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let one = new_worktree(&f, "feat/one");
    let two = new_worktree(&f, "feat/two");
    let a = start_isolated(&f, &one);
    let b = start_isolated(&f, &two);

    assert_ne!(a.ports["postgres"], b.ports["postgres"]);
    assert_ne!(a.ports["redis"], b.ports["redis"]);
    assert_ne!(f.project(&one), f.project(&two));
    assert_eq!(
        docker::services_up(&f.home, &f.project(&one)),
        vec![
            ("postgres".to_string(), a.ports["postgres"]),
            ("redis".to_string(), a.ports["redis"])
        ]
    );
    assert_eq!(
        docker::services_up(&f.home, &f.project(&two)),
        vec![
            ("postgres".to_string(), b.ports["postgres"]),
            ("redis".to_string(), b.ports["redis"])
        ]
    );
}

#[test]
fn a_service_that_never_becomes_ready_fails_the_start_and_stops_what_came_up() {
    if skip_without_python() {
        return;
    }
    let f = iso_with(&config_toml(&listener_printing("DATABASE_URL")).replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }\nready_timeout_s = 1",
    ));
    let name = new_worktree(&f, "feat/one");
    docker::never_ready(&f.home, &f.project(&name));

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("did not become ready"), "{err}");
    assert!(err.contains("postgres") || err.contains("redis"), "{err}");
    let seen = docker::invocations_for(&f.home, &f.project(&name));
    assert!(
        seen.iter().any(|line| line.ends_with(" stop")),
        "what did come up is stopped again: {seen:?}"
    );
    // And no process was started behind a database that is not there.
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(store.worktrees[&name].processes.is_empty());
}

// Docker completes the handshake on a published port as soon as the
// container is running, whatever is listening inside it. Readiness by
// connect alone therefore says "the container exists", and the migration
// hook behind it runs against a database that is not accepting anything.
#[test]
fn a_published_port_with_nothing_behind_it_is_never_ready() {
    if skip_without_python() {
        return;
    }
    let f = iso_with(&config_toml(&listener_printing("DATABASE_URL")).replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }\nready_timeout_s = 2",
    ));
    let name = new_worktree(&f, "feat/one");
    docker::proxy_only(&f.home, &f.project(&name));

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("did not become ready"), "{err}");
    assert!(err.contains("postgres") || err.contains("redis"), "{err}");
    // The connect itself succeeded throughout — that a connect cannot tell
    // this from a database is what `ports::something_is_serving` and its
    // unit tests pin down; here the point is that the start failed rather
    // than reporting success and spawning behind it.
    let store = state::load(&f.paths.state_file()).unwrap();
    let seen = docker::invocations_for(&f.home, &f.project(&name));
    assert!(
        seen.iter().any(|line| line.ends_with(" stop")),
        "what did come up is stopped again: {seen:?}"
    );
    assert!(
        store.worktrees[&name].processes.is_empty(),
        "and nothing was spawned behind it"
    );
}

// Isolation is remembered, and only `rm` forgets it. Writing it down
// before anything was brought up therefore turned a start that created
// nothing into a worktree that could not be started at all — not even
// shared — until `pando.toml` was edited by hand.
#[test]
fn a_start_that_fails_before_any_container_leaves_the_worktree_startable() {
    if skip_without_python() {
        return;
    }
    let f = iso_with(&config_toml(&listener_on_port_env()).replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { DB_HOST = \"postgres\" }",
    ));
    // A mapping `rewrite` can never satisfy: a bare host name has nowhere
    // to put a port number.
    let example = f.root.join(".env.example");
    let text = std::fs::read_to_string(&example).unwrap();
    std::fs::write(&example, format!("{text}DB_HOST=postgres\n")).unwrap();
    common::git(&f.root, &["add", "."]);
    common::git(&f.root, &["commit", "--quiet", "-m", "a host key"]);
    let name = new_worktree(&f, "feat/h");

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("DB_HOST"), "{err}");
    assert!(
        !f.record(&name).isolated,
        "nothing was brought up, so nothing is remembered"
    );
    assert!(
        docker::invocations(&f.home).is_empty(),
        "docker was never even asked"
    );

    // And a plain start still works, which it could not if the worktree
    // were stuck in a mode whose first step always fails.
    let report = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert!(report.ports.contains_key("web"));
    assert!(!f.record(&name).isolated);
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
}

// The binary is there and the daemon is not. The friendly sentence was
// the context on a *spawn* failure only, so what the developer got was the
// whole `docker compose -p … -f … -f … up -d postgres redis` line.
#[test]
fn a_docker_whose_daemon_is_down_gets_the_hint_rather_than_the_command_line() {
    let f = iso_with(&config_toml("sleep 30"));
    let name = new_worktree(&f, "feat/one");
    docker::daemon_down(&f.home);

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("Docker daemon is not running"), "{err}");
    assert!(err.contains("without --isolated"), "{err}");
    assert!(
        !err.contains("compose -p"),
        "not the command line, which is not something to act on: {err}"
    );
}

// A record for a service config no longer includes is kept, because only
// it knows which compose project to take down. Keeping its *port* as well
// made `status` show two services on one number, and only one of them was
// telling the truth.
#[test]
fn a_service_config_no_longer_includes_keeps_its_name_but_not_its_port() {
    if skip_without_python() {
        return;
    }
    let mut f = iso_with(&config_toml(&listener_on_port_env()));
    let name = new_worktree(&f, "feat/one");
    let first = start_isolated(&f, &name);
    assert_eq!(f.record(&name).services.len(), 2);
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();

    // The include list changed under the worktree: only redis now, so the
    // window is two ports and redis lands on the number postgres had.
    std::fs::write(
        f.paths.config_file(),
        config_toml(&listener_on_port_env())
            .replace(
                "include = [\"postgres\", \"redis\"]",
                "include = [\"redis\"]",
            )
            .replace(
                "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
                "env = { REDIS_URL = \"redis\" }",
            ),
    )
    .unwrap();
    f.config = config::load(&f.paths).unwrap().config;

    let second = start_isolated(&f, &name);
    assert_eq!(
        second.ports["redis"], first.ports["postgres"],
        "the window moved up, which is what makes the stale record a lie"
    );
    let record = f.record(&name);
    let stale = record
        .services
        .iter()
        .find(|s| s.name == "postgres")
        .expect("the record survives, so rm can still name the compose project");
    assert_eq!(stale.port, None, "but it claims no port it does not have");
    assert!(stale.compose_project.is_some());

    let statuses = actions::service_statuses(&record);
    let mut ports: Vec<u16> = statuses.iter().filter_map(|s| s.port).collect();
    let before = ports.len();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(
        ports.len(),
        before,
        "no two services on one port: {statuses:?}"
    );
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
}

#[test]
fn an_env_key_no_file_in_the_worktree_sets_is_an_error_naming_it() {
    let f = iso_with(&config_toml("sleep 30").replace(
        "env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }",
        "env = { NOWHERE_URL = \"postgres\", REDIS_URL = \"redis\" }",
    ));
    let name = new_worktree(&f, "feat/one");
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("NOWHERE_URL"), "{err}");
    assert!(err.contains(".env.example"), "{err}");
}

// A fresh clone has no `.env`, so the worktree's one is a copy of the
// project's example. From then on it is the worktree's own ignored file,
// and the port rewriting reads it exactly as it reads a linked one: an edit
// made inside the worktree is what reaches the app, with this worktree's
// service ports in it.
#[test]
fn the_ports_of_a_seeded_env_file_are_rewritten_like_any_other() {
    let dir = TempDir::new().unwrap();
    let root = build_fresh_clone(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        "[project]\nprovision = [\".env\"]\n\
         provision_from = { \".env\" = \".env.example\" }\n",
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    let name = actions::new(&paths, &config, "feat/one", None, &|_| {}).unwrap();
    let worktree = config.worktrees_dir(&paths).join(&name);
    let seeded = worktree.join(".env");
    assert!(seeded.is_file(), "the example seeded the worktree's .env");

    let mapping = BTreeMap::from([
        ("DATABASE_URL".to_string(), "postgres".to_string()),
        ("REDIS_URL".to_string(), "redis".to_string()),
    ]);
    let ports = BTreeMap::from([
        ("postgres".to_string(), 15432u16),
        ("redis".to_string(), 16379),
    ]);

    let env = services::app_env(&worktree, &mapping, &ports).unwrap();
    assert_eq!(
        env["DATABASE_URL"],
        "postgres://acme:acme@localhost:15432/acme"
    );
    assert_eq!(env["REDIS_URL"], "redis://localhost:16379");

    // And it is the worktree's file that is read, not the example it came
    // from: a developer editing their own ignored file is the point of
    // having one.
    std::fs::write(
        &seeded,
        "DATABASE_URL=postgres://acme:acme@localhost:5432/just_this_worktree\n\
         REDIS_URL=redis://localhost:6379\n",
    )
    .unwrap();
    let env = services::app_env(&worktree, &mapping, &ports).unwrap();
    assert_eq!(
        env["DATABASE_URL"],
        "postgres://acme:acme@localhost:15432/just_this_worktree"
    );
    assert_eq!(
        common::status_porcelain(&worktree),
        "",
        "and none of it shows in the worktree: the file is gitignored"
    );
}

#[test]
fn a_project_with_no_services_runs_shared_and_says_so() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::GoService, dir.path()).root;
    let home = dir.path().join("pando-home");
    docker::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        "[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = { PORT = \"web\" }\n",
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    let name = actions::new(&paths, &config, "feat/one", None, &|_| {}).unwrap();

    let said = std::sync::Mutex::new(Vec::<String>::new());
    let report = {
        let notice = |m: &str| said.lock().unwrap().push(m.to_string());
        actions::start(
            &paths,
            &config,
            &name,
            None,
            actions::Mode::Isolated,
            &notice,
        )
        .unwrap()
    };
    let said = said.into_inner().unwrap();
    assert!(
        said.iter().any(|m| m.contains("shared mode")),
        "a flag about services on a project with none is a notice, not a failure: {said:?}"
    );
    assert_eq!(
        report.ports.keys().collect::<Vec<_>>(),
        vec!["web"],
        "and nothing extra is reserved"
    );
    assert!(!state::load(&paths.state_file()).unwrap().worktrees[&name].isolated);
    assert!(
        docker::invocations(&home).is_empty(),
        "docker is never even asked"
    );
    let _ = actions::stop_all(&paths, &|_| {});
}

// The override pando generates is what compose is handed, so it is worth
// asserting as a file rather than only through the fake's behaviour.
#[test]
fn the_override_lands_under_pandos_home_and_says_what_it_should() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let report = start_isolated(&f, &name);
    let path = f.paths.compose_override_file(&name);
    assert!(
        path.starts_with(&f.home),
        "the project's compose file is never edited: {}",
        path.display()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains(&format!(
            "ports: !override [\"127.0.0.1:{}:5432\"]",
            report.ports["postgres"]
        )),
        "{text}"
    );
    assert!(text.contains("container_name: !reset"), "{text}");
    assert_eq!(
        common::status_porcelain(Path::new(&f.root)),
        "",
        "and nothing of it landed in the repository"
    );
}

#[test]
fn a_bind_mount_inside_the_repository_is_refused_by_name() {
    let f = iso_with(&config_toml("sleep 30"));
    // The compose file is the project's own, so this is the shape a real
    // project arrives in.
    std::fs::write(
        f.root.join("docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n    \
         volumes:\n      - ./pgdata:/var/lib/postgresql/data\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    common::git(&f.root, &["add", "."]);
    common::git(&f.root, &["commit", "--quiet", "-m", "bind mount"]);
    let name = new_worktree(&f, "feat/one");

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("postgres"), "{err}");
    assert!(err.contains("./pgdata"), "{err}");
    assert!(
        docker::invocations(&f.home).is_empty(),
        "refused before docker was asked to do anything"
    );
}

// A compose file with a service called `web` is the common case, and a
// role is one port: two things claiming it would be handed one number and
// the second would die on `EADDRINUSE` for a reason nothing could explain.
#[test]
fn a_service_name_that_collides_with_a_process_role_is_refused_at_load() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let paths = paths_for(&dir.path().join("pando-home"), &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        config_toml("sleep 30").replace("PORT = \"web\"", "PORT = \"postgres\""),
    )
    .unwrap();

    let err = format!("{:#}", config::load(&paths).unwrap_err());
    assert!(err.contains("\"postgres\""), "{err}");
    assert!(err.contains("role"), "{err}");
    let _: BTreeMap<String, String> = BTreeMap::new();
}

// ---- the way back ---------------------------------------------------------

// `--shared` is how a worktree stops running its own copies of everything
// and goes back to the project's. The containers stop; their volumes stay,
// because a mode switch is not a decision to throw data away.
#[test]
fn start_shared_stops_the_private_services_and_clears_the_mode() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let isolated = start_isolated(&f, &name);
    assert!(f.record(&name).isolated);
    assert!(!docker::services_up(&f.home, &f.project(&name)).is_empty());
    let pump = f.service(&name, "postgres");
    let pump_pid = pump.pid.expect("a log pump");

    let shared = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Shared,
        &|_| {},
    )
    .unwrap();

    let record = f.record(&name);
    assert!(!record.isolated, "the mode is cleared");
    assert!(
        docker::services_up(&f.home, &f.project(&name)).is_empty(),
        "the private services are stopped"
    );
    assert!(
        !docker::was_downed(&f.home, &f.project(&name)),
        "but not taken down: a mode switch is not `rm`, and the volumes stay"
    );
    assert!(
        common::wait_until(Duration::from_secs(5), || !process::is_alive(pump_pid)),
        "the log pump in front of a stopped service is stopped with it"
    );
    for service in &record.services {
        assert_eq!(service.pid, None, "{}: pump forgotten", service.name);
        assert_eq!(
            service.port, None,
            "{}: the port belongs to a window this start re-derived",
            service.name
        );
        assert!(
            service.compose_project.is_some(),
            "{}: but rm can still find the container and its volumes",
            service.name
        );
    }
    assert_eq!(
        shared.ports["web"], isolated.ports["web"],
        "the application keeps the port it was bookmarked on"
    );
    // And it really is running again, in shared mode: the processes are
    // replaced rather than left pointed at a database that has gone.
    assert_eq!(shared.started.len(), 1, "{:?}", shared.started);
    assert!(
        shared.already_running.is_empty(),
        "a process still pointed at the old database is not in shared mode"
    );
    assert!(
        !f.record(&name).ports.contains_key("postgres"),
        "and the service roles are out of the worktree's window"
    );
}

// And a plain start afterwards stays shared: the mode is remembered in
// both directions.
#[test]
fn a_plain_start_after_shared_stays_shared() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Shared,
        &|_| {},
    )
    .unwrap();
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();

    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert!(!f.record(&name).isolated, "a plain start does not undo it");
    assert!(
        docker::services_up(&f.home, &f.project(&name)).is_empty(),
        "and nothing brought the private services back up"
    );
}

// ---- a log pump that died -------------------------------------------------

/// Kills a service's log pump and waits for it to go, returning its pid.
fn kill_the_pump(f: &Iso, name: &str, service: &str) -> u32 {
    let pump = f.service(name, service);
    let pid = pump.pid.expect("a log pump");
    process::stop(pump.pgid.expect("a group"), Duration::from_secs(2)).unwrap();
    assert!(
        common::wait_until(Duration::from_secs(5), || !process::is_alive(pid)),
        "the pump never died"
    );
    pid
}

fn pump_is_running(f: &Iso, name: &str, service: &str) -> bool {
    actions::service_statuses(&f.record(name))
        .into_iter()
        .find(|status| status.name == service)
        .expect("a service status")
        .logging
}

// A read path reports a dead pump and leaves it dead: nothing that merely
// looks at state is allowed to spawn a process.
#[test]
fn a_dead_pump_is_reported_by_a_read_path_and_never_respawned_by_one() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    assert!(pump_is_running(&f, &name, "postgres"));

    let dead = kill_the_pump(&f, &name, "postgres");
    assert!(!pump_is_running(&f, &name, "postgres"), "status says so");

    actions::refresh(&f.paths);
    actions::service_statuses(&f.record(&name));
    actions::ls(&f.paths).unwrap();
    assert!(
        !pump_is_running(&f, &name, "postgres"),
        "and every read of it leaves it exactly as dead"
    );
    assert_eq!(
        f.service(&name, "postgres").pid,
        Some(dead),
        "no read path started a new one"
    );
    assert!(!process::is_alive(dead));
}

// `start` is one of the two that put it back, and it does so without
// disturbing a process that is already up.
#[test]
fn a_dead_pump_comes_back_on_the_next_start() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    let first = start_isolated(&f, &name);
    let dev = first.started[0].record.pid;
    let dead = kill_the_pump(&f, &name, "postgres");

    let report = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();

    assert!(pump_is_running(&f, &name, "postgres"), "the pump is back");
    assert_ne!(
        f.service(&name, "postgres").pid,
        Some(dead),
        "and it is a new one"
    );
    assert!(
        report.started.is_empty() && report.already_running.len() == 1,
        "the process that was up was left exactly as it was: {report:?}"
    );
    assert!(process::is_alive(dev), "including its pid");
}

// And `restart` is the other one.
#[test]
fn a_dead_pump_comes_back_on_restart() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let dead = kill_the_pump(&f, &name, "postgres");

    actions::restart(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();

    assert!(pump_is_running(&f, &name, "postgres"));
    assert_ne!(f.service(&name, "postgres").pid, Some(dead));
}

// ---- `--only` through a mode change ---------------------------------------

/// Two processes, so "the one you named and the one you did not" is a
/// real distinction, plus compose services to switch between.
fn two_processes_with_services() -> String {
    format!(
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [processes.web]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
         [processes.api]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\nenv = {{ DATABASE_URL = \"postgres\" }}\n",
        listener_printing("DATABASE_URL")
    )
}

// A mode change replaces the address every process was given. `--only web`
// through it would restart `web` against the new database and leave `api`
// on the old one — two halves of one application on two databases, with
// nothing saying so.
#[test]
fn only_is_refused_when_the_start_would_change_which_services_are_used() {
    let f = iso_with(&two_processes_with_services());
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before: Vec<String> = f.record(&name).processes.keys().cloned().collect();
    assert_eq!(before, vec!["api".to_string(), "web".to_string()]);

    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            Some("web"),
            actions::Mode::Shared,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("--only web"), "{e}");
    assert!(e.contains("the project's shared"), "{e}");
    assert!(e.contains("without `--only`"), "{e}");

    // Refused, so nothing moved: both processes are the ones that were
    // running, and the worktree is still isolated.
    let after = f.record(&name);
    assert!(after.isolated, "the mode changed under a refusal");
    assert_eq!(
        after.processes.keys().cloned().collect::<Vec<_>>(),
        before,
        "a process was replaced by a start that refused"
    );

    // The same refusal the other way round, on a worktree that is
    // running against the project's shared services.
    let two = new_worktree(&f, "feat/two");
    actions::start(
        &f.paths,
        &f.config,
        &two,
        None,
        actions::Mode::Shared,
        &|_| {},
    )
    .unwrap();
    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &two,
            Some("web"),
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("its own"), "{e}");
    assert!(!f.record(&two).isolated, "the mode changed under a refusal");

    // And asking for the mode a worktree is already in is no change at
    // all, so `--only` is fine: the refusal is about the switch, not
    // about the flag.
    actions::start(
        &f.paths,
        &f.config,
        &name,
        Some("web"),
        actions::Mode::Isolated,
        &|_| {},
    )
    .expect("an isolated worktree asked for isolated again");
}

// `restart` stops before it starts, so the refusal has to come first — a
// refusal that has already taken the process down is not a refusal.
#[test]
fn restart_only_across_a_mode_change_refuses_before_it_stops_anything() {
    let f = iso_with(&two_processes_with_services());
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.record(&name);
    let web = before.processes["web"].pid;

    let e = format!(
        "{:#}",
        actions::restart(
            &f.paths,
            &f.config,
            &name,
            Some("web"),
            actions::Mode::Shared,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("--only web"), "{e}");
    assert!(
        process::is_alive(web),
        "the process was stopped by a command that then refused"
    );
    assert!(f.record(&name).isolated);
}

// `--only` is fine when the mode is not changing, which is the whole
// point of having it: restarting one process of several is the common
// case and must keep working.
#[test]
fn only_still_works_when_the_mode_stays_as_it_is() {
    let f = iso_with(&two_processes_with_services());
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let api = f.record(&name).processes["api"].pid;

    actions::restart(
        &f.paths,
        &f.config,
        &name,
        Some("web"),
        actions::Mode::Remembered,
        &|_| {},
    )
    .expect("a restart that changes no mode");
    let after = f.record(&name);
    assert_eq!(after.processes["api"].pid, api, "the sibling was replaced");
    assert!(after.isolated);
}
