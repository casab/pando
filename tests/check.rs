//! `pando check`, end to end: the binary against generated fixture
//! repositories with an injected `PANDO_HOME`, and the action itself where
//! a test has to shorten a wait. Every check here runs a python server or
//! a `sleep`, never a real toolchain.

use crate::common;

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use common::{Kind, build, git_raw, python3_available, wait_until};
use pando::paths::{CHECK_WORKTREE, PandoPaths};
use pando::setup::{CheckOutcome, CheckRecord, FailureKind, SetupState};
use tempfile::TempDir;

/// A dev server that answers every request with `status`.
///
/// Brace-free: `{` is pando's template syntax.
fn answering(status: u16) -> String {
    format!(
        "python3 -u -c \"import http.server as h,os;C=type('C',(h.BaseHTTPRequestHandler,),\
         dict(do_GET=lambda s:(s.send_response({status}),s.end_headers())));\
         h.HTTPServer(('127.0.0.1',int(os.environ['PORT'])),C).serve_forever()\""
    )
}

/// pando's config for a project whose one process runs `cmd` on the `web`
/// role, with an install step that does nothing, and `extra` after it.
fn config_running(cmd: &str, extra: &str) -> String {
    format!(
        "[project]\ninstall = \"true\"\n\n[dev]\ncmd = '''{cmd}'''\nports = {{ PORT = \"web\" }}\n\n\
         {extra}"
    )
}

struct Env {
    _dir: TempDir,
    home: PathBuf,
    root: PathBuf,
    paths: PandoPaths,
}

fn env(config: &str) -> Env {
    env_of(Kind::Plain, Some(config))
}

fn env_of(kind: Kind, config: Option<&str>) -> Env {
    let dir = TempDir::new().unwrap();
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    let root = build(kind, &parent).root;
    let home = parent.join("pando-home");
    let paths = common::paths_for(&home, &root);
    paths.ensure_home().unwrap();
    // Nothing here needs a runtime initialised in front of it.
    std::fs::write(home.join("config.toml"), "[runtime]\nprelude = \"\"\n").unwrap();
    if let Some(config) = config {
        std::fs::write(paths.config_file(), config).unwrap();
    }
    Env {
        _dir: dir,
        home,
        root: paths.root().to_path_buf(),
        paths,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.root.exists() {
            let _ = self.pando(&["stop", "--all"]);
        }
    }
}

impl Env {
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pando"));
        command
            .env("PANDO_HOME", &self.home)
            .env_remove(pando::actions::CHECK_RAN_BY_ENV)
            .current_dir(&self.root)
            .args(args);
        command
    }

    fn pando(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run pando")
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pando")
    }

    fn setup_state(&self) -> SetupState {
        let config = pando::config::load(&self.paths).unwrap().config;
        pando::setup::read(&self.paths, &config).state
    }

    fn record(&self) -> Option<CheckRecord> {
        CheckRecord::load(&self.paths)
    }

    fn write_config(&self, config: &str) {
        std::fs::write(self.paths.config_file(), config).unwrap();
    }

    fn check_dir(&self) -> PathBuf {
        let config = pando::config::load(&self.paths).unwrap().config;
        config.check_worktree_path(&self.paths)
    }

    /// Waits until the running check's record has a step starting with
    /// `step`.
    fn wait_for_step(&self, step: &str) {
        assert!(
            wait_until(Duration::from_secs(30), || {
                self.record()
                    .is_some_and(|r| r.progress.iter().any(|line| line.starts_with(step)))
            }),
            "the check never got to {step:?}: {:?}",
            self.record()
        );
    }

    fn git(&self, args: &[&str]) -> String {
        let out = git_raw(&self.root, args);
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// No worktree, no branch, no record and no directory of the check.
    fn assert_nothing_left(&self, branches_before: &str) {
        let listed = self.git(&["worktree", "list", "--porcelain"]);
        assert_eq!(
            listed.matches("worktree ").count(),
            1,
            "git lists only the main checkout: {listed}"
        );
        assert_eq!(
            self.git(&["branch", "--list"]),
            branches_before,
            "no branch was made"
        );
        let state = pando::state::load(&self.paths.state_file()).unwrap();
        assert!(
            !state.worktrees.contains_key(CHECK_WORKTREE),
            "the check's state record is gone"
        );
        assert!(!self.check_dir().exists(), "its directory is gone");
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("stdout is one JSON object ({e}): {}", stderr(out)))
}

fn skip_without_python() -> bool {
    if !python3_available() {
        eprintln!("skipped: python3 is not available");
        return true;
    }
    false
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn signal(child: &Child, signal: nix::sys::signal::Signal) {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), signal).unwrap();
}

// A pass: the page answers 404, which is an app serving pages; the hooks
// after services are skipped and said to be; and nothing is left but the
// logs.
#[test]
fn a_check_passes_skips_the_hooks_after_services_and_leaves_nothing_behind() {
    if skip_without_python() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let migrated = dir.path().join("migrated");
    let seeded = dir.path().join("seeded");
    let e = env(&config_running(
        &answering(404),
        &format!(
            "[[hooks]]\nname = \"migrate\"\nafter = \"services\"\non = \"always\"\ncmd = \"touch {}\"\n\n\
             [[hooks]]\nname = \"seed\"\nafter = \"dev\"\ncmd = \"touch {}\"\n",
            migrated.display(),
            seeded.display()
        ),
    ));
    let branches = e.git(&["branch", "--list"]);
    let out = e.pando(&["check"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "✓ plain is ready: `pando` opens it\n"
    );
    assert!(err.contains("in a throwaway worktree (no branch)"), "{err}");
    assert!(
        err.contains("skipped the hooks that run after services (migrate, seed)"),
        "{err}"
    );
    assert!(err.contains("dev ready :"), "{err}");
    assert!(err.contains("(HTTP 404)"), "{err}");
    assert!(
        err.contains("removed the test worktree; nothing left behind"),
        "{err}"
    );
    assert!(!migrated.exists(), "a hook after services ran in a check");
    assert!(!seeded.exists(), "a hook after dev ran in a check");
    e.assert_nothing_left(&branches);
    let config = pando::config::load(&e.paths).unwrap().config;
    assert!(
        !config.worktrees_dir(&e.paths).exists(),
        "the worktrees directory the check made, empty, went with it"
    );
    assert!(
        e.paths.log_file(CHECK_WORKTREE, "dev").is_file(),
        "the check's logs are kept"
    );

    let record = e.record().unwrap();
    assert_eq!(record.outcome, CheckOutcome::Passed);
    assert_eq!(record.processes.len(), 1);
    assert_eq!(record.processes[0].http_status, Some(404));
    assert!(record.processes[0].ready);
    assert_eq!(record.base_ref.as_deref(), Some("main"));
    assert_eq!(
        record.commit.as_deref(),
        Some(e.git(&["rev-parse", "main"]).trim())
    );
    assert!(!record.changed_while_running());
    assert_eq!(record.ran_by, pando::setup::RanBy::Program);
    assert_eq!(
        e.setup_state(),
        SetupState::Ready,
        "what the dashboard reads"
    );

    // The same, as the published shape, run by the TUI.
    let out = e
        .command(&["check", "--json"])
        .env(pando::actions::CHECK_RAN_BY_ENV, "tui")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json(&out);
    assert_eq!(v["result"], "passed");
    assert_eq!(v["kind"], serde_json::Value::Null);
    assert_eq!(v["ran_by"], "tui");
    assert_eq!(v["processes"][0]["http_status"], 404);
    e.assert_nothing_left(&branches);
}

#[test]
fn a_process_that_exits_at_once_fails_the_check_with_its_lines_redacted() {
    let e = env(&config_running(
        "echo 'connecting with password=hunter2'; echo 'Error: boom'; exit 3",
        "",
    ));
    let branches = e.git(&["branch", "--list"]);
    let out = e.pando(&["check", "--json"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    let v = json(&out);
    assert_eq!(v["result"], "failed");
    assert_eq!(v["kind"], "settings");
    assert_eq!(v["failed_process"], "dev");
    assert_eq!(v["processes"][0]["ready"], false);
    let tail: Vec<&str> = v["failed_tail"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(tail, ["connecting with password=(hidden)", "Error: boom"]);
    assert!(!err.contains("hunter2"), "{err}");
    assert!(err.contains("the last lines of the dev log:"), "{err}");
    assert!(err.contains("pando: the check failed: dev failed"), "{err}");
    e.assert_nothing_left(&branches);
    assert_eq!(e.setup_state(), SetupState::Failing);
}

#[test]
fn a_page_that_answers_500_fails_the_check() {
    if skip_without_python() {
        return;
    }
    let e = env(&config_running(&answering(500), ""));
    let branches = e.git(&["branch", "--list"]);
    let out = e.pando(&["check", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let v = json(&out);
    assert_eq!(v["kind"], "settings");
    assert!(
        v["reason"].as_str().unwrap().contains("HTTP 500"),
        "{}",
        v["reason"]
    );
    assert_eq!(v["processes"][0]["http_status"], 500);
    assert_eq!(v["failed_process"], "dev");
    e.assert_nothing_left(&branches);
}

// A server that takes the connection and never answers fails once the
// wait is over — shortened here, where it is ninety seconds for real.
#[test]
fn a_page_that_never_comes_fails_when_the_wait_is_over() {
    if skip_without_python() {
        return;
    }
    let e = env(&config_running(&common::listener_on_port_env(), ""));
    let config = pando::config::load(&e.paths).unwrap().config;
    let quiet = |_: &str| {};
    let say = pando::actions::Narration {
        step: &quiet,
        detail: &quiet,
    };
    let checked = pando::ports::with_page_wait(Duration::from_secs(1), || {
        pando::actions::check(&e.paths, &config, pando::setup::RanBy::Program, &say)
    })
    .unwrap();
    match &checked.record.outcome {
        CheckOutcome::Failed { kind, reason } => {
            assert_eq!(*kind, FailureKind::Settings);
            assert!(
                reason.contains("did not answer its first page within 1s"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!e.check_dir().exists());
}

// A shared service nothing answers for is the machine's: said with what
// starts it, and nothing is made — not even the install.
#[test]
fn a_stopped_shared_service_fails_as_the_machines_before_anything_is_made() {
    let dir = TempDir::new().unwrap();
    let installed = dir.path().join("installed");
    let e = env(&format!(
        "[project]\ninstall = \"touch {}\"\n\n[dev]\ncmd = \"sleep 600\"\nports = {{ PORT = \"web\" }}\n\n\
         [[services]]\nkind = \"native\"\nname = \"redis\"\nenv = {{ REDIS_URL = \"redis\" }}\n",
        installed.display()
    ));
    let port = free_port();
    let env_file = e.root.join(".env");
    let mut text = std::fs::read_to_string(&env_file).unwrap();
    text.push_str(&format!("REDIS_URL=redis://localhost:{port}/0\n"));
    std::fs::write(&env_file, text).unwrap();
    let branches = e.git(&["branch", "--list"]);

    let out = e.pando(&["check", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let v = json(&out);
    assert_eq!(v["result"], "failed");
    assert_eq!(v["kind"], "machine");
    let reason = v["reason"].as_str().unwrap();
    assert!(
        reason.contains(&format!("nothing answers on localhost:{port}")),
        "{reason}"
    );
    assert!(reason.contains("REDIS_URL"), "{reason}");
    assert!(stderr(&out).contains("This is the machine's to fix"));
    assert!(!installed.exists(), "nothing ran");
    let record = e.record().unwrap();
    assert!(
        !record.progress.iter().any(|l| l == "installing"),
        "{:?}",
        record.progress
    );
    e.assert_nothing_left(&branches);
}

// A question the rules cannot settle alone is exit 3, as everywhere, with
// the result still one object on stdout.
#[test]
fn a_question_still_open_exits_3_and_prints_the_result() {
    let e = env_of(Kind::MonoWebApi, None);
    let branches = e.git(&["branch", "--list"]);
    let out = e.pando(&["check", "--json"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    let v = json(&out);
    assert_eq!(v["result"], "not_set_up");
    assert_eq!(v["slot"], "processes");
    assert_eq!(v["commit"], serde_json::Value::Null);
    assert!(err.contains("never answers one itself"), "{err}");
    assert!(err.contains("Run these as separate processes?"), "{err}");
    assert_eq!(
        e.record().unwrap().outcome,
        CheckOutcome::NotSetUp {
            slot: "processes".to_string()
        }
    );
    e.assert_nothing_left(&branches);
}

#[test]
fn a_second_check_is_refused_while_one_runs() {
    let e = env(&config_running("sleep 600", ""));
    let _held = pando::state::try_lock(&e.paths.check_lock_file())
        .unwrap()
        .expect("the lock is free");
    let out = e.pando(&["check", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("a check is already running for this project"),
        "{}",
        stderr(&out)
    );
    assert!(
        out.stdout.is_empty(),
        "nothing on stdout: the running one's result is the one"
    );
    assert!(
        e.record().is_none(),
        "the running check's record is not written over"
    );
}

// `stop --all` stops a check's processes like any other's, and the check,
// finding nothing left to wait on, records itself interrupted and still
// takes its worktree down.
#[test]
fn a_stop_during_a_check_records_it_interrupted() {
    let e = env(&config_running("sleep 600", ""));
    let branches = e.git(&["branch", "--list"]);
    let child = e.spawn(&["check", "--json"]);
    e.wait_for_step("waiting for dev");
    let stopped = e.pando(&["stop", "--all"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(json(&out)["result"], "interrupted");
    assert_eq!(e.record().unwrap().outcome, CheckOutcome::Interrupted);
    e.assert_nothing_left(&branches);
}

#[test]
fn a_signal_ends_a_check_through_its_teardown() {
    let e = env(&config_running("sleep 600", ""));
    let branches = e.git(&["branch", "--list"]);
    let child = e.spawn(&["check"]);
    e.wait_for_step("waiting for dev");
    let pid = pando::state::load(&e.paths.state_file()).unwrap().worktrees[CHECK_WORKTREE]
        .processes["dev"]
        .pid;
    signal(&child, nix::sys::signal::Signal::SIGTERM);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("the check was stopped before it finished"));
    assert_eq!(e.record().unwrap().outcome, CheckOutcome::Interrupted);
    assert!(!pando::process::is_alive(pid), "its process was stopped");
    e.assert_nothing_left(&branches);
}

// What a SIGKILL leaves — a worktree with a server still running in it —
// is in no list, is a problem `doctor` reports, and is swept by the next
// check before it starts anything.
#[test]
fn a_killed_checks_leftover_is_hidden_reported_and_swept() {
    if skip_without_python() {
        return;
    }
    let e = env(&config_running(&common::listener_on_port_env(), ""));
    let branches = e.git(&["branch", "--list"]);
    let mut child = e.spawn(&["check"]);
    e.wait_for_step("asking dev for its first page");
    signal(&child, nix::sys::signal::Signal::SIGKILL);
    child.wait().unwrap();
    let pid = pando::state::load(&e.paths.state_file()).unwrap().worktrees[CHECK_WORKTREE]
        .processes["dev"]
        .pid;
    assert!(
        pando::process::is_alive(pid),
        "a SIGKILL leaves its server up"
    );
    assert!(e.git(&["worktree", "list"]).contains(CHECK_WORKTREE));
    assert_eq!(e.setup_state(), SetupState::Interrupted);

    let ls = e.pando(&["ls", "--json"]);
    assert!(!String::from_utf8_lossy(&ls.stdout).contains(CHECK_WORKTREE));
    let status = e.pando(&["status", "--json"]);
    assert!(!String::from_utf8_lossy(&status.stdout).contains(CHECK_WORKTREE));
    let names = e.pando(&["ls", "--names"]);
    assert!(!String::from_utf8_lossy(&names.stdout).contains(CHECK_WORKTREE));
    let by_name = e.pando(&["stop", CHECK_WORKTREE]);
    assert_eq!(by_name.status.code(), Some(1));
    assert!(
        stderr(&by_name).contains("no worktree named"),
        "{}",
        stderr(&by_name)
    );
    let doctor = e.pando(&["doctor", "--json"]);
    assert_eq!(doctor.status.code(), Some(1), "a leftover is a problem");
    let report = json(&doctor);
    assert!(
        report["findings"].as_array().unwrap().iter().any(|f| {
            f["severity"] == "problem"
                && f["message"]
                    .as_str()
                    .unwrap()
                    .contains("a `pando check` that did not finish")
                && f["fix"]
                    .as_str()
                    .unwrap()
                    .contains("`pando check` sweeps it")
        }),
        "{report}"
    );
    assert!(
        report["worktrees"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == CHECK_WORKTREE),
        "doctor shows it"
    );

    e.write_config(&config_running(&answering(200), ""));
    let out = e.pando(&["check"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("left its worktree behind — sweeping it first"),
        "{err}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !pando::process::is_alive(pid)),
        "the leftover's server was stopped"
    );
    e.assert_nothing_left(&branches);
    let doctor = e.pando(&["doctor", "--json"]);
    assert!(
        !String::from_utf8_lossy(&doctor.stdout).contains("did not finish"),
        "nothing left to report"
    );
}
