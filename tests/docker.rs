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

mod common;

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

/// Names of everything docker has that belongs to pando, by kind.
fn pando_items(kind: &str) -> Vec<String> {
    let args: Vec<&str> = match kind {
        "container" => vec!["ps", "-a", "--format", "{{.Names}}"],
        "volume" => vec!["volume", "ls", "--format", "{{.Name}}"],
        _ => vec!["network", "ls", "--format", "{{.Name}}"],
    };
    docker(&args)
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(PREFIX))
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
        let _ = actions::stop_all(&self.paths);
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
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\", \"redis\"]\n\
             env = {{ DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }}\n\
             ready_timeout_s = 90\n",
            dev
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

/// `<container> <ports>` for one compose project, sorted, so two samples
/// compare as one value.
fn published_ports(project: &str) -> Vec<String> {
    let mut out: Vec<String> = docker(&["ps", "--format", "{{.Names}} {{.Ports}}"])
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(project))
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

    let first = actions::start(&f.paths, &f.config, &one, None, true, &|m| {
        eprintln!("first: {m}")
    })
    .unwrap();
    let before = published_ports(&f.projects[0]);
    assert_eq!(before.len(), 2, "postgres and redis are up: {before:?}");

    let second = actions::start(&f.paths, &f.config, &one, None, true, &|m| {
        eprintln!("second: {m}")
    })
    .unwrap();
    assert_eq!(second.ports, first.ports, "every port stays where it was");
    assert!(!second.reassigned, "and nothing is reported as moved");
    assert_eq!(
        published_ports(&f.projects[0]),
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
    let before = pando_items("volume");
    let mut f = real();
    let one = actions::new(&f.paths, &f.config, "feat/one", None, &|_| {}).unwrap();
    let two = actions::new(&f.paths, &f.config, "feat/two", None, &|_| {}).unwrap();
    f.projects
        .push(compose::project_name(f.paths.project_id(), &one));
    f.projects
        .push(compose::project_name(f.paths.project_id(), &two));

    let a = actions::start(&f.paths, &f.config, &one, None, true, &|m| {
        eprintln!("one: {m}")
    })
    .unwrap();
    let b = actions::start(&f.paths, &f.config, &two, None, true, &|m| {
        eprintln!("two: {m}")
    })
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
    let published = docker(&["ps", "--format", "{{.Names}} {{.Ports}}"]);
    let mine: Vec<&str> = published
        .lines()
        .filter(|line| line.starts_with(PREFIX))
        .collect();
    assert_eq!(mine.len(), 4, "four containers, two per worktree: {mine:?}");
    assert!(
        !mine.iter().any(|line| line.contains(":5432->")),
        "the project's own port must not be published: {mine:?}"
    );

    // And each worktree's data is its own: the project name prefixes the
    // named volumes, so there are two distinct sets.
    let volumes = pando_items("volume");
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
    actions::stop(&f.paths, &one, None).unwrap();
    assert!(
        pando::ports::something_is_listening(b.ports["postgres"]),
        "stopping one worktree must not touch another's services"
    );
    assert!(!pando::ports::something_is_listening(a.ports["postgres"]));

    // And `rm` takes the containers, the network and the volumes with it.
    actions::rm(&f.paths, &one, false, true).unwrap();
    actions::rm(&f.paths, &two, false, true).unwrap();
    for kind in ["container", "volume", "network"] {
        let left: Vec<String> = pando_items(kind)
            .into_iter()
            .filter(|name| f.projects.iter().any(|p| name.starts_with(p)))
            .collect();
        assert!(left.is_empty(), "{kind}s left behind: {left:?}");
    }
    assert_eq!(
        pando_items("volume")
            .into_iter()
            .filter(|v| !before.contains(v))
            .collect::<Vec<_>>(),
        Vec::<String>::new(),
        "no volume of pando's outlived the test"
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
    );
    assert!(err.contains("./pgdata"), "{err}");
    assert!(err.contains("named volume"), "{err}");
    let project = compose::project_name(f.paths.project_id(), &name);
    assert!(
        !pando_items("container")
            .iter()
            .any(|c| c.starts_with(&project)),
        "nothing was started for a worktree pando refused"
    );
    assert!(worktree_of(&f, &name).is_dir());
    let _ = state::load(&f.paths.state_file());
    let _ = Duration::from_secs(0);
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
        actions::start(&f.paths, &f.config, &name, None, true, &|_| {}).unwrap_err()
    );
    assert!(err.contains(&inside.display().to_string()), "{err}");
    assert!(
        err.contains("named volume"),
        "it says what to change: {err}"
    );

    let project = compose::project_name(f.paths.project_id(), &name);
    assert!(
        !pando_items("container")
            .iter()
            .any(|c| c.starts_with(&project)),
        "nothing was started for a worktree pando refused"
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
