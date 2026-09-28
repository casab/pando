//! Where a check's logs are kept: the last check's in its worktree's own
//! log directory, and a probe's beside them, so a probe of another base
//! never costs the saved result the logs it is read from.
//!
//! The check's processes, hooks and install all write under
//! [`CHECK_WORKTREE`], whichever kind of run it is. So a probe holds the
//! last check's logs aside at [`CHECK_HELD_LOGS`] while it runs, and
//! [`settle`] moves the probe's to [`CHECK_PROBE_LOGS`] and puts the held
//! ones back.

use anyhow::{Context, Result};
use std::path::Path;

use crate::paths::{CHECK_HELD_LOGS, CHECK_PROBE_LOGS, CHECK_WORKTREE, PandoPaths};
use crate::setup::CheckOutcome;

/// Makes the check's log directory this run's. An ordinary check clears
/// the last check's logs; a probe holds them aside, empty when there were
/// none, so that [`settle`] knows the logs in their place are a probe's.
///
/// Called once whatever settled a leftover probe has run: nothing is held
/// already.
pub(super) fn claim(paths: &PandoPaths, probe: bool) -> Result<()> {
    let live = paths.logs_dir(CHECK_WORKTREE);
    if !probe {
        let _ = std::fs::remove_dir_all(&live);
        return Ok(());
    }
    let held = paths.logs_dir(CHECK_HELD_LOGS);
    if is_dir(&live) {
        std::fs::rename(&live, &held).with_context(|| {
            format!(
                "hold the last check's logs aside at {}, so the probe does not write over them",
                held.display()
            )
        })
    } else {
        std::fs::create_dir_all(&held).with_context(|| format!("create {}", held.display()))
    }
}

/// After a probe, or at the start of any check after one that was killed
/// before it could: the probe's logs go to [`CHECK_PROBE_LOGS`], replacing
/// the last probe's, and the last check's go back where they were. Does
/// nothing when no probe held them.
pub(super) fn settle(paths: &PandoPaths) {
    let held = paths.logs_dir(CHECK_HELD_LOGS);
    if !is_dir(&held) {
        return;
    }
    let live = paths.logs_dir(CHECK_WORKTREE);
    let probe = paths.logs_dir(CHECK_PROBE_LOGS);
    let _ = std::fs::remove_dir_all(&probe);
    if is_dir(&live) && std::fs::rename(&live, &probe).is_err() {
        // Left for the next check to settle: the held logs are not put
        // back over the probe's.
        return;
    }
    // An empty one held the place of none.
    if std::fs::remove_dir(&held).is_err() {
        let _ = std::fs::rename(&held, &live);
    }
}

/// A probe's failure, with the log it names where [`settle`] moved it: a
/// hook's failure says where its whole log is, and at the check's own
/// place that is the last check's log by now.
pub(super) fn pointed_at_probe(paths: &PandoPaths, outcome: CheckOutcome) -> CheckOutcome {
    let probe = paths.logs_dir(CHECK_PROBE_LOGS);
    match outcome {
        CheckOutcome::Failed { kind, reason } if is_dir(&probe) => CheckOutcome::Failed {
            kind,
            reason: reason.replace(
                &format!("{}/", paths.logs_dir(CHECK_WORKTREE).display()),
                &format!("{}/", probe.display()),
            ),
        },
        other => other,
    }
}

/// A directory of its own, not a link to one somewhere else.
fn is_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}
