//! Generic `[[hooks]]`: the lifecycle points, the fingerprint, and the
//! fallback.
//!
//! Hooks are observed through a sink file each one appends to, because the
//! thing worth asserting is *which hooks ran and in what order* — a log
//! file per hook would say each one ran but never say when.

mod common;

use std::path::{Path, PathBuf};

use common::{Kind, build, docker, paths_for};
use pando::config::{self, Config};
use pando::paths::PandoPaths;
use pando::{actions, state};
use tempfile::TempDir;

struct Hx {
    _dir: TempDir,
    root: PathBuf,
    sink: PathBuf,
    paths: PandoPaths,
    config: Config,
}

impl Drop for Hx {
    fn drop(&mut self) {
        let _ = actions::stop_all(&self.paths, &|_| {});
    }
}

impl Hx {
    fn ran(&self) -> Vec<String> {
        std::fs::read_to_string(&self.sink)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.sink);
    }

    fn log(&self, name: &str, source: &str) -> String {
        std::fs::read_to_string(self.paths.log_file(name, source)).unwrap_or_default()
    }

    fn rewrite_config(&mut self, text: &str) {
        std::fs::write(self.paths.config_file(), text).unwrap();
        self.config = config::load(&self.paths).unwrap().config;
    }
}

fn hx(kind: Kind, config_for: impl Fn(&Path) -> String) -> Hx {
    let dir = TempDir::new().unwrap();
    let root = build(kind, dir.path()).root;
    let home = dir.path().join("pando-home");
    docker::install(&home);
    let sink = dir.path().join("hooks-ran.txt");
    let paths = paths_for(&home, &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(paths.config_file(), config_for(&sink)).unwrap();
    let config = config::load(&paths).unwrap().config;
    Hx {
        _dir: dir,
        root,
        sink,
        paths,
        config,
    }
}

/// One hook at each of the four points, plus the built-in install step.
fn every_point(sink: &Path) -> String {
    let sink = sink.display();
    format!(
        "[project]\ninstall = \"echo installed >> '{sink}'\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"on-create\"\nafter = \"create\"\n\
         cmd = \"echo create >> '{sink}'\"\n\n\
         [[hooks]]\nname = \"on-install\"\nafter = \"install\"\n\
         cmd = \"echo install >> '{sink}'\"\n\n\
         [[hooks]]\nname = \"on-services\"\nafter = \"services\"\n\
         cmd = \"echo services >> '{sink}'\"\n\n\
         [[hooks]]\nname = \"on-dev\"\nafter = \"dev\"\n\
         cmd = \"echo dev >> '{sink}'\"\n"
    )
}

fn new_worktree(f: &Hx, branch: &str) -> String {
    actions::new(&f.paths, &f.config, branch, None, &|_| {}).unwrap()
}

fn start(f: &Hx, name: &str) -> actions::StartReport {
    actions::start(
        &f.paths,
        &f.config,
        name,
        None,
        actions::Mode::Remembered,
        &|_| {},
    )
    .unwrap()
}

#[test]
fn every_lifecycle_point_runs_its_hooks_in_order() {
    let f = hx(Kind::Plain, every_point);
    let name = new_worktree(&f, "feat/one");
    assert_eq!(
        f.ran(),
        vec!["installed", "create"],
        "`new` runs the create point, and the install step is the first hook of it"
    );

    f.clear();
    start(&f, &name);
    assert_eq!(
        f.ran(),
        vec!["installed", "create", "install", "services", "dev"],
        "and a start walks the whole lifecycle in order"
    );
}

#[test]
fn a_hooks_output_lands_in_its_own_log() {
    let f = hx(Kind::Plain, every_point);
    let name = new_worktree(&f, "feat/one");
    start(&f, &name);
    for source in [
        "install",
        "on-create",
        "on-install",
        "on-services",
        "on-dev",
    ] {
        assert!(
            f.paths.log_file(&name, source).is_file(),
            "{source} has no log, so the viewer has no tab for it"
        );
    }
}

/// A hook keyed on a file, so it can be skipped.
fn gated(sink: &Path, cmd: &str) -> String {
    let sink = sink.display();
    format!(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"dev\"\n\
         fingerprint = [\"seed.sql\"]\n\
         cmd = \"{cmd} >> '{sink}'\"\n"
    )
}

#[test]
fn a_fingerprinted_hook_runs_again_only_when_its_inputs_change() {
    let mut f = hx(Kind::Plain, |sink| gated(sink, "echo ran"));
    std::fs::write(f.root.join("seed.sql"), "one\n").unwrap();
    common::git(&f.root, &["add", "."]);
    common::git(&f.root, &["commit", "--quiet", "-m", "seed"]);
    let name = new_worktree(&f, "feat/one");
    let worktree = f.paths.worktree_path(&name);

    start(&f, &name);
    assert_eq!(f.ran(), vec!["ran"], "the first start runs it");

    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    start(&f, &name);
    assert_eq!(
        f.ran(),
        vec!["ran"],
        "nothing changed, so it does not run again"
    );

    std::fs::write(worktree.join("seed.sql"), "two\n").unwrap();
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    start(&f, &name);
    assert_eq!(
        f.ran(),
        vec!["ran", "ran"],
        "a changed input is a reason to run"
    );

    // And so is a changed command — the Phase 2 gap this closes.
    let sink = f.sink.clone();
    f.rewrite_config(&gated(&sink, "echo ran-again"));
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    start(&f, &name);
    assert_eq!(
        f.ran(),
        vec!["ran", "ran", "ran-again"],
        "editing what a hook runs has to run it"
    );

    // Recorded per hook name, so the viewer and `status` can say when.
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(store.worktrees[&name].hooks.contains_key("migrate"));
}

#[test]
fn a_hook_with_nothing_to_watch_runs_on_every_start() {
    let f = hx(Kind::Plain, |sink| {
        let sink = sink.display();
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
             [[hooks]]\nname = \"always\"\nafter = \"dev\"\n\
             cmd = \"echo ran >> '{sink}'\"\n"
        )
    });
    let name = new_worktree(&f, "feat/one");
    start(&f, &name);
    actions::stop(&f.paths, &name, None, &|_| {}).unwrap();
    start(&f, &name);
    assert_eq!(f.ran(), vec!["ran", "ran"]);
}

#[test]
fn a_fallback_runs_when_the_command_fails_and_the_hook_still_succeeds() {
    let f = hx(Kind::Plain, |sink| {
        let sink = sink.display();
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
             [[hooks]]\nname = \"migrate\"\nafter = \"dev\"\n\
             cmd = \"echo no-migrations >&2 && exit 1\"\n\
             fallback = \"echo pushed >> '{sink}'\"\n"
        )
    });
    let name = new_worktree(&f, "feat/one");
    start(&f, &name);
    assert_eq!(f.ran(), vec!["pushed"]);
    let log = f.log(&name, "migrate");
    assert!(log.contains("no-migrations"), "{log}");
    assert!(log.contains("trying the fallback"), "{log}");
}

#[test]
fn when_both_the_command_and_its_fallback_fail_the_fallbacks_reason_is_reported() {
    let f = hx(Kind::Plain, |_| {
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"install\"\n\
         cmd = \"echo FIRST >&2 && exit 1\"\n\
         fallback = \"echo SECOND >&2 && exit 2\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("the migrate hook failed"), "{err}");
    assert!(err.contains("SECOND"), "{err}");
    assert!(
        err.contains("FIRST"),
        "it still names what sent it there: {err}"
    );
    // And nothing started behind a schema that is not there.
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(store.worktrees[&name].processes.is_empty());
}

// The whole point of running hooks after the services: a migration has to
// reach *this* worktree's database, not the shared one.
#[test]
fn a_services_hook_is_told_where_this_worktrees_services_are() {
    let f = hx(Kind::NextPnpmCompose, |_| {
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\nenv = { DATABASE_URL = \"postgres\" }\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
         cmd = \"echo url=$DATABASE_URL\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let report = actions::start(
        &f.paths,
        &f.config,
        &name,
        None,
        actions::Mode::Isolated,
        &|_| {},
    )
    .unwrap();
    let log = f.log(&name, "migrate");
    assert!(
        log.contains(&format!(
            "url=postgres://acme:acme@localhost:{}/acme",
            report.ports["postgres"]
        )),
        "{log}"
    );
    let _ = actions::rm(&f.paths, &name, true, true, &|_| {});
}

// A hook's `cwd` is held to a process's rules: outside the worktree is a
// write into a repository, which Invariant 1 forbids.
#[test]
fn a_hook_cwd_that_leaves_the_worktree_is_refused() {
    let f = hx(Kind::Plain, |_| {
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"odd\"\nafter = \"install\"\ncwd = \"nope\"\n\
         cmd = \"true\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("\"odd\""), "{err}");
    assert!(err.contains("does not exist in this worktree"), "{err}");
}

// ---- probes ---------------------------------------------------------------

fn with_probe(cmd: &str, match_: &str) -> String {
    format!(
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[probes]]\nname = \"native-abi\"\ncmd = \"{cmd}\"\n\
         match = \"{match_}\"\n\
         hint = \"Rebuild native modules under the dev runtime.\"\n"
    )
}

#[test]
fn a_probe_that_recognises_the_failure_aborts_the_start_with_its_hint() {
    let f = hx(Kind::Plain, |_| {
        with_probe(
            "echo NODE_MODULE_VERSION 127 >&2 && exit 1",
            "NODE_MODULE_VERSION",
        )
    });
    let name = new_worktree(&f, "feat/one");
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("native-abi"), "{err}");
    assert!(err.contains("NODE_MODULE_VERSION"), "{err}");
    assert!(
        err.contains("Rebuild native modules under the dev runtime."),
        "the hint is the point of a probe: {err}"
    );
    // And nothing was started behind it.
    let store = state::load(&f.paths.state_file()).unwrap();
    assert!(store.worktrees[&name].processes.is_empty());
}

#[test]
fn a_probe_that_fails_some_other_way_is_ignored() {
    let f = hx(Kind::Plain, |_| {
        with_probe(
            "echo something-else-entirely >&2 && exit 3",
            "NODE_MODULE_VERSION",
        )
    });
    let name = new_worktree(&f, "feat/one");
    let said = std::sync::Mutex::new(Vec::<String>::new());
    {
        let notice = |m: &str| said.lock().unwrap().push(m.to_string());
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &notice,
        )
        .unwrap();
    }
    let said = said.into_inner().unwrap();
    assert!(
        said.iter().any(|m| m.contains("does not recognise")),
        "ignored, but never silently: {said:?}"
    );
    let store = state::load(&f.paths.state_file()).unwrap();
    assert_eq!(store.worktrees[&name].processes.len(), 1);
}

#[test]
fn a_probe_that_passes_says_nothing_and_starts_everything() {
    let f = hx(Kind::Plain, |_| with_probe("true", "NODE_MODULE_VERSION"));
    let name = new_worktree(&f, "feat/one");
    start(&f, &name);
    let store = state::load(&f.paths.state_file()).unwrap();
    assert_eq!(store.worktrees[&name].processes.len(), 1);
}

#[test]
fn a_probe_runs_in_the_worktree_with_the_process_environment() {
    let f = hx(Kind::Plain, |sink| {
        let sink = sink.display();
        format!(
            "[project]\ninstall = \"true\"\n\n\
             [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
             [[probes]]\nname = \"where\"\n\
             cmd = \"pwd >> '{sink}' && echo name=$PANDO_NAME >> '{sink}' && exit 1\"\n\
             match = \"never-matches\"\nhint = \"unused\"\n"
        )
    });
    let name = new_worktree(&f, "feat/one");
    start(&f, &name);
    let ran = f.ran();
    assert!(
        ran[0].ends_with(&name),
        "a probe runs inside the worktree it is about: {ran:?}"
    );
    assert_eq!(ran[1], format!("name={name}"));
}

// ---- what a hook leaves behind, and where it came from --------------------

fn worktree_of(f: &Hx, name: &str) -> PathBuf {
    f.config.worktrees_dir(&f.paths).join(name)
}

/// Runs a start, collecting every notice it printed.
fn start_saying(f: &Hx, name: &str) -> Vec<String> {
    let said = std::sync::Mutex::new(Vec::<String>::new());
    {
        let notice = |m: &str| said.lock().unwrap().push(m.to_string());
        actions::start(
            &f.paths,
            &f.config,
            name,
            None,
            actions::Mode::Remembered,
            &notice,
        )
        .unwrap();
    }
    said.into_inner().unwrap()
}

// `docs/02-principles.md`: a hook that writes a new file into the worktree
// is misconfigured. The comparison only ever saw the files the hook was
// keyed on, so anything it created outside them was invisible.
#[test]
fn a_hook_that_leaves_an_untracked_file_in_the_worktree_names_it() {
    let f = hx(Kind::Plain, |_| {
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"writer\"\nafter = \"install\"\n\
         fingerprint = [\"README.md\"]\n\
         cmd = \"touch generated-by-a-hook.txt\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let worktree = worktree_of(&f, &name);
    assert_eq!(common::status_porcelain(&worktree), "");

    let said = start_saying(&f, &name);
    // A warning, not merely the "writer: touch …" line every hook prints.
    assert!(
        said.iter().any(|m| m.starts_with("warning:")
            && m.contains("writer")
            && m.contains("generated-by-a-hook.txt")),
        "the hook and the path it left: {said:?}"
    );
    assert!(
        common::status_porcelain(&worktree).contains("generated-by-a-hook.txt"),
        "and it really is there"
    );
    let _ = actions::stop(&f.paths, &name, None, &|_| {});
}

// A hook pando invented — the detected `migrate` this phase adds is the one
// that matters — failed with a message naming the hook but never saying
// pando wrote it, nor where the one edit lives.
#[test]
fn a_failing_hook_names_the_entry_it_came_from_and_says_it_can_be_deleted() {
    let f = hx(Kind::Plain, |_| {
        "[project]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"install\"\n\
         cmd = \"echo boom >&2 && exit 7\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let err = format!(
        "{:#}",
        actions::start(
            &f.paths,
            &f.config,
            &name,
            None,
            actions::Mode::Remembered,
            &|_| {}
        )
        .unwrap_err()
    );
    assert!(err.contains("the migrate hook failed"), "{err}");
    assert!(err.contains("[[hooks]]"), "{err}");
    assert!(
        err.contains(&f.paths.config_file().display().to_string()),
        "it names the file the one edit lives in: {err}"
    );
    assert!(err.contains("delete it"), "{err}");
}

// `fingerprint = ["prisma/migrations"]` instead of `["prisma/migrations/**"]`
// is the natural typo, and it costs a full migration on every start. So
// does a glob that matches nothing. Every guess is visible; so is this.
#[test]
fn a_fingerprint_that_matches_nothing_is_said_out_loud() {
    let f = hx(Kind::NextPnpmCompose, |_| {
        "[project]\nprovision = [\".env\"]\ninstall = \"true\"\n\n\
         [dev]\ncmd = \"sleep 30\"\nports = []\n\n\
         [[hooks]]\nname = \"migrate\"\nafter = \"install\"\n\
         fingerprint = [\"prisma/migrations\"]\ncmd = \"true\"\n\n\
         [[hooks]]\nname = \"absent\"\nafter = \"install\"\n\
         fingerprint = [\"nothing-*.zzz\"]\ncmd = \"true\"\n\n\
         [[hooks]]\nname = \"keyed\"\nafter = \"install\"\n\
         fingerprint = [\"package.json\"]\ncmd = \"true\"\n"
            .to_string()
    });
    let name = new_worktree(&f, "feat/one");
    let said = start_saying(&f, &name);

    let about = |hook: &str| -> Vec<&String> {
        let subject = format!("the {hook} hook");
        said.iter()
            .filter(|m| m.starts_with("warning:") && m.contains(&subject))
            .collect()
    };
    let migrate = about("migrate");
    assert_eq!(migrate.len(), 1, "one notice, not one per glob: {said:?}");
    assert!(migrate[0].contains("prisma/migrations"), "{migrate:?}");
    assert!(
        migrate[0].contains("prisma/migrations/**"),
        "a literal that is a directory gets the fix suggested: {migrate:?}"
    );

    let absent = about("absent");
    assert_eq!(absent.len(), 1, "{said:?}");
    assert!(absent[0].contains("nothing-*.zzz"), "{absent:?}");

    assert!(
        about("keyed").is_empty(),
        "a fingerprint that matched says nothing: {said:?}"
    );
    let _ = actions::stop(&f.paths, &name, None, &|_| {});
}

// A hook and a service both write `logs/<worktree>/<name>.log`: the pump
// truncates it, the hook appends to it, and `logs --source db` shows a
// mixture. A service name already may not collide with a process's role.
#[test]
fn a_hook_named_after_a_service_is_refused_at_load() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::NextPnpmCompose, dir.path()).root;
    let paths = paths_for(&dir.path().join("pando-home"), &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    std::fs::write(
        paths.config_file(),
        "[dev]\ncmd = \"true\"\n\n\
         [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
         include = [\"postgres\"]\n\n\
         [[hooks]]\nname = \"postgres\"\nafter = \"services\"\ncmd = \"true\"\n",
    )
    .unwrap();
    let err = format!("{:#}", config::load(&paths).unwrap_err());
    assert!(err.contains("\"postgres\""), "{err}");
    assert!(err.contains("hook"), "{err}");
    assert!(
        err.contains("log"),
        "it says what they would collide over: {err}"
    );
}

#[test]
fn a_hook_name_that_would_escape_the_log_directory_is_refused_at_load() {
    let dir = TempDir::new().unwrap();
    let root = build(Kind::Plain, dir.path()).root;
    let paths = paths_for(&dir.path().join("pando-home"), &root);
    std::fs::create_dir_all(paths.project_dir()).unwrap();
    for name in ["../escape", "install", "tunnel"] {
        std::fs::write(
            paths.config_file(),
            format!(
                "[dev]\ncmd = \"true\"\n\n[[hooks]]\nname = \"{name}\"\n\
                 after = \"dev\"\ncmd = \"true\"\n"
            ),
        )
        .unwrap();
        let err = format!("{:#}", config::load(&paths).unwrap_err());
        assert!(err.contains(name), "{name}: {err}");
    }
}
