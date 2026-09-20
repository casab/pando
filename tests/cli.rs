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
const EXIT_NEEDS_ANSWER: i32 = 3;

struct Env {
    _dir: TempDir,
    home: std::path::PathBuf,
    root: std::path::PathBuf,
}

fn env() -> Env {
    env_of(Kind::Plain)
}

fn env_of(kind: Kind) -> Env {
    let dir = TempDir::new().unwrap();
    let root = build(kind, dir.path()).root;
    Env {
        home: dir.path().join("pando-home"),
        root,
        _dir: dir,
    }
}

/// Every process a test starts is stopped when its environment goes, even
/// when an assertion panicked first. `Drop` on the struct runs before its
/// fields, so the fixture is still on disk when this runs.
impl Drop for Env {
    fn drop(&mut self) {
        if self.home.exists() {
            let _ = self.pando(&["stop"]);
        }
    }
}

impl Env {
    fn pando(&self, args: &[&str]) -> Output {
        self.pando_in(&self.root, args)
    }

    /// Writes pando's own config for this project — the layer detection
    /// would write, and the only one pando ever writes to.
    fn write_config(&self, toml: &str) {
        let path = self.config_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, toml).unwrap();
    }

    fn config_file(&self) -> std::path::PathBuf {
        self.project_dir().join("pando.toml")
    }

    fn log_file(&self, name: &str, source: &str) -> std::path::PathBuf {
        self.project_dir()
            .join("logs")
            .join(name)
            .join(format!("{source}.log"))
    }

    fn project_dir(&self) -> std::path::PathBuf {
        let project = pando::project::ProjectRef::from_root(&self.root).unwrap();
        self.home.join("projects").join(&project.id)
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

    // pando's own file is a different matter: a command that would act on
    // it fails hard, and says which key.
    std::fs::remove_file(e.root.join("pando.toml")).unwrap();
    e.write_config("[project]\nnope = 1\n");
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("nope"), "{}", stderr(&out));
}

// You need `stop` most when the config is broken, and `ls` to see what is
// there at all. Only the commands that would act on the config refuse.
#[test]
fn a_home_pando_toml_pando_cannot_use_still_lets_you_look_and_stop() {
    for (what, bad) in [
        ("a syntax error", "[dev\ncmd = \"x\"\n"),
        ("a type error", "[dev]\ncmd = 3\n"),
        (
            "a validation error",
            "[project]\nprovision = [\"../escape\"]\n",
        ),
    ] {
        let e = env_of(Kind::NextPnpmCompose);
        e.write_config(bad);

        let main_name = Kind::NextPnpmCompose.dir_name();
        for args in [
            vec!["ls"],
            vec!["status"],
            vec!["stop"],
            vec!["path", main_name],
        ] {
            let out = e.pando(&args);
            assert_eq!(
                code(&out),
                EXIT_OK,
                "{what}: {args:?} should still run: {}",
                stderr(&out)
            );
            assert!(
                stderr(&out).contains("pando.toml"),
                "{what}: {args:?} should say what it ignored: {}",
                stderr(&out)
            );
        }
        // `rm` gets as far as its own refusal rather than the config's.
        let out = e.pando(&["rm", "feat+nope"]);
        assert_eq!(code(&out), EXIT_ERROR);
        assert!(
            stderr(&out).contains("no worktree named"),
            "{what}: {}",
            stderr(&out)
        );

        for args in [
            vec!["start", "feat+one"],
            vec!["restart", "feat+one"],
            vec!["new", "feat/one"],
        ] {
            let out = e.pando(&args);
            assert_eq!(
                code(&out),
                EXIT_ERROR,
                "{what}: {args:?} must refuse: {}",
                stdout(&out)
            );
            assert!(
                stderr(&out).contains("pando.toml"),
                "{what}: {args:?} must say the config is why: {}",
                stderr(&out)
            );
        }
    }
}

// The review's repro: detection used to append `[dev]` beside a configured
// process, writing a file pando's own loader refuses — after which every
// command failed, `stop` included.
#[test]
fn a_configured_process_is_never_given_a_dev_table_beside_it() {
    let e = env_of(Kind::NextPnpmCompose);
    e.write_config("[project]\ninstall = \"true\"\n\n[processes.web]\ncmd = \"sleep 300\"\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let before = std::fs::read_to_string(e.config_file()).unwrap();

    let out = e.pando(&["start", "feat+one", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let after = std::fs::read_to_string(e.config_file()).unwrap();
    assert_eq!(
        after, before,
        "a project that declares its processes is not detected at"
    );
    assert!(!after.contains("[dev]"), "{after}");

    for args in [vec!["ls"], vec!["status"], vec!["stop"]] {
        let out = e.pando(&args);
        assert_eq!(
            code(&out),
            EXIT_OK,
            "{args:?} after the start: {}",
            stderr(&out)
        );
    }
}

// The shape a developer writes when they want pando to fill the command in:
// every other command still works, and only `start` has something to say.
#[test]
fn a_dev_table_with_no_command_is_refused_only_by_start() {
    // A repository with nothing to serve, so detection has no command to
    // fill in and `cmd` really is missing when `start` asks for it.
    let e = env_of(Kind::RustLib);
    // The install step is only here so there is a log to read below.
    e.write_config("[project]\ninstall = \"true\"\n\n[dev]\ncwd = \".\"\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    for args in [
        vec!["ls"],
        vec!["status"],
        vec!["stop"],
        vec!["path", "feat+one"],
        vec!["logs", "feat+one", "--source", "install"],
    ] {
        let out = e.pando(&args);
        assert_eq!(code(&out), EXIT_OK, "{args:?}: {}", stderr(&out));
    }

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("cmd"),
        "it says what is missing: {}",
        stderr(&out)
    );
}

// The other half of the same shape: a `[dev]` a developer left half
// written is an invitation, not an error. Detection fills the command and
// the ports, and keeps every key they did write.
#[test]
fn a_dev_table_with_no_command_is_filled_in_by_detection() {
    let e = env_of(Kind::NextPnpmCompose);
    e.write_config("[dev]\ncwd = \".\"\nenv = { GREETING = \"hello\" }\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(
        text.contains("cmd = \"pnpm dev\""),
        "the command is filled in: {text}"
    );
    assert!(
        text.contains("# detected:"),
        "and says where it came from: {text}"
    );
    assert!(
        text.contains("ports = { PORT = \"web\" }"),
        "and so is the port, which was unset: {text}"
    );
    assert!(
        text.contains("cwd = \".\"") && text.contains("GREETING"),
        "what the developer wrote is untouched: {text}"
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
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

// ---- start, stop, status, logs -------------------------------------------

/// A dev "server": prints a line and stays up. No real server is needed to
/// prove the lifecycle, and `sleep` is available everywhere.
const SLEEPER: &str = "[dev]\ncmd = \"echo started-ok && sleep 30\"\nports = { PORT = \"web\" }\n";

#[test]
fn start_status_logs_and_stop_work_from_the_cli() {
    let e = env();
    e.write_config(SLEEPER);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("started feat+one"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("http://localhost:"),
        "start prints the URL: {}",
        stdout(&out)
    );

    // Status, as a machine sees it.
    let out = e.pando(&["status", "--json"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("status --json parses");
    assert_eq!(v["version"], 1);
    let wt = &v["worktrees"][0];
    assert_eq!(wt["name"], "feat+one");
    let port = wt["ports"]["web"].as_u64().expect("a web port");
    assert!(
        (17_000..=56_998).contains(&port),
        "port {port} out of range"
    );
    let phase = wt["processes"]["dev"]["phase"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(phase == "running" || phase == "starting", "{phase}");

    // The listing carries them too.
    let out = e.pando(&["ls"]);
    assert!(stdout(&out).contains(&port.to_string()), "{}", stdout(&out));

    // Logs.
    let mut found = false;
    for _ in 0..40 {
        let out = e.pando(&["logs", "feat+one", "--tail", "5"]);
        assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
        if stdout(&out).contains("started-ok") {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(found, "the dev process's output never reached its log");

    let out = e.pando(&["logs", "feat+one", "--json"]);
    let line: serde_json::Value = serde_json::from_str(stdout(&out).lines().next().unwrap())
        .expect("logs --json emits one object per line");
    assert!(line["line"].is_string());
    assert!(line["level"].is_string());

    // Starting twice says so rather than starting twice.
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(stdout(&out).contains("already running"), "{}", stdout(&out));

    let out = e.pando(&["stop", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("stopped feat+one"),
        "{}",
        stdout(&out)
    );

    let out = e.pando(&["status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(
        v["worktrees"][0]["processes"]
            .as_object()
            .unwrap()
            .is_empty(),
        "a stopped worktree has no processes"
    );
    assert_eq!(
        v["worktrees"][0]["ports"]["web"].as_u64(),
        Some(port),
        "but it still owns its ports"
    );
    assert_eq!(status_porcelain(&e.root), "");
}

/// Two processes, each with its own role, in the long `[processes.<name>]`
/// form the workspace shape uses.
const PAIR: &str = "[processes.web]\ncmd = \"echo web-ok && sleep 30\"\nports = { PORT = \"web\" }\n\
                    \n[processes.api]\ncmd = \"echo api-ok && sleep 30\"\nports = { API_PORT = \"api\" }\n";

fn status_of(e: &Env) -> serde_json::Value {
    serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).expect("status --json parses")
}

#[test]
fn only_starts_and_stops_one_process_of_a_pair() {
    let e = env();
    e.write_config(PAIR);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one", "--only", "api"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let v = status_of(&e);
    let wt = &v["worktrees"][0];
    assert_eq!(
        wt["processes"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["api"],
        "only the process it named was started"
    );
    let ports = wt["ports"].clone();
    assert!(
        ports["web"].is_number() && ports["api"].is_number(),
        "every role is reserved, whatever was started: {ports}"
    );

    // The text form: one line for the worktree, one for each process.
    let out = e.pando(&["status"]);
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[1].trim_start().starts_with("api"), "{text}");

    let out = e.pando(&["start", "feat+one", "--only", "web"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let v = status_of(&e);
    let wt = &v["worktrees"][0];
    assert_eq!(
        wt["processes"].as_object().unwrap().len(),
        2,
        "and now both are running"
    );
    assert_eq!(wt["ports"], ports, "starting the second never moved a port");

    // Stopping one leaves the other exactly as it was.
    let out = e.pando(&["stop", "feat+one", "--only", "web"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let v = status_of(&e);
    assert_eq!(
        v["worktrees"][0]["processes"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["api"],
        "the api never stopped"
    );

    // And a restart of the one that is left keeps its port.
    let before = status_of(&e)["worktrees"][0]["processes"]["api"]["pid"].clone();
    let out = e.pando(&["restart", "feat+one", "--only", "api"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let v = status_of(&e);
    assert_ne!(v["worktrees"][0]["processes"]["api"]["pid"], before);
    assert_eq!(v["worktrees"][0]["ports"], ports);

    assert_eq!(code(&e.pando(&["stop", "feat+one"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

#[test]
fn an_only_nothing_answers_to_names_the_processes_there_are() {
    let e = env();
    e.write_config(PAIR);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one", "--only", "worker"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("api, web"), "{}", stderr(&out));

    // And `--only` without a worktree to apply it to is a usage error, not
    // a silent stop of everything.
    let out = e.pando(&["stop", "--only", "api"]);
    assert_eq!(code(&out), EXIT_USAGE, "stdout: {}", stdout(&out));
}

#[test]
fn restart_keeps_the_port_and_replaces_the_process() {
    let e = env();
    e.write_config(SLEEPER);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    assert_eq!(code(&e.pando(&["start", "feat+one"])), EXIT_OK);

    let first: serde_json::Value =
        serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).unwrap();
    let port = first["worktrees"][0]["ports"]["web"].as_u64().unwrap();
    let pid = first["worktrees"][0]["processes"]["dev"]["pid"]
        .as_u64()
        .unwrap();

    let out = e.pando(&["restart", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let second: serde_json::Value =
        serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).unwrap();
    assert_eq!(
        second["worktrees"][0]["ports"]["web"].as_u64(),
        Some(port),
        "a restart keeps the URL"
    );
    assert_ne!(
        second["worktrees"][0]["processes"]["dev"]["pid"].as_u64(),
        Some(pid)
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
}

#[test]
fn a_project_with_no_dev_process_fails_with_exit_one_and_a_hint() {
    let e = env();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR);
    assert_eq!(
        stderr(&out).trim(),
        "pando: no processes configured; add [dev] to pando.toml"
    );
}

#[test]
fn stopping_nothing_is_not_an_error() {
    let e = env();
    let out = e.pando(&["stop"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(
        stdout(&out).contains("nothing was running"),
        "{}",
        stdout(&out)
    );

    let out = e.pando(&["stop", "nope"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(stdout(&out).contains("was not running"), "{}", stdout(&out));
}

#[test]
fn logs_for_a_worktree_that_has_never_run_says_where_they_would_be() {
    let e = env();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["logs", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR);
    assert!(
        stderr(&out).contains("no logs for feat+one"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_crashed_process_stays_visible_as_failed() {
    let e = env();
    e.write_config("[dev]\ncmd = \"echo 'Error: Cannot find module next' && exit 1\"\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    assert_eq!(code(&e.pando(&["start", "feat+one"])), EXIT_OK);

    let mut reason = String::new();
    for _ in 0..40 {
        let v: serde_json::Value =
            serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).unwrap();
        let process = &v["worktrees"][0]["processes"]["dev"];
        if process["phase"] == "failed" {
            reason = process["reason"].as_str().unwrap_or_default().to_string();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        reason.starts_with("process exited"),
        "reason was {reason:?}"
    );
    assert!(
        reason.contains("dependencies are missing"),
        "the log's own words become a hint: {reason}"
    );
    // Still there on the next read: a failure is sticky until acted on.
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).unwrap();
    assert_eq!(v["worktrees"][0]["processes"]["dev"]["phase"], "failed");
}

// ---- questions ------------------------------------------------------------

// An agent never hangs: a question it cannot answer is exit code 3 with the
// question printed, not a process blocked on a prompt nobody will read.
#[test]
fn a_question_with_no_terminal_to_ask_on_exits_three() {
    let e = env_of(Kind::NextMessy);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), 3, "stdout: {}", stdout(&out));
    let printed = stderr(&out);
    assert!(
        printed.contains("Which command starts the local development server?"),
        "{printed}"
    );
    assert!(
        printed.contains("pnpm dev"),
        "the options are printed: {printed}"
    );
    assert!(printed.contains("--yes"), "and the way out: {printed}");
    assert!(
        stdout(&out).is_empty(),
        "stdout stays clean: {}",
        stdout(&out)
    );
}

#[test]
fn yes_accepts_the_recommendation_and_never_asks_again() {
    let e = env_of(Kind::NextMessy);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    // Written down, with the reason, in pando's own config.
    let project = pando::project::ProjectRef::from_root(&e.root).unwrap();
    let config = e.home.join("projects").join(&project.id).join("pando.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("cmd = \"pnpm dev\""), "{text}");
    assert!(text.contains("ports = { PORT = \"web\" }"), "{text}");
    assert!(text.contains("# detected:"), "{text}");
    assert!(
        text.contains("install = \"pnpm install --frozen-lockfile\""),
        "new answered the install slot on its own: {text}"
    );

    assert_eq!(code(&e.pando(&["stop", "feat+one"])), EXIT_OK);
    // Without --yes and without a terminal: nothing left to ask, so it runs.
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(
        code(&out),
        EXIT_OK,
        "an answered question is never asked again: {}",
        stderr(&out)
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

// Level zero: a library has no dev server, so there is nothing to ask about
// and the failure is the honest one.
#[test]
fn a_library_is_never_asked_about_a_dev_server() {
    let e = env_of(Kind::RustLib);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR, "stderr: {}", stderr(&out));
    assert_eq!(
        stderr(&out).trim(),
        "pando: no processes configured; add [dev] to pando.toml"
    );
}

// The common case: detection resolves every slot, so the first start needs
// no answers at all.
#[test]
fn a_common_project_starts_with_no_questions() {
    let e = env_of(Kind::NextPnpmCompose);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("pnpm dev"), "{}", stderr(&out));
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

// `logs -f` went permanently silent once the log reached `--tail` lines:
// the follow loop compared against the length of a bounded ring buffer,
// which stops growing, so every later line was skipped as already printed.
#[test]
fn follow_keeps_printing_once_the_log_is_longer_than_the_tail() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    let e = env();
    let log = e.log_file("feat+one", "dev");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "l1\nl2\nl3\nl4\nl5\n").unwrap();

    // Under a guard: an assertion that panics part way through must not
    // leave a follower running for the rest of the day.
    let mut child = Follower(
        Command::new(env!("CARGO_BIN_EXE_pando"))
            .env("PANDO_HOME", &e.home)
            .current_dir(&e.root)
            .args(["logs", "feat+one", "--tail", "3", "-f"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("run pando logs -f"),
    );

    let (tx, rx) = mpsc::channel::<String>();
    let out = child.0.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    // Whatever happens, the follower does not outlive this test.
    let printed = |what: &str| -> String {
        rx.recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("nothing was printed for {what}"))
    };

    for expected in ["l3", "l4", "l5"] {
        assert_eq!(printed("the initial tail"), expected);
    }

    let append = |text: &str| {
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    };
    // Well past the ring's capacity, in batches, the way a dev server logs.
    for i in 6..=20 {
        append(&format!("l{i}\n"));
        std::thread::sleep(Duration::from_millis(20));
    }
    for i in 6..=20 {
        let line = printed("a line after the window filled");
        assert_eq!(line, format!("l{i}"), "a line went missing after l{i}");
    }

    // And across the truncation a restart does: the file shrinks, and the
    // lines written after it are still new.
    std::fs::write(&log, "").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    append("fresh-1\nfresh-2\n");
    assert_eq!(printed("the first line after a truncation"), "fresh-1");
    assert_eq!(printed("the second line after a truncation"), "fresh-2");

    drop(child);
    drop(rx);
    let _ = reader.join();
}

/// A `logs -f` child that is killed when it goes out of scope, however the
/// test ended.
struct Follower(std::process::Child);

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// `--yes` takes the first option of a question nothing decided, so the line
// it writes may not claim the rules detected it.
#[test]
fn yes_records_that_it_took_the_first_option_rather_than_detecting_one() {
    let e = env_of(Kind::NextMessy);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = std::fs::read_to_string(e.config_file()).unwrap();
    let cmd = text
        .lines()
        .find(|l| l.starts_with("cmd = "))
        .unwrap_or_else(|| panic!("no dev command was written: {text}"));
    assert!(
        cmd.contains("--yes took the first of 4 options"),
        "the comment has to say a flag chose it: {cmd}"
    );
    assert!(
        !cmd.contains("# detected:"),
        "the rules did not decide this one: {cmd}"
    );
    // And the same for the port question, which had three.
    let ports = text
        .lines()
        .find(|l| l.starts_with("ports = "))
        .unwrap_or_else(|| panic!("no ports were written: {text}"));
    assert!(
        ports.contains("--yes took the first of 3 options"),
        "{ports}"
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
}

// A process the developer deliberately gave no ports — a worker, a watcher,
// a queue consumer — used to have one injected into its own `[dev]` table,
// and was then reported failed for never binding a port it was never told
// about.
#[test]
fn a_process_with_no_ports_of_its_own_is_running_once_it_is_alive() {
    // A project full of port signals, so there is every temptation.
    let e = env_of(Kind::NextPnpmCompose);
    e.write_config("[dev]\ncmd = \"sleep 300\"\n\n[dev.ready]\ntimeout_s = 2\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(
        !text.contains("ports ="),
        "nothing may give a portless process a port: {text}"
    );

    // Well past its readiness window, which it has no port to satisfy.
    std::thread::sleep(std::time::Duration::from_secs(4));
    let out = e.pando(&["status", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("running"),
        "a process with no ports is running once alive: {}",
        stdout(&out)
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
}

// Phase 2b review, finding 5, second half. `Command::Restart` passed the
// raw config while `Command::Start` resolved it, so `restart` on a project
// whose process question has never been answered refused instead of asking
// — the same input, two different answers, depending on which verb was
// typed.
#[test]
fn restart_asks_the_question_start_would_have_asked() {
    let e = env_of(Kind::MonoWebApi);
    assert_eq!(code(&e.pando(&["new", "feat/one", "--yes"])), EXIT_OK);

    let restart = e.pando(&["restart", "feat+one"]);
    let start = e.pando(&["start", "feat+one"]);
    assert_eq!(
        code(&restart),
        EXIT_NEEDS_ANSWER,
        "stdout: {} stderr: {}",
        stdout(&restart),
        stderr(&restart)
    );
    assert_eq!(
        code(&start),
        EXIT_NEEDS_ANSWER,
        "and this is the answer it should match: {}",
        stderr(&start)
    );
    assert_eq!(
        stderr(&restart),
        stderr(&start),
        "the same question, asked the same way"
    );
    assert_eq!(status_porcelain(&e.root), "");
}
