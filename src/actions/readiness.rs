//! When a started worktree is ready: the verdict on its processes, how
//! long a wait for it may take, and how long a process with no port is
//! watched. One copy, shared by `start --wait` and `check`.

use crate::state::{self, Aggregate, Phase, WorktreeRecord};
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Slack past the longest a process can stay `Starting`. The phase machine
/// is what gives up on a process; this only stops a wait that has somehow
/// outlived it from hanging a script for ever.
const GRACE: Duration = Duration::from_secs(15);

/// How long a process with no port to probe is watched after it starts,
/// when it declares no `ready.timeout_s` of its own.
///
/// Such a process is `Running` as soon as it is alive, which is at once —
/// so without a watch, `--wait` returned success for a dev command that
/// exited a second later. A dev server stays up; one that is still up
/// after this long is as ready as pando can tell. Three seconds let a
/// worker that died at four through, which is why this is five.
pub const NO_PORT_WATCH: Duration = Duration::from_secs(5);

/// The longest a portless process's own `ready.timeout_s` stretches its
/// watch: that timeout is written for a port a slow build takes to open,
/// and sitting a whole build out on a process with nothing to probe would
/// make a wait slower, not surer.
pub const NO_PORT_WATCH_MAX: Duration = Duration::from_secs(10);

/// How long `p` is watched after it starts when it has no port to probe:
/// its own `ready.timeout_s`, capped at [`NO_PORT_WATCH_MAX`], or
/// [`NO_PORT_WATCH`].
pub fn no_port_watch(p: &state::ProcessRecord) -> Duration {
    match p.ready_timeout_s {
        Some(secs) => Duration::from_secs(secs).min(NO_PORT_WATCH_MAX),
        None => NO_PORT_WATCH,
    }
}

/// Whether `p` has no port to probe and is still inside its watch at `now`.
pub fn still_watched(p: &state::ProcessRecord, now: DateTime<Utc>) -> bool {
    let watch = chrono::Duration::from_std(no_port_watch(p)).expect("a few seconds");
    p.ready_port.is_none() && now - p.started_at < watch
}

/// What one look at the processes a wait is about says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyVerdict {
    /// Every one of them is ready.
    Ready,
    /// Still coming up, or a portless one still being watched.
    Waiting,
    Failed {
        process: String,
        reason: String,
        log: std::path::PathBuf,
    },
    /// Nothing of them is left.
    Gone,
}

/// The processes of `record` a wait is about: the one `only` names, or
/// every one. A sibling that failed an hour ago is not what `start --only
/// web --wait` asked about, and must not fail it.
pub fn watched_processes<'a>(
    record: &'a WorktreeRecord,
    only: Option<&str>,
) -> impl Iterator<Item = (&'a String, &'a state::ProcessRecord)> {
    record
        .processes
        .iter()
        .filter(move |(process, _)| only.is_none_or(|wanted| process.as_str() == wanted))
}

pub fn ready_verdict(
    record: &WorktreeRecord,
    only: Option<&str>,
    now: DateTime<Utc>,
) -> ReadyVerdict {
    let mut subset = record.clone();
    subset.processes = watched_processes(record, only)
        .map(|(n, p)| (n.clone(), p.clone()))
        .collect();
    match state::aggregate_phase(&subset) {
        None => ReadyVerdict::Gone,
        Some(Aggregate::Failed {
            process, reason, ..
        }) => {
            let log = subset.processes[&process].log_path.clone();
            ReadyVerdict::Failed {
                process,
                reason,
                log,
            }
        }
        Some(Aggregate::Starting { .. }) => ReadyVerdict::Waiting,
        Some(Aggregate::Running { .. }) => {
            match subset.processes.values().any(|p| still_watched(p, now)) {
                true => ReadyVerdict::Waiting,
                false => ReadyVerdict::Ready,
            }
        }
    }
}

/// How long a wait may take before it gives up on its own: the longest
/// any watched process can stay `Starting` — its window, plus the grace an
/// unanswerable port scan earns it — or be watched for want of a port, and
/// a little more.
pub fn ready_limit(record: &WorktreeRecord, only: Option<&str>) -> Duration {
    watched_processes(record, only)
        .map(|(_, p)| Duration::from_secs(longest_starting(p)).max(no_port_watch(p)))
        .max()
        .unwrap_or_default()
        .max(NO_PORT_WATCH)
        .saturating_add(GRACE)
}

/// [`state::longest_starting_secs`] for `p`, saturating. `ready.timeout_s`
/// is any number config takes, and the window plus its grace is twice it:
/// past half of `i64::MAX` that overflowed — a panic in a debug build, and
/// in a release build a negative sum that capped the wait at 20 seconds
/// while the process was, correctly, still starting.
fn longest_starting(p: &state::ProcessRecord) -> u64 {
    let timeout = p.ready_timeout_s.map_or(state::START_TIMEOUT_SECS, |s| {
        i64::try_from(s).unwrap_or(i64::MAX)
    });
    let grace = state::unconfirmed_grace_secs(timeout);
    u64::try_from(timeout.saturating_add(grace)).unwrap_or(0)
}

/// The line a wait narrates once `p` is ready at `now`, or `None` while it
/// is not: still `Starting`, or portless and inside its watch. "Ready"
/// printed before the watch was over was a promise the watch had not kept.
pub fn ready_line(
    process: &str,
    p: &state::ProcessRecord,
    now: DateTime<Utc>,
    elapsed: Duration,
) -> Option<String> {
    if !matches!(p.phase, Phase::Running { .. }) || still_watched(p, now) {
        return None;
    }
    Some(match p.ready_port {
        Some(_) => format!("{process} is ready ({:.1}s)", elapsed.as_secs_f32()),
        None => format!(
            "{process} is up (no port to check; watched {}s)",
            no_port_watch(p).as_secs()
        ),
    })
}
