//! Worktree discovery and git enrichment.
//!
//! The managed list comes from `git worktree list --porcelain`, not from a
//! directory scan: that is what makes "pando manages every worktree git
//! reports" true, and it means adopted worktrees can live anywhere on disk.
//! The first porcelain entry is the main checkout and is never part of the
//! managed list.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::SystemTime;

use crate::project::ProjectRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// Directory basename, as-is. Worktrees pando creates are named by
    /// sanitizing the branch; adopted ones keep whatever they have.
    pub name: String,
    /// Canonical path, except for a prunable entry whose directory is gone.
    pub path: PathBuf,
    /// Full sha of the checked-out commit, from porcelain.
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub bare: bool,
    /// Directory birthtime; drives the newest-first list order. `None` when
    /// the filesystem cannot say, or the directory is gone.
    pub created_at: Option<SystemTime>,
    /// Short sha, filled by enrichment.
    pub head_sha: Option<String>,
    pub head_subject: Option<String>,
    pub head_age: Option<String>,
    pub dirty: Option<bool>,
    pub ahead_behind: Option<(u32, u32)>,
}

impl Worktree {
    fn from_entry(entry: PorcelainEntry) -> Self {
        let name = entry
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| entry.path.to_string_lossy().to_string());
        let path = std::fs::canonicalize(&entry.path).unwrap_or(entry.path);
        let created_at = std::fs::metadata(&path).ok().and_then(|m| m.created().ok());
        Self {
            name,
            path,
            head: entry.head,
            branch: entry.branch,
            detached: entry.detached,
            prunable: entry.prunable,
            prunable_reason: entry.prunable_reason,
            locked: entry.locked,
            lock_reason: entry.lock_reason,
            bare: entry.bare,
            created_at,
            head_sha: None,
            head_subject: None,
            head_age: None,
            dirty: None,
            ahead_behind: None,
        }
    }
}

/// The main checkout plus every worktree pando manages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub main: Worktree,
    pub worktrees: Vec<Worktree>,
}

/// Every worktree git reports except the main checkout.
pub fn discover(project: &ProjectRef) -> Result<Vec<Worktree>> {
    Ok(discover_all(project)?.worktrees)
}

pub fn discover_all(project: &ProjectRef) -> Result<Discovery> {
    let text = porcelain_text(&project.root)?;
    let mut entries = parse_porcelain(&text);
    if entries.is_empty() {
        anyhow::bail!("git listed no worktrees for {}", project.root.display());
    }
    let main = Worktree::from_entry(entries.remove(0));
    let mut worktrees: Vec<Worktree> = entries.into_iter().map(Worktree::from_entry).collect();

    // Two worktrees sharing a basename would silently overwrite each other in
    // every name-keyed map pando has, so it is a hard error that names both.
    let mut seen: HashMap<String, PathBuf> = HashMap::new();
    for wt in &worktrees {
        if let Some(first) = seen.insert(wt.name.clone(), wt.path.clone()) {
            anyhow::bail!(
                "two worktrees are both named {:?}: {} and {} — rename one, pando keys worktrees by directory name",
                wt.name,
                first.display(),
                wt.path.display()
            );
        }
    }

    // Newest first: the worktree just created is the one being reached for.
    // Name breaks ties and orders entries with no birthtime, which the
    // Option ordering already puts last.
    worktrees.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Discovery { main, worktrees })
}

fn porcelain_text(root: &Path) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("spawn git worktree list")?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree list failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every path `git worktree list` reports, the main checkout included,
/// resolved the way paths are compared elsewhere. A state record whose path
/// is not in here belongs to a worktree git has forgotten; a prunable entry,
/// whose directory is gone, is still listed and so still counts.
pub fn porcelain_paths(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(parse_porcelain(&porcelain_text(root)?)
        .iter()
        .map(|e| crate::paths::resolve_for_compare(&e.path))
        .collect())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PorcelainEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub bare: bool,
}

/// Entries in the order git printed them, so the caller can rely on the
/// first being the main checkout. Unknown lines are ignored: git adds new
/// ones and an unrecognised attribute is not a reason to fail.
pub fn parse_porcelain(text: &str) -> Vec<PorcelainEntry> {
    let mut out = Vec::new();
    let mut current: Option<PorcelainEntry> = None;
    for line in text.lines() {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                out.push(entry);
            }
            current = Some(PorcelainEntry {
                path: PathBuf::from(rest),
                ..Default::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("HEAD ") {
            entry.head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            entry.branch = Some(rest.to_string());
        } else if line == "detached" {
            entry.detached = true;
        } else if line == "bare" {
            entry.bare = true;
        } else if let Some(rest) = strip_attribute(line, "locked") {
            entry.locked = true;
            entry.lock_reason = rest;
        } else if let Some(rest) = strip_attribute(line, "prunable") {
            entry.prunable = true;
            entry.prunable_reason = rest;
        }
    }
    if let Some(entry) = current.take() {
        out.push(entry);
    }
    out
}

/// `locked` and `prunable` appear bare or with a free-text reason after one
/// space. Returns `Some(reason)` for a match, `None` for a non-match.
#[allow(clippy::manual_map)]
fn strip_attribute(line: &str, name: &str) -> Option<Option<String>> {
    if line == name {
        Some(None)
    } else if let Some(rest) = line.strip_prefix(&format!("{name} ")) {
        let reason = rest.trim();
        Some(if reason.is_empty() {
            None
        } else {
            Some(reason.to_string())
        })
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichUpdate {
    pub name: String,
    pub branch: Option<String>,
    pub prunable: bool,
    pub head_sha: Option<String>,
    pub head_subject: Option<String>,
    pub head_age: Option<String>,
    pub dirty: Option<bool>,
    pub ahead_behind: Option<(u32, u32)>,
}

pub fn enrich_from_git(worktrees: &mut [Worktree], root: &Path) -> Result<()> {
    let items: Vec<(String, PathBuf)> = worktrees
        .iter()
        .map(|w| (w.name.clone(), w.path.clone()))
        .collect();
    let (tx, rx) = mpsc::channel();
    enrich_stream(root, items, tx, 16);

    let mut updates: HashMap<String, EnrichUpdate> = HashMap::new();
    while let Ok(u) = rx.recv() {
        updates.insert(u.name.clone(), u);
    }
    for wt in worktrees.iter_mut() {
        if let Some(u) = updates.remove(&wt.name) {
            apply_update(wt, u);
        }
    }
    Ok(())
}

/// `pool_cap` bounds worker parallelism: every job forks `git status` against
/// a full working tree, so a wide pool saturates the disk for the whole run.
/// Callers that block on the result want it wide; the TUI keeps it narrow so
/// startup enrichment does not starve the UI.
pub fn enrich_stream(
    root: &Path,
    items: Vec<(String, PathBuf)>,
    sender: mpsc::Sender<EnrichUpdate>,
    pool_cap: usize,
) {
    let count = items.len();
    if count == 0 {
        return;
    }
    let porcelain = porcelain_by_path(root);
    let base = resolve_base_branch(root);

    // Two repo-wide calls replace two forks per worktree: commit meta for
    // every worktree HEAD, and ahead/behind for every branch. Misses fall
    // back to a per-worktree fork.
    let shas: Vec<&str> = porcelain
        .values()
        .filter_map(|e| e.head.as_deref())
        .collect();
    let commit_meta = batch_commit_meta(root, &shas);
    let branch_counts = batch_ahead_behind(root, base.as_deref());

    let pool_size = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(pool_cap)
        .min(count)
        .max(1);

    let queue: Mutex<VecDeque<(String, PathBuf)>> = Mutex::new(items.into_iter().collect());
    let porcelain_ref = &porcelain;
    let base_ref = base.as_deref();
    let commit_meta_ref = &commit_meta;
    let branch_counts_ref = &branch_counts;
    let queue_ref = &queue;

    std::thread::scope(|s| {
        for _ in 0..pool_size {
            let tx = sender.clone();
            s.spawn(move || {
                loop {
                    let item = {
                        let mut g = queue_ref.lock().unwrap();
                        g.pop_front()
                    };
                    let Some((name, path)) = item else { break };
                    let update = enrich_one(
                        name,
                        &path,
                        porcelain_ref,
                        base_ref,
                        commit_meta_ref,
                        branch_counts_ref,
                    );
                    if tx.send(update).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

fn porcelain_by_path(root: &Path) -> HashMap<PathBuf, PorcelainEntry> {
    let Ok(text) = porcelain_text(root) else {
        return HashMap::new();
    };
    parse_porcelain(&text)
        .into_iter()
        .map(|e| {
            let key = std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone());
            (key, e)
        })
        .collect()
}

/// Short sha, subject, and relative age for every given commit from one
/// `git log --no-walk`, keyed by full sha. An empty map on failure just
/// routes callers to the per-worktree fallback.
fn batch_commit_meta(root: &Path, shas: &[&str]) -> HashMap<String, (String, String, String)> {
    if shas.is_empty() {
        return HashMap::new();
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "log",
            "--no-walk=unsorted",
            "--format=%H%x1f%h%x1f%s%x1f%cr",
        ])
        .args(shas)
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
        if let [full, short, subject, age] = parts[..] {
            map.insert(
                full.to_string(),
                (short.to_string(), subject.to_string(), age.to_string()),
            );
        }
    }
    map
}

/// Per-branch ahead/behind against the base, from one `git for-each-ref`.
/// Requires git >= 2.41 for `%(ahead-behind:...)`; on failure the empty map
/// routes callers to the per-worktree fallback.
fn batch_ahead_behind(root: &Path, base: Option<&str>) -> HashMap<String, (u32, u32)> {
    let Some(base) = base else {
        return HashMap::new();
    };
    let format = format!("%(refname:short)\x1f%(ahead-behind:{base})");
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["for-each-ref", "refs/heads", &format!("--format={format}")])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split('\x1f');
        let Some(branch) = fields.next() else {
            continue;
        };
        let Some(counts) = fields.next() else {
            continue;
        };
        let mut nums = counts.split_whitespace();
        let parsed = (|| {
            let ahead: u32 = nums.next()?.parse().ok()?;
            let behind: u32 = nums.next()?.parse().ok()?;
            Some((ahead, behind))
        })();
        if let Some(pair) = parsed {
            map.insert(branch.to_string(), pair);
        }
    }
    map
}

fn enrich_one(
    name: String,
    path: &Path,
    porcelain: &HashMap<PathBuf, PorcelainEntry>,
    base: Option<&str>,
    commit_meta: &HashMap<String, (String, String, String)>,
    branch_counts: &HashMap<String, (u32, u32)>,
) -> EnrichUpdate {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let entry = porcelain.get(&canonical);
    let (branch, prunable) = match entry {
        Some(e) => (e.branch.clone(), e.prunable),
        None => (None, false),
    };
    let batched_meta = entry
        .and_then(|e| e.head.as_ref())
        .and_then(|sha| commit_meta.get(sha));
    let (head_sha, head_subject, head_age) = match batched_meta {
        Some((short, subject, age)) => (
            Some(short.clone()),
            Some(subject.clone()),
            Some(age.clone()),
        ),
        None => match last_commit(path) {
            Ok((sha, subject, age)) => (Some(sha), Some(subject), Some(age)),
            Err(_) => (None, None, None),
        },
    };
    let dirty = is_dirty(path);
    let ahead_behind = match branch.as_deref().and_then(|b| branch_counts.get(b)) {
        Some(counts) => Some(*counts),
        // Detached, or the batch call failed — one fork for this worktree.
        None => base.and_then(|b| ahead_behind(path, b)),
    };

    EnrichUpdate {
        name,
        branch,
        prunable,
        head_sha,
        head_subject,
        head_age,
        dirty,
        ahead_behind,
    }
}

pub fn apply_update(wt: &mut Worktree, u: EnrichUpdate) {
    wt.branch = u.branch;
    wt.prunable = u.prunable;
    wt.head_sha = u.head_sha;
    wt.head_subject = u.head_subject;
    wt.head_age = u.head_age;
    wt.dirty = u.dirty;
    wt.ahead_behind = u.ahead_behind;
}

fn last_commit(path: &Path) -> Result<(String, String, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["log", "-1", "--format=%h%x1f%s%x1f%cr", "HEAD"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git log failed at {}", path.display());
    }
    let text = String::from_utf8(out.stdout)?;
    let parts: Vec<&str> = text.trim_end_matches('\n').splitn(3, '\x1f').collect();
    if parts.len() != 3 {
        anyhow::bail!("unexpected git log output: {text:?}");
    }
    Ok((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

/// The branch new worktrees fork from and existing ones are measured
/// against: what `origin/HEAD` points at, else `main`, else `master`.
/// Generic on purpose — no project-specific branch convention lives here.
pub fn resolve_base_branch(root: &Path) -> Option<String> {
    if let Some(head) = origin_head(root) {
        return Some(head);
    }
    for candidate in ["main", "master"] {
        if let Some(found) = first_existing_ref(root, &[&format!("origin/{candidate}"), candidate])
        {
            return Some(found);
        }
    }
    None
}

fn origin_head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "symbolic-ref",
            "--short",
            "--quiet",
            "refs/remotes/origin/HEAD",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() { None } else { Some(name) }
}

fn first_existing_ref(root: &Path, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|c| ref_resolves(root, c))
        .map(|c| c.to_string())
}

fn ref_resolves(root: &Path, refname: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{refname}^{{commit}}"),
        ])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// `--no-optional-locks` so a status probe never writes an index lock into
/// the worktree — invariant 1 covers `.git` too.
fn is_dirty(wt_path: &Path) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt_path)
        .args(["--no-optional-locks", "status", "--porcelain", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!out.stdout.is_empty())
}

fn ahead_behind(wt_path: &Path, base: &str) -> Option<(u32, u32)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt_path)
        .args([
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{base}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut parts = text.split_whitespace();
    let ahead: u32 = parts.next()?.parse().ok()?;
    let behind: u32 = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchSource {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchEntry {
    pub name: String,
    pub source: BranchSource,
}

/// Local branches, then remote-only branches on `origin`, each alphabetical.
/// Empty on any git failure — the create modal degrades to typing a name.
pub fn list_branches(root: &Path) -> Vec<BranchEntry> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes/origin",
        ])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut local: Vec<String> = Vec::new();
    let mut remote: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("refs/heads/") {
            if !rest.is_empty() {
                local.push(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("refs/remotes/origin/") {
            if rest.is_empty() || rest == "HEAD" {
                continue;
            }
            remote.push(rest.to_string());
        }
    }
    local.sort();
    remote.sort();

    let local_set: HashSet<&String> = local.iter().collect();
    let mut entries: Vec<BranchEntry> = Vec::with_capacity(local.len() + remote.len());
    for name in &local {
        entries.push(BranchEntry {
            name: name.clone(),
            source: BranchSource::Local,
        });
    }
    for name in &remote {
        if local_set.contains(name) {
            continue;
        }
        entries.push(BranchEntry {
            name: name.clone(),
            source: BranchSource::Remote,
        });
    }
    entries
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrInfo {
    pub number: u32,
    pub title: String,
    pub branch: String,
    pub author: String,
    pub draft: bool,
    pub state: PrState,
    pub url: String,
}

/// Pull requests via the `gh` CLI, which resolves the repository from the
/// checkout's origin remote. Errors carry enough context to explain a
/// missing or unauthenticated `gh`; callers treat that as "no chips".
pub fn list_prs(root: &Path) -> Result<Vec<PrInfo>> {
    let out = Command::new("gh")
        .current_dir(root)
        .args([
            "pr",
            "list",
            "--state",
            "all",
            "--limit",
            "300",
            "--json",
            "number,title,headRefName,author,isDraft,state,url",
        ])
        .output()
        .context("spawn gh — is the GitHub CLI installed?")?;
    if !out.status.success() {
        anyhow::bail!(
            "gh pr list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_pr_list(&String::from_utf8_lossy(&out.stdout))
}

fn parse_pr_list(json: &str) -> Result<Vec<PrInfo>> {
    #[derive(serde::Deserialize)]
    struct RawAuthor {
        login: String,
    }
    #[derive(serde::Deserialize)]
    struct RawPr {
        number: u32,
        title: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        author: RawAuthor,
        #[serde(rename = "isDraft")]
        is_draft: bool,
        state: String,
        url: String,
    }
    let raw: Vec<RawPr> = serde_json::from_str(json).context("parse gh pr list JSON")?;
    Ok(raw
        .into_iter()
        .map(|p| PrInfo {
            number: p.number,
            title: p.title,
            branch: p.head_ref_name,
            author: p.author.login,
            draft: p.is_draft,
            state: match p.state.as_str() {
                "OPEN" => PrState::Open,
                "MERGED" => PrState::Merged,
                _ => PrState::Closed,
            },
            url: p.url,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{git, init_repo};
    use tempfile::{TempDir, tempdir};

    fn project_at(path: &Path) -> ProjectRef {
        ProjectRef::from_root(path).unwrap()
    }

    /// A repo with one commit and a place to put linked worktrees.
    fn repo_with_worktrees(names: &[(&str, &str)]) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let base = dir.path().join("trees");
        std::fs::create_dir_all(&base).unwrap();
        for (dir_name, branch) in names {
            let path = base.join(dir_name);
            git(
                &repo,
                &["worktree", "add", "-b", branch, path.to_str().unwrap()],
            );
        }
        (dir, repo)
    }

    #[test]
    fn parses_every_entry_shape_git_prints() {
        let text = "\
worktree /repo
HEAD abc123
branch refs/heads/main

worktree /trees/feat+detached
HEAD def456
detached

worktree /trees/feat+gone
HEAD abc123
branch refs/heads/feat/gone
prunable gitdir file points to non-existent location

worktree /trees/feat+locked
HEAD abc123
branch refs/heads/feat/locked
locked busy testing

worktree /trees/feat+plainlock
HEAD abc123
locked

worktree /bare.git
bare

";
        let entries = parse_porcelain(text);
        assert_eq!(entries.len(), 6);

        assert_eq!(entries[0].path, PathBuf::from("/repo"));
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[0].head.as_deref(), Some("abc123"));

        assert!(entries[1].detached);
        assert!(entries[1].branch.is_none());

        assert!(entries[2].prunable);
        assert_eq!(
            entries[2].prunable_reason.as_deref(),
            Some("gitdir file points to non-existent location")
        );

        assert!(entries[3].locked);
        assert_eq!(entries[3].lock_reason.as_deref(), Some("busy testing"));

        assert!(entries[4].locked, "a bare `locked` line still means locked");
        assert_eq!(entries[4].lock_reason, None);

        assert!(entries[5].bare);
    }

    #[test]
    fn parsing_ignores_unknown_lines_and_survives_a_missing_trailing_blank() {
        let entries = parse_porcelain(
            "worktree /repo\nsomething-git-added-later value\nHEAD abc\nbranch refs/heads/main",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn discover_excludes_the_main_checkout() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+one", "feat/one")]);
        let project = project_at(&repo);
        let discovery = discover_all(&project).unwrap();

        assert_eq!(discovery.main.path, project.root);
        assert_eq!(discovery.main.branch.as_deref(), Some("main"));
        let names: Vec<&str> = discovery
            .worktrees
            .iter()
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(names, vec!["feat+one"]);
    }

    #[test]
    fn discover_is_empty_when_the_repository_has_only_a_main_checkout() {
        let (_dir, repo) = repo_with_worktrees(&[]);
        assert!(discover(&project_at(&repo)).unwrap().is_empty());
    }

    #[test]
    fn discovered_names_are_basenames_and_paths_are_canonical() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+checkout", "feat/checkout")]);
        let wts = discover(&project_at(&repo)).unwrap();
        assert_eq!(wts[0].name, "feat+checkout");
        assert_eq!(
            wts[0].path,
            std::fs::canonicalize(&wts[0].path).unwrap(),
            "paths must already be canonical — git prints /private/var on macOS"
        );
        assert!(wts[0].created_at.is_some());
    }

    #[test]
    fn discover_sorts_newest_first() {
        let (_dir, repo) = repo_with_worktrees(&[]);
        let base = repo.parent().unwrap().join("trees");
        std::fs::create_dir_all(&base).unwrap();
        // Creation order deliberately opposes alphabetical order.
        for n in ["mango", "apple", "zed"] {
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &format!("feat/{n}"),
                    base.join(n).to_str().unwrap(),
                ],
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let names: Vec<String> = discover(&project_at(&repo))
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["zed", "apple", "mango"]);
    }

    #[test]
    fn two_worktrees_with_the_same_basename_are_an_error_naming_both() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let a = dir.path().join("a").join("shared");
        let b = dir.path().join("b").join("shared");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        git(
            &repo,
            &["worktree", "add", "-b", "one", a.to_str().unwrap()],
        );
        git(
            &repo,
            &["worktree", "add", "-b", "two", b.to_str().unwrap()],
        );

        let err = discover(&project_at(&repo)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("shared"), "{msg}");
        assert!(
            msg.contains(&a.canonicalize().unwrap().display().to_string()),
            "{msg}"
        );
        assert!(
            msg.contains(&b.canonicalize().unwrap().display().to_string()),
            "{msg}"
        );
    }

    #[test]
    fn discover_reports_prunable_and_locked_entries_with_their_reasons() {
        let (_dir, repo) =
            repo_with_worktrees(&[("feat+gone", "feat/gone"), ("feat+lk", "feat/lk")]);
        let base = repo.parent().unwrap().join("trees");
        std::fs::remove_dir_all(base.join("feat+gone")).unwrap();
        git(
            &repo,
            &[
                "worktree",
                "lock",
                "--reason",
                "holding this one",
                base.join("feat+lk").to_str().unwrap(),
            ],
        );

        let wts = discover(&project_at(&repo)).unwrap();
        let gone = wts.iter().find(|w| w.name == "feat+gone").unwrap();
        assert!(gone.prunable);
        assert!(gone.prunable_reason.is_some());
        let locked = wts.iter().find(|w| w.name == "feat+lk").unwrap();
        assert!(locked.locked);
        assert_eq!(locked.lock_reason.as_deref(), Some("holding this one"));
    }

    #[test]
    fn enrichment_fills_branch_sha_subject_and_age() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+x", "feat/x")]);
        let path = repo.parent().unwrap().join("trees").join("feat+x");
        git(&path, &["commit", "--allow-empty", "-m", "add feature x"]);

        let mut wts = discover(&project_at(&repo)).unwrap();
        enrich_from_git(&mut wts, &repo).unwrap();

        let w = &wts[0];
        assert_eq!(w.branch.as_deref(), Some("feat/x"));
        assert!(w.head_sha.as_deref().is_some_and(|s| !s.is_empty()));
        assert_eq!(w.head_subject.as_deref(), Some("add feature x"));
        assert!(w.head_age.is_some());
        assert_eq!(w.dirty, Some(false));
    }

    #[test]
    fn dirty_is_true_for_an_untracked_or_modified_file() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+u", "feat/u"), ("feat+m", "feat/m")]);
        let trees = repo.parent().unwrap().join("trees");
        std::fs::write(trees.join("feat+u").join("scratch.txt"), "hello").unwrap();

        let modified = trees.join("feat+m");
        std::fs::write(modified.join("a.txt"), "first").unwrap();
        git(&modified, &["add", "a.txt"]);
        git(&modified, &["commit", "-m", "add a"]);
        std::fs::write(modified.join("a.txt"), "changed").unwrap();

        let mut wts = discover(&project_at(&repo)).unwrap();
        enrich_from_git(&mut wts, &repo).unwrap();
        for w in &wts {
            assert_eq!(w.dirty, Some(true), "{} should be dirty", w.name);
        }
    }

    #[test]
    fn ahead_and_behind_are_counted_against_the_base() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+ab", "feat/ab")]);
        let path = repo.parent().unwrap().join("trees").join("feat+ab");
        git(&path, &["commit", "--allow-empty", "-m", "ahead 1"]);
        git(&path, &["commit", "--allow-empty", "-m", "ahead 2"]);
        git(&repo, &["commit", "--allow-empty", "-m", "main 1"]);

        let mut wts = discover(&project_at(&repo)).unwrap();
        enrich_from_git(&mut wts, &repo).unwrap();
        assert_eq!(wts[0].ahead_behind, Some((2, 1)));
    }

    // A detached-HEAD worktree has no branch for the batched lookups, so it
    // exercises the per-worktree fallbacks — and must enrich just as fully.
    #[test]
    fn a_detached_worktree_enriches_through_the_fallbacks() {
        let (_dir, repo) = repo_with_worktrees(&[("feat+d", "feat/d")]);
        let path = repo.parent().unwrap().join("trees").join("feat+d");
        git(&path, &["commit", "--allow-empty", "-m", "on branch"]);
        git(&path, &["checkout", "--detach"]);

        let mut wts = discover(&project_at(&repo)).unwrap();
        assert!(wts[0].detached);
        enrich_from_git(&mut wts, &repo).unwrap();

        let w = &wts[0];
        assert!(w.branch.is_none());
        assert_eq!(w.head_subject.as_deref(), Some("on branch"));
        assert_eq!(w.ahead_behind, Some((1, 0)));
        assert_eq!(w.dirty, Some(false));
    }

    #[test]
    fn enrich_stream_emits_one_update_per_worktree() {
        let (_dir, repo) = repo_with_worktrees(&[
            ("alpha", "feat/alpha"),
            ("bravo", "feat/bravo"),
            ("charlie", "feat/charlie"),
            ("delta", "feat/delta"),
        ]);
        let wts = discover(&project_at(&repo)).unwrap();
        let items: Vec<(String, PathBuf)> = wts
            .iter()
            .map(|w| (w.name.clone(), w.path.clone()))
            .collect();

        let (tx, rx) = mpsc::channel();
        enrich_stream(&repo, items, tx, 16);
        let mut names: Vec<String> = rx.iter().map(|u| u.name).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "bravo", "charlie", "delta"]);
    }

    #[test]
    fn enrichment_degrades_to_none_when_git_cannot_answer() {
        let dir = tempdir().unwrap();
        let ghost = dir.path().join("ghost");
        std::fs::create_dir_all(&ghost).unwrap();
        let mut wts = vec![Worktree {
            name: "ghost".into(),
            path: ghost,
            head: None,
            branch: None,
            detached: false,
            prunable: false,
            prunable_reason: None,
            locked: false,
            lock_reason: None,
            bare: false,
            created_at: None,
            head_sha: None,
            head_subject: None,
            head_age: None,
            dirty: None,
            ahead_behind: None,
        }];
        enrich_from_git(&mut wts, dir.path()).unwrap();
        assert!(wts[0].dirty.is_none());
        assert!(wts[0].ahead_behind.is_none());
    }

    /// A bare remote plus a clone, so remote-tracking refs exist.
    fn repo_with_origin(dir: &Path, extra_remote_branches: &[&str]) -> PathBuf {
        let bare = dir.join("origin.git");
        git(
            dir,
            &[
                "init",
                "--bare",
                "--quiet",
                "--initial-branch=main",
                bare.to_str().unwrap(),
            ],
        );
        let seed = dir.join("seed");
        git(
            dir,
            &[
                "clone",
                "--quiet",
                bare.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        git(&seed, &["commit", "--allow-empty", "-m", "origin main 1"]);
        git(&seed, &["push", "--quiet", "origin", "main"]);
        for branch in extra_remote_branches {
            git(&seed, &["checkout", "--quiet", "-b", branch]);
            git(&seed, &["commit", "--allow-empty", "-m", "remote work"]);
            git(&seed, &["push", "--quiet", "origin", branch]);
        }
        let repo = dir.join("repo");
        git(
            dir,
            &[
                "clone",
                "--quiet",
                bare.to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
        );
        repo
    }

    #[test]
    fn the_base_branch_comes_from_origin_head_then_main_then_master() {
        let dir = tempdir().unwrap();
        let cloned = repo_with_origin(dir.path(), &[]);
        assert_eq!(
            resolve_base_branch(&cloned).as_deref(),
            Some("origin/main"),
            "a clone knows origin/HEAD"
        );

        let local = dir.path().join("local");
        init_repo(&local);
        assert_eq!(resolve_base_branch(&local).as_deref(), Some("main"));

        let master = dir.path().join("master-repo");
        std::fs::create_dir_all(&master).unwrap();
        git(&master, &["init", "--quiet", "--initial-branch=master"]);
        git(
            &master,
            &["commit", "--quiet", "--allow-empty", "-m", "root"],
        );
        assert_eq!(resolve_base_branch(&master).as_deref(), Some("master"));

        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        git(&empty, &["init", "--quiet", "--initial-branch=trunk"]);
        git(
            &empty,
            &["commit", "--quiet", "--allow-empty", "-m", "root"],
        );
        assert_eq!(resolve_base_branch(&empty), None);
    }

    #[test]
    fn ahead_behind_measures_against_origin_not_a_stale_local_branch() {
        let dir = tempdir().unwrap();
        let repo = repo_with_origin(dir.path(), &[]);
        git(&repo, &["commit", "--allow-empty", "-m", "local extra 1"]);
        git(&repo, &["commit", "--allow-empty", "-m", "local extra 2"]);

        let trees = dir.path().join("trees");
        std::fs::create_dir_all(&trees).unwrap();
        let path = trees.join("feat+o");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                "feat/o",
                path.to_str().unwrap(),
                "origin/main",
            ],
        );
        git(&path, &["commit", "--allow-empty", "-m", "wt c1"]);

        let mut wts = discover(&project_at(&repo)).unwrap();
        enrich_from_git(&mut wts, &repo).unwrap();
        assert_eq!(
            wts[0].ahead_behind,
            Some((1, 0)),
            "counts must be against origin/main, not the local main (which would be (1, 2))"
        );
    }

    #[test]
    fn list_branches_returns_locals_then_remote_only_branches() {
        let dir = tempdir().unwrap();
        let repo = repo_with_origin(dir.path(), &["remote-only"]);
        git(&repo, &["branch", "local-only"]);

        let entries = list_branches(&repo);
        let names: Vec<(&str, &BranchSource)> = entries
            .iter()
            .map(|e| (e.name.as_str(), &e.source))
            .collect();
        assert_eq!(
            names,
            vec![
                ("local-only", &BranchSource::Local),
                ("main", &BranchSource::Local),
                ("remote-only", &BranchSource::Remote),
            ]
        );
    }

    #[test]
    fn list_branches_is_empty_outside_a_repository() {
        let dir = tempdir().unwrap();
        assert!(list_branches(dir.path()).is_empty());
    }

    #[test]
    fn parse_pr_list_maps_the_gh_json_fields() {
        let json = r#"[
            {"author":{"login":"dev"},"headRefName":"fix/one","isDraft":true,"number":435,
             "title":"fix: one","state":"OPEN","url":"https://github.com/org/repo/pull/435"},
            {"author":{"login":"dev"},"headRefName":"docs/two","isDraft":false,"number":427,
             "title":"docs: two","state":"MERGED","url":"https://github.com/org/repo/pull/427"},
            {"author":{"login":"dev"},"headRefName":"feat/three","isDraft":false,"number":400,
             "title":"abandoned","state":"CLOSED","url":"https://github.com/org/repo/pull/400"}
        ]"#;
        let prs = parse_pr_list(json).unwrap();
        assert_eq!(prs.len(), 3);
        assert_eq!(prs[0].number, 435);
        assert_eq!(prs[0].branch, "fix/one");
        assert!(prs[0].draft);
        assert_eq!(prs[0].state, PrState::Open);
        assert_eq!(prs[1].state, PrState::Merged);
        assert_eq!(prs[2].state, PrState::Closed);
    }

    #[test]
    fn parse_pr_list_rejects_malformed_json_with_context() {
        let err = parse_pr_list("not json").unwrap_err();
        assert!(format!("{err:#}").contains("parse gh pr list JSON"));
    }
}
