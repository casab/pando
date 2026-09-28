//! Queue workers: the libraries whose worker process takes jobs off a
//! queue, and how a process that runs one is recognised.
//!
//! One row per library. `doctor` reads these to say when every worktree's
//! worker would consume the same queue: in a shared start each of them is
//! pointed at the main checkout's Redis, and a job one worktree enqueues
//! can run on another worktree's code.

/// One queue library.
#[derive(Debug, Clone, Copy)]
pub struct QueueWorker {
    pub name: &'static str,
    /// The package a project declares to use it.
    pub package: &'static str,
    /// The manifests that package is declared in.
    pub manifests: &'static [&'static str],
    /// Commands that run its worker, each as the words that must all be
    /// in the command line: `celery -A proj worker` is Celery's, and
    /// `celery -A proj beat` is not. Empty for a library whose worker is
    /// the project's own script, as BullMQ's is.
    pub commands: &'static [&'static [&'static str]],
    /// Whether its queue lives in Redis and nowhere else. Celery's broker
    /// may as well be RabbitMQ, so only a project that addresses Redis
    /// shares a Celery queue through it.
    pub redis_only: bool,
}

const PYTHON: &[&str] = &["pyproject.toml", "requirements.txt"];

pub const QUEUE_WORKERS: [QueueWorker; 5] = [
    QueueWorker {
        name: "ARQ",
        package: "arq",
        manifests: PYTHON,
        commands: &[&["arq"]],
        redis_only: true,
    },
    QueueWorker {
        name: "Celery",
        package: "celery",
        manifests: PYTHON,
        commands: &[&["celery", "worker"]],
        redis_only: false,
    },
    QueueWorker {
        name: "RQ",
        package: "rq",
        manifests: PYTHON,
        commands: &[&["rq", "worker"], &["rqworker"]],
        redis_only: true,
    },
    QueueWorker {
        name: "Sidekiq",
        package: "sidekiq",
        manifests: &["Gemfile"],
        commands: &[&["sidekiq"]],
        redis_only: true,
    },
    QueueWorker {
        name: "BullMQ",
        package: "bullmq",
        manifests: &["package.json"],
        commands: &[],
        redis_only: true,
    },
];

/// The words of a command line or a manifest, as the rows above compare
/// them: runs of letters, digits, `_` and `-`, so `src.scripts.worker`
/// is `src`, `scripts` and `worker`, and `"arq>=0.26"` is `arq`, `0` and
/// `26`.
pub fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|word| !word.is_empty())
}

/// The library whose worker `cmd` runs by its own command.
pub fn run_by(cmd: &str) -> Option<&'static QueueWorker> {
    let said: Vec<&str> = words(cmd).collect();
    QUEUE_WORKERS.iter().find(|worker| {
        worker
            .commands
            .iter()
            .any(|command| command.iter().all(|word| said.contains(word)))
    })
}

/// Whether a process's name or command calls it a worker: `worker`,
/// `python -m app.worker`, `node dist/queue-worker.js`. Only worth asking
/// of a project that declares one of the libraries above, since a worker
/// is what runs their jobs when no command of theirs does.
pub fn named_a_worker(name: &str, cmd: &str) -> bool {
    words(name)
        .chain(words(cmd))
        .any(|word| word.to_ascii_lowercase().contains("worker"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_is_known_by_its_own_command() {
        let name = |cmd: &str| run_by(cmd).map(|worker| worker.name);
        assert_eq!(name("uv run arq app.worker.WorkerSettings"), Some("ARQ"));
        assert_eq!(name("celery -A proj worker -l info"), Some("Celery"));
        assert_eq!(name("celery -A proj beat"), None);
        assert_eq!(name("rq worker high default"), Some("RQ"));
        assert_eq!(
            name("bundle exec sidekiq -C config/sidekiq.yml"),
            Some("Sidekiq")
        );
        assert_eq!(name("pnpm dev"), None);
        assert_eq!(name("uv run uvicorn app.main:app"), None, "no word of it");
    }

    #[test]
    fn a_process_named_a_worker_is_one_by_its_name_or_its_command() {
        assert!(named_a_worker("worker", "npm run jobs"));
        assert!(named_a_worker(
            "jobs",
            "uv run python -m src.scripts.worker"
        ));
        assert!(named_a_worker("jobs", "node dist/queue-worker.js"));
        assert!(!named_a_worker("api", "uv run uvicorn app.main:app"));
    }

    #[test]
    fn library_names_are_unique() {
        for (i, worker) in QUEUE_WORKERS.iter().enumerate() {
            assert!(
                QUEUE_WORKERS[i + 1..]
                    .iter()
                    .all(|other| other.name != worker.name && other.package != worker.package),
                "{} is listed twice",
                worker.name
            );
        }
    }
}
