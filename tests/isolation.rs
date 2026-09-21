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

use common::{Kind, build, docker, listener_on_port_env, listener_printing, paths_for};
use pando::compose;
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::state::{self, ServiceKind};
use pando::{actions, process};
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
        let _ = actions::stop_all(&self.paths);
        if let Ok(store) = state::load(&self.paths.state_file()) {
            for name in store.worktrees.keys() {
                let _ = actions::rm(&self.paths, name, true, true);
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
    actions::start(&f.paths, &f.config, name, None, true, &|_| {}).unwrap()
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

    actions::stop(&f.paths, &name, None).unwrap();
    // A plain start, with no flag at all: the worktree remembers.
    let second = actions::start(&f.paths, &f.config, &name, None, false, &|_| {}).unwrap();
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

    actions::stop(&f.paths, &name, None).unwrap();
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

#[test]
fn rm_takes_the_compose_project_down_with_its_volumes() {
    if skip_without_python() {
        return;
    }
    let f = iso();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let project = f.project(&name);

    actions::rm(&f.paths, &name, false, true).unwrap();
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
    actions::stop(&f.paths, &name, None).unwrap();
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
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
    let report = actions::start(&f.paths, &f.config, &name, None, false, &|_| {}).unwrap();
    assert!(report.ports.contains_key("web"));
    assert!(!f.record(&name).isolated);
    actions::stop(&f.paths, &name, None).unwrap();
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
    );
    assert!(err.contains("NOWHERE_URL"), "{err}");
    assert!(err.contains(".env.example"), "{err}");
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
        actions::start(&paths, &config, &name, None, true, &notice).unwrap()
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
    let _ = actions::stop_all(&paths);
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
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
