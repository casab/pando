//! The one test that uses the real Docker, gated by `PANDO_TEST_DOCKER=1`.
//!
//! Everything else in the suite drives a fake docker, so the default run
//! needs no daemon. This is what pins the two behaviours the fake cannot
//! honestly claim: that Compose really replaces a `ports` list when it
//! sees `!override`, and that a per-worktree project name really gives
//! each worktree its own containers, network and volumes.
//!
//! It only ever creates, inspects or removes compose projects whose name
//! starts with `pando-`, and it takes its own down on the way out even
//! when an assertion panicked.
//!
//! Every assertion about what docker holds is scoped to the *compose
//! projects the test itself made*, never to the `pando-` prefix. These
//! tests run on parallel threads, each with containers, volumes and
//! networks of its own up at the same moment, so "how many `pando-`
//! containers are there" is a question about the whole run rather than
//! about the test asking it. Scoping is the fix; serialising them would
//! hide the coupling rather than remove it.

use crate::common;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use common::{Kind, build, listener_on_port_env, paths_for};
use pando::compose;
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::{actions, state};
use tempfile::TempDir;

/// The prefix every compose project pando makes carries. Nothing in this
/// file is allowed to touch a project that does not start with it.
const PREFIX: &str = "pando-";

fn enabled() -> bool {
    std::env::var("PANDO_TEST_DOCKER").as_deref() == Ok("1")
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run docker {args:?}: {e}"));
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Names of everything docker has for these compose projects, by kind.
///
/// Scoped to the projects the calling test made, never to the bare
/// `pando-` prefix. These tests run on parallel threads and every one of
/// them has `pando-` containers, volumes and networks of its own up at the
/// same moment, so a global count is not a fact about the test making it —
/// it is a fact about whichever sibling happened to be mid-run.
fn items_of(kind: &str, projects: &[String]) -> Vec<String> {
    let args: Vec<&str> = match kind {
        "container" => vec!["ps", "-a", "--format", "{{.Names}}"],
        "volume" => vec!["volume", "ls", "--format", "{{.Name}}"],
        _ => vec!["network", "ls", "--format", "{{.Name}}"],
    };
    docker(&args)
        .lines()
        .map(str::trim)
        .filter(|name| projects.iter().any(|project| name.starts_with(project)))
        .map(str::to_string)
        .collect()
}

struct RealDocker {
    _dir: TempDir,
    paths: PandoPaths,
    config: Config,
    /// Every compose project this test may have created, taken down on the
    /// way out however the test ended.
    projects: Vec<String>,
}

impl Drop for RealDocker {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
        for project in &self.projects {
            // The one guard that matters: a name that is not pando's is
            // never passed to `down -v`.
            assert!(project.starts_with(PREFIX), "{project}");
            let _ = Command::new("docker")
                .args(["compose", "-p", project, "down", "-v"])
                .output();
        }
    }
}

/// Fixture 1, with its compose file rewritten to images that are small and
/// already on this machine, and with named volumes — the thing a
/// per-worktree project name has to isolate.
fn real() -> RealDocker {
    real_with(&listener_on_port_env())
}

/// The same fixture with the dev command injected, so a test can watch what
/// the process that is running was actually told.
fn real_with(dev: &str) -> RealDocker {
    real_tuned(dev, 90)
}

/// …and with the readiness timeout injected, so a test about a service
/// that never becomes ready does not have to wait a minute and a half for
/// the answer.
fn real_tuned(dev: &str, ready_timeout_s: u64) -> RealDocker {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    std::fs::write(
        root.join("docker-compose.yml"),
        "services:\n  \
         postgres:\n    image: postgres:16-alpine\n    \
         environment:\n      POSTGRES_PASSWORD: pando\n    \
         ports: [\"5432:5432\"]\n    volumes:\n      - pgdata:/var/lib/postgresql/data\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n    \
         volumes:\n      - redisdata:/data\n\
         volumes:\n  pgdata:\n  redisdata:\n",
    )
    .unwrap();
    common::git(&root, &["add", "."]);
    common::git(&root, &["commit", "--quiet", "-m", "small images"]);

    let paths = paths_for(&dir.path().join("pando-home"), &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{dev}'''\nports = {{ PORT = \"web\" }}\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\", \"redis\"]\n\
             env = {{ DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }}\n\
             ready_timeout_s = {ready_timeout_s}\n"
        ),
    )
    .unwrap();
    let config = config::load(&paths).unwrap().config;
    RealDocker {
        _dir: dir,
        paths,
        config,
        projects: Vec::new(),
    }
}

fn worktree_of(f: &RealDocker, name: &str) -> PathBuf {
    f.config.worktrees_dir(&f.paths).join(name)
}

/// `<container> <ports>` for the containers of these compose projects that
/// are *running*, sorted so two samples compare as one value.
///
/// Scoped like [`items_of`], and for the same reason.
fn published_ports(projects: &[String]) -> Vec<String> {
    let mut out: Vec<String> = docker(&["ps", "--format", "{{.Names}} {{.Ports}}"])
        .lines()
        .map(str::trim)
        .filter(|line| projects.iter().any(|project| line.starts_with(project)))
        .map(str::to_string)
        .collect();
    out.sort();
    out
}

/// A second `start` of a worktree that is already running must keep every
/// port it owns. The containers hold the service ports themselves, and
/// reading that as "somebody took them" moves the whole window and leaves
/// the live application pointed at nothing.
#[test]
fn a_second_isolated_start_keeps_the_containers_and_the_ports_they_were_given() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real_with(&common::listener_printing("DATABASE_URL"));
    let one = actions::new(&f.paths, &f.config, "feat/one", None, &|_| {}).unwrap();
    f.projects
        .push(compose::project_name(f.paths.project_id(), &one));

    let first = actions::start(
        &f.paths,
        &f.config,
        &one,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("first: {m}"),
    )
    .unwrap();
    let before = published_ports(&f.projects);
    assert_eq!(before.len(), 2, "postgres and redis are up: {before:?}");

    let second = actions::start(
        &f.paths,
        &f.config,
        &one,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("second: {m}"),
    )
    .unwrap();
    assert_eq!(second.ports, first.ports, "every port stays where it was");
    assert!(!second.reassigned, "and nothing is reported as moved");
    assert_eq!(
        published_ports(&f.projects),
        before,
        "the containers keep the ports they were published on"
    );
    for role in ["postgres", "redis"] {
        assert!(
            pando::ports::something_is_listening(first.ports[role]),
            "{role} still answers on {}",
            first.ports[role]
        );
    }

    // The process that never stopped printed its DATABASE_URL when it was
    // spawned. `status --env` must still say the same thing.
    let env = actions::resolved_env(&f.paths, &f.config, &one).unwrap();
    let path = f.paths.log_file(&one, "dev");
    let mut log = String::new();
    for _ in 0..100 {
        log = std::fs::read_to_string(&path).unwrap_or_default();
        if log.contains("DATABASE_URL=") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        log.contains(&format!("DATABASE_URL={}", env["DATABASE_URL"])),
        "the running process and `status --env` disagree: env={:?} log={log:?} at {}",
        env["DATABASE_URL"],
        path.display()
    );
}

#[test]
fn two_worktrees_get_private_services_on_their_own_ports_and_volumes() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real();
    let one = actions::new(&f.paths, &f.config, "feat/one", None, &|_| {}).unwrap();
    let two = actions::new(&f.paths, &f.config, "feat/two", None, &|_| {}).unwrap();
    f.projects
        .push(compose::project_name(f.paths.project_id(), &one));
    f.projects
        .push(compose::project_name(f.paths.project_id(), &two));
    // Two project names carrying this fixture's own id, so nothing
    // anywhere belongs to them yet. Whatever belongs to them at the end is
    // something this test leaked.
    let before = items_of("volume", &f.projects);
    assert!(
        before.is_empty(),
        "a fresh fixture owns no volumes: {before:?}"
    );

    let a = actions::start(
        &f.paths,
        &f.config,
        &one,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("one: {m}"),
    )
    .unwrap();
    let b = actions::start(
        &f.paths,
        &f.config,
        &two,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("two: {m}"),
    )
    .unwrap();

    // Different ports for the same two services, which is the whole point.
    assert_ne!(a.ports["postgres"], b.ports["postgres"]);
    assert_ne!(a.ports["redis"], b.ports["redis"]);

    // Readiness went through a connect, with no client for either
    // service installed — and the ports really are open.
    for port in [
        a.ports["postgres"],
        a.ports["redis"],
        b.ports["postgres"],
        b.ports["redis"],
    ] {
        assert!(
            pando::ports::something_is_listening(port),
            "nothing answers on {port}"
        );
    }

    // `!override` replaced the project's hardcoded 5432, rather than
    // being merged beside it: two worktrees on one machine could not both
    // have published it.
    let mine = published_ports(&f.projects);
    assert_eq!(mine.len(), 4, "four containers, two per worktree: {mine:?}");
    assert!(
        !mine.iter().any(|line| line.contains(":5432->")),
        "the project's own port must not be published: {mine:?}"
    );

    // And each worktree's data is its own: the project name prefixes the
    // named volumes, so there are two distinct sets.
    let volumes = items_of("volume", &f.projects);
    let a_volumes: Vec<&String> = volumes
        .iter()
        .filter(|v| v.starts_with(&f.projects[0]))
        .collect();
    let b_volumes: Vec<&String> = volumes
        .iter()
        .filter(|v| v.starts_with(&f.projects[1]))
        .collect();
    assert_eq!(a_volumes.len(), 2, "{volumes:?}");
    assert_eq!(b_volumes.len(), 2, "{volumes:?}");
    assert!(!a_volumes.iter().any(|v| b_volumes.contains(v)));

    // The app is told where its own database is.
    let env = actions::resolved_env(&f.paths, &f.config, &one).unwrap();
    assert_eq!(
        env["DATABASE_URL"],
        format!(
            "postgres://acme:acme@localhost:{}/acme",
            a.ports["postgres"]
        )
    );

    // Stopping one leaves the other serving.
    actions::stop(&f.paths, &one, None, &|_| {}).unwrap();
    assert!(
        pando::ports::something_is_listening(b.ports["postgres"]),
        "stopping one worktree must not touch another's services"
    );
    assert!(!pando::ports::something_is_listening(a.ports["postgres"]));

    // And `rm` takes the containers, the network and the volumes with it.
    actions::rm(&f.paths, &one, false, true, &|_| {}).unwrap();
    actions::rm(&f.paths, &two, false, true, &|_| {}).unwrap();
    for kind in ["container", "volume", "network"] {
        let left = items_of(kind, &f.projects);
        assert!(left.is_empty(), "{kind}s left behind: {left:?}");
    }
    assert_eq!(
        items_of("volume", &f.projects),
        before,
        "no volume of this test's outlived it"
    );
    // Nothing of pando's is running, so the guard has nothing left to do.
    f.projects.clear();
}

#[test]
fn a_service_a_worktree_cannot_isolate_is_refused_before_docker_is_asked() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let f = real();
    // The shape a real project arrives in: a bind mount for the database's
    // data directory, which an isolated copy would write into the worktree.
    std::fs::write(
        f.paths.root().join("docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16-alpine\n    \
         ports: [\"5432:5432\"]\n    volumes:\n      - ./pgdata:/var/lib/postgresql/data\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    common::git(f.paths.root(), &["add", "."]);
    common::git(f.paths.root(), &["commit", "--quiet", "-m", "bind mount"]);
    let name = actions::new(&f.paths, &f.config, "feat/three", None, &|_| {}).unwrap();

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
    assert!(err.contains("./pgdata"), "{err}");
    assert!(err.contains("named volume"), "{err}");
    let project = compose::project_name(f.paths.project_id(), &name);
    let made = items_of("container", std::slice::from_ref(&project));
    assert!(
        made.is_empty(),
        "nothing was started for a worktree pando refused: {made:?}"
    );
    assert!(worktree_of(&f, &name).is_dir());
    let _ = state::load(&f.paths.state_file());
    let _ = Duration::from_secs(0);
}

/// The published port answers from the moment the container is running.
/// Only a real Docker has that proxy in front of it, so only a real Docker
/// can prove that readiness is not satisfied by it.
#[test]
fn a_container_that_publishes_a_port_and_never_listens_is_never_ready() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real_tuned(&listener_on_port_env(), 8);
    std::fs::write(
        f.paths.root().join("docker-compose.yml"),
        "services:\n  postgres:\n    image: alpine:latest\n    \
         command: [\"sleep\", \"300\"]\n    ports: [\"5432:5432\"]\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    common::git(f.paths.root(), &["add", "."]);
    common::git(f.paths.root(), &["commit", "--quiet", "-m", "a sleeper"]);
    let name = actions::new(&f.paths, &f.config, "feat/sleeper", None, &|_| {}).unwrap();
    let project = compose::project_name(f.paths.project_id(), &name);
    f.projects.push(project.clone());

    let started = std::time::Instant::now();
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|m| { eprintln!("sleeper: {m}") }
        )
        .unwrap_err()
    );
    assert!(err.contains("did not become ready"), "{err}");
    assert!(
        err.contains("postgres"),
        "it names the one that was not: {err}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(7),
        "it waited out the timeout rather than giving up early: {:?}",
        started.elapsed()
    );

    // What did come up is stopped again, so a failed start leaves nothing
    // holding a port.
    let running = published_ports(&f.projects);
    assert!(
        running.is_empty(),
        "the services were left running: {running:?}"
    );
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(
        store.worktrees[&name].processes.is_empty(),
        "and nothing was spawned behind them"
    );
}

/// Only compose resolves `extends:` and a top-level `include:`. pando's own
/// reader sees a service with no image and no ports, and used to refuse it
/// by telling the developer to add a `ports:` entry their file already has.
#[test]
fn a_compose_file_that_extends_another_is_resolved_by_compose_itself() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real();
    let root = f.paths.root().to_path_buf();
    std::fs::write(
        root.join("base.yml"),
        "services:\n  pg-template:\n    image: postgres:16-alpine\n    \
         environment:\n      POSTGRES_PASSWORD: pando\n    ports: [\"5432:5432\"]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("extra.yml"),
        "services:\n  extra:\n    image: redis:7\n    ports: [\"6390:6379\"]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("docker-compose.yml"),
        "include:\n  - extra.yml\n\
         services:\n  \
         postgres:\n    extends:\n      file: base.yml\n      service: pg-template\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    common::git(&root, &["add", "."]);
    common::git(&root, &["commit", "--quiet", "-m", "extends and include"]);

    // What pando's own reader makes of it: a service it cannot place.
    let parsed = compose::read(&root.join("docker-compose.yml")).unwrap();
    assert_eq!(parsed.unresolved.extends, vec!["postgres".to_string()]);
    assert!(parsed.unresolved.include);
    assert_eq!(parsed.services["postgres"].container_port(), None);

    let name = actions::new(&f.paths, &f.config, "feat/ext", None, &|_| {}).unwrap();
    let project = compose::project_name(f.paths.project_id(), &name);
    f.projects.push(project.clone());

    let report = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("extends: {m}"),
    )
    .unwrap();
    for role in ["postgres", "redis"] {
        assert!(
            pando::ports::something_is_listening(report.ports[role]),
            "{role} is up on {}",
            report.ports[role]
        );
    }
    // The `include:` brought `extra` into the file, and it is not in
    // `include = [...]`, so nothing started it.
    let mine = published_ports(&f.projects);
    assert_eq!(mine.len(), 2, "only what `include` asked for: {mine:?}");
}

/// `up -d <name>` enables that service's profile implicitly; a `stop` with
/// the same `-f` files does not. Only a real compose has profiles, so only
/// a real compose can prove the cleanup reaches one.
#[test]
fn a_profiled_service_is_stopped_too_when_readiness_fails() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real_tuned(&listener_on_port_env(), 6);
    // `postgres` never listens, so readiness fails; `redis` comes up and
    // is behind a profile.
    std::fs::write(
        f.paths.root().join("docker-compose.yml"),
        "services:\n  postgres:\n    image: alpine:latest\n    \
         command: [\"sleep\", \"300\"]\n    ports: [\"5432:5432\"]\n  \
         redis:\n    image: redis:7\n    profiles: [\"extra\"]\n    \
         ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    common::git(f.paths.root(), &["add", "."]);
    common::git(f.paths.root(), &["commit", "--quiet", "-m", "a profile"]);
    let name = actions::new(&f.paths, &f.config, "feat/profile", None, &|_| {}).unwrap();
    let project = compose::project_name(f.paths.project_id(), &name);
    f.projects.push(project.clone());

    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Isolated,
            &|m| { eprintln!("profile: {m}") }
        )
        .unwrap_err()
    );
    assert!(err.contains("did not become ready"), "{err}");

    let left = published_ports(&f.projects);
    assert!(
        left.is_empty(),
        "a failed start leaves nothing running, profiles included: {left:?}"
    );
}

/// The whole reason finding 3 matters: the detected `migrate` hook runs at
/// the `services` point, right after readiness, and a postgres that has
/// only just been created spends a second or two running `initdb` with
/// nothing listening. If readiness is satisfied by the port proxy, the
/// migration runs against a database that refuses connections.
#[test]
fn the_database_accepts_connections_by_the_time_start_returns() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real();
    let name = actions::new(&f.paths, &f.config, "feat/ready", None, &|_| {}).unwrap();
    let project = compose::project_name(f.paths.project_id(), &name);
    f.projects.push(project.clone());

    // A fresh named volume, so this postgres really does have to
    // initialise before it will answer anything.
    actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|m| eprintln!("ready: {m}"),
    )
    .unwrap();

    // Asked the instant `start` returns, which is the instant a hook at
    // the `services` point would have run.
    let out = Command::new("docker")
        .args(["exec", &format!("{project}-postgres-1"), "pg_isready"])
        .output()
        .expect("run pg_isready in the container");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && said.contains("accepting connections"),
        "start returned before postgres was accepting connections: {said}"
    );
}

/// The one the fake docker can never catch: it mounts nothing, so only a
/// real container can prove that an absolute bind source really does land
/// in the developer's own checkout.
#[test]
fn an_absolute_bind_mount_into_the_checkout_is_refused_and_writes_nothing() {
    if !enabled() {
        eprintln!("skipping: set PANDO_TEST_DOCKER=1 to run against the real Docker");
        return;
    }
    let mut f = real();
    let inside = f.paths.root().join("data").join("pg");
    // An absolute path does not move with the worktree: this one is the
    // main checkout, and the container would write straight into it.
    std::fs::write(
        f.paths.root().join("docker-compose.yml"),
        format!(
            "services:\n  postgres:\n    image: alpine:latest\n    \
             command: [\"sh\",\"-c\",\"touch /mnt/pando-wrote-into-your-repo && sleep 120\"]\n    \
             ports: [\"5432:5432\"]\n    volumes:\n      - {}:/mnt\n  \
             redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
            inside.display()
        ),
    )
    .unwrap();
    common::git(f.paths.root(), &["add", "."]);
    common::git(
        f.paths.root(),
        &["commit", "--quiet", "-m", "absolute bind"],
    );
    assert_eq!(common::status_porcelain(f.paths.root()), "");
    let name = actions::new(&f.paths, &f.config, "feat/abs", None, &|_| {}).unwrap();
    // Registered before the start, so the guard takes anything down that a
    // regression here would leave behind.
    f.projects
        .push(compose::project_name(f.paths.project_id(), &name));

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
    assert!(err.contains(&inside.display().to_string()), "{err}");
    assert!(
        err.contains("named volume"),
        "it says what to change: {err}"
    );

    let made = items_of("container", &f.projects);
    assert!(
        made.is_empty(),
        "nothing was started for a worktree pando refused: {made:?}"
    );
    assert!(
        !inside.join("pando-wrote-into-your-repo").exists(),
        "pando never writes into your repository"
    );
    assert_eq!(
        common::status_porcelain(f.paths.root()),
        "",
        "and the main checkout is untouched"
    );
    assert_eq!(common::status_porcelain(&worktree_of(&f, &name)), "");
}
