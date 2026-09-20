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
    /// Listening sockets actually seen in the process group.
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
            "state file {} is version {}, this pando speaks version {STATE_VERSION}",
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
/// Every read path calls this before trusting the map. Returns whether
/// anything changed, so a caller holding the lock knows to save.
pub fn reconcile(state: &mut State, is_alive: impl Fn(u32) -> bool) -> bool {
    let mut changed = false;
    for rec in state.worktrees.values_mut() {
        let before = rec.processes.len();
        rec.processes.retain(|_, p| is_alive(p.pid));
        changed |= rec.processes.len() != before;

        let before = rec.services.len();
        rec.services
            .retain(|s| s.pid.map(&is_alive).unwrap_or(true));
        changed |= rec.services.len() != before;

        if !rec.processes.is_empty() {
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

/// Moves each process through Starting → Running → Failed. `is_port_free` is
/// consulted for a process that declared a `ready_port`; one that did not is
/// Running as soon as it is alive.
pub fn advance_phases(
    state: &mut State,
    is_alive: impl Fn(u32) -> bool,
    is_port_free: impl Fn(u16) -> bool,
) -> bool {
    let now = Utc::now();
    let mut changed = false;
    for rec in state.worktrees.values_mut() {
        for proc in rec.processes.values_mut() {
            match &proc.phase {
                Phase::Starting { since } => {
                    if !is_alive(proc.pid) {
                        proc.phase = Phase::Failed {
                            at: now,
                            reason: "process exited".into(),
                        };
                        changed = true;
                    } else if proc.ready_port.map(|p| !is_port_free(p)).unwrap_or(true) {
                        proc.phase = Phase::Running { since: now };
                        changed = true;
                    } else if now.signed_duration_since(*since).num_seconds() > START_TIMEOUT_SECS {
                        proc.phase = Phase::Failed {
                            at: now,
                            reason: "timeout".into(),
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
        rec.observed_ports = vec![17_000, 17_001];
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

        assert!(advance_phases(&mut state, |_| true, |_| false));
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

        assert!(advance_phases(&mut state, |_| true, |_| true));
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

        assert!(advance_phases(&mut state, |_| false, |_| true));
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

        assert!(advance_phases(&mut state, |_| true, |_| true));
        let phase = &state.worktrees["w"].processes["dev"].phase;
        assert!(
            matches!(phase, Phase::Failed { reason, .. } if reason == "timeout"),
            "got {phase:?}"
        );
    }

    #[test]
    fn advance_phases_moves_running_to_failed_when_the_process_dies() {
        let mut state = State::new();
        state.worktrees.insert("w".into(), record_with(100));
        assert!(advance_phases(&mut state, |_| false, |_| true));
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
        assert!(!advance_phases(&mut state, |_| true, |_| false));
        assert_eq!(state, before);
    }

    #[test]
    fn advance_phases_decides_per_process_not_per_worktree() {
        let mut state = State::new();
        let mut rec = WorktreeRecord::new("/abs/w", true);
        rec.processes.insert("dev".into(), running(100));
        rec.processes.insert("api".into(), running(200));
        state.worktrees.insert("w".into(), rec);

        advance_phases(&mut state, |pid| pid == 100, |_| false);
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
