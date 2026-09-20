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

use common::{Kind, build, git, git_raw, paths_for, status_porcelain};
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
        let _ = actions::stop_all(&self.paths);
    }
}

fn harness() -> Harness {
    harness_with("[project]\nprovision = [\".env\", \".env.local\"]\n")
}

fn harness_with(config_toml: &str) -> Harness {
    let dir = TempDir::new().unwrap();
    // Canonical throughout: on macOS the temp dir is /var/... but git (and
    // every path pando canonicalises) says /private/var/..., and the
    // comparisons below are all path equality.
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    let root = build(Kind::Plain, &parent).root;
    let home = parent.join("pando-home");
    let paths = paths_for(&home, &root);

    // Provisioning is the only thing that writes inside a worktree at all,
    // so the invariant is tested with it turned on.
    paths.ensure_home().unwrap();
    std::fs::write(paths.config_file(), config_toml).unwrap();
    let loaded = config::load(&paths).unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    let baseline = tree(&root);
    assert!(
        baseline.contains_key(".env"),
        "the fixture must have an ignored .env to provision"
    );
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

    actions::rm(&h.paths, &name, false, false).unwrap();
    h.assert_untouched("rm", None);
    assert!(!worktree.exists());
}

// The refusal path matters just as much: a `new` that cannot proceed must
// not leave a half-made directory or a stray branch behind.
#[test]
fn a_refused_command_leaves_the_repository_untouched() {
    let mut h = harness();
    h.config.project.provision = vec!["README.md".into()];
    assert!(
        actions::new(&h.paths, &h.config, "feat/one", None, &|_| {}).is_err(),
        "README.md is tracked, so provisioning it must be refused"
    );
    h.assert_untouched("a refused new", None);

    assert!(actions::rm(&h.paths, "nope", true, true).is_err());
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
        actions::rm(&h.paths, "adopted-elsewhere", false, false).is_err(),
        "an adopted worktree needs --yes"
    );
    actions::rm(&h.paths, "adopted-elsewhere", true, false).unwrap();
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

    h.config.project.provision = vec!["local.pando".into()];
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
    h.config.project.provision = vec!["secrets.local".into()];

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

    let outcome = actions::start(&h.paths, &h.config, &name, None, &|_| {}).unwrap();
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
        actions::stop(&h.paths, &name, None).unwrap(),
        actions::StopOutcome::Stopped(vec!["dev".to_string()])
    );
    h.assert_untouched("stop", Some(&worktree));

    let restarted = actions::restart(&h.paths, &h.config, &name, None, &|_| {}).unwrap();
    assert_eq!(
        restarted.ports["web"], port,
        "a restart keeps the port, so the URL keeps working"
    );
    h.assert_untouched("restart", Some(&worktree));

    actions::stop(&h.paths, &name, None).unwrap();
    h.assert_untouched("stop again", Some(&worktree));

    actions::rm(&h.paths, &name, false, false).unwrap();
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
    let outcome = actions::start(&h.paths, &h.config, &name, None, &|_| {}).unwrap();
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

    actions::stop(&h.paths, &name, None).unwrap();
    h.assert_untouched("the whole lifecycle", None);
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
