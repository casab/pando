use super::run::commit_to_test;
use super::*;
use crate::config::Config;
use crate::setup::RanBy;
use crate::testutil::git;
use tempfile::TempDir;

#[test]
fn who_ran_a_check_is_the_tui_when_it_says_so_else_whoever_has_stderr() {
    assert_eq!(ran_by(Some("tui"), false), RanBy::Tui);
    assert_eq!(ran_by(Some("tui"), true), RanBy::Tui);
    assert_eq!(ran_by(None, true), RanBy::Terminal);
    assert_eq!(ran_by(Some("anything"), true), RanBy::Terminal);
    assert_eq!(ran_by(None, false), RanBy::Program);
    assert_eq!(CHECK_RAN_BY_ENV, "PANDO_CHECK_RAN_BY");
}

/// A repository whose default branch `main` is one commit behind the
/// branch it has checked out.
fn repo_ahead_of_main() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    crate::testutil::init_repo(&root);
    git(&root, &["checkout", "--quiet", "-b", "feat/wip"]);
    git(&root, &["commit", "--quiet", "--allow-empty", "-m", "wip"]);
    (dir, root)
}

fn sha(root: &std::path::Path, refname: &str) -> String {
    let out = crate::project::git(root, ["rev-parse", refname]).unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn the_commit_tested_is_the_one_new_would_fork_from_not_head() {
    let (_dir, root) = repo_ahead_of_main();
    let (commit, base) = commit_to_test(&root, &Config::default()).unwrap();
    assert_eq!(base.as_deref(), Some("main"));
    assert_eq!(commit, sha(&root, "main"));
    assert_ne!(commit, sha(&root, "HEAD"));

    // The project's own base wins, as it does for `new`.
    let mut config = Config::default();
    config.project.base = Some("feat/wip".to_string());
    let (commit, base) = commit_to_test(&root, &config).unwrap();
    assert_eq!(base.as_deref(), Some("feat/wip"));
    assert_eq!(commit, sha(&root, "HEAD"));
}

#[test]
fn with_no_default_branch_the_commit_is_head_and_with_no_commit_there_is_none() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch=trunk"]);
    assert_eq!(commit_to_test(&root, &Config::default()), None);

    git(&root, &["commit", "--quiet", "--allow-empty", "-m", "root"]);
    let (commit, base) = commit_to_test(&root, &Config::default()).unwrap();
    assert_eq!(base, None, "HEAD is said as HEAD");
    assert_eq!(commit, sha(&root, "HEAD"));
}
