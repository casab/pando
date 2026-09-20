//! Helpers shared by the unit tests of several modules. Compiled only under
//! `cfg(test)`; integration tests use `tests/common/mod.rs` instead, since
//! they cannot see a test-gated module of the library.

use std::path::Path;
use std::process::{Command, Stdio};

/// Runs git with a fixture identity so committing works on any machine and
/// never picks up (or depends on) the operator's own git config. These flags
/// belong to generated fixture repositories only.
const FIXTURE_IDENTITY: [&str; 10] = [
    "-c",
    "user.name=t",
    "-c",
    "user.email=t@t",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "init.defaultBranch=main",
];

pub fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(FIXTURE_IDENTITY)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

/// A repository with one commit on `main`, for tests that need a real repo
/// but no particular contents.
pub fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["commit", "--quiet", "--allow-empty", "-m", "root"]);
}
