//! Persistent state: what pando started, what it owns, and what it observed.
//!
//! Two rules are inherited from the origin tool unchanged: every mutation
//! holds the flock, and every read path reconciles before trusting what is
//! in the map. State is v2 from the first commit — there is no v1 to migrate.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const STATE_VERSION: u32 = 2;

/// How long a process may sit in `Starting` before it is called failed.
pub const START_TIMEOUT_SECS: i64 = 30;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub version: u32,
    pub worktrees: BTreeMap<String, WorktreeRecord>,
}

/// Everything pando knows about one worktree. Records exist for worktrees
/// pando created even when nothing is running — `created_by_pando` is what
/// lets `rm` tell an adopted worktree from one of its own.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRecord {
    pub path: PathBuf,
    #[serde(default)]
    pub created_by_pando: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub processes: BTreeMap<String, ProcessRecord>,
    /// Role name to allocated port. Survives a stop: a stopped worktree
    /// still owns its ports.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Which process owns which roles, as config declared them at the last
    /// start. Process name to its roles, in the order it declared them.
    ///
    /// Written for every process of the worktree, whatever `--only` asked
    /// for, and it survives a stop exactly as `ports` does. Without it the
    /// URL rule — the `web` role, else the first role of the alphabetically
    /// first process — is only answerable while something is running, and
    /// `start` and a later `status` would disagree about a worktree that
    /// has since been stopped.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, Vec<String>>,
    /// Every listening socket seen across this worktree's process groups:
    /// the union of the per-process lists below.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, HookRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<ShareRecord>,
}

impl WorktreeRecord {
    /// A record for a worktree with nothing running yet.
    pub fn new(path: impl Into<PathBuf>, created_by_pando: bool) -> Self {
        Self {
            path: path.into(),
            created_by_pando,
            processes: BTreeMap::new(),
            ports: BTreeMap::new(),
            roles: BTreeMap::new(),
            observed_ports: Vec::new(),
            services: Vec::new(),
            hooks: BTreeMap::new(),
            share: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pid: u32,
    pub pgid: i32,
    pub started_at: DateTime<Utc>,
    pub log_path: PathBuf,
    /// The port `advance_phases` watches for the Starting → Running
    /// transition. `None` means "running once alive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_port: Option<u16>,
    /// How long this process may sit in `Starting`. `None` is
    /// [`START_TIMEOUT_SECS`]; a slow first build sets its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_timeout_s: Option<u64>,
    /// The listening sockets seen in *this* process's own group, sorted.
    ///
    /// Per process, not per worktree: a port an api opened for its
    /// debugger is indistinguishable from one the web server opened once
    /// the two are merged into one list, and the URL a worktree hands out
    /// then follows whichever of them nothing claimed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_ports: Vec<u16>,
    pub phase: Phase,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "phase")]
pub enum Phase {
    Starting { since: DateTime<Utc> },
    Running { since: DateTime<Utc> },
    Failed { at: DateTime<Utc>, reason: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Compose,
    Native,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ServiceRecord {
    pub name: String,
    pub kind: ServiceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Native services run as pando's own detached children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<i32>,
    /// Compose services are addressed by their compose project name instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_project: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HookRecord {
    /// Content hash of the hook's fingerprint globs at the last run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub ran_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ShareRecord {
    pub tunnel_pid: u32,
    pub tunnel_pgid: i32,
    pub public_url: String,
    pub local_port: u16,
    pub started_at: DateTime<Utc>,
    pub log_path: PathBuf,
    /// Optional local proxy that injects an auth header into tunnel traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_pgid: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_port: Option<u16>,
}

impl State {
    pub fn new() -> Self {
        Self {
            version: STATE_VERSION,
            worktrees: BTreeMap::new(),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

pub fn load(path: &Path) -> Result<State> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::new()),
        Err(e) => {
            return Err(
                anyhow::Error::from(e).context(format!("read state file {}", path.display()))
            );
        }
    };
    let state: State = serde_json::from_str(&text)
        .with_context(|| format!("parse state file {}", path.display()))?;
    if state.version != STATE_VERSION {
        anyhow::bail!(
            "state file {} is version {}, this pando speaks version {STATE_VERSION} — upgrade \
             pando, or move that file aside to start over (worktrees pando created will then \
             read as adopted)",
            path.display(),
            state.version
        );
    }
    Ok(state)
}

pub fn save(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(state).context("serialize state")?;
    std::fs::write(&tmp, json).with_context(|| format!("write tmp state {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename tmp → {}", path.display()))?;
    Ok(())
}

/// Drops what is no longer running: process records whose pid is gone,
/// native service records whose pid is gone, and a share whose tunnel or
/// proxy has exited. The worktree record itself survives — `created_by_pando`
/// and the port assignment outlive any process.
///
/// One exception: a `Failed` record stays. It is the one phase whose whole
/// purpose is to outlive its process — `refresh` advances phases rather
/// than reconciling precisely so a crashed dev server stays visible until
/// the developer acts on it — and dropping it here erased that from
/// `status` the moment anything else in the project was started or stopped.
/// The mutations that *are* about those processes clear them explicitly:
/// `start` for the ones it starts, `stop` for the ones it stops, `rm` for
/// all of a worktree's.
///
/// Every read path calls this before trusting the map. Returns whether
/// anything changed, so a caller holding the lock knows to save.
pub fn reconcile(state: &mut State, is_alive: impl Fn(u32) -> bool) -> bool {
    let mut changed = false;
    for rec in state.worktrees.values_mut() {
        let before = rec.processes.len();
        rec.processes
            .retain(|_, p| is_alive(p.pid) || matches!(p.phase, Phase::Failed { .. }));
        changed |= rec.processes.len() != before;

        let before = rec.services.len();
        rec.services
            .retain(|s| s.pid.map(&is_alive).unwrap_or(true));
        changed |= rec.services.len() != before;

        // Nothing is up, whatever records are left, so the worktree is not
        // listening on anything.
        if rec
            .processes
            .values()
            .any(|p| matches!(p.phase, Phase::Starting { .. } | Phase::Running { .. }))
        {
            continue;
        }
        if !rec.observed_ports.is_empty() {
            rec.observed_ports.clear();
            changed = true;
        }
    }
    changed |= sweep_dead_shares(state, &is_alive);
    changed
}

/// A share is only useful while both halves live: a tunnel whose proxy died
/// serves the wrong thing, and a proxy whose tunnel died is unreachable.
fn sweep_dead_shares(state: &mut State, is_alive: &impl Fn(u32) -> bool) -> bool {
    let mut changed = false;
    for rec in state.worktrees.values_mut() {
        let Some(share) = &rec.share else { continue };
        let tunnel_dead = !is_alive(share.tunnel_pid);
        let proxy_dead = share.proxy_pid.map(|p| !is_alive(p)).unwrap_or(false);
        if tunnel_dead || proxy_dead {
            rec.share = None;
            changed = true;
        }
    }
    changed
}

/// Moves each process through Starting → Running → Failed.
///
/// `port_bound(pgid, port)` answers "has this process group opened that
/// port yet?" and is injected: the honest answer comes from scanning the
/// group's own sockets, and the one thing it must never do is *bind* the
/// port to find out, which would hand the server being waited for an
/// `EADDRINUSE`. A process that declared no `ready_port` is Running as soon
/// as it is alive.
pub fn advance_phases(
    state: &mut State,
    is_alive: impl Fn(u32) -> bool,
    port_bound: impl Fn(i32, u16) -> bool,
) -> bool {
    let now = Utc::now();
    let mut changed = false;
    for rec in state.worktrees.values_mut() {
        for proc in rec.processes.values_mut() {
            match &proc.phase {
                Phase::Starting { since } => {
                    let timeout = proc
                        .ready_timeout_s
                        .map(|s| s as i64)
                        .unwrap_or(START_TIMEOUT_SECS);
                    if !is_alive(proc.pid) {
                        proc.phase = Phase::Failed {
                            at: now,
                            reason: "process exited".into(),
                        };
                        changed = true;
                    } else if proc
                        .ready_port
                        .map(|p| port_bound(proc.pgid, p))
                        .unwrap_or(true)
                    {
                        proc.phase = Phase::Running { since: now };
                        changed = true;
                    } else if now.signed_duration_since(*since).num_seconds() > timeout {
                        // The watched port is the whole content of this
                        // failure: "timeout" alone tells nobody what pando
                        // was waiting for.
                        let port = proc
                            .ready_port
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        proc.phase = Phase::Failed {
                            at: now,
                            reason: format!("timeout: nothing bound port {port} in {timeout}s"),
                        };
                        changed = true;
                    }
                }
                Phase::Running { .. } => {
                    if !is_alive(proc.pid) {
                        proc.phase = Phase::Failed {
                            at: now,
                            reason: "process exited".into(),
                        };
                        changed = true;
                    }
                }
                Phase::Failed { .. } => {}
            }
        }
    }
    changed |= sweep_dead_shares(state, &is_alive);
    changed
}

/// A worktree's phase, aggregated across every process it runs.
///
/// A worktree is one thing in a list and one row in the TUI, however many
/// processes it has, so the several phases have to become one. Failed wins,
/// because a half-running worktree is not running; Starting comes next,
/// because something is still on its way up; only when every process is
/// Running is the worktree Running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aggregate {
    /// Since the *first* process started coming up: that is how long the
    /// worktree as a whole has been starting.
    Starting { since: DateTime<Utc> },
    /// Since the *last* process reached Running: that is the moment the
    /// worktree as a whole became ready.
    Running { since: DateTime<Utc> },
    /// Named, because "failed" without which process is a question rather
    /// than an answer — and carrying that process's own reason, including
    /// the hint the classifier wrote for it.
    Failed {
        process: String,
        at: DateTime<Utc>,
        reason: String,
    },
}

impl Aggregate {
    /// The one word a listing shows.
    pub fn word(&self) -> &'static str {
        match self {
            Aggregate::Starting { .. } => "starting",
            Aggregate::Running { .. } => "running",
            Aggregate::Failed { .. } => "failed",
        }
    }

    /// When this phase began.
    pub fn since(&self) -> DateTime<Utc> {
        match self {
            Aggregate::Starting { since } | Aggregate::Running { since } => *since,
            Aggregate::Failed { at, .. } => *at,
        }
    }

    /// Why a worktree is failed, with the process it happened to in front
    /// of it. `None` for anything that has not failed.
    pub fn reason(&self) -> Option<String> {
        match self {
            Aggregate::Failed {
                process, reason, ..
            } => Some(format!("{process}: {reason}")),
            _ => None,
        }
    }
}

/// The phase of a whole worktree, or `None` when it is running nothing.
///
/// The failure reported is the *earliest* one: with a web process that died
/// because its api never came up, the api's failure is the one that
/// explains the worktree, and the classifier's hint for it is the one worth
/// showing. Ties go to the first name in order, so the answer is stable
/// across reads.
pub fn aggregate_phase(record: &WorktreeRecord) -> Option<Aggregate> {
    if record.processes.is_empty() {
        return None;
    }
    let mut failed: Option<(&str, DateTime<Utc>, &str)> = None;
    let mut earliest_start: Option<DateTime<Utc>> = None;
    let mut latest_running: Option<DateTime<Utc>> = None;
    for (name, proc) in &record.processes {
        match &proc.phase {
            Phase::Failed { at, reason } => {
                if failed.is_none_or(|(_, first, _)| *at < first) {
                    failed = Some((name.as_str(), *at, reason.as_str()));
                }
            }
            Phase::Starting { since } => {
                earliest_start = Some(earliest_start.map_or(*since, |e| e.min(*since)));
            }
            Phase::Running { since } => {
                latest_running = Some(latest_running.map_or(*since, |l| l.max(*since)));
            }
        }
    }
    if let Some((process, at, reason)) = failed {
        return Some(Aggregate::Failed {
            process: process.to_string(),
            at,
            reason: reason.to_string(),
        });
    }
    if let Some(since) = earliest_start {
        return Some(Aggregate::Starting { since });
    }
    Some(Aggregate::Running {
        since: latest_running.expect("a record with processes is in one of the three phases"),
    })
}

pub struct StateLock {
    _file: std::fs::File,
}

pub fn lock(lock_path: &Path) -> Result<StateLock> {
    let file = open_lock_file(lock_path)?;
    let ret = unsafe { libc::flock(fd(&file), libc::LOCK_EX) };
    if ret != 0 {
        anyhow::bail!(
            "flock on {}: {}",
            lock_path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(StateLock { _file: file })
}

pub fn try_lock(lock_path: &Path) -> Result<Option<StateLock>> {
    let file = open_lock_file(lock_path)?;
    let ret = unsafe { libc::flock(fd(&file), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(Some(StateLock { _file: file }));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // EWOULDBLOCK and EAGAIN are the same value on this platform.
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => anyhow::bail!("flock (try) on {}: {}", lock_path.display(), err),
    }
}

fn open_lock_file(lock_path: &Path) -> Result<std::fs::File> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("open lock {}", lock_path.display()))
}

fn fd(file: &std::fs::File) -> i32 {
    use std::os::unix::io::AsRawFd;
    file.as_raw_fd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::tempdir;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 20, hour, 0, 0).unwrap()
    }

    fn process(pid: u32, phase: Phase) -> ProcessRecord {
        ProcessRecord {
            pid,
            pgid: pid as i32,
            started_at: at(9),
            log_path: PathBuf::from("logs/feat+x/dev.log"),
            ready_port: Some(17_000),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            phase,
        }
    }

    fn running(pid: u32) -> ProcessRecord {
        process(pid, Phase::Running { since: at(9) })
    }

    fn starting(pid: u32, since: DateTime<Utc>) -> ProcessRecord {
        process(pid, Phase::Starting { since })
    }

    fn record_with(pid: u32) -> WorktreeRecord {
        let mut rec = WorktreeRecord::new("/abs/feat+x", true);
        rec.processes.insert("dev".into(), running(pid));
        rec.ports.insert("web".into(), 17_000);
        rec
    }

    // ---- the aggregate phase ---------------------------------------------

    /// A worktree running `web` and `api` in the phases given.
    fn two_processes(web: Phase, api: Phase) -> WorktreeRecord {
        let mut rec = WorktreeRecord::new("/abs/feat+x", true);
        rec.processes.insert("web".into(), process(1, web));
        rec.processes.insert("api".into(), process(2, api));
        rec
    }

    fn failed(at_hour: u32, reason: &str) -> Phase {
        Phase::Failed {
            at: at(at_hour),
            reason: reason.to_string(),
        }
    }

    #[test]
    fn a_worktree_running_nothing_has_no_phase() {
        let rec = WorktreeRecord::new("/abs/feat+x", true);
        assert_eq!(aggregate_phase(&rec), None);
    }

    #[test]
    fn one_process_is_its_own_aggregate() {
        let rec = record_with(1);
        assert_eq!(
            aggregate_phase(&rec),
            Some(Aggregate::Running { since: at(9) })
        );
    }

    /// Every combination of two processes' phases, and what the worktree
    /// reads as. Failed beats Starting beats Running: a worktree with a
    /// dead api is not "running", and one still bringing a process up is
    /// not ready.
    #[test]
    fn the_aggregate_of_two_phases_is_the_worst_of_them() {
        let starting = || Phase::Starting { since: at(10) };
        let running = || Phase::Running { since: at(11) };
        let broken = || failed(12, "process exited");
        let cases: [(Phase, Phase, &str); 9] = [
            (running(), running(), "running"),
            (running(), starting(), "starting"),
            (running(), broken(), "failed"),
            (starting(), running(), "starting"),
            (starting(), starting(), "starting"),
            (starting(), broken(), "failed"),
            (broken(), running(), "failed"),
            (broken(), starting(), "failed"),
            (broken(), broken(), "failed"),
        ];
        for (web, api, expected) in cases {
            let rec = two_processes(web.clone(), api.clone());
            let aggregate = aggregate_phase(&rec).expect("two processes have a phase");
            assert_eq!(
                aggregate.word(),
                expected,
                "web {web:?} and api {api:?} should read as {expected}"
            );
        }
    }

    #[test]
    fn a_failed_aggregate_names_the_process_that_failed() {
        let rec = two_processes(
            Phase::Running { since: at(11) },
            failed(12, "process exited — the command was not found"),
        );
        let aggregate = aggregate_phase(&rec).expect("a phase");
        assert_eq!(
            aggregate,
            Aggregate::Failed {
                process: "api".into(),
                at: at(12),
                reason: "process exited — the command was not found".into(),
            }
        );
        assert_eq!(
            aggregate.reason().as_deref(),
            Some("api: process exited — the command was not found"),
            "the hint belongs to the process it was read from"
        );
        assert_eq!(aggregate.since(), at(12));
    }

    // The classifier runs per log, so the reason shown must be the failing
    // process's own — not the first one's in name order.
    #[test]
    fn the_earliest_failure_is_the_one_that_explains_the_worktree() {
        let rec = two_processes(
            failed(13, "timeout: nothing bound port 17000 in 30s"),
            failed(12, "process exited — port 17001 is already in use"),
        );
        let aggregate = aggregate_phase(&rec).expect("a phase");
        assert_eq!(
            aggregate.reason().as_deref(),
            Some("api: process exited — port 17001 is already in use"),
            "the api died first and took the web process with it"
        );
    }

    #[test]
    fn starting_dates_from_the_first_process_and_running_from_the_last() {
        let rec = two_processes(
            Phase::Starting { since: at(9) },
            Phase::Starting { since: at(11) },
        );
        assert_eq!(
            aggregate_phase(&rec),
            Some(Aggregate::Starting { since: at(9) }),
            "the worktree has been coming up since the first one started"
        );

        let rec = two_processes(
            Phase::Running { since: at(9) },
            Phase::Running { since: at(11) },
        );
        assert_eq!(
            aggregate_phase(&rec),
            Some(Aggregate::Running { since: at(11) }),
            "and it was only ready when the last one was"
        );
    }

    #[test]
    fn a_process_still_starting_holds_the_whole_worktree_back() {
        let rec = two_processes(
            Phase::Running { since: at(9) },
            Phase::Starting { since: at(11) },
        );
        assert_eq!(
            aggregate_phase(&rec),
            Some(Aggregate::Starting { since: at(11) })
        );
        assert_eq!(aggregate_phase(&rec).unwrap().reason(), None);
    }

    fn share(tunnel_pid: u32) -> ShareRecord {
        ShareRecord {
            tunnel_pid,
            tunnel_pgid: tunnel_pid as i32,
            public_url: "https://x.trycloudflare.com".into(),
            local_port: 17_000,
            started_at: at(9),
            log_path: PathBuf::from("logs/feat+x/tunnel.log"),
            proxy_pid: None,
            proxy_pgid: None,
            proxy_port: None,
        }
    }

    fn full_state() -> State {
        let mut rec = record_with(4242);
        rec.roles.insert("dev".to_string(), vec!["web".to_string()]);
        rec.observed_ports = vec![17_000, 17_001];
        if let Some(dev) = rec.processes.get_mut("dev") {
            dev.observed_ports = vec![17_000, 17_001];
        }
        rec.services = vec![
            ServiceRecord {
                name: "postgres".into(),
                kind: ServiceKind::Compose,
                port: Some(17_002),
                pid: None,
                pgid: None,
                compose_project: Some("pando-acme-feat+x".into()),
            },
            ServiceRecord {
                name: "redis".into(),
                kind: ServiceKind::Native,
                port: Some(17_003),
                pid: Some(5150),
                pgid: Some(5150),
                compose_project: None,
            },
        ];
        rec.hooks.insert(
            "migrate".into(),
            HookRecord {
                fingerprint: Some("sha256:abc".into()),
                ran_at: at(10),
            },
        );
        rec.share = Some(share(7000));
        let mut state = State::new();
        state.worktrees.insert("feat+x".into(), rec);
        state
    }

    #[test]
    fn a_new_state_is_version_two() {
        assert_eq!(State::new().version, 2);
        assert_eq!(State::default(), State::new());
    }

    #[test]
    fn v2_round_trips_through_json_with_every_record_type() {
        let state = full_state();
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"version\":2"));
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn phases_round_trip_through_json() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", false);
        rec.processes.insert("s".into(), starting(1, at(9)));
        rec.processes.insert("r".into(), running(2));
        rec.processes.insert(
            "f".into(),
            process(
                3,
                Phase::Failed {
                    at: at(11),
                    reason: "timeout".into(),
                },
            ),
        );
        state.worktrees.insert("w".into(), rec);
        let back: State = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn load_of_a_missing_file_is_an_empty_state() {
        let dir = tempdir().unwrap();
        assert_eq!(load(&dir.path().join("nope.json")).unwrap(), State::new());
    }

    #[test]
    fn save_then_load_preserves_state_and_creates_parents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("projects").join("p").join("state.json");
        let state = full_state();
        save(&path, &state).unwrap();
        assert!(path.exists());
        assert_eq!(load(&path).unwrap(), state);
    }

    #[test]
    fn save_overwrites_without_leaking_the_temp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut a = State::new();
        a.worktrees.insert("one".into(), record_with(1));
        save(&path, &a).unwrap();

        let mut b = State::new();
        b.worktrees.insert("two".into(), record_with(2));
        save(&path, &b).unwrap();

        assert_eq!(load(&path).unwrap(), b);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file must not survive the rename"
        );
    }

    #[test]
    fn load_rejects_malformed_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&path).is_err());
    }

    // pando starts at v2; a file from a future (or imagined past) version is
    // an error rather than something to guess at.
    #[test]
    fn load_rejects_another_state_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"version":1,"worktrees":{}}"#).unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            format!("{err:#}").contains("version 1"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn reconcile_drops_dead_process_records_and_keeps_live_ones() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes.insert("dev".into(), running(100));
        rec.processes.insert("api".into(), running(200));
        state.worktrees.insert("w".into(), rec);

        assert!(reconcile(&mut state, |pid| pid == 100));
        let rec = state.worktrees.get("w").unwrap();
        assert!(rec.processes.contains_key("dev"));
        assert!(
            !rec.processes.contains_key("api"),
            "a dead process record must be dropped"
        );
    }

    // Phase 2b review, finding 7. `Failed` is the one phase whose whole
    // purpose is to outlive its process — `refresh` deliberately advances
    // phases rather than reconciling so a crash stays visible — and
    // `reconcile` threw it away on the next mutation of *any* worktree.
    #[test]
    fn reconcile_keeps_a_failed_record_and_drops_the_merely_dead() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes.insert("web".into(), running(100));
        rec.processes
            .insert("api".into(), process(200, failed(12, "process exited")));
        state.worktrees.insert("w".into(), rec);

        assert!(reconcile(&mut state, |_| false));
        let rec = state.worktrees.get("w").unwrap();
        assert!(
            !rec.processes.contains_key("web"),
            "a dead record nobody was told about is bookkeeping"
        );
        assert!(
            rec.processes.contains_key("api"),
            "a crash has to stay visible until the developer acts on it"
        );
        // And the worktree stops claiming to be listening on anything.
        assert!(rec.observed_ports.is_empty());
    }

    #[test]
    fn reconcile_keeps_the_worktree_record_and_its_ports() {
        let mut state = State::new();
        state.worktrees.insert("w".into(), record_with(999_999));
        reconcile(&mut state, |_| false);

        let rec = state.worktrees.get("w").expect("the record survives");
        assert!(rec.processes.is_empty());
        assert!(
            rec.created_by_pando,
            "created_by_pando outlives any process"
        );
        assert_eq!(
            rec.ports.get("web"),
            Some(&17_000),
            "a stopped worktree still owns its ports"
        );
    }

    #[test]
    fn reconcile_is_a_no_op_when_everything_is_alive() {
        let mut state = full_state();
        let before = state.clone();
        assert!(!reconcile(&mut state, |_| true));
        assert_eq!(state, before);
    }

    #[test]
    fn reconcile_drops_native_services_whose_process_died_and_keeps_compose() {
        let mut state = full_state();
        reconcile(&mut state, |pid| pid != 5150);
        let services = &state.worktrees.get("feat+x").unwrap().services;
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "postgres");
    }

    #[test]
    fn reconcile_clears_observed_ports_once_nothing_runs() {
        let mut state = full_state();
        reconcile(&mut state, |_| false);
        assert!(
            state
                .worktrees
                .get("feat+x")
                .unwrap()
                .observed_ports
                .is_empty()
        );
    }

    #[test]
    fn reconcile_clears_a_share_whose_tunnel_died() {
        let mut state = full_state();
        assert!(reconcile(&mut state, |pid| pid != 7000));
        assert!(state.worktrees.get("feat+x").unwrap().share.is_none());
    }

    #[test]
    fn reconcile_clears_a_share_whose_proxy_died_even_when_the_tunnel_lives() {
        let mut state = full_state();
        if let Some(share) = state.worktrees.get_mut("feat+x").unwrap().share.as_mut() {
            share.proxy_pid = Some(999_999);
            share.proxy_pgid = Some(999_999);
            share.proxy_port = Some(17_500);
        }
        reconcile(&mut state, |pid| pid != 999_999);
        assert!(
            state.worktrees.get("feat+x").unwrap().share.is_none(),
            "a tunnel without its proxy serves the wrong thing"
        );
    }

    #[test]
    fn advance_phases_moves_starting_to_running_when_the_ready_port_binds() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes
            .insert("dev".into(), starting(100, Utc::now()));
        state.worktrees.insert("w".into(), rec);

        assert!(advance_phases(&mut state, |_| true, |_, _| true));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        assert!(matches!(phase, Phase::Running { .. }), "got {phase:?}");
    }

    #[test]
    fn a_process_with_no_ready_port_is_running_once_alive() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        let mut proc = starting(100, Utc::now());
        proc.ready_port = None;
        rec.processes.insert("worker".into(), proc);
        state.worktrees.insert("w".into(), rec);

        assert!(advance_phases(&mut state, |_| true, |_, _| false));
        let phase = &state.worktrees["w"].processes["worker"].phase;
        assert!(matches!(phase, Phase::Running { .. }), "got {phase:?}");
    }

    #[test]
    fn advance_phases_moves_starting_to_failed_when_the_process_exits() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes
            .insert("dev".into(), starting(100, Utc::now()));
        state.worktrees.insert("w".into(), rec);

        assert!(advance_phases(&mut state, |_| false, |_, _| false));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        assert!(
            matches!(phase, Phase::Failed { reason, .. } if reason == "process exited"),
            "got {phase:?}"
        );
    }

    #[test]
    fn advance_phases_moves_starting_to_failed_on_timeout() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        let long_ago = Utc::now() - chrono::Duration::seconds(START_TIMEOUT_SECS + 1);
        rec.processes.insert("dev".into(), starting(100, long_ago));
        state.worktrees.insert("w".into(), rec);

        assert!(advance_phases(&mut state, |_| true, |_, _| false));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        // The watched port is the content of this failure: "timeout" alone
        // does not say what pando was waiting for.
        assert!(
            matches!(phase, Phase::Failed { reason, .. }
                if reason.contains("timeout") && reason.contains("17000")),
            "got {phase:?}"
        );
    }

    // A first build can take minutes; a project that says so must not be
    // called failed after the default thirty seconds.
    #[test]
    fn a_process_with_its_own_timeout_is_given_it() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        let elapsed = Utc::now() - chrono::Duration::seconds(START_TIMEOUT_SECS + 5);
        let mut proc = starting(100, elapsed);
        proc.ready_timeout_s = Some(300);
        rec.processes.insert("dev".into(), proc);
        state.worktrees.insert("w".into(), rec);

        assert!(
            !advance_phases(&mut state, |_| true, |_, _| false),
            "still inside its own window, so nothing changes"
        );
        assert!(matches!(
            state.worktrees["w"].processes["dev"].phase,
            Phase::Starting { .. }
        ));

        state
            .worktrees
            .get_mut("w")
            .unwrap()
            .processes
            .get_mut("dev")
            .unwrap()
            .ready_timeout_s = Some(1);
        assert!(advance_phases(&mut state, |_| true, |_, _| false));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        assert!(
            matches!(phase, Phase::Failed { reason, .. } if reason.contains("1s")),
            "the reason names the window it ran out of: {phase:?}"
        );
    }

    #[test]
    fn advance_phases_moves_running_to_failed_when_the_process_dies() {
        let mut state = State::new();
        state.worktrees.insert("w".into(), record_with(100));
        assert!(advance_phases(&mut state, |_| false, |_, _| false));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        assert!(
            matches!(phase, Phase::Failed { reason, .. } if reason == "process exited"),
            "got {phase:?}"
        );
    }

    #[test]
    fn advance_phases_leaves_running_and_failed_alone() {
        let mut state = State::new();
        state.worktrees.insert("alive".into(), record_with(100));
        let mut failed = WorktreeRecord::new("/abs/f", true);
        failed.processes.insert(
            "dev".into(),
            process(
                200,
                Phase::Failed {
                    at: at(9),
                    reason: "timeout".into(),
                },
            ),
        );
        state.worktrees.insert("failed".into(), failed);

        let before = state.clone();
        assert!(!advance_phases(&mut state, |_| true, |_, _| true));
        assert_eq!(state, before);
    }

    #[test]
    fn advance_phases_decides_per_process_not_per_worktree() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes.insert("dev".into(), running(100));
        rec.processes.insert("api".into(), running(200));
        state.worktrees.insert("w".into(), rec);

        advance_phases(&mut state, |pid| pid == 100, |_, _| true);
        let procs = &state.worktrees["w"].processes;
        assert!(matches!(procs["dev"].phase, Phase::Running { .. }));
        assert!(matches!(procs["api"].phase, Phase::Failed { .. }));
    }

    #[test]
    fn try_lock_is_exclusive_and_releases_on_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub").join("state.lock");
        let held = lock(&path).unwrap();
        assert!(
            try_lock(&path).unwrap().is_none(),
            "a contended try_lock must not hand out the lock"
        );
        drop(held);
        assert!(
            reacquired(&path),
            "the lock must become available once the holder drops it"
        );
    }

    /// A `fork` anywhere in the test process duplicates every open
    /// descriptor, so a sibling test spawning git can hold a copy of this
    /// lock's fd for the few microseconds before it `exec`s and CLOEXEC
    /// closes it. Retrying briefly tests the release, not that race.
    fn reacquired(path: &Path) -> bool {
        for _ in 0..50 {
            if try_lock(path).unwrap().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn try_lock_succeeds_when_uncontended() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.lock");
        assert!(try_lock(&path).unwrap().is_some());
    }
}
