//! The orchestration layer. Everything user-facing — the CLI and the TUI —
//! calls into here; those two stay thin wrappers.
//!
//! Invariant 1 is enforced at this level: the only things written inside a
//! worktree are paths the project's own gitignore already ignores, checked
//! with `git check-ignore` before anything is created.
//!
//! One file per concern: worktrees, hooks, questions, `init`, the runtime
//! check, start/stop/restart, private services, namespaced starts, share,
//! and reading state.

mod hooks;
mod init;
mod lifecycle;
mod namespaced;
mod questions;
mod refresh;
mod runtime;
mod services;
mod share;
mod worktree;

pub use hooks::{
    HookContext, INSTALL_HOOK, hook_scope, install_remedy, matched_nothing, run_hooks, runs_again,
};
pub use init::{
    ALL_SLOTS, InitReport, SlotSummary, init, init_dry_run, machine_evidence,
    machine_evidence_from, machine_evidence_script,
};
pub use lifecycle::{
    Mode, StartReport, StartedProcess, StopOutcome, process_names, refuse_only_on_a_mode_change,
    restart, start, stop, stop_all, stop_all_with,
};
pub use namespaced::{
    Leftover, login_question, namespace_leftovers, namespace_lines, namespace_login,
    namespaced_not_own_data, namespaces_rm_drops,
};
pub use questions::{
    Answer, Answering, Ask, NEW_SLOTS, NeedsAnswer, Question, RefusedAnswer, START_SLOTS,
    Volunteered, question_for, recommended, resolve, resolve_for_new, resolve_for_start,
    resolve_on, resolve_process, resolve_silencing, settled,
};
pub use refresh::{FAILURE_SHOWN_LINES, Refreshed, failure_tail, inspect, refresh};
pub use runtime::{Machine, runs_through_runner, runtime_shell, user_home, with_prelude};
pub use services::{
    ServiceStatus, export_lines, recorded_service_statuses, resolved_env, service_roles,
    service_statuses, shared_service_statuses, worktree_url,
};
pub use share::{ENV_SHARE_PORT, ShareOutcome, SpawnProxy, share, share_with, unshare};
pub use worktree::{
    CREATED_BUT_INSTALL_FAILED, KEPT_OVER_RACED_RECORD, Ownership, created_by_pando,
    guard_write_locations, ls, new, new_for_pr, ownership, path, rm, sanitize_branch_to_dir,
};

#[cfg(test)]
mod tests;
