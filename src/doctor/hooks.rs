//! The hooks section: when each hook last ran and whether the next start runs
//! it again.

use crate::config::{self, Config};
use crate::paths::PandoPaths;
use crate::{actions, detect};

use super::report::{Finding, HookReport, HookRunReport, Section, WorktreeReport};

pub(super) fn hooks_report(
    paths: &PandoPaths,
    config: &Config,
    view: &actions::Refreshed,
    worktrees: &[WorktreeReport],
    findings: &mut Vec<Finding>,
) -> Vec<HookReport> {
    schema_slot_finding(paths, config, findings);
    let mut out = Vec::new();
    for hook in &config.hooks {
        let matches = (!hook.fingerprint.is_empty())
            .then(|| crate::hooks::matched(paths.root(), &hook.fingerprint).len());
        if matches == Some(0) {
            findings.push(
                Finding::note(
                    Section::Hooks,
                    // The sentence a start prints while it happens, so the
                    // two cannot drift apart.
                    actions::matched_nothing(paths.root(), hook)
                        .trim_start_matches("warning: ")
                        .to_string(),
                )
                .with_fix("key it on the files it really depends on, or leave `fingerprint` out"),
            );
        }
        let mut runs = Vec::new();
        for worktree in worktrees {
            let Some(record) = view.state.worktrees.get(&worktree.name) else {
                continue;
            };
            let Some(run) = record.hooks.get(&hook.name) else {
                continue;
            };
            let current = crate::hooks::fingerprint(&record.path, &hook.fingerprint, &hook.cmd);
            runs.push(HookRunReport {
                worktree: worktree.name.clone(),
                ran_at: run.ran_at,
                will_run_again: current.is_none() || current != run.fingerprint,
            });
        }
        out.push(HookReport {
            name: hook.name.clone(),
            after: match hook.after {
                config::HookPoint::Create => "create",
                config::HookPoint::Install => "install",
                config::HookPoint::Services => "services",
                config::HookPoint::Dev => "dev",
            },
            cmd: hook.cmd.clone(),
            fingerprint: hook.fingerprint.clone(),
            matches,
            runs,
        });
    }
    out
}

/// The schema question has no way to record "this project has no schema
/// step", so an undecided one comes back on every start.
///
/// Reported, not fixed: giving the slot an empty form is the same piece of
/// design the services and provision slots each had done for them, and it
/// belongs to whoever owns that question rather than to the command that
/// noticed it.
fn schema_slot_finding(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    if actions::already_answered(detect::Slot::SchemaHook, config) {
        return;
    }
    let signals = detect::signals(paths.root());
    let undecided = detect::propose(paths.root(), &signals)
        .into_iter()
        .any(|p| p.slot == detect::Slot::SchemaHook && !p.decided && !p.candidates.is_empty());
    if !undecided {
        return;
    }
    findings.push(
        Finding::note(
            Section::Hooks,
            "pando has a question about the schema step that its rules cannot settle, and no \
             way to record \"this project has none\" — so it comes back on every start until a \
             hook is written down"
                .to_string(),
        )
        .with_fix(
            "answer it once with `pando init`, or write a `[[hooks]]` entry by hand — one whose \
             `cmd` is `true` is the shape that means \"nothing to do\"",
        ),
    );
}
