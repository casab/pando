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
    let env = Env {
        home: dir.path().join("pando-home"),
        root,
        _dir: dir,
    };
    // The one question that is about this machine rather than the fixture.
    // Several fixtures pin a runtime — `.nvmrc` 22, `.python-version` 3.12
    // — and the commands these tests actually run are `sleep` and a python
    // listener, so nothing has to be initialised in front of them. The
    // answer goes where that kind of answer lives: the user layer.
    env.write_user_config("[runtime]\nprelude = \"\"\n");
    env
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

    /// The machine-wide layer, under every project's own config.
    fn write_user_config(&self, toml: &str) {
        std::fs::create_dir_all(&self.home).unwrap();
        std::fs::write(self.home.join("config.toml"), toml).unwrap();
    }

    /// Takes the machine answer away again, for the tests that are about
    /// pando noticing the machine does not resolve what the project pins.
    fn unanswer_the_runtime(&self) {
        std::fs::remove_file(self.home.join("config.toml")).unwrap();
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

    /// Runs pando with something on its stdin, for `--answers -`.
    fn pando_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = Command::new(env!("CARGO_BIN_EXE_pando"))
            .env("PANDO_HOME", &self.home)
            .current_dir(&self.root)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run pando");
        child
            .stdin
            .take()
            .expect("a piped stdin")
            .write_all(input.as_bytes())
            .expect("write the answers");
        child.wait_with_output().expect("wait for pando")
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

/// A fixture with compose services, a fake docker in its home, and a dev
/// process that needs no framework — everything `--isolated` needs.
fn env_isolated() -> Env {
    let e = env_of(Kind::NextPnpmCompose);
    common::docker::install(&e.home);
    e.write_config(&format!(
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
         [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\", \"redis\"]\n\
         env = {{ DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }}\n",
        common::listener_on_port_env()
    ));
    e
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
// A config that configures a service, and a build with no way to run it.
// The block is legal and is kept; saying nothing about it left the
// developer reading "no services configured" over a file that configures
// one.
#[test]
fn a_service_kind_this_build_cannot_run_is_warned_about_on_every_command() {
    let e = env();
    e.write_config(
        "[project]\nprovision = [\".env\"]\n\n\
         [[services]]\nkind = \"native\"\nname = \"postgres\"\npreset = \"postgres\"\n",
    );
    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_OK, "a legal block is not an error");
    let said = stderr(&out);
    assert!(said.contains("postgres"), "it names the block: {said}");
    assert!(said.contains("native"), "{said}");

    // And on stderr, so `--json` is still parseable by whatever reads it.
    let out = e.pando(&["ls", "--json"]);
    assert_eq!(code(&out), EXIT_OK);
    assert!(stderr(&out).contains("postgres"), "{}", stderr(&out));
    serde_json::from_str::<serde_json::Value>(&stdout(&out))
        .expect("the warning must go to stderr, leaving stdout parseable");
}

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
    // Detection may still fill slots this file says nothing about — the
    // services and the schema step — but never a process, and never by
    // rewriting what was already there.
    assert!(
        after.starts_with(&before),
        "a project that declares its processes is not detected at\n\
         before:\n{before}\nafter:\n{after}"
    );
    assert!(!after.contains("[dev]"), "{after}");
    assert!(after.contains("[processes.web]"), "{after}");

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

/// Polls until `check` passes, or gives up.
///
/// A spawned process writes when it gets round to it, which is not when
/// `start` returns — and a busy CI runner gets round to it a lot later
/// than an idle laptop does. The budget is therefore generous rather than
/// tight, and costs nothing when the machine is quick: the loop ends on
/// the first pass either way.
fn poll_until(mut check: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if check() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Polls a log file until it holds `needle`.
fn log_contains(path: &Path, needle: &str) -> bool {
    poll_until(|| {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .contains(needle)
    })
}

// Committed < user < project, end to end. The machine-wide file answers
// what nothing else has, and pando's own file for this project beats it.
#[test]
fn the_user_layer_is_read_and_the_project_layer_beats_it() {
    let e = env();
    std::fs::create_dir_all(&e.home).unwrap();
    let user = e.home.join("config.toml");
    std::fs::write(
        &user,
        "[dev]\ncmd = \"echo from-the-user-layer && sleep 30\"\nports = []\n",
    )
    .unwrap();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        log_contains(&e.log_file("feat+one", "dev"), "from-the-user-layer"),
        "the user layer's dev command is the one that ran: {}",
        std::fs::read_to_string(e.log_file("feat+one", "dev")).unwrap_or_default()
    );

    // And the project layer, which is the one pando writes, wins over it.
    e.write_config("[dev]\ncmd = \"echo from-the-project-layer && sleep 30\"\nports = []\n");
    let out = e.pando(&["restart", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        log_contains(&e.log_file("feat+one", "dev"), "from-the-project-layer"),
        "{}",
        std::fs::read_to_string(e.log_file("feat+one", "dev")).unwrap_or_default()
    );

    // A machine-wide file pando cannot use is dropped with a warning, not
    // a failure: it applies to every project on the laptop.
    std::fs::write(&user, "[project\nbase =\n").unwrap();
    let out = e.pando(&["ls"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("ignoring") && stderr(&out).contains("config.toml"),
        "{}",
        stderr(&out)
    );
}

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
    let found = poll_until(|| {
        let out = e.pando(&["logs", "feat+one", "--tail", "5"]);
        assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
        stdout(&out).contains("started-ok")
    });
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

// A dev server that prints a binary blob leaves invalid UTF-8 in its log;
// `from_utf8_lossy` turns it into U+FFFD, and a short date-ish token a few
// bytes later used to put a timestamp match inside that character. Slicing
// there panicked the process — `pando logs` exited 101, and the TUI died
// with it. Reading a log must never be able to crash pando.
#[test]
fn logs_reads_a_file_with_invalid_utf8_before_a_short_timestamp() {
    let e = env();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let log = e.log_file("feat+one", "dev");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(
        &log,
        b"starting up fine\n\xff 21-09-26T10:00:00 hello\nafter\n",
    )
    .unwrap();

    let out = e.pando(&["logs", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("21-09-26T10:00:00"),
        "the line is printed, not swallowed: {}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("after"), "{}", stdout(&out));
}

#[test]
fn a_crashed_process_stays_visible_as_failed() {
    let e = env();
    e.write_config("[dev]\ncmd = \"echo 'Error: Cannot find module next' && exit 1\"\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    assert_eq!(code(&e.pando(&["start", "feat+one"])), EXIT_OK);

    let mut reason = String::new();
    poll_until(|| {
        let v: serde_json::Value =
            serde_json::from_str(&stdout(&e.pando(&["status", "--json"]))).unwrap();
        let process = &v["worktrees"][0]["processes"]["dev"];
        if process["phase"] == "failed" {
            reason = process["reason"].as_str().unwrap_or_default().to_string();
            return true;
        }
        false
    });
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

// Phase 2b review, finding 7. Stopping one process reconciled the whole
// state file, and a sibling's `Failed` record — the one phase whose entire
// purpose is to outlive its process — went with it, so `status` stopped
// mentioning that anything had crashed.
#[test]
fn stopping_one_process_leaves_the_others_crash_visible() {
    let e = env();
    e.write_config(
        "[project]\ninstall = \"true\"\n\n\
         [processes.web]\ncmd = \"sleep 300\"\nports = []\n\n\
         [processes.api]\ncmd = \"sleep 1\"\nports = []\n",
    );
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    // The api exits on its own, and a read is what notices — but *when* it
    // exits is a fact about how quickly the machine got round to spawning
    // it, not about pando. A fixed sleep asserted that fact and failed on a
    // loaded runner; this waits for the state the test is really about.
    assert!(
        poll_until(|| stdout(&e.pando(&["status", "feat+one"])).contains("failed")),
        "the api should have failed by now: {}",
        stdout(&e.pando(&["status", "feat+one"]))
    );

    let out = e.pando(&["stop", "feat+one", "--only", "web"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = e.pando(&["status", "feat+one"]);
    assert!(
        stdout(&text).contains("api") && stdout(&text).contains("failed"),
        "stopping web must not erase the api's crash: {}",
        stdout(&text)
    );
    let json = e.pando(&["status", "feat+one", "--json"]);
    assert!(
        stdout(&json).contains("\"api\""),
        "and --json says so too: {}",
        stdout(&json)
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

// ---- isolation from the command line --------------------------------------

#[test]
fn start_isolated_brings_up_services_and_status_shows_them() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let e = env_isolated();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one", "--isolated"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = e.pando(&["status", "feat+one"]);
    let shown = stdout(&text);
    assert!(shown.contains("postgres"), "{shown}");
    assert!(shown.contains("redis"), "{shown}");
    assert!(shown.contains("service on"), "with its port: {shown}");
    assert!(shown.contains("up"), "and whether it is answering: {shown}");

    let json = stdout(&e.pando(&["status", "feat+one", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let worktree = &value["worktrees"][0];
    assert_eq!(worktree["isolated"], serde_json::json!(true), "{json}");
    let postgres = &worktree["services"]["postgres"];
    assert_eq!(postgres["kind"], serde_json::json!("compose"));
    assert_eq!(postgres["up"], serde_json::json!(true), "{json}");
    assert_eq!(
        postgres["port"],
        serde_json::json!(worktree["ports"]["postgres"].as_u64().unwrap())
    );
    assert!(
        postgres["project"]
            .as_str()
            .unwrap()
            .starts_with("pando-next-pnpm-compose-"),
        "{json}"
    );

    // The service's container log is a source like any other.
    let logs = e.pando(&["logs", "feat+one", "--source", "postgres"]);
    assert_eq!(code(&logs), EXIT_OK, "stderr: {}", stderr(&logs));
    assert!(
        stdout(&logs).contains("fake docker log for postgres"),
        "{}",
        stdout(&logs)
    );

    assert_eq!(code(&e.pando(&["rm", "feat+one", "--force"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

#[test]
fn status_env_prints_lines_a_shell_can_eval() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let e = env_isolated();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    assert_eq!(
        code(&e.pando(&["start", "feat+one", "--isolated"])),
        EXIT_OK
    );
    let json = stdout(&e.pando(&["status", "feat+one", "--json"]));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let ports = &value["worktrees"][0]["ports"];
    let postgres = ports["postgres"].as_u64().unwrap();
    let web = ports["web"].as_u64().unwrap();

    let out = e.pando(&["status", "feat+one", "--env"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains(&format!(
            "export DATABASE_URL='postgres://acme:acme@localhost:{postgres}/acme'"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("export PORT='{web}'")), "{text}");
    assert!(text.contains("export PANDO_NAME='feat+one'"), "{text}");
    // Every line is an export, so `eval` on the whole thing is safe.
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        assert!(line.starts_with("export "), "{line:?}");
    }

    // And a shell really does read it back.
    let shell = Command::new("bash")
        .arg("-c")
        .arg("eval \"$(cat)\" && echo \"$DATABASE_URL|$PANDO_NAME\"")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(text.as_bytes())?;
            child.wait_with_output()
        })
        .expect("run bash");
    assert_eq!(
        stdout(&shell).trim(),
        format!("postgres://acme:acme@localhost:{postgres}/acme|feat+one")
    );

    assert_eq!(code(&e.pando(&["rm", "feat+one", "--force"])), EXIT_OK);
}

#[test]
fn status_env_on_a_worktree_that_has_never_started_says_so() {
    let e = env_isolated();
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["status", "feat+one", "--env"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("start it once"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "nothing to eval: {}", stdout(&out));
}

#[test]
fn status_env_without_a_name_is_a_usage_error() {
    let e = env();
    assert_eq!(code(&e.pando(&["status", "--env"])), EXIT_USAGE);
    assert_eq!(
        code(&e.pando(&["status", "feat+one", "--env", "--json"])),
        EXIT_USAGE,
        "--env and --json are two different shapes"
    );
}

// Every guess is visible, and so is every non-guess: a flag taking the
// services must not be written down as if a human had chosen them.
#[test]
fn yes_says_in_the_file_that_a_flag_took_the_services() {
    let e = env_of(Kind::NextMessy);
    common::docker::install(&e.home);
    // The process slots are already answered, so `--isolated --yes` has
    // exactly one question left to take.
    e.write_config("[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one", "--isolated", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let text = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(text.contains("[[services]]"), "{text}");
    assert!(
        text.contains("include = [\"db\", \"mail\"]"),
        "--yes takes the ones a rule resolved and leaves cache and queue: {text}"
    );
    assert!(
        text.contains("--yes took the 2 of 4 the rules resolved"),
        "the file has to say a flag decided this: {text}"
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
}

// The plan's own words: an isolated start on a project with no services
// runs shared and says so, rather than refusing.
#[test]
fn start_isolated_on_a_project_with_no_services_runs_shared() {
    let e = env_of(Kind::GoService);
    common::docker::install(&e.home);
    e.write_config("[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n");
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    let out = e.pando(&["start", "feat+one", "--isolated"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("shared mode"), "{}", stderr(&out));
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
}

// ---- the runtime the project asks for -------------------------------------

/// The fixture, plus a committed pin no machine resolves, and the machine
/// answer taken away again.
fn env_pinning_an_impossible_runtime() -> Env {
    let e = env();
    e.unanswer_the_runtime();
    std::fs::write(e.root.join(".nvmrc"), "99\n").unwrap();
    git(&e.root, &["add", ".nvmrc"]);
    git(&e.root, &["commit", "--quiet", "-m", "pin node 99"]);
    e.write_config(SLEEPER);
    assert_eq!(code(&e.pando(&["new", "feat/one"])), EXIT_OK);
    e
}

// A dev server started under a runtime the project rejects dies of it. The
// whole point of asking first is that nothing is started.
#[test]
fn a_runtime_this_machine_does_not_resolve_is_a_question_not_a_dead_process() {
    let e = env_pinning_an_impossible_runtime();

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_NEEDS_ANSWER, "stdout: {}", stdout(&out));
    let printed = stderr(&out);
    assert!(printed.contains("node 99 (.nvmrc)"), "{printed}");
    assert!(
        printed.contains("bash -lc"),
        "the shell pando actually uses is named: {printed}"
    );
    assert!(
        printed.contains("~/.pando/config.toml"),
        "and the file the answer belongs in, which is not the project's own: {printed}"
    );
    assert!(
        !e.log_file("feat+one", "dev").exists(),
        "nothing may be spawned before the runtime is settled"
    );
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert_eq!(status_porcelain(&e.root), "");
}

// The case that is invisible without a probe: a prelude is set, and it is
// not doing anything.
#[test]
fn a_prelude_that_does_not_work_stops_the_start_and_names_the_file() {
    let e = env_pinning_an_impossible_runtime();
    e.write_user_config("[runtime]\nprelude = \"true\"\n");

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_ERROR, "stdout: {}", stdout(&out));
    let printed = stderr(&out);
    assert!(printed.contains("is not working"), "{printed}");
    assert!(printed.contains("prelude: true"), "{printed}");
    assert!(printed.contains("config.toml"), "{printed}");
    assert!(
        !e.log_file("feat+one", "dev").exists(),
        "nothing may be spawned"
    );
}

// And the answer that makes it go away: a line this machine really can
// run, written to the machine-wide layer and never asked about again.
#[test]
fn a_prelude_that_works_is_written_to_the_user_layer_and_the_start_proceeds() {
    let e = env_pinning_an_impossible_runtime();
    let bin = e.home.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("node"), "#!/bin/sh\necho v99.0.0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("node"), std::fs::Permissions::from_mode(0o755)).unwrap();
    e.write_user_config(&format!(
        "[runtime]\nprelude = 'export PATH=\"{}:$PATH\"'\n",
        bin.display()
    ));

    let out = e.pando(&["start", "feat+one"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("started feat+one"),
        "{}",
        stdout(&out)
    );
    assert_eq!(code(&e.pando(&["stop"])), EXIT_OK);
    assert_eq!(status_porcelain(&e.root), "");
}

// ---- init -----------------------------------------------------------------

// The batch form of every question, in one pass, through the same paths
// `new` and `start` use. It prints where the answers went.
#[test]
fn init_answers_every_slot_and_says_which_file_it_wrote() {
    let e = env_of(Kind::NextPnpmCompose);

    let out = e.pando(&["init", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let printed = stdout(&out);
    assert!(
        printed.contains(&format!("wrote {}", e.config_file().display())),
        "the file to go and read is the first thing it says: {printed}"
    );
    assert!(
        printed.contains("pnpm install --frozen-lockfile"),
        "{printed}"
    );
    assert!(printed.contains("pnpm dev"), "{printed}");
    assert!(printed.contains("postgres, redis"), "{printed}");

    let written = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(written.contains(r#"cmd = "pnpm dev""#), "{written}");
    assert!(written.contains("[[services]]"), "{written}");
    // Nothing was started, and nothing was written into the repository.
    assert_eq!(
        stdout(&e.pando(&["ls"])),
        "no worktrees — `pando new <branch>` creates one\n"
    );
    assert_eq!(status_porcelain(&e.root), "");
}

#[test]
fn a_second_init_has_nothing_left_to_answer() {
    let e = env_of(Kind::NextPnpmCompose);
    assert_eq!(code(&e.pando(&["init", "--yes"])), EXIT_OK);
    let first = std::fs::read_to_string(e.config_file()).unwrap();

    // No `--yes` this time: a run that asks nothing needs no flag, which
    // is the whole point of having written the answers down.
    let out = e.pando(&["init"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("nothing left to answer"),
        "{}",
        stdout(&out)
    );
    assert_eq!(std::fs::read_to_string(e.config_file()).unwrap(), first);
}

// Agents never hang: with nothing to read an answer from, the first
// undecided slot is exit 3 and the question is on stderr.
#[test]
fn init_with_nobody_to_ask_exits_three_with_the_question() {
    let e = env_of(Kind::NextMessy);

    let out = e.pando(&["init"]);
    assert_eq!(code(&out), EXIT_NEEDS_ANSWER, "stdout: {}", stdout(&out));
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
    // What the rules settled on the way is written down — the pass got
    // that far — and the slot it stopped on is not: a question is not an
    // answer.
    let written = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(written.contains("install = "), "{written}");
    assert!(!written.contains("[dev]"), "{written}");
    assert_eq!(status_porcelain(&e.root), "");
}

// `--yes` answers the same questions and says in the file that a flag did.
#[test]
fn init_with_yes_records_that_a_flag_chose() {
    let e = env_of(Kind::NextMessy);
    let out = e.pando(&["init", "--yes"]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let written = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(
        written.contains("# answered: --yes took the first of"),
        "a config that claims a rule decided what a flag decided is one nobody can review: \
         {written}"
    );
}

// ---- an answers file ------------------------------------------------------

/// The answers a program would send for the deliberately ambiguous
/// fixture: the two questions its rules cannot settle, plus the services.
const ANSWERS: &str = r#"{
  "dev_cmd": "pnpm dev:web",
  "port_env": "WEB_PORT",
  "services": ["db", "cache"]
}"#;

#[test]
fn an_answers_file_fills_the_config_and_says_a_program_answered() {
    let e = env_of(Kind::NextMessy);
    let path = e.home.join("answers.json");
    std::fs::write(&path, ANSWERS).unwrap();

    let out = e.pando(&["init", "--answers", path.to_str().unwrap()]);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let written = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(
        written.contains(r#"cmd = "pnpm dev:web""#),
        "the option was named by its own text: {written}"
    );
    assert!(
        written.contains(r#"ports = { WEB_PORT = "web" }"#),
        "{written}"
    );
    assert!(
        written.contains(r#"include = ["db", "cache"]"#),
        "{written}"
    );
    assert!(
        written.matches("# answered: a program,").count() >= 3,
        "every key a program answered says so, and none of the detected ones do: {written}"
    );
    assert!(
        written.contains(r#"install = "pnpm install --frozen-lockfile"  # detected:"#),
        "a rule's own answer still says a rule decided it: {written}"
    );
    assert_eq!(status_porcelain(&e.root), "");
}

#[test]
fn an_answers_file_can_come_from_stdin() {
    let e = env_of(Kind::NextMessy);
    let out = e.pando_stdin(&["init", "--answers", "-"], ANSWERS);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    assert!(
        std::fs::read_to_string(e.config_file())
            .unwrap()
            .contains(r#"cmd = "pnpm dev:web""#)
    );
}

// Never a silent skip: a program that named a question pando does not ask
// has to hear about it, with the names it could have used.
#[test]
fn a_question_pando_does_not_ask_is_a_usage_error() {
    let e = env_of(Kind::NextMessy);
    let out = e.pando_stdin(
        &["init", "--answers", "-"],
        r#"{"dev_command": "pnpm dev"}"#,
    );
    assert_eq!(code(&out), EXIT_USAGE, "stdout: {}", stdout(&out));
    let printed = stderr(&out);
    assert!(printed.contains("dev_command"), "{printed}");
    assert!(printed.contains("dev_cmd"), "{printed}");
    assert!(
        !e.config_file().exists(),
        "the file is checked before a single key is written"
    );
}

#[test]
fn an_answer_that_is_not_on_offer_names_what_is() {
    let e = env_of(Kind::NextMessy);
    let out = e.pando_stdin(
        &["init", "--answers", "-"],
        r#"{"services": ["postgres"], "dev_cmd": "pnpm dev", "port_env": "PORT"}"#,
    );
    assert_eq!(code(&out), EXIT_USAGE, "stdout: {}", stdout(&out));
    let printed = stderr(&out);
    assert!(printed.contains("postgres"), "{printed}");
    assert!(
        printed.contains("cache, db, mail, queue"),
        "a service the compose file does not declare is not one pando can run: {printed}"
    );
}

// Re-running is safe: the answers for slots that already have one are
// reported and left alone, comment and all.
#[test]
fn answers_for_questions_already_answered_are_reported_and_not_reapplied() {
    let e = env_of(Kind::NextMessy);
    assert_eq!(
        code(&e.pando_stdin(&["init", "--answers", "-"], ANSWERS)),
        EXIT_OK
    );
    let first = std::fs::read_to_string(e.config_file()).unwrap();

    let out = e.pando_stdin(&["init", "--answers", "-"], ANSWERS);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));
    let printed = stderr(&out);
    for name in ["dev_cmd", "port_env", "services"] {
        assert!(
            printed.contains(&format!("{name} is already answered")),
            "{printed}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(e.config_file()).unwrap(),
        first,
        "a second run with the same answers changes nothing"
    );
}

// The preview is the real renderer against a copy, so what it prints is
// what would land — and nothing is kept.
#[test]
fn dry_run_prints_the_config_it_would_write_and_writes_nothing() {
    let e = env_of(Kind::NextMessy);
    let out = e.pando_stdin(&["init", "--answers", "-", "--dry-run"], ANSWERS);
    assert_eq!(code(&out), EXIT_OK, "stderr: {}", stderr(&out));

    let printed = stdout(&out);
    assert!(
        printed.contains(&format!("# {}", e.config_file().display())),
        "the file it would write is named: {printed}"
    );
    assert!(printed.contains(r#"cmd = "pnpm dev:web""#), "{printed}");
    assert!(printed.contains("# answered: a program,"), "{printed}");
    assert!(
        stderr(&out).contains("would write"),
        "the summary says it did not: {}",
        stderr(&out)
    );
    assert!(
        !e.config_file().exists(),
        "a preview that writes the file is not a preview"
    );
    assert!(
        !e.home.join("preview").exists(),
        "and the scratch copy it made is gone"
    );
    assert_eq!(status_porcelain(&e.root), "");

    // And the real run then writes exactly what the preview showed.
    assert_eq!(
        code(&e.pando_stdin(&["init", "--answers", "-"], ANSWERS)),
        EXIT_OK
    );
    let written = std::fs::read_to_string(e.config_file()).unwrap();
    assert!(
        printed.contains(&written),
        "the preview was the real thing: {printed}"
    );
}
