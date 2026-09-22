use super::*;
use super::{dialogs::*, log_view::*, operations::*, pending::*, tails::*};
use crate::actions;
use crate::config::Config;
use crate::log_tail::{LogLevel, LogTail};
use crate::paths::PandoPaths;
use crate::project::ProjectRef;
use crate::state::Phase;
use crate::state::State;
use crate::worktree::BranchSource;
use crate::worktree::{BranchEntry, Worktree};
use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub fn wt(name: &str) -> Worktree {
    Worktree {
        name: name.to_string(),
        path: PathBuf::from("/trees").join(name),
        head: Some("abc123".into()),
        branch: Some(name.replace('+', "/")),
        detached: false,
        prunable: false,
        prunable_reason: None,
        locked: false,
        lock_reason: None,
        bare: false,
        created_at: None,
        head_sha: Some("abc1234".into()),
        head_subject: Some("do the thing".into()),
        head_age: Some("2 hours ago".into()),
        dirty: Some(false),
        ahead_behind: Some((1, 0)),
    }
}

pub fn test_app(names: &[&str]) -> App {
    // A path that cannot exist: these tests drive the app's own logic,
    // and a worker thread that wandered into a real repository would be
    // exactly the thing the testing policy forbids.
    let paths = PandoPaths::new(
        "/pando-test-does-not-exist/home",
        ProjectRef {
            id: "acme-shop-3f9a2c1d".into(),
            root: PathBuf::from("/pando-test-does-not-exist/acme-shop"),
            display_name: "acme-shop".into(),
        },
    );
    let worktrees: Vec<Worktree> = names.iter().map(|n| wt(n)).collect();
    let mut app = App::new_for_test(paths, Config::default(), worktrees);
    app.created_by_pando = names.iter().map(|n| (n.to_string(), true)).collect();
    app
}

use crate::state::{ProcessRecord, WorktreeRecord};
use chrono::Utc;

/// Gives a worktree a process in `phase`, as a refresh would have.
pub fn with_process(app: &mut App, name: &str, phase: Phase) {
    let mut record = WorktreeRecord::new(format!("/trees/{name}"), true);
    record.ports.insert("web".to_string(), 17_342);
    record
        .roles
        .insert("dev".to_string(), vec!["web".to_string()]);
    record.processes.insert(
        "dev".to_string(),
        ProcessRecord {
            pid: 4242,
            pgid: 4242,
            started_at: Utc::now(),
            log_path: PathBuf::from("/does/not/exist/dev.log"),
            ready_port: Some(17_342),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase,
        },
    );
    app.state.worktrees.insert(name.to_string(), record);
}

/// Gives a worktree a second process, as a workspace start would have.
pub fn with_second_process(app: &mut App, name: &str, process: &str, phase: Phase) {
    let record = app
        .state
        .worktrees
        .get_mut(name)
        .expect("the worktree has a record");
    let port = 17_343 + record.processes.len() as u16;
    record.ports.insert(process.to_string(), port);
    record
        .roles
        .insert(process.to_string(), vec![process.to_string()]);
    record.processes.insert(
        process.to_string(),
        ProcessRecord {
            pid: 4343,
            pgid: 4343,
            started_at: Utc::now(),
            log_path: PathBuf::from(format!("/does/not/exist/{process}.log")),
            ready_port: Some(port),
            ready_timeout_s: None,
            observed_ports: Vec::new(),
            swept: false,
            phase,
        },
    );
}

/// Gives a worktree a live share, as a `share` would have.
pub fn with_share(app: &mut App, name: &str, proxy_port: Option<u16>) {
    let record = app
        .state
        .worktrees
        .get_mut(name)
        .expect("the worktree has a record");
    record.share_port = proxy_port;
    record.share = Some(crate::state::ShareRecord {
        tunnel_pid: 5151,
        tunnel_pgid: 5151,
        public_url: "https://fake-host.trycloudflare.com".to_string(),
        local_port: 17_342,
        started_at: Utc::now(),
        log_path: PathBuf::from("/does/not/exist/tunnel.log"),
        proxy_pid: proxy_port.map(|_| 5252),
        proxy_pgid: proxy_port.map(|_| 5252),
        proxy_port,
    });
}

pub fn running_phase() -> Phase {
    Phase::Running { since: Utc::now() }
}

fn a_question() -> actions::Question {
    actions::Question {
        slot: crate::detect::Slot::DevCmd,
        prompt: "Which command starts the local development server?".to_string(),
        options: vec![
            (
                "pnpm dev".to_string(),
                "package.json scripts.dev".to_string(),
            ),
            (
                "pnpm dev:web".to_string(),
                "package.json scripts.dev:web".to_string(),
            ),
        ],
        preselect: Some(0),
        allow_custom: true,
        allow_none: false,
        multi: false,
        checked: Vec::new(),
        details: Vec::new(),
    }
}

/// Opens the question modal the way a worker would, and hands back the
/// end of the channel that worker would be blocked on.
fn open_question(
    app: &mut App,
    question: actions::Question,
) -> Receiver<Result<actions::Answer, String>> {
    let (tx, rx) = mpsc::channel();
    app.handle_event(AppEvent::AskQuestion(Box::new((question, tx))));
    rx
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

/// The services question, as detection on fixture 6 would raise it.
fn a_services_question() -> actions::Question {
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
    }
}

// ---- the multi-select modal ------------------------------------------

#[test]
fn a_set_question_opens_with_the_rules_answer_ticked() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_services_question());
    assert_eq!(app.question_checked, vec![1, 2]);
    assert!(
        matches!(app.modal, Some(Modal::Question { custom: None, .. })),
        "a set question has nothing to type"
    );
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        rx.try_recv().unwrap(),
        Ok(actions::Answer::Many(vec![1, 2]))
    );
}

#[test]
fn space_ticks_the_row_under_the_cursor_and_enter_takes_the_set() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_services_question());
    // The cursor starts on `cache`; tick it, then move to `db` and
    // untick that.
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(app.question_checked, vec![0, 2]);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        rx.try_recv().unwrap(),
        Ok(actions::Answer::Many(vec![0, 2]))
    );
}

#[test]
fn an_empty_set_is_the_answer_none() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_services_question());
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(rx.try_recv().unwrap(), Ok(actions::Answer::None));
}

#[test]
fn escape_on_a_set_question_still_answers_the_worker() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_services_question());
    press(&mut app, KeyCode::Esc);
    assert!(
        rx.try_recv().unwrap().is_err(),
        "a modal that closes without sending leaves a worker waiting forever"
    );
    assert!(app.modal.is_none());
}

#[test]
fn the_cursor_of_a_set_question_clamps_at_both_ends() {
    let mut app = test_app(&["feat+one"]);
    let _rx = open_question(&mut app, a_services_question());
    for _ in 0..8 {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Char(' '));
    assert!(
        app.question_checked.contains(&3),
        "the last row, not past it"
    );
    for _ in 0..8 {
        press(&mut app, KeyCode::Char('k'));
    }
    press(&mut app, KeyCode::Char(' '));
    assert!(app.question_checked.contains(&0));
}

// ---- services in the app --------------------------------------------

#[test]
fn the_isolated_key_starts_the_selected_worktree() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Char('i'));
    let pending = app.pending.as_ref().expect("the key started something");
    assert_eq!(pending.kind, PendingKind::Start);
    assert_eq!(pending.name, "feat+one");
}

#[test]
fn service_health_reaches_the_app_off_the_ui_thread() {
    let mut app = test_app(&["feat+one"]);
    assert!(app.services_of("feat+one").is_empty());
    let health = ServiceHealth {
        shared: vec![actions::ServiceStatus {
            name: "postgres".into(),
            port: Some(5432),
            up: false,
            logging: false,
        }],
        worktrees: BTreeMap::from([(
            "feat+one".to_string(),
            vec![actions::ServiceStatus {
                name: "postgres".into(),
                port: Some(17_004),
                up: true,
                logging: false,
            }],
        )]),
    };
    assert!(
        app.handle_event(AppEvent::ServiceHealth(Box::new(health.clone()))),
        "a change in health is a reason to repaint"
    );
    assert_eq!(app.services_of("feat+one")[0].port, Some(17_004));
    assert!(!app.service_health.shared[0].up);
    assert!(
        !app.handle_event(AppEvent::ServiceHealth(Box::new(health))),
        "and the same answer twice is not"
    );
}

fn type_str(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

#[test]
fn the_cursor_moves_and_clamps_at_both_ends() {
    let mut app = test_app(&["a", "b", "c"]);
    assert_eq!(app.list_state.selected(), Some(0));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.list_state.selected(), Some(2));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.list_state.selected(), Some(2), "clamped at the end");
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.list_state.selected(), Some(0));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.list_state.selected(), Some(0), "clamped at the start");
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.list_state.selected(), Some(2));
}

#[test]
fn an_empty_list_has_no_selection_and_ignores_movement() {
    let mut app = test_app(&[]);
    assert_eq!(app.list_state.selected(), None);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.list_state.selected(), None);
}

#[test]
fn filtering_narrows_the_list_and_escape_restores_it() {
    let mut app = test_app(&["feat+one", "feat+two", "fix+three"]);
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(app.mode, Mode::Filter);
    type_str(&mut app, "fix");
    assert_eq!(app.filtered_indices.len(), 1);
    assert_eq!(app.selected_worktree().unwrap().name, "fix+three");

    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.filtered_indices.len(), 1, "\"fi\" still matches one");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.mode, Mode::Normal);
    assert!(app.filter.is_empty());
    assert_eq!(app.filtered_indices.len(), 3);
}

#[test]
fn filtering_matches_the_branch_as_well_as_the_name() {
    let mut app = test_app(&["odd+name"]);
    app.worktrees[0].branch = Some("release/1.2".into());
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "release");
    assert_eq!(app.filtered_indices.len(), 1);
}

#[test]
fn enter_leaves_filter_mode_but_keeps_the_filter() {
    let mut app = test_app(&["feat+one", "fix+two"]);
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "fix");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Normal);
    assert_eq!(app.filter, "fix");
    assert_eq!(app.filtered_indices.len(), 1);
}

#[test]
fn q_quits_and_ctrl_c_quits_from_anywhere() {
    let mut app = test_app(&["a"]);
    press(&mut app, KeyCode::Char('q'));
    assert!(app.should_quit);

    let mut app = test_app(&["a"]);
    press(&mut app, KeyCode::Char('?'));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.should_quit, "ctrl-c quits even with a modal open");
}

#[test]
fn the_help_modal_opens_scrolls_and_closes() {
    let mut app = test_app(&["a"]);
    press(&mut app, KeyCode::Char('?'));
    assert!(matches!(app.modal, Some(Modal::Help)));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.help_scroll, 1);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.help_scroll, 0);
    press(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none());
    assert_eq!(app.help_scroll, 0, "scroll resets for the next open");
}

#[test]
fn the_create_modal_opens_accepts_typing_and_closes_on_escape() {
    let mut app = test_app(&["a"]);
    press(&mut app, KeyCode::Char('n'));
    assert!(matches!(app.modal, Some(Modal::Create { .. })));
    type_str(&mut app, "feat/new");
    match &app.modal {
        Some(Modal::Create { input, .. }) => assert_eq!(input, "feat/new"),
        other => panic!("expected the create modal, got {other:?}"),
    }
    press(&mut app, KeyCode::Backspace);
    match &app.modal {
        Some(Modal::Create { input, .. }) => assert_eq!(input, "feat/ne"),
        other => panic!("expected the create modal, got {other:?}"),
    }
    press(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none());
}

// Validation happens in the modal, before any worker starts, so a
// colliding name is refused with what was typed still on screen.
#[test]
fn the_create_modal_refuses_a_name_that_already_exists() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Char('n'));
    type_str(&mut app, "feat/one");
    press(&mut app, KeyCode::Enter);

    assert!(
        matches!(app.modal, Some(Modal::Create { .. })),
        "modal stays open"
    );
    assert!(app.pending.is_none(), "no worker should have started");
    let (message, is_error) = app.active_status().unwrap();
    assert!(is_error);
    assert!(message.contains("already exists"), "{message}");
}

#[test]
fn enter_on_an_empty_create_modal_asks_for_a_name() {
    let mut app = test_app(&[]);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.modal, Some(Modal::Create { .. })));
    assert!(app.active_status().unwrap().0.contains("branch name"));
}

#[test]
fn branches_arriving_populate_an_open_create_modal() {
    let mut app = test_app(&[]);
    press(&mut app, KeyCode::Char('n'));
    let branches = vec![BranchEntry {
        name: "main".into(),
        source: BranchSource::Local,
    }];
    app.handle_event(AppEvent::BranchesReady(branches));
    match &app.modal {
        Some(Modal::Create { branches, .. }) => {
            assert!(!branches.is_loading());
            assert_eq!(branches.as_slice().len(), 1);
        }
        other => panic!("expected the create modal, got {other:?}"),
    }
}

#[test]
fn the_remove_modal_names_its_target_and_closes_on_escape() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Char('d'));
    match &app.modal {
        Some(Modal::Remove { name, blocker, .. }) => {
            assert_eq!(name, "feat+one");
            assert!(blocker.is_none(), "a clean pando worktree has no blocker");
        }
        other => panic!("expected the remove modal, got {other:?}"),
    }
    press(&mut app, KeyCode::Char('n'));
    assert!(app.modal.is_none());
}

#[test]
fn the_remove_modal_states_why_a_removal_would_be_refused() {
    let mut app = test_app(&["locked+one", "dirty+one", "adopted+one"]);
    app.worktrees[0].locked = true;
    app.worktrees[0].lock_reason = Some("benchmarking".into());
    app.worktrees[1].dirty = Some(true);
    app.created_by_pando.insert("adopted+one".into(), false);

    press(&mut app, KeyCode::Char('d'));
    let blocker = match &app.modal {
        Some(Modal::Remove { blocker, .. }) => blocker.clone().unwrap(),
        other => panic!("expected the remove modal, got {other:?}"),
    };
    assert!(blocker.is_fatal(), "a locked worktree can never be removed");
    assert!(blocker.line().contains("benchmarking"));

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('d'));
    match &app.modal {
        Some(Modal::Remove { blocker, .. }) => {
            assert_eq!(blocker, &Some(RemoveBlocker::Dirty));
            assert!(!blocker.as_ref().unwrap().is_fatal());
        }
        other => panic!("expected the remove modal, got {other:?}"),
    }

    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('d'));
    match &app.modal {
        Some(Modal::Remove { blocker, .. }) => {
            assert_eq!(blocker, &Some(RemoveBlocker::NotOurs))
        }
        other => panic!("expected the remove modal, got {other:?}"),
    }
}

// Confirming a locked worktree must not start a worker that git would
// refuse anyway.
#[test]
fn confirming_a_locked_removal_refuses_without_starting_work() {
    let mut app = test_app(&["locked+one"]);
    app.worktrees[0].locked = true;
    app.worktrees[0].lock_reason = Some("benchmarking".into());
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));

    assert!(app.pending.is_none(), "no worker for a refusal");
    assert!(app.modal.is_none());
    let (message, is_error) = app.active_status().unwrap();
    assert!(is_error);
    assert!(message.contains("locked"), "{message}");
}

#[test]
fn d_with_nothing_selected_says_so() {
    let mut app = test_app(&[]);
    press(&mut app, KeyCode::Char('d'));
    assert!(app.modal.is_none());
    assert!(app.active_status().unwrap().1, "should be an error");
}

#[test]
fn y_copies_the_selected_worktree_path() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(app.clipboard.as_deref(), Some("/trees/feat+one"));
    assert!(app.active_status().unwrap().0.contains("/trees/feat+one"));
}

#[test]
fn create_rows_offers_the_typed_name_then_matching_branches() {
    let branches = vec![
        BranchEntry {
            name: "main".into(),
            source: BranchSource::Local,
        },
        BranchEntry {
            name: "feat/one".into(),
            source: BranchSource::Remote,
        },
    ];
    let rows = create_rows("", &branches);
    assert_eq!(rows.len(), 2, "empty input offers no new-branch row");
    assert!(matches!(rows[0], CreateRow::Existing(_)));

    let rows = create_rows("feat", &branches);
    assert_eq!(rows[0], CreateRow::NewBranch("feat".into()));
    assert!(matches!(&rows[1], CreateRow::Existing(b) if b.name == "feat/one"));

    let rows = create_rows("feat/one", &branches);
    assert_eq!(
        rows.len(),
        1,
        "an exact match suppresses the new-branch row: {rows:?}"
    );
    assert!(matches!(&rows[0], CreateRow::Existing(b) if b.name == "feat/one"));

    let rows = create_rows("nothing-matches", &branches);
    assert_eq!(rows, vec![CreateRow::NewBranch("nothing-matches".into())]);

    assert!(create_rows("", &[]).is_empty());
}

#[test]
fn create_rows_matching_is_case_insensitive_and_input_is_trimmed() {
    let branches = vec![BranchEntry {
        name: "Feat/One".into(),
        source: BranchSource::Local,
    }];
    let rows = create_rows("  feat  ", &branches);
    assert_eq!(rows[0], CreateRow::NewBranch("feat".into()));
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_snapshot_keeps_enrichment_for_unchanged_worktrees() {
    let mut app = test_app(&["feat+one"]);
    let mut refreshed = wt("feat+one");
    refreshed.head_sha = None;
    refreshed.dirty = None;
    let fresh = app.apply_snapshot(Snapshot {
        main: wt("acme-shop"),
        worktrees: vec![refreshed, wt("feat+two")],
        created_by_pando: BTreeMap::new(),
        state: State::new(),
        warning: None,
        notices: Vec::new(),
        default_base: Some("main".into()),
    });

    assert_eq!(fresh, vec!["feat+two"], "only new entries need enriching");
    let kept = app.worktrees.iter().find(|w| w.name == "feat+one").unwrap();
    assert_eq!(
        kept.head_sha.as_deref(),
        Some("abc1234"),
        "enrichment already collected must survive a refresh"
    );
}

#[test]
fn a_snapshot_re_enriches_a_worktree_whose_head_moved() {
    let mut app = test_app(&["feat+one"]);
    let mut moved = wt("feat+one");
    moved.head = Some("def456".into());
    let fresh = app.apply_snapshot(Snapshot {
        main: wt("acme-shop"),
        worktrees: vec![moved],
        created_by_pando: BTreeMap::new(),
        state: State::new(),
        warning: None,
        notices: Vec::new(),
        default_base: None,
    });
    assert_eq!(fresh, vec!["feat+one"]);
}

// The ownership map is what the Remove modal's "pando did not create
// this" warning reads, so a state file the refresh could not use has to
// reach the user rather than turning every row silently adopted.
#[test]
fn a_state_warning_from_a_refresh_reaches_the_status_line() {
    let mut app = test_app(&["feat+one"]);
    let snapshot = |warning: Option<&str>| Snapshot {
        main: wt("acme-shop"),
        worktrees: vec![wt("feat+one")],
        created_by_pando: BTreeMap::new(),
        state: State::new(),
        warning: warning.map(str::to_string),
        notices: Vec::new(),
        default_base: None,
    };

    app.apply_snapshot(snapshot(Some("state file /s is version 3")));
    let (message, is_error) = app.active_status().unwrap();
    assert!(message.contains("version 3"), "{message}");
    assert!(is_error, "a state file pando cannot use is an error");

    // A standing warning is not re-announced on every refresh.
    app.status = None;
    app.apply_snapshot(snapshot(Some("state file /s is version 3")));
    assert!(app.active_status().is_none());

    app.apply_snapshot(snapshot(None));
    assert_eq!(app.state_warning, None);
}

// Finding 10. A share that dies is announced once, because the record
// is gone by the next tick — so when two die together, showing only
// the first means the second worktree's URL closed in silence.
#[test]
fn every_refresh_notice_reaches_the_status_line_not_only_the_first() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    app.apply_snapshot(Snapshot {
        main: wt("acme-shop"),
        worktrees: vec![wt("feat+one"), wt("feat+two")],
        created_by_pando: BTreeMap::new(),
        state: State::new(),
        warning: None,
        notices: vec![
            "feat+one: the share's tunnel exited, so the public URL is closed".to_string(),
            "feat+two: the share's proxy exited, so the public URL is closed".to_string(),
        ],
        default_base: None,
    });

    let (message, _) = app.active_status().expect("a notice");
    assert!(message.contains("feat+one"), "{message}");
    assert!(
        message.contains("feat+two"),
        "the second share closed in silence: {message}"
    );
}

// The cursor follows the worktree, not the row it happened to be on:
// a refresh that reorders the list (a new worktree is newest-first)
// must not move the selection to a different one.
#[test]
fn a_refresh_keeps_the_cursor_on_the_same_worktree_when_the_order_changes() {
    let mut app = test_app(&["feat+one", "feat+two", "fix+three"]);
    app.select_index(2);
    assert_eq!(app.selected_worktree().unwrap().name, "fix+three");

    app.apply_snapshot(Snapshot {
        main: wt("acme-shop"),
        worktrees: vec![wt("fix+three"), wt("feat+one"), wt("feat+two")],
        created_by_pando: BTreeMap::new(),
        state: State::new(),
        warning: None,
        notices: Vec::new(),
        default_base: None,
    });
    assert_eq!(
        app.selected_worktree().unwrap().name,
        "fix+three",
        "the cursor must stay on the worktree it was on"
    );
    assert_eq!(app.list_state.selected(), Some(0));
}

#[test]
fn the_status_message_expires() {
    let mut app = test_app(&[]);
    app.set_status("hello");
    assert!(app.active_status().is_some());
    app.status.as_mut().unwrap().at = Instant::now() - STATUS_TTL - Duration::from_secs(1);
    assert!(app.active_status().is_none());
    assert!(app.expire_status());
    assert!(app.status.is_none());
}

#[test]
fn base64_matches_the_reference_encoding() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foob"), "Zm9vYg==");
    assert_eq!(base64(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    assert_eq!(base64("/trees/feat+one".as_bytes()), "L3RyZWVzL2ZlYXQrb25l");
}
// ---- processes -------------------------------------------------------

#[test]
fn the_process_keys_each_start_their_own_work() {
    for (key, kind) in [
        (KeyCode::Char('s'), PendingKind::Start),
        (KeyCode::Char('x'), PendingKind::Stop),
        (KeyCode::Char('r'), PendingKind::Restart),
    ] {
        let mut app = test_app(&["feat+one"]);
        press(&mut app, key);
        let pending = app.pending.as_ref().expect("the key started something");
        assert_eq!(pending.kind, kind, "{key:?}");
        assert_eq!(pending.name, "feat+one");
        // And the work is on a worker thread, not this one: the frame
        // is still answering keys.
        assert!(!app.should_quit);
    }
}

// The answers a session writes have to be in that session's own copy of
// the config. They were written to `pando.toml` and nowhere else, so
// the next `s` re-ran detection against a config that still had
// nothing and asked the same question again — with the rule's first
// candidate preselected rather than the answer just given.
#[test]
fn what_a_start_resolves_is_applied_to_this_session_in_memory() {
    // A real repository, because detection reads one. Nothing is
    // started: the worktree in the list does not exist, so the worker
    // resolves, sends the config, and then fails to find it — which is
    // exactly the case the config must survive.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("acme-shop");
    crate::testutil::init_repo(&root);
    std::fs::write(
        root.join("package.json"),
        "{\n  \"name\": \"x\",\n  \"scripts\": { \"dev\": \"next dev\" }\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
    std::fs::write(root.join(".env.example"), "PORT=3000\n").unwrap();
    let paths = PandoPaths::new(
        dir.path().join("pando-home"),
        crate::project::ProjectRef::from_root(&root).unwrap(),
    );
    let mut app = App::new_for_test(paths, Config::default(), vec![wt("feat+one")]);
    assert!(app.config.processes.is_empty(), "nothing is known yet");

    press(&mut app, KeyCode::Char('s'));
    let rx = app.event_rx.take().expect("the app owns its receiver");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut applied = false;
    while Instant::now() < deadline && !applied {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => {
                let is_config = matches!(event, AppEvent::ConfigResolved(_));
                app.handle_event(event);
                applied = is_config;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    app.event_rx = Some(rx);

    assert!(applied, "the worker never sent what it resolved");
    assert_eq!(
        app.config.processes["dev"].cmd, "pnpm dev",
        "the session knows what it just answered, so the next `s` asks nothing"
    );
    assert_eq!(app.config.processes["dev"].roles(), vec!["web"]);
}

#[test]
fn enter_starts_the_selected_worktree() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        app.pending.as_ref().map(|p| p.kind),
        Some(PendingKind::Start)
    );
}

#[test]
fn a_process_key_with_nothing_selected_says_so() {
    let mut app = test_app(&[]);
    press(&mut app, KeyCode::Char('s'));
    assert!(app.pending.is_none());
    assert_eq!(
        app.active_status().map(|(m, _)| m),
        Some("nothing selected")
    );
}

#[test]
fn open_needs_a_port_before_it_has_a_url() {
    let mut app = test_app(&["feat+one"]);
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(app.opened, None);
    let (message, is_error) = app.active_status().unwrap();
    assert!(message.contains("no port yet"), "{message}");
    assert!(is_error);

    with_process(&mut app, "feat+one", running_phase());
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(app.opened.as_deref(), Some("http://localhost:17342"));
}

// ---- share -----------------------------------------------------------

#[test]
fn t_on_a_worktree_that_is_not_shared_starts_a_share() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());

    press(&mut app, KeyCode::Char('t'));

    let pending = app.pending.as_ref().expect("a share is in flight");
    assert_eq!(pending.kind, PendingKind::Share);
    assert_eq!(pending.name, "feat+one");
    assert!(app.modal.is_none(), "sharing asks nothing");
}

#[test]
fn t_on_a_shared_worktree_asks_before_taking_the_url_away() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_share(&mut app, "feat+one", None);

    press(&mut app, KeyCode::Char('t'));
    match &app.modal {
        Some(Modal::Unshare { name, url }) => {
            assert_eq!(name, "feat+one");
            assert_eq!(url, "https://fake-host.trycloudflare.com");
        }
        other => panic!("expected the unshare confirmation, got {other:?}"),
    }
    assert!(
        app.pending.is_none(),
        "nothing happens until it is confirmed"
    );
}

#[test]
fn the_unshare_confirmation_takes_the_url_down_on_y_and_keeps_it_otherwise() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_share(&mut app, "feat+one", None);

    press(&mut app, KeyCode::Char('t'));
    press(&mut app, KeyCode::Esc);
    assert!(app.modal.is_none());
    assert!(app.pending.is_none(), "escape keeps the URL up");

    press(&mut app, KeyCode::Char('t'));
    press(&mut app, KeyCode::Char('n'));
    assert!(app.pending.is_none(), "so does n");

    press(&mut app, KeyCode::Char('t'));
    press(&mut app, KeyCode::Char('y'));
    let pending = app.pending.as_ref().expect("an unshare is in flight");
    assert_eq!(pending.kind, PendingKind::Unshare);
    assert!(app.modal.is_none());
}

#[test]
fn shift_o_opens_the_public_url_and_says_so_when_there_is_none() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());

    press(&mut app, KeyCode::Char('O'));
    assert_eq!(app.opened, None);
    let (message, is_error) = app.active_status().unwrap();
    assert!(message.contains("not shared"), "{message}");
    assert!(is_error);

    with_share(&mut app, "feat+one", None);
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(
        app.opened.as_deref(),
        Some("https://fake-host.trycloudflare.com")
    );
}

// `o` is the local one and `O` is the public one: two keys, two URLs,
// and neither may quietly become the other.
#[test]
fn the_two_open_keys_hand_out_different_urls() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_share(&mut app, "feat+one", None);

    press(&mut app, KeyCode::Char('o'));
    assert_eq!(app.opened.as_deref(), Some("http://localhost:17342"));
    press(&mut app, KeyCode::Char('O'));
    assert_eq!(
        app.opened.as_deref(),
        Some("https://fake-host.trycloudflare.com")
    );
}

#[test]
fn the_tunnel_and_proxy_logs_come_last_in_tab_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = test_app(&["feat+one"]);
    app.paths = PandoPaths::new(dir.path(), app.paths.project.clone());
    app.config
        .processes
        .insert("dev".to_string(), crate::config::ProcessConfig::default());
    let logs = app.paths.logs_dir("feat+one");
    std::fs::create_dir_all(&logs).unwrap();
    for source in ["proxy", "tunnel", "dev", "install"] {
        std::fs::write(logs.join(format!("{source}.log")), "x\n").unwrap();
    }

    assert_eq!(
        app.log_sources("feat+one"),
        vec!["dev", "install", "tunnel", "proxy"],
        "a share's own logs sort last, and the tunnel before the proxy"
    );
}

// Phase 2b review, finding 6. `url_of` was a third implementation of
// the URL rule: the alphabetically first *role* rather than the first
// role of the first process, and it never looked at what the process
// was really listening on. `o` opened a different address from the one
// `pando status` had just printed.
#[test]
fn open_uses_the_same_url_rule_as_status() {
    let mut app = test_app(&["feat+url2"]);
    let mut state = State::new();
    let mut record = WorktreeRecord::new("/trees/feat+url2", true);
    record.ports.insert("srv".to_string(), 19_056);
    record.ports.insert("admin".to_string(), 19_057);
    record
        .roles
        .insert("alpha".to_string(), vec!["srv".to_string()]);
    record
        .roles
        .insert("beta".to_string(), vec!["admin".to_string()]);
    // And `alpha` ignored the port it was given, which the TUI never
    // noticed at all.
    record.processes.insert(
        "alpha".to_string(),
        ProcessRecord {
            pid: 4242,
            pgid: 4242,
            started_at: Utc::now(),
            log_path: PathBuf::from("/does/not/exist/alpha.log"),
            ready_port: Some(19_056),
            ready_timeout_s: None,
            observed_ports: vec![3000],
            swept: false,
            phase: running_phase(),
        },
    );
    record.observed_ports = vec![3000];
    state.worktrees.insert("feat+url2".to_string(), record);
    app.handle_event(AppEvent::Refreshed(Box::new(Ok(state.clone()))));

    assert_eq!(
        app.url_of("feat+url2"),
        crate::actions::worktree_url(&state.worktrees["feat+url2"]),
        "one rule, wherever it is asked"
    );
    press(&mut app, KeyCode::Char('o'));
    assert_eq!(app.opened.as_deref(), Some("http://localhost:3000"));
}

#[test]
fn a_refresh_replaces_the_process_state() {
    let mut app = test_app(&["feat+one"]);
    let mut state = State::new();
    let mut record = WorktreeRecord::new("/trees/feat+one", true);
    record.ports.insert("web".to_string(), 17_342);
    state.worktrees.insert("feat+one".to_string(), record);
    assert!(app.handle_event(AppEvent::Refreshed(Box::new(Ok(state)))));
    assert_eq!(
        app.url_of("feat+one").as_deref(),
        Some("http://localhost:17342")
    );
    assert!(!app.refreshing, "the single-flight slot is free again");
}

#[test]
fn a_refresh_that_failed_reaches_the_status_line() {
    let mut app = test_app(&["feat+one"]);
    app.handle_event(AppEvent::Refreshed(Box::new(Err("state is v3".into()))));
    let (message, is_error) = app.active_status().unwrap();
    assert!(message.contains("state is v3"), "{message}");
    assert!(is_error);
}

// ---- the question modal ----------------------------------------------

#[test]
fn a_question_opens_a_modal_with_the_recommendation_preselected() {
    let mut app = test_app(&["feat+one"]);
    let _rx = open_question(&mut app, a_question());
    match app.modal.as_ref() {
        Some(Modal::Question {
            question,
            selected,
            custom,
            ..
        }) => {
            assert_eq!(*selected, 0, "the rules' own pick is preselected");
            assert!(custom.is_none());
            assert_eq!(question.options.len(), 2);
        }
        other => panic!("expected a question modal, got {:?}", other.is_some()),
    }
}

#[test]
fn choosing_an_option_answers_the_worker_and_closes_the_modal() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_question());
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    assert_eq!(rx.try_recv().unwrap(), Ok(actions::Answer::Choice(1)));
    assert!(app.modal.is_none(), "the modal closes once it is answered");
}

// Every slot accepts a shell command, so there is never a dead end.
#[test]
fn a_typed_command_is_sent_as_written() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_question());
    press(&mut app, KeyCode::Char('c'));
    type_str(&mut app, "./serve.sh");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        rx.try_recv().unwrap(),
        Ok(actions::Answer::Custom("./serve.s".to_string()))
    );
    assert!(app.modal.is_none());
}

// The worker is blocked on the reply channel. A modal that closed
// without sending would leave it there for the life of the process.
#[test]
fn cancelling_a_question_always_tells_the_worker() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_question());
    press(&mut app, KeyCode::Esc);
    assert_eq!(rx.try_recv().unwrap(), Err("cancelled".to_string()));
    assert!(app.modal.is_none());
}

#[test]
fn escape_from_the_typing_line_goes_back_to_the_options() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_question());
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(
        app.modal.as_ref(),
        Some(Modal::Question { custom: None, .. })
    ));
    assert!(rx.try_recv().is_err(), "nothing was answered yet");
    press(&mut app, KeyCode::Esc);
    assert_eq!(rx.try_recv().unwrap(), Err("cancelled".to_string()));
}

#[test]
fn a_question_with_no_options_opens_straight_into_typing() {
    let mut app = test_app(&["feat+one"]);
    let mut question = a_question();
    question.options.clear();
    question.preselect = None;
    let rx = open_question(&mut app, question);
    assert!(matches!(
        app.modal.as_ref(),
        Some(Modal::Question {
            custom: Some(_),
            ..
        })
    ));
    type_str(&mut app, "node server.js");
    press(&mut app, KeyCode::Enter);
    assert_eq!(
        rx.try_recv().unwrap(),
        Ok(actions::Answer::Custom("node server.js".to_string()))
    );
}

#[test]
fn an_empty_typed_answer_is_refused_rather_than_sent() {
    let mut app = test_app(&["feat+one"]);
    let rx = open_question(&mut app, a_question());
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Enter);
    assert!(rx.try_recv().is_err());
    assert!(app.modal.is_some());
    assert!(app.active_status().unwrap().0.contains("type a command"));
}

// ---- several processes -----------------------------------------------

#[test]
fn tab_switches_the_tail_between_processes() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());

    let (_, first, _) = app.tail_target().expect("a process to tail");
    assert_eq!(first, "api", "config order, which is the row order");

    press(&mut app, KeyCode::Tab);
    let (key_of, second, path) = app.tail_target().expect("a process to tail");
    assert_eq!(second, "dev");
    assert_eq!(
        key_of, "feat+one/dev",
        "one tail per process, not per worktree"
    );
    assert!(path.ends_with("dev.log"));

    // And round it goes.
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tail_target().unwrap().1, "api");
}

#[test]
fn tab_on_a_worktree_with_one_process_says_so_rather_than_cycling() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tail_target().unwrap().1, "dev");
    let (message, is_error) = app.active_status().expect("something was said");
    assert!(message.contains("one process"), "{message}");
    assert!(is_error);
}

#[test]
fn moving_to_another_worktree_starts_at_its_first_process() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());
    with_process(&mut app, "feat+two", running_phase());

    press(&mut app, KeyCode::Tab);
    assert_eq!(app.tail_target().unwrap().1, "dev");
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.tail_target().unwrap().0, "feat+two/dev");
    assert_eq!(app.tail_index, 0, "the index belongs to the row");
}

// ---- the log tail ----------------------------------------------------

#[test]
fn the_tail_lru_keeps_the_newest_and_drops_the_coldest() {
    let mut tails = LogTails::default();
    for i in 0..MAX_LOG_TAILS + 2 {
        tails.touch(&format!("w{i}"), PathBuf::from("/does/not/exist.log"));
    }
    assert_eq!(tails.len(), MAX_LOG_TAILS);
    assert!(tails.get("w0").is_none(), "the coldest went");
    assert!(tails.get(&format!("w{}", MAX_LOG_TAILS + 1)).is_some());
}

#[test]
fn the_selected_tail_never_evicts_itself() {
    let mut tails = LogTails::default();
    tails.touch("keep", PathBuf::from("/does/not/exist.log"));
    for i in 0..MAX_LOG_TAILS + 4 {
        tails.touch("keep", PathBuf::from("/does/not/exist.log"));
        tails.touch(&format!("w{i}"), PathBuf::from("/does/not/exist.log"));
    }
    assert!(tails.get("keep").is_some());
}

// `l` used to focus the inline tail so j/k scrolled it; it opens the
// full viewer now, and the page keys scroll the tail in place — which
// is the origin tool's own binding, and leaves j/k on the list.
#[test]
fn the_page_keys_scroll_the_inline_tail_without_moving_the_list() {
    let (_dir, mut app) = app_with_logs(&["feat+one", "feat+two"]);
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    tail_the_log(&mut app, "feat+one", "dev");
    app.tail_rows = 10;
    app.handle_event(AppEvent::Tick);

    let row = app.list_state.selected();
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.tail_scroll, 10, "a page back through the tail");
    assert_eq!(app.list_state.selected(), row, "the list cursor stays put");
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.tail_scroll, 0, "and a page forward returns to the end");
}

#[test]
fn moving_the_cursor_returns_the_tail_to_the_end() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    app.tail_scroll = 12;
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.tail_scroll, 0);
}

// ---- the log viewer --------------------------------------------------

/// An app whose home is a real (temporary) directory, so the log files
/// the viewer reads are real files.
pub fn app_with_logs(names: &[&str]) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = PandoPaths::new(
        dir.path().join("home"),
        ProjectRef {
            id: "acme-shop-3f9a2c1d".into(),
            root: dir.path().join("acme-shop"),
            display_name: "acme-shop".into(),
        },
    );
    let worktrees: Vec<Worktree> = names.iter().map(|n| wt(n)).collect();
    // Every project that has a dev log has a dev process configured;
    // tab order is read off config, so the fixture carries one.
    let mut config = Config::default();
    config
        .processes
        .insert("dev".to_string(), crate::config::ProcessConfig::default());
    let mut app = App::new_for_test(paths, config, worktrees);
    app.created_by_pando = names.iter().map(|n| (n.to_string(), true)).collect();
    (dir, app)
}

pub fn write_log<S: AsRef<str>>(app: &App, worktree: &str, source: &str, lines: &[S]) {
    let path = app.paths.log_file(worktree, source);
    std::fs::create_dir_all(path.parent().expect("a log has a directory")).unwrap();
    let body: String = lines
        .iter()
        .map(|line| format!("{}\n", line.as_ref()))
        .collect();
    std::fs::write(path, body).unwrap();
}

/// Points the detail pane's tail at a log that really exists.
fn tail_the_log(app: &mut App, worktree: &str, source: &str) {
    let path = app.paths.log_file(worktree, source);
    if !app.state.worktrees.contains_key(worktree) {
        with_process(app, worktree, running_phase());
    }
    let record = app
        .state
        .worktrees
        .get_mut(worktree)
        .expect("the worktree has a record");
    let process = record
        .processes
        .remove("dev")
        .expect("with_process left a dev record");
    record.processes.insert(
        source.to_string(),
        ProcessRecord {
            log_path: path,
            ..process
        },
    );
}

/// Opens the viewer the way a key press would, then paints once so the
/// tab list and the viewport are what the first frame decided.
fn open_viewer(app: &mut App, width: u16, height: u16) {
    press(app, KeyCode::Char('l'));
    paint(app, width, height);
}

fn paint(app: &mut App, width: u16, height: u16) {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| crate::tui::render::render(f, app))
        .unwrap();
}

fn viewer(app: &App) -> &LogView {
    app.log_view().expect("the viewer is open")
}

#[test]
fn l_opens_the_viewer_on_the_selected_worktree_and_q_returns_to_the_list() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["listening on 17342"]);
    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).name, "feat+one");
    assert_eq!(viewer(&app).source, "dev");
    press(&mut app, KeyCode::Char('q'));
    assert!(app.log_view().is_none(), "q goes back to the list");
    assert!(!app.should_quit, "and does not quit pando");
}

#[test]
fn a_worktree_with_two_logs_gets_two_tabs_and_one_with_one_gets_one() {
    let (_dir, mut app) = app_with_logs(&["feat+one", "feat+two"]);
    write_log(&app, "feat+one", "dev", &["up"]);
    write_log(&app, "feat+one", "install", &["installed"]);
    write_log(&app, "feat+two", "dev", &["up"]);

    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).available, vec!["dev", "install"]);

    press(&mut app, KeyCode::Char('q'));
    press(&mut app, KeyCode::Char('j'));
    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).available, vec!["dev"]);
}

#[test]
fn a_source_that_appears_later_gets_a_tab_on_the_next_draw() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["up"]);
    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).available, vec!["dev"]);

    write_log(&app, "feat+one", "migrate", &["done"]);
    paint(&mut app, 80, 20);
    assert_eq!(
        viewer(&app).available,
        vec!["dev", "migrate"],
        "the tab bar is rebuilt from the files that are there"
    );
}

#[test]
fn tabs_run_processes_first_then_hooks_then_whatever_is_left() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    app.config
        .processes
        .insert("web".to_string(), crate::config::ProcessConfig::default());
    app.config
        .processes
        .insert("api".to_string(), crate::config::ProcessConfig::default());
    app.config.hooks.push(crate::config::HookConfig {
        name: "migrate".to_string(),
        after: crate::config::HookPoint::Services,
        fingerprint: Vec::new(),
        cmd: "true".to_string(),
        cwd: None,
        fallback: None,
    });
    for source in ["tunnel", "migrate", "install", "web", "api"] {
        write_log(&app, "feat+one", source, &["x"]);
    }
    open_viewer(&mut app, 80, 20);
    assert_eq!(
        viewer(&app).available,
        vec!["api", "web", "install", "migrate", "tunnel"],
        "processes, then hooks, then the rest"
    );
}

#[test]
fn the_viewer_opens_on_the_source_the_detail_tail_was_showing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "api", &["api up"]);
    write_log(&app, "feat+one", "dev", &["dev up"]);
    tail_the_log(&mut app, "feat+one", "api");
    open_viewer(&mut app, 80, 20);
    assert_eq!(
        viewer(&app).source,
        "api",
        "pressing l while reading the api's tail must not land on dev"
    );
}

#[test]
fn tab_cycles_the_source_and_shift_tab_goes_back() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["dev up"]);
    write_log(&app, "feat+one", "install", &["installed"]);
    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).source, "dev");
    press(&mut app, KeyCode::Tab);
    assert_eq!(viewer(&app).source, "install");
    press(&mut app, KeyCode::Tab);
    assert_eq!(viewer(&app).source, "dev", "and it wraps");
    press(&mut app, KeyCode::BackTab);
    assert_eq!(viewer(&app).source, "install");
}

#[test]
fn tab_does_nothing_when_there_is_only_one_source() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["dev up"]);
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Tab);
    assert_eq!(viewer(&app).source, "dev");
}

#[test]
fn a_worktree_with_no_log_yet_opens_a_viewer_that_says_so() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    open_viewer(&mut app, 80, 20);
    let view = viewer(&app);
    assert!(view.missing);
    assert_eq!(
        view.available,
        vec!["dev".to_string()],
        "the source on screen is always in the tab list, file or no file, \
         so `tab` can leave it"
    );
    assert!(!view.follow, "nothing to follow");
}

// The other half of finding 4: the title and the tab list are decided
// by the paint, and a tail whose file has been deleted never grows —
// so without this the frame goes on naming a file that is not there
// until something else happens to repaint it.
#[test]
fn a_log_deleted_under_the_viewer_asks_for_the_repaint_that_says_so() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one"]);
    open_viewer(&mut app, 80, 12);
    assert!(!viewer(&app).gone);

    std::fs::remove_file(app.paths.log_file("feat+one", "dev")).unwrap();
    assert!(
        app.handle_event(AppEvent::Tick),
        "the tick has to ask for a repaint; nothing else will"
    );
    paint(&mut app, 80, 12);
    assert!(viewer(&app).gone, "and the paint records what it decided");

    assert!(
        !app.handle_event(AppEvent::Tick),
        "once the frame agrees with the file, the viewer settles"
    );
}

// Phase 2c review, finding 5. `poll_viewer` returned early while
// `missing`, so a viewer opened before `start` had written anything —
// the common case — stayed on `no log file for this source yet` for as
// long as it was open.
#[test]
fn a_log_that_appears_after_the_viewer_opened_is_picked_up_on_the_next_tick() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    open_viewer(&mut app, 80, 20);
    assert!(viewer(&app).missing);

    write_log(&app, "feat+one", "dev", &["listening on 17342"]);
    app.handle_event(AppEvent::Tick);

    let view = viewer(&app);
    assert!(!view.missing, "the file is there now");
    assert!(view.follow, "and a viewer that has nothing yet follows it");
    assert_eq!(view.tail.lines().len(), 1, "read on the same tick");
}

#[test]
fn the_cursor_moves_by_a_count_and_never_leaves_the_log() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    open_viewer(&mut app, 80, 12);
    assert!(viewer(&app).follow, "the viewer opens on the live tail");

    press(&mut app, KeyCode::Char('k'));
    assert!(!viewer(&app).follow, "k breaks follow");
    assert_eq!(viewer(&app).cursor, 18);

    type_str(&mut app, "5");
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(viewer(&app).cursor, 13, "a count repeats the motion");
    assert_eq!(viewer(&app).count_prefix, None, "and is consumed");

    type_str(&mut app, "99");
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(viewer(&app).cursor, 19, "clamped at the last line");
    type_str(&mut app, "99");
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(viewer(&app).cursor, 0, "and at the first");
}

#[test]
fn g_goes_to_the_top_capital_g_follows_and_a_count_jumps_to_a_line() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    open_viewer(&mut app, 80, 12);

    press(&mut app, KeyCode::Char('g'));
    assert_eq!(viewer(&app).cursor, 0);
    assert!(!viewer(&app).follow);

    type_str(&mut app, "7");
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(viewer(&app).cursor, 6, "<n>G is one-based");

    press(&mut app, KeyCode::Char('G'));
    assert!(viewer(&app).follow, "bare G returns to the live tail");
}

#[test]
fn the_half_page_keys_move_by_half_the_viewer() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    // 20 rows minus the border and the footer leaves 17 body rows.
    open_viewer(&mut app, 80, 20);
    let half = app.viewer_height / 2;
    assert!(half > 1, "the body has room for a half page: {half}");

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(viewer(&app).cursor, 59 - half);
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(viewer(&app).cursor, 59);
}

#[test]
fn motions_on_an_empty_and_a_one_line_log_stay_in_range() {
    for lines in [vec![], vec!["only".to_string()]] {
        let (_dir, mut app) = app_with_logs(&["feat+one"]);
        write_log(&app, "feat+one", "dev", &lines);
        open_viewer(&mut app, 80, 8);
        for key in ['j', 'k', 'g', 'G', 'w'] {
            type_str(&mut app, "9");
            press(&mut app, KeyCode::Char(key));
            paint(&mut app, 80, 8);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        paint(&mut app, 80, 8);
        assert!(viewer(&app).cursor <= lines.len().saturating_sub(1).max(0));
    }
}

#[test]
fn w_toggles_wrap() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["x".repeat(400)]);
    open_viewer(&mut app, 80, 12);
    assert!(viewer(&app).wrap, "lines wrap by default");
    press(&mut app, KeyCode::Char('w'));
    assert!(!viewer(&app).wrap);
    press(&mut app, KeyCode::Char('w'));
    assert!(viewer(&app).wrap);
}

#[test]
fn the_viewer_polls_its_own_log_on_the_tick() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one"]);
    open_viewer(&mut app, 80, 12);
    assert_eq!(viewer(&app).tail.lines().len(), 1);
    write_log(&app, "feat+one", "dev", &["one", "two", "three"]);
    app.handle_event(AppEvent::Tick);
    assert_eq!(viewer(&app).tail.lines().len(), 3);
}

// ---- search and the level filter -------------------------------------

fn search_for(app: &mut App, query: &str) {
    press(app, KeyCode::Char('/'));
    for c in query.chars() {
        press(app, KeyCode::Char(c));
    }
    press(app, KeyCode::Enter);
}

#[test]
fn search_is_case_insensitive_and_lands_on_the_first_match() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["plain", "Compiled in 30ms", "plain", "compiled again"],
    );
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "COMPILED");
    assert_eq!(viewer(&app).search.matches, vec![1, 3]);
    assert_eq!(viewer(&app).cursor, 1, "the viewer jumps to the first one");
    assert!(!viewer(&app).follow);
    assert_eq!(viewer(&app).search_mode, SearchMode::Active);
}

#[test]
fn n_and_capital_n_step_the_matches_and_wrap() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["hit a", "miss", "hit b", "miss", "hit c"],
    );
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    assert_eq!(viewer(&app).search.cursor, 0);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 2);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 4);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 0, "and it wraps");
    press(&mut app, KeyCode::Char('N'));
    assert_eq!(viewer(&app).cursor, 4, "backwards too");
    type_str(&mut app, "2");
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 2, "a count steps that many matches");
}

#[test]
fn ctrl_n_and_ctrl_p_step_the_matches_as_well() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit a", "miss", "hit b"]);
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(viewer(&app).cursor, 2);
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(viewer(&app).cursor, 0);
}

#[test]
fn escape_while_typing_clears_the_search_and_keeps_the_viewer_open() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('/'));
    type_str(&mut app, "hi");
    assert_eq!(viewer(&app).search.query, "hi");
    press(&mut app, KeyCode::Backspace);
    assert_eq!(viewer(&app).search.query, "h");
    press(&mut app, KeyCode::Esc);
    assert!(
        app.log_view().is_some(),
        "esc clears the query, not the view"
    );
    assert_eq!(viewer(&app).search_mode, SearchMode::Inactive);
    assert!(viewer(&app).search.query.is_empty());
}

// The search bar promises "esc clear", so esc has to clear rather than
// throw away the whole viewer and the reading position with it. One
// layer at a time; a second esc leaves.
#[test]
fn escape_on_a_live_search_clears_it_and_a_second_one_leaves() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit a", "plain", "hit b"]);
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    assert_eq!(viewer(&app).search_mode, SearchMode::Active);

    press(&mut app, KeyCode::Esc);
    assert!(app.log_view().is_some(), "the viewer stays open");
    assert_eq!(viewer(&app).search_mode, SearchMode::Inactive);
    assert!(viewer(&app).search.matches.is_empty());

    press(&mut app, KeyCode::Esc);
    assert!(app.log_view().is_none(), "and the next one leaves");
}

#[test]
fn q_leaves_the_viewer_even_with_a_search_running() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit"]);
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    press(&mut app, KeyCode::Char('q'));
    assert!(app.log_view().is_none());
    assert!(!app.should_quit);
}

#[test]
fn a_query_with_no_matches_leaves_the_cursor_where_it_was() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one", "two", "three"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('g'));
    search_for(&mut app, "nothing here");
    assert!(viewer(&app).search.matches.is_empty());
    assert_eq!(viewer(&app).cursor, 0);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 0, "stepping nothing moves nothing");
}

#[test]
fn f_cycles_the_level_filter_and_the_viewer_shows_only_what_passes() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["just info", "WARN slow", "ERROR boom", "more info"],
    );
    open_viewer(&mut app, 80, 12);
    assert_eq!(viewer(&app).visible_len(), 4);

    press(&mut app, KeyCode::Char('f'));
    assert_eq!(viewer(&app).log_filter, LogFilter::WarnPlus);
    assert_eq!(viewer(&app).visible_len(), 2);

    press(&mut app, KeyCode::Char('f'));
    assert_eq!(viewer(&app).log_filter, LogFilter::ErrorOnly);
    assert_eq!(viewer(&app).visible_len(), 1);

    press(&mut app, KeyCode::Char('f'));
    assert_eq!(viewer(&app).log_filter, LogFilter::All);
    assert!(viewer(&app).follow, "a filter change returns to the tail");
}

// dwt's `search_with_filter_skips_hidden_matches_and_maps_scroll_to_
// filtered_position`: the scroll and cursor are positions in the
// *filtered* list, so a match's absolute index has to be translated.
#[test]
fn search_under_a_filter_skips_hidden_matches_and_maps_the_position() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "target in an info line",
            "just info",
            "WARN target in a warn line",
            "just info",
            "ERROR target in an error line",
        ],
    );
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('f')); // warn+
    search_for(&mut app, "target");
    assert_eq!(
        viewer(&app).search.matches,
        vec![2, 4],
        "the info line matches the query but not the filter"
    );
    assert_eq!(
        viewer(&app).cursor,
        0,
        "line 2 is the first of the two lines the filter shows"
    );
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).cursor, 1, "and line 4 is the second");
}

#[test]
fn changing_the_filter_drops_matches_it_now_hides() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["target info", "WARN target", "plain"],
    );
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "target");
    assert_eq!(viewer(&app).search.matches, vec![0, 1]);
    press(&mut app, KeyCode::Char('f'));
    assert_eq!(
        viewer(&app).search.matches,
        vec![1],
        "the cursor can never land on a row that is not painted"
    );
}

#[test]
fn ampersand_collapses_the_view_to_the_matches_and_expands_again() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["hit a", "miss", "hit b", "miss", "hit c"],
    );
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    press(&mut app, KeyCode::Char('&'));
    assert!(viewer(&app).collapsed());
    assert_eq!(viewer(&app).visible_len(), 3, "only the matches");
    assert_eq!(viewer(&app).cursor, 0, "and it lands at the top");
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(
        viewer(&app).cursor,
        1,
        "stepping stays in the collapsed list"
    );
    press(&mut app, KeyCode::Char('&'));
    assert!(!viewer(&app).collapsed());
    assert_eq!(viewer(&app).visible_len(), 5);
}

#[test]
fn ampersand_does_nothing_without_an_active_search() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a", "b"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('&'));
    assert!(!viewer(&app).filter_to_matches);
    assert_eq!(viewer(&app).visible_len(), 2);
}

#[test]
fn matches_are_realigned_when_the_ring_buffer_evicts() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit a", "filler", "hit b"]);
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    assert_eq!(viewer(&app).search.matches, vec![0, 2]);

    // A tail with room for three lines, so two more evict two.
    let path = app.paths.log_file("feat+one", "dev");
    let view = app.log_view_mut().expect("the viewer is open");
    view.tail = LogTail::new(path.clone(), 3);
    view.tail.poll().unwrap();
    view.search.matches = vec![0, 2];
    view.follow = false;
    write_log(
        &app,
        "feat+one",
        "dev",
        &["hit a", "filler", "hit b", "more", "and more"],
    );
    app.handle_event(AppEvent::Tick);

    let view = viewer(&app);
    assert_eq!(view.tail.lines().len(), 3);
    assert_eq!(
        view.search.matches,
        vec![0],
        "the match at index 0 fell out; index 2 moved down to 0"
    );
    assert!(view.search.cursor < view.search.matches.len().max(1));
}

#[test]
fn a_new_matching_line_joins_the_search_without_moving_the_reader() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit a", "plain", "hit b"]);
    open_viewer(&mut app, 80, 12);
    search_for(&mut app, "hit");
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(viewer(&app).search.cursor, 1);
    let was = viewer(&app).cursor;

    write_log(
        &app,
        "feat+one",
        "dev",
        &["hit a", "plain", "hit b", "hit c"],
    );
    app.handle_event(AppEvent::Tick);
    assert_eq!(viewer(&app).search.matches, vec![0, 2, 3]);
    assert_eq!(
        viewer(&app).search.cursor,
        1,
        "the reader stays on the match they were on"
    );
    assert_eq!(viewer(&app).cursor, was);
}

// ---- JSON blocks and the inspect overlay -----------------------------

/// A pretty-printed JSON block, as a framework prints one, with
/// ordinary lines on either side.
fn json_block_log() -> Vec<String> {
    [
        "starting up",
        "{",
        "  \"level\": \"error\",",
        "  \"msg\": \"boom\",",
        "  \"count\": 3",
        "}",
        "carrying on",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[test]
fn the_lines_of_a_json_block_share_one_id_and_one_severity() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &json_block_log());
    open_viewer(&mut app, 80, 20);
    let lines: Vec<_> = viewer(&app).tail.lines().iter().cloned().collect();
    let ids: Vec<Option<u64>> = lines.iter().map(|l| l.block_id).collect();
    assert_eq!(ids[0], None, "the line before the block is its own");
    assert!(ids[1].is_some());
    assert_eq!(
        ids[1..6]
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "every line of the block carries the same id: {ids:?}"
    );
    assert_eq!(ids[6], None, "and the line after it is its own again");
    for line in &lines[1..6] {
        assert_eq!(
            line.level,
            LogLevel::Error,
            "the whole block takes the block's severity: {}",
            line.plain
        );
    }
}

#[test]
fn a_level_filter_keeps_or_drops_a_block_as_one_unit() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &json_block_log());
    open_viewer(&mut app, 80, 20);
    assert_eq!(viewer(&app).visible_len(), 7);
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('f')); // errors only
    assert_eq!(
        viewer(&app).visible_len(),
        5,
        "all five lines of the block, and neither of the plain ones"
    );
}

#[test]
fn capital_j_inspects_the_whole_block_and_j_scrolls_it() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &json_block_log());
    open_viewer(&mut app, 80, 20);
    // Onto a line in the middle of the block.
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('J'));

    let inspect = app.inspect.as_ref().expect("the overlay is open");
    assert!(
        inspect.text.contains("\"msg\": \"boom\""),
        "{}",
        inspect.text
    );
    assert!(inspect.text.contains("\"count\": 3"), "{}", inspect.text);
    assert!(
        !inspect.text.contains("starting up"),
        "the block, and only the block: {}",
        inspect.text
    );
    assert_eq!(inspect.scroll, 0);

    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.inspect.as_ref().unwrap().scroll, 1);
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.inspect.as_ref().unwrap().scroll, 0);
    press(&mut app, KeyCode::Char('q'));
    assert!(app.inspect.is_none(), "q closes the overlay");
    assert!(app.log_view().is_some(), "and leaves the viewer open");
}

#[test]
fn enter_inspects_the_cursor_line_and_pretty_prints_its_json() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["12:00:01 request {\"path\":\"/x\",\"ms\":30}"],
    );
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Enter);
    let inspect = app.inspect.as_ref().expect("the overlay is open");
    assert!(
        inspect.text.starts_with("12:00:01 request"),
        "the prefix is kept above the JSON: {}",
        inspect.text
    );
    assert!(
        inspect.text.contains("\n  \"path\": \"/x\""),
        "and the JSON is expanded: {}",
        inspect.text
    );
}

#[test]
fn a_line_with_no_json_inspects_as_itself() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["just a plain line"]);
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(app.inspect.as_ref().unwrap().text, "just a plain line");
    assert_eq!(app.inspect.as_ref().unwrap().lines.len(), 1);
}

#[test]
fn a_block_with_raw_output_interleaved_inspects_verbatim() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "{",
            "  \"level\": \"warn\",",
            "SELECT * FROM users;",
            "  \"msg\": \"slow\"",
            "}",
        ],
    );
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('J'));
    let inspect = app.inspect.as_ref().expect("the overlay is open");
    assert!(
        inspect.text.contains("SELECT * FROM users;"),
        "an unparseable block is shown as it arrived: {}",
        inspect.text
    );
    assert_eq!(inspect.lines.len(), 5, "one styled line per raw line");
}

#[test]
fn the_overlay_swallows_the_viewers_keys_while_it_is_open() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('g'));
    let cursor = viewer(&app).cursor;
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(
        viewer(&app).cursor,
        cursor,
        "the line underneath does not move"
    );
    press(&mut app, KeyCode::Esc);
    assert!(app.inspect.is_none());
    assert!(
        app.log_view().is_some(),
        "esc closes the overlay, not the viewer"
    );
}

#[test]
fn leaving_the_viewer_closes_the_overlay_with_it() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a line"]);
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('J'));
    assert!(app.inspect.is_some());
    app.close_log_viewer();
    assert!(app.inspect.is_none());
}

// ---- error jumps -----------------------------------------------------

#[test]
fn capital_e_lands_on_the_first_line_of_each_error_block() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "starting up", // 0
            "{",           // 1  block A, error
            "  \"level\": \"error\",",
            "  \"msg\": \"a\"",
            "}",          // 4
            "still fine", // 5
            "{",          // 6  block B, error
            "  \"level\": \"error\",",
            "  \"msg\": \"b\"",
            "}",    // 9
            "done", // 10
        ],
    );
    open_viewer(&mut app, 80, 24);
    press(&mut app, KeyCode::Char('g'));

    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 1, "the first line of the first block");
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(
        viewer(&app).cursor,
        6,
        "not every line of it — the next block"
    );
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 1, "and it wraps");
    press(&mut app, KeyCode::Char('e'));
    assert_eq!(viewer(&app).cursor, 6, "backwards too");
}

#[test]
fn two_adjacent_error_blocks_are_two_jump_targets() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "{",
            "  \"level\": \"error\",",
            "}",
            "{",
            "  \"level\": \"error\",",
            "}",
        ],
    );
    open_viewer(&mut app, 80, 24);
    assert_eq!(
        error_ranks(viewer(&app)),
        vec![0, 3],
        "back-to-back blocks must not merge into one run"
    );
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(
        viewer(&app).cursor,
        3,
        "the cursor was already on the first"
    );
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 0, "and it wraps back to it");
}

#[test]
fn a_run_of_standalone_error_lines_is_one_target() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "ok",
            "ERROR one",
            "ERROR two",
            "ERROR three",
            "ok again",
            "ERROR four",
        ],
    );
    open_viewer(&mut app, 80, 24);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 1);
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 5, "the run counts once");
}

#[test]
fn an_error_jump_with_no_errors_moves_nothing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["all", "quite", "fine"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(viewer(&app).cursor, 0);
    assert!(!viewer(&app).follow);
}

#[test]
fn an_error_jump_respects_the_level_filter() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["info", "WARN slow", "info", "ERROR boom"],
    );
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('f')); // warn+, so two lines are shown
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('E'));
    assert_eq!(
        viewer(&app).cursor,
        1,
        "the error is the second of the two visible lines"
    );
}

// ---- yank ------------------------------------------------------------

#[test]
fn y_copies_exactly_the_cursor_line_and_says_so() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["first", "second", "third"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(app.clipboard.as_deref(), Some("second"));
    let (message, is_error) = app.active_status().expect("a confirmation");
    assert!(message.contains("copied line"), "{message}");
    assert!(!is_error);
}

#[test]
fn y_inside_a_block_copies_the_one_line_not_the_block() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &json_block_log());
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(
        app.clipboard.as_deref(),
        Some("  \"level\": \"error\","),
        "y is one line; J is the block"
    );
}

#[test]
fn y_while_following_copies_the_newest_line() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["older", "newest"]);
    open_viewer(&mut app, 80, 12);
    assert!(viewer(&app).follow);
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(app.clipboard.as_deref(), Some("newest"));
}

#[test]
fn y_on_an_empty_log_copies_nothing_and_says_nothing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log::<&str>(&app, "feat+one", "dev", &[]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('y'));
    assert!(app.clipboard.is_none());
    assert!(app.active_status().is_none());
}

#[test]
fn capital_y_copies_the_url_on_the_line_or_says_there_is_none() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["no url here", "ready on http://localhost:17342/ in 1.2s"],
    );
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('Y'));
    assert_eq!(app.clipboard.as_deref(), Some("http://localhost:17342/"));
    assert!(app.active_status().unwrap().0.contains("copied http"));

    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('Y'));
    assert_eq!(
        app.clipboard.as_deref(),
        Some("http://localhost:17342/"),
        "the clipboard is left as it was"
    );
    assert!(app.active_status().unwrap().0.contains("no URL"));
}

#[test]
fn first_url_stops_at_whitespace_and_closing_delimiters() {
    assert_eq!(
        first_url("ready on http://localhost:17342 now"),
        Some("http://localhost:17342".to_string())
    );
    assert_eq!(
        first_url("see <https://example.com/a>"),
        Some("https://example.com/a".to_string())
    );
    assert_eq!(
        first_url("both http://a.test and https://b.test"),
        Some("http://a.test".to_string()),
        "the first one"
    );
    assert_eq!(first_url("nothing here"), None);
}

#[test]
fn y_in_the_overlay_copies_the_whole_block_and_counts_its_lines() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &json_block_log());
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Char('y'));
    let copied = app.clipboard.as_deref().expect("the block was copied");
    assert!(copied.contains("\"msg\": \"boom\""), "{copied}");
    assert!(copied.lines().count() > 1);
    let (message, _) = app.active_status().expect("a confirmation");
    assert!(message.contains("lines"), "{message}");
}

#[test]
fn a_single_line_confirmation_is_not_plural() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a plain line"]);
    open_viewer(&mut app, 80, 20);
    press(&mut app, KeyCode::Char('J'));
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(app.active_status().unwrap().0, "✓ copied 1 line");
}

#[test]
fn the_yank_confirmation_expires() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a line"]);
    open_viewer(&mut app, 80, 12);
    press(&mut app, KeyCode::Char('y'));
    assert!(app.active_status().is_some());
    // Rather than sleeping for the whole window, age the status.
    app.status.as_mut().unwrap().at = Instant::now() - STATUS_TTL;
    assert!(
        app.active_status().is_none(),
        "a confirmation is a confirmation, not a fixture"
    );
}

// ---- eviction --------------------------------------------------------

/// Replaces the open viewer's tail with one that holds `capacity`
/// lines, so a test can make the ring buffer evict without writing ten
/// thousand lines.
fn shrink_viewer_tail(app: &mut App, capacity: usize) {
    let view = app.log_view_mut().expect("the viewer is open");
    let path = view.tail.path().to_path_buf();
    view.tail = LogTail::new(path, capacity);
    view.tail.poll().ok();
}

#[test]
fn scrolling_tracks_only_the_evictions_the_filter_was_showing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["ERROR one", "info a", "info b", "ERROR two"],
    );
    open_viewer(&mut app, 80, 12);
    shrink_viewer_tail(&mut app, 4);
    press(&mut app, KeyCode::Char('f'));
    press(&mut app, KeyCode::Char('f')); // errors only: two lines visible
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(viewer(&app).cursor, 1, "on the second of the two errors");

    // Three more lines, of which only one is an error, push the first
    // three out of a four-line buffer.
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "ERROR one",
            "info a",
            "info b",
            "ERROR two",
            "info c",
            "info d",
            "ERROR three",
        ],
    );
    app.handle_event(AppEvent::Tick);
    assert_eq!(viewer(&app).tail.lines().len(), 4);
    assert_eq!(
        viewer(&app).cursor,
        0,
        "one visible line was evicted, so the cursor moved by one — \
         not by the three lines that actually fell out"
    );
    assert_eq!(viewer(&app).visible_len(), 2);
}

#[test]
fn motions_on_a_log_that_evicts_while_it_is_open_stay_in_range() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let first: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &first);
    open_viewer(&mut app, 80, 12);
    shrink_viewer_tail(&mut app, 4);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));

    for round in 0..5 {
        let lines: Vec<String> = (0..10 + round * 3).map(|i| format!("line {i}")).collect();
        write_log(&app, "feat+one", "dev", &lines);
        app.handle_event(AppEvent::Tick);
        paint(&mut app, 80, 12);
        let view = viewer(&app);
        assert!(
            view.cursor < view.visible_len().max(1),
            "round {round}: cursor {} of {}",
            view.cursor,
            view.visible_len()
        );
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('k'));
    }
    // And a truncation, which resets the tail to the start of the file.
    write_log(&app, "feat+one", "dev", &["one short line"]);
    app.handle_event(AppEvent::Tick);
    paint(&mut app, 80, 12);
    assert!(viewer(&app).cursor < viewer(&app).visible_len().max(1));
}

// ---- the subprocess and state rules ----------------------------------

/// Every key the viewer binds, in one list, so the guards below can
/// drive the lot.
fn every_viewer_key() -> Vec<KeyEvent> {
    let mut keys = Vec::new();
    for c in [
        'j', 'k', 'g', 'G', 'w', 'f', 'E', 'e', 'y', 'Y', 'J', 'n', 'N', '&', '/', '3', '0', 'z',
    ] {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    for c in ['d', 'u', 'n', 'p'] {
        keys.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }
    for code in [
        KeyCode::Enter,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Backspace,
        KeyCode::Esc,
    ] {
        keys.push(KeyEvent::new(code, KeyModifiers::NONE));
    }
    keys
}

// The origin tool's hard-won rule: a key handler that reads state takes
// the flock and forks a socket scan, and the frame freezes until it is
// done. The viewer reads its own log file on the tick and nothing else.
#[test]
fn no_key_the_viewer_binds_reads_or_writes_state() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &a_busy_log());
    open_viewer(&mut app, 80, 20);
    let before = app.state.clone();
    for key in every_viewer_key() {
        app.handle_key(key);
        paint(&mut app, 80, 20);
    }
    assert_eq!(app.state, before, "the viewer never touches state");
    assert!(
        !app.paths.state_file().exists(),
        "and never writes one either"
    );
    assert!(
        !app.paths.lock_file().exists(),
        "so it never takes the lock a mutation holds"
    );
}

/// A log with something for every key to act on.
fn a_busy_log() -> Vec<String> {
    let mut lines = json_block_log();
    lines.push("ERROR later on".to_string());
    lines.push("ready on http://localhost:17342/".to_string());
    lines
}

// The other half of the rule: a child that inherits the terminal paints
// over the alternate screen. The two commands the TUI is allowed to run
// are the browser opener and `pbcopy`, both on a worker thread with
// every stream redirected.
#[test]
fn the_tui_spawns_nothing_that_could_paint_over_the_screen() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected: [(&str, usize); 12] = [
        ("src/tui/app/mod.rs", 0),
        ("src/tui/app/background.rs", 0),
        ("src/tui/app/dialogs.rs", 0),
        ("src/tui/app/log_keys.rs", 0),
        ("src/tui/app/log_view.rs", 0),
        ("src/tui/app/operations.rs", 2),
        ("src/tui/app/pending.rs", 0),
        ("src/tui/app/tails.rs", 0),
        ("src/tui/app/tests.rs", 0),
        ("src/tui/render.rs", 0),
        ("src/tui/modal.rs", 0),
        ("src/tui/mod.rs", 0),
    ];
    // Built rather than written, so this test does not match itself.
    // `wait` as well as `status`: waiting on a child pando spawned
    // blocks whatever thread asks, and the rule is about the thread,
    // not about which call is used to wait.
    let blocking = [format!(".{}()", "status"), format!(".{}()", "wait")];
    let spawn = format!("Command::{}", "new");
    for (file, allowed) in expected {
        let whole = std::fs::read_to_string(root.join(file)).unwrap();
        // Comments explain the rule; code has to keep it.
        let source: String = whole
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for call in &blocking {
            assert!(
                !source.contains(call.as_str()),
                "{file}: `{call}` blocks on a child, which belongs on a \
                 worker thread and never in a key handler"
            );
        }
        assert_eq!(
            source.matches(&spawn).count(),
            allowed,
            "{file}: every subprocess the TUI runs has to be one of the \
             documented ones, off the UI thread with its streams redirected"
        );
    }
}

#[test]
fn the_viewer_keeps_ten_thousand_lines() {
    assert_eq!(LOG_VIEWER_CAPACITY, 10_000);
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one"]);
    open_viewer(&mut app, 80, 12);
    assert_eq!(viewer(&app).tail.capacity(), LOG_VIEWER_CAPACITY);
}
