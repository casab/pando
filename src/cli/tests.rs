use super::answers::answer_from;
use super::answers::slot_named;
use super::answers::slot_names;
use super::logs::silence_notes;
use super::logs::{leading_timestamp, level_word};
use super::ls::ORDER;
use super::prompt::asker;
use super::prompt::prompt_with;
use super::status::{human_duration, phase_word};
use super::*;
use crate::actions;
use crate::actions::worktree_url;
use crate::cache;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::project::ProjectRef;
use crate::state::{Phase, ProcessRecord, WorktreeRecord};
use crate::testutil::git;
use crate::worktree::PrState;
use anyhow::Result;
use chrono::Utc;
use clap::CommandFactory;
use std::collections::BTreeMap;
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

/// The notice channel, for a test that is not about it.
fn quiet(_: &str) {}

/// stdout and the notices, separately — which is the whole point of
/// there being two channels.
fn capture_both(
    f: impl FnOnce(&mut Vec<u8>, &dyn Fn(&str)) -> Result<()>,
) -> (String, Vec<String>) {
    let notes = std::cell::RefCell::new(Vec::new());
    let mut buf = Vec::new();
    f(&mut buf, &|line: &str| {
        notes.borrow_mut().push(line.to_string())
    })
    .unwrap();
    (String::from_utf8(buf).unwrap(), notes.into_inner())
}

#[test]
fn the_cli_definition_is_valid() {
    Cli::command().debug_assert();
}

// A person opening `--help` is looking for how to use it, so the top
// level and every verb with a non-obvious shape show examples, and every
// example is a command the binary really takes.
#[test]
fn help_carries_examples_that_parse() {
    let mut cli = Cli::command();
    let top = cli.render_help().to_string();
    assert!(top.contains("Examples:"), "{top}");
    for verb in [
        "new",
        "ls",
        "start",
        "stop",
        "status",
        "open",
        "logs",
        "completions",
    ] {
        let sub = cli
            .find_subcommand_mut(verb)
            .unwrap_or_else(|| panic!("no {verb}"));
        let help = sub.render_help().to_string();
        assert!(help.contains("Example"), "{verb} has no example:\n{help}");
    }
    for line in top.lines().chain(
        cli.get_subcommands()
            .flat_map(|s| s.get_after_help().map(|h| h.to_string()))
            .collect::<Vec<_>>()
            .iter()
            .flat_map(|h| h.lines()),
    ) {
        let Some(rest) = line.trim_start().strip_prefix("pando ") else {
            continue;
        };
        // The command is everything before the two-space gap that starts
        // its description.
        let command = rest.split("  ").next().unwrap().trim();
        let mut argv = vec!["pando"];
        argv.extend(command.split_whitespace().filter(|w| !w.starts_with('>')));
        let argv: Vec<&str> = argv
            .into_iter()
            .take_while(|w| !w.starts_with('~'))
            .collect();
        Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?} does not parse: {e}"));
    }
}

#[test]
fn a_typo_is_matched_to_the_names_it_was_probably_meant_to_be() {
    use super::names::{edit_distance, suggest};
    assert_eq!(edit_distance("feat+one", "feat+one"), 0);
    assert_eq!(edit_distance("feat+on", "feat+one"), 1);
    assert_eq!(edit_distance("faet+one", "feat+one"), 2);

    let fx = fixture();
    for branch in ["feat/one", "feat/two", "fix/login"] {
        actions::new(&fx.paths, &fx.config, branch, None, &|_| {}).unwrap();
    }
    let worktrees = crate::worktree::discover(&fx.paths.project).unwrap();
    // A prefix of the branch or the directory name.
    let mut prefix = suggest("feat", &worktrees);
    prefix.sort();
    assert_eq!(prefix, ["feat/one", "feat/two"]);
    // A typo.
    assert_eq!(suggest("fix/logni", &worktrees), ["fix/login"]);
    // Something in the middle.
    assert_eq!(suggest("login", &worktrees), ["fix/login"]);
    // Nothing close.
    assert!(suggest("zzzzzz", &worktrees).is_empty());
}

// A branch literally called `a+b`, beside the directory `a+b` of the
// branch `a/b`: the typed string names two worktrees, and picking the
// directory silently is how `rm a+b` removes the one that was not meant.
#[test]
fn a_name_that_is_one_worktrees_directory_and_anothers_branch_is_refused() {
    let fx = fixture();
    let first = actions::new(&fx.paths, &fx.config, "a/b", None, &|_| {}).unwrap();
    assert_eq!(first, "a+b");
    let elsewhere = fx.root.parent().unwrap().join("other");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "a+b",
            elsewhere.to_str().unwrap(),
        ],
    );

    let err = super::names::resolve(&fx.paths, "a+b").unwrap_err();
    assert!(err.downcast_ref::<UsageError>().is_some(), "{err:#}");
    let msg = format!("{err:#}");
    assert!(msg.contains("names two worktrees"), "{msg}");
    assert!(msg.contains("`a/b`") && msg.contains("`other`"), "{msg}");
    // Each has an unambiguous name, and both resolve.
    assert_eq!(super::names::resolve(&fx.paths, "a/b").unwrap(), "a+b");
    assert_eq!(super::names::resolve(&fx.paths, "other").unwrap(), "other");
}

// The heuristic is on `--help`, where a script author will look for it.
#[test]
fn start_help_says_how_a_portless_process_is_judged_ready() {
    use clap::CommandFactory;
    let mut cli = super::Cli::command();
    let start = cli.find_subcommand_mut("start").unwrap();
    let help = start.render_long_help().to_string();
    assert!(help.contains("no port"), "{help}");
    assert!(help.contains("5s") && help.contains("10s"), "{help}");
}

// A database that crashed during `start --wait` was forgotten by the
// wait's refresh and saved as gone, and the wait never said so: the
// failure it caused came with no cause, and no later command had one.
#[test]
fn a_wait_tells_what_its_refresh_forgot() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut exited = std::process::Command::new("true").spawn().unwrap();
    exited.wait().unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    let mut dev = listening(std::process::id() as i32, &[]);
    dev.started_at = Utc::now() - chrono::Duration::seconds(60);
    record.processes.insert("dev".to_string(), dev);
    record.services.push(crate::state::ServiceRecord {
        name: "mariadb".into(),
        kind: crate::state::ServiceKind::Native,
        port: Some(17_004),
        pid: Some(exited.id()),
        pgid: None,
        compose_project: None,
    });
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let said = std::cell::RefCell::new(Vec::new());
    let named = super::names::target_named(&fx.paths, Some("feat/one"), "start").unwrap();
    super::wait::wait_ready(&fx.paths, &named, None, &|line| {
        said.borrow_mut().push(line.to_string())
    })
    .unwrap();
    let said = said.into_inner();
    assert!(
        said.iter().any(|l| l.contains("its mariadb exited")),
        "{said:?}"
    );
}

// A refresh that read the state and could not save what it changed was
// taken for one that could not read it: `open` of a worktree never
// started answered with the save's error, not "not running", and a wait
// whose worktree stopped did not say it stopped.
#[test]
fn a_refresh_that_only_failed_to_save_still_answers_about_the_worktree() {
    let fx = fixture();
    let one = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let two = actions::new(&fx.paths, &fx.config, "feat/two", None, &|_| {}).unwrap();
    let mut exited = std::process::Command::new("true").spawn().unwrap();
    exited.wait().unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    // Forgotten by every refresh, so every refresh has something to save.
    store
        .worktrees
        .get_mut(&one)
        .unwrap()
        .services
        .push(crate::state::ServiceRecord {
            name: "mariadb".into(),
            kind: crate::state::ServiceKind::Native,
            port: Some(17_004),
            pid: Some(exited.id()),
            pgid: None,
            compose_project: None,
        });
    store.worktrees.remove(&two);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    // A save writes here first, and cannot.
    std::fs::create_dir(fx.paths.state_file().with_extension("json.tmp")).unwrap();
    let refreshed = actions::refresh(&fx.paths);
    assert!(refreshed.warning.is_some() && !refreshed.unreadable);

    let named = super::names::target_named(&fx.paths, Some("feat/two"), "open").unwrap();
    let err =
        super::open::url_to_open(&fx.paths, &with_dev(&fx.config), &named, false).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "feat/two is not running — `pando start feat/two` starts it"
    );
    let err = super::wait::wait_ready(&fx.paths, &named, None, &quiet).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "feat/two has nothing running — it stopped while pando waited"
    );
}

#[test]
fn a_name_resolves_by_directory_by_branch_or_by_the_directory_it_is_run_in() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    assert_eq!(super::names::resolve(&fx.paths, "feat+one").unwrap(), name);
    assert_eq!(super::names::resolve(&fx.paths, "feat/one").unwrap(), name);
    let err = super::names::resolve(&fx.paths, "feat/on").unwrap_err();
    assert!(
        format!("{err:#}").contains("did you mean feat/one?"),
        "{err:#}"
    );

    let inside = fx.paths.worktrees_dir().join(&name).join("sub");
    std::fs::create_dir_all(&inside).unwrap();
    assert_eq!(
        super::names::containing(&fx.paths, &inside).unwrap(),
        Some(name.clone())
    );
    assert_eq!(super::names::containing(&fx.paths, &fx.root).unwrap(), None);
}

// The main checkout is a place `path` can go, and nothing else pando
// does. It used to resolve like a worktree for every verb: `status
// acme-shop` printed "no worktree named" and exited 0, `stop` said it
// "was not running", and `open` suggested a `start` that refuses it.
#[test]
fn the_main_checkout_resolves_for_path_only_and_an_empty_name_is_a_usage_error() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    for typed in ["acme-shop", "main"] {
        let err = super::names::resolve(&fx.paths, typed).unwrap_err();
        assert!(
            format!("{err:#}").contains("is the main checkout"),
            "{typed}: {err:#}"
        );
        assert_eq!(
            super::names::path(&fx.paths, typed).unwrap().file_name(),
            Some(std::ffi::OsStr::new("acme-shop")),
            "{typed}"
        );
    }
    assert_eq!(super::names::resolve(&fx.paths, "feat/one").unwrap(), name);

    let err = super::names::resolve(&fx.paths, "").unwrap_err();
    assert!(err.downcast_ref::<UsageError>().is_some(), "{err:#}");
    assert!(!format!("{err:#}").contains("did you mean"), "{err:#}");
}

// A pty nobody sized reports zero columns, and `ls` fitted its table to
// that: every column but NAME and STATUS shed and the name cut to its
// floor. COLUMNS, when it is a number, is what a person asked for.
#[test]
fn the_listing_width_survives_a_zero_sized_terminal_and_reads_columns() {
    use super::ls::width_from;
    assert_eq!(width_from(true, None, Some(0)), 80);
    assert_eq!(width_from(true, None, None), 80);
    assert_eq!(width_from(true, None, Some(120)), 120);
    assert_eq!(width_from(true, Some("60"), Some(120)), 60);
    for garbage in ["", "0", "wide", "-5"] {
        assert_eq!(
            width_from(true, Some(garbage), Some(120)),
            120,
            "{garbage:?}"
        );
    }
    assert_eq!(
        width_from(false, Some("60"), Some(120)),
        usize::MAX,
        "a pipe"
    );
}

#[test]
fn logs_default_to_dev_else_to_the_only_process() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    // No log at all: `dev`, so the error names it.
    assert_eq!(
        super::logs::default_sources(&fx.paths, &name, "dev"),
        super::logs::Sources::One("dev".to_string())
    );
    // One process, not called dev.
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.processes.remove("api");
    record.roles.remove("api");
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    write_log(&fx, &name, "web", "web line\n");
    assert_eq!(
        super::logs::default_sources(&fx.paths, &name, "dev"),
        super::logs::Sources::One("web".to_string())
    );
    // A dev log wins whenever there is one.
    write_log(&fx, &name, "dev", "dev line\n");
    assert_eq!(
        super::logs::default_sources(&fx.paths, &name, "dev"),
        super::logs::Sources::One("dev".to_string())
    );
}

// A worktree with an api and a web process and no `dev` used to answer a
// plain `pando logs` with an error. It gets both, compose-style.
#[test]
fn logs_with_several_processes_and_no_dev_merge_them() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });
    write_log(&fx, &name, "api", "a1\na2\n");
    write_log(&fx, &name, "web", "w1\n");
    // A hook's log is not a process's, and stays out of the merge.
    write_log(&fx, &name, "install", "installed\n");
    let sources = match super::logs::default_sources(&fx.paths, &name, "dev") {
        super::logs::Sources::Merged(sources) => sources,
        other => panic!("{other:?}"),
    };
    assert_eq!(sources, vec!["api".to_string(), "web".to_string()]);

    // No timestamps to order by: one source after the other.
    let (text, notes) = capture_both(|b, n| {
        super::logs::logs_merged(&fx.paths, &name, &sources, 50, false, false, b, n)
    });
    assert_eq!(text, "api | a1\napi | a2\nweb | w1\n");
    assert!(notes.iter().any(|n| n.contains("-s api")), "{notes:?}");

    // `-n` counts per source, as compose's `--tail` does.
    let text = capture(|b| {
        super::logs::logs_merged(&fx.paths, &name, &sources, 1, false, false, b, &quiet)
    });
    assert_eq!(text, "api | a2\nweb | w1\n");

    // Timestamps on every line: one timeline.
    write_log(
        &fx,
        &name,
        "api",
        "2026-09-20T10:00:00Z api first\n2026-09-20T10:00:02Z api third\n",
    );
    write_log(&fx, &name, "web", "2026-09-20T10:00:01Z web second\n");
    let text = capture(|b| {
        super::logs::logs_merged(&fx.paths, &name, &sources, 50, false, false, b, &quiet)
    });
    assert_eq!(
        text,
        "api | 2026-09-20T10:00:00Z api first\n\
         web | 2026-09-20T10:00:01Z web second\n\
         api | 2026-09-20T10:00:02Z api third\n"
    );

    // And as JSON, each line says where it came from.
    let text = capture(|b| {
        super::logs::logs_merged(&fx.paths, &name, &sources, 50, false, true, b, &quiet)
    });
    let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["source"], "api");
    assert_eq!(first["version"], JSON_VERSION);
    // A read of one named log carries no `source`, as before.
    let one = capture(|b| logs(&fx.paths, &name, "api", 1, false, true, b, &quiet));
    let one: serde_json::Value = serde_json::from_str(one.trim()).unwrap();
    assert!(one.get("source").is_none(), "{one}");
}

// The merged stream's `source` key is published, so the contract says so.
#[test]
fn agent_json_documents_the_merged_logs_source_key() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md"),
    )
    .unwrap();
    let section = doc
        .split("## `pando logs <name> --json`")
        .nth(1)
        .expect("the logs section")
        .split("\n## ")
        .next()
        .unwrap();
    assert!(section.contains("\"source\": \"api\""), "{section}");
    assert!(section.contains("merged"), "{section}");
}

// Every key `pando signals` publishes is one an agent reads, so the
// contract names each of them.
#[test]
fn agent_json_documents_every_signals_key() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md"),
    )
    .unwrap();
    let section = doc
        .split("## `pando signals`")
        .nth(1)
        .expect("the signals section")
        .split("\n## ")
        .next()
        .unwrap();
    let published = serde_json::to_value(crate::detect::Signals::default()).unwrap();
    for key in published.as_object().unwrap().keys() {
        assert!(
            section.contains(&format!("\"{key}\"")),
            "agent/json.md never documents signals.{key}"
        );
    }
}

#[test]
fn start_waits_on_a_terminal_unless_told_not_to() {
    assert!(super::waits_on(false, false, true), "a terminal waits");
    assert!(!super::waits_on(false, true, true), "--no-wait");
    assert!(!super::waits_on(false, false, false), "a script does not");
    assert!(super::waits_on(true, false, false), "--wait, from a script");
}

#[test]
fn open_wants_something_up_or_a_share() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let config = with_dev(&fx.config);
    let named = super::names::target_named(&fx.paths, Some("feat/one"), "open").unwrap();
    let open =
        |config: &Config, public| super::open::url_to_open(&fx.paths, config, &named, public);
    let err = open(&config, false).unwrap_err();
    // Named as a person knows it, and the command echoes what was typed.
    assert_eq!(
        format!("{err:#}"),
        "feat/one is not running — `pando start feat/one` starts it"
    );
    let err = open(&config, true).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "feat/one is not shared — `pando share feat/one` publishes it"
    );

    with_share(&fx, &name, None);
    assert_eq!(open(&config, false).unwrap(), "http://localhost:17342");
    assert_eq!(
        open(&config, true).unwrap(),
        "https://fake-host.trycloudflare.com"
    );
}

fn with_dev(config: &Config) -> Config {
    let mut config = config.clone();
    config.processes.insert(
        "dev".into(),
        crate::config::ProcessConfig {
            cmd: "true".into(),
            ..Default::default()
        },
    );
    config
}

// `pando open feat/r` on a project with nothing to run suggested
// "`pando start feat+r` starts it", which cannot work.
#[test]
fn open_on_a_project_with_nothing_to_run_says_so_and_how_to_add_one() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/r", None, &|_| {}).unwrap();
    let named = super::names::target_named(&fx.paths, Some("feat/r"), "open").unwrap();
    let err = super::open::url_to_open(&fx.paths, &fx.config, &named, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.starts_with("feat/r is not running, and this project has nothing to run"));
    assert!(!msg.contains("pando start"), "{msg}");
    assert!(
        msg.contains(&fx.paths.config_file().display().to_string()),
        "{msg}"
    );
    assert!(msg.contains("[dev]\ncmd = \""), "{msg}");
}

// `init --dry-run` said "nothing left to answer — … already says it all"
// above "schema command (unanswered) …".
#[test]
fn init_never_says_nothing_is_left_above_an_unanswered_slot() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("pando.toml");
    std::fs::write(&file, "").unwrap();
    let slot = |slot, label, value: &str| actions::SlotSummary {
        slot,
        label,
        value: Some(value.to_string()),
        answered_now: false,
    };
    let mut report = actions::InitReport {
        config_file: file.clone(),
        user_file: None,
        slots: vec![
            slot(crate::detect::Slot::DevCmd, "dev command", "cargo run"),
            slot(
                crate::detect::Slot::SchemaHook,
                "schema command",
                "(unanswered) run the schema step?",
            ),
        ],
        warnings: Vec::new(),
    };
    let text = super::answers::render_init(&report, "would write");
    assert!(!text.contains("nothing left"), "{text}");
    assert!(text.starts_with("1 question is still unanswered"), "{text}");
    assert!(text.contains("schema command  (unanswered)"), "{text}");

    report.slots.pop();
    let text = super::answers::render_init(&report, "would write");
    assert!(text.starts_with("nothing left to answer"), "{text}");
}

// `start` on a library said "no processes configured; add [dev] to
// pando.toml" — no path, no example — and a failed install said only that
// it failed.
#[test]
fn a_start_error_says_which_file_to_edit_and_what_to_write() {
    let fx = fixture();
    let err = super::with_a_way_past(
        &fx.paths,
        anyhow::anyhow!("no processes configured; add [dev] to pando.toml"),
    );
    let msg = format!("{err:#}");
    assert!(msg.starts_with("nothing to run: "), "{msg}");
    assert!(
        msg.contains(&format!(
            "{}:\n[dev]\ncmd = \"",
            fx.paths.config_file().display()
        )),
        "{msg}"
    );

    let err = super::with_a_way_past(
        &fx.paths,
        anyhow::anyhow!("exited 2: ERR_PNPM_OUTDATED_LOCKFILE").context("the install hook failed"),
    );
    let msg = format!("{err:#}");
    assert!(
        msg.starts_with("the install hook failed: exited 2: ERR_PNPM"),
        "{msg}"
    );
    assert!(
        msg.contains(&format!(
            "`project.install` in {} is the command; fix it there, or set it to \"\" to skip \
             installing",
            fx.paths.config_file().display()
        )),
        "{msg}"
    );
    // The file that really says it, when a committed one does.
    std::fs::write(fx.root.join("pando.toml"), "[project]\ninstall = \"x\"\n").unwrap();
    let msg = format!(
        "{:#}",
        super::with_a_way_past(
            &fx.paths,
            anyhow::anyhow!("the install hook failed: exited 2")
        )
    );
    assert!(
        msg.contains(&fx.root.join("pando.toml").display().to_string()),
        "{msg}"
    );

    // Anything else, including a question, is left exactly as it was.
    let usage = super::with_a_way_past(&fx.paths, UsageError("x".into()).into());
    assert!(usage.downcast_ref::<UsageError>().is_some());
}

// An error from below the CLI knew only the directory: the sentence gets
// the branch, a suggested command what was typed, and a path stays a path.
#[test]
fn an_error_is_reworded_to_the_name_a_person_knows() {
    let named = super::names::Named {
        dir: "feat+m".into(),
        shown: "feat/m".into(),
        typed: "feat+m".into(),
    };
    assert_eq!(
        named.in_words(
            "feat+m is still starting — `pando status feat+m` says when; log in /h/logs/feat+m/dev.log"
        ),
        "feat/m is still starting — `pando status feat+m` says when; log in /h/logs/feat+m/dev.log"
    );
    let typed_branch = super::names::Named {
        typed: "feat/m".into(),
        ..named.clone()
    };
    assert_eq!(
        typed_branch.in_words("feat+m has no port yet — start it first"),
        "feat/m has no port yet — start it first"
    );
    assert_eq!(
        typed_branch.in_words("feat+mx is other"),
        "feat+mx is other"
    );
    // A question keeps its type, so its exit code survives.
    let question = anyhow::Error::new(UsageError("feat+m".into()));
    assert!(
        named
            .reword(question)
            .downcast_ref::<UsageError>()
            .is_some()
    );
}

#[test]
fn completions_cover_every_verb() {
    let mut out = Vec::new();
    completions(clap_complete::Shell::Zsh, &mut out).unwrap();
    let script = String::from_utf8(out).unwrap();
    for sub in Cli::command().get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        assert!(script.contains(sub.get_name()), "{}", sub.get_name());
    }
}

fn completion_script(shell: clap_complete::Shell) -> String {
    let mut out = Vec::new();
    completions(shell, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

// `completions zsh` completed `<name>` with file names.
#[test]
fn a_worktree_argument_completes_worktree_names_not_files() {
    let verbs = super::completion::verbs_taking_a_worktree();
    for verb in [
        "start", "stop", "restart", "logs", "open", "share", "unshare", "status", "rm", "path",
    ] {
        assert!(verbs.iter().any(|v| v == verb), "{verb}: {verbs:?}");
    }
    assert!(!verbs.iter().any(|v| v == "new"), "new takes a new branch");

    let zsh = completion_script(clap_complete::Shell::Zsh);
    assert!(zsh.contains("_pando_worktrees() {"), "{zsh}");
    assert!(zsh.contains("pando ls --names 2>/dev/null"), "{zsh}");
    // Defined before the function that calls it.
    assert!(zsh.find("_pando_worktrees() {") < zsh.find("_pando() {"));
    let name_lines: Vec<&str> = zsh.lines().filter(|l| l.contains("name -- ")).collect();
    assert_eq!(name_lines.len(), verbs.len(), "{name_lines:#?}");
    for line in name_lines {
        assert!(line.ends_with(":_pando_worktrees' \\"), "{line}");
    }

    let bash = completion_script(clap_complete::Shell::Bash);
    assert_eq!(
        bash.matches("$(pando ls --names 2>/dev/null)").count(),
        verbs.len(),
        "{bash}"
    );
    let fish = completion_script(clap_complete::Shell::Fish);
    assert!(
        fish.contains("__fish_seen_subcommand_from rm path start") && fish.contains("ls --names"),
        "{fish}"
    );
}

#[test]
fn ls_names_lists_what_a_worktree_argument_accepts() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| super::completion::names(&fx.paths, b));
    let names: Vec<&str> = text.lines().collect();
    assert_eq!(names, ["acme-shop", "feat/one"], "{text}");
    // The main checkout is offered for `path`, the one verb it is a place
    // for; every worktree resolves for all of them.
    for name in &names {
        super::names::path(&fx.paths, name).unwrap();
    }
    super::names::resolve(&fx.paths, names[1]).unwrap();
}

// A menu entry is a line: the long `--help` paragraph belongs to `--help`.
#[test]
fn completion_menus_get_the_short_help_only() {
    let zsh = completion_script(clap_complete::Shell::Zsh);
    for line in zsh.lines().filter(|l| l.starts_with('\'')) {
        let Some(open) = line.find('[') else { continue };
        let Some(close) = line.rfind(']') else {
            continue;
        };
        if close <= open {
            continue;
        }
        let description = &line[open + 1..close];
        assert!(
            description.chars().count() <= 100,
            "a menu description of {} characters: {line}",
            description.chars().count()
        );
    }
}

#[test]
fn help_documents_the_needs_answer_exit_code() {
    let help = Cli::command().render_help().to_string();
    assert!(help.contains("Exit codes"), "{help}");
    assert!(
        help.contains("3  needs an answer"),
        "an agent has to be able to tell a question from a failure: {help}"
    );
}

// ---- ls columns ------------------------------------------------------

fn widths(pairs: &[(Col, usize)]) -> BTreeMap<Col, usize> {
    pairs.iter().copied().collect()
}

fn every_column(path: usize) -> BTreeMap<Col, usize> {
    widths(&[
        (Col::Name, 10),
        (Col::Status, 8),
        (Col::Url, 22),
        (Col::Ports, 5),
        (Col::Mode, 8),
        (Col::Public, 40),
        (Col::Git, 5),
        (Col::Branch, 10),
        (Col::Head, 7),
        (Col::Path, path),
    ])
}

#[test]
fn a_wide_terminal_keeps_every_column() {
    assert_eq!(keep_columns(400, &every_column(40)), ORDER.to_vec());
}

// A tmux split is the normal case, so the listing has to survive one:
// what a worktree is doing outlives what git thinks of it, and the URL is
// what somebody came to copy.
#[test]
fn a_narrow_terminal_sheds_columns_and_keeps_name_status_and_url() {
    let w = every_column(60);
    let mid = keep_columns(100, &w);
    assert!(
        !mid.contains(&Col::Path) && !mid.contains(&Col::Head),
        "the path and the sha go first: {mid:?}"
    );
    assert!(mid.contains(&Col::Url), "{mid:?}");

    let tight = keep_columns(44, &w);
    assert_eq!(tight, vec![Col::Name, Col::Status, Col::Url]);
    let tighter = keep_columns(24, &w);
    assert_eq!(tighter, vec![Col::Name, Col::Status]);
    let sliver = keep_columns(4, &w);
    assert_eq!(
        sliver,
        vec![Col::Name, Col::Status],
        "the name and the status are never dropped"
    );
}

#[test]
fn ls_shows_the_ports_and_status_of_a_running_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.processes.insert(
        "dev".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: std::process::id() as i32,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "dev"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(text.contains("PORTS") && text.contains("STATUS"), "{text}");
    // One port is named by its role like many are: a bare "17342" beside
    // another row's "api:29496 web:29497" read as a different thing.
    assert!(text.contains("web:17342"), "{text}");
    assert!(text.contains("running"), "{text}");

    let narrow = capture(|b| ls_text_at(&fx.paths, b, 24));
    assert!(narrow.contains("feat/one"), "{narrow}");
    assert!(
        !narrow.contains("PATH"),
        "a narrow listing sheds the path: {narrow}"
    );
}

#[test]
fn a_worktree_with_nothing_running_shows_dashes() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(text.contains("feat/one"), "{text}");
    assert!(text.contains(" -"), "{text}");
}

/// A process record for a live group with the sockets it was seen
/// holding.
fn listening(pgid: i32, observed: &[u16]) -> crate::state::ProcessRecord {
    crate::state::ProcessRecord {
        pid: std::process::id(),
        pgid,
        started_at: Utc::now(),
        log_path: PathBuf::from("/does/not/exist/dev.log"),
        ready_port: None,
        ready_timeout_s: None,
        observed_ports: observed.to_vec(),
        swept: false,
        phase: Phase::Running { since: Utc::now() },
    }
}

/// A worktree shaped like a one-process start: `dev` owning `web`.
fn one_process_record(observed: &[u16]) -> WorktreeRecord {
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_342);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    record
        .processes
        .insert("dev".to_string(), listening(101, observed));
    record.observed_ports = observed.to_vec();
    record
}

/// A worktree shaped like a two-process start: `web` and `api`, each
/// owning its own role and running in its own group.
fn two_process_record(web: &[u16], api: &[u16]) -> WorktreeRecord {
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_342);
    record.ports.insert("api".to_string(), 17_343);
    record
        .roles
        .insert("web".to_string(), vec!["web".to_string()]);
    record
        .roles
        .insert("api".to_string(), vec!["api".to_string()]);
    record
        .processes
        .insert("web".to_string(), listening(101, web));
    record
        .processes
        .insert("api".to_string(), listening(102, api));
    let mut union: Vec<u16> = web.iter().chain(api).copied().collect();
    union.sort_unstable();
    union.dedup();
    record.observed_ports = union;
    record
}

// The documented behaviour — "what it is really listening on when that
// is known" — was two identical match arms, so a framework that ignored
// `PORT` and bound something else still had the assigned port printed
// as its URL.
#[test]
fn the_url_prefers_a_port_the_process_is_really_listening_on() {
    assert_eq!(
        worktree_url(&one_process_record(&[17_342, 17_399])).as_deref(),
        Some("http://localhost:17342"),
        "the assigned port is among them, so it is the one"
    );
    assert_eq!(
        worktree_url(&one_process_record(&[3_000])).as_deref(),
        Some("http://localhost:3000"),
        "it ignored the port pando gave it; the URL follows the process"
    );
    assert_eq!(
        worktree_url(&one_process_record(&[])).as_deref(),
        Some("http://localhost:17342"),
        "nothing observed at all falls back to what was assigned"
    );
    // And nothing running at all: the port survives the stop, so the
    // URL the developer bookmarked is still the one they get.
    let mut record = one_process_record(&[3_000]);
    record.processes.clear();
    record.observed_ports.clear();
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:17342")
    );
}

// Phase 2b review, finding 6. One rule with three implementations:
// `start` took the first role of the first process, `status` and `ls`
// took the alphabetically first *role*, and the TUI took that and never
// looked at what was really listening. Two processes whose role names
// sort the other way round from their own names were all it took for
// `pando start` and `pando status`, seconds apart, to hand out two
// different URLs.
#[test]
fn the_url_is_the_first_role_of_the_first_process_when_nothing_owns_web() {
    let mut record = WorktreeRecord::new("/trees/feat+url2", true);
    record.ports.insert("srv".to_string(), 19_056);
    record.ports.insert("admin".to_string(), 19_057);
    record
        .roles
        .insert("alpha".to_string(), vec!["srv".to_string()]);
    record
        .roles
        .insert("beta".to_string(), vec!["admin".to_string()]);
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:19056"),
        "alpha comes first, so alpha's first role is the worktree's URL"
    );

    // `web` still wins wherever anything owns it, whatever it sorts
    // against.
    record.ports.insert("web".to_string(), 19_058);
    record
        .roles
        .insert("zeta".to_string(), vec!["web".to_string()]);
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:19058")
    );
}

// Phase 2b review, finding 4. `f6093df` narrowed "prefer an observed
// port" to "prefer one no role claims", but the observed list was one
// flat set per worktree, so a socket the *api* opened — an HMR socket,
// `node --inspect`, a metrics port — was indistinguishable from one the
// web process opened, and became the worktree's URL while the web
// server was not serving at all.
#[test]
fn the_url_follows_a_listener_only_in_the_group_that_owns_the_role() {
    assert_eq!(
        worktree_url(&two_process_record(&[17_342], &[17_343])).as_deref(),
        Some("http://localhost:17342"),
        "both up, each on its own port"
    );

    assert_eq!(
        worktree_url(&two_process_record(&[17_342], &[9876, 17_343])).as_deref(),
        Some("http://localhost:17342"),
        "the api's second socket is the api's, whatever claims it"
    );

    // The review's reproduction: the web process stopped, the api kept
    // serving, and it holds a port no role claims.
    let mut record = two_process_record(&[], &[9876, 17_343]);
    record.processes.remove("web");
    assert_eq!(
        worktree_url(&record).as_deref(),
        Some("http://localhost:17342"),
        "the web role's own port, not whatever the api happens to hold"
    );

    // And a framework that ignored `PORT` is still followed, because
    // there it is the process that owns the role doing the ignoring.
    assert_eq!(
        worktree_url(&two_process_record(&[3000], &[17_343])).as_deref(),
        Some("http://localhost:3000"),
        "the web process itself bound 3000"
    );

    // A port another role already has is never a candidate either.
    assert_eq!(
        worktree_url(&two_process_record(&[17_343], &[17_343])).as_deref(),
        Some("http://localhost:17342")
    );
}

// ---- questions -------------------------------------------------------

fn dev_question(options: &[&str]) -> actions::Question {
    actions::Question {
        slot: crate::detect::Slot::DevCmd,
        prompt: "Which command starts the local development server?".to_string(),
        options: options
            .iter()
            .map(|v| (v.to_string(), "a signal".to_string()))
            .collect(),
        preselect: (!options.is_empty()).then_some(0),
        allow_custom: true,
        allow_none: false,
        multi: false,
        checked: Vec::new(),
        details: Vec::new(),
        answer_file: None,
        snippet: String::new(),
    }
}

/// The services question of fixture 6: two ticked by a rule, two the
/// rules could not place.
fn services_question() -> actions::Question {
    actions::Question {
        slot: crate::detect::Slot::Services,
        prompt: "Run private copies of these services for each worktree?".to_string(),
        options: ["cache", "db", "mail", "queue"]
            .iter()
            .map(|v| (v.to_string(), "docker-compose.yml".to_string()))
            .collect(),
        preselect: Some(0),
        allow_custom: false,
        allow_none: true,
        multi: true,
        checked: vec![1, 2],
        details: Vec::new(),
        answer_file: None,
        snippet: String::new(),
    }
}

/// The prompt driven by a script of typed lines, as a terminal would.
fn answer_with(question: &actions::Question, lines: &[&str]) -> (Result<actions::Answer>, String) {
    let mut typed = lines.iter().map(|l| format!("{l}\n"));
    let mut out = Vec::new();
    let answer = prompt_with(question, &mut out, || Ok(typed.next()));
    (answer, String::from_utf8(out).unwrap())
}

// ---- the multi-select question ---------------------------------------

#[test]
fn a_set_question_starts_from_what_the_rules_resolved() {
    let question = services_question();
    let (answer, printed) = answer_with(&question, &[""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
    assert!(printed.contains("[ ] 1) cache"), "{printed}");
    assert!(printed.contains("[x] 2) db"), "{printed}");
    assert!(printed.contains("[x] 3) mail"), "{printed}");
    assert!(printed.contains("[ ] 4) queue"), "{printed}");
    assert!(printed.contains("accepts [db, mail]"), "{printed}");
}

#[test]
fn a_number_toggles_one_option_and_enter_takes_the_rest() {
    let question = services_question();
    // Tick `cache`, untick `mail`, accept.
    let (answer, _) = answer_with(&question, &["1", "3", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![0, 1]));
}

#[test]
fn unticking_everything_is_the_answer_none() {
    let question = services_question();
    let (answer, _) = answer_with(&question, &["2", "3", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::None);
    // And so is saying so outright.
    let (answer, _) = answer_with(&services_question(), &["n"]);
    assert_eq!(answer.unwrap(), actions::Answer::None);
}

#[test]
fn a_number_out_of_range_reprints_the_range_and_ticks_nothing() {
    let question = services_question();
    let (answer, printed) = answer_with(&question, &["9", "not-a-number", ""]);
    assert_eq!(answer.unwrap(), actions::Answer::Many(vec![1, 2]));
    assert_eq!(
        printed
            .matches("a number between 1 and 4 toggles one")
            .count(),
        2,
        "{printed}"
    );
}

// `Auto`, not `Many`: the resolver turns `Auto` into the ticked set
// *and* into a comment saying a flag took it. A `Many` would be
// written down as if a human had chosen, which is a config nobody can
// review.
#[test]
fn yes_takes_the_ticked_set_as_an_auto_answer_not_a_choice() {
    let question = services_question();
    assert_eq!(asker(true)(&question).unwrap(), actions::Answer::Auto(0));
}

#[test]
fn exit_three_shows_a_set_question_with_its_boxes() {
    let needs = actions::NeedsAnswer {
        question: services_question(),
    };
    let text = render_needs_answer(&needs);
    assert!(text.contains("[ ] 1) cache"), "{text}");
    assert!(text.contains("[x] 2) db"), "{text}");
    assert!(text.contains("[[services]]"), "{text}");
    assert!(
        text.contains("--yes to take the ticked ones"),
        "an agent has to be told what --yes would do: {text}"
    );
}

// A fat-fingered number used to fall through to "it must be a command",
// so `5` on a four-option question became `cmd = "5"`, dated as if a
// human had meant it, and `start` reported success over a shell error.
#[test]
fn a_number_at_the_prompt_is_always_a_choice() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let (answer, printed) = answer_with(&question, &["5", "0", "99", "2"]);
    assert_eq!(answer.unwrap(), actions::Answer::Choice(1));
    assert_eq!(
        printed.matches("pick a number between 1 and 2").count(),
        3,
        "every out-of-range number reprints the range: {printed}"
    );
    assert!(
        !printed.contains("command > "),
        "and none of them is a command: {printed}"
    );
}

// The way a command that is only digits is still reachable.
#[test]
fn a_number_typed_after_c_is_taken_as_the_command() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let (answer, _) = answer_with(&question, &["c", "5"]);
    assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
}

// Unchanged: anything that is not a number is the command itself, so
// nobody has to discover that `c` exists first.
#[test]
fn a_line_that_is_not_a_number_is_still_the_command() {
    let question = dev_question(&["pnpm dev"]);
    let (answer, _) = answer_with(&question, &["./my-own-server"]);
    assert_eq!(
        answer.unwrap(),
        actions::Answer::Custom("./my-own-server".to_string())
    );
}

// With nothing on offer, a number cannot be a choice at all.
#[test]
fn a_question_with_no_options_takes_a_number_as_the_command() {
    let question = dev_question(&[]);
    let (answer, _) = answer_with(&question, &["5"]);
    assert_eq!(answer.unwrap(), actions::Answer::Custom("5".to_string()));
}

// `--yes` takes the first option; it cannot take one that is not there.
#[test]
fn a_question_with_nothing_to_offer_does_not_point_at_yes() {
    let needs = actions::NeedsAnswer {
        question: dev_question(&[]),
    };
    let text = render_needs_answer(&needs);
    assert!(
        !text.contains("--yes"),
        "there is nothing for --yes to take: {text}"
    );
    assert!(text.contains("pando.toml"), "{text}");
}

#[test]
fn a_question_with_options_still_points_at_yes() {
    let needs = actions::NeedsAnswer {
        question: dev_question(&["pnpm dev", "pnpm dev:web"]),
    };
    assert!(render_needs_answer(&needs).contains("--yes"));
}

// ---- status ----------------------------------------------------------

#[test]
fn status_json_carries_the_documented_shape() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(&name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.observed_ports = vec![17_342, 17_399];
    // Alive, so the read path leaves it Running, with a process group
    // that no longer exists — so a scan that runs and finds nothing is
    // an answer, and the ports it is really listening on are none. The
    // last good answer survives only a scan that could not run at all.
    record.processes.insert(
        "dev".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_998,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "dev"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    record.processes.insert(
        "worker".to_string(),
        crate::state::ProcessRecord {
            pid: 4242,
            pgid: 4242,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(&name, "worker"),
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Failed {
                at: Utc::now(),
                reason: "process exited".to_string(),
            },
        },
    );
    record.hooks.insert(
        "install".to_string(),
        crate::state::HookRecord {
            fingerprint: Some("md5:abc".to_string()),
            ran_at: Utc::now(),
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    // Pinned as a literal on purpose: a bump must fail here, at the
    // commit that makes it, rather than passing quietly.
    assert_eq!(v["version"], 2);
    assert_eq!(v["project"]["name"], "acme-shop");
    let wt = &v["worktrees"][0];
    assert_eq!(wt["name"], "feat+one");
    assert_eq!(wt["branch"], "feat/one");
    assert_eq!(wt["ports"]["web"], 17_342);
    assert_eq!(wt["observed_ports"], serde_json::json!([]));
    assert_eq!(wt["url"], "http://localhost:17342");
    let dev = &wt["processes"]["dev"];
    assert_eq!(dev["pid"], std::process::id());
    assert_eq!(dev["phase"], "running");
    assert_eq!(dev["reason"], serde_json::Value::Null);
    assert!(dev["since"].is_string());
    assert!(dev["log"].as_str().unwrap().ends_with("dev.log"));
    let worker = &wt["processes"]["worker"];
    assert_eq!(worker["phase"], "failed");
    assert_eq!(worker["reason"], "process exited");
    assert_eq!(wt["hooks"]["install"]["fingerprint"], "md5:abc");
}

/// A worktree with a live share recorded, so the status shapes can be
/// asserted without a tunnel. Every pid is this process: alive, so the
/// refresh leaves the record alone — the application it publishes
/// included, because a share whose application is gone is closed.
fn with_share(fx: &Fx, name: &str, proxy: Option<u16>) {
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    record
        .processes
        .insert("dev".to_string(), listening(std::process::id() as i32, &[]));
    record.share_port = proxy;
    record.share = Some(crate::state::ShareRecord {
        tunnel_pid: std::process::id(),
        tunnel_pgid: 999_998,
        public_url: "https://fake-host.trycloudflare.com".to_string(),
        local_port: 17_342,
        started_at: Utc::now(),
        log_path: fx.paths.log_file(name, "tunnel"),
        proxy_pid: proxy.map(|_| std::process::id()),
        proxy_pgid: proxy.map(|_| 999_997),
        proxy_port: proxy,
    });
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
}

#[test]
fn status_json_carries_the_public_url_and_never_a_cookie() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let share = &v["worktrees"][0]["share"];
    assert_eq!(share["url"], "https://fake-host.trycloudflare.com");
    assert_eq!(share["local_port"], 17_342);
    assert_eq!(share["proxy_port"], 17_349);
    assert!(share["since"].is_string());
    assert!(
        !text.to_lowercase().contains("cookie"),
        "a credential must never reach a shape that gets piped into things:\n{text}"
    );
}

#[test]
fn status_json_says_null_for_a_worktree_that_is_not_shared() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"][0]["share"], serde_json::Value::Null);
}

#[test]
fn status_json_reports_a_share_with_no_proxy_in_front_of_it() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, None);
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["worktrees"][0]["share"]["proxy_port"],
        serde_json::Value::Null
    );
}

#[test]
fn status_text_prints_the_public_url_under_its_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_text_at(&fx.paths, None, b, 120));
    assert!(text.contains("share"), "{text}");
    assert!(
        text.contains("https://fake-host.trycloudflare.com"),
        "{text}"
    );
    assert!(
        text.contains("through a proxy on 17349"),
        "the proxy is worth saying: a visitor arrives authenticated: {text}"
    );
}

// The same degradation every other row has: truncated, never wrapped.
#[test]
fn the_share_row_truncates_on_a_narrow_terminal() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, Some(17_349));

    let text = capture(|b| status_text_at(&fx.paths, None, b, 40));
    for line in text.lines() {
        assert!(
            line.chars().count() <= 40,
            "a row wider than the terminal: {line:?}"
        );
    }
}

#[test]
fn status_json_reports_a_worktree_that_was_never_started() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let wt = &v["worktrees"][0];
    assert!(wt["processes"].as_object().unwrap().is_empty());
    assert_eq!(wt["url"], serde_json::Value::Null);
    assert!(wt["observed_ports"].as_array().unwrap().is_empty());
    assert_eq!(wt["mode"], "shared", "never started is shared");
    assert_eq!(wt["isolated"], false);
}

// `mode` came after `isolated`, which programs already read: the new
// word is added beside it, and the old flag stays true for isolated alone.
#[test]
fn status_and_ls_json_publish_the_mode_beside_the_old_isolated_flag() {
    use crate::state::ServiceMode;
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    for mode in ServiceMode::ALL {
        let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
        store.worktrees.get_mut(&name).unwrap().mode = Some(mode);
        crate::state::save(&fx.paths.state_file(), &store).unwrap();

        let text = capture(|b| status_json(&fx.paths, None, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let wt = &v["worktrees"][0];
        assert_eq!(wt["mode"], mode.word(), "{text}");
        assert_eq!(wt["isolated"], mode == ServiceMode::Isolated, "{text}");

        let text = capture(|b| ls_json(&fx.paths, b));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["worktrees"][0]["mode"], mode.word(), "{text}");
    }
}

#[test]
fn status_can_be_asked_about_one_worktree() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    actions::new(&fx.paths, &fx.config, "feat/two", None, &|_| {}).unwrap();
    let text = capture(|b| status_json(&fx.paths, Some("feat+two"), b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"].as_array().unwrap().len(), 1);
    assert_eq!(v["worktrees"][0]["name"], "feat+two");

    let err = status_text(&fx.paths, Some("nope"), &mut Vec::new()).unwrap_err();
    assert!(format!("{err:#}").contains("no worktree named"), "{err:#}");
}

// A worktree removed with `git worktree remove --force` while its dev
// server ran still resolves, from its record, so `stop` can clean up.
// `status` of it said "no worktree named" and exited 0, and `--json`
// printed an empty list, while its processes still held their ports.
#[test]
fn status_of_a_worktree_git_no_longer_lists_fails_and_names_what_still_runs() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_share(&fx, &name, None);
    let dir = fx.config.worktrees_dir(&fx.paths).join(&name);
    git(
        &fx.root,
        &["worktree", "remove", "--force", dir.to_str().unwrap()],
    );
    assert_eq!(super::names::resolve(&fx.paths, "feat+one").unwrap(), name);

    let err = status_text(&fx.paths, Some(&name), &mut Vec::new()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("git no longer lists feat+one"), "{msg}");
    assert!(
        msg.contains("runs dev") && msg.contains("`pando stop feat+one`"),
        "{msg}"
    );
    let err = status_json(&fx.paths, Some(&name), &mut Vec::new()).unwrap_err();
    assert_eq!(format!("{err:#}"), msg);
}

#[test]
fn status_text_names_what_each_worktree_is_doing() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| status_text(&fx.paths, None, b));
    assert!(text.contains("feat/one"), "{text}");
    assert!(text.contains("stopped"), "{text}");
}

/// A worktree running `web` and `api`, recorded as a refresh would
/// leave it. `pid` is this test process, which really is alive, so the
/// read path does not turn the phase into a failure underneath.
fn with_two_processes(fx: &Fx, name: &str, api_phase: Phase) {
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(name).unwrap();
    record.ports.insert("web".to_string(), 17_342);
    record.ports.insert("api".to_string(), 17_343);
    record.observed_ports = vec![17_342, 17_343];
    record.processes.insert(
        "web".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_998,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(name, "web"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Running { since: Utc::now() },
        },
    );
    record.processes.insert(
        "api".to_string(),
        crate::state::ProcessRecord {
            pid: std::process::id(),
            pgid: 999_997,
            started_at: Utc::now(),
            log_path: fx.paths.log_file(name, "api"),
            ready_port: Some(17_343),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: api_phase,
        },
    );
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
}

// `the_url_follows_a_listener_only_when_no_other_role_owns_that_port`
// lived here. It asserted that a port no role claims becomes the
// worktree's URL, which is the bug finding 4 reproduces: that port
// belongs to whichever group opened it.
// `the_url_follows_a_listener_only_in_the_group_that_owns_the_role`
// above is the rule it should have pinned.

#[test]
fn status_text_lists_every_process_under_its_worktree() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

    let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "one worktree line and two process lines:\n{text}"
    );
    assert!(lines[0].starts_with("feat/one"), "{text}");
    assert!(lines[0].contains("running"), "{text}");
    assert!(
        lines[0].contains("api 17343") && lines[0].contains("web 17342"),
        "the worktree line carries every role: {text}"
    );
    assert!(
        lines[0].contains("http://localhost:17342"),
        "and one URL, the web role's: {text}"
    );
    // Indented, in config order, each with its own pid.
    assert!(lines[1].starts_with("  api"), "{text}");
    assert!(lines[2].starts_with("  web"), "{text}");
    for line in &lines[1..] {
        assert!(
            line.contains(&format!("pid {}", std::process::id())),
            "{text}"
        );
        assert!(line.contains("running"), "{text}");
    }
}

// Phase 2b review, finding 9. `ls` sheds columns and the TUI detail
// pane truncates; `status` had no width parameter at all, and the
// per-process rows 2b added grow the block it prints. In the tmux split
// the TUI is designed for, it wrapped.
#[test]
fn status_text_sheds_the_url_and_then_the_ports_as_the_terminal_narrows() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });

    let wide = capture(|b| status_text_at(&fx.paths, None, b, 200));
    assert!(wide.contains("http://localhost:17342"), "{wide}");
    assert!(
        wide.contains("api 17343") && wide.contains("web 17342"),
        "{wide}"
    );

    for width in [60, 44, 32, 24, 12] {
        let text = capture(|b| status_text_at(&fx.paths, None, b, width));
        for line in text.lines() {
            assert!(
                line.chars().count() <= width,
                "{line:?} is wider than {width} columns:\n{text}"
            );
        }
        assert!(
            text.contains("feat/one"),
            "the name is the identifier and never goes: {text}"
        );
    }

    // The URL is the longest cell and `--json` still carries it, so it
    // is the first thing to go; the ports cell is truncated after that.
    let narrow = capture(|b| status_text_at(&fx.paths, None, b, 44));
    assert!(!narrow.contains("http://"), "{narrow}");
    assert!(narrow.contains("running"), "{narrow}");
}

// `status` measured in characters, as `ls` once did: a name with wide
// characters pushed its phase word right of its neighbours', and a
// failure reason in Japanese, cut to the width in characters, still ran
// past the terminal and wrapped.
#[test]
fn status_text_lines_up_and_fits_wide_characters_by_the_columns_they_take() {
    let fx = fixture();
    let wide = actions::new(&fx.paths, &fx.config, "feat/日本語ログイン", None, &|_| {}).unwrap();
    actions::new(&fx.paths, &fx.config, "feat/api", None, &|_| {}).unwrap();
    with_two_processes(
        &fx,
        &wide,
        Phase::Failed {
            at: Utc::now(),
            reason: "ポートはすでに別のプロセスが使っています".repeat(4),
        },
    );

    let text = capture(|b| status_text_at(&fx.paths, None, b, 80));
    let phase_columns: Vec<usize> = text
        .lines()
        .filter(|line| line.starts_with("feat/"))
        .map(|line| {
            let at = line.find("failed").or(line.find("stopped")).unwrap();
            crate::term::text_width(&line[..at])
        })
        .collect();
    assert_eq!(phase_columns.len(), 2, "{text}");
    assert_eq!(phase_columns[0], phase_columns[1], "{text}");
    for line in text.lines() {
        assert!(
            crate::term::text_width(line) <= 80,
            "{line:?} is wider than 80 columns:\n{text}"
        );
    }
}

// A name as wide as the terminal was printed whole, leaving the rest of
// its line no room at all, so every line of it wrapped.
#[test]
fn status_text_cuts_a_name_too_long_for_the_terminal() {
    let fx = fixture();
    let branch = format!("feat/{}", "long-".repeat(20));
    actions::new(&fx.paths, &fx.config, &branch, None, &|_| {}).unwrap();

    let text = capture(|b| status_text_at(&fx.paths, None, b, 80));
    let line = text.lines().next().unwrap();
    assert!(crate::term::text_width(line) <= 80, "{line:?}");
    assert!(line.contains("stopped"), "{line:?}");
}

#[test]
fn status_text_says_which_process_failed() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_two_processes(
        &fx,
        &name,
        Phase::Failed {
            at: Utc::now(),
            reason: "process exited".to_string(),
        },
    );

    let text = capture(|b| status_text_at(&fx.paths, None, b, 200));
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].contains("failed") && lines[0].contains("api: process exited"),
        "a worktree with a dead api is failed, and says which: {text}"
    );
    assert!(
        lines[1].contains("api") && lines[1].contains("failed"),
        "{text}"
    );
    assert!(
        lines[2].contains("web") && lines[2].contains("running"),
        "the process that is still up says so: {text}"
    );
}

#[test]
fn the_listing_shows_the_aggregate_not_the_first_process() {
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    // `api` sorts first and is running; `web` is the failed one, so a
    // row that showed the first process would read "running".
    with_two_processes(&fx, &name, Phase::Running { since: Utc::now() });
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store
        .worktrees
        .get_mut(&name)
        .unwrap()
        .processes
        .get_mut("web")
        .unwrap()
        .phase = Phase::Failed {
        at: Utc::now(),
        reason: "process exited".to_string(),
    };
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let text = capture(|b| ls_text_at(&fx.paths, b, 200));
    assert!(
        text.contains("failed"),
        "the row is the worst of its processes: {text}"
    );
}

#[test]
fn logs_read_the_source_they_are_asked_for() {
    let fx = fixture();
    write_log(&fx, "feat+one", "web", "web line\n");
    write_log(&fx, "feat+one", "api", "api line\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "api", 5, false, false, b, &quiet));
    assert_eq!(text, "api line\n");

    let mut out = Vec::new();
    let err = logs(
        &fx.paths, "feat+one", "worker", 5, false, false, &mut out, &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("worker"), "{msg}");
    assert!(
        msg.contains("api") && msg.contains("web"),
        "an unknown source lists the ones there are: {msg}"
    );
}

#[test]
fn uptime_reads_in_the_unit_that_fits() {
    use chrono::TimeDelta;
    assert_eq!(human_duration(TimeDelta::seconds(9)), "9s");
    assert_eq!(human_duration(TimeDelta::seconds(70)), "1m10s");
    assert_eq!(human_duration(TimeDelta::seconds(3_700)), "1h1m");
    assert_eq!(human_duration(TimeDelta::seconds(90_000)), "1d1h");
    assert_eq!(human_duration(TimeDelta::seconds(-5)), "0s");
}

// ---- logs ------------------------------------------------------------

fn write_log(fx: &Fx, name: &str, source: &str, text: &str) {
    let path = fx.paths.log_file(name, source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// The read side of finding 1: `--source` is the other half of the same
// path component, so a traversal there reads a file outside the
// worktree's log directory — one `available_sources` never lists, so
// nothing even suggests it is reachable.
#[test]
fn a_log_source_that_escapes_the_log_directory_is_refused() {
    let fx = fixture();
    write_log(&fx, "feat+one", "web", "web line\n");
    // A real file the traversal would reach, so the refusal is about
    // the name rather than about the file not being there.
    std::fs::create_dir_all(fx.paths.project_dir()).unwrap();
    std::fs::write(fx.paths.project_dir().join("outside.log"), "secret\n").unwrap();

    let mut out = Vec::new();
    let err = logs(
        &fx.paths,
        "feat+one",
        "../../outside",
        5,
        false,
        false,
        &mut out,
        &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("\"../../outside\""), "{msg}");
    assert!(msg.contains("logs/<worktree>"), "{msg}");
    assert!(
        out.is_empty(),
        "nothing outside the log directory may be printed: {}",
        String::from_utf8_lossy(&out)
    );

    // And the hook logs pando writes itself are still readable by name.
    write_log(&fx, "feat+one", "install", "install line\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "install", 5, false, false, b, &quiet));
    assert_eq!(text, "install line\n");
}

#[test]
fn logs_prints_the_last_lines() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "one\ntwo\nthree\nfour\n");
    let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 2, false, false, b, &quiet));
    assert_eq!(text, "three\nfour\n");
}

/// The third dead end of the first contact run, and the worst of them:
/// `doctor` said the process failed and pointed at `pando logs`; that
/// printed nothing and exited 0. Following pando's own advice led to
/// silence, with no way to tell an empty log from a wrong worktree
/// name, a wrong `--source`, or a broken command.
#[test]
fn an_empty_log_says_that_it_is_empty() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert_eq!(text, "", "stdout is still only the log");
    assert!(
        notes.iter().any(|n| n.contains("is empty")),
        "an empty log is a fact pando knows: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("dev") && n.contains("feat+one")),
        "and it names what was read: {notes:?}"
    );
}

/// `-n 0` asks for no lines, and gets none — without being told that a
/// log full of them is empty. And a `-n` far past the file is the whole
/// file, not a buffer sized for it up front: `usize::MAX` panicked with
/// "capacity overflow".
#[test]
fn a_tail_of_zero_or_of_everything_reads_the_log_as_it_is() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "one\ntwo\n");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 0, false, false, b, n));
    assert_eq!(text, "");
    assert!(notes.is_empty(), "the log is not empty: {notes:?}");

    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", usize::MAX, false, false, b, n));
    assert_eq!(text, "one\ntwo\n");
    assert!(notes.is_empty(), "{notes:?}");
}

/// A process killed mid-line leaves its last words without a newline.
/// The failure classifier reads them — `snapshot` flushes the pending
/// line — and `pando logs` used to withhold them and print nothing, so
/// pando knew more about the crash than the developer could see.
#[test]
fn a_last_line_with_no_newline_is_still_printed() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "done\nSegmentation fault");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert_eq!(text, "done\nSegmentation fault\n");
    assert!(
        notes.is_empty(),
        "there was something to print, so nothing to explain: {notes:?}"
    );
}

/// Which leaves one state a one-shot read cannot reach and a follower
/// can: `-f` on a file holding an unterminated first line, where the
/// rest of it really is still coming.
#[test]
fn a_log_with_no_complete_line_is_described_by_its_size() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "half a line with no newline");
    let notes = silence_notes(
        &fx.paths,
        "feat+one",
        "dev",
        &fx.paths.log_file("feat+one", "dev"),
    );
    assert!(
        notes.iter().any(|n| n.contains("27 bytes")),
        "the size is the whole difference from an empty file: {notes:?}"
    );
    assert!(
        !notes.iter().any(|n| n.contains("is empty")),
        "and it is not empty: {notes:?}"
    );
}

/// The sentence the developer came for: why the log is empty. The
/// record already knows, because `explain_failure` wrote it there.
#[test]
fn an_empty_log_carries_the_reason_the_record_knows() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let mut store = crate::state::State::new();
    let mut record = WorktreeRecord::new(fx.paths.worktree_path("feat+one"), true);
    record.processes.insert(
        "dev".to_string(),
        ProcessRecord {
            pid: 1,
            pgid: 1,
            started_at: Utc::now(),
            log_path: fx.paths.log_file("feat+one", "dev"),
            ready_port: None,
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase: Phase::Failed {
                at: Utc::now(),
                reason: "process exited with status 0 — it printed nothing at all".to_string(),
            },
        },
    );
    store.worktrees.insert("feat+one".to_string(), record);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();

    let (_, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, false, b, n));
    assert!(
        notes.iter().any(|n| n.contains("status 0")),
        "the reason is already written down; this is where it is wanted: {notes:?}"
    );
}

/// `--json` is a stream of objects, one per line. A note about the log
/// is not one of them, so it goes to the other channel and stdout
/// stays parseable — empty is a valid answer there.
#[test]
fn an_empty_log_in_json_keeps_stdout_clean() {
    let fx = fixture();
    write_log(&fx, "feat+one", "dev", "");
    let (text, notes) =
        capture_both(|b, n| logs(&fx.paths, "feat+one", "dev", 10, false, true, b, n));
    assert_eq!(text, "", "nothing that is not a log line may be on stdout");
    assert!(
        !notes.is_empty(),
        "and the developer is still told: {notes:?}"
    );
}

#[test]
fn logs_json_emits_one_object_per_line() {
    let fx = fixture();
    write_log(
        &fx,
        "feat+one",
        "dev",
        "2026-09-20T10:00:00Z ready in 412ms\nError: it broke\n",
    );
    let text = capture(|b| logs(&fx.paths, "feat+one", "dev", 10, false, true, b, &quiet));
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["ts"], "2026-09-20T10:00:00+00:00");
    assert_eq!(lines[0]["level"], "info");
    assert!(lines[0]["line"].as_str().unwrap().contains("ready in"));
    assert_eq!(lines[1]["ts"], serde_json::Value::Null);
    assert_eq!(lines[1]["level"], "error");
}

#[test]
fn a_line_with_no_timestamp_pando_can_read_gets_null() {
    assert_eq!(
        leading_timestamp("2026-09-20T10:00:00Z ready"),
        Some("2026-09-20T10:00:00+00:00".to_string())
    );
    assert_eq!(
        leading_timestamp("[2026-09-20T10:00:00+02:00] ready"),
        Some("2026-09-20T08:00:00+00:00".to_string())
    );
    assert_eq!(leading_timestamp("ready in 412ms"), None);
    assert_eq!(leading_timestamp(""), None);
    assert_eq!(leading_timestamp("20/09/2026 10:00:00 ready"), None);
}

#[test]
fn logs_names_the_sources_a_worktree_has() {
    let fx = fixture();
    let err = logs(
        &fx.paths,
        "feat+one",
        "dev",
        10,
        false,
        false,
        &mut Vec::new(),
        &quiet,
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("no logs for feat+one"),
        "{err:#}"
    );

    write_log(&fx, "feat+one", "install", "installing\n");
    let err = logs(
        &fx.paths,
        "feat+one",
        "dev",
        10,
        false,
        false,
        &mut Vec::new(),
        &quiet,
    )
    .unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("install"),
        "it says what is there instead: {msg}"
    );
}

#[test]
fn ls_text_says_so_when_there_are_no_worktrees() {
    let fx = fixture();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(text.contains("no worktrees"), "{text}");
}

// The lead's first read of the old table: a PATH column that wrapped every
// row, and a STATE column that said `pando`. The branch is the name a
// person knows, said once; the path is one flag away.
#[test]
fn ls_text_names_by_branch_says_what_runs_and_leaves_the_path_to_long() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    let header: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["NAME", "STATUS", "URL", "PORTS", "GIT"], "{text}");
    assert!(text.contains("feat/one"), "{text}");
    assert!(
        !text.contains("feat+one"),
        "the directory spelling is not repeated: {text}"
    );
    assert!(text.contains("stopped"), "{text}");
    assert!(text.contains("clean"), "{text}");
    assert!(
        !text.contains(" pando"),
        "no word only pando understands: {text}"
    );
    let path = fx.paths.worktrees_dir().join("feat+one");
    assert!(!text.contains(&path.display().to_string()), "{text}");

    let long = LsView {
        long: true,
        ..LsView::plain(usize::MAX)
    };
    let text = capture(|b| ls_text_with(&fx.paths, b, &long));
    assert!(text.contains("HEAD") && text.contains("PATH"), "{text}");
    let canonical = std::fs::canonicalize(&path).unwrap();
    assert!(text.contains(&canonical.display().to_string()), "{text}");
}

#[test]
fn a_long_listing_writes_home_as_a_tilde() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let path = std::fs::canonicalize(fx.paths.worktrees_dir().join("feat+one")).unwrap();
    let home = path.parent().unwrap().to_path_buf();
    let view = LsView {
        long: true,
        style: crate::term::Style::with(false, Some(home)),
        ..LsView::plain(usize::MAX)
    };
    let text = capture(|b| ls_text_with(&fx.paths, b, &view));
    assert!(text.contains("~/feat+one"), "{text}");
}

// A worktree whose directory is not its branch — adopted, or detached —
// keeps its directory name, and gets a BRANCH column to say what it has
// checked out.
#[test]
fn a_worktree_not_named_for_its_branch_brings_the_branch_column() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let elsewhere = fx.root.parent().unwrap().join("scratch");
    git(
        &fx.root,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic/x",
            elsewhere.to_str().unwrap(),
        ],
    );
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(text.lines().next().unwrap().contains("BRANCH"), "{text}");
    let row = text.lines().find(|l| l.starts_with("scratch")).unwrap();
    assert!(row.contains("topic/x"), "{text}");
    assert!(row.contains("adopted"), "{text}");
}

// Two long branch names that differ late must not print as the same
// truncated name.
#[test]
fn long_names_that_share_a_prefix_stay_distinct_on_a_narrow_terminal() {
    let fx = fixture();
    for n in 1..=2 {
        let branch = format!("feature/very-long-branch-name-number-{n}-with-extra-words");
        actions::new(&fx.paths, &fx.config, &branch, None, &|_| {}).unwrap();
    }
    let wide = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(
        wide.contains("feature/very-long-branch-name-number-1-with-extra-words"),
        "never cut when not a terminal: {wide}"
    );
    let narrow = capture(|b| ls_text_at(&fx.paths, b, 50));
    let names: Vec<&str> = narrow
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{narrow}");
    assert_ne!(names[0], names[1], "{narrow}");
    for line in narrow.lines() {
        assert!(line.chars().count() <= 50, "{line:?} in\n{narrow}");
    }
}

#[test]
fn colour_is_only_there_when_asked_for_and_never_skews_a_column() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let plain = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(!plain.contains('\x1b'), "{plain:?}");
    let view = LsView {
        style: crate::term::Style::with(true, None),
        ..LsView::plain(usize::MAX)
    };
    let painted = capture(|b| ls_text_with(&fx.paths, b, &view));
    assert!(painted.contains('\x1b'), "{painted:?}");
    let stripped: Vec<usize> = painted.lines().map(crate::term::visible_width).collect();
    let widths: Vec<usize> = plain.lines().map(|l| l.chars().count()).collect();
    assert_eq!(stripped, widths, "{painted:?}");
}

#[test]
fn the_listing_shows_mode_and_public_only_when_some_worktree_has_them() {
    let fx = fixture();
    let one = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    actions::new(&fx.paths, &fx.config, "feat/two", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(!text.contains("MODE") && !text.contains("PUBLIC"), "{text}");

    with_share(&fx, &one, None);
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store.worktrees.get_mut(&one).unwrap().mode = Some(crate::state::ServiceMode::Isolated);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(text.contains("MODE") && text.contains("isolated"), "{text}");
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store.worktrees.get_mut(&one).unwrap().mode = Some(crate::state::ServiceMode::Namespaced);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    assert!(
        text.contains("MODE") && text.contains("namespaced"),
        "{text}"
    );
    assert!(
        text.contains("https://fake-host.trycloudflare.com"),
        "{text}"
    );
    assert!(
        text.contains("http://localhost:17342"),
        "a running worktree has its URL: {text}"
    );
    // Narrow: the public URL shortens to `yes` before anything else of
    // weight goes.
    let narrow = capture(|b| ls_text_at(&fx.paths, b, 60));
    assert!(narrow.contains("PUBLIC"), "{narrow}");
    assert!(!narrow.contains("trycloudflare"), "{narrow}");
    assert!(narrow.contains("yes"), "{narrow}");
}

#[test]
fn ls_text_marks_adopted_uncommitted_and_prunable_worktrees() {
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

    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    for word in ["adopted", "uncommitted", "prunable"] {
        assert!(text.contains(word), "missing {word:?} in:\n{text}");
    }
}

#[test]
fn ls_json_emits_the_documented_shape() {
    let fx = fixture();
    actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let text = capture(|b| ls_json(&fx.paths, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();

    // Pinned as a literal on purpose: a bump must fail here, at the
    // commit that makes it, rather than passing quietly.
    assert_eq!(v["version"], 2);
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
    assert_eq!(w["mode"], "shared");
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
            cross_repository: false,
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

// Only stdout's own broken pipe is a reader that stopped. One to a
// process pando runs is a failure, and still has to say so.
#[test]
fn only_a_broken_stdout_ends_quietly() {
    use std::io::{Error, ErrorKind};
    let closed = anyhow::Error::from(super::stdout_error(Error::from(ErrorKind::BrokenPipe)))
        .context("print a line");
    assert!(stdout_closed(&closed));
    let child = anyhow::Error::from(Error::from(ErrorKind::BrokenPipe)).context("feed a client");
    assert!(!stdout_closed(&child));
    let other = anyhow::Error::from(super::stdout_error(Error::from(ErrorKind::Other)));
    assert!(!stdout_closed(&other));
}

// `head` is documented as "abc1234". Porcelain's sha is all forty, and
// enrichment's is git's own abbreviation, which is longer in a large
// repository or under `core.abbrev`: one listing used to publish both
// shapes in the same field.
#[test]
fn the_json_head_is_always_the_short_sha() {
    let mut w = crate::tui::app::tests::wt("feat+one");
    w.head = Some("0123456789012345678901234567890123456789".into());
    w.head_sha = None;
    assert_eq!(short_head(&w).as_deref(), Some("0123456"));

    w.head_sha = Some("0123456789ab".into());
    assert_eq!(short_head(&w).as_deref(), Some("0123456"));

    w.head = None;
    assert_eq!(short_head(&w).as_deref(), Some("0123456"));

    w.head_sha = None;
    assert_eq!(short_head(&w), None);
}

// ---- an answers file -------------------------------------------------

// The names in the file are the names `signals` publishes, because
// they are the same function of the same type.
#[test]
fn every_question_has_one_name_that_round_trips() {
    for slot in actions::ALL_SLOTS {
        let name = slot_name(slot);
        assert!(!name.is_empty(), "{slot:?} has no name");
        assert_eq!(slot_named(&name), Some(slot), "{name} does not round trip");
    }
    assert_eq!(slot_names().len(), actions::ALL_SLOTS.len());
}

/// The nine names, written out.
///
/// `signals` publishes them and `--answers` takes them, and both get
/// them from `Slot`'s own serde names — so a rename stays invisible to
/// every test that only compares the two against each other, while
/// breaking every program ever written against them. This is the
/// assertion a rename has to walk past, and the list is also published
/// in `agent/json.md`, which the test below holds to the same order.
#[test]
fn the_nine_question_names_are_frozen() {
    assert_eq!(
        slot_names(),
        [
            "install",
            "version_files",
            "prelude",
            "processes",
            "dev_cmd",
            "port_env",
            "services",
            "schema_hook",
            "provision",
        ]
    );
}

/// Every `pando …` an agent-facing document tells a reader to run,
/// from its fenced blocks and its inline code spans.
fn commands_named_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            let line = line.trim().split('#').next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("pando ") {
                out.push(rest.trim().to_string());
            }
            continue;
        }
        // Inline: `pando doctor --json` in the middle of a sentence.
        for span in line.split('`').skip(1).step_by(2) {
            if let Some(rest) = span.strip_prefix("pando ") {
                out.push(rest.trim().to_string());
            }
        }
    }
    out
}

/// Holds a document's commands to what the binary really takes.
///
/// Instructions for a language model are the one kind of code that
/// fails silently and plausibly: a flag renamed in `cli.rs` leaves a
/// document that still reads perfectly and no longer works. clap is
/// asked rather than a list kept beside it, so there is nothing to
/// keep in step.
fn assert_every_documented_command_is_real(file: &str) {
    use clap::CommandFactory;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let cli = Cli::command();
    let mut checked = 0;
    for command in commands_named_in(&text) {
        let mut tokens = command.split_whitespace();
        let Some(verb) = tokens.next() else { continue };
        // A bare `pando` with a flag of its own, like --version.
        if verb.starts_with('-') {
            continue;
        }
        let sub = cli
            .get_subcommands()
            .find(|c| c.get_name() == verb)
            .unwrap_or_else(|| {
                panic!("{file} says `pando {command}`, and pando has no {verb:?} command")
            });
        for token in tokens {
            let Some(flag) = token.strip_prefix("--") else {
                continue;
            };
            // `--answers answers.json` — the value is the next token
            // and is not a flag; nothing here needs to know that.
            assert!(
                sub.get_arguments().any(|a| a.get_long() == Some(flag)),
                "{file} says `pando {command}`, and `pando {verb}` has no --{flag}"
            );
        }
        checked += 1;
    }
    assert!(
        checked > 0,
        "{file} names no commands at all — did the format change?"
    );
}

#[test]
fn the_contract_only_names_commands_pando_has() {
    assert_every_documented_command_is_real("agent/json.md");
}

// The host wrappers are glue, but glue that names commands: the same
// rename that would rot the brief rots them.
#[test]
fn every_host_wrapper_only_names_commands_pando_has() {
    // The repository's own README is prose for people, whose command
    // table is not in this shape. Everything here is a document an
    // agent is pointed at and follows literally.
    for file in [
        "agent/README.md",
        "agent/skills/pando-setup/SKILL.md",
        "agent/skills/pando-operate/SKILL.md",
        "agent/codex/pando-setup/SKILL.md",
        "agent/codex/pando-operate/SKILL.md",
    ] {
        assert_every_documented_command_is_real(file);
    }
}

// The brief is a procedure written for a language model, which is the
// one kind of reader that will follow a command that does not exist
// and report that it worked.
#[test]
fn the_brief_only_names_commands_pando_has() {
    assert_every_documented_command_is_real("agent/brief.md");
}

/// `CLAUDE.md` names the CLI verbs and calls them canonical — "used
/// identically in every document" — which is exactly the claim that
/// rots. It was missing `restart` for as long as `restart` existed,
/// in the file that tells every other document what the list is.
///
/// The README has its own check; this is the second place the verbs
/// are written down by hand, and the last one that was unguarded.
///
/// Only the list itself counts, and it is held to clap both ways: the
/// rest of the file says "new", "start" and "open" in prose, so a search
/// of the whole file passed with any of the three gone from the list.
#[test]
fn claude_md_lists_every_verb_pando_has() {
    use clap::CommandFactory;
    use std::collections::BTreeSet;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("CLAUDE.md");
    let text = std::fs::read_to_string(&path).expect("CLAUDE.md");
    // Hard-wrapped, so the list can straddle lines.
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let marker = "CLI verbs, used identically in every document: `";
    let at = flat
        .find(marker)
        .expect("CLAUDE.md has its canonical verb list");
    let rest = &flat[at + marker.len()..];
    let listed: BTreeSet<&str> = rest[..rest.find('`').expect("the list's closing backtick")]
        .split_whitespace()
        .collect();
    let command = Cli::command();
    let verbs: BTreeSet<&str> = command
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help" && !sub.is_hide_set())
        .map(|sub| sub.get_name())
        .collect();
    assert_eq!(
        listed, verbs,
        "CLAUDE.md calls its verb list canonical, and it is not the verbs pando has"
    );
}

/// The README's own command list, against clap.
///
/// It is prose for people, so it is not in the shape
/// [`assert_every_documented_command_is_real`] parses — the
/// description runs on after the verb, and the first line is a bare
/// `pando` that opens the TUI. But the Status section under the list
/// says every command in it is implemented, and that is a claim worth
/// failing over: the line above it said "there is no code yet" for
/// eight phases of code, because nothing read it.
#[test]
fn the_readme_lists_only_commands_pando_has() {
    use clap::CommandFactory;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
    let text = std::fs::read_to_string(&path).expect("the README");
    let cli = Cli::command();
    let mut checked = 0;
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        // Only the fenced list. "pando never writes into your
        // repository" is a sentence, and the promise it belongs to is
        // not a command table.
        if !fenced {
            continue;
        }
        let Some(rest) = line.strip_prefix("pando ") else {
            continue;
        };
        // The one line with no verb: `pando` alone, padded out to the
        // description column.
        if rest.starts_with(' ') {
            continue;
        }
        let verb = rest.split_whitespace().next().unwrap_or_default();
        assert!(
            cli.get_subcommands().any(|c| c.get_name() == verb),
            "README.md lists `pando {verb}`, and pando has no {verb:?} command"
        );
        checked += 1;
    }
    assert!(
        checked >= 13,
        "only {checked} commands were found in README.md — did the list move?"
    );

    // And the other direction, which is the half that was missing:
    // the Status section says every command in the list is
    // implemented, and a reader takes a checked list to be a whole
    // one. `help` is clap's own, and a hidden subcommand is hidden
    // precisely because it is not for people.
    for sub in cli.get_subcommands() {
        let name = sub.get_name();
        if name == "help" || sub.is_hide_set() {
            continue;
        }
        assert!(
            text.contains(&format!("\npando {name} "))
                || text.contains(&format!("\npando {name}\n")),
            "pando has a {name:?} command and README.md does not list it — a list that is \
                 checked reads as a complete one"
        );
    }
}

/// The brief is the only place the reasoning lives, so the things it
/// has to teach are worth failing over if somebody trims it.
///
/// Phrases, not sentences: this is a guard against a section being
/// deleted, not a style checker.
#[test]
fn the_brief_teaches_the_things_only_it_teaches() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/brief.md");
    let text = std::fs::read_to_string(&path).expect("the brief");
    // Collapsed, because the document is hard-wrapped and a phrase it
    // makes is as likely as not to straddle two lines.
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for (phrase, why) in [
        ("init --answers", "the one write path"),
        ("never edit", "and that nothing else is"),
        ("by value", "how an option is named"),
        ("non-frozen", "the install guardrail"),
        (
            "built from the repository",
            "a compose file that only packages the app",
        ),
        ("[isolation] prefer", "the preference an agent cannot write"),
        (
            "machine-wide",
            "that the preference is not a fact about this repository",
        ),
        (
            "gap in the corpus",
            "the one answer the decisions log cannot hold",
        ),
        ("decisions.jsonl", "what pando records about the answerer"),
        ("Prove it by running", "a setup is proved by starting it"),
        (
            "listening on … instead",
            "the failure a config that reads right hides",
        ),
        ("exit 3", "the code that means a question is open"),
        ("--json", "never parse human-readable output"),
    ] {
        assert!(
            text.to_lowercase().contains(&phrase.to_lowercase()),
            "the brief no longer teaches {why}: it never says {phrase:?}"
        );
    }
    // And every question it tells a reader to answer.
    for name in slot_names() {
        assert!(text.contains(&name), "the brief never mentions {name}");
    }
}

/// The contract file says the same names, in the same order.
///
/// A document is the one part of a contract nothing compiles, so it is
/// the part that rots. Reading it from the test is what makes a rename
/// fail in the commit that does it rather than in somebody's agent a
/// month later.
#[test]
fn the_published_contract_names_every_question_in_order() {
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md");
    let text =
        std::fs::read_to_string(&doc).unwrap_or_else(|e| panic!("read {}: {e}", doc.display()));
    assert!(
        text.contains(&slot_names().join("  ")),
        "agent/json.md does not list the nine questions in the order pando asks them"
    );
    for name in slot_names() {
        assert!(text.contains(&name), "agent/json.md never mentions {name}");
    }
}

fn parse_err(json: &str) -> String {
    format!("{:#}", Answers::parse(json).unwrap_err())
}

#[test]
fn a_name_pando_does_not_ask_about_is_a_usage_error_naming_it() {
    let err = parse_err(r#"{"dev_command": "pnpm dev"}"#);
    assert!(err.contains("dev_command"), "{err}");
    assert!(err.contains("dev_cmd"), "and what it does ask about: {err}");
    assert!(
        Answers::parse(r#"{"dev_command": "x"}"#)
            .unwrap_err()
            .downcast_ref::<UsageError>()
            .is_some(),
        "a name that is not a question is a usage error, not a failure"
    );
}

// Caught when the file is read, not when a question happens to reach
// the slot: a shape this slot cannot take is knowable from the slot.
#[test]
fn a_shape_the_slot_cannot_take_is_refused_before_anything_is_written() {
    let err = parse_err(r#"{"install": 42}"#);
    assert!(err.contains("install"), "{err}");
    assert!(err.contains("string"), "{err}");

    let err = parse_err(r#"{"install": null}"#);
    assert!(err.contains("no \"none\" answer"), "{err}");

    let err = parse_err(r#"{"install": ["a", "b"]}"#);
    assert!(err.contains("not a list"), "{err}");

    let err = parse_err(r#"{"services": "db"}"#);
    assert!(err.contains("list of the options"), "{err}");

    let err = parse_err(r#"{"install": "   "}"#);
    assert!(err.contains("empty string"), "{err}");

    // And the shapes that are fine everywhere they are offered.
    assert!(Answers::parse(r#"{"port_env": null}"#).is_ok());
    assert!(Answers::parse(r#"{"services": []}"#).is_ok());
    assert!(Answers::parse(r#"{"provision": [".env"]}"#).is_ok());
    assert!(Answers::parse(r#"{"version_files": [".nvmrc"]}"#).is_ok());
}

#[test]
fn a_file_that_is_not_a_json_object_is_a_usage_error() {
    assert!(parse_err("[1, 2]").contains("JSON object"));
    assert!(parse_err("{").contains("not JSON"));
}

// By value, never by index: the option carries the roles a command
// owns and the process tables a workspace answer is, and a list of
// indexes is a contract that breaks the day a rule finds one more
// candidate.
#[test]
fn an_option_is_answered_by_its_own_text() {
    let question = dev_question(&["pnpm dev", "pnpm dev:web"]);
    let answer = answer_from(&question, &serde_json::json!("pnpm dev:web")).unwrap();
    assert_eq!(
        answer,
        actions::Answer::Program(Box::new(actions::Answer::Choice(1)))
    );
}

// Every question has a custom answer, and a program gets the same one.
#[test]
fn a_value_no_option_has_is_a_command_of_your_own() {
    let question = dev_question(&["pnpm dev"]);
    let answer = answer_from(&question, &serde_json::json!("./serve.sh")).unwrap();
    assert_eq!(
        answer,
        actions::Answer::Program(Box::new(actions::Answer::Custom("./serve.sh".to_string())))
    );
}

// Except at the set question, where there is nothing to type: a
// service the compose file does not declare is not one pando can run.
#[test]
fn a_set_answer_that_names_nothing_on_offer_says_what_is() {
    let question = services_question();
    let err = answer_from(&question, &serde_json::json!(["postgres"])).unwrap_err();
    let printed = format!("{err:#}");
    assert!(printed.contains("postgres"), "{printed}");
    assert!(printed.contains("cache, db, mail, queue"), "{printed}");
    assert!(err.downcast_ref::<UsageError>().is_some());
}

#[test]
fn a_set_answer_is_the_options_it_names_and_an_empty_one_is_none_of_them() {
    let question = services_question();
    assert_eq!(
        answer_from(&question, &serde_json::json!(["db", "cache"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Many(vec![1, 0])))
    );
    assert_eq!(
        answer_from(&question, &serde_json::json!([])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::None))
    );
    assert_eq!(
        answer_from(&question, &serde_json::Value::Null).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::None))
    );
}

// A list slot takes a JSON array, joined into the one value the slot
// writes — so a program never has to know the separator.
#[test]
fn a_list_slot_takes_an_array_and_joins_it_the_way_the_slot_splits_it() {
    let question = actions::Question {
        slot: crate::detect::Slot::Provision,
        prompt: crate::detect::Slot::Provision.prompt().to_string(),
        options: vec![(".env,.env.local".to_string(), "here".to_string())],
        preselect: Some(0),
        allow_custom: true,
        allow_none: true,
        multi: false,
        checked: Vec::new(),
        details: Vec::new(),
        answer_file: None,
        snippet: String::new(),
    };
    // The option's own text, reached without spelling the separator.
    assert_eq!(
        answer_from(&question, &serde_json::json!([".env", ".env.local"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Choice(0)))
    );
    // And a list nothing offered is still an answer.
    assert_eq!(
        answer_from(&question, &serde_json::json!([".env", ".envrc"])).unwrap(),
        actions::Answer::Program(Box::new(actions::Answer::Custom(".env,.envrc".to_string())))
    );
}

// An answer nothing asked about is reported rather than dropped: a
// program that answered a question pando did not ask has to hear it.
#[test]
fn the_answers_a_run_never_used_are_the_ones_nothing_asked_about() {
    let answers = Answers::parse(r#"{"install": "npm ci", "dev_cmd": "pnpm dev"}"#).unwrap();
    let question = dev_question(&["pnpm dev"]);
    assert!(answers.for_question(&question).is_some());
    assert_eq!(answers.unasked(), vec![crate::detect::Slot::Install]);
}

/// The values `agent/json.md` offers for one enum-valued field, as it
/// spells them: the first `"key": "a|b|c"` in the document.
fn documented(doc: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\": \"");
    let value = doc
        .match_indices(&needle)
        .map(|(at, _)| {
            let rest = &doc[at + needle.len()..];
            &rest[..rest.find('"').expect("a closing quote")]
        })
        .find(|value| value.contains('|'))
        .unwrap_or_else(|| panic!("agent/json.md lists no values for `{key}`"));
    let mut values: Vec<String> = value.split('|').map(str::to_string).collect();
    values.sort();
    values
}

fn serde_word<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("serialises")
        .as_str()
        .expect("a unit variant serialises to a string")
        .to_string()
}

/// agent/json.md quotes every enum-valued field by hand, and one of those
/// strings was once wrong for as long as it shipped. This holds each list
/// to the type that prints it or, where the words are spelled by hand, to
/// what the binary prints. The matches are exhaustive on purpose: a new
/// variant does not compile here until somebody decides what the document
/// says about it.
#[test]
fn every_enum_value_agent_json_documents_is_one_the_binary_prints() {
    use crate::decisions::Shape;
    use crate::doctor::{Section, Severity};
    use crate::log_tail::LogLevel;
    use crate::state::ServiceKind;

    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md"),
    )
    .expect("read agent/json.md");
    let check = |key: &str, printed: Vec<String>| {
        let mut printed = printed;
        printed.sort();
        assert_eq!(
            documented(&doc, key),
            printed,
            "agent/json.md's `{key}` values and the binary's disagree"
        );
    };

    let sections = [
        Section::Project,
        Section::Config,
        Section::Runtime,
        Section::Tools,
        Section::Worktrees,
        Section::Services,
        Section::Hooks,
        Section::Adoption,
    ];
    for s in sections {
        match s {
            Section::Project
            | Section::Config
            | Section::Runtime
            | Section::Tools
            | Section::Worktrees
            | Section::Services
            | Section::Hooks
            | Section::Adoption => {}
        }
    }
    check("section", sections.map(serde_word).to_vec());

    let severities = [Severity::Problem, Severity::Note];
    for s in severities {
        match s {
            Severity::Problem | Severity::Note => {}
        }
    }
    check("severity", severities.map(serde_word).to_vec());

    let kinds = [ServiceKind::Compose, ServiceKind::Native];
    for k in kinds {
        match k {
            ServiceKind::Compose | ServiceKind::Native => {}
        }
    }
    // `status --json` spells the kind with a match of its own, not through
    // the enum's serde, so the words are read back from what it prints for
    // one service of each kind.
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store.worktrees.get_mut(&name).unwrap().services = kinds
        .map(|kind| crate::state::ServiceRecord {
            name: serde_word(kind),
            kind,
            port: None,
            pid: None,
            pgid: None,
            compose_project: None,
        })
        .to_vec();
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    let status: serde_json::Value =
        serde_json::from_str(&capture(|b| status_json(&fx.paths, None, b))).unwrap();
    let printed = kinds.map(|kind| {
        status["worktrees"][0]["services"][serde_word(kind)]["kind"]
            .as_str()
            .unwrap_or_else(|| panic!("status --json prints no kind for a {kind:?} service"))
            .to_string()
    });
    assert_eq!(
        printed,
        kinds.map(serde_word),
        "status --json names a kind by another word"
    );
    check("kind", printed.to_vec());

    // Every mode, and the published words for them: `status --json`,
    // `ls --json` and `doctor --json` print these, and a program reading
    // them decides by them which services a worktree is on.
    for m in crate::state::ServiceMode::ALL {
        match m {
            crate::state::ServiceMode::Shared
            | crate::state::ServiceMode::Namespaced
            | crate::state::ServiceMode::Isolated => {}
        }
    }
    check(
        "mode",
        crate::state::ServiceMode::ALL.map(serde_word).to_vec(),
    );

    let states = [PrState::Open, PrState::Merged, PrState::Closed];
    for s in states {
        match s {
            PrState::Open | PrState::Merged | PrState::Closed => {}
        }
    }
    check("state", states.map(serde_word).to_vec());

    let shapes = [Shape::Choice, Shape::Custom, Shape::Set, Shape::None];
    for s in shapes {
        match s {
            Shape::Choice | Shape::Custom | Shape::Set | Shape::None => {}
        }
    }
    check("shape", shapes.map(serde_word).to_vec());

    let levels = [
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ];
    for l in levels {
        match l {
            LogLevel::Debug | LogLevel::Info | LogLevel::Warn | LogLevel::Error => {}
        }
    }
    check("level", levels.map(|l| level_word(l).to_string()).to_vec());

    let since = Utc::now();
    let phases = [
        Phase::Starting { since },
        Phase::Running { since },
        Phase::Failed {
            at: since,
            reason: String::new(),
        },
    ];
    for p in &phases {
        match p {
            Phase::Starting { .. } | Phase::Running { .. } | Phase::Failed { .. } => {}
        }
    }
    check(
        "phase",
        phases.iter().map(|p| phase_word(p).to_string()).collect(),
    );

    // Plain strings in the report rather than an enum, so they are read
    // from a report doctor makes, as `doctor --json` prints it; doctor's
    // own tests pin the order. The shell finds nothing, so no login shell
    // runs.
    let finds_nothing = |_: &str| None;
    let report = crate::doctor::run_on(
        &fx.paths,
        &crate::actions::Machine {
            shell: &finds_nothing,
            home: fx.root.join("no-such-home"),
        },
    );
    let report = serde_json::to_value(&report).unwrap();
    let layers = report["config"]["layers"]
        .as_array()
        .expect("doctor --json lists the config layers")
        .iter()
        .map(|layer| layer["layer"].as_str().expect("a layer's name").to_string())
        .collect();
    check("layer", layers);
}

// "answer it in pando.toml" named no key and no real path. Exit 3 now
// names the absolute file (under whatever home `PANDO_HOME` names), what
// the first option would be written as there, and the answers-file line
// a program would send instead.
#[test]
fn exit_three_names_the_absolute_file_the_key_and_the_answers_file() {
    let proposal = crate::detect::Proposal::of(
        crate::detect::Slot::DevCmd,
        vec![crate::detect::Candidate {
            value: "pnpm dev".to_string(),
            why: "package.json scripts.dev".to_string(),
            ..Default::default()
        }],
        false,
    );
    let mut question = actions::question_for(&proposal, &[]);
    question.answer_file = Some(std::path::PathBuf::from(
        "/somewhere/pando-home/projects/p-1/pando.toml",
    ));
    let text = render_needs_answer(&actions::NeedsAnswer { question });
    for wanted in [
        "/somewhere/pando-home/projects/p-1/pando.toml",
        "[dev]",
        "cmd = \"pnpm dev\"",
        "pando init --answers",
        "{\"dev_cmd\": \"pnpm dev\"}",
    ] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
}

// The port question's typed answer is variable names, not a command, and
// a choice is echoed so the transcript shows what was taken.
#[test]
fn the_port_question_asks_for_variable_names_and_echoes_the_choice() {
    let mut question = dev_question(&["WEB_PORT", "PORT"]);
    question.slot = crate::detect::Slot::PortEnv;
    let (answer, printed) = answer_with(&question, &["2"]);
    assert_eq!(answer.unwrap(), actions::Answer::Choice(1));
    assert!(printed.contains("type the variable names"), "{printed}");
    assert!(printed.contains("→ PORT"), "{printed}");
}

// A CJK branch takes two columns a character; sized by characters, its
// row pushed STATUS and everything after it out of line.
#[test]
fn a_wide_branch_name_keeps_the_ls_columns_straight() {
    let fx = fixture();
    actions::new(
        &fx.paths,
        &fx.config,
        "feat/日本語のブランチ",
        None,
        &|_| {},
    )
    .unwrap();
    actions::new(&fx.paths, &fx.config, "feat/ascii-branch", None, &|_| {}).unwrap();
    let text = capture(|b| ls_text_at(&fx.paths, b, usize::MAX));
    let status_column = |needle: &str| {
        let line = text.lines().find(|l| l.contains(needle)).unwrap();
        let at = line.find("stopped").unwrap();
        crate::term::text_width(&line[..at])
    };
    assert_eq!(
        status_column("日本語"),
        status_column("ascii-branch"),
        "{text}"
    );
}

// ---- the namespace login question -----------------------------------------------

fn login_question_for_tests() -> actions::Question {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    crate::testutil::init_repo(&root);
    let paths = PandoPaths::new(
        dir.path().join("pando-home"),
        ProjectRef::from_root(&root).unwrap(),
    );
    actions::login_question(&paths, "mariadb", &["DATABASE_PORT".to_string()])
}

// Nothing to choose: the prompt asks for the login outright, says it is
// not shown, and takes the whole line — colons and all — as the answer.
#[test]
fn the_login_question_asks_for_a_login_that_is_not_shown_as_it_is_typed() {
    let question = login_question_for_tests();
    let (answer, printed) = answer_with(&question, &["root:p@ss:word"]);
    assert_eq!(
        answer.unwrap(),
        actions::Answer::Custom("root:p@ss:word".to_string())
    );
    assert!(printed.contains("not shown as you type"), "{printed}");
    assert!(printed.contains("user:password"), "{printed}");
    assert!(!printed.contains("c) something else"), "{printed}");
    assert!(
        !printed.contains("p@ss"),
        "the prompt never prints it back: {printed}"
    );
}

// A script gets exit 3 with the table to write and the file to write it
// in — and no answers-file line, because `init` never asks this one.
#[test]
fn the_login_question_at_exit_3_names_the_table_to_write_and_not_an_answers_file() {
    let question = login_question_for_tests();
    let file = question.answer_file.clone().unwrap();
    let text = render_needs_answer(&actions::NeedsAnswer { question });
    for wanted in [
        "[namespaced.mariadb]",
        "user = \"<user>\"",
        "password = \"<password>\"",
        file.to_str().unwrap(),
    ] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    assert!(!text.contains("init --answers"), "{text}");
}

// A namespace login lives in pando's own config; nothing `status` prints
// reads it, and nothing it prints may carry it.
//
// The worktree runs namespaced and its config declares a service pando
// cannot reach, so the one path in `status` that reads the config — the
// namespace lines, and the shared service's reason among them — runs.
#[test]
fn status_never_prints_a_namespace_login() {
    use crate::state::ServiceMode;
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    std::fs::write(
        fx.paths.config_file(),
        "[[services]]\nkind = \"native\"\nname = \"mariadb\"\n\n\
         [namespaced.mariadb]\nuser = \"root\"\npassword = \"hunter2\"\n",
    )
    .unwrap();
    with_namespaces(&fx, &name, ServiceMode::Namespaced);
    let json = capture(|b| status_json(&fx.paths, None, b));
    let text = capture(|b| status_text_at(&fx.paths, None, b, usize::MAX));
    assert!(
        text.lines()
            .any(|l| l.contains("mariadb") && l.contains("shared")),
        "the namespace lines ran with the config: {text}"
    );
    for shown in [&json, &text] {
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}

#[test]
fn an_answers_file_cannot_answer_the_login_a_namespaced_start_asks_for() {
    let e = crate::cli::answers::Answers::parse(r#"{"login": "root:hunter2"}"#).unwrap_err();
    let e = format!("{e:#}");
    assert!(e.contains("not a question pando asks"), "{e}");
    assert!(!e.contains("hunter2"), "{e}");
}

// A slot to free has nothing to write down and nothing `--yes` may take:
// a script is told how a person answers it, and what it can do instead.
#[test]
fn the_slot_question_at_exit_3_says_how_it_is_answered_and_offers_no_flag() {
    let question = actions::Question {
        slot: crate::detect::Slot::FreeSlot,
        prompt: "Every slot of redis on 127.0.0.1:6379 is held. Which stopped worktree gives up \
                 its slot?"
            .into(),
        options: vec![("feat+old".into(), "slot 3, last ran 4 days ago".into())],
        preselect: None,
        allow_custom: false,
        allow_none: true,
        multi: false,
        checked: Vec::new(),
        details: vec!["the one chosen has its slot emptied".into()],
        answer_file: None,
        snippet: String::new(),
    };
    assert!(actions::recommended(&question).is_none());
    let text = render_needs_answer(&actions::NeedsAnswer {
        question: question.clone(),
    });
    for wanted in ["feat+old", "slot 3, last ran 4 days ago", "pando rm"] {
        assert!(text.contains(wanted), "{wanted}: {text}");
    }
    for unwanted in ["--yes", "init --answers", "pando.toml"] {
        assert!(!text.contains(unwanted), "{unwanted}: {text}");
    }
    let (answer, printed) = answer_with(&question, &["n"]);
    assert_eq!(answer.unwrap(), actions::Answer::None);
    assert!(printed.contains("free nothing"), "{printed}");
}

// ---- namespaces in status ---------------------------------------------------------

fn with_namespaces(fx: &Fx, name: &str, mode: crate::state::ServiceMode) {
    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    let record = store.worktrees.get_mut(name).unwrap();
    record.mode = Some(mode);
    for (service, kind, ns, host, port) in [
        (
            "mariadb",
            crate::state::NamespaceKind::Database,
            "shop__feat_one",
            "localhost",
            3306,
        ),
        (
            "redis",
            crate::state::NamespaceKind::Slot,
            "3",
            "127.0.0.1",
            6379,
        ),
    ] {
        record.namespaces.push(crate::state::NamespaceRecord {
            service: service.into(),
            recipe: service.into(),
            kind,
            host: host.into(),
            port,
            name: ns.into(),
            main: "shop".into(),
            mains: Vec::new(),
            keys: Vec::new(),
            used_at: Utc::now(),
        });
    }
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
}

// Each namespace a worktree holds is in `status --json`, database by name
// and slot by number, on the server it was made on — and whether the
// worktree runs on it now or keeps it for the way back.
#[test]
fn status_json_lists_the_namespaces_a_worktree_holds() {
    use crate::state::ServiceMode;
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_namespaces(&fx, &name, ServiceMode::Namespaced);
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let namespaces = &v["worktrees"][0]["namespaces"];
    assert_eq!(namespaces[0]["service"], "mariadb");
    assert_eq!(namespaces[0]["database"], "shop__feat_one");
    assert!(namespaces[0].get("slot").is_none());
    assert_eq!(namespaces[0]["host"], "localhost");
    assert_eq!(namespaces[0]["port"], 3306);
    assert_eq!(namespaces[0]["in_use"], true);
    assert_eq!(namespaces[1]["slot"], 3);
    assert!(namespaces[1].get("database").is_none());

    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store.worktrees.get_mut(&name).unwrap().mode = Some(ServiceMode::Shared);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    let text = capture(|b| status_json(&fx.paths, None, b));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["worktrees"][0]["namespaces"][0]["in_use"], false);

    // Every field is in the contract, and the contract says them.
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/json.md"),
    )
    .unwrap();
    for field in ["\"namespaces\"", "\"database\"", "\"slot\"", "\"in_use\""] {
        assert!(doc.contains(field), "agent/json.md never mentions {field}");
    }
}

// `status` says per service what the worktree holds: its own database and
// slot while it runs namespaced, and the same, kept until `rm`, once it
// runs in another mode.
#[test]
fn status_text_says_what_a_worktree_holds_in_each_service() {
    use crate::state::ServiceMode;
    let fx = fixture();
    let name = actions::new(&fx.paths, &fx.config, "feat/one", None, &|_| {}).unwrap();
    with_namespaces(&fx, &name, ServiceMode::Namespaced);
    let text = capture(|b| status_text_at(&fx.paths, None, b, usize::MAX));
    assert!(
        text.lines().any(|l| l.contains("mariadb")
            && l.contains("own")
            && l.contains("database shop__feat_one on localhost:3306")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("redis") && l.contains("slot 3 on 127.0.0.1:6379")),
        "{text}"
    );

    let mut store = crate::state::load(&fx.paths.state_file()).unwrap();
    store.worktrees.get_mut(&name).unwrap().mode = Some(ServiceMode::Shared);
    crate::state::save(&fx.paths.state_file(), &store).unwrap();
    let text = capture(|b| status_text_at(&fx.paths, None, b, usize::MAX));
    assert!(
        text.lines()
            .any(|l| l.contains("kept") && l.contains("shop__feat_one") && l.contains("until rm")),
        "{text}"
    );
}

#[test]
fn three_flags_mean_three_modes_and_none_is_the_remembered_one() {
    use actions::Mode;
    assert_eq!(Mode::of(false, false, false), Mode::Remembered);
    assert_eq!(Mode::of(true, false, false), Mode::Isolated);
    assert_eq!(Mode::of(false, true, false), Mode::Namespaced);
    assert_eq!(Mode::of(false, false, true), Mode::Shared);
}
