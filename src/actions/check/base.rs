//! Whose a failure is when the commit tested lacks what the step needed:
//! a file the main checkout's branch has, and the base does not.
//!
//! The trap this closes: an install run with `--frozen` fails on a
//! lockfile the tested commit never had, and a failure read as the
//! settings' invites loosening the install until it passes — on the
//! wrong commit, with a lockfile it wrote itself.

use std::path::Path;

use crate::catalog::package_managers::PACKAGE_MANAGERS;
use crate::setup::{CheckOutcome, FailureKind};

/// A failure of the settings the base explains instead, as a base
/// failure naming both refs; any other outcome as it was.
///
/// The one signal, cheap and hard to fool: a file the failure names —
/// in its reason or in its last lines — or, for a failed install, a
/// lockfile of a manager the install runs, that the main checkout's
/// branch has and the commit tested does not. One `git diff` over the
/// two, and nothing when they are the same commit.
pub(super) fn on_the_base(
    root: &Path,
    outcome: CheckOutcome,
    commit: &str,
    base_ref: Option<&str>,
    tail: &[String],
    install: Option<&str>,
) -> CheckOutcome {
    let CheckOutcome::Failed {
        kind: FailureKind::Settings,
        reason,
    } = &outcome
    else {
        return outcome;
    };
    let text = std::iter::once(reason.as_str())
        .chain(tail.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let Some((file, branch)) = missing_at_base(root, commit, &text, install) else {
        return outcome;
    };
    let short: String = commit.chars().take(7).collect();
    let tested = base_ref.unwrap_or("HEAD");
    CheckOutcome::Failed {
        kind: FailureKind::Base,
        reason: format!(
            "{reason} — {file} is on {branch}, the main checkout's branch, but not at {tested} \
             ({short}), the commit this check tested. No setting fixes that: test {branch} with \
             `pando check --base {branch}`, or answer `base` with it"
        ),
    }
}

/// The first file the main checkout's HEAD added over `commit` that the
/// failure needed, and the branch HEAD is on — its short sha when it is
/// on none, which `--base` takes as well.
fn missing_at_base(
    root: &Path,
    commit: &str,
    text: &str,
    install: Option<&str>,
) -> Option<(String, String)> {
    let head = git_line(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    if head == commit {
        return None;
    }
    let out = crate::project::git(
        root,
        [
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            "-z",
            commit,
            "HEAD",
            "--",
        ],
    )
    .ok()
    .filter(|out| out.status.success())?;
    let added = String::from_utf8_lossy(&out.stdout).into_owned();
    let lockfiles = install.map(lockfiles_run_by).unwrap_or_default();
    let file = added.split('\0').filter(|p| !p.is_empty()).find(|path| {
        let name = path.rsplit('/').next().unwrap_or(path);
        // A name of a letter or three is a word before it is a file.
        names(text, path) || (name.len() > 3 && names(text, name)) || lockfiles.contains(&name)
    })?;
    let branch =
        crate::worktree::checked_out_branch(root).unwrap_or_else(|| head.chars().take(7).collect());
    Some((file.to_string(), branch))
}

/// The lockfiles of every manager an install command runs, by its
/// program as a word of the command.
fn lockfiles_run_by(install: &str) -> Vec<&'static str> {
    let words: Vec<&str> = install
        .split(|c: char| c.is_whitespace() || ";&|()".contains(c))
        .collect();
    PACKAGE_MANAGERS
        .iter()
        .filter(|manager| words.contains(&manager.program))
        .flat_map(|manager| manager.lockfiles.iter().copied())
        .collect()
}

/// Whether `text` names `path` as a whole: not as the tail of a longer
/// name, nor its head (`uv.lock` is not named by `uv.lockfile`).
pub(super) fn names(text: &str, path: &str) -> bool {
    let part = |c: char| c.is_alphanumeric() || "_-.".contains(c);
    text.match_indices(path).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + path.len()..].chars().next();
        !before.is_some_and(part)
            && !after
                .is_some_and(|c| part(c) && !(c == '.' && ends_sentence(&text[at + path.len()..])))
    })
}

/// Whether a `.` right after a name ends the sentence rather than
/// continuing the name: `uv.lock.` names `uv.lock`, `uv.lock.bak` does not.
fn ends_sentence(rest: &str) -> bool {
    rest.chars()
        .nth(1)
        .is_none_or(|c| c.is_whitespace() || "`'\")".contains(c))
}

fn git_line(root: &Path, args: &[&str]) -> Option<String> {
    let out = crate::project::git(root, args).ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !line.is_empty()).then_some(line)
}
