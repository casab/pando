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
            let chosen = proposal.preferred_set();
            match proposal.mechanism {
                // A native answer is one entry per service; a compose
                // answer is one entry naming the file, which is why the
                // empty set still writes something there and nothing here.
                Some("native") => detect::apply_native_services(&chosen, &mut config),
                _ => {
                    if let Some(file) = proposal.service_file() {
                        detect::apply_services(file, &chosen, &mut config);
                    }
                }
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
    // The schema step is the one question the common case still has: it
    // touches data, so it is asked even with a single candidate.
    assert_eq!(
        asked.iter().map(|p| p.slot).collect::<Vec<_>>(),
        vec![Slot::SchemaHook],
        "the common case must need no other answers"
    );
}

// Non-JavaScript, with the port on the command line rather than in the
// environment — which is why `{port:web}` exists.
#[test]
fn a_python_project_gets_its_port_on_the_command_line() {
    let (_dir, root) = fixture(Kind::DjangoUvPostgres);
    let (config, asked) = detected(&root);
    assert_eq!(config, Kind::DjangoUvPostgres.expected_config());
    assert!(
        asked.iter().all(|p| p.slot == Slot::SchemaHook),
        "only the schema step is a question: {asked:?}"
    );
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
            "**/migrations/*.py",
        ),
    ] {
        let (_dir, root) = fixture(kind);
        let signals = detect::signals(&root);
        let hook = detect::propose(&root, &signals)
            .into_iter()
            .find(|p| p.slot == Slot::SchemaHook)
            .unwrap_or_else(|| panic!("{} has a schema step", kind.dir_name()));
        // Always a question: it is the one that touches data.
        assert!(!hook.decided, "{}", kind.dir_name());
        let candidate = hook.preferred().unwrap();
        assert_eq!(candidate.value, cmd);
        let hook = candidate.hook.as_ref().unwrap();
        assert_eq!(hook.name, "migrate");
        assert_eq!(hook.after, pando::config::HookPoint::Services);
        assert_eq!(hook.fingerprint, vec![glob.to_string()]);
        // Written explicitly, so the entry shows how to change it, and
        // isolated: a shared start never migrates the shared database.
        assert_eq!(hook.on, Some(pando::config::HookScope::Isolated));
        assert!(!hook.runs_on(false, true) && hook.runs_on(true, true));
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

// Signals carry what a version file *says*, not only that it exists: that
// is the fact `signals --json` publishes and the one the runtime check
// compares a machine against.
#[test]
fn signals_carry_the_runtime_a_fixture_asks_for() {
    for (kind, language, spec, source) in [
        (Kind::NextPnpmCompose, "node", "22", ".nvmrc"),
        (Kind::DjangoUvPostgres, "python", "3.12", ".python-version"),
    ] {
        let (_dir, root) = fixture(kind);
        let signals = detect::signals(&root);
        let requirement = pando::runtime::for_language(&signals.runtime_requirements, language)
            .unwrap_or_else(|| panic!("{kind:?} pins {language}: {signals:?}"));
        assert_eq!(requirement.spec, spec, "{kind:?}");
        assert_eq!(requirement.source, source, "{kind:?}");
        assert!(requirement.pinned, "{kind:?} states a pin");
        assert_eq!(
            signals.version_files,
            vec![source.to_string()],
            "the file list still says which files exist"
        );
    }

    // And a fixture that pins nothing asks for nothing.
    let (_dir, root) = fixture(Kind::GoService);
    assert!(
        detect::signals(&root).runtime_requirements.is_empty(),
        "a Go service with no version file states no requirement"
    );
}

// ---- the hard shapes -----------------------------------------------------
//
// The corpus *is* pando's validation: real repositories are off limits
// without exception, and a corpus of tidy shapes validates nothing,
// because tidy is not what a first run meets. What `expected_config`
// cannot express is the other half of an answer — which slots these
// shapes leave as a *question* — so it is asserted here.

#[test]
fn each_hard_shape_asks_exactly_what_it_should() {
    for (kind, expected) in [
        // Several apps and no workspace file: whether this is one
        // process or one per app is the developer's call, not a rule's.
        (Kind::WorkspaceNoLock, vec![Slot::Processes]),
        // Two app ports named in the env example, and a dev script
        // that takes neither on the command line. Which of them the one
        // process serves on — or whether it serves on both — is a
        // question, and the pre-ticked answer is both.
        (Kind::EnvPorts, vec![Slot::PortEnv]),
        // One compose service, built from this repository. There is
        // nothing to offer and nothing to ask.
        (Kind::ComposeAppOnly, vec![]),
        (Kind::PinnedRuntime, vec![]),
        // The addresses name the engines, so the recipes are decided.
        (Kind::ServicesNoManifest, vec![]),
        // And the hybrid of those two: the compose file offers nothing,
        // the env example's address names the engine, and the recipe is
        // decided the same way.
        (Kind::ComposeAppAndDatabase, vec![]),
        // Seeding a worktree's `.env` from an example is a copy of a
        // tracked file into a worktree, which is a write nobody has
        // authorised yet — so it is asked rather than taken, and `--yes`
        // declines it.
        (Kind::EnvNeverArrived, vec![Slot::Provision]),
    ] {
        let (_dir, root) = fixture(kind);
        let (_, asked) = detected(&root);
        let slots: Vec<Slot> = asked.iter().map(|p| p.slot).collect();
        assert_eq!(slots, expected, "{} asked the wrong slots", kind.dir_name());
    }
}

// A project with no lockfile has no frozen install to propose, and pando
// never proposes a non-frozen one. Silence is the answer, not a guess.
#[test]
fn a_workspace_with_no_lockfile_proposes_no_install_at_all() {
    let (_dir, root) = fixture(Kind::WorkspaceNoLock);
    let signals = detect::signals(&root);
    assert!(signals.lockfiles.is_empty(), "{:?}", signals.lockfiles);
    let (config, _) = detected(&root);
    assert_eq!(config.project.install, None);
    assert!(
        detect::propose(&root, &signals)
            .iter()
            .all(|p| p.slot != Slot::Install),
        "an install was proposed with no lockfile to freeze"
    );
}

// The build-from-this-repository filter, at the only shape where it is
// the whole answer: a compose file that packages the application and
// nothing else. The negative is recorded so the question does not come
// back on every isolated start.
#[test]
fn a_compose_file_that_only_packages_the_app_records_the_negative() {
    let (_dir, root) = fixture(Kind::ComposeAppOnly);
    let signals = detect::signals(&root);
    let services = detect::propose(&root, &signals)
        .into_iter()
        .find(|p| p.slot == Slot::Services)
        .expect("a services proposal");
    assert!(services.decided, "there is nothing here to ask about");
    assert!(services.candidates.is_empty());
    assert_eq!(services.service_file(), Some("docker-compose.yml"));
    assert!(
        services
            .none_because
            .as_deref()
            .unwrap_or_default()
            .contains("built from this repository"),
        "{:?}",
        services.none_because
    );
}

// The shape Phase 6 exists for. No compose file means there is no
// container option to weigh, so the preference never comes into it and
// the engines come from what the app's own addresses say.
#[test]
fn a_project_that_needs_services_with_no_manifest_gets_recipes() {
    let (_dir, root) = fixture(Kind::ServicesNoManifest);
    let signals = detect::signals(&root);
    let services = detect::propose(&root, &signals)
        .into_iter()
        .find(|p| p.slot == Slot::Services)
        .expect("a services proposal");
    assert_eq!(services.mechanism, Some("native"));
    let names: Vec<&str> = services
        .candidates
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(names, vec!["postgres", "redis"]);
    let evidence = services.evidence.join(" | ");
    assert!(evidence.contains("no compose file"), "{evidence}");
    assert!(evidence.contains("postgres and redis"), "{evidence}");
}

// The hybrid the two shapes above only cover half of each: a compose
// file *and* a database, where the compose file is not a container
// option because the one service in it is the application.
//
// This is the branch of `service_choice` where `compose_declared` is
// false and `native_declared` is true while a compose file is sitting
// right there — the case in which reading "there is a compose file, so
// run the services in containers" gets the whole project wrong.
#[test]
fn a_compose_file_that_packages_the_app_still_leaves_the_database_native() {
    let (_dir, root) = fixture(Kind::ComposeAppAndDatabase);
    let signals = detect::signals(&root);
    assert_eq!(
        signals.compose_files,
        vec!["docker-compose.yml".to_string()],
        "the shape is only interesting while the compose file is really there"
    );
    let services = detect::propose(&root, &signals)
        .into_iter()
        .find(|p| p.slot == Slot::Services)
        .expect("a services proposal");
    assert_eq!(
        services.mechanism,
        Some("native"),
        "a compose file that packages the app is not a container option"
    );
    let names: Vec<&str> = services
        .candidates
        .iter()
        .map(|c| c.value.as_str())
        .collect();
    assert_eq!(names, vec!["postgres"]);
    assert!(
        services.decided,
        "the app's own address names the engine, so there is nothing to ask"
    );
    let evidence = services.evidence.join(" | ");
    assert!(
        evidence.contains("docker-compose.yml declares nothing this project depends on"),
        "the evidence has to say which mechanism was weighed and dropped: {evidence}"
    );
    assert!(
        evidence.contains("env example addresses postgres"),
        "{evidence}"
    );
    // And no preference was consulted: there was never a tie to break.
    assert!(
        !evidence.contains("prefer"),
        "one option is not a choice, and saying so is shorter: {evidence}"
    );
}

// A pin no machine resolves, and an `engines` range that disagrees with
// it. Both are recorded, the pin first, which is what "a pinned file
// beats a range" means where it matters.
#[test]
fn a_pinned_runtime_and_a_range_that_disagrees_are_both_recorded() {
    let (_dir, root) = fixture(Kind::PinnedRuntime);
    let signals = detect::signals(&root);
    let node: Vec<&pando::runtime::Requirement> = signals
        .runtime_requirements
        .iter()
        .filter(|r| r.language == "node")
        .collect();
    assert_eq!(node.len(), 2, "{node:?}");
    assert_eq!(node[0].spec, "99.0.0", "the pin sorts first");
    assert!(node[0].pinned);
    assert_eq!(node[1].spec, ">=18 <21");
    assert!(!node[1].pinned);
}
