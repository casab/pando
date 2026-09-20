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
    for proposal in &proposals {
        if !detect::still_needed(proposal.slot, &config) {
            continue;
        }
        let Some(candidate) = proposal.preferred() else {
            continue;
        };
        if !proposal.decided {
            asked.push(proposal.clone());
        }
        detect::apply(proposal.slot, candidate, &mut config);
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
fn an_ambiguous_project_asks_exactly_two_questions() {
    let (_dir, root) = fixture(Kind::NextMessy);
    let (config, asked) = detected(&root);
    let slots: Vec<Slot> = asked.iter().map(|p| p.slot).collect();
    assert_eq!(
        slots,
        vec![Slot::DevCmd, Slot::PortEnv],
        "the command and the port are what rules cannot settle here"
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

    // Answering with the preselections lands on the same config as the
    // unambiguous fixture, which is the point of the preselections.
    assert_eq!(config, Kind::NextMessy.expected_config());
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
