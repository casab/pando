//! Invariant 1: pando never writes into your repository.
//!
//! This is the most important test in the project. It runs every command
//! this phase has against a fixture repo and asserts, after each one, that
//! the repository is byte-for-byte the same tree it was before — not just
//! that `git status` is clean, because an ignored file would not show there.
//!
//! `.git` is excluded: `git worktree add` and `git worktree remove`
//! legitimately write `.git/worktrees/<name>`, and that is the one thing
//! the invariant explicitly allows.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{Kind, build, build_fresh_clone, git, git_raw, paths_for, status_porcelain};
use pando::config::{self, Config};
use pando::{actions, state};
use tempfile::TempDir;

/// What a path is, so a file silently replaced by a symlink (or a directory)
/// is caught as a change rather than compared only by name.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    File(u64),
    Dir,
    Symlink(PathBuf),
}

/// Every path under `root` except `.git`, keyed by its relative path.
fn tree(root: &Path) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Entry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked path is under root")
            .to_string_lossy()
            .to_string();
        if relative == ".git" {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path).expect("stat walked path");
        if meta.file_type().is_symlink() {
            out.insert(
                relative,
                Entry::Symlink(std::fs::read_link(&path).unwrap_or_default()),
            );
        } else if meta.is_dir() {
            out.insert(relative, Entry::Dir);
            walk(root, &path, out);
        } else {
            out.insert(relative, Entry::File(meta.len()));
        }
    }
}

struct Harness {
    _dir: TempDir,
    parent: PathBuf,
    root: PathBuf,
    home: PathBuf,
    paths: pando::paths::PandoPaths,
    config: Config,
    baseline: BTreeMap<String, Entry>,
}

impl Harness {
    /// Asserts the repository is untouched and, while the worktree exists,
    /// that it is clean too. Called after every single command.
    fn assert_untouched(&self, step: &str, worktree: Option<&Path>) {
        assert_eq!(
            status_porcelain(&self.root),
            "",
            "after {step}: the main checkout must have nothing to report"
        );
        assert_eq!(
            tree(&self.root),
            self.baseline,
            "after {step}: the repository tree changed — `git status` alone would not have seen it"
        );
        if let Some(wt) = worktree
            && wt.exists()
        {
            assert_eq!(
                status_porcelain(wt),
                "",
                "after {step}: the worktree must be clean too"
            );
        }
        // Everything new under the fixture's parent must be inside pando's
        // own home; nothing may be scattered next to the repository.
        for entry in std::fs::read_dir(&self.parent).unwrap().flatten() {
            let path = entry.path();
            assert!(
                path == self.home || path == self.root,
                "after {step}: {} appeared next to the repository",
                path.display()
            );
        }
    }
}

/// Nothing a test started outlives it, even when an assertion panics.
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
    }
}

fn harness() -> Harness {
    harness_with("[project]\nprovision = [\".env\", \".env.local\"]\n")
}

fn harness_with(config_toml: &str) -> Harness {
    harness_of(Kind::Plain, config_toml)
}

fn harness_of(kind: Kind, config_toml: &str) -> Harness {
    let h = harness_built(kind, config_toml, true);
    assert!(
        h.baseline.contains_key(".env"),
        "the fixture must have an ignored .env to provision"
    );
    h
}

/// A fixture as a fresh clone leaves it: the gitignored local files never
/// arrived, so the project's own example is the only thing a worktree could
/// be given a copy of.
fn fresh_clone_harness(kind: Kind, config_toml: &str) -> Harness {
    let h = harness_built(kind, config_toml, false);
    assert!(
        !h.baseline.contains_key(".env"),
        "a fresh clone has no .env — that is the whole case"
    );
    assert!(
        h.baseline.contains_key(".env.example"),
        "and it does have the example, tracked"
    );
    h
}

fn harness_built(kind: Kind, config_toml: &str, local_files: bool) -> Harness {
    let dir = TempDir::new().unwrap();
    // Canonical throughout: on macOS the temp dir is /var/... but git (and
    // every path pando canonicalises) says /private/var/..., and the
    // comparisons below are all path equality.
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    let root = match local_files {
        true => build(kind, &parent).root,
        false => build_fresh_clone(kind, &parent).root,
    };
    let home = parent.join("pando-home");
    let paths = paths_for(&home, &root);

    // Provisioning is the only thing that writes inside a worktree at all,
    // so the invariant is tested with it turned on.
    paths.ensure_home().unwrap();
    std::fs::write(paths.config_file(), config_toml).unwrap();
    let loaded = config::load(&paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    let baseline = tree(&root);
    Harness {
        parent,
        root: paths.root().to_path_buf(),
        home,
        paths,
        config: loaded.config,
        baseline,
        _dir: dir,
    }
}

// A test that only ran `git status` would pass while pando quietly filled
// the repository with ignored files, so the snapshot has to be the stricter
// check. This proves it is.
#[test]
fn the_tree_snapshot_catches_what_git_status_cannot() {
    let h = harness();
    std::fs::create_dir_all(h.root.join("node_modules")).unwrap();
    std::fs::write(h.root.join("node_modules").join("planted"), "x").unwrap();
    assert_eq!(
        status_porcelain(&h.root),
        "",
        "git status cannot see an ignored file — that is the point"
    );
    assert_ne!(
        tree(&h.root),
        h.baseline,
        "the snapshot must catch an ignored file appearing"
    );

    // A tracked file swapped for a symlink to the same content is another
    // change `git status` would report but a name-only snapshot would miss.
    let h = harness();
    let readme = h.root.join("README.md");
    std::fs::remove_file(&readme).unwrap();
    std::os::unix::fs::symlink(h.root.join(".env"), &readme).unwrap();
    assert_ne!(tree(&h.root), h.baseline, "a symlink swap must be caught");
}

#[test]
fn every_command_leaves_the_repository_untouched() {
    let h = harness();
    h.assert_untouched("setup", None);

    actions::ls(&h.paths).unwrap();
    h.assert_untouched("ls on an empty project", None);

    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));
    assert!(
        worktree.join(".env").exists(),
        "provisioning must have linked .env into the worktree"
    );

    actions::ls(&h.paths).unwrap();
    h.assert_untouched("ls", Some(&worktree));

    let printed = actions::path(&h.paths, &name).unwrap();
    assert_eq!(printed, worktree.canonicalize().unwrap());
    h.assert_untouched("path", Some(&worktree));

    actions::created_by_pando(&h.paths, &actions::ls(&h.paths).unwrap());
    h.assert_untouched("created_by_pando", Some(&worktree));

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
    assert!(!worktree.exists());
}

// The refusal path matters just as much: a `new` that cannot proceed must
// not leave a half-made directory or a stray branch behind.
#[test]
fn a_refused_command_leaves_the_repository_untouched() {
    let mut h = harness();
    h.config.project.provision = Some(vec!["README.md".into()]);
    assert!(
        actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).is_err(),
        "README.md is tracked, so provisioning it must be refused"
    );
    h.assert_untouched("a refused new", None);

    assert!(actions::rm(&h.paths, "nope", true, true, &|_| {}).is_err());
    h.assert_untouched("a refused rm", None);

    assert!(actions::path(&h.paths, "nope").is_err());
    h.assert_untouched("a refused path", None);
}

#[test]
fn everything_pando_writes_lives_under_its_own_home() {
    let h = harness();
    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();

    for path in [
        h.paths.config_file(),
        h.paths.state_file(),
        h.paths.worktree_path(&name),
    ] {
        assert!(path.exists(), "{} should have been written", path.display());
        assert!(
            path.starts_with(&h.home),
            "{} escaped pando's home",
            path.display()
        );
    }
    // The state file knows the worktree is pando's own, which is what lets
    // `rm` tell it apart from an adopted one.
    let store = state::load(&h.paths.state_file()).unwrap();
    assert!(store.worktrees.get(&name).unwrap().created_by_pando);
}

// An adopted worktree lives outside pando's home, so the invariant has to
// hold for a repository pando did not lay out.
#[test]
fn adopting_and_removing_a_worktree_elsewhere_leaves_the_repository_untouched() {
    let h = harness();
    let adopted = h.parent.join("adopted-elsewhere");
    git(
        &h.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "adopted",
            adopted.to_str().unwrap(),
        ],
    );
    assert_eq!(
        status_porcelain(&h.root),
        "",
        "adding a worktree elsewhere must not dirty the repository"
    );
    assert_eq!(tree(&h.root), h.baseline);

    let listed = actions::ls(&h.paths).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "adopted-elsewhere");
    assert_eq!(tree(&h.root), h.baseline, "ls must not write anything");

    assert!(
        actions::rm(&h.paths, "adopted-elsewhere", false, false, &|_| {}).is_err(),
        "an adopted worktree needs --yes"
    );
    actions::rm(&h.paths, "adopted-elsewhere", true, false, &|_| {}).unwrap();
    assert_eq!(status_porcelain(&h.root), "");
    assert_eq!(tree(&h.root), h.baseline);
}

// Config is read from the repository when a team commits one, and written
// only to pando's home. Both halves are covered here because a regression
// would be the quietest possible invariant break.
#[test]
fn config_is_read_from_the_repository_but_never_written_to_it() {
    let h = harness();
    std::fs::write(
        h.root.join("pando.toml"),
        "[project]\nbase = \"main\"\nworktrees_dir = \"/tmp/hijacked\"\n",
    )
    .unwrap();
    // A committed pando.toml is a tracked file in a real project; here it is
    // just written, so the baseline is retaken to keep the comparison about
    // what pando does.
    let baseline = tree(&h.root);

    let loaded = config::load(&h.paths).unwrap();
    assert_eq!(loaded.config.project.base.as_deref(), Some("main"));
    assert_eq!(
        loaded.config.project.worktrees_dir, None,
        "a committed file may not redirect where pando writes"
    );
    assert_eq!(loaded.warnings.len(), 1);

    config::write(&h.paths, &loaded.config).unwrap();
    assert_eq!(
        tree(&h.root),
        baseline,
        "config::write must not touch the repository"
    );
    assert!(h.paths.config_file().starts_with(&h.home));
}

fn branch_exists(root: &Path, branch: &str) -> bool {
    git_raw(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .status
    .success()
}

// The pre-flight `check-ignore` runs in the main checkout, but the file is
// written into a worktree that has a different commit checked out — and a
// different `.gitignore`. The worktree is where the invariant has to hold,
// so that is where the last word is.
//
// The provisioned path is deliberately not `.env`: a name a developer's own
// global gitignore might list would make this pass for the wrong reason.
#[test]
fn a_branch_whose_gitignore_lacks_the_provision_path_is_refused() {
    let mut h = harness();
    std::fs::write(
        h.root.join(".gitignore"),
        ".env\n.env.local\nnode_modules/\nlocal.pando\n",
    )
    .unwrap();
    git(&h.root, &["add", ".gitignore"]);
    git(&h.root, &["commit", "--quiet", "-m", "ignore local.pando"]);
    std::fs::write(h.root.join("local.pando"), "TOKEN=1\n").unwrap();

    // A branch committed before the ignore rule existed.
    git(&h.root, &["checkout", "--quiet", "-b", "legacy"]);
    std::fs::write(h.root.join(".gitignore"), ".env\nnode_modules/\n").unwrap();
    git(&h.root, &["commit", "--quiet", "-am", "legacy gitignore"]);
    git(&h.root, &["checkout", "--quiet", "main"]);

    h.config.project.provision = Some(vec!["local.pando".into()]);
    h.baseline = tree(&h.root);

    let err = actions::new(&h.paths, &h.config, "legacy", None, &|_| {}).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(msg.contains("local.pando"), "{msg}");

    h.assert_untouched("a new refused inside the worktree", None);
    assert!(
        actions::ls(&h.paths).unwrap().is_empty(),
        "the half-created worktree must have been unwound"
    );
    assert!(
        !h.config.worktrees_dir(&h.paths).join("legacy").exists(),
        "the worktree directory must be gone"
    );
    assert!(
        branch_exists(&h.root, "legacy"),
        "a branch pando did not create must survive the unwind"
    );
    let store = state::load(&h.paths.state_file()).unwrap_or_else(|_| state::State::new());
    assert!(!store.worktrees.contains_key("legacy"));
}

// The same hole from the other side: an *uncommitted* `.gitignore` edit in
// the main checkout authorises the pre-flight, and the worktree checks out
// the committed file that never had the rule.
#[test]
fn an_uncommitted_gitignore_edit_does_not_authorise_a_write_into_a_worktree() {
    let mut h = harness();
    std::fs::write(
        h.root.join(".gitignore"),
        ".env\n.env.local\nnode_modules/\nsecrets.local\n",
    )
    .unwrap();
    std::fs::write(h.root.join("secrets.local"), "TOKEN=1\n").unwrap();
    h.config.project.provision = Some(vec!["secrets.local".into()]);

    // The edit itself is the user's, so the comparison is against the tree
    // as they left it rather than against an empty `git status`.
    let before_status = status_porcelain(&h.root);
    assert!(before_status.contains(".gitignore"), "{before_status}");
    let before_tree = tree(&h.root);

    let err = actions::new(&h.paths, &h.config, "feat/u", None, &|_| {}).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(msg.contains("secrets.local"), "{msg}");

    assert_eq!(status_porcelain(&h.root), before_status);
    assert_eq!(tree(&h.root), before_tree);
    assert!(actions::ls(&h.paths).unwrap().is_empty());
    assert!(
        !branch_exists(&h.root, "feat/u"),
        "a branch pando created in this call must be deleted by the unwind"
    );
}

/// The whole Phase 2 lifecycle, with the tree snapshot checked after every
/// step.
///
/// The dev process is a real one that really binds a port, so readiness,
/// observed ports, and the log tail are all exercised — and every one of
/// them is a chance to write a file where pando must not.
#[test]
fn starting_and_stopping_never_writes_into_the_repository() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut toml = String::new();
    toml.push_str("[project]\nprovision = [\".env\", \".env.local\"]\n");
    // The simplest install step that can succeed. It still writes a log,
    // a fingerprint, and a hook record — all of which must land in the home.
    toml.push_str("install = \"true\"\n\n[dev]\ncmd = '''");
    toml.push_str(&common::listener_on_port_template());
    toml.push_str("'''\nports = [\"web\"]\n");

    let h = harness_with(&toml);
    assert_eq!(h.config.project.install.as_deref(), Some("true"));
    h.assert_untouched("setup", None);

    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new with an install step", Some(&worktree));
    assert!(
        h.paths.log_file(&name, "install").exists(),
        "the install hook logs under pando's home"
    );

    let outcome = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    let port = outcome.ports["web"];
    h.assert_untouched("start", Some(&worktree));

    // Wait for it to really be listening, which is when observed ports and
    // the Running phase appear — the states with the most to write.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut running = false;
    while std::time::Instant::now() < deadline {
        let state = actions::refresh(&h.paths).state;
        if matches!(
            state.worktrees[&name].processes["dev"].phase,
            state::Phase::Running { .. }
        ) && state.worktrees[&name].observed_ports.contains(&port)
        {
            running = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        running,
        "the listener never came up on {port}: {:?}",
        std::fs::read_to_string(h.paths.log_file(&name, "dev"))
    );
    h.assert_untouched("refresh while running", Some(&worktree));

    // The read paths a human and a machine use.
    let mut out = Vec::new();
    pando::cli::status_json(&h.paths, None, &mut out).unwrap();
    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(status["worktrees"][0]["ports"]["web"], port);
    h.assert_untouched("status --json", Some(&worktree));

    let mut out = Vec::new();
    pando::cli::logs(&h.paths, &name, "dev", 5, false, false, &mut out).unwrap();
    assert!(
        String::from_utf8_lossy(&out).contains("listening on"),
        "the log has the process's own output"
    );
    h.assert_untouched("logs --tail 5", Some(&worktree));

    let mut out = Vec::new();
    pando::cli::ls_text_at(&h.paths, &mut out, 200).unwrap();
    h.assert_untouched("ls", Some(&worktree));

    assert_eq!(
        actions::stop(&h.paths, &name, None, &|_| {}).unwrap(),
        actions::StopOutcome::Stopped(vec!["dev".to_string()])
    );
    h.assert_untouched("stop", Some(&worktree));

    let restarted = actions::restart(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        restarted.ports["web"], port,
        "a restart keeps the port, so the URL keeps working"
    );
    h.assert_untouched("restart", Some(&worktree));

    actions::stop(&h.paths, &name, None, &|_| {}).unwrap();
    h.assert_untouched("stop again", Some(&worktree));

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
    assert!(!worktree.exists());
    assert!(
        !h.paths.logs_dir(&name).exists(),
        "rm wipes the logs it wrote"
    );
}

/// Phase 2b review, finding 1: a process's name is a path component of its
/// log file, so a hand-written `[processes."../../x"]` created — and
/// truncated — a `.log` file outside pando's home, and, aimed back at the
/// checkout, inside the repository.
///
/// The invariant test's own blind spot: nothing else here gives a process
/// an adversarial name.
#[test]
fn an_adversarial_process_name_writes_nothing_outside_pandos_home() {
    let h = harness();
    let name = actions::new(&h.paths, &h.config, "feat/esc", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));

    // Five levels above `logs/<worktree>/` is the fixture's own parent
    // directory, which is where the repository sits. One name lands beside
    // it; the other lands inside it, which is Invariant 1 itself.
    let repo = h
        .root
        .file_name()
        .expect("the fixture root has a name")
        .to_string_lossy()
        .to_string();
    for escape in [
        "../../../../../escaped-log".to_string(),
        format!("../../../../../{repo}/inside-repo"),
    ] {
        std::fs::write(
            h.paths.config_file(),
            format!(
                "[project]\nprovision = [\".env\"]\n\n[processes.\"{escape}\"]\n\
                 cmd = \"true\"\nports = []\n"
            ),
        )
        .unwrap();
        match config::load(&h.paths) {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains(&format!("{escape:?}")),
                    "the refusal quotes the name: {msg}"
                );
            }
            Ok(loaded) => {
                // Unfixed: the name loads, and starting it is what writes
                // the file. The assertions below are the ones that fail.
                let _ = actions::start(
                    &h.paths,
                    &loaded.config,
                    &name,
                    None,
                    actions::Mode::Remembered,
                    &|_| {},
                );
                h.assert_untouched(&format!("start of {escape:?}"), Some(&worktree));
                panic!("a process name that escapes the log directory must be refused at load");
            }
        }
        h.assert_untouched(&format!("load of {escape:?}"), Some(&worktree));
        assert!(
            !h.parent.join("escaped-log.log").exists(),
            "a log was written above pando's home"
        );
        assert!(
            !h.root.join("inside-repo.log").exists(),
            "a log was written into the repository"
        );
    }
}

/// Phase 2b: the same guarantee with two processes in two directories.
///
/// The workspace fixture, its listener config, and the whole lifecycle —
/// including the two commands that only exist because there are two
/// processes: `logs --source api` and `stop --only web`.
#[test]
fn two_processes_never_write_into_the_repository() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let h = harness_of(Kind::MonoWebApi, &common::workspace_listener_config());
    h.assert_untouched("setup", None);

    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));

    let report = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        report
            .started
            .iter()
            .map(|p| p.process.as_str())
            .collect::<Vec<_>>(),
        vec!["api", "web"],
        "alphabetically by process name, which is the order they are spawned in — \
         the file lists web first, and nothing in the loader preserves that"
    );
    let web_port = report.ports["web"];
    let api_port = report.ports["api"];
    assert_eq!(
        report.url,
        Some(format!("http://localhost:{web_port}")),
        "one URL for the worktree, and it is the web role's"
    );
    h.assert_untouched("start", Some(&worktree));

    // Both up, both observed, and the web process carrying the api's real
    // port — which is the whole reason `{port:<role>}` exists.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut running = false;
    while std::time::Instant::now() < deadline {
        let state = actions::refresh(&h.paths).state;
        let record = &state.worktrees[&name];
        if matches!(
            state::aggregate_phase(record),
            Some(state::Aggregate::Running { .. })
        ) && record.observed_ports.contains(&web_port)
            && record.observed_ports.contains(&api_port)
        {
            running = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        running,
        "the listeners never came up on {web_port} and {api_port}: {:?} / {:?}",
        std::fs::read_to_string(h.paths.log_file(&name, "web")),
        std::fs::read_to_string(h.paths.log_file(&name, "api"))
    );
    let web_log = std::fs::read_to_string(h.paths.log_file(&name, "web")).unwrap();
    assert!(
        web_log.contains(&format!("VITE_API_URL=http://localhost:{api_port}")),
        "the web process is told where the api really is: {web_log}"
    );
    h.assert_untouched("refresh while both are running", Some(&worktree));

    // What a machine reads.
    let mut out = Vec::new();
    pando::cli::status_json(&h.paths, None, &mut out).unwrap();
    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let wt = &status["worktrees"][0];
    assert_eq!(wt["ports"]["web"], web_port);
    assert_eq!(wt["ports"]["api"], api_port);
    assert_eq!(wt["processes"]["web"]["phase"], "running");
    assert_eq!(wt["processes"]["api"]["phase"], "running");
    h.assert_untouched("status --json", Some(&worktree));

    // One log per process, and `--source` picks between them.
    let mut out = Vec::new();
    pando::cli::logs(&h.paths, &name, "api", 5, false, false, &mut out).unwrap();
    let api_log = String::from_utf8_lossy(&out).into_owned();
    assert!(
        api_log.contains(&format!("listening on {api_port}")),
        "the api's own log: {api_log}"
    );
    assert!(
        !api_log.contains("VITE_API_URL"),
        "and not the web process's: {api_log}"
    );
    h.assert_untouched("logs --source api", Some(&worktree));

    // Stopping one leaves the other serving.
    let web_pgid = report
        .started
        .iter()
        .find(|p| p.process == "web")
        .expect("a web process")
        .record
        .pgid;
    assert_eq!(
        actions::stop(&h.paths, &name, Some("web"), &|_| {}).unwrap(),
        actions::StopOutcome::Stopped(vec!["web".to_string()])
    );
    assert!(!pando::process::group_alive(web_pgid));
    let state = actions::refresh(&h.paths).state;
    assert_eq!(
        state.worktrees[&name]
            .processes
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["api"],
        "the api never stopped"
    );
    h.assert_untouched("stop --only web", Some(&worktree));

    // Starting again brings the missing one back on the same ports.
    let again = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        again
            .started
            .iter()
            .map(|p| p.process.as_str())
            .collect::<Vec<_>>(),
        vec!["web"],
        "only what was missing"
    );
    assert_eq!(
        again
            .already_running
            .iter()
            .map(|p| p.process.as_str())
            .collect::<Vec<_>>(),
        vec!["api"]
    );
    assert_eq!(again.ports, report.ports, "and on the ports it already had");
    h.assert_untouched("start again", Some(&worktree));

    actions::stop(&h.paths, &name, None, &|_| {}).unwrap();
    h.assert_untouched("stop", Some(&worktree));

    // Every log the pair wrote lives under pando's home.
    for source in ["web", "api", "install"] {
        let log = h.paths.log_file(&name, source);
        assert!(log.exists(), "{source} has no log");
        assert!(
            log.starts_with(&h.home),
            "{} escaped the home",
            log.display()
        );
    }

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
    assert!(!worktree.exists());
    assert!(
        !h.paths.logs_dir(&name).exists(),
        "rm wipes the logs it wrote"
    );
}

/// Everything the lifecycle writes is under the home: nothing is scattered
/// next to the repository, and nothing is left in the worktree.
#[test]
fn every_file_the_lifecycle_writes_is_under_pandos_home() {
    let h = harness_with("[project]\ninstall = \"true\"\n\n[dev]\ncmd = \"sleep 30\"\n");
    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let outcome = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    actions::refresh(&h.paths);

    for path in [
        h.paths.config_file(),
        h.paths.state_file(),
        h.paths.log_file(&name, "dev"),
        h.paths.log_file(&name, "install"),
    ] {
        assert!(path.exists(), "{} should have been written", path.display());
        assert!(
            path.starts_with(&h.home),
            "{} escaped pando's home",
            path.display()
        );
    }
    let store = state::load(&h.paths.state_file()).unwrap();
    let record = &store.worktrees[&name];
    assert!(record.hooks.contains_key("install"));
    assert_eq!(record.processes["dev"].pid, outcome.started[0].record.pid);

    actions::stop(&h.paths, &name, None, &|_| {}).unwrap();
    h.assert_untouched("the whole lifecycle", None);
}

/// The whole share lifecycle, against a fake provider: a tunnel, an auth
/// command, and a proxy all write logs and spawn processes, and none of it
/// may touch the repository.
#[test]
fn sharing_never_writes_into_the_repository() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let mut toml = String::new();
    toml.push_str("[project]\nprovision = [\".env\", \".env.local\"]\ninstall = \"true\"\n\n");
    toml.push_str("[dev]\ncmd = '''");
    toml.push_str(&common::listener_on_port_template());
    toml.push_str("'''\nports = [\"web\"]\n\n");
    // An auth command that runs *in the worktree* — the one thing about a
    // share that executes a project's own script there.
    toml.push_str("[share]\nauth_cmd = \"printf 'pando_session=abc123'\"\n");

    let h = harness_with(&toml);
    common::fake_cloudflared(&h.home);
    h.assert_untouched("setup", None);

    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    let outcome = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    let port = outcome.ports["web"];
    assert!(
        wait_for(|| {
            matches!(
                actions::refresh(&h.paths).state.worktrees[&name].processes["dev"].phase,
                state::Phase::Running { .. }
            )
        }),
        "the listener never came up on {port}: {:?}",
        std::fs::read_to_string(h.paths.log_file(&name, "dev"))
    );
    h.assert_untouched("start", Some(&worktree));

    let shared = share_through_the_binary(&h, &name);
    assert_eq!(shared.public_url, common::FAKE_TUNNEL_URL);
    assert!(shared.pre_authed, "auth_cmd means a proxy");
    h.assert_untouched("share", Some(&worktree));

    // Everything the share wrote is under the home, including the isolated
    // cloudflared config that exists to shadow the user's own.
    for path in [
        h.paths.log_file(&name, "tunnel"),
        h.paths.log_file(&name, "proxy"),
        h.paths.tunnel_config_file(),
    ] {
        assert!(path.exists(), "{} should have been written", path.display());
        assert!(
            path.starts_with(&h.home),
            "{} escaped pando's home",
            path.display()
        );
    }
    assert!(
        !std::fs::read_to_string(h.paths.tunnel_config_file())
            .unwrap()
            .contains("ingress:"),
        "the config pando writes must define no ingress"
    );

    let mut out = Vec::new();
    pando::cli::status_json(&h.paths, None, &mut out).unwrap();
    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        status["worktrees"][0]["share"]["url"],
        common::FAKE_TUNNEL_URL
    );
    h.assert_untouched("status --json while shared", Some(&worktree));

    // The cookie is in the proxy's environment and nowhere on disk.
    let logs = h.paths.logs_dir(&name);
    let written: String = std::fs::read_dir(&logs)
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect::<Vec<_>>()
        .join("\n");
    let state_text = std::fs::read_to_string(h.paths.state_file()).unwrap();
    for haystack in [&written, &state_text] {
        assert!(
            !haystack.contains("pando_session=abc123"),
            "the cookie reached something that outlives the share:\n{haystack}"
        );
    }

    actions::unshare(&h.paths, &name).unwrap();
    h.assert_untouched("unshare", Some(&worktree));

    // And again, so the second share reuses the proxy port and starts from
    // a truncated tunnel log rather than the last session's URL.
    let again = share_through_the_binary(&h, &name);
    assert_eq!(again.public_url, common::FAKE_TUNNEL_URL);
    h.assert_untouched("share again", Some(&worktree));

    actions::stop(&h.paths, &name, None, &|_| {}).unwrap();
    assert!(
        state::load(&h.paths.state_file()).unwrap().worktrees[&name]
            .share
            .is_none(),
        "a stopped worktree never keeps a public URL"
    );
    h.assert_untouched("stop", Some(&worktree));

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
    assert!(
        !h.paths.logs_dir(&name).exists(),
        "rm wipes the tunnel and proxy logs with the rest"
    );
}

/// `share`, with the proxy told to re-exec the real `pando` binary.
///
/// The proxy runs `pando __share-proxy` in whatever binary is running —
/// which inside an integration test is this test harness, and it exits at
/// once. Everything else here is the real path.
fn share_through_the_binary(h: &Harness, name: &str) -> actions::ShareOutcome {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_pando"));
    let provider = pando::tunnel::provider_for(h.config.share.provider.as_deref()).unwrap();
    actions::share_with(
        &h.paths,
        &h.config,
        name,
        provider.as_ref(),
        &|paths, name, listen, upstream, cookie| {
            pando::share_proxy::spawn_with(paths, name, listen, upstream, cookie, &binary)
        },
        &|_| {},
    )
    .expect("share")
}

/// Polls until `ready` or twenty seconds pass.
fn wait_for(ready: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Fixture 1 with a fake docker in pando's home and a config that runs
/// private copies of its compose services.
fn isolated_harness() -> Harness {
    let h = harness_of(
        Kind::NextPnpmCompose,
        &format!(
            "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
             [dev]\ncmd = '''{}'''\nports = {{ PORT = \"web\" }}\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\", \"redis\"]\n\
             env = {{ DATABASE_URL = \"postgres\", REDIS_URL = \"redis\" }}\n",
            common::listener_on_port_env()
        ),
    );
    common::docker::install(&h.home);
    h
}

/// The phase's own invariant test: isolation adds an override file, two
/// service logs, a compose project and a set of volumes, and not one of
/// them may land inside the developer's repository.
#[test]
fn an_isolated_lifecycle_never_writes_into_the_repository() {
    if !common::python3_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let h = isolated_harness();
    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));

    let report = actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .unwrap();
    assert!(report.ports.contains_key("postgres"), "{:?}", report.ports);
    h.assert_untouched("start --isolated", Some(&worktree));

    // The compose file pando read is the project's own, and it is
    // untouched: the override that remaps the ports is a separate file
    // under the home.
    let override_file = h.paths.compose_override_file(&name);
    assert!(override_file.is_file(), "the override was not written");
    assert!(
        override_file.starts_with(&h.home),
        "{} escaped pando's home",
        override_file.display()
    );
    assert!(
        std::fs::read_to_string(&override_file)
            .unwrap()
            .contains("!override")
    );

    actions::refresh(&h.paths);
    h.assert_untouched("status", Some(&worktree));

    // Each service's container log is a file under the home, like any
    // other log source.
    for service in ["postgres", "redis"] {
        let log = h.paths.log_file(&name, service);
        assert!(
            log.starts_with(&h.home),
            "{} escaped pando's home",
            log.display()
        );
        let mut text = String::new();
        for _ in 0..80 {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains(service) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(text.contains(service), "{service}: {text:?}");
    }
    h.assert_untouched("logs", Some(&worktree));

    actions::stop(&h.paths, &name, None, &|_| {}).unwrap();
    h.assert_untouched("stop", Some(&worktree));

    // A plain start after a stop: the worktree remembers it is isolated,
    // and still writes nothing into the repository.
    actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap();
    assert!(state::load(&h.paths.state_file()).unwrap().worktrees[&name].isolated);
    h.assert_untouched("start", Some(&worktree));

    // And the way back, which stops containers and restarts processes:
    // still not a byte inside the repository.
    actions::start(
        &h.paths,
        &h.config,
        &name,
        None,
        actions::Mode::Shared,
        &|_| {},
    )
    .unwrap();
    assert!(!state::load(&h.paths.state_file()).unwrap().worktrees[&name].isolated);
    h.assert_untouched("start --shared", Some(&worktree));

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
    assert!(
        !override_file.exists(),
        "rm takes the override with the worktree"
    );

    // And everything the fake docker was told to pretend lives under the
    // home too — a marker file next to the repository would be exactly
    // the write this test exists to catch.
    assert!(common::docker::state_dir(&h.home).starts_with(&h.home));
    assert!(!common::docker::invocations(&h.home).is_empty());
}

// A failed install keeps the worktree — and still writes nothing into it.
#[test]
fn a_failed_install_leaves_the_repository_and_the_worktree_alone() {
    let h = harness_with("[project]\ninstall = \"echo nope >&2 && exit 1\"\n");
    let err = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("install"), "{err:#}");
    let worktree = h.config.worktrees_dir(&h.paths).join("feat+one");
    assert!(worktree.is_dir(), "the worktree survives a failed install");
    h.assert_untouched("a failed install", Some(&worktree));
}

// A fresh clone has no `.env` to link, and the project's own `.env.example`
// is right there. Seeding from it is still a write *into a worktree*, so it
// is held to Invariant 1 in full: the worktree's own gitignore authorises
// it, nothing lands in the repository, and the file is a copy — a symlink
// to the tracked example would make the worktree's edits writes into the
// repository by the back door.
#[test]
fn seeding_a_worktree_env_file_from_the_example_writes_nothing_into_the_repository() {
    let h = fresh_clone_harness(
        Kind::NextPnpmCompose,
        "[project]\nprovision = [\".env\"]\nprovision_from = { \".env\" = \".env.example\" }\n",
    );
    h.assert_untouched("setup", None);

    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new with a seeded provision source", Some(&worktree));

    let seeded = worktree.join(".env");
    let example = std::fs::read_to_string(h.root.join(".env.example")).unwrap();
    assert_eq!(
        std::fs::read_to_string(&seeded).unwrap(),
        example,
        "the worktree got the example's contents"
    );
    assert!(
        !std::fs::symlink_metadata(&seeded)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a link would make an edit in the worktree a write into the repository"
    );
    assert!(
        !h.root.join(".env").exists(),
        "and nothing was created in the main checkout"
    );

    actions::rm(&h.paths, &name, false, false, &|_| {}).unwrap();
    h.assert_untouched("rm", None);
}

// The same file, on a branch whose committed gitignore never had the rule.
// The worktree is what has the last word, and the refusal has to unwind
// everything it created.
#[test]
fn a_branch_that_does_not_ignore_a_seeded_file_is_refused_and_leaves_nothing() {
    let mut h = fresh_clone_harness(
        Kind::NextPnpmCompose,
        "[project]\nprovision = [\".env\"]\nprovision_from = { \".env\" = \".env.example\" }\n",
    );
    git(&h.root, &["checkout", "--quiet", "-b", "legacy"]);
    std::fs::write(h.root.join(".gitignore"), "node_modules/\n.next/\n").unwrap();
    git(&h.root, &["commit", "--quiet", "-am", "an older gitignore"]);
    git(&h.root, &["checkout", "--quiet", "main"]);
    h.baseline = tree(&h.root);

    let err = actions::new(&h.paths, &h.config, "legacy", None, &|_| {}).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("not ignored"), "{msg}");
    assert!(msg.contains(".env"), "{msg}");

    h.assert_untouched("a seeded write the worktree refused", None);
    assert!(
        actions::ls(&h.paths).unwrap().is_empty(),
        "the half-created worktree must have been unwound"
    );
    assert!(
        !h.config.worktrees_dir(&h.paths).join("legacy").exists(),
        "the worktree directory must be gone"
    );
}

// The runtime question is about the machine, so its answer goes to the
// machine-wide layer under pando's home — and the probe that provoked it
// leaves a cache there too. Neither may land in the repository, and a
// refused answer may land nowhere at all.
#[test]
fn answering_the_runtime_question_writes_only_under_pandos_home() {
    let mut h = harness_with(
        "[project]\nprovision = [\".env\"]\n\n[dev]\ncmd = \"sleep 30\"\nports = []\n",
    );
    // A pin nothing on any machine resolves, committed so the fixture's
    // own tree is still clean.
    std::fs::write(h.root.join(".nvmrc"), "99\n").unwrap();
    git(&h.root, &["add", ".nvmrc"]);
    git(&h.root, &["commit", "--quiet", "-m", "pin a runtime"]);
    // The pin is part of the fixture from here on.
    h.baseline = tree(&h.root);
    let baseline = tree(&h.root);
    let project_layer = std::fs::read_to_string(h.paths.config_file()).unwrap();

    // A prelude that really does fix it: a `node` of this test's own,
    // under pando's home rather than anywhere near the repository.
    let bin = h.home.join("fake-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let node = bin.join("node");
    std::fs::write(&node, "#!/bin/sh\necho v99.0.0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prelude = format!("export PATH=\"{}:$PATH\"", bin.display());

    // A line that does not work is refused, and nothing is written.
    let refused = actions::resolve_process(
        &h.paths,
        &h.config,
        actions::Mode::Remembered,
        &|_| Ok(actions::Answer::Custom("true".to_string())),
        &|_| {},
    );
    assert!(refused.is_err(), "a prelude that does not work is refused");
    assert!(
        !h.paths.user_config_file().exists(),
        "and a refused answer is not written down"
    );
    assert_eq!(
        tree(&h.root),
        baseline,
        "a refused answer touched the repository"
    );

    // And one that works is written to the machine-wide layer.
    let answer = prelude.clone();
    let config = actions::resolve_process(
        &h.paths,
        &h.config,
        actions::Mode::Remembered,
        &move |_| Ok(actions::Answer::Custom(answer.clone())),
        &|_| {},
    )
    .unwrap();
    assert_eq!(config.runtime.prelude.as_deref(), Some(prelude.as_str()));

    for written in [h.paths.user_config_file(), h.paths.runtime_cache_file()] {
        assert!(
            written.exists() && written.starts_with(&h.home),
            "{} must exist under pando's home",
            written.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(h.paths.config_file()).unwrap(),
        project_layer,
        "the project layer says what the project needs, not what this laptop does"
    );
    assert_eq!(tree(&h.root), baseline);
    assert_eq!(status_porcelain(&h.root), "");
    h.assert_untouched("answering the runtime question", None);
}

// ---- the commands that answer questions -----------------------------------

/// The real binary, with its home injected, in the fixture's main
/// checkout. These commands are the ones a developer or an agent runs
/// *before* anything is configured, which is exactly when a tool is most
/// tempted to leave something behind.
fn pando(h: &Harness, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_pando"))
        .env("PANDO_HOME", &h.home)
        .current_dir(&h.root)
        .args(args)
        .output()
        .expect("run pando")
}

#[test]
fn init_and_signals_never_write_into_the_repository() {
    // No config at all, and the fixture whose rules cannot settle
    // everything, so these runs really do answer questions rather than
    // finding it all decided.
    let h = harness_of(Kind::NextMessy, "");
    // The one answer that is about this machine rather than the fixture.
    // Without it a host whose `bash -lc` does not resolve the pinned node
    // would be answering a different question.
    std::fs::write(h.home.join("config.toml"), "[runtime]\nprelude = \"\"\n").unwrap();

    // A worktree, so "every worktree" has one to check.
    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));

    let answers = h.home.join("answers.json");
    std::fs::write(
        &answers,
        r#"{"dev_cmd": "pnpm dev:web", "port_env": "PORT", "services": ["db", "cache"]}"#,
    )
    .unwrap();
    let answers = answers.to_str().unwrap().to_string();

    for args in [
        vec!["signals"],
        vec!["init", "--dry-run", "--yes"],
        vec!["init", "--answers", &answers, "--dry-run"],
        vec!["init", "--answers", &answers],
        vec!["init", "--yes"],
        vec!["init"],
        vec!["signals"],
    ] {
        let out = pando(&h, &args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        h.assert_untouched(&args.join(" "), Some(&worktree));
    }

    // And the answers really did land, so this is a test about a pass that
    // did something rather than one that found nothing to do.
    let written = std::fs::read_to_string(h.paths.config_file()).unwrap();
    assert!(written.contains("# answered: a program,"), "{written}");
    assert!(written.contains(r#"cmd = "pnpm dev:web""#), "{written}");
}

// doctor is the command a developer runs when something is already wrong,
// so it is the one that must not change anything on the way past. Not the
// repository, and not pando's own home either: it reads three config
// layers, loads the state, probes a login shell and walks every project
// folder, and every one of those has a write next to it that would have
// been easy to reach for.
#[test]
fn doctor_writes_nothing_anywhere() {
    let h = harness_of(Kind::NextMessy, "");
    // The one answer that is about this machine rather than the fixture.
    std::fs::write(h.home.join("config.toml"), "[runtime]\nprelude = \"\"\n").unwrap();
    let name = actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).unwrap();
    let worktree = h.config.worktrees_dir(&h.paths).join(&name);
    h.assert_untouched("new", Some(&worktree));

    let home_before = tree(&h.home);
    for args in [
        vec!["doctor"],
        vec!["doctor", "--json"],
        // Twice, because a cache written on the first run would only show
        // up as a difference on the second.
        vec!["doctor"],
    ] {
        let out = pando(&h, &args);
        // 0 or 1: this fixture pins a runtime, and whether this machine
        // resolves it is a fact about the machine. Anything else would be
        // a crash.
        assert!(
            matches!(out.status.code(), Some(0) | Some(1)),
            "{args:?} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        h.assert_untouched(&args.join(" "), Some(&worktree));
        assert_eq!(
            tree(&h.home),
            home_before,
            "after {args:?}: pando's own home changed — doctor reports, it does not write"
        );
    }
}
