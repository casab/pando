//! Queue workers on a shared Redis: a note in the services section.

use std::path::Path;

use crate::catalog::queue_workers::{self, QUEUE_WORKERS, QueueWorker};
use crate::config::{Config, ServiceConfig};
use crate::detect;
use crate::paths::PandoPaths;

use super::report::{Finding, Section};

/// The recipe whose server holds the queues the rows in
/// [`QUEUE_WORKERS`] take their jobs from.
const REDIS: &str = "redis";

/// A note for each process that looks like a queue worker in a project
/// that queues on Redis.
///
/// A shared start points every worktree at the main checkout's servers,
/// so every worktree's worker takes jobs off the same queue, and a job one
/// worktree enqueues can run on another worktree's code — or on main's.
/// Nothing is broken until it is, which is why it is a note and not a
/// problem, and why doctor says it before an afternoon goes on it.
///
/// A process is a worker when its command runs one, `celery -A app
/// worker`, or when its name or command says `worker` in a project that
/// declares one of the libraries: `python -m app.worker` beside `arq` in
/// the manifest.
pub(super) fn shared_queue_findings(
    paths: &PandoPaths,
    config: &Config,
    findings: &mut Vec<Finding>,
) {
    let root = paths.root();
    let mut root_redis: Option<bool> = None;
    for (name, process) in &config.processes {
        let dir = match &process.cwd {
            Some(cwd) => root.join(cwd),
            None => root.to_path_buf(),
        };
        let worker = queue_workers::run_by(&process.cmd).or_else(|| {
            queue_workers::named_a_worker(name, &process.cmd)
                .then(|| {
                    QUEUE_WORKERS
                        .iter()
                        .find(|worker| declares(root, worker) || declares(&dir, worker))
                })
                .flatten()
        });
        let Some(worker) = worker else {
            continue;
        };
        let on_redis = worker.redis_only
            || *root_redis.get_or_insert_with(|| {
                detect::addressed_engines(root).contains(&REDIS) || runs_redis(config)
            })
            || detect::addressed_engines(&dir).contains(&REDIS);
        if !on_redis {
            continue;
        }
        findings.push(
            Finding::note(
                Section::Services,
                format!(
                    "the process {name:?} takes {} jobs off a queue, and a shared start points \
                     every worktree's worker at the same Redis — a job one worktree enqueues can \
                     run on another worktree's code",
                    worker.name
                ),
            )
            .with_fix(format!(
                "start a worktree with `--namespaced` or `--isolated` to give it a Redis of \
                 its own, or keep the worker to one worktree: `pando stop <worktree> --only \
                 {name}` in the others"
            )),
        );
    }
}

/// Whether one of the worker's manifests in `dir` names its package as a
/// word of its own: `"arq>=0.26"`, `gem 'sidekiq'`, `"bullmq": "^5"`.
fn declares(dir: &Path, worker: &QueueWorker) -> bool {
    worker.manifests.iter().any(|manifest| {
        std::fs::read_to_string(dir.join(manifest)).is_ok_and(|text| {
            queue_workers::words(&text).any(|word| word.eq_ignore_ascii_case(worker.package))
        })
    })
}

/// Whether config runs a native Redis of the project's own for isolated
/// starts: the project talks to Redis, whether or not its env example
/// says so.
fn runs_redis(config: &Config) -> bool {
    config.services.iter().any(|service| match service {
        ServiceConfig::Native { name, preset, .. } => preset.as_deref().unwrap_or(name) == REDIS,
        ServiceConfig::Compose { .. } => false,
    })
}
