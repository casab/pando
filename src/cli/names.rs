//! Which worktree a command means: the name as typed, the branch it
//! checks out, or the worktree the shell is standing in — and, when it is
//! none of those, the names that were probably meant. The main checkout
//! is one of them, by its directory's name or its branch: pando runs it
//! too.

use super::UsageError;
use super::ls::display_name;
use crate::actions::sanitize_branch_to_dir;
use crate::paths::PandoPaths;
use crate::worktree::{self, Discovery, Worktree};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The most names a "did you mean" offers. More than this is a listing,
/// and `pando ls` is the listing.
const MAX_SUGGESTIONS: usize = 3;

/// A project with no more worktrees than this has them all named in the
/// error, since that is shorter than sending the reader to `pando ls`.
const LIST_ALL_UP_TO: usize = 6;

/// The worktree name `typed` refers to.
///
/// Exact names win. A branch name is accepted too — `feat/one` for the
/// worktree `feat+one` — because the `+` is how a branch becomes a
/// directory name, not something a person should have to remember. A
/// name only pando's state still knows (a worktree whose directory went
/// away) is passed through, so `stop` and `unshare` can still clean up
/// after it. There is deliberately no prefix matching: `rm` takes a name,
/// and a name that means one worktree today and another tomorrow is not
/// one to delete by.
pub(super) fn resolve(paths: &PandoPaths, typed: &str) -> Result<String> {
    refuse_empty(typed)?;
    resolve_listed(paths, &discover_listed(paths)?, typed)
}

/// The path of what `typed` names, the main checkout's included. Resolved
/// and looked up in one listing of the worktrees.
pub(super) fn path(paths: &PandoPaths, typed: &str) -> Result<PathBuf> {
    refuse_empty(typed)?;
    let discovery = discover_listed(paths)?;
    let name = resolve_listed(paths, &discovery, typed)?;
    crate::actions::path_in(discovery, &name)
}

/// The worktrees a name can mean: every one git lists but a check's
/// throwaway worktree, which comes and goes on its own and is no
/// command's to act on by name — see [`worktree::is_check`].
fn discover_listed(paths: &PandoPaths) -> Result<Discovery> {
    let mut discovery = worktree::discover_all(&paths.project)?;
    discovery.worktrees.retain(|w| !worktree::is_check(&w.name));
    Ok(discovery)
}

/// Every name starts with "", so the suggestions for it were every
/// worktree there is, offered as if they were near misses.
fn refuse_empty(typed: &str) -> Result<()> {
    if typed.trim().is_empty() {
        return Err(UsageError(
            "a worktree name cannot be empty — `pando ls` lists them".to_string(),
        )
        .into());
    }
    Ok(())
}

/// [`resolve`], against a listing already made.
fn resolve_listed(paths: &PandoPaths, discovery: &Discovery, typed: &str) -> Result<String> {
    // One worktree's directory and another's branch can be the same
    // string — a branch literally called `a+b` beside the directory `a+b`
    // of the branch `a/b`. Picking the directory silently is how `rm`
    // removes the other one; so it is not picked at all.
    let by_dir = discovery.worktrees.iter().find(|w| w.name == typed);
    let by_branch_name = discovery
        .worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(typed));
    if let (Some(dir), Some(branch)) = (by_dir, by_branch_name)
        && dir.name != branch.name
    {
        return Err(ambiguous(typed, dir, branch).into());
    }
    if by_dir.is_some() {
        return Ok(typed.to_string());
    }
    // The main checkout, by its directory's name or by its branch: every
    // verb takes it, and the ones it cannot do — `rm` — say why.
    let main = &discovery.main;
    if main.name == typed || (by_branch_name.is_none() && main.branch.as_deref() == Some(typed)) {
        return Ok(main.name.clone());
    }
    if let Some(w) = by_branch(&discovery.worktrees, typed) {
        return Ok(w.name.clone());
    }
    // Nor is the check's by what pando still has of it: its record while
    // it runs, and the logs it keeps, a probe's included.
    if worktree::is_check(typed) || crate::paths::CHECK_LOG_DIRS.contains(&typed) {
        anyhow::bail!("{}", unknown(typed, &discovery.worktrees));
    }
    // Read without the lock: this only asks whether the name is known.
    let recorded: Vec<String> = crate::state::load(&paths.state_file())
        .map(|s| s.worktrees.into_keys().collect())
        .unwrap_or_default();
    // Or only its logs are left, which `logs` can still read.
    let has_logs = crate::paths::validate_log_source("worktree", typed).is_ok()
        && paths.logs_dir(typed).is_dir();
    if has_logs || recorded.iter().any(|name| name == typed) {
        return Ok(typed.to_string());
    }
    let sanitized = sanitize_branch_to_dir(typed);
    if recorded.contains(&sanitized) {
        return Ok(sanitized);
    }
    anyhow::bail!("{}", unknown(typed, &discovery.worktrees))
}

/// The usage error for a name that is one worktree's directory and another
/// one's branch, with the unambiguous way to name each.
fn ambiguous(typed: &str, dir: &Worktree, branch: &Worktree) -> UsageError {
    let first = match &dir.branch {
        Some(b) => format!("`{b}`"),
        None => "nothing else — it has no branch".to_string(),
    };
    UsageError(format!(
        "{typed:?} names two worktrees: the directory {typed} (branch {}) and the worktree \
         {} whose branch is {typed} — name the first by {first}, the second by `{}`",
        dir.branch.as_deref().unwrap_or("none"),
        branch.name,
        branch.name
    ))
}

/// The name a person knows the worktree `name` by: its branch, where the
/// directory was named for it. For messages only — stdout lines and JSON
/// carry `name` itself. The directory name when discovery cannot say.
pub(super) fn shown(paths: &PandoPaths, name: &str) -> String {
    match discover_listed(paths) {
        Ok(discovery) => shown_in(&discovery, name),
        Err(_) => name.to_string(),
    }
}

/// [`shown`], from a listing already made.
fn shown_in(discovery: &Discovery, name: &str) -> String {
    discovery
        .worktrees
        .iter()
        .chain(std::iter::once(&discovery.main))
        .find(|w| w.name == name)
        .map(|w| w.display_name())
        .unwrap_or_else(|| name.to_string())
}

/// A worktree as a message speaks of it: `dir`, the name state and stdout
/// carry; `shown`, the name a person knows it by, for the sentence; and
/// `typed`, the spelling a suggested command echoes back — what the user
/// typed, or `shown` when they typed nothing and the shell's directory
/// chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Named {
    pub(super) dir: String,
    pub(super) shown: String,
    pub(super) typed: String,
}

impl Named {
    fn listed(discovery: &Discovery, typed: Option<&str>, dir: &str) -> Named {
        let shown = shown_in(discovery, dir);
        Named {
            dir: dir.to_string(),
            typed: typed.map(str::to_string).unwrap_or_else(|| shown.clone()),
            shown,
        }
    }

    /// `text` — an error composed below the CLI, which only knew the
    /// directory — with the worktree named as [`Named::shown`] names it.
    /// A suggested command, "`pando <verb> dir…`", gets
    /// [`Named::typed`] instead.
    ///
    /// Only a whole name is replaced: the directory inside a path — a log
    /// under `…/feat+m/dev.log` — is a path, and stays one.
    pub(super) fn in_words(&self, text: &str) -> String {
        if self.dir == self.shown && self.dir == self.typed {
            return text.to_string();
        }
        let dir = self.dir.as_str();
        let is_name_char = |c: char| c.is_alphanumeric() || matches!(c, '+' | '-' | '_' | '.');
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(dir) {
            out.push_str(&rest[..at]);
            let after = &rest[at + dir.len()..];
            let prev = out.chars().last();
            let next = after.chars().next();
            let whole = !prev.is_some_and(|c| is_name_char(c) || c == '/')
                && !next.is_some_and(|c| is_name_char(c) || c == '/');
            if whole {
                let in_command = out
                    .rfind('`')
                    .is_some_and(|tick| out[tick..].starts_with("`pando "));
                let command_open = out.matches('`').count() % 2 == 1;
                match in_command && command_open {
                    true => out.push_str(&self.typed),
                    false => out.push_str(&self.shown),
                }
            } else {
                out.push_str(dir);
            }
            rest = after;
        }
        out.push_str(rest);
        out
    }

    /// An error from below the CLI, reworded by [`Named::in_words`]. The
    /// errors whose type main reads — a question, a usage error — pass
    /// through untouched, so their exit code survives.
    pub(super) fn reword(&self, e: anyhow::Error) -> anyhow::Error {
        if e.downcast_ref::<crate::actions::NeedsAnswer>().is_some()
            || e.downcast_ref::<UsageError>().is_some()
            || e.downcast_ref::<crate::actions::RefusedAnswer>().is_some()
        {
            return e;
        }
        anyhow::anyhow!("{}", self.in_words(&format!("{e:#}")))
    }
}

/// The worktree checking out `branch`, when exactly that branch is typed
/// or its directory form is.
fn by_branch<'a>(worktrees: &'a [Worktree], typed: &str) -> Option<&'a Worktree> {
    let sanitized = sanitize_branch_to_dir(typed);
    worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(typed))
        .or_else(|| worktrees.iter().find(|w| w.name == sanitized))
}

/// The error for a name that matches nothing, with the likeliest names
/// it was meant to be.
pub(super) fn unknown(typed: &str, worktrees: &[Worktree]) -> String {
    let head = format!("no worktree named {typed:?}");
    if worktrees.is_empty() {
        return format!("{head} — there are none yet; `pando new <branch>` creates one");
    }
    let suggestions = suggest(typed, worktrees);
    if !suggestions.is_empty() {
        return format!("{head} — did you mean {}?", or_list(&suggestions));
    }
    if worktrees.len() <= LIST_ALL_UP_TO {
        let names: Vec<String> = worktrees.iter().map(display_name).collect();
        return format!("{head} — this project has {}", names.join(", "));
    }
    format!("{head} — `pando ls` lists them")
}

/// Names close enough to `typed` to be what was meant: one that starts
/// with it or contains it, or one a typo or two away from it — compared
/// against both the name and the branch, since either may be what was
/// typed.
pub(super) fn suggest(typed: &str, worktrees: &[Worktree]) -> Vec<String> {
    let typed_lower = typed.to_lowercase();
    let typed_dir = sanitize_branch_to_dir(&typed_lower);
    let mut scored: Vec<(usize, String)> = Vec::new();
    for w in worktrees {
        let mut best: Option<usize> = None;
        let forms = std::iter::once(w.name.as_str()).chain(w.branch.as_deref());
        for form in forms {
            let form = form.to_lowercase();
            let score = if form.starts_with(&typed_lower) || form.starts_with(&typed_dir) {
                Some(0)
            } else if typed_lower.len() >= 3
                && (form.contains(&typed_lower) || form.contains(&typed_dir))
            {
                Some(1)
            } else {
                let distance = edit_distance(&typed_lower, &form);
                // A typo or two in a short name, a few more in a long one.
                let allowed = (typed_lower.chars().count() / 4).clamp(1, 3);
                (distance <= allowed).then_some(1 + distance)
            };
            if let Some(score) = score {
                best = Some(best.map_or(score, |b: usize| b.min(score)));
            }
        }
        if let Some(score) = best {
            scored.push((score, display_name(w)));
        }
    }
    scored.sort();
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, name)| name)
        .collect()
}

fn or_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// Levenshtein distance, the one config's "did you mean" uses too.
pub(super) use crate::config::edit_distance;

/// The linked worktree `dir` is inside, if any — the one `stop` run with
/// no name means. Never the main checkout: `stop` there stops every one,
/// main's own processes among them, as it always has.
pub(super) fn containing(paths: &PandoPaths, dir: &Path) -> Result<Option<String>> {
    Ok(containing_in(&discover_listed(paths)?, dir, false))
}

/// [`containing`], against a listing already made, and the main checkout
/// counted when `with_main` says: every verb but `stop` means it.
fn containing_in(discovery: &Discovery, dir: &Path, with_main: bool) -> Option<String> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let main = with_main.then_some(&discovery.main);
    discovery
        .worktrees
        .iter()
        .chain(main)
        .filter(|w| dir.starts_with(&w.path))
        // Nested worktrees are legal git, and the innermost is the one
        // the shell is in.
        .max_by_key(|w| w.path.components().count())
        .map(|w| w.name.clone())
}

/// The worktree a verb acts on: the one named, or else the one the shell
/// is in. A verb given no name outside every worktree is a usage error —
/// exit 2, with the two ways to say which.
pub(super) fn target(paths: &PandoPaths, name: Option<&str>, verb: &str) -> Result<String> {
    Ok(target_named(paths, name, verb)?.dir)
}

/// [`target`], as a message speaks of it. Found and named from one
/// listing of the worktrees, where finding it and then naming it listed
/// them twice.
pub(super) fn target_named(paths: &PandoPaths, typed: Option<&str>, verb: &str) -> Result<Named> {
    if let Some(typed) = typed {
        refuse_empty(typed)?;
    }
    let discovery = discover_listed(paths)?;
    let dir = match typed {
        Some(typed) => resolve_listed(paths, &discovery, typed)?,
        None => {
            let cwd = std::env::current_dir()?;
            containing_in(&discovery, &cwd, true).ok_or_else(|| {
                UsageError(format!(
                    "which worktree? `pando {verb} <name>`, or run it from inside one"
                ))
            })?
        }
    };
    Ok(Named::listed(&discovery, typed, &dir))
}
