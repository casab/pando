//! End-to-end behaviour of the `pando` binary: exit codes, and what it
//! prints. Everything runs against a generated fixture repository with an
//! injected `PANDO_HOME`, so no test can reach a real repo or the real home.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::{Kind, build, build_with_origin, git, status_porcelain};
use tempfile::TempDir;

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;

struct Env {
    _dir: TempDir,
    home: std::path::PathBuf,
    root: std::path::PathBuf,
}

fn env() -> Env {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::Plain, dir.path()).root;
    Env {
        home: dir.path().join("pando-home"),
        root,
        _dir: dir,
    }
}

impl Env {
    fn pando(&self, args: &[&str]) -> Output {
        self.pando_in(&self.root, args)
    }

    fn pando_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pando"))
            .env("PANDO_HOME", &self.home)
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run pando")
    }
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("pando exited via a signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn ls_on_a_fresh_fixture_succeeds_with_exit_zero() {
    let e = env();
    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("no worktrees"), "{}", stdout(&out));
}

#[test]
fn the_full_new_path_rm_cycle_works_from_the_cli() {
    let e = env();

    let out = e.pando(&["new", "feat/one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("feat+one"), "{}", stdout(&out));
    let created = stdout(&out)
        .trim()
        .rsplit_once(" at ")
        .map(|(_, path)| path.to_string())
        .expect("new prints where it created the worktree");

    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(stdout(&out).contains("feat+one"));

    let out = e.pando(&["path", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK);
    let printed = stdout(&out).trim().to_string();
    assert_eq!(
        created, printed,
        "new must print the canonical path it recorded, the one `path` gives"
    );
    assert!(Path::new(&printed).is_dir(), "path printed {printed:?}");
    // Canonical on both sides: git prints /private/var where the shell and
    // TempDir say /var.
    let canonical_home = std::fs::canonicalize(&e.home).unwrap();
    assert!(
        Path::new(&printed).starts_with(&canonical_home),
        "{printed} is not under {}",
        canonical_home.display()
    );

    let out = e.pando(&["rm", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(!Path::new(&printed).exists());
    assert_eq!(status_porcelain(&e.root), "", "the fixture must stay clean");
}

#[test]
fn ls_json_parses_and_carries_the_documented_keys() {
    let e = env();
    e.pando(&["new", "feat/one"]);
    let out = e.pando(&["ls", "--json"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    assert_eq!(v["version"], 1);
    assert!(v["project"]["id"].as_str().is_some());
    let w = &v["worktrees"][0];
    for key in [
        "name",
        "path",
        "branch",
        "head",
        "detached",
        "dirty",
        "ahead",
        "behind",
        "created_by_pando",
        "prunable",
        "locked",
        "pr",
    ] {
        assert!(w.get(key).is_some(), "missing key {key:?} in {w}");
    }
}

// Discovery is from porcelain, whose first entry is the main checkout from
// any cwd — so a subdirectory sees exactly what the root sees.
#[test]
fn commands_work_from_a_subdirectory_of_the_repository() {
    let e = env();
    e.pando(&["new", "feat/one"]);
    let nested = e.root.join("apps").join("web");
    std::fs::create_dir_all(&nested).unwrap();

    let from_root = stdout(&e.pando(&["ls", "--json"]));
    let from_sub = stdout(&e.pando_in(&nested, &["ls", "--json"]));
    assert_eq!(from_root, from_sub);
}

#[test]
fn running_outside_a_git_repository_fails_with_exit_one_and_one_line() {
    let e = env();
    let outside = e.root.parent().unwrap().join("not-a-repo");
    std::fs::create_dir_all(&outside).unwrap();

    let out = e.pando_in(&outside, &["ls"]);
    assert_eq!(code(&out), EXIT_ERROR);
    let err = stderr(&out);
    assert_eq!(err.trim().lines().count(), 1, "expected one line: {err}");
    assert!(err.contains("not inside a git repository"), "{err}");
}

#[test]
fn a_bare_repository_fails_the_same_way() {
    let e = env();
    let bare = e.root.parent().unwrap().join("bare.git");
    git(
        e.root.parent().unwrap(),
        &[
            "init",
            "--bare",
            "--quiet",
            "--initial-branch=main",
            bare.to_str().unwrap(),
        ],
    );

    let out = e.pando_in(&bare, &["ls"]);
    assert_eq!(code(&out), EXIT_ERROR);
    assert!(
        stderr(&out).contains("bare repositories are not supported"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_unknown_subcommand_is_a_usage_error() {
    let e = env();
    assert_eq!(code(&e.pando(&["definitely-not-a-command"])), EXIT_USAGE);
    assert_eq!(code(&e.pando(&["rm"])), EXIT_USAGE, "missing argument");
}

#[test]
fn help_and_version_work_outside_a_repository() {
    let e = env();
    let outside = e.root.parent().unwrap().join("nowhere");
    std::fs::create_dir_all(&outside).unwrap();
    assert_eq!(code(&e.pando_in(&outside, &["--help"])), EXIT_OK);
    assert_eq!(code(&e.pando_in(&outside, &["--version"])), EXIT_OK);
}

#[test]
fn removing_a_worktree_pando_did_not_create_needs_yes() {
    let e = env();
    let adopted = e.root.parent().unwrap().join("adopted");
    git(
        &e.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );

    let out = e.pando(&["rm", "adopted"]);
    assert_eq!(code(&out), EXIT_ERROR);
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(adopted.exists());

    let out = e.pando(&["rm", "adopted", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(!adopted.exists());
}

// A fixture with a real origin is what makes the tracking rules testable:
// a forked branch must not end up with the base as its upstream.
#[test]
fn a_new_branch_created_through_the_cli_does_not_track_its_base() {
    let dir = TempDir::new().unwrap();
    let fixture = build_with_origin(Kind::Plain, dir.path());
    let home = dir.path().join("pando-home");
    assert!(fixture.remote.is_some());

    let out = Command::new(env!("CARGO_BIN_EXE_pando"))
        .env("PANDO_HOME", &home)
        .current_dir(&fixture.root)
        .args(["new", "feat/one"])
        .output()
        .expect("run pando");
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let upstream = common::git_raw(
        &fixture.root,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "feat/one@{upstream}",
        ],
    );
    assert!(
        !upstream.status.success(),
        "a forked branch must have no upstream, got {:?}",
        String::from_utf8_lossy(&upstream.stdout)
    );
    assert_eq!(status_porcelain(&fixture.root), "");
}

#[test]
fn every_fixture_kind_builds_a_clean_repository() {
    let dir = TempDir::new().unwrap();
    for kind in Kind::ALL {
        let fixture = build(kind, &dir.path().join(kind.dir_name()));
        assert!(
            fixture.remote.is_none(),
            "{kind:?} was built without an origin"
        );
        assert!(
            fixture.root.join(".gitignore").is_file(),
            "{:?} has no .gitignore",
            kind
        );
        assert_eq!(
            status_porcelain(&fixture.root),
            "",
            "{:?} must be clean after building — its ignored files are ignored",
            kind
        );
    }
}

// `PANDO_HOME` decides where state, caches, logs, and every worktree pando
// creates live. A home inside the repository puts all of it in the working
// tree, which Invariant 1 forbids — so it is refused before anything is
// written, in the absolute and the relative form alike.
#[test]
fn a_pando_home_inside_the_repository_is_refused() {
    let e = env();
    let absolute = e.root.join(".pando-home");
    for home in [absolute.to_str().unwrap(), ".pando"] {
        for args in [["new", "feat/inside"], ["ls", "--json"]] {
            let out = Command::new(env!("CARGO_BIN_EXE_pando"))
                .env("PANDO_HOME", home)
                .current_dir(&e.root)
                .args(args)
                .output()
                .expect("run pando");
            assert_eq!(
                code(&out),
                EXIT_ERROR,
                "PANDO_HOME={home} {args:?} should be refused; stdout: {}",
                stdout(&out)
            );
            assert!(
                stderr(&out).contains("inside the repository"),
                "PANDO_HOME={home}: {}",
                stderr(&out)
            );
        }
    }
    assert!(!absolute.exists(), "the refused home must not be created");
    assert!(!e.root.join(".pando").exists());
    assert_eq!(status_porcelain(&e.root), "", "the fixture must stay clean");
}

// `adopted` in the listing and `rm`'s confirmation rule read the same state
// file. When it cannot be read, the listing has to say so rather than call
// every worktree adopted with exit 0 while `rm` fails hard on the same file.
#[test]
fn ls_warns_when_the_state_file_cannot_be_used() {
    let e = env();
    e.pando(&["new", "feat/one"]);
    let listed: serde_json::Value =
        serde_json::from_str(&stdout(&e.pando(&["ls", "--json"]))).expect("valid json");
    let id = listed["project"]["id"].as_str().expect("a project id");
    let state = e.home.join("projects").join(id).join("state.json");
    std::fs::write(&state, r#"{"version":3,"worktrees":{}}"#).unwrap();

    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("adopted"), "{}", stdout(&out));
    assert!(stderr(&out).contains("version 3"), "{}", stderr(&out));

    let out = e.pando(&["ls", "--json"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(stderr(&out).contains("version 3"), "{}", stderr(&out));
    serde_json::from_str::<serde_json::Value>(&stdout(&out))
        .expect("the warning must go to stderr, leaving stdout parseable");

    // The same one line `rm` refuses with, so the two never disagree.
    let out = e.pando(&["rm", "feat+one", "--yes"]);
    assert_eq!(code(&out), EXIT_ERROR);
    assert!(stderr(&out).contains("version 3"), "{}", stderr(&out));
}

// The committed file is the one a teammate can change under you, so the
// worst it may do is drop out with a warning. Every read-only command has
// to keep working.
#[test]
fn a_committed_pando_toml_pando_cannot_use_does_not_stop_ls() {
    let e = env();
    for bad in [
        "[dev]\ncmd = \"x\"\n\n[processes.api]\ncmd = \"y\"\n",
        "[project]\nprovision = [\"../shared/.env\"]\n",
        "[project]\nbase = \"main\"\nnope = 1\n",
        "[project\nbase =\n",
    ] {
        std::fs::write(e.root.join("pando.toml"), bad).unwrap();
        let out = e.pando(&["ls"]);
        assert_eq!(code(&out), EXIT_OK, "{bad:?} stderr: {}", stderr(&out));
        assert!(
            stderr(&out).contains("ignoring"),
            "{bad:?} should warn: {}",
            stderr(&out)
        );
        assert!(stdout(&out).contains("no worktrees"), "{}", stdout(&out));
    }

    // pando's own file is a different matter: it fails hard, and says which.
    std::fs::remove_file(e.root.join("pando.toml")).unwrap();
    let home_config = e.home.join("projects");
    std::fs::create_dir_all(&home_config).unwrap();
    let out = e.pando(&["ls", "--json"]);
    let listed: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("valid json");
    let id = listed["project"]["id"].as_str().unwrap();
    std::fs::create_dir_all(home_config.join(id)).unwrap();
    std::fs::write(
        home_config.join(id).join("pando.toml"),
        "[project]\nnope = 1\n",
    )
    .unwrap();
    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("nope"), "{}", stderr(&out));
}

// A command run from a directory that no longer exists fails before git is
// ever consulted, and a bare errno names nothing at all.
#[test]
fn running_from_a_deleted_directory_says_which_directory() {
    let e = env();
    let gone = e.root.parent().unwrap().join("gone");
    std::fs::create_dir_all(&gone).unwrap();
    // Only a child can delete its own cwd out from under itself, so the
    // shell sets that up and then becomes pando.
    let script = format!(
        "cd '{dir}' && rmdir '{dir}' && exec '{bin}' ls",
        dir = gone.display(),
        bin = env!("CARGO_BIN_EXE_pando"),
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("PANDO_HOME", &e.home)
        .output()
        .expect("run sh");

    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(err.contains("current directory"), "{err}");
    assert_eq!(err.trim().lines().count(), 1, "expected one line: {err}");
}
