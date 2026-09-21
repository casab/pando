//! The compose adapter against the fake docker.
//!
//! The full round: bring services up, wait for them, pump their logs into
//! a file, stop them, and take them down with their volumes — with every
//! invocation the adapter made asserted exactly, because the difference
//! between `down` and `down -v` is a database a developer wanted kept.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use common::docker;
use pando::compose::{self, Published};
use pando::process;
use pando::services::{self, Compose, Wanted};
use tempfile::TempDir;

const PROJECT: &str = "pando-fixture-1234abcd-feat-one";

struct Harness {
    _dir: TempDir,
    home: PathBuf,
    worktree: PathBuf,
    compose: Compose,
    /// Every log pump this test started, so none of them outlives it.
    pumps: Vec<i32>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for pgid in &self.pumps {
            let _ = process::stop(*pgid, Duration::from_secs(2));
        }
        // Whatever the assertions did, the fake's listeners go.
        let _ = self.compose.down_with_volumes();
    }
}

fn harness(published: &[Published]) -> Harness {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("pando-home");
    let worktree = dir.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let program = docker::install(&home);

    std::fs::write(
        worktree.join("docker-compose.yml"),
        "services:\n  postgres:\n    image: postgres:16\n    ports: [\"5432:5432\"]\n  \
         redis:\n    image: redis:7\n    ports: [\"6379:6379\"]\n",
    )
    .unwrap();
    let override_dir = home.join("compose");
    std::fs::create_dir_all(&override_dir).unwrap();
    let override_file = override_dir.join("feat+one.override.yml");
    std::fs::write(
        &override_file,
        compose::render_override("feat+one", published),
    )
    .unwrap();

    let compose = Compose::new(
        program,
        PROJECT,
        vec![worktree.join("docker-compose.yml"), override_file],
        &worktree,
    );
    Harness {
        _dir: dir,
        home,
        worktree,
        compose,
        pumps: Vec::new(),
    }
}

/// Two ports nothing else is using, below the ephemeral floor so no
/// parallel test's `bind(0)` can land on them.
///
/// A window per test rather than "the first two that are free": every test
/// in this file runs at the same moment, and a freeness probe answers for
/// the instant it ran, so two of them would pick the same pair and the
/// second fake's listener would fail to bind.
fn free_pair() -> (u16, u16) {
    let slot = SLOT.fetch_add(1, Ordering::SeqCst);
    let mut base = 24_000 + ((std::process::id() % 1000) as u16) * 4 + slot * 4;
    for _ in 0..40 {
        if pando::ports::is_port_free(base) && pando::ports::is_port_free(base + 1) {
            return (base, base + 1);
        }
        base += 137;
    }
    panic!("no free pair of ports for the fake docker");
}

static SLOT: AtomicU16 = AtomicU16::new(0);

fn published_pair() -> (Vec<Published>, u16, u16) {
    let (pg, redis) = free_pair();
    (
        vec![
            Published {
                service: "postgres".into(),
                container: 5432,
                host: pg,
            },
            Published {
                service: "redis".into(),
                container: 6379,
                host: redis,
            },
        ],
        pg,
        redis,
    )
}

#[test]
fn up_wait_pump_stop_and_down_make_exactly_the_invocations_they_should() {
    let (published, pg, redis) = published_pair();
    let mut h = harness(&published);

    h.compose
        .up(&["postgres".to_string(), "redis".to_string()])
        .unwrap();
    // Readiness by connect: the fake really bound those ports.
    services::wait_ready(
        &h.compose,
        &[
            Wanted {
                service: "postgres".into(),
                port: pg,
                healthcheck: false,
            },
            Wanted {
                service: "redis".into(),
                port: redis,
                healthcheck: false,
            },
        ],
        Duration::from_secs(10),
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        docker::services_up(&h.home, PROJECT),
        vec![("postgres".to_string(), pg), ("redis".to_string(), redis)]
    );

    // The pump is a detached process group, like every other child pando
    // owns, so `stop` and the orphan sweep already cover it.
    let log = h.home.join("postgres.log");
    let spawned = process::spawn_detached(process::SpawnOptions {
        shell_cmd: &h.compose.logs_shell_cmd("postgres"),
        cwd: &h.worktree,
        log_file: &log,
        env: &[],
    })
    .unwrap();
    h.pumps.push(spawned.pgid);
    let mut text = String::new();
    for _ in 0..80 {
        text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("fake docker log for postgres") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(text.contains("fake docker log for postgres"), "{text:?}");

    h.compose.stop().unwrap();
    assert!(
        docker::services_up(&h.home, PROJECT).is_empty(),
        "stop leaves nothing running"
    );
    assert!(
        !docker::was_downed(&h.home, PROJECT),
        "stop must not take the volumes with it"
    );

    h.compose.down_with_volumes().unwrap();
    assert!(docker::was_downed(&h.home, PROJECT));

    let seen = docker::invocations_for(&h.home, PROJECT);
    // `ps` is the one invocation whose count depends on how fast the
    // containers came up, so it is asserted separately; everything else is
    // exact, because the difference between `down` and `down -v` is a
    // database somebody wanted kept.
    let (checks, acted): (Vec<String>, Vec<String>) =
        seen.iter().cloned().partition(|line| line.contains(" ps "));
    assert_eq!(
        acted,
        vec![
            format!(
                "compose -p {PROJECT} -f docker-compose.yml -f feat+one.override.yml up -d postgres redis"
            ),
            format!(
                "compose -p {PROJECT} -f docker-compose.yml -f feat+one.override.yml logs -f --no-color postgres"
            ),
            format!("compose -p {PROJECT} -f docker-compose.yml -f feat+one.override.yml stop"),
            format!("compose -p {PROJECT} -f docker-compose.yml -f feat+one.override.yml down -v"),
        ],
    );
    assert!(
        !checks.is_empty(),
        "even a connect-based wait asks `ps` on a slow beat, so a container \
         that died is noticed rather than waited out: {seen:?}"
    );
}

#[test]
fn readiness_goes_through_health_when_the_service_declares_one() {
    let (published, pg, _) = published_pair();
    let h = harness(&published);
    docker::with_healthcheck(&h.home, PROJECT, &["postgres"]);
    h.compose.up(&["postgres".to_string()]).unwrap();

    services::wait_ready(
        &h.compose,
        &[Wanted {
            service: "postgres".into(),
            // Deliberately a port nothing is on: health is the answer, and
            // a connect probe would have to fail for this to be honest.
            port: 1,
            healthcheck: true,
        }],
        Duration::from_secs(10),
        &|_| {},
    )
    .unwrap();

    let seen = docker::invocations_for(&h.home, PROJECT);
    assert!(
        seen.iter().any(|line| line.contains("ps --all --format")),
        "health is read from `docker compose ps`: {seen:?}"
    );
    let _ = pg;
}

#[test]
fn a_service_that_never_becomes_ready_fails_with_its_name_and_the_timeout() {
    let (published, pg, _) = published_pair();
    let h = harness(&published);
    docker::never_ready(&h.home, PROJECT);
    h.compose.up(&["postgres".to_string()]).unwrap();

    let err = format!(
        "{:#}",
        services::wait_ready(
            &h.compose,
            &[Wanted {
                service: "postgres".into(),
                port: pg,
                healthcheck: false,
            }],
            Duration::from_millis(600),
            &|_| {},
        )
        .unwrap_err()
    );
    assert!(err.contains("postgres"), "{err}");
    assert!(err.contains("did not become ready in 1s"), "{err}");
}

#[test]
fn a_second_up_leaves_a_container_that_is_already_running_alone() {
    let (published, pg, _) = published_pair();
    let h = harness(&published);
    h.compose.up(&["postgres".to_string()]).unwrap();
    let first = docker::services_up(&h.home, PROJECT);
    h.compose.up(&["postgres".to_string()]).unwrap();
    assert_eq!(
        docker::services_up(&h.home, PROJECT),
        first,
        "an idempotent up is what makes a plain start beside live services safe"
    );
    assert_eq!(first, vec![("postgres".to_string(), pg)]);
}

#[test]
fn everything_the_fake_writes_lives_under_pandos_home() {
    let (published, _, _) = published_pair();
    let h = harness(&published);
    h.compose.up(&["postgres".to_string()]).unwrap();
    // Nothing next to the worktree: the shim's markers are pando's files,
    // and Invariant 1 covers them like any other.
    let entries: Vec<String> = std::fs::read_dir(&h.worktree)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["docker-compose.yml".to_string()]);
    assert!(docker::state_dir(&h.home).join(PROJECT).is_dir());
}
