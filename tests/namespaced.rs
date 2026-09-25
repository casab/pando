//! Namespaced mode against real servers, gated by `PANDO_TEST_NATIVE=1`.
//!
//! Namespaced mode writes into a server the developer owns, so what a fake
//! client cannot honestly claim is pinned here, against throwaway servers
//! this test starts itself: a MariaDB with its grant tables on, where a
//! login really is refused until the printed grant is run, and a Redis
//! with a password, where emptying one slot really leaves slot 0 alone.
//!
//! Everything is inside a temporary directory, on ports the kernel handed
//! out, and stopped when the test ends — never the developer's own
//! servers. Run it with:
//!
//! ```text
//! PANDO_TEST_NATIVE=1 cargo test --test namespaced -- --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pando::namespace::{Created, Login, Server};
use pando::recipes::{NamespaceRecipe, Recipes};
use tempfile::TempDir;

const APP_PASSWORD: &str = "p@ss w'rd $x";

fn enabled() -> bool {
    std::env::var("PANDO_TEST_NATIVE").as_deref() == Ok("1")
}

fn missing(binaries: &[&str]) -> Vec<String> {
    binaries
        .iter()
        .filter(|binary| {
            !Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {binary}"))
                .output()
                .is_ok_and(|out| out.status.success())
        })
        .map(|binary| binary.to_string())
        .collect()
}

fn skip(binaries: &[&str]) -> bool {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_NATIVE=1 to run against real servers");
        return true;
    }
    let missing = missing(binaries);
    if !missing.is_empty() {
        eprintln!("skipping: this machine has no {}", missing.join(", "));
        return true;
    }
    false
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn recipe(name: &str) -> NamespaceRecipe {
    Recipes::built_in()
        .get(name)
        .unwrap()
        .recipe
        .namespace
        .clone()
        .unwrap()
}

/// A server this test started, stopped when it goes out of scope.
struct Throwaway {
    dir: TempDir,
    port: u16,
    child: Child,
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("{what} never came up");
}

// ---- MariaDB ------------------------------------------------------------------

/// A MariaDB with its grant tables on — so logins are real — holding the
/// main checkout's database `shop` and an app login that may use it and
/// nothing else, as a developer's own server would.
fn mariadb() -> Throwaway {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    let init = Command::new("mariadb-install-db")
        .arg(format!("--datadir={}", data.display()))
        .arg("--auth-root-authentication-method=normal")
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let port = free_port();
    let child = Command::new("mariadbd")
        .arg(format!("--datadir={}", data.display()))
        .arg(format!("--port={port}"))
        .arg("--bind-address=127.0.0.1")
        .arg(format!("--socket={}", dir.path().join("s.sock").display()))
        .arg(format!("--pid-file={}", dir.path().join("pid").display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Throwaway { dir, port, child };
    wait_for("mariadb", || root_sql(&server, "SELECT 1").is_ok());
    root_sql(
        &server,
        &format!(
            "CREATE DATABASE shop; CREATE USER 'app'@'localhost' IDENTIFIED BY '{}'; \
             GRANT ALL ON shop.* TO 'app'@'localhost';",
            APP_PASSWORD.replace('\'', "''")
        ),
    )
    .unwrap();
    server
}

/// SQL as root, the server's administrator.
fn root_sql(server: &Throwaway, sql: &str) -> Result<String, String> {
    let out = Command::new("mariadb")
        .args(["--protocol=tcp", "-h", "127.0.0.1", "-P"])
        .arg(server.port.to_string())
        .args(["-u", "root", "-N", "-B", "-e", sql])
        .output()
        .unwrap();
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(String::from_utf8_lossy(&out.stderr).to_string()),
    }
}

fn app_server<'a>(recipe: &'a NamespaceRecipe, db: &Throwaway, bin: &Path) -> Server<'a> {
    Server {
        service: "mariadb",
        recipe,
        host: "127.0.0.1".into(),
        port: db.port,
        login: Login::new(
            Some("app".into()),
            Some(APP_PASSWORD.into()),
            "the test's app login",
        ),
        bin_dir: bin.to_path_buf(),
    }
}

// Decision 4, end to end: the app's login is refused until the grant pando
// prints is run once, and then it may make and drop `shop__…` — and still
// nothing else, which is the server's own wall behind pando's guard.
#[test]
fn a_real_mariadb_makes_a_worktrees_database_once_the_printed_grant_is_run() {
    if skip(&["mariadb-install-db", "mariadbd", "mariadb"]) {
        return;
    }
    let db = mariadb();
    let recipe = recipe("mariadb");
    let bin = db.dir.path().join("bin");
    let server = app_server(&recipe, &db, &bin);
    server.ping().expect("the app login answers");

    let refused = format!("{:#}", server.create("shop__feat_x", "shop").unwrap_err());
    assert!(refused.contains("Nothing was made"), "{refused}");
    assert!(!refused.contains("p@ss"), "{refused}");
    let grant = refused
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("GRANT "))
        .unwrap_or_else(|| panic!("no grant in: {refused}"))
        .to_string();
    assert_eq!(grant, "GRANT ALL ON `shop\\_\\_%`.* TO 'app'@'localhost';");
    assert_eq!(
        root_sql(&db, "SHOW DATABASES LIKE 'shop\\_\\_%'").unwrap(),
        ""
    );

    root_sql(&db, &grant).expect("the grant pando printed runs as written");
    assert!(!server.exists("shop__feat_x").unwrap());
    assert_eq!(
        server.create("shop__feat_x", "shop").unwrap(),
        Created::Made
    );
    assert!(server.exists("shop__feat_x").unwrap());
    assert_eq!(
        server.create("shop__feat_x", "shop").unwrap(),
        Created::AlreadyThere
    );
    // The grant is the prefix and nothing else.
    let outside = format!("{:#}", server.create("other__feat_x", "shop").unwrap_err());
    assert!(
        outside.contains("ERROR 1044") || outside.contains("does not let"),
        "{outside}"
    );

    server.drop("shop__feat_x", "shop").unwrap();
    assert!(!server.exists("shop__feat_x").unwrap());
    assert_eq!(
        root_sql(&db, "SHOW DATABASES LIKE 'shop'").unwrap(),
        "shop",
        "the main database is where it was"
    );

    let mut wrong = app_server(&recipe, &db, &bin);
    wrong.login = Login::new(Some("app".into()), Some("nope".into()), "a wrong password");
    let e = format!("{:#}", wrong.ping().unwrap_err());
    assert!(
        e.contains("ERROR 1045") && e.contains("a wrong password"),
        "{e}"
    );
}

// ---- Redis --------------------------------------------------------------------

const REDIS_PASSWORD: &str = "redis p@ss";

fn redis() -> Throwaway {
    let dir = TempDir::new().unwrap();
    let port = free_port();
    let child = Command::new("redis-server")
        .args(["--port", &port.to_string(), "--bind", "127.0.0.1"])
        .args(["--dir", &dir.path().display().to_string()])
        .args(["--save", "", "--daemonize", "no"])
        .args(["--requirepass", REDIS_PASSWORD])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Throwaway { dir, port, child };
    wait_for("redis", || {
        redis_cli(&server, &["ping"]).is_ok_and(|out| out == "PONG")
    });
    server
}

fn redis_cli(server: &Throwaway, args: &[&str]) -> Result<String, String> {
    let out = Command::new("redis-cli")
        .env("REDISCLI_AUTH", REDIS_PASSWORD)
        .args(["-e", "-h", "127.0.0.1", "-p", &server.port.to_string()])
        .args(args)
        .output()
        .unwrap();
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        false => Err(String::from_utf8_lossy(&out.stdout).to_string()),
    }
}

// The trap the recipe exists to avoid: emptying slot 3 leaves slot 0's
// keys, and a slot the server does not have is an error that touches
// nothing — where `redis-cli -n 16 FLUSHDB` would have emptied slot 0.
#[test]
fn a_real_redis_empties_one_slot_and_never_falls_back_to_slot_0() {
    if skip(&["redis-server", "redis-cli"]) {
        return;
    }
    let cache = redis();
    let recipe = recipe("redis");
    let bin: PathBuf = cache.dir.path().join("bin");
    let server = Server {
        service: "redis",
        recipe: &recipe,
        host: "127.0.0.1".into(),
        port: cache.port,
        login: Login::new(None, Some(REDIS_PASSWORD.into()), "the test's password"),
        bin_dir: bin.clone(),
    };
    server.ping().unwrap();
    redis_cli(&cache, &["-n", "0", "SET", "main-key", "keep"]).unwrap();
    redis_cli(&cache, &["-n", "3", "SET", "feat-key", "go"]).unwrap();
    assert_eq!(server.size(3).unwrap(), 1);
    assert_eq!(server.size(4).unwrap(), 0);

    server.drop("3", "0").unwrap();
    assert_eq!(server.size(3).unwrap(), 0);
    assert!(server.size(16).is_err(), "a slot the server does not have");
    assert!(server.drop("16", "0").is_err());
    assert_eq!(
        redis_cli(&cache, &["-n", "0", "GET", "main-key"]).unwrap(),
        "keep",
        "slot 0 is untouched"
    );

    let refused = Server {
        login: Login::none(),
        bin_dir: bin,
        ..server
    };
    let e = format!("{:#}", refused.ping().unwrap_err());
    assert!(e.contains("NOAUTH"), "{e}");
}
