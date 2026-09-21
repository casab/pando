//! The one test that uses a real PostgreSQL, gated by
//! `PANDO_TEST_NATIVE=1`.
//!
//! Everything else about native services drives a fake engine, so the
//! default run needs no server installed. This is what pins the things a
//! fake cannot honestly claim: that the shipped recipe's `initdb` line
//! really builds a cluster, that `postgres` really comes up on the port
//! pando allocated with its socket in a directory short enough to bind,
//! that `pg_isready` really answers, that the `create` step really makes
//! the role and the database the app's own URL names, that a row written
//! in one worktree is absent from another, and that `rm` really takes the
//! data with it.
//!
//! Everything it touches is inside a temporary directory: its own pando
//! home, its own fixture repository, its own clusters. It binds no fixed
//! port — every port comes from `ports::assign` — and it stops what it
//! started even when an assertion panics.
//!
//! Run it with:
//!
//! ```text
//! PANDO_TEST_NATIVE=1 cargo test --test postgres -- --nocapture
//! ```
//!
//! `initdb` refuses to run as root, so a CI container that runs as root
//! has to run this as an unprivileged user or leave it gated off.

mod common;

use std::process::Command;

use common::{Kind, build, listener_on_port_env, paths_for};
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::state::{self, ServiceKind};
use pando::{actions, process};
use tempfile::TempDir;

fn enabled() -> bool {
    std::env::var("PANDO_TEST_NATIVE").as_deref() == Ok("1")
}

/// The binaries the built-in recipe names that this machine does *not*
/// have, so a skip can say which one is missing.
fn missing_binaries() -> Vec<&'static str> {
    ["postgres", "initdb", "pg_isready", "psql", "createdb"]
        .into_iter()
        .filter(|binary| {
            Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {binary}"))
                .output()
                .map(|out| !out.status.success())
                .unwrap_or(true)
        })
        .collect()
}

struct Real {
    _dir: TempDir,
    paths: PandoPaths,
    config: Config,
}

/// Nothing this test started outlives it, clusters included — and the
/// data directories go with the temp directory whatever happens.
impl Drop for Real {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
        if let Ok(store) = state::load(&self.paths.state_file()) {
            for name in store.worktrees.keys() {
                let _ = actions::rm(&self.paths, name, true, true, &|_| {});
            }
        }
    }
}

impl Real {
    fn service(&self, name: &str) -> state::ServiceRecord {
        let store = state::load(&self.paths.state_file()).unwrap();
        store.worktrees[name]
            .services
            .iter()
            .find(|s| s.name == "postgres")
            .cloned()
            .unwrap_or_else(|| panic!("no postgres record for {name}"))
    }

    fn url(&self, name: &str) -> String {
        actions::resolved_env(&self.paths, &self.config, name).unwrap()["DATABASE_URL"].clone()
    }
}

fn real() -> Real {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let home = dir.path().join("pando-home");
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
             [[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             env = {{ DATABASE_URL = \"postgres\" }}\n",
            listener_on_port_env()
        ),
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    Real {
        _dir: dir,
        paths,
        config,
    }
}

/// One SQL statement through the app's own URL, as the app would.
fn psql(url: &str, sql: &str) -> String {
    let out = Command::new("psql")
        .args([url, "-v", "ON_ERROR_STOP=1", "-tAc", sql])
        .output()
        .unwrap_or_else(|e| panic!("run psql: {e}"));
    assert!(
        out.status.success(),
        "psql {sql:?} against {url} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_real_postgres_comes_up_keeps_its_data_and_is_gone_after_rm() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_NATIVE=1 to run against a real PostgreSQL");
        return;
    }
    let missing = missing_binaries();
    if !missing.is_empty() {
        eprintln!("skipping: this machine has no {}", missing.join(", "));
        return;
    }
    let f = real();
    let name = actions::new(&f.paths, &f.config, "feat/one", None, &|_| {}).unwrap();
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .expect("start --isolated against a real postgres");

    // A cluster of its own, under pando's home, on a port pando chose.
    let service = f.service(&name);
    assert_eq!(service.kind, ServiceKind::Native);
    let port = service.port.expect("a port");
    let datadir = f.paths.service_data_dir(&name, "postgres");
    assert!(datadir.join("PG_VERSION").is_file());
    assert!(datadir.starts_with(&f.paths.home));
    assert!(process::is_alive(service.pid.unwrap()));

    // Its socket is in the short hashed directory, not in the data
    // directory — which is the thing `sun_path` would not have held.
    let socket_dir = f.paths.service_socket_dir(&name, "postgres");
    assert!(
        socket_dir.join(format!(".s.PGSQL.{port}")).exists(),
        "no socket in {}",
        socket_dir.display()
    );
    assert!(
        !datadir.join(format!(".s.PGSQL.{port}")).exists(),
        "the socket went into the data directory after all"
    );

    // The role and the database the app's own URL names really exist,
    // and the app can really use them.
    let url = f.url(&name);
    assert!(url.contains(&port.to_string()), "{url}");
    psql(&url, "CREATE TABLE pando_probe (note text)");
    psql(&url, "INSERT INTO pando_probe VALUES ('one')");
    assert_eq!(psql(&url, "SELECT note FROM pando_probe"), "one");

    // A second worktree is a second database: the row is not there.
    let two = actions::new(&f.paths, &f.config, "feat/two", None, &|_| {}).unwrap();
    actions::start(
        &f.paths,
        &f.config,
        &two,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .expect("start the second worktree");
    let other = f.url(&two);
    assert_ne!(other, url, "two worktrees on one port");
    assert_eq!(
        psql(
            &other,
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'pando_probe'"
        ),
        "0",
        "the second worktree can see the first one's table"
    );

    // Stopped and started again, with the row still there: the data
    // directory is adopted rather than rebuilt.
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    assert!(!process::is_alive(service.pid.unwrap()));
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .expect("start it again");
    let again = f.service(&name);
    assert_eq!(again.port, Some(port), "the port moved under the data");
    assert_eq!(psql(&f.url(&name), "SELECT note FROM pando_probe"), "one");

    // And rm takes the cluster with the worktree.
    let pid = again.pid.unwrap();
    actions::rm(&f.paths, &name, true, true, &|_| {}).unwrap();
    assert!(!process::is_alive(pid));
    assert!(!datadir.exists(), "the cluster outlived the worktree");
    assert!(!socket_dir.exists(), "the socket directory outlived it");
    // The sibling is untouched by its neighbour's removal.
    assert!(process::is_alive(f.service(&two).pid.unwrap()));
}

#[test]
fn doctor_reports_the_real_engine_it_found() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_NATIVE=1 to run against a real PostgreSQL");
        return;
    }
    if !missing_binaries().is_empty() {
        eprintln!("skipping: this machine has no PostgreSQL");
        return;
    }
    let f = real();
    let report = pando::doctor::run(&f.paths);
    let native = report
        .services
        .native
        .iter()
        .find(|n| n.name == "postgres")
        .expect("a native report");
    assert_eq!(native.source.as_deref(), Some("built-in"));
    assert!(
        native.engine.iter().all(|b| b.path.is_some()),
        "doctor could not find the engine this machine has: {:?}",
        native.engine
    );
    // Whatever version is installed; the claim is that it read one, not
    // which one, because that is a fact about the machine.
    let version = native.version.as_deref().unwrap_or_default();
    assert!(
        version.to_ascii_lowercase().contains("postgres"),
        "{version:?}"
    );
}
