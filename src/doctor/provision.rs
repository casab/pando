//! Worktrees pando did not create that lack a file `provision` names.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::actions::{self, Unprovisioned};
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::process::shell_word;

use super::report::{Finding, Section};

/// A note for each provisioned path that worktrees pando did not create
/// lack, and that the main checkout can give them.
///
/// `provision` reaches only the worktrees `new` makes, and a `start` of
/// one of them gives it a path it lacks. Every other worktree — made with
/// `git worktree add` and adopted — is one pando never writes into, which
/// is Invariant 1, so its app starts with no `.env` and the report is the
/// place left to say so. Grouped by path, because a project that adopted
/// its worktrees has most of them lacking the same file, and a note per
/// worktree would be the whole report.
///
/// Only a path the worktree's own gitignore ignores gets a command: one it
/// does not is a file that would show in `git status` there, and that is
/// said instead.
pub(super) fn unprovisioned_findings(
    paths: &PandoPaths,
    config: &Config,
    view: &actions::Refreshed,
    findings: &mut Vec<Finding>,
) {
    if config.project.provision_paths().is_empty() {
        return;
    }
    let Ok(found) = crate::worktree::discover_all(&paths.project) else {
        return;
    };
    let ours = actions::ownership(&view.state, &found.worktrees);
    // By path, then by whether it is ignored there: each group is one note.
    let mut groups: BTreeMap<(String, bool), Vec<Lacking>> = BTreeMap::new();
    for worktree in &found.worktrees {
        if worktree.prunable || ours.get(&worktree.name) == Some(&true) {
            continue;
        }
        // Not a path whose directory the branch does not have: nothing
        // there reads it, and the command would fail on it.
        for missing in actions::unprovisioned(paths, config, &worktree.path)
            .into_iter()
            .filter(Unprovisioned::has_place)
        {
            groups
                .entry((missing.rel.clone(), missing.ignored))
                .or_default()
                .push(Lacking {
                    name: worktree.display_name(),
                    dir: worktree.path.clone(),
                    missing,
                });
        }
    }
    for ((rel, ignored), mut lacking) in groups {
        // By name, not in git's order, which is the order they were made.
        lacking.sort_by(|a, b| a.name.cmp(&b.name));
        findings.push(match ignored {
            true => lacking_note(&rel, &lacking),
            false => not_ignored_note(&rel, &lacking),
        });
    }
}

/// One worktree that lacks a provisioned path.
struct Lacking {
    name: String,
    dir: PathBuf,
    missing: Unprovisioned,
}

/// Worktrees whose gitignore ignores `rel`, and the command that gives
/// them what `new` would have.
fn lacking_note(rel: &str, lacking: &[Lacking]) -> Finding {
    let names: Vec<&str> = lacking.iter().map(|w| w.name.as_str()).collect();
    let first = &lacking[0].missing;
    let message = match (names.as_slice(), &first.seeded_from) {
        ([one], None) => format!(
            "{one}: pando did not create this worktree, so `provision` never gave it {rel}, \
             which the main checkout has — its app starts without it"
        ),
        ([one], Some(from)) => format!(
            "{one}: pando did not create this worktree, so `provision` never seeded its {rel} \
             from the main checkout's {from} — its app starts without it"
        ),
        (several, None) => format!(
            "{} worktrees pando did not create have no {rel}, which `provision` gives the ones \
             it creates and the main checkout has — their apps start without it: {}",
            several.len(),
            several.join(", ")
        ),
        (several, Some(from)) => format!(
            "{} worktrees pando did not create have no {rel}, which `provision` seeds from the \
             main checkout's {from} in the ones it creates — their apps start without it: {}",
            several.len(),
            several.join(", ")
        ),
    };
    let command = match lacking {
        [one] => one.missing.command(),
        // One loop over their directories rather than a line each: the
        // project that adopted its worktrees has a dozen of them.
        several => format!(
            "for w in {}; do {} {} \"$w\"/{}; done",
            several
                .iter()
                .map(|w| shell_word(&w.dir.display().to_string()))
                .collect::<Vec<_>>()
                .join(" "),
            first.verb(),
            shell_word(&first.source.display().to_string()),
            shell_word(rel)
        ),
    };
    Finding::note(Section::Worktrees, message).with_fix(format!(
        "`{command}` — pando writes only into worktrees it created, so this is yours to run"
    ))
}

/// Worktrees that lack `rel` and do not ignore it: pando would not write
/// it there, even into a worktree of its own.
fn not_ignored_note(rel: &str, lacking: &[Lacking]) -> Finding {
    let names: Vec<&str> = lacking.iter().map(|w| w.name.as_str()).collect();
    let why = "pando would not write it there, and a copy would show in `git status`";
    let message = match names.as_slice() {
        [one] => format!(
            "{one}: pando did not create this worktree, and it has no {rel}, which `provision` \
             names and its .gitignore does not ignore — {why}"
        ),
        several => format!(
            "{} worktrees pando did not create have no {rel}, which `provision` names and their \
             .gitignore does not ignore — {why}: {}",
            several.len(),
            several.join(", ")
        ),
    };
    Finding::note(Section::Worktrees, message).with_fix(format!(
        "add {rel} to that branch's .gitignore first, and then `pando doctor` gives the command \
         that copies it in"
    ))
}
