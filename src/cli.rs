//! The clap front end. A thin wrapper: every behaviour lives in `actions`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::Write;

use crate::actions;
use crate::cache;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::worktree::{PrState, Worktree};

/// Shape version for machine-readable output, bumped independently of the
/// crate version so agents can pin what they parse.
pub const JSON_VERSION: u32 = 1;

#[derive(Parser, Debug)]
#[command(name = "pando", version, about = "One repo. Every branch alive.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a worktree and branch from the default base.
    New {
        /// Branch name. Slashes become plus signs in the directory name.
        branch: String,
        /// Base to fork a new branch from. A bare name prefers the
        /// remote-tracking ref, so a stale local branch is never the fork
        /// point.
        #[arg(long)]
        base: Option<String>,
    },
    /// List worktrees with their git state.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove a worktree and wipe its pando data. The branch is kept.
    Rm {
        name: String,
        /// Confirm removing a worktree pando did not create.
        #[arg(long)]
        yes: bool,
        /// Let git discard modified or untracked files.
        #[arg(long)]
        force: bool,
    },
    /// Print a worktree's absolute path.
    Path { name: String },
}

pub fn dispatch(command: Command, paths: &PandoPaths, config: &Config) -> Result<()> {
    let mut out = std::io::stdout();
    match command {
        Command::New { branch, base } => {
            let name = actions::new(paths, config, &branch, base.as_deref(), &|msg| {
                eprintln!("pando: {msg}");
            })?;
            writeln!(
                out,
                "created {name} at {}",
                config.worktrees_dir(paths).join(&name).display()
            )?;
            Ok(())
        }
        Command::Ls { json } => {
            if json {
                ls_json(paths, &mut out)
            } else {
                ls_text(paths, &mut out)
            }
        }
        Command::Rm { name, yes, force } => {
            actions::rm(paths, config, &name, yes, force)?;
            writeln!(out, "removed {name}")?;
            Ok(())
        }
        Command::Path { name } => {
            writeln!(out, "{}", actions::path(paths, &name)?.display())?;
            Ok(())
        }
    }
}

pub fn ls_text<W: Write>(paths: &PandoPaths, out: &mut W) -> Result<()> {
    let worktrees = actions::ls(paths)?;
    let owned = actions::created_by_pando(paths);
    if worktrees.is_empty() {
        writeln!(out, "no worktrees — `pando new <branch>` creates one")?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<24} {:<24} {:<8} {:<7} PATH",
        "NAME", "BRANCH", "HEAD", "STATE"
    )?;
    for w in &worktrees {
        writeln!(
            out,
            "{:<24} {:<24} {:<8} {:<7} {}",
            ellipsize(&w.name, 24),
            ellipsize(w.branch.as_deref().unwrap_or("(detached)"), 24),
            w.head_sha.as_deref().unwrap_or("-"),
            state_word(w, owned.get(&w.name).copied().unwrap_or(false)),
            w.path.display()
        )?;
    }
    Ok(())
}

/// One word per worktree for the text listing, ordered by how much it should
/// stop you: a gone or locked entry first, then dirty, then ownership.
fn state_word(w: &Worktree, created_by_pando: bool) -> &'static str {
    if w.prunable {
        "gone"
    } else if w.locked {
        "locked"
    } else if w.dirty == Some(true) {
        "dirty"
    } else if created_by_pando {
        "pando"
    } else {
        "adopted"
    }
}

#[derive(Serialize)]
struct LsOutput {
    version: u32,
    project: ProjectOut,
    worktrees: Vec<WorktreeOut>,
}

#[derive(Serialize)]
struct ProjectOut {
    id: String,
    root: String,
    name: String,
}

#[derive(Serialize)]
struct WorktreeOut {
    name: String,
    path: String,
    branch: Option<String>,
    head: Option<String>,
    detached: bool,
    /// `null` when git could not be asked.
    dirty: Option<bool>,
    ahead: Option<u32>,
    behind: Option<u32>,
    created_by_pando: bool,
    prunable: bool,
    /// `null` when unlocked; the lock reason, possibly empty, when locked.
    locked: Option<String>,
    pr: Option<PrOut>,
}

#[derive(Serialize)]
struct PrOut {
    number: u32,
    state: PrState,
    url: String,
}

pub fn ls_json<W: Write>(paths: &PandoPaths, out: &mut W) -> Result<()> {
    let worktrees = actions::ls(paths)?;
    let owned = actions::created_by_pando(paths);
    // Cache only: the CLI never spawns `gh`, so `ls --json` stays fast and
    // works offline. The TUI is what refreshes this.
    let prs = cache::load_prs(&paths.pr_cache_file());

    let output = LsOutput {
        version: JSON_VERSION,
        project: ProjectOut {
            id: paths.project.id.clone(),
            root: paths.project.root.display().to_string(),
            name: paths.project.display_name.clone(),
        },
        worktrees: worktrees
            .into_iter()
            .map(|w| {
                let pr = w
                    .branch
                    .as_deref()
                    .and_then(|b| prs.prs.get(b))
                    .map(|p| PrOut {
                        number: p.number,
                        state: p.state,
                        url: p.url.clone(),
                    });
                WorktreeOut {
                    created_by_pando: owned.get(&w.name).copied().unwrap_or(false),
                    name: w.name,
                    path: w.path.display().to_string(),
                    branch: w.branch,
                    head: w.head_sha.or(w.head),
                    detached: w.detached,
                    dirty: w.dirty,
                    ahead: w.ahead_behind.map(|(a, _)| a),
                    behind: w.ahead_behind.map(|(_, b)| b),
                    prunable: w.prunable,
                    locked: if w.locked {
                        Some(w.lock_reason.unwrap_or_default())
                    } else {
                        None
                    },
                    pr,
                }
            })
            .collect(),
    };
    writeln!(out, "{}", serde_json::to_string_pretty(&output)?)?;
    Ok(())
}

/// Truncate to at most `max` chars with a trailing ellipsis, so a long name
/// never overflows its column.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::testutil::git;
    use clap::CommandFactory;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    struct Fx {
        _dir: TempDir,
        root: PathBuf,
        paths: PandoPaths,
        config: Config,
    }

    fn fixture() -> Fx {
        let dir = tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "--quiet", "-m", "root"]);
        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        Fx {
            root: paths.root().to_path_buf(),
            paths,
            config: Config::default(),
            _dir: dir,
        }
    }

    fn capture(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn ls_text_says_so_when_there_are_no_worktrees() {
        let fx = fixture();
        let text = capture(|b| ls_text(&fx.paths, b));
        assert!(text.contains("no worktrees"), "{text}");
    }

    #[test]
    fn ls_text_lists_name_branch_head_state_and_path() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| ls_text(&fx.paths, b));

        assert!(text.contains("NAME"), "{text}");
        assert!(text.contains("feat+one"), "{text}");
        assert!(text.contains("feat/one"), "{text}");
        assert!(text.contains("pando"), "state column: {text}");
        assert!(
            text.contains(
                &fx.paths
                    .worktrees_dir()
                    .join("feat+one")
                    .display()
                    .to_string()
            ),
            "{text}"
        );
    }

    #[test]
    fn ls_text_marks_adopted_dirty_and_gone_worktrees() {
        let fx = fixture();
        let adopted = fx.root.parent().unwrap().join("adopted");
        git(
            &fx.root,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "adopted",
                adopted.to_str().unwrap(),
            ],
        );
        let dirty = actions::new(&fx.paths, &fx.config, "feat/dirty", None, &|_| {}).unwrap();
        std::fs::write(
            fx.paths.worktrees_dir().join(&dirty).join("scratch.txt"),
            "wip",
        )
        .unwrap();
        let gone = actions::new(&fx.paths, &fx.config, "feat/gone", None, &|_| {}).unwrap();
        std::fs::remove_dir_all(fx.paths.worktrees_dir().join(&gone)).unwrap();

        let text = capture(|b| ls_text(&fx.paths, b));
        for word in ["adopted", "dirty", "gone"] {
            assert!(text.contains(word), "missing {word:?} in:\n{text}");
        }
    }

    #[test]
    fn ls_json_emits_the_documented_shape() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(v["version"], 1);
        assert_eq!(v["project"]["id"], fx.paths.project.id.as_str());
        assert_eq!(v["project"]["name"], "acme-shop");
        assert_eq!(v["project"]["root"], fx.root.display().to_string().as_str());

        let w = &v["worktrees"][0];
        assert_eq!(w["name"], "feat+one");
        assert_eq!(w["branch"], "feat/one");
        assert_eq!(w["detached"], false);
        assert_eq!(w["dirty"], false);
        assert_eq!(w["ahead"], 0);
        assert_eq!(w["behind"], 0);
        assert_eq!(w["created_by_pando"], true);
        assert_eq!(w["prunable"], false);
        assert_eq!(w["locked"], serde_json::Value::Null);
        assert_eq!(w["pr"], serde_json::Value::Null);
        assert!(w["head"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(w["path"].as_str().is_some_and(|s| s.starts_with('/')));
    }

    #[test]
    fn ls_json_is_an_empty_list_rather_than_an_error_with_no_worktrees() {
        let fx = fixture();
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ls_json_reports_a_locked_worktree_with_its_reason() {
        let fx = fixture();
        let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        git(
            &fx.root,
            &[
                "worktree",
                "lock",
                "--reason",
                "benchmarking",
                fx.paths.worktrees_dir().join(&name).to_str().unwrap(),
            ],
        );
        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["locked"], "benchmarking");
    }

    // The CLI never spawns `gh`; chips come from whatever the TUI last saw.
    #[test]
    fn ls_json_fills_the_pr_field_from_the_cache() {
        let fx = fixture();
        actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
        let mut prs = cache::PrCacheFile::new();
        prs.prs.insert(
            "feat/one".into(),
            crate::worktree::PrInfo {
                number: 42,
                title: "feat: one".into(),
                branch: "feat/one".into(),
                author: "dev".into(),
                draft: false,
                state: PrState::Open,
                url: "https://example.test/pull/42".into(),
            },
        );
        cache::save_prs(&fx.paths.pr_cache_file(), &prs).unwrap();

        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["pr"]["number"], 42);
        assert_eq!(v["worktrees"][0]["pr"]["state"], "open");
        assert_eq!(
            v["worktrees"][0]["pr"]["url"],
            "https://example.test/pull/42"
        );
    }

    #[test]
    fn ellipsize_keeps_short_strings_and_truncates_long_ones() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("abcdefghij", 5), "abcd…");
    }
}
