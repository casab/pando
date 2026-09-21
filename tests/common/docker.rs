//! A fake `docker` for the tests.
//!
//! The default test suite must pass on a machine with no Docker, so every
//! test that exercises isolation installs this shim at
//! `<pando home>/bin/docker`, which is where `services::docker_program`
//! looks before it falls back to PATH. One home per test, so the shims
//! never see each other's projects and nothing mutates the process
//! environment.
//!
//! It emulates the six invocations pando makes — `up -d`, `ps`, `logs -f`,
//! `stop`, `down -v`, and anything else as a no-op — and it is real enough
//! to drive readiness: `up` reads the published ports out of the override
//! pando generated and puts a listener on each one, so a connect probe has
//! something to connect to.
//!
//! Everything it writes lives under `<pando home>/docker-fake/`, which the
//! invariant test needs: a marker file next to the repository would be a
//! write into the developer's project.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Writes the shim into a pando home and returns its path.
pub fn install(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("docker");
    std::fs::write(&path, SHIM).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Where the shim keeps what it pretends to run.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join("docker-fake")
}

/// Every invocation the shim has seen, in order, with absolute paths
/// shortened to their file names so an assertion can be exact.
pub fn invocations(home: &Path) -> Vec<String> {
    let path = state_dir(home).join("invocations.log");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Only the invocations for one compose project.
pub fn invocations_for(home: &Path, project: &str) -> Vec<String> {
    invocations(home)
        .into_iter()
        .filter(|line| line.contains(project))
        .collect()
}

/// Tells the shim that these services declare a compose `healthcheck`, so
/// `ps` reports health for them and readiness goes through it.
pub fn with_healthcheck(home: &Path, project: &str, services: &[&str]) {
    let dir = state_dir(home).join(project);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("healthcheck"), services.join(",")).unwrap();
}

/// Tells the shim to bring the project up but never let it become ready:
/// no listener is bound and health stays `starting`.
pub fn never_ready(home: &Path, project: &str) {
    let dir = state_dir(home).join(project);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("never-ready"), "1").unwrap();
}

/// Tells the shim to publish the ports with nothing behind them: the
/// container runs, the connect succeeds, and the connection is closed at
/// once. That is what Docker's port proxy does in front of a container
/// that is up but is not listening inside yet — a database still running
/// its first-boot initialisation, or an image that publishes a port and
/// never binds it.
pub fn proxy_only(home: &Path, project: &str) {
    let dir = state_dir(home).join(project);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("proxy-only"), "1").unwrap();
}

/// Whether the shim still has containers for this project, and on which
/// ports. Empty once `down -v` has run.
pub fn services_up(home: &Path, project: &str) -> Vec<(String, u16)> {
    let path = state_dir(home).join(project).join("services.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<(String, u16)> = map
        .iter()
        .filter(|(_, entry)| entry.get("stopped").and_then(|v| v.as_bool()) != Some(true))
        .filter_map(|(name, entry)| Some((name.clone(), entry.get("port")?.as_u64()? as u16)))
        .collect();
    out.sort();
    out
}

/// The pid of the listener the shim put behind each service, so a test can
/// tell "the container that was already up was left alone" from "it was
/// replaced by one on a different port".
pub fn service_pids(home: &Path, project: &str) -> Vec<(String, u64)> {
    let path = state_dir(home).join(project).join("services.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<(String, u64)> = map
        .iter()
        .filter_map(|(name, entry)| Some((name.clone(), entry.get("pid")?.as_u64()?)))
        .collect();
    out.sort();
    out
}

/// Whether `down -v` has been run for this project.
pub fn was_downed(home: &Path, project: &str) -> bool {
    state_dir(home).join(format!("{project}.down")).exists()
}

const SHIM: &str = r##"#!/usr/bin/env python3
"""A fake docker, enough of one for pando's tests."""
import json
import os
import shutil
import signal
import subprocess
import sys
import time

ARGS = sys.argv[1:]
HOME = os.path.dirname(os.path.dirname(os.path.abspath(sys.argv[0])))
ROOT = os.path.join(HOME, "docker-fake")
os.makedirs(ROOT, exist_ok=True)


def scan():
    project, files, words, i = None, [], [], 0
    while i < len(ARGS):
        a = ARGS[i]
        nxt = ARGS[i + 1] if i + 1 < len(ARGS) else None
        if a == "-p" and nxt is not None and not nxt.startswith("-"):
            project, i = nxt, i + 2
            continue
        if a == "-f" and nxt is not None and not nxt.startswith("-"):
            files.append(nxt)
            i += 2
            continue
        if a == "--format" and nxt is not None:
            i += 2
            continue
        if a.startswith("-"):
            i += 1
            continue
        words.append(a)
        i += 1
    return project, files, words


PROJECT, FILES, WORDS = scan()
VERB = WORDS[1] if len(WORDS) > 1 else ""
REST = WORDS[2:]
PROJ = os.path.join(ROOT, PROJECT or "no-project")


def record():
    short = [os.path.basename(a) if a.startswith("/") else a for a in ARGS]
    with open(os.path.join(ROOT, "invocations.log"), "a") as handle:
        handle.write(" ".join(short) + "\n")


def read_state():
    try:
        with open(os.path.join(PROJ, "services.json")) as handle:
            return json.load(handle)
    except Exception:
        return {}


def write_state(state):
    os.makedirs(PROJ, exist_ok=True)
    with open(os.path.join(PROJ, "services.json"), "w") as handle:
        json.dump(state, handle)


def healthchecked():
    try:
        with open(os.path.join(PROJ, "healthcheck")) as handle:
            return [s for s in handle.read().strip().split(",") if s]
    except Exception:
        return []


def published():
    """service -> (host port, container port), read from pando's override."""
    out, current = {}, None
    for path in FILES:
        try:
            lines = open(path).read().splitlines()
        except Exception:
            continue
        for line in lines:
            if line.startswith("  ") and not line.startswith("    ") and line.strip().endswith(":"):
                current = line.strip()[:-1]
            elif "ports: !override" in line and current:
                inside = line[line.index("[") + 1:line.rindex("]")].strip().strip('"')
                parts = inside.split(":")
                out[current] = (int(parts[1]), int(parts[2]))
    return out


def spawn(code):
    child = subprocess.Popen(
        [sys.executable, "-c", code],
        start_new_session=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return child.pid


def listen(port):
    """A server: it holds the connection open and says nothing."""
    return spawn(
        "import socket,time\n"
        "s=socket.socket()\n"
        "s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n"
        "s.bind(('127.0.0.1',%d))\n"
        "s.listen(5)\n"
        "time.sleep(86400)\n" % port
    )


def hang_up(port):
    """Docker's own port proxy in front of a container that is running but
    is not listening inside: the connect succeeds and the connection is
    closed at once."""
    return spawn(
        "import socket\n"
        "s=socket.socket()\n"
        "s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n"
        "s.bind(('127.0.0.1',%d))\n"
        "s.listen(5)\n"
        "while True:\n"
        "    c,_=s.accept()\n"
        "    c.close()\n" % port
    )


def alive(pid):
    if not pid:
        return False
    try:
        os.kill(pid, 0)
        return True
    except Exception:
        return False


def kill_all(state):
    for entry in state.values():
        pid = entry.get("pid") or 0
        if alive(pid):
            try:
                os.killpg(os.getpgid(pid), signal.SIGTERM)
            except Exception:
                try:
                    os.kill(pid, signal.SIGTERM)
                except Exception:
                    pass
        entry["pid"] = 0
        entry["stopped"] = True


def do_up():
    stuck = os.path.exists(os.path.join(PROJ, "never-ready"))
    proxy = os.path.exists(os.path.join(PROJ, "proxy-only"))
    checks = healthchecked()
    state = read_state()
    wanted = REST or list(published().keys())
    for name, (host, container) in published().items():
        if name not in wanted:
            continue
        entry = state.get(name, {})
        if alive(entry.get("pid") or 0):
            continue
        entry["port"] = host
        entry["container"] = container
        entry["stopped"] = False
        entry["pid"] = 0 if stuck else (hang_up(host) if proxy else listen(host))
        entry["health"] = ("starting" if stuck else "healthy") if name in checks else ""
        state[name] = entry
    write_state(state)


def do_ps():
    stuck = os.path.exists(os.path.join(PROJ, "never-ready"))
    for name, entry in sorted(read_state().items()):
        if entry.get("stopped"):
            state = "exited"
        elif stuck:
            state = "running"
        else:
            state = "running" if alive(entry.get("pid") or 0) else "exited"
        print(json.dumps({
            "Service": name,
            "State": state,
            "Health": entry.get("health", ""),
            "Publishers": [{
                "URL": "127.0.0.1",
                "TargetPort": entry.get("container", 0),
                "PublishedPort": entry.get("port", 0),
                "Protocol": "tcp",
            }],
        }))


def do_logs():
    name = REST[0] if REST else "service"
    print("%s-1  | fake docker log for %s" % (name, name), flush=True)
    while True:
        time.sleep(0.2)


def do_stop():
    state = read_state()
    kill_all(state)
    write_state(state)


def do_down():
    state = read_state()
    kill_all(state)
    with open(os.path.join(ROOT, (PROJECT or "no-project") + ".down"), "w") as handle:
        handle.write("-v" if "-v" in ARGS else "")
    shutil.rmtree(PROJ, ignore_errors=True)


record()
if VERB == "up":
    do_up()
elif VERB == "ps":
    do_ps()
elif VERB == "logs":
    do_logs()
elif VERB == "stop":
    do_stop()
elif VERB == "down":
    do_down()
sys.exit(0)
"##;
