//! `start --isolated` with a native service, end to end, against a fake
//! engine.
//!
//! The shape of the thing, and the same shape `isolation.rs` asserts for
//! containers: the service gets a role and a port beside the processes,
//! the app is told where it is through its own env key, its data
//! directory is initialised once and never again, it gets a log tab,
//! `stop` takes it down, and `rm` takes its data with it.
//!
//! Nothing here needs PostgreSQL installed. The shims in
//! `common::postgres` are driven by the *shipped* recipe, so the argument
//! shapes the built-in passes are what is under test; only the engine's
//! semantics are faked. `tests/postgres.rs` runs the same path against a
//! real server when `PANDO_TEST_NATIVE=1` asks for it.

use crate::common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Kind, build, listener_printing, paths_for, postgres};
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::state::{self, ServiceKind};
use pando::{actions, process};
use tempfile::TempDir;

struct Nat {
    _dir: TempDir,
    root: PathBuf,
    home: PathBuf,
    paths: PandoPaths,
    config: Config,
}

impl Nat {
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

    fn datadir(&self, name: &str, service: &str) -> PathBuf {
        self.paths.service_data_dir(name, service)
    }
}

/// Nothing this test started outlives it, databases included.
impl Drop for Nat {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
        if let Ok(store) = state::load(&self.paths.state_file()) {
            for name in store.worktrees.keys() {
                let _ = actions::rm(&self.paths, name, true, true, &|_| {});
            }
        }
    }
}

/// One native Postgres, and a dev process that prints the `DATABASE_URL`
/// it was given before it binds its port — so what the app was told is
/// visible in its own log.
fn config_toml() -> String {
    format!(
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
         [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
         env = {{ DATABASE_URL = \"postgres\" }}\n",
        listener_printing("DATABASE_URL")
    )
}

fn nat_with(config_text: &str) -> Nat {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    postgres::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(paths.config_file(), config_text).unwrap();
    let config = config::load(&paths).unwrap().config;
    Nat {
        _dir: dir,
        root,
        home,
        paths,
        config,
    }
}

fn nat() -> Nat {
    nat_with(&config_toml())
}

fn new_worktree(f: &Nat, branch: &str) -> String {
    actions::new(&f.paths, &f.config, branch, None, &|_| {}).unwrap()
}

fn start_isolated(f: &Nat, name: &str) -> actions::StartReport {
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

fn log_of(f: &Nat, name: &str, source: &str) -> String {
    let path = f.paths.log_file(name, source);
    for _ in 0..200 {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.trim().is_empty() {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    String::new()
}

// ---- the whole path -------------------------------------------------------

#[test]
fn an_isolated_start_gives_a_native_service_a_port_a_database_and_a_log() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);

    // A role and a port like any other service, and the worktree
    // remembers it is isolated.
    let record = f.record(&name);
    assert!(record.mode() == pando::state::ServiceMode::Isolated);
    let service = f.service(&name, "postgres");
    assert_eq!(service.kind, ServiceKind::Native);
    assert_eq!(
        service.compose_project, None,
        "a native service has no compose project"
    );
    let port = service.port.expect("a port");
    assert_eq!(record.ports.get("postgres"), Some(&port));
    assert_ne!(record.ports.get("web"), Some(&port));

    // The server is pando's own child, alive, and answering.
    let pid = service.pid.expect("a pid");
    assert!(process::is_alive(pid));
    assert_eq!(service.pgid, Some(pid as i32), "it leads its own group");
    assert!(pando::ports::something_is_listening(port));

    // Initialised exactly once, in pando's home and nowhere near the
    // repository.
    let datadir = f.datadir(&name, "postgres");
    assert!(datadir.starts_with(&f.home));
    assert!(!datadir.starts_with(&f.root));
    assert_eq!(postgres::initdb_runs(&datadir), 1);
    assert!(pando::native::marker(&datadir).is_some());

    // The role and the database the app's own URL names, created after
    // readiness: `.env.example` says `postgres://acme:acme@…/acme`.
    assert!(postgres::roles(&datadir).contains(&"acme".to_string()));
    assert!(postgres::databases(&datadir).contains(&"acme".to_string()));

    // The app was told where it is, on the allocated port, with its
    // credentials and database intact.
    let dev_log = log_of(&f, &name, "dev");
    assert!(
        dev_log.contains(&format!(
            "DATABASE_URL=postgres://acme:acme@localhost:{port}/acme"
        )),
        "{dev_log}"
    );

    // And it has a log tab of its own, which is the server's own output
    // rather than a pump in front of a container.
    let service_log = log_of(&f, &name, "postgres");
    assert!(
        service_log.contains("ready to accept connections"),
        "{service_log}"
    );
}

#[test]
fn a_second_start_reuses_the_database_it_already_has() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let first = f.service(&name, "postgres");
    let datadir = f.datadir(&name, "postgres");
    // A precondition, not the claim: if the server died between the two
    // starts for some reason of its own, the second start is *right* to
    // replace it, and the assertion below would blame the wrong thing.
    assert!(
        process::is_alive(first.pid.unwrap()),
        "the server was already gone before the second start"
    );

    start_isolated(&f, &name);
    let second = f.service(&name, "postgres");
    assert_eq!(second.pid, first.pid, "the live server was left alone");
    assert_eq!(second.port, first.port, "and keeps its port");
    assert_eq!(postgres::initdb_runs(&datadir), 1, "initdb ran again");
}

#[test]
fn two_worktrees_get_two_databases_on_two_ports() {
    let f = nat();
    let one = new_worktree(&f, "feat/one");
    let two = new_worktree(&f, "feat/two");
    start_isolated(&f, &one);
    start_isolated(&f, &two);

    let a = f.service(&one, "postgres");
    let b = f.service(&two, "postgres");
    assert_ne!(a.port, b.port);
    assert_ne!(a.pid, b.pid);
    assert_ne!(f.datadir(&one, "postgres"), f.datadir(&two, "postgres"));
    // Separate sockets too, which is the whole reason the socket
    // directory is hashed per worktree.
    assert_ne!(
        f.paths.service_socket_dir(&one, "postgres"),
        f.paths.service_socket_dir(&two, "postgres")
    );
    assert!(pando::ports::something_is_listening(a.port.unwrap()));
    assert!(pando::ports::something_is_listening(b.port.unwrap()));
}

#[test]
fn stop_takes_the_server_down_and_leaves_the_data() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.service(&name, "postgres");
    let datadir = f.datadir(&name, "postgres");
    let socket_dir = f.paths.service_socket_dir(&name, "postgres");
    assert!(socket_dir.is_dir());

    let outcome = actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    assert!(
        matches!(outcome, actions::StopOutcome::Stopped(_)),
        "a worktree whose database is up has something to stop: {outcome:?}"
    );
    assert!(!process::is_alive(before.pid.unwrap()));
    // The data survives a stop, and so does the port: the next start
    // brings the same database back.
    assert!(datadir.join("PG_VERSION").is_file());
    assert_eq!(f.service(&name, "postgres").port, before.port);
    assert!(
        !socket_dir.exists(),
        "the socket directory outlived the stop"
    );

    // And starting again adopts what is there rather than initialising a
    // second cluster over it.
    start_isolated(&f, &name);
    assert_eq!(postgres::initdb_runs(&datadir), 1);
    assert!(process::is_alive(f.service(&name, "postgres").pid.unwrap()));
}

#[test]
fn rm_takes_the_database_with_the_worktree() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let pid = f.service(&name, "postgres").pid.unwrap();
    let datadir = f.datadir(&name, "postgres");
    let socket_dir = f.paths.service_socket_dir(&name, "postgres");
    assert!(datadir.join("PG_VERSION").is_file());

    actions::rm(&f.paths, &name, true, true, &|_| {}).unwrap();
    assert!(!process::is_alive(pid));
    assert!(!datadir.exists(), "the data outlived the worktree");
    assert!(!f.paths.data_dir(&name).exists());
    assert!(!socket_dir.exists());
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(!store.worktrees.contains_key(&name));
}

#[test]
fn a_database_that_dies_comes_back_on_the_next_start_with_its_data() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.service(&name, "postgres");
    let datadir = f.datadir(&name, "postgres");

    // What a crashed database looks like from outside: a native record
    // *is* a process record, so the pid on it is simply gone.
    process::stop(before.pgid.unwrap(), Duration::from_secs(5)).unwrap();
    assert!(!process::is_alive(before.pid.unwrap()));
    assert!(!pando::ports::something_is_listening(before.port.unwrap()));

    start_isolated(&f, &name);
    let after = f.service(&name, "postgres");
    assert_ne!(after.pid, before.pid, "the dead pid was kept");
    assert!(process::is_alive(after.pid.unwrap()));
    assert_eq!(after.port, before.port, "and it keeps the port it owned");
    // Its data is the data it had: the marker is there, so the restart
    // adopts the cluster rather than making a second one.
    assert_eq!(postgres::initdb_runs(&datadir), 1);
}

// A crashed database used to linger in state until the next mutation, so
// `status` and the TUI went on reporting a service that was gone — on a
// pid the kernel may since have handed to something else. The read path
// forgets it, and says so once.
#[test]
fn a_database_that_dies_is_forgotten_by_the_next_read() {
    let f = nat();
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.service(&name, "postgres");
    process::stop(before.pgid.unwrap(), Duration::from_secs(5)).unwrap();

    let refreshed = actions::refresh(&f.paths);
    let record = &refreshed.state.worktrees[&name];
    assert!(
        record.services.iter().all(|s| s.name != "postgres"),
        "a dead database is still reported: {:?}",
        record.services
    );
    assert!(
        refreshed
            .notices
            .iter()
            .any(|n| n.contains("postgres") && n.contains("exited")),
        "{:?}",
        refreshed.notices
    );
    assert!(
        f.record(&name).services.is_empty(),
        "and the forgetting is saved"
    );
    assert!(
        record.mode() == pando::state::ServiceMode::Isolated,
        "the worktree is still an isolated one"
    );
    assert!(
        actions::refresh(&f.paths).notices.is_empty(),
        "said once, not on every tick"
    );

    // And the next start brings it back, as it always did.
    start_isolated(&f, &name);
    assert!(process::is_alive(f.service(&name, "postgres").pid.unwrap()));
}

// ---- what goes wrong ------------------------------------------------------

#[test]
fn an_engine_this_machine_does_not_have_refuses_before_anything_is_created() {
    // A recipe naming a binary no machine has, rather than one this one
    // might: PostgreSQL really is installed on the development laptop, so
    // deleting the shim would only make the *real* server answer, and the
    // test would then pass or fail depending on the host.
    let f = nat_with(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = { PORT = \"web\" }\n\n\
         [[services]]\nkind = \"native\"\nname = \"db\"\n",
    );
    let recipes = f.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(
        recipes.join("db.toml"),
        "kind = \"service\"\nname = \"db\"\n\
         binaries = [\"pando-no-such-engine\"]\n\
         install = \"brew install pando-no-such-engine\"\n\n\
         [service]\ninit = \"pando-no-such-engine init {datadir}\"\n\
         cmd = \"exec pando-no-such-engine -p {port}\"\n",
    )
    .unwrap();

    let name = new_worktree(&f, "feat/one");
    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("pando-no-such-engine"), "{e}");
    assert!(e.contains("not on PATH"), "{e}");
    assert!(e.contains("brew install pando-no-such-engine"), "{e}");
    assert!(e.contains("pando never installs an engine"), "{e}");
    // Refused before anything was created: no data directory, and no
    // process record for the dev server behind it.
    assert!(!f.datadir(&name, "db").exists());
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(store.worktrees[&name].processes.is_empty());
}

#[test]
fn a_preset_no_recipe_answers_to_names_the_ones_there_are() {
    let f = nat_with(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = { PORT = \"web\" }\n\n\
         [[services]]\nkind = \"native\"\nname = \"db\"\npreset = \"cassandra\"\n",
    );
    let name = new_worktree(&f, "feat/one");
    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("no recipe named \"cassandra\""), "{e}");
    assert!(e.contains("postgres"), "it names the ones there are: {e}");
}

#[test]
fn a_server_that_never_becomes_ready_fails_the_start_and_leaves_nothing_running() {
    // A recipe of the entry's own, with no preset at all: the shape a
    // developer reaches for when pando ships no recipe for their engine.
    let f = nat_with(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = { PORT = \"web\" }\n\n\
         [[services]]\nkind = \"native\"\nname = \"db\"\n\
         cmd = \"exec sleep 300\"\nready = \"false\"\nready_timeout_s = 1\n",
    );
    let name = new_worktree(&f, "feat/one");
    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("did not become ready"), "{e}");
    // The server it started is gone, and so is the claim that one is
    // there: the worktree was switching to its own services and never got
    // there, so it is left as the shared one it still is — no record of a
    // service, no port for it, no remembered mode. The data directory
    // stays: it was initialised, and the next start adopts it rather than
    // making a second one.
    let record = f.record(&name);
    assert!(
        record.services.is_empty(),
        "no record of a service that is not there: {:?}",
        record.services
    );
    assert!(!record.ports.contains_key("db"), "{:?}", record.ports);
    assert!(record.mode() != pando::state::ServiceMode::Isolated);
    assert!(f.datadir(&name, "db").exists());
    assert!(
        f.record(&name).processes.is_empty(),
        "the dev process was started behind a failed service"
    );
}

#[test]
fn a_users_own_recipe_replaces_the_built_in_for_the_start_that_runs_it() {
    let f = nat();
    // Same name, a different engine entirely: a recipe is data, and
    // dropping in a file of the same name is the documented way to
    // replace one.
    let recipes = f.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(
        recipes.join("postgres.toml"),
        "kind = \"service\"\nname = \"postgres\"\n\n[service]\n\
         port_env = \"DATABASE_URL\"\ninit = \"mkdir -p {datadir} && touch {datadir}/mine\"\n\
         cmd = \"exec python3 -u -c \\\"import socket,time;s=socket.socket();\
         s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
         s.bind(('127.0.0.1',{port}));s.listen(5);print('mine is up');time.sleep(300)\\\"\"\n",
    )
    .unwrap();

    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let datadir = f.datadir(&name, "postgres");
    assert!(
        datadir.join("mine").is_file(),
        "the user's init did not run"
    );
    assert_eq!(
        postgres::initdb_runs(&datadir),
        0,
        "the built-in's initdb ran even though the file replaced it"
    );
    assert!(log_of(&f, &name, "postgres").contains("mine is up"));
}

// ---- the two kinds in one project -----------------------------------------

#[test]
fn a_compose_service_and_a_native_one_share_a_role_space_and_come_up_together() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    // Both fakes in one home: they never see each other, because each
    // answers to a different name on the same PATH hook.
    common::docker::install(&home);
    postgres::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"redis\"]\nenv = {{ REDIS_URL = \"redis\" }}\n\n\
             [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             env = {{ DATABASE_URL = \"postgres\" }}\n",
            listener_printing("DATABASE_URL")
        ),
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    let f = Nat {
        _dir: dir,
        root,
        home,
        paths,
        config,
    };

    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);

    // One role space: three roles, three different ports, one window.
    let record = f.record(&name);
    let web = record.ports["web"];
    let redis = record.ports["redis"];
    let pg = record.ports["postgres"];
    assert_ne!(web, redis);
    assert_ne!(web, pg);
    assert_ne!(redis, pg);

    // One record each, of the right kind — a compose record carries the
    // project that can take its volume down, a native one carries the pid
    // that is the server.
    let compose = f.service(&name, "redis");
    let native = f.service(&name, "postgres");
    assert_eq!(compose.kind, ServiceKind::Compose);
    assert!(compose.compose_project.is_some());
    assert_eq!(native.kind, ServiceKind::Native);
    assert_eq!(native.compose_project, None);
    assert!(process::is_alive(native.pid.unwrap()));

    // Both env keys rewritten, in one environment.
    let env = actions::resolved_env(&f.paths, &f.config, &name).unwrap();
    assert_eq!(
        env.get("DATABASE_URL").map(String::as_str),
        Some(format!("postgres://acme:acme@localhost:{pg}/acme").as_str())
    );
    assert_eq!(
        env.get("REDIS_URL").map(String::as_str),
        Some(format!("redis://localhost:{redis}").as_str())
    );

    // And one stop takes both down.
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    assert!(!process::is_alive(native.pid.unwrap()));
    assert!(
        common::docker::services_up(&f.home, compose.compose_project.as_ref().unwrap()).is_empty(),
        "the container outlived the stop"
    );
}

// `native_instead` tells a developer to swap a compose service for a
// native one of the same name. The next start wrote the native record
// over the compose one and kept the log pump's pid, which then passed for
// a running server: no native postgres started, the app stayed on the
// container, and nothing in pando could find the container again.
#[test]
fn a_compose_service_that_becomes_native_is_stopped_before_the_server_starts() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    common::docker::install(&home);
    postgres::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    let config_with = |services: &str| {
        format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n{services}",
            listener_printing("DATABASE_URL")
        )
    };
    std::fs::write(
        paths.config_file(),
        config_with(
            "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\", \"redis\"]\n\
             env = { DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }\n",
        ),
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    let mut f = Nat {
        _dir: dir,
        root,
        home,
        paths,
        config,
    };
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.service(&name, "postgres");
    let project = before.compose_project.clone().unwrap();
    let pump = before.pgid.unwrap();
    let redis = common::docker::service_pids(&f.home, &project);

    std::fs::write(
        f.paths.config_file(),
        config_with(
            "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"redis\"]\nenv = { REDIS_URL = \"redis\" }\n\n\
             [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             env = { DATABASE_URL = \"postgres\" }\n",
        ),
    )
    .unwrap();
    f.config = config::load(&f.paths).unwrap().config;
    let said = std::cell::RefCell::new(Vec::<String>::new());
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|m| said.borrow_mut().push(m.to_string()),
    )
    .unwrap();

    let native = f.service(&name, "postgres");
    assert_eq!(native.kind, ServiceKind::Native);
    assert_eq!(native.compose_project, None);
    assert_ne!(native.pgid, Some(pump), "the log pump is not the server");
    assert!(!process::group_alive(pump), "the log pump was stopped");
    assert!(
        process::is_alive(native.pid.unwrap()),
        "a native server runs"
    );
    assert_eq!(native.port, before.port, "on the port the container had");
    assert!(
        common::docker::invocations_for(&f.home, &project)
            .contains(&format!("compose -p {project} stop postgres")),
        "{:?}",
        common::docker::invocations_for(&f.home, &project)
    );
    assert_eq!(
        common::docker::service_pids(&f.home, &project)
            .into_iter()
            .filter(|(service, _)| service == "redis")
            .collect::<Vec<_>>(),
        redis
            .into_iter()
            .filter(|(service, _)| service == "redis")
            .collect::<Vec<_>>(),
        "the compose service that stayed was left alone"
    );
    let said = said.into_inner();
    assert!(
        said.iter()
            .any(|m| m.contains("postgres runs natively now") && !m.contains("down -v")),
        "redis still names the project, so `rm` takes its data: {said:?}"
    );
}

/// A worktree started isolated with postgres in compose, its config then
/// switched to postgres natively: the record still names the compose
/// project, and the next isolated start is the switch. Returned with the
/// compose record as the first start left it. A `migrate` hook runs after
/// the services, and a start that forgets it says so.
fn switched_from_compose() -> (Nat, String, state::ServiceRecord) {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    common::docker::install(&home);
    postgres::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    let config_with = |services: &str| {
        format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n{services}\n\
             [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
             fingerprint = [\"package.json\"]\ncmd = \"true\"\n",
            listener_printing("DATABASE_URL")
        )
    };
    std::fs::write(
        paths.config_file(),
        config_with(
            "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }\n",
        ),
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    let mut f = Nat {
        _dir: dir,
        root,
        home,
        paths,
        config,
    };
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    let before = f.service(&name, "postgres");

    std::fs::write(
        f.paths.config_file(),
        config_with(
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             env = { DATABASE_URL = \"postgres\" }\n",
        ),
    )
    .unwrap();
    f.config = config::load(&f.paths).unwrap().config;
    (f, name, before)
}

/// Takes a compose project's containers and volumes down, as a developer
/// running the `down -v` a start prints does.
fn compose_down(f: &Nat, project: &str) {
    let out = std::process::Command::new(f.home.join("bin").join("docker"))
        .args(["compose", "-p", project, "down", "-v"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

/// The switch from compose to postgres natively, run to the end: the
/// native record, with a live server on the port the container had.
fn assert_switched(f: &Nat, name: &str, before: &state::ServiceRecord) {
    let native = f.service(name, "postgres");
    assert_eq!(native.kind, ServiceKind::Native, "{native:?}");
    assert_eq!(native.compose_project, None);
    assert!(
        process::is_alive(native.pid.unwrap()),
        "a native server runs"
    );
    assert_eq!(native.port, before.port, "on the port the container had");
}

// The native record was saved over the compose one before the container
// was stopped. A stop Docker could not do — the daemon down, or a stop
// that timed out — then left a container that could come back onto the
// native server's port with no record left to find it, and every later
// start of the native server failing on a port in use.
#[test]
fn a_compose_container_docker_could_not_stop_is_stopped_by_the_next_start() {
    let (f, name, before) = switched_from_compose();
    let project = before.compose_project.clone().unwrap();
    let _down = common::docker::down_on_drop(&f.home, &project);

    common::docker::daemon_down(&f.home);
    let refused = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .unwrap_err();
    assert!(
        format!("{refused:#}").contains("Docker is not running"),
        "{refused:#}"
    );
    let kept = f.service(&name, "postgres");
    assert_eq!(kept.kind, ServiceKind::Compose, "{kept:?}");
    assert_eq!(kept.compose_project.as_deref(), Some(project.as_str()));
    assert_eq!(kept.pid, None, "its log pump was stopped");

    std::fs::remove_file(common::docker::state_dir(&f.home).join("daemon-down")).unwrap();
    let said = std::cell::RefCell::new(Vec::<String>::new());
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|m| said.borrow_mut().push(m.to_string()),
    )
    .unwrap();

    assert_eq!(
        common::docker::services_up(&f.home, &project),
        Vec::<(String, u16)>::new(),
        "the container was stopped"
    );
    assert_switched(&f, &name, &before);
    let said = said.into_inner();
    assert!(
        said.iter().any(|m| m.contains("postgres runs natively now")
            && m.contains(&format!("docker compose -p {project} down -v"))),
        "nothing else names the project, so the start says how its data goes: {said:?}"
    );
}

// Compose, asked by project, fails a stop of a service it has no
// container for. A container removed outside pando — by the `down -v`
// the switch itself prints — failed every switch after it on that stop,
// and the compose record that was kept to retry it was kept for ever.
#[test]
fn a_compose_record_whose_container_is_gone_gives_way_to_the_native_server() {
    let (f, name, before) = switched_from_compose();
    let project = before.compose_project.clone().unwrap();
    compose_down(&f, &project);

    start_isolated(&f, &name);

    assert_switched(&f, &name, &before);
    let asked = common::docker::invocations_for(&f.home, &project);
    assert!(
        !asked.contains(&format!("compose -p {project} stop postgres")),
        "there was nothing to stop: {asked:?}"
    );
}

// A docker that cannot be run — uninstalled, or gone from the PATH —
// failed the stop of the container a native server replaces, so the
// switch was refused on every start after it: nothing could stop that
// container, or find it gone.
#[test]
fn a_compose_record_docker_cannot_be_run_for_gives_way_to_the_native_server() {
    let (f, name, before) = switched_from_compose();
    let project = before.compose_project.clone().unwrap();
    // Docker goes, and its containers with it.
    compose_down(&f, &project);
    std::fs::write(
        f.home.join("bin").join("docker"),
        "#!/nonexistent/python3\n",
    )
    .unwrap();

    let said = std::cell::RefCell::new(Vec::<String>::new());
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|m| said.borrow_mut().push(m.to_string()),
    )
    .unwrap();

    assert_switched(&f, &name, &before);
    let said = said.into_inner();
    assert!(
        said.iter()
            .any(|m| m.contains("docker cannot be run here") && m.contains(&project)),
        "{said:?}"
    );
    assert!(
        said.iter()
            .any(|m| m.contains("on other data") && m.contains("migrate")),
        "the switch itself runs the migration again: {said:?}"
    );
    // Nothing here could tell the container was gone — a docker missing
    // only from this PATH still has its daemon — so the record that names
    // the project stays, beside the native one, for what can ask.
    let record = f.record(&name);
    assert!(
        record
            .services
            .iter()
            .any(|s| s.kind == ServiceKind::Compose
                && s.compose_project.as_deref() == Some(project.as_str())
                && s.port.is_none()),
        "{:?}",
        record.services
    );
    let server = f.service(&name, "postgres").pid;

    // The leftover is not a service moving to other data: every start
    // after the switch, while docker could not be run, forgot the hooks
    // after the services and ran them again over the same native data.
    let said = std::cell::RefCell::new(Vec::<String>::new());
    let report = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|m| said.borrow_mut().push(m.to_string()),
    )
    .unwrap();
    assert!(report.started_nothing(), "the dev server stays up");
    let said = said.into_inner();
    assert!(
        !said.iter().any(|m| m.contains("on other data")),
        "{said:?}"
    );
    assert_eq!(f.service(&name, "postgres").pid, server);

    // Docker back, the next start finds no container left and lets the
    // record go, with the server left as it was.
    common::docker::install(&f.home);
    start_isolated(&f, &name);
    let record = f.record(&name);
    assert_eq!(record.services.len(), 1, "{:?}", record.services);
    assert_eq!(f.service(&name, "postgres").pid, server);
}

// No preflight ran for a worktree that was isolated already, so a switch
// to an engine this machine does not have stopped the container first and
// found the missing engine only when the native server was to start: the
// dev server, "already running", was left on a port nothing served.
#[test]
fn a_switch_to_an_engine_this_machine_does_not_have_leaves_the_container_running() {
    let (f, name, before) = switched_from_compose();
    let project = before.compose_project.clone().unwrap();
    let _down = common::docker::down_on_drop(&f.home, &project);
    let recipes = f.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(
        recipes.join("postgres.toml"),
        "kind = \"service\"\nname = \"postgres\"\n\
         binaries = [\"pando-no-such-engine\"]\n\
         install = \"brew install pando-no-such-engine\"\n\n\
         [service]\nport_env = \"DATABASE_URL\"\n\
         init = \"pando-no-such-engine init {datadir}\"\n\
         cmd = \"exec pando-no-such-engine -p {port}\"\n",
    )
    .unwrap();
    let up = common::docker::services_up(&f.home, &project);
    let dev = f.record(&name).processes["dev"].pgid;

    for mode in [actions::Mode::Remembered, actions::Mode::Isolated] {
        let e = format!(
            "{:#}",
            actions::start(&f.paths, &f.config, &name, None, mode, &|_| {}).unwrap_err()
        );
        assert!(e.contains("pando-no-such-engine"), "{mode:?}: {e}");
    }
    assert_eq!(
        f.service(&name, "postgres"),
        before,
        "the compose record, pump and all"
    );
    assert!(
        process::group_alive(before.pgid.unwrap()),
        "its log pump runs"
    );
    assert_eq!(common::docker::services_up(&f.home, &project), up);
    assert!(
        !common::docker::invocations_for(&f.home, &project)
            .iter()
            .any(|call| call.ends_with("stop postgres")),
        "{:?}",
        common::docker::invocations_for(&f.home, &project)
    );
    assert!(process::group_alive(dev), "the dev server is as it was");
}

// ---- a data directory the fingerprint cannot see --------------------------

// The other half of the hook-fingerprint bug, and the one only a native
// service has: no mode change at all, but the data directory is gone — a
// developer cleared it by hand — so the next start builds an empty one
// while the migration hook's inputs are untouched. Same silent wrongness,
// different cause.
#[test]
fn a_data_directory_built_from_nothing_runs_the_migration_again() {
    let sink = std::env::temp_dir().join(format!("pando-fresh-hook-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&sink);
    let f = nat_with(&format!(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
         env = {{ DATABASE_URL = \"postgres\" }}\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
         fingerprint = [\"package.json\"]\ncmd = \"echo migrated >> '{}'\"\n",
        sink.display()
    ));
    let ran = || {
        std::fs::read_to_string(&sink)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    assert_eq!(ran(), 1, "the first start migrates");

    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    start_isolated(&f, &name);
    assert_eq!(ran(), 1, "nothing changed, so it is skipped");

    // The cluster is gone; the migration files are not.
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    std::fs::remove_dir_all(f.datadir(&name, "postgres")).unwrap();
    start_isolated(&f, &name);
    assert_eq!(
        ran(),
        2,
        "an empty database is a changed input, whatever the files say"
    );
    let _ = std::fs::remove_file(&sink);
}

// A service that changes kind is on other data just as a new data
// directory is, and nothing reopened the gate for it: native to compose
// brought up an empty volume under a live dev server with no migration,
// and compose back to native found the old data directory, so no init
// said anything either.
#[test]
fn a_service_that_changes_kind_runs_the_migration_again_each_way() {
    let dir = TempDir::new().unwrap();
    let sink = dir.path().join("ran.txt");
    let config_with = |service: &str| {
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n{service}\n\n\
             [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
             fingerprint = [\"package.json\"]\ncmd = \"echo migrated >> '{}'\"\n",
            sink.display()
        )
    };
    let native = "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
                  env = { DATABASE_URL = \"postgres\" }";
    let compose = "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
                   include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }";
    let mut f = nat_with(&config_with(native));
    common::docker::install(&f.home);
    let ran = || {
        std::fs::read_to_string(&sink)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    assert_eq!(ran(), 1, "the first start migrates");

    let switch = |f: &mut Nat, service: &str| {
        std::fs::write(f.paths.config_file(), config_with(service)).unwrap();
        f.config = config::load(&f.paths).unwrap().config;
        let said = std::cell::RefCell::new(Vec::<String>::new());
        let report = actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|m| said.borrow_mut().push(m.to_string()),
        )
        .unwrap();
        assert!(report.started_nothing(), "the dev server stays up");
        said.into_inner()
    };
    let said = switch(&mut f, compose);
    let project = f.service(&name, "postgres").compose_project.unwrap();
    let _down = common::docker::down_on_drop(&f.home, &project);
    assert_eq!(ran(), 2, "an empty volume is a changed input: {said:?}");
    assert!(
        said.iter()
            .any(|m| m.contains("postgres runs in a container now") && m.contains("migrate")),
        "{said:?}"
    );

    let said = switch(&mut f, native);
    assert_eq!(f.service(&name, "postgres").kind, ServiceKind::Native);
    assert_eq!(
        postgres::initdb_runs(&f.datadir(&name, "postgres")),
        1,
        "the data directory it had before"
    );
    assert_eq!(ran(), 3, "and so is the old one: {said:?}");
}

// The same empty database, reached through a start that failed after the
// init. The marker was written before the server was waited on, so the
// retry saw a data directory it had made before — and nothing else said
// the migration had never run against it.
#[test]
fn a_data_directory_built_by_a_start_that_failed_still_runs_the_migration_again() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let sink = dir.path().join("ran.txt");
    let f = nat_with(&format!(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
         env = {{ DATABASE_URL = \"postgres\" }}\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
         fingerprint = [\"package.json\"]\ncmd = \"echo migrated >> '{}'\"\n",
        sink.display()
    ));
    let ran = || {
        std::fs::read_to_string(&sink)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let name = new_worktree(&f, "feat/one");
    start_isolated(&f, &name);
    assert_eq!(ran(), 1, "the first start migrates");
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();

    // The data is cleared by hand, and the start that rebuilds it never
    // hears the server say it is ready. Only this start waits a second:
    // the ones that come up keep the default, which a loaded machine
    // needs for a real server.
    std::fs::remove_dir_all(f.datadir(&name, "postgres")).unwrap();
    let probe = f.home.join("bin").join("pg_isready");
    std::fs::write(&probe, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut impatient = f.config.clone();
    for service in &mut impatient.services {
        if let config::ServiceConfig::Native {
            ready_timeout_s, ..
        } = service
        {
            *ready_timeout_s = Some(1);
        }
    }
    let e = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &impatient,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(e.contains("did not become ready"), "{e}");
    assert_eq!(postgres::initdb_runs(&f.datadir(&name, "postgres")), 1);
    assert_eq!(ran(), 1, "nothing ran behind a server that was not up");

    postgres::install(&f.home);
    start_isolated(&f, &name);
    assert_eq!(
        ran(),
        2,
        "the database the retry comes up on is the empty one the failed start made"
    );
}

// ---- what a native answer writes ------------------------------------------

/// A project that needs a database and has no file describing one: the
/// shape the native path exists for.
fn needs_a_database(config_text: &str) -> Nat {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::Plain, dir.path()).root;
    std::fs::write(
        root.join(".env.example"),
        "DATABASE_URL=postgres://acme:acme@localhost:5432/acme\n",
    )
    .unwrap();
    common::git(&root, &["add", "."]);
    common::git(&root, &["commit", "--quiet", "-m", "env example"]);
    let home = dir.path().join("pando-home");
    postgres::install(&home);
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(paths.config_file(), config_text).unwrap();
    let config = config::load(&paths).unwrap().config;
    Nat {
        _dir: dir,
        root,
        home,
        paths,
        config,
    }
}

/// Answers every question the way `--yes` does: takes what a rule
/// pre-ticked, and refuses anything it did not.
fn take_the_ticked(question: &actions::Question) -> anyhow::Result<actions::Answer> {
    if question.multi {
        return Ok(actions::Answer::Many(question.checked.clone()));
    }
    match question.preselect {
        Some(index) => Ok(actions::Answer::Auto(index)),
        None => anyhow::bail!("nothing to take for {:?}", question.prompt),
    }
}

/// Declines every set question, and takes the first of every other.
fn decline_the_set(question: &actions::Question) -> anyhow::Result<actions::Answer> {
    if question.multi {
        return Ok(actions::Answer::Many(Vec::new()));
    }
    take_the_ticked(question)
}

fn config_text(f: &Nat) -> String {
    std::fs::read_to_string(f.paths.config_file()).unwrap()
}

#[test]
fn a_native_answer_is_written_as_a_services_entry_of_its_own() {
    let f = needs_a_database(
        "[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n",
    );
    let resolved = actions::resolve_process(
        &f.paths,
        &f.config,
        actions::Mode::Isolated,
        &take_the_ticked,
        &|_| {},
    )
    .expect("resolve the services question");

    let text = config_text(&f);
    assert!(text.contains("kind = \"native\""), "{text}");
    assert!(text.contains("name = \"postgres\""), "{text}");
    assert!(text.contains("DATABASE_URL = \"postgres\""), "{text}");
    // `name` already says which recipe, so nothing repeats it.
    assert!(!text.contains("preset ="), "{text}");
    // And the provenance, as every written answer carries.
    assert!(
        text.contains("# detected:") || text.contains("# answered:"),
        "{text}"
    );
    assert_eq!(resolved.services.len(), 1);

    // It really runs, which is the point of writing it.
    let name = new_worktree(&f, "feat/one");
    actions::start(
        &f.paths,
        &resolved,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .expect("start what detection wrote");
    assert!(f.datadir(&name, "postgres").join("PG_VERSION").is_file());
}

// A native entry is one service and has nowhere to put "none of them",
// where a compose entry says it with an empty `include`. Without a place
// to record the negative the question returns on every isolated start,
// with nowhere to answer it but the TOML by hand.
#[test]
fn declining_the_native_services_question_is_recorded_and_never_asked_again() {
    let f = needs_a_database(
        "[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n",
    );
    // An engine no machine has, so the question is really asked rather
    // than taken: a recipe whose binary is missing is offered unticked.
    // Injected as a user recipe rather than by deleting a shim, because
    // this laptop has a real PostgreSQL and deleting the shim would only
    // make *that* answer.
    let recipes = f.paths.recipes_dir();
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(
        recipes.join("postgres.toml"),
        "kind = \"service\"\nname = \"postgres\"\n\
         binaries = [\"pando-no-such-engine\"]\n\n\
         [service]\ncmd = \"exec pando-no-such-engine -p {port}\"\n",
    )
    .unwrap();
    let resolved = actions::resolve_process(
        &f.paths,
        &f.config,
        actions::Mode::Isolated,
        &decline_the_set,
        &|_| {},
    )
    .expect("decline the services question");
    assert!(resolved.services.is_empty());
    assert!(resolved.isolation.none, "the answer was not recorded");
    let text = config_text(&f);
    assert!(text.contains("[isolation]"), "{text}");
    assert!(text.contains("none = true"), "{text}");

    // And the second pass asks nothing: an asker that panics proves it.
    let reloaded = config::load(&f.paths).unwrap().config;
    actions::resolve_process(
        &f.paths,
        &reloaded,
        actions::Mode::Isolated,
        &|q: &actions::Question| panic!("asked again: {:?}", q.prompt),
        &|_| {},
    )
    .expect("nothing left to ask");
}

// The preference lives in the user layer, because which mechanism a
// developer wants is a property of their laptop. `[services]` could not
// be the table: a `[services]` table and a `[[services]]` array of
// tables cannot both exist in one TOML document.
#[test]
fn the_preference_is_read_from_the_machine_wide_layer() {
    let f = needs_a_database(
        "[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n",
    );
    std::fs::write(
        f.paths.user_config_file(),
        "[isolation]\nprefer = \"native\"\n",
    )
    .unwrap();
    let config = config::load(&f.paths).unwrap().config;
    assert_eq!(config.isolation.preferred(), Some("native"));
    // And a spelling that is not one of the two kinds is refused rather
    // than silently ignored.
    std::fs::write(
        f.paths.user_config_file(),
        "[isolation]\nprefer = \"podman\"\n",
    )
    .unwrap();
    let loaded = config::load(&f.paths).unwrap();
    assert!(
        loaded.warnings.iter().any(|w| w.contains("prefer")),
        "{:?}",
        loaded.warnings
    );
    assert_eq!(loaded.config.isolation.preferred(), None);
}

// ---- two starts at once -----------------------------------------------------

// A switch to isolated lets go of the lock while its services come up and
// its hooks run. A plain start that runs in that window — the TUI and the
// CLI at once — finds a worktree that is not isolated with a live server on
// it, reads that as an interrupted switch, and takes the server down. The
// switch then carried on regardless: it replaced the plain start's process,
// set the worktree isolated, and spawned its application against a database
// that had just been stopped, on a worktree with no service left on record.
#[test]
fn a_switch_whose_services_another_start_took_down_starts_nothing() {
    let mut f =
        nat_with("[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n");
    std::fs::write(
        f.paths.config_file(),
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [runtime]\nprelude = \"\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
             [[services]]\nkind = \"native\"\nname = \"db\"\n\
             cmd = '''exec python3 -c \"import socket,time;s=socket.socket();\
             s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
             s.bind(('127.0.0.1',{{port}}));s.listen(5);time.sleep(300)\"'''\n\n\
             [[hooks]]\nname = \"a-plain-start-meanwhile\"\nafter = \"services\"\n\
             cmd = \"PANDO_HOME='{home}' '{bin}' start feat+one --no-wait\"\n",
            home = f.home.display(),
            bin = env!("CARGO_BIN_EXE_pando"),
        ),
    )
    .unwrap();
    f.config = config::load(&f.paths).unwrap().config;
    let name = new_worktree(&f, "feat/one");
    assert_eq!(name, "feat+one");

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(
        err.contains("another start of this worktree took down"),
        "{err}"
    );
    let record = f.record(&name);
    assert!(
        record.mode() != pando::state::ServiceMode::Isolated,
        "a worktree with no services is not isolated"
    );
    let dev = &record.processes["dev"];
    assert!(
        process::is_alive(dev.pid),
        "the plain start's process is the one running"
    );
}

// ---- a state file that breaks under a start ---------------------------------

// The second lock of a switch to isolated used to hand a state file it could
// not read straight to the undo — with that lock still held. The undo takes
// the same lock, and a second `flock` from one process on a second
// descriptor waits for the first: the start never returned, and every other
// pando command on the machine queued behind it.
#[test]
fn a_state_file_that_cannot_be_read_after_the_services_fails_the_start_rather_than_hanging_it() {
    let mut f =
        nat_with("[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n");
    let state_file = f.paths.state_file();
    let backup = state_file.with_extension("json.bak");
    std::fs::write(
        f.paths.config_file(),
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
             [[services]]\nkind = \"native\"\nname = \"db\"\n\
             cmd = '''exec python3 -c \"import socket,time;s=socket.socket();\
             s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);\
             s.bind(('127.0.0.1',{{port}}));s.listen(5);time.sleep(300)\"'''\n\n\
             [[probes]]\nname = \"breaks-state\"\nmatch = \"never printed\"\nhint = \"\"\n\
             cmd = \"cp '{state}' '{backup}' && echo broken > '{state}'\"\n",
            state = state_file.display(),
            backup = backup.display(),
        ),
    )
    .unwrap();
    f.config = config::load(&f.paths).unwrap().config;
    let name = new_worktree(&f, "feat/one");

    let (tx, rx) = std::sync::mpsc::channel();
    {
        let paths = f.paths.clone();
        let config = f.config.clone();
        let name = name.clone();
        std::thread::spawn(move || {
            let outcome = actions::start(
                &paths,
                &config,
                &name,
                None,
                actions::Mode::Isolated,
                &|_| {},
            );
            let _ = tx.send(outcome.map(|_| ()).map_err(|e| format!("{e:#}")));
        });
    }
    let Ok(outcome) = rx.recv_timeout(Duration::from_secs(60)) else {
        // The fixture's own teardown would queue behind the held lock too.
        std::mem::forget(f);
        panic!("the start never returned");
    };
    // Put the state back, so the fixture can stop what started.
    std::fs::copy(&backup, &state_file).unwrap();
    let err = outcome.expect_err("a state file that cannot be read is an error");
    assert!(err.contains("parse state file"), "{err}");
}
