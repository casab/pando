//! A fake PostgreSQL for the tests.
//!
//! The default suite must pass on a machine with no Postgres, so the
//! native tests install these five shims at `<pando home>/bin/`, which is
//! the directory `native.rs` prepends to PATH for every recipe command.
//! One home per test, so no test sees another's cluster and nothing
//! mutates the process environment.
//!
//! It is driven by the *shipped* recipe, not by a copy of it: `initdb`,
//! `postgres`, `pg_isready`, `psql` and `createdb` parse the arguments the
//! built-in `postgres.toml` really passes. So the recipe's own argument
//! shapes are under test here, and only the engine's semantics are faked.
//!
//! Two things it does deliberately, both learned from the fake docker:
//!
//! - the server **accepts** connections and holds them, rather than only
//!   binding. A listener that never accepts fills its backlog after a
//!   handful of readiness probes and then looks exactly like a server that
//!   died.
//! - it exits cleanly on SIGTERM, removing its socket, because that is
//!   what `stop` signals and a fake that ignored it would make `stop` look
//!   like it needed the SIGKILL.
//!
//! Everything it writes lives inside the data directory it was given and
//! under `<pando home>/pg-fake/`, both of which the invariant test needs
//! to be outside the repository.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The five binaries the built-in Postgres recipe names.
pub const BINARIES: [&str; 5] = ["initdb", "postgres", "pg_isready", "psql", "createdb"];

/// Writes every shim into a pando home.
pub fn install(home: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in BINARIES {
        let path = bin.join(name);
        std::fs::write(&path, SHIM).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Installs everything except `what`, so a test can drive the path where
/// this machine has no engine.
pub fn install_without(home: &Path, what: &str) {
    install(home);
    std::fs::remove_file(home.join("bin").join(what)).unwrap();
}

/// Where the fake keeps the port-to-cluster map that lets `psql` find the
/// data directory it is talking to.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join("pg-fake")
}

/// The databases this cluster has, as the fake records them.
pub fn databases(datadir: &Path) -> Vec<String> {
    lines(&datadir.join("fake-databases"))
}

/// The roles this cluster has.
pub fn roles(datadir: &Path) -> Vec<String> {
    lines(&datadir.join("fake-roles"))
}

/// How many times `initdb` has run against this data directory. The one
/// number that says whether "initialise once" is true.
pub fn initdb_runs(datadir: &Path) -> usize {
    lines(&datadir.join("fake-initdb-runs")).len()
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

const SHIM: &str = r##"#!/usr/bin/env python3
"""A fake PostgreSQL, enough of one for pando's tests."""
import os
import signal
import socket
import sys
import time

WHO = os.path.basename(sys.argv[0])
ARGS = sys.argv[1:]
HOME = os.path.dirname(os.path.dirname(os.path.abspath(sys.argv[0])))
ROOT = os.path.join(HOME, "pg-fake")
os.makedirs(ROOT, exist_ok=True)


def flag(*names):
    """The value of the first of `names` present, in either spelling."""
    for i, a in enumerate(ARGS):
        for name in names:
            if a == name and i + 1 < len(ARGS):
                return ARGS[i + 1]
            if a.startswith(name + "="):
                return a[len(name) + 1:]
    return None


def positionals():
    out, i = [], 0
    while i < len(ARGS):
        a = ARGS[i]
        if a.startswith("-") and a not in ("-q",):
            # Every flag this fake sees either takes a value or is `-q`.
            if a in ("--no-locale", "--auth=trust"):
                i += 1
                continue
            i += 2
            continue
        out.append(a)
        i += 1
    return out


def append(path, line):
    with open(path, "a") as handle:
        handle.write(line + "\n")


def has(path, value):
    try:
        with open(path) as handle:
            return value in [l.strip() for l in handle]
    except OSError:
        return False


def registry(port):
    return os.path.join(ROOT, "port-%s" % port)


def datadir_of(port):
    with open(registry(port)) as handle:
        return handle.read().strip()


def do_initdb():
    data = flag("--pgdata", "-D")
    user = flag("--username", "-U") or "postgres"
    if data is None:
        sys.stderr.write("initdb: error: no data directory specified\n")
        sys.exit(1)
    if os.path.exists(os.path.join(data, "PG_VERSION")):
        # What the real one does, and the reason the adopt rule exists.
        sys.stderr.write("initdb: error: directory \"%s\" exists but is not empty\n" % data)
        sys.exit(1)
    os.makedirs(data, exist_ok=True)
    with open(os.path.join(data, "PG_VERSION"), "w") as handle:
        handle.write("16\n")
    append(os.path.join(data, "fake-initdb-runs"), "ran")
    append(os.path.join(data, "fake-roles"), user)
    append(os.path.join(data, "fake-databases"), "postgres")
    print("Success.")


def do_postgres():
    data = flag("-D", "--pgdata")
    port = int(flag("-p", "--port"))
    sock = flag("-k", "--unix_socket_directories")
    if not os.path.exists(os.path.join(data, "PG_VERSION")):
        sys.stderr.write(
            "postgres: could not find the database system in \"%s\"\n" % data)
        sys.exit(2)
    listen = flag("-c") or ""
    if "listen_addresses=127.0.0.1" not in " ".join(ARGS):
        sys.stderr.write("postgres: refusing to guess listen_addresses (%s)\n" % listen)
        sys.exit(2)
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        server.bind(("127.0.0.1", port))
    except OSError as e:
        sys.stderr.write("postgres: could not bind IPv4 address \"127.0.0.1\": %s\n" % e)
        sys.exit(1)
    server.listen(64)
    socket_path = os.path.join(sock, ".s.PGSQL.%d" % port)
    with open(socket_path, "w") as handle:
        handle.write("")
    with open(registry(port), "w") as handle:
        handle.write(data)

    def bye(_signum, _frame):
        try:
            os.unlink(socket_path)
        except OSError:
            pass
        try:
            os.unlink(registry(port))
        except OSError:
            pass
        print("database system is shut down", flush=True)
        sys.exit(0)

    signal.signal(signal.SIGTERM, bye)
    signal.signal(signal.SIGINT, bye)
    print("database system is ready to accept connections on port %d" % port, flush=True)
    held = []
    while True:
        try:
            conn, _ = server.accept()
        except InterruptedError:
            continue
        held.append(conn)
        held = held[-64:]


def do_pg_isready():
    port = int(flag("-p", "--port"))
    probe = socket.socket()
    probe.settimeout(1.0)
    try:
        probe.connect(("127.0.0.1", port))
    except OSError:
        if "-q" not in ARGS:
            print("127.0.0.1:%d - no response" % port)
        sys.exit(2)
    finally:
        probe.close()
    if "-q" not in ARGS:
        print("127.0.0.1:%d - accepting connections" % port)
    sys.exit(0)


def quoted_after(text, needle):
    """The `'x'` that follows `needle` in a SQL fragment."""
    at = text.find(needle)
    if at < 0:
        return None
    rest = text[at + len(needle):]
    open_quote = rest.find("'")
    if open_quote < 0:
        return None
    close_quote = rest.find("'", open_quote + 1)
    return rest[open_quote + 1:close_quote]


def do_psql():
    port = int(flag("-p", "--port"))
    data = datadir_of(port)
    sql = flag("-tAc") or flag("-c") or ""
    name = quoted_after(sql, "rolname")
    if name is not None:
        if has(os.path.join(data, "fake-roles"), name):
            print("1")
        sys.exit(0)
    name = quoted_after(sql, "datname")
    if name is not None:
        if has(os.path.join(data, "fake-databases"), name):
            print("1")
        sys.exit(0)
    if "CREATE ROLE" in sql:
        role = sql.split('"')[1]
        append(os.path.join(data, "fake-roles"), role)
        print("CREATE ROLE")
        sys.exit(0)
    sys.stderr.write("psql: error: this fake does not understand %r\n" % sql)
    sys.exit(1)


def do_createdb():
    port = int(flag("-p", "--port"))
    data = datadir_of(port)
    owner = flag("-O", "--owner")
    names = [a for a in positionals()]
    if not names:
        sys.stderr.write("createdb: error: no database name\n")
        sys.exit(1)
    name = names[-1]
    if not has(os.path.join(data, "fake-roles"), owner or "postgres"):
        sys.stderr.write("createdb: error: role \"%s\" does not exist\n" % owner)
        sys.exit(1)
    if has(os.path.join(data, "fake-databases"), name):
        sys.stderr.write("createdb: error: database \"%s\" already exists\n" % name)
        sys.exit(1)
    append(os.path.join(data, "fake-databases"), name)


if WHO == "initdb":
    do_initdb()
elif WHO == "postgres":
    do_postgres()
elif WHO == "pg_isready":
    do_pg_isready()
elif WHO == "psql":
    do_psql()
elif WHO == "createdb":
    do_createdb()
else:
    sys.stderr.write("the fake postgres was called as %r\n" % WHO)
    sys.exit(127)
time.sleep(0)
"##;
