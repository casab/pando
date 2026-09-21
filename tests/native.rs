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

mod common;

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
    assert!(record.isolated);
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
    // there. The data directory stays: it was initialised, and the next
    // start adopts it rather than making a second one.
    let service = f.service(&name, "db");
    assert_eq!(service.pid, None);
    assert_eq!(service.pgid, None);
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
