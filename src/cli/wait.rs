//! `start --wait` and `restart --wait`: block until a worktree is ready,
//! or say which process failed and why.

use super::names::Named;
use crate::actions;
use crate::paths::PandoPaths;
use crate::state::{self, Aggregate, Phase, WorktreeRecord};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How often the phases are read again. The same cadence `logs -f` uses.
const POLL: Duration = Duration::from_millis(250);

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
pub(super) const NO_PORT_WATCH: Duration = Duration::from_secs(5);

/// The longest a portless process's own `ready.timeout_s` stretches its
/// watch: that timeout is written for a port a slow build takes to open,
/// and sitting a whole build out on a process with nothing to probe would
/// make a wait slower, not surer.
pub(super) const NO_PORT_WATCH_MAX: Duration = Duration::from_secs(10);

/// How long `p` is watched after it starts when it has no port to probe:
/// its own `ready.timeout_s`, capped at [`NO_PORT_WATCH_MAX`], or
/// [`NO_PORT_WATCH`].
pub(super) fn no_port_watch(p: &state::ProcessRecord) -> Duration {
    match p.ready_timeout_s {
        Some(secs) => Duration::from_secs(secs).min(NO_PORT_WATCH_MAX),
        None => NO_PORT_WATCH,
    }
}

/// Whether `p` has no port to probe and is still inside its watch at `now`.
fn still_watched(p: &state::ProcessRecord, now: DateTime<Utc>) -> bool {
    let watch = chrono::Duration::from_std(no_port_watch(p)).expect("a few seconds");
    p.ready_port.is_none() && now - p.started_at < watch
}

/// What one look at the processes a wait is about says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
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
fn watched<'a>(
    record: &'a WorktreeRecord,
    only: Option<&str>,
) -> impl Iterator<Item = (&'a String, &'a state::ProcessRecord)> {
    record
        .processes
        .iter()
        .filter(move |(process, _)| only.is_none_or(|wanted| process.as_str() == wanted))
}

pub(super) fn verdict(record: &WorktreeRecord, only: Option<&str>, now: DateTime<Utc>) -> Verdict {
    let mut subset = record.clone();
    subset.processes = watched(record, only)
        .map(|(n, p)| (n.clone(), p.clone()))
        .collect();
    match state::aggregate_phase(&subset) {
        None => Verdict::Gone,
        Some(Aggregate::Failed {
            process, reason, ..
        }) => {
            let log = subset.processes[&process].log_path.clone();
            Verdict::Failed {
                process,
                reason,
                log,
            }
        }
        Some(Aggregate::Starting { .. }) => Verdict::Waiting,
        Some(Aggregate::Running { .. }) => {
            match subset.processes.values().any(|p| still_watched(p, now)) {
                true => Verdict::Waiting,
                false => Verdict::Ready,
            }
        }
    }
}

/// How long a wait may take before it gives up on its own: the longest
/// any watched process can stay `Starting` — its window, plus the grace an
/// unanswerable port scan earns it — or be watched for want of a port, and
/// a little more.
pub(super) fn limit(record: &WorktreeRecord, only: Option<&str>) -> Duration {
    watched(record, only)
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
pub(super) fn ready_line(
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

/// Waits for the processes of `name` a start was about — every one, or
/// the one `only` names — to be ready, narrating each one as it gets
/// there. Fails — exit 1 — as soon as one of them fails, with its reason,
/// the closing lines of its log, and where the rest is.
///
/// Messages name the worktree as a person knows it, and the commands they
/// suggest spell it as it was typed.
pub(super) fn wait_ready(
    paths: &PandoPaths,
    named: &Named,
    only: Option<&str>,
    notice: &dyn Fn(&str),
) -> Result<()> {
    let Named {
        dir: name,
        shown,
        typed,
    } = named;
    let began = Instant::now();
    let mut announced: BTreeSet<String> = BTreeSet::new();
    let mut said_waiting = false;
    loop {
        let refreshed = actions::refresh(paths);
        let Some(record) = refreshed.state.worktrees.get(name.as_str()) else {
            bail!("{shown} has nothing running — it stopped while pando waited");
        };
        let now = Utc::now();
        let starting: Vec<&str> = watched(record, only)
            .filter(|(_, p)| matches!(p.phase, Phase::Starting { .. }) || still_watched(p, now))
            .map(|(n, _)| n.as_str())
            .collect();
        if !said_waiting && !starting.is_empty() {
            notice(&format!("waiting for {} to be ready", starting.join(", ")));
            said_waiting = true;
        }
        for (process, p) in watched(record, only) {
            let Some(line) = ready_line(process, p, now, began.elapsed()) else {
                continue;
            };
            // Only a process this wait watched come up is narrated: one
            // that was already running says nothing new.
            if announced.insert(process.clone()) && said_waiting {
                notice(&line);
            }
        }
        match verdict(record, only, now) {
            Verdict::Ready => return Ok(()),
            Verdict::Gone => bail!("{shown} has nothing running — it stopped while pando waited"),
            Verdict::Failed {
                process,
                reason,
                log,
            } => {
                let tail = actions::failure_tail(&log);
                if !tail.is_empty() {
                    notice(&format!("the last lines of the {process} log:"));
                    for line in &tail {
                        eprintln!("  {line}");
                    }
                }
                bail!(
                    "{process} failed: {reason} — `pando logs {typed} --source {process}` has \
                     the rest"
                )
            }
            Verdict::Waiting => {}
        }
        if began.elapsed() > limit(record, only) {
            bail!(
                "{shown} is still starting after {}s — `pando logs {typed}` shows what it is \
                 doing",
                began.elapsed().as_secs()
            );
        }
        std::thread::sleep(POLL);
    }
}
