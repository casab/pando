//! A check that is told to stop — Ctrl-C, a closed terminal, a `kill` —
//! still takes its worktree down before it goes.

/// Makes SIGINT, SIGTERM and SIGHUP end a check through its own teardown
/// rather than where they land: the handler only notes the signal, and the
/// check, which looks between every step and every quarter second while
/// it waits, stops, removes its worktree and records itself interrupted.
///
/// For the CLI, once, before the check — and before `new`, whose
/// copy-on-write checkout is pando's own work rather than one `git
/// worktree add`, which cleans up after itself when told to stop: a `new`
/// told to stop mid-checkout unwinds what it made instead of leaving a
/// half-filled worktree. A second signal of the same kind
/// ends pando at once, as it would have without the handler: somebody
/// pressing Ctrl-C twice means it, and `pando check` sweeps whatever that
/// leaves the next time it runs. A hook the check runs in pando's own
/// process group gets a terminal's Ctrl-C itself and ends; one that a
/// `kill` of pando alone does not reach runs to its end first.
pub fn catch_check_interrupts() {
    crate::platform::signals::catch_interrupts();
}

/// Whether the check, or a `new` in its checkout, has been told to stop.
/// Read between the check's steps and on every look it takes while it
/// waits.
pub(in crate::actions) fn interrupted() -> bool {
    crate::platform::signals::interrupted()
}
