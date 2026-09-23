//! Adoption: project folders a moved repository left behind, and `doctor
//! --adopt`.

use anyhow::Context as _;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::paths::PandoPaths;
use crate::state;

use super::render::plural;

/// A project folder under pando's home whose repository is no longer where
/// it was.
///
/// The project id is the main checkout's directory name plus a hash of its
/// canonical path, so moving a repository gives it a new id and an empty
/// folder beside the old one — with the old one still holding the config,
/// the state and, by default, the worktrees.
#[derive(Debug, Clone, Serialize)]
pub struct Adoptable {
    pub id: String,
    pub path: String,
    /// The repository that folder belonged to, when it can be worked out —
    /// from a worktree's own `.git` file, which names the repository it
    /// was linked from.
    pub old_root: Option<String>,
    /// The worktree directories still sitting inside it.
    pub worktrees: Vec<String>,
}

/// What `--adopt` would do, for the confirmation.
#[derive(Debug, Clone)]
pub struct AdoptPlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub old_root: Option<PathBuf>,
    pub worktrees: Vec<String>,
}

/// What `--adopt` did.
#[derive(Debug, Clone)]
pub struct Adoption {
    pub from: PathBuf,
    pub to: PathBuf,
    /// How many recorded paths moved with the folder.
    pub rewritten: usize,
    /// Anything worth saying that is not a failure: what git had to be
    /// told, and what it said back.
    pub notices: Vec<String>,
}

pub(super) fn adoptable(paths: &PandoPaths) -> Vec<Adoptable> {
    let Ok(entries) = std::fs::read_dir(paths.projects_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        if id == paths.project_id() || !entry.path().is_dir() {
            continue;
        }
        // The id is `<directory name>-<8 hex>`, so the name in front of
        // the hash is the repository's own directory name — which is what
        // survives a move, and the only evidence that this folder is about
        // *this* repository rather than some other project.
        let Some((display, hash)) = id.rsplit_once('-') else {
            continue;
        };
        if display != paths.project.display_name
            || hash.len() != 8
            || !hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        let old_root = recorded_root(&entry.path());
        // A folder whose repository is still there belongs to a live
        // project — two checkouts of the same repository under different
        // directories is an ordinary thing, and adopting one of them would
        // be stealing it.
        if old_root.as_deref().is_some_and(Path::exists) {
            continue;
        }
        out.push(Adoptable {
            id,
            path: entry.path().display().to_string(),
            old_root: old_root.map(|p| p.display().to_string()),
            worktrees: worktree_names(&entry.path()),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The repository a project folder belonged to.
///
/// Nothing writes `project.root`, so the honest source is a worktree's own
/// `.git` file: git writes `gitdir: <repository>/.git/worktrees/<name>`
/// into it, and that names the checkout it was linked from. A folder with
/// no worktrees left has nothing to say, and says `None` rather than a
/// guess.
fn recorded_root(project_dir: &Path) -> Option<PathBuf> {
    if let Some(root) = std::fs::read_to_string(project_dir.join("pando.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            Some(PathBuf::from(
                doc.get("project")?.get("root")?.as_str()?.to_string(),
            ))
        })
    {
        return Some(root);
    }
    for name in worktree_names(project_dir) {
        let marker = project_dir.join("worktrees").join(&name).join(".git");
        let Ok(text) = std::fs::read_to_string(&marker) else {
            continue;
        };
        let Some(gitdir) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
            continue;
        };
        let gitdir = PathBuf::from(gitdir.trim());
        // `<root>/.git/worktrees/<name>` — three components back to the
        // checkout it was linked from. Absolute, because git writes it
        // absolute and a relative one climbing out of three components
        // leaves an empty path that would read as "its repository was at
        // , and is not there now".
        if let Some(root) = gitdir.ancestors().nth(3)
            && root.is_absolute()
        {
            return Some(root.to_path_buf());
        }
    }
    None
}

fn worktree_names(project_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_dir.join("worktrees")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Moves a project folder under this repository's current id.
///
/// The one thing `doctor` does rather than reports, which is why it asks
/// first. Nothing inside the folder changes except the paths that named the
/// folder itself: the config is the same file, the state is the same
/// records, and the worktrees are the same directories.
///
/// Two things have to be put back afterwards, and both are consequences of
/// the folder having moved rather than decisions of pando's. pando's own
/// records hold absolute paths, so every one that pointed inside the old
/// folder is rewritten to point inside the new one. And git holds absolute
/// paths in both directions — `.git/worktrees/<name>/gitdir` in the
/// repository, and the `.git` file in each worktree — so `git worktree
/// repair` is run to reconnect them. That is the one command git documents
/// for exactly this, and Invariant 1 names what `git worktree` records
/// inside `.git` as pando's to touch.
pub fn adopt(
    paths: &PandoPaths,
    old_id: &str,
    confirm: &dyn Fn(&AdoptPlan) -> anyhow::Result<bool>,
) -> anyhow::Result<Adoption> {
    if old_id == paths.project_id() {
        anyhow::bail!("{old_id} is this repository's own project folder");
    }
    // Exactly one ordinary directory name. `.` and `..` are each one
    // component too, and either of them would make the destination a
    // subdirectory of the source.
    let mut parts = Path::new(old_id).components();
    let one_plain_name =
        matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none();
    if old_id.is_empty() || !one_plain_name {
        anyhow::bail!("{old_id:?} is not a project id — it is one directory name under projects/");
    }
    let from = paths.projects_dir().join(old_id);
    if !from.is_dir() {
        anyhow::bail!(
            "there is no project folder {old_id:?} in {} — `pando doctor` lists the ones there \
             are",
            paths.projects_dir().display()
        );
    }
    let to = paths.project_dir();
    if to.exists() {
        anyhow::bail!(
            "{} already exists — pando will not merge two project folders, and moving {old_id} \
             on top of it would lose whichever it overwrote",
            to.display()
        );
    }
    let old_root = recorded_root(&from);
    if let Some(root) = &old_root
        && root.exists()
    {
        anyhow::bail!(
            "{old_id} belongs to the repository at {} — it is still there, so this is not a \
             repository that moved",
            root.display()
        );
    }
    // Before anything moves, because everything after the rename is
    // reconnecting and none of it can be retried: `--adopt` looks for the
    // folder by its old id, and after the move there is no folder by that
    // name to look for. A state file this build cannot read is exactly
    // the shape an *old* folder has — it is by construction from before
    // the move — and it is the one thing that would leave every recorded
    // path pointing at a directory that is no longer there.
    if let Err(e) = state::load(&from.join("state.json")) {
        anyhow::bail!(
            "{e:#} — that file has to be readable before the folder moves, because the paths in \
             it all name the folder it is in; fix it, or move it aside and adopt again"
        );
    }
    let plan = AdoptPlan {
        from: from.clone(),
        to: to.clone(),
        old_root,
        worktrees: worktree_names(&from),
    };
    if !confirm(&plan)? {
        anyhow::bail!("nothing was moved");
    }

    // Creates `projects/` the one way that makes the home 0700 and refuses
    // one inside the repository.
    paths.ensure_home()?;
    // `ensure_home` creates the *new* project directory, and a rename onto
    // an existing directory fails. It is pando's own, it was created a
    // moment ago, and it is empty.
    if to.is_dir() {
        std::fs::remove_dir(&to)
            .with_context(|| format!("clear the empty {} to move onto it", to.display()))?;
    }
    std::fs::rename(&from, &to)
        .with_context(|| format!("move {} to {}", from.display(), to.display()))?;

    // Past this point the folder has moved, which is the part that
    // cannot be done by hand. Everything left is reconnecting, and a
    // failure in it is reported rather than raised: an error after a
    // successful move would read as "it did not happen".
    let mut notices = Vec::new();
    let rewritten = match rewrite_recorded_paths(paths, &from, &to) {
        Ok(rewritten) => rewritten,
        Err(e) => {
            notices.push(format!(
                "the folder moved, and pando's own records still point inside the old one:                  {e:#} — `pando rm` and `pando start` will not find those worktrees until the                  state file is fixed or moved aside"
            ));
            0
        }
    };
    notices.extend(repair_worktrees(paths, &to));
    Ok(Adoption {
        from,
        to,
        rewritten,
        notices,
    })
}

/// Every recorded path that pointed inside the old folder, pointed inside
/// the new one.
///
/// Worktree directories, process logs and a share's log all live under the
/// project folder by default, and all three are recorded absolute. A
/// `worktrees_dir` the developer put somewhere else is not under the old
/// folder, so it is not touched — which is the right answer for it too.
fn rewrite_recorded_paths(paths: &PandoPaths, from: &Path, to: &Path) -> anyhow::Result<usize> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let mut moved_count = 0usize;
    for record in store.worktrees.values_mut() {
        if let Some(path) = moved(&record.path, from, to) {
            record.path = path;
            moved_count += 1;
        }
        for process in record.processes.values_mut() {
            if let Some(path) = moved(&process.log_path, from, to) {
                process.log_path = path;
                moved_count += 1;
            }
        }
        if let Some(share) = record.share.as_mut()
            && let Some(path) = moved(&share.log_path, from, to)
        {
            share.log_path = path;
            moved_count += 1;
        }
    }
    if moved_count > 0 {
        state::save(&paths.state_file(), &store)?;
    }
    Ok(moved_count)
}

/// `path` with the `from` prefix replaced by `to`, or `None` when it was
/// never inside `from`.
///
/// Compared through `resolve_for_compare`, because a recorded path was
/// canonicalised when it was written and macOS spells the same directory
/// `/var/…` and `/private/var/…`.
fn moved(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let resolved = crate::paths::resolve_for_compare(path);
    let prefix = crate::paths::resolve_for_compare(from);
    // Rebuilt from the canonical destination, so a record keeps the
    // spelling every other path in the file has — `record_is_for` compares
    // these against a canonicalised worktree path.
    let destination = crate::paths::resolve_for_compare(to);
    resolved
        .strip_prefix(&prefix)
        .ok()
        .map(|rest| destination.join(rest))
}

/// Tells git where the worktrees are now.
///
/// After a move, both halves of git's bookkeeping are wrong: the `.git`
/// file in each worktree names a repository that is not there, and
/// `.git/worktrees/<name>/gitdir` in the repository names a worktree that
/// is not there. `git worktree repair` is git's own command for exactly
/// this, and it writes only inside `.git`, which Invariant 1 allows.
///
/// A failure here is reported, never fatal: the folder has already moved,
/// which is the part that cannot be done by hand.
fn repair_worktrees(paths: &PandoPaths, project_dir: &Path) -> Vec<String> {
    let names = worktree_names(project_dir);
    if names.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec!["worktree".to_string(), "repair".to_string()];
    args.extend(names.iter().map(|name| {
        project_dir
            .join("worktrees")
            .join(name)
            .display()
            .to_string()
    }));
    let out = crate::project::git(paths.root(), &args);
    match out {
        Ok(out) if out.status.success() => {
            let mut notices = vec![format!(
                "told git where {} {} now: git worktree repair",
                names.len(),
                plural(names.len(), "worktree")
            )];
            notices.extend(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(|l| format!("git: {l}")),
            );
            notices
        }
        Ok(out) => vec![format!(
            "git could not repair the worktrees: {} — run `git worktree repair` in {} by hand",
            String::from_utf8_lossy(&out.stderr).trim(),
            paths.root().display()
        )],
        Err(e) => vec![format!(
            "git could not be run to repair the worktrees ({e}) — run `git worktree repair` in \
             {} by hand",
            paths.root().display()
        )],
    }
}
