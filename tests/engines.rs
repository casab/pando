//! The tests that use real database engines, gated by
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
//! PANDO_TEST_NATIVE=1 cargo test --test engines -- --nocapture
//! ```
//!
//! Each engine skips on its own when this machine does not have it, so a
//! laptop with Postgres and no MongoDB runs what it can and says what it
//! did not.
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

// ---- every shipped recipe, on whatever this machine has -------------------

/// One engine's recipe, and how to ask its server a question through the
/// address the app was given.
struct Engine {
    /// The recipe name, which is also the `[[services]]` entry's name.
    recipe: &'static str,
    /// What the env example has to call it, so the app is pointed at it.
    env_key: &'static str,
    /// A value in the shape the engine's clients expect, with a port for
    /// pando to rewrite.
    env_value: &'static str,
    /// Writes a value the server should keep, given the app's URL.
    write: fn(&str) -> (),
    /// Reads it back.
    read: fn(&str) -> String,
    /// What `read` should say.
    expect: &'static str,
}

fn run(program: &str, args: &[&str]) -> String {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `redis://localhost:PORT` → the port, for a client that takes one.
fn port_of(url: &str) -> String {
    url.rsplit(':')
        .next()
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

const ENGINES: [Engine; 3] = [
    Engine {
        recipe: "postgres",
        env_key: "DATABASE_URL",
        env_value: "postgres://acme:acme@localhost:5432/acme",
        write: |url| {
            psql(url, "CREATE TABLE pando_probe (note text)");
            psql(url, "INSERT INTO pando_probe VALUES ('kept')");
        },
        read: |url| psql(url, "SELECT note FROM pando_probe"),
        expect: "kept",
    },
    Engine {
        recipe: "redis",
        env_key: "REDIS_URL",
        env_value: "redis://localhost:6379",
        write: |url| {
            run(
                "redis-cli",
                &[
                    "-h",
                    "127.0.0.1",
                    "-p",
                    &port_of(url),
                    "set",
                    "pando_probe",
                    "kept",
                ],
            );
        },
        read: |url| {
            run(
                "redis-cli",
                &["-h", "127.0.0.1", "-p", &port_of(url), "get", "pando_probe"],
            )
        },
        expect: "kept",
    },
    Engine {
        recipe: "mariadb",
        env_key: "DATABASE_URL",
        env_value: "mysql://acme:acme@localhost:3306/acme",
        write: |url| {
            let port = port_of(url);
            run(
                "mariadb",
                &[
                    "--protocol=tcp",
                    "-h",
                    "127.0.0.1",
                    "-P",
                    &port,
                    "-u",
                    "acme",
                    "acme",
                    "-e",
                    "CREATE TABLE pando_probe (note text); \
                           INSERT INTO pando_probe VALUES ('kept')",
                ],
            );
        },
        read: |url| {
            let port = port_of(url);
            run(
                "mariadb",
                &[
                    "--protocol=tcp",
                    "--skip-column-names",
                    "-h",
                    "127.0.0.1",
                    "-P",
                    &port,
                    "-u",
                    "acme",
                    "acme",
                    "-e",
                    "SELECT note FROM pando_probe",
                ],
            )
        },
        expect: "kept",
    },
];

/// A fixture whose env example names this engine the way its clients
/// expect, and whose one service is that engine's recipe.
fn real_for(engine: &Engine) -> Real {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::Plain, dir.path()).root;
    // The env example is where the address lives, and the shape of the
    // value is what pando rewrites the port inside.
    std::fs::write(
        root.join(".env.example"),
        format!("{}={}\n", engine.env_key, engine.env_value),
    )
    .unwrap();
    common::git(&root, &["add", "."]);
    common::git(&root, &["commit", "--quiet", "-m", "env example"]);
    let home = dir.path().join("pando-home");
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 120\"\nports = []\n\n\
             [[services]]\nkind = \"native\"\nname = \"{}\"\nenv = {{ {} = \"{}\" }}\n",
            engine.recipe, engine.env_key, engine.recipe
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

/// The binaries a shipped recipe names that this machine does not have.
fn engine_missing(recipe: &str) -> Vec<String> {
    let recipes = pando::recipes::Recipes::built_in();
    recipes
        .get(recipe)
        .unwrap()
        .recipe
        .binaries
        .iter()
        .filter(|binary| {
            std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {binary}"))
                .output()
                .map(|out| !out.status.success())
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

// The same lifecycle for every shipped recipe, against the real engine:
// it comes up on the port pando allocated, the app's own address reaches
// it, what is written survives a stop and a start, and `rm` takes it all.
#[test]
fn every_shipped_recipe_runs_its_real_engine() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_NATIVE=1 to run against real engines");
        return;
    }
    let mut ran = Vec::new();
    for engine in &ENGINES {
        let missing = engine_missing(engine.recipe);
        if !missing.is_empty() {
            eprintln!(
                "skipping {}: this machine has no {}",
                engine.recipe,
                missing.join(", ")
            );
            continue;
        }
        let f = real_for(engine);
        let name = actions::new(&f.paths, &f.config, "feat/one", None, &|_| {}).unwrap();
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|_| {},
        )
        .unwrap_or_else(|e| panic!("{}: start --isolated: {e:#}", engine.recipe));

        let record = {
            let store = state::load(&f.paths.state_file()).unwrap();
            store.worktrees[&name]
                .services
                .iter()
                .find(|s| s.name == engine.recipe)
                .cloned()
                .unwrap()
        };
        assert_eq!(record.kind, ServiceKind::Native, "{}", engine.recipe);
        let port = record.port.expect("a port");
        assert!(process::is_alive(record.pid.unwrap()), "{}", engine.recipe);

        // The address the app was given, on the port pando allocated.
        let url =
            actions::resolved_env(&f.paths, &f.config, &name).unwrap()[engine.env_key].clone();
        assert!(url.contains(&port.to_string()), "{}: {url}", engine.recipe);
        (engine.write)(&url);
        assert_eq!((engine.read)(&url), engine.expect, "{}", engine.recipe);

        // Stopped and started again, with what was written still there.
        actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
        assert!(!process::is_alive(record.pid.unwrap()), "{}", engine.recipe);
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|_| {},
        )
        .unwrap_or_else(|e| panic!("{}: second start: {e:#}", engine.recipe));
        let again =
            actions::resolved_env(&f.paths, &f.config, &name).unwrap()[engine.env_key].clone();
        assert_eq!(again, url, "{}: the address moved", engine.recipe);
        assert_eq!(
            (engine.read)(&again),
            engine.expect,
            "{}: the data did not survive a restart",
            engine.recipe
        );

        // And rm takes the data and the socket directory with it.
        let datadir = f.paths.service_data_dir(&name, engine.recipe);
        let socket_dir = f.paths.service_socket_dir(&name, engine.recipe);
        actions::rm(&f.paths, &name, true, true, &|_| {}).unwrap();
        assert!(!datadir.exists(), "{}: the data outlived rm", engine.recipe);
        assert!(!socket_dir.exists(), "{}", engine.recipe);
        ran.push(engine.recipe);
    }
    eprintln!("ran against: {}", ran.join(", "));
    assert!(
        !ran.is_empty(),
        "PANDO_TEST_NATIVE=1 was set and no engine at all is installed"
    );
}

// MongoDB ships untested, and what that looks like to a user is the
// thing worth pinning: pando says so before it starts it, `doctor` says
// so, and the recipe's own note says so. If someone with mongod runs
// this and it passes, the recipe's `untested` flag is what should change.
#[test]
fn mongodb_is_shipped_saying_it_has_never_been_run() {
    let recipe = pando::recipes::Recipes::built_in()
        .get("mongodb")
        .unwrap()
        .recipe
        .clone();
    assert!(recipe.untested, "mongodb is marked as proven");
    if engine_missing("mongodb").is_empty() {
        eprintln!("this machine has mongod: the recipe could be proven and `untested` removed");
    }
}
