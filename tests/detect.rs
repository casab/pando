//! Tier 1 detection against the fixture catalogue.
//!
//! Each fixture's expected config was written by hand from the fixture
//! notes before detection existed, so these tests compare what the rules
//! find against what a human decided the answer is — not against the rules'
//! own output.

mod common;

use common::{Kind, build};
use pando::config::Config;
use pando::detect::{self, Proposal, Slot};
use std::path::Path;
use tempfile::TempDir;

/// What the rules would write if every proposal were taken at its
/// preferred candidate, plus the questions that would be asked on the way.
fn detected(root: &Path) -> (Config, Vec<Proposal>) {
    let signals = detect::signals(root);
    let proposals = detect::propose(root, &signals);
    let mut config = Config::default();
    let mut asked = Vec::new();
    // The same two gates `actions::resolve` applies, in the same order:
    // this helper is what the resolver does with the answers taken at
    // their preferred candidate, so it has to make the same decisions.
    let mut may_fill_dev = detect::may_fill_dev(&config);
    for proposal in &proposals {
        if matches!(proposal.slot, Slot::DevCmd | Slot::PortEnv) && !may_fill_dev {
            continue;
        }
        if !detect::still_needed(proposal.slot, &config) {
            continue;
        }
        // The one slot whose answer is a set: taking it at its
        // preselection is what `--yes` does, and what a developer who
        // presses enter on the pre-ticked boxes does.
        if proposal.slot.is_multi() {
            if !proposal.decided {
                asked.push(proposal.clone());
            }
            if let Some(file) = proposal.service_file() {
                detect::apply_services(file, &proposal.preferred_set(), &mut config);
            }
            continue;
        }
        let Some(candidate) = proposal.preferred() else {
            continue;
        };
        if !proposal.decided {
            asked.push(proposal.clone());
        }
        detect::apply(proposal.slot, candidate, &mut config);
        if proposal.slot == Slot::Processes {
            may_fill_dev = detect::fills_one_dev_process(&config);
        }
    }
    (config, asked)
}

fn fixture(kind: Kind) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = build(kind, dir.path()).root;
    (dir, root)
}

#[test]
fn the_common_case_resolves_every_slot_with_no_question() {
    let (_dir, root) = fixture(Kind::NextPnpmCompose);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::NextPnpmCompose.expected_config());
    assert!(
        asked.is_empty(),
        "the common case must need no answers: {:?}",
        asked.iter().map(|p| p.slot).collect::<Vec<_>>()
    );
}

// Non-JavaScript, with the port on the command line rather than in the
// environment — which is why `{port:web}` exists.
#[test]
fn a_python_project_gets_its_port_on_the_command_line() {
    let (_dir, root) = fixture(Kind::DjangoUvPostgres);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::DjangoUvPostgres.expected_config());
    assert!(asked.is_empty(), "{asked:?}");
    assert!(
        config.processes["dev"].cmd.contains("{port:web}"),
        "the port has to reach the process somehow"
    );
}

#[test]
fn a_go_service_needs_no_install_step() {
    let (_dir, root) = fixture(Kind::GoService);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::GoService.expected_config());
    assert!(asked.is_empty(), "{asked:?}");
    assert_eq!(
        config.project.install, None,
        "`go run` resolves its own modules; a warm-up step would be noise"
    );
}

// Level zero: pando is useful on a repository with no dev server at all,
// and must not invent one.
#[test]
fn a_library_gets_no_process_and_no_question() {
    let (_dir, root) = fixture(Kind::RustLib);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::RustLib.expected_config());
    assert_eq!(config, Config::default());
    assert!(config.processes.is_empty());
    assert!(asked.is_empty(), "{asked:?}");
}

#[test]
fn an_ambiguous_project_asks_about_its_command_its_port_and_its_services() {
    let (_dir, root) = fixture(Kind::NextMessy);
    let (config, asked) = detected(&root);
    let slots: Vec<Slot> = asked.iter().map(|p| p.slot).collect();
    assert_eq!(
        slots,
        vec![Slot::DevCmd, Slot::PortEnv, Slot::Services],
        "the command, the port, and which services to run are what rules cannot settle here"
    );

    let dev = &asked[0];
    assert_eq!(
        dev.preferred().unwrap().value,
        "pnpm dev",
        "the script named exactly dev is preselected"
    );
    let offered: Vec<&str> = dev.candidates.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(
        offered,
        vec![
            "pnpm dev",
            "pnpm dev:all",
            "pnpm dev:web",
            "pnpm dev:worker"
        ],
        "build, lint, preview and the production start and serve scripts are not dev servers"
    );

    let port = &asked[1];
    assert_eq!(port.preferred().unwrap().value, "PORT");
    let offered: Vec<&str> = port.candidates.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(
        offered,
        vec!["PORT", "API_PORT", "VITE_PORT"],
        "DB_PORT and SMTP_PORT are services' ports, never the web server's"
    );

    // `db` and `mail` resolve by rule — a postgres image behind `DB_PORT`,
    // a mail catcher behind `SMTP_PORT`. `cache` and `queue` are services
    // an app talks to with nothing in the env example naming them, which
    // is exactly the case pando cannot settle and has to ask about.
    let services = &asked[2];
    let offered: Vec<&str> = services
        .candidates
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(offered, vec!["cache", "db", "mail", "queue"]);
    assert_eq!(
        services
            .preferred_set()
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>(),
        vec!["db", "mail"],
        "only the two a rule resolved start ticked"
    );
    assert_eq!(services.preselected(), vec![1, 2]);

    // Answering with the preselections lands on the same config as the
    // unambiguous fixture, which is the point of the preselections.
    assert_eq!(config, Kind::NextMessy.expected_config());
}

// Fixture 1: every service resolved, so nothing is asked and `mailpit` is
// simply left out — it is a mail catcher, not something the app addresses.
#[test]
fn a_project_whose_services_all_resolve_is_never_asked_about_them() {
    let (_dir, root) = fixture(Kind::NextPnpmCompose);
    let signals = detect::signals(&root);
    let services = detect::propose(&root, &signals)
        .into_iter()
        .find(|p| p.slot == Slot::Services)
        .expect("a services proposal");
    assert!(services.decided);
    let offered: Vec<&str> = services
        .candidates
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(offered, vec!["mailpit", "postgres", "redis"]);
    assert_eq!(
        services
            .preferred_set()
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>(),
        vec!["postgres", "redis"]
    );
    assert!(
        services.candidates[0]
            .why
            .contains("nothing in the env example"),
        "and it says why mailpit is unticked: {}",
        services.candidates[0].why
    );
}

// A project with no compose file has nothing to propose, and an isolated
// start on it runs shared rather than failing.
#[test]
fn a_project_with_no_compose_file_proposes_no_services() {
    for kind in [Kind::GoService, Kind::RustLib, Kind::Plain] {
        let (_dir, root) = fixture(kind);
        let signals = detect::signals(&root);
        assert!(
            detect::propose(&root, &signals)
                .iter()
                .all(|p| p.slot != Slot::Services && p.slot != Slot::SchemaHook),
            "{} has no services and no schema step",
            kind.dir_name()
        );
    }
}

// The schema hook is keyed on the files whose change means it has to run
// again, because a hook without them runs on every start.
#[test]
fn a_schema_hook_carries_the_files_it_is_keyed_on() {
    for (kind, cmd, glob) in [
        (
            Kind::NextPnpmCompose,
            "pnpm prisma migrate deploy",
            "prisma/migrations/**",
        ),
        (
            Kind::DjangoUvPostgres,
            "uv run python manage.py migrate",
            "*/migrations/*.py",
        ),
    ] {
        let (_dir, root) = fixture(kind);
        let signals = detect::signals(&root);
        let hook = detect::propose(&root, &signals)
            .into_iter()
            .find(|p| p.slot == Slot::SchemaHook)
            .unwrap_or_else(|| panic!("{} has a schema step", kind.dir_name()));
        assert!(hook.decided, "{}", kind.dir_name());
        let candidate = hook.preferred().unwrap();
        assert_eq!(candidate.value, cmd);
        let hook = candidate.hook.as_ref().unwrap();
        assert_eq!(hook.name, "migrate");
        assert_eq!(hook.after, pando::config::HookPoint::Services);
        assert_eq!(hook.fingerprint, vec![glob.to_string()]);
    }
}

// The rule that makes the ambiguous fixture ask: a `dev` script that is
// really one dev server is taken silently, and one that fans out to several
// is not.
#[test]
fn a_dev_script_that_runs_one_server_is_taken_without_asking() {
    let (_dir, root) = fixture(Kind::NextPnpmCompose);
    let signals = detect::signals(&root);
    let dev = detect::propose(&root, &signals)
        .into_iter()
        .find(|p| p.slot == Slot::DevCmd)
        .expect("a dev command");
    assert!(dev.decided);
    assert_eq!(dev.preferred().unwrap().value, "pnpm dev");
}

// Phase 2b: the workspace fixture proposes the two-process form, with one
// confirmation, and everything `fixtures.md` says it should hold.
#[test]
fn a_workspace_proposes_a_process_per_app_after_one_question() {
    let (_dir, root) = fixture(Kind::MonoWebApi);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::MonoWebApi.expected_config());

    let slots: Vec<Slot> = asked.iter().map(|p| p.slot).collect();
    assert_eq!(
        slots,
        vec![Slot::Processes],
        "one confirmation, not one question per app"
    );
    let offered: Vec<&str> = asked[0]
        .candidates
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(offered.len(), 2, "the per-app form, or the root script");
    assert_eq!(
        offered[1], "pnpm dev",
        "declining falls back to the root script"
    );
    assert_eq!(asked[0].preferred().unwrap().value, offered[0]);

    // The whole point of the shape: one process is told the other's port.
    assert_eq!(
        config.processes["web"].env["VITE_API_URL"],
        "http://localhost:{port:api}"
    );
    assert_eq!(config.processes["api"].env["PORT"], "{port:api}");
    assert_eq!(config.processes["web"].cwd.as_deref(), Some("apps/web"));
    assert_eq!(config.processes["api"].cwd.as_deref(), Some("apps/api"));
}

// Fixture 6 has a `concurrently` wrapper and no workspace behind it: the
// per-app form is only offered when there are per-app manifests to offer.
#[test]
fn a_wrapper_script_with_no_workspace_is_still_one_process() {
    for kind in [Kind::NextMessy, Kind::NextPnpmCompose] {
        let (_dir, root) = fixture(kind);
        let signals = detect::signals(&root);
        assert!(
            detect::propose(&root, &signals)
                .iter()
                .all(|p| p.slot != Slot::Processes),
            "{} is not a workspace",
            kind.dir_name()
        );
        assert!(detect::workspace_apps(&root, &signals).is_empty());
    }
}

#[test]
fn provision_finds_the_local_files_a_new_worktree_would_be_missing() {
    let (_dir, root) = fixture(Kind::NextPnpmCompose);
    let signals = detect::signals(&root);
    assert_eq!(signals.ignored_present, vec![".env", ".env.local"]);
    // An ignored directory is build output, not something to link.
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(root.join("node_modules/pkg/index.js"), "x").unwrap();
    let signals = detect::signals(&root);
    assert_eq!(
        signals.ignored_present,
        vec![".env", ".env.local"],
        "a worktree builds its own dependency tree"
    );
}

#[test]
fn every_fixture_kind_detects_what_its_notes_say_it_should() {
    for kind in Kind::ALL {
        let (_dir, root) = fixture(kind);
        let (config, _) = detected(&root);
        assert_eq!(
            config,
            kind.expected_config(),
            "{} detected the wrong config",
            kind.dir_name()
        );
    }
}
