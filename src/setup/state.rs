//! Where a project's setup stands: seven states, decided in order.

use super::fingerprint::fingerprint;
use super::record::{CheckOutcome, CheckRecord, SetupMemory};
use crate::config::Config;
use crate::paths::PandoPaths;

/// Where a project's setup stands, from what pando's own files say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupState {
    /// A check holds the lock now.
    Testing,
    /// The last check never finished: its record says running and nothing
    /// holds the lock, or it says it was stopped.
    Interrupted,
    /// Nothing to run yet. `skipped` is the developer's `esc`: a skipped
    /// project is still new — calling a library "untested" would offer a
    /// test of nothing — but gets a one-line hint instead of the setup
    /// screen.
    New { skipped: bool },
    /// The last check passed, on today's run settings.
    Ready,
    /// The last check failed, or found a run question open, on today's
    /// run settings.
    Failing,
    /// The run settings are not the ones the last check tested.
    Stale,
    /// Something to run, and never checked. A project configured before
    /// checks existed is here, never sent back to the setup screen.
    Untested,
}

/// A project's setup: its state and what it was decided from, so a screen
/// can show the record without reading it again.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    pub state: SetupState,
    pub last_check: Option<CheckRecord>,
    pub memory: SetupMemory,
    /// Today's run settings' fingerprint.
    pub fingerprint: String,
}

/// Reads the project's setup. Creates nothing: a project pando has never
/// written a file for reads as new or untested, and leaves no trace.
pub fn read(paths: &PandoPaths, config: &Config) -> Setup {
    let last_check = CheckRecord::load(paths);
    let memory = SetupMemory::load(paths);
    let fingerprint = fingerprint(config);
    let state = decide(
        check_running(paths),
        last_check.as_ref(),
        &memory,
        config,
        &fingerprint,
    );
    Setup {
        state,
        last_check,
        memory,
        fingerprint,
    }
}

/// The decision, from its inputs alone, in the order the states are
/// listed: a held lock beats everything, an unfinished record beats the
/// settings, and only a finished record is compared with them.
pub fn decide(
    lock_held: bool,
    record: Option<&CheckRecord>,
    memory: &SetupMemory,
    config: &Config,
    today: &str,
) -> SetupState {
    if lock_held {
        return SetupState::Testing;
    }
    if let Some(record) = record
        && matches!(
            record.outcome,
            CheckOutcome::Running | CheckOutcome::Interrupted
        )
    {
        return SetupState::Interrupted;
    }
    if config.runnable_processes().next().is_none() {
        return SetupState::New {
            skipped: memory.skipped_at.is_some(),
        };
    }
    let Some(record) = record else {
        return SetupState::Untested;
    };
    if record.changed_while_running() || record.fingerprint() != today {
        return SetupState::Stale;
    }
    match record.outcome {
        CheckOutcome::Passed => SetupState::Ready,
        CheckOutcome::Failed { .. } | CheckOutcome::NotSetUp { .. } => SetupState::Failing,
        CheckOutcome::Running | CheckOutcome::Interrupted => {
            unreachable!("an unfinished record is decided above")
        }
    }
}

/// Whether another process holds the check lock.
///
/// Asked by taking the lock without waiting and dropping it at once, and
/// only when the lock file exists: `try_lock` creates the file and its
/// directory, and reading the state must write nothing. A check creates
/// the file before it holds it, so no file means no check. A lock that
/// cannot be asked about reads as free, which at worst shows the last
/// record rather than a live one.
fn check_running(paths: &PandoPaths) -> bool {
    let lock = paths.check_lock_file();
    if !lock.exists() {
        return false;
    }
    matches!(crate::state::try_lock(&lock), Ok(None))
}
