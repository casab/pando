//! `start --wait` and `restart --wait`: block until a worktree is ready,
//! or say which process failed and why. When a process counts as ready
//! is `actions`'s: the check judges it the same way.

use super::names::Named;
use crate::actions::{self, ReadyVerdict, ready_limit, ready_line, ready_verdict};
use crate::actions::{still_watched, watched_processes as watched};
use crate::paths::PandoPaths;
use crate::state::Phase;
use anyhow::{Result, bail};
use chrono::Utc;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How often the phases are read again. The same cadence `logs -f` uses.
const POLL: Duration = Duration::from_millis(250);

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
    let mut told: BTreeSet<String> = BTreeSet::new();
    loop {
        let refreshed = actions::refresh(paths);
        // A service that died under the wait is forgotten by this refresh
        // and saved as gone: the wait is the one command that sees it go,
        // and a failure it causes comes next. Told once, even if a save
        // that failed makes the next refresh find it again.
        for line in &refreshed.notices {
            if told.insert(line.clone()) {
                notice(line);
            }
        }
        let Some(record) = refreshed.state.worktrees.get(name.as_str()) else {
            // A state file pando could not read says nothing about whether
            // anything stopped. One it read and could not save still does.
            if refreshed.unreadable
                && let Some(warning) = &refreshed.warning
            {
                bail!("{warning}");
            }
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
        match ready_verdict(record, only, now) {
            ReadyVerdict::Ready => return Ok(()),
            ReadyVerdict::Gone => {
                bail!("{shown} has nothing running — it stopped while pando waited")
            }
            ReadyVerdict::Failed {
                process,
                reason,
                log,
            } => {
                let tail = actions::failure_tail(&log);
                if !tail.is_empty() {
                    notice(&format!("the last lines of the {process} log:"));
                    for line in &tail {
                        super::to_stderr(&format!("  {line}\n"));
                    }
                }
                bail!(
                    "{process} failed: {reason} — `pando logs {typed} --source {process}` has \
                     the rest"
                )
            }
            ReadyVerdict::Waiting => {}
        }
        if began.elapsed() > ready_limit(record, only) {
            bail!(
                "{shown} is still starting after {}s — `pando logs {typed}` shows what it is \
                 doing",
                began.elapsed().as_secs()
            );
        }
        std::thread::sleep(POLL);
    }
}
