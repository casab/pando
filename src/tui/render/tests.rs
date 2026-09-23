use super::*;
use super::{chrome::*, detail::*, list::*, log_viewer::*};
use crate::log_tail::{LogLevel, ParsedLine};
use crate::theme::{
    cyan, green, highlight_bg, red, search_cursor_bg, search_match_bg, text_dim, text_muted, yellow,
};
use crate::tui::app::App;
use crate::tui::app::tests::{
    app_with_logs, running_phase, test_app, with_process, with_second_process, write_log, wt,
};
use crate::tui::app::{BranchLoadState, Modal};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

fn draw(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn text_of(buf: &Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Shared mode runs no containers of pando's, so the header is the only
// place a developer finds out the project's database is down.
#[test]
fn the_header_carries_a_chip_for_each_shared_service() {
    let mut app = test_app(&["feat+one"]);
    app.main = Some(wt("acme-shop"));
    app.service_health = crate::tui::app::ServiceHealth {
        shared: vec![
            crate::actions::ServiceStatus {
                name: "postgres".into(),
                port: Some(5432),
                up: true,
                logging: false,
            },
            crate::actions::ServiceStatus {
                name: "redis".into(),
                port: Some(6379),
                up: false,
                logging: false,
            },
        ],
        worktrees: std::collections::BTreeMap::new(),
    };
    let text = text_of(&draw(&mut app, 100, 12));
    let header = text.lines().next().unwrap_or_default().to_string();
    assert!(header.contains("postgres"), "{header}");
    assert!(header.contains("redis"), "{header}");
    assert!(header.contains("●"), "{header}");
}

// Isolated mode puts them in the detail pane instead, one row apiece,
// because there they belong to a worktree rather than to the project.
#[test]
fn the_detail_pane_has_a_row_for_each_private_service() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    app.service_health = crate::tui::app::ServiceHealth {
        shared: Vec::new(),
        worktrees: std::collections::BTreeMap::from([(
            "feat+one".to_string(),
            vec![
                crate::actions::ServiceStatus {
                    name: "postgres".into(),
                    port: Some(17_004),
                    up: true,
                    logging: false,
                },
                crate::actions::ServiceStatus {
                    name: "redis".into(),
                    port: Some(17_006),
                    up: false,
                    logging: false,
                },
            ],
        )]),
    };
    let text = text_of(&draw(&mut app, 120, 24));
    assert!(text.contains("postgres"), "{text}");
    assert!(text.contains("port 17004"), "{text}");
    assert!(
        text.contains("down      port 17006"),
        "a service that is not answering says so: {text}"
    );
}

#[test]
fn renders_at_any_terminal_size_without_panicking() {
    let mut app = test_app(&["feat+one", "feat+two", "a-very-long-worktree-name-here"]);
    app.worktrees[1].dirty = Some(true);
    app.worktrees[2].prunable = true;
    app.main = Some(wt("acme-shop"));
    // The detail pane has something to paint in every phase, including
    // a failure whose reason is longer than any pane.
    with_process(&mut app, "feat+one", running_phase());
    with_process(
        &mut app,
        "feat+two",
        crate::state::Phase::Starting {
            since: chrono::Utc::now(),
        },
    );
    with_process(
        &mut app,
        "a-very-long-worktree-name-here",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited — something else is listening on port 17342; stop it, \
                     or `pando stop` the worktree that owns it"
                .into(),
        },
    );
    // And a worktree running two processes, which adds a row per
    // process to a pane that may have room for none of them.
    with_second_process(
        &mut app,
        "feat+one",
        "api",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited".into(),
        },
    );
    // Private services add another row apiece to the same pane, and
    // another chip apiece to a header that may be one column wide.
    app.service_health = crate::tui::app::ServiceHealth {
        shared: vec![
            crate::actions::ServiceStatus {
                name: "postgres".into(),
                port: Some(5432),
                up: true,
                logging: false,
            },
            crate::actions::ServiceStatus {
                name: "an-extremely-long-service-name".into(),
                port: None,
                up: false,
                logging: false,
            },
        ],
        worktrees: std::collections::BTreeMap::from([(
            "feat+one".to_string(),
            vec![
                crate::actions::ServiceStatus {
                    name: "postgres".into(),
                    port: Some(17_004),
                    up: true,
                    logging: false,
                },
                crate::actions::ServiceStatus {
                    name: "an-extremely-long-service-name".into(),
                    port: None,
                    up: false,
                    logging: false,
                },
            ],
        )]),
    };
    // And a public URL, which adds a marker to every list row and a
    // line to a detail pane that may have room for neither.
    crate::tui::app::tests::with_share(&mut app, "feat+one", Some(17_009));

    for width in 1..=120u16 {
        for height in [1u16, 2, 3, 5, 12, 40] {
            draw(&mut app, width, height);
        }
    }
    // Well past the width where a percentage of it stops fitting in a
    // u16 (1093 * 60 overflows), and as tall again: a large display with
    // a small font really does reach four digits.
    for width in [1092u16, 1093, 1500, 2000, 3000] {
        for height in [1u16, 3, 40] {
            draw(&mut app, width, height);
        }
    }
    for height in [1092u16, 1093, 3000] {
        draw(&mut app, 80, height);
    }
}

#[test]
fn renders_every_modal_at_any_terminal_size() {
    let (reply, _rx) = std::sync::mpsc::channel();
    let modals = [
        Modal::Help,
        Modal::Messages,
        Modal::Create {
            input: "feat/new".into(),
            branches: BranchLoadState::Loading,
            selected: 0,
            base: Some("origin/some-rather-long-release-branch".into()),
        },
        Modal::Remove {
            name: "feat+one".into(),
            created_by_pando: false,
        },
        Modal::Unshare {
            name: "feat+one".into(),
            url: "https://a-rather-long-quick-tunnel-hostname.trycloudflare.com".into(),
        },
        Modal::StopAll {
            names: (0..20).map(|i| format!("feat+number-{i}")).collect(),
        },
        Modal::Question {
            question: crate::actions::Question {
                slot: crate::detect::Slot::DevCmd,
                prompt: "Which command starts the local development server?".into(),
                options: (0..12)
                    .map(|i| {
                        (
                            format!("pnpm dev:{i}"),
                            format!("package.json scripts.dev:{i}"),
                        )
                    })
                    .collect(),
                preselect: Some(11),
                allow_custom: true,
                allow_none: false,
                multi: false,
                checked: Vec::new(),
                details: Vec::new(),
                answer_file: None,
                snippet: String::new(),
            },
            selected: 11,
            custom: Some("a rather long command typed by hand".into()),
            reply,
        },
    ];
    for modal in modals {
        let mut app = test_app(&["feat+one"]);
        app.modal = Some(modal);
        for width in [1u16, 4, 20, 41, 80, 200, 1092, 1093, 1500, 2000, 3000] {
            for height in [1u16, 3, 8, 24] {
                draw(&mut app, width, height);
            }
        }
        for height in [1092u16, 1093, 3000] {
            draw(&mut app, 80, height);
        }
    }
}

/// A log with one of everything the viewer has to paint: ANSI colour, a
/// multi-line JSON block, errors and warnings, a duration, a URL that
/// has to wrap, wide glyphs, and a line longer than any terminal.
fn a_log_of_everything() -> Vec<String> {
    vec![
        "\x1b[32mready\x1b[0m in 85ms".to_string(),
        "listening on https://a-rather-long-hostname.example.com/a/b/c".to_string(),
        "WARN slow query took 450 ms".to_string(),
        "{".to_string(),
        "  \"level\": \"error\",".to_string(),
        "  \"msg\": \"boom\",".to_string(),
        "  \"detail\": \"…\"".to_string(),
        "}".to_string(),
        "日本語のログ行 with 🎉 emoji and e\u{0301} combining".to_string(),
        // A date-ish token eleven bytes after a multi-byte character:
        // the timestamp match starts inside the character unless the
        // parser checks.
        "日 21-09-26T10:00:00 起動".to_string(),
        "x".repeat(4000),
        "ERROR the last one".to_string(),
    ]
}

#[test]
fn renders_the_viewer_at_any_terminal_size_without_panicking() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &a_log_of_everything());
    write_log(&app, "feat+one", "install", &["installed"]);
    app.open_log_viewer();

    // One pass per state the viewer can be painted in. The keys are
    // pressed between passes, so each sweep starts from the one before.
    let states: [&[KeyEvent]; 8] = [
        // Following the live tail, which is how it opens.
        &[],
        // A cursor part way up, with wrap off.
        &[
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        ],
        // A query being typed.
        &[
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        ],
        // And confirmed.
        &[KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)],
        // Collapsed to the matches.
        &[KeyEvent::new(KeyCode::Char('&'), KeyModifiers::NONE)],
        // Errors only, with a count prefix half typed.
        &[
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE),
        ],
        // The inspect overlay over all of it.
        &[KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE)],
        // And the help overlay over that.
        &[
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        ],
    ];
    for keys in states {
        for key in keys {
            app.handle_key(*key);
        }
        for width in 1..=120u16 {
            for height in [1u16, 2, 3, 5, 12, 40] {
                draw(&mut app, width, height);
            }
        }
        // Past the width where a percentage of it stops fitting in a
        // u16 (1093 * 60 overflows), and as tall again.
        for width in [1092u16, 1093, 1500, 2000, 3000] {
            for height in [1u16, 3, 40] {
                draw(&mut app, width, height);
            }
        }
        for height in [1092u16, 1093, 3000] {
            draw(&mut app, 80, height);
        }
    }
}

#[test]
fn renders_a_viewer_with_no_log_and_one_with_nothing_in_it_at_any_size() {
    for lines in [Vec::<String>::new(), vec![String::new()]] {
        let (_dir, mut app) = app_with_logs(&["feat+one"]);
        if !lines.is_empty() {
            write_log(&app, "feat+one", "dev", &lines);
        }
        app.open_log_viewer();
        for width in [1u16, 2, 5, 40, 120, 1093, 3000] {
            for height in [1u16, 2, 3, 40] {
                draw(&mut app, width, height);
            }
        }
    }
}

// Wide and zero-width glyphs are counted as one cell by every budget in
// here; what must never happen is a panic or a lost line.
#[test]
fn a_line_of_wide_glyphs_paints_at_every_width() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "日本語".repeat(40),
            "🎉🎉🎉".repeat(20),
            "e\u{0301}".repeat(60),
        ],
    );
    app.open_log_viewer();
    for width in 1..=60u16 {
        draw(&mut app, width, 10);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    }
}

#[test]
fn an_escape_the_terminal_cannot_read_is_painted_as_text_not_as_an_escape() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[
            "\x1b[38;5;mbroken \x1b[999Xthing",
            "a bare \x1b in the middle",
            "trailing escape \x1b[",
            "\x1b[mzero length",
        ],
    );
    app.open_log_viewer();
    let buffer = draw(&mut app, 60, 12);
    let painted: String = text_of(&buffer);
    assert!(
        !painted.contains('\x1b'),
        "an escape must never reach the screen as text: {painted:?}"
    );
    assert!(painted.contains("thing"), "{painted}");
    assert!(painted.contains("zero length"), "{painted}");
}

#[test]
fn a_real_ansi_colour_reaches_the_screen_as_colour() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["\x1b[32mready\x1b[0m now"]);
    app.open_log_viewer();
    let buffer = draw(&mut app, 60, 8);
    let painted = text_of(&buffer);
    assert!(painted.contains("ready now"), "{painted}");
    assert!(!painted.contains("[32m"), "never as text: {painted}");
    let colours: Vec<_> = (0..60)
        .map(|x| buffer.cell((x, 1)).unwrap().style().fg)
        .collect();
    assert!(
        colours.contains(&Some(ratatui::style::Color::Green)),
        "the escape painted the colour it asked for: {colours:?}"
    );
}

#[test]
fn renders_an_empty_list_and_a_filter_with_no_matches() {
    // Wide enough that the list pane, now sharing the body with the
    // detail pane, still fits the sentence.
    let mut app = test_app(&[]);
    let first_run = text_of(&draw(&mut app, 120, 24));
    assert!(first_run.contains("welcome"), "{first_run}");
    assert!(first_run.contains("create a worktree"), "{first_run}");

    let mut app = test_app(&["feat+one"]);
    for code in [KeyCode::Char('/'), KeyCode::Char('z'), KeyCode::Char('z')] {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
    let rendered = text_of(&draw(&mut app, 120, 10));
    assert!(rendered.contains("no matches"), "{rendered}");
}

#[test]
fn the_header_shows_the_project_branch_and_count() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    app.main = Some(wt("acme-shop"));
    let rendered = text_of(&draw(&mut app, 80, 10));
    assert!(rendered.contains("acme-shop"), "{rendered}");
    assert!(rendered.contains("2 worktrees"), "{rendered}");
}

#[test]
fn the_header_gives_the_whole_bar_to_a_status_message() {
    let mut app = test_app(&["feat+one"]);
    app.main = Some(wt("acme-shop"));
    app.set_status("created feat+one");
    let first_line = text_of(&draw(&mut app, 80, 10))
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(first_line.contains("created feat+one"), "{first_line}");
    assert!(!first_line.contains("worktrees"), "{first_line}");
}

fn every_column(width: usize) -> Vec<(Col, usize)> {
    vec![
        (Col::Aside, 10),
        (Col::Status, 7),
        (Col::Url, 22),
        (Col::Port, 6),
        (Col::Share, 1),
        (Col::Mode, 8),
        (Col::Signals, width),
        (Col::Pr, 4),
        (Col::Adopted, 7),
    ]
}

#[test]
fn list_columns_shed_the_least_useful_first_and_the_status_last() {
    let all = every_column(4);
    assert_eq!(
        list_columns(200, &all),
        vec![
            Col::Aside,
            Col::Status,
            Col::Url,
            Col::Share,
            Col::Mode,
            Col::Signals,
            Col::Pr,
            Col::Adopted
        ],
        "wide enough for everything, and the port is not shown beside its URL"
    );
    let medium = list_columns(70, &all);
    assert!(!medium.contains(&Col::Aside), "{medium:?}");
    assert!(!medium.contains(&Col::Adopted), "{medium:?}");
    assert!(
        medium.contains(&Col::Status) && medium.contains(&Col::Url),
        "{medium:?}"
    );

    let narrow = list_columns(40, &all);
    assert!(
        narrow.contains(&Col::Port) && !narrow.contains(&Col::Url),
        "the URL shrinks to its port before it goes: {narrow:?}"
    );
    assert!(narrow.contains(&Col::Status), "{narrow:?}");

    assert_eq!(
        list_columns(16, &all),
        Vec::<Col>::new(),
        "a sliver keeps only the glyph and the label"
    );
}

#[test]
fn list_columns_keep_everything_when_the_optional_columns_are_empty() {
    let empty: Vec<(Col, usize)> = every_column(0).into_iter().map(|(c, _)| (c, 0)).collect();
    assert_eq!(
        list_columns(20, &empty),
        Vec::<Col>::new(),
        "columns with no content cost nothing"
    );
}

#[test]
fn a_narrow_list_still_shows_the_name() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 30, 8));
    assert!(rendered.contains("feat/one"), "{rendered}");
}

/// The list pane's part of the row that mentions `needle`.
fn list_row(rendered: &str, needle: &str) -> String {
    rendered
        .lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| line.split("││").next().unwrap_or_default())
        .find(|row| row.contains(needle))
        .unwrap_or_default()
        .to_string()
}

// The branch is the name people use. The directory it lives in is shown
// only when it is not simply the branch with its slashes encoded.
#[test]
fn a_row_is_labelled_by_its_branch() {
    let mut app = test_app(&["feat+one", "scratch"]);
    app.worktrees[0].branch = Some("feat/one".into());
    app.worktrees[1].branch = Some("fix/typo".into());
    let rendered = text_of(&draw(&mut app, 120, 10));
    let one = list_row(&rendered, "feat/one");
    assert!(
        !one.contains("feat+one"),
        "the encoded name adds nothing:\n{rendered}"
    );
    assert!(
        list_row(&rendered, "fix/typo").contains("scratch"),
        "a directory named otherwise says so:\n{rendered}"
    );
}

#[test]
fn a_row_says_what_it_is_doing_and_where() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    crate::tui::app::tests::with_process(
        &mut app,
        "feat+one",
        crate::tui::app::tests::running_phase(),
    );
    let rendered = text_of(&draw(&mut app, 140, 10));
    let row = |name: &str| list_row(&rendered, name);
    assert!(row("feat/one").contains("running"), "{rendered}");
    assert!(
        row("feat/one").contains("http://localhost:17342"),
        "{rendered}"
    );
    assert!(row("feat/two").contains("stopped"), "{rendered}");
    assert!(
        !row("feat/two").contains("localhost"),
        "a stopped row promises no page:\n{rendered}"
    );
}

#[test]
fn the_row_marks_adopted_worktrees_in_words() {
    let mut app = test_app(&["mine", "theirs"]);
    app.created_by_pando.insert("mine".into(), true);
    app.created_by_pando.insert("theirs".into(), false);
    let rendered = text_of(&draw(&mut app, 100, 10));
    let row = |name: &str| {
        rendered
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_default()
            .to_string()
    };
    assert!(row("theirs").contains("adopted"), "{rendered}");
    assert!(!row("mine").contains("adopted"), "{rendered}");
}

// Thirty branches that share a long prefix must still read as thirty
// different rows.
#[test]
fn long_labels_keep_the_part_that_tells_them_apart() {
    let names: Vec<String> = (1..=12)
        .map(|n| format!("feature+very-long-branch-name-number-{n}-with-extra-words"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut app = test_app(&refs);
    for wt in &mut app.worktrees {
        wt.branch = Some(wt.name.replace('+', "/"));
    }
    let rendered = text_of(&draw(&mut app, 80, 20));
    for n in [1, 7, 12] {
        assert!(
            rendered.contains(&format!("number-{n}-")),
            "row {n} is told apart:\n{rendered}"
        );
    }
}

#[test]
fn truncate_distinct_cuts_where_it_costs_nothing() {
    let label = "feature/very-long-branch-name-number-12-with-extra-words";
    let at = label.find("12").unwrap();
    let cut = truncate_distinct(label, 24, at);
    assert!(cut.contains("12"), "{cut}");
    assert!(cut.starts_with("feature/…"), "{cut}");
    assert_eq!(cut.chars().count(), 24, "{cut}");
    assert_eq!(
        truncate_distinct("feat/short-and-different", 12, 2),
        "feat/short-…",
        "a difference near the front is kept by a plain cut"
    );
}

#[test]
fn distinct_offsets_find_where_each_label_leaves_its_nearest_neighbour() {
    let labels: Vec<String> = ["feat/a1", "feat/a2", "fix/b"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(distinct_offsets(&labels), vec![6, 6, 1]);
}

#[test]
fn truncate_middle_keeps_both_ends() {
    assert_eq!(truncate_middle("abcdefghij", 10), "abcdefghij");
    let cut = truncate_middle("/very/long/path/to/worktrees/feat+one", 20);
    assert_eq!(cut.chars().count(), 20, "{cut}");
    assert!(cut.starts_with("/very"), "{cut}");
    assert!(cut.ends_with("feat+one"), "{cut}");
}

#[test]
fn wrap_text_breaks_at_spaces_and_cuts_only_what_it_must() {
    assert_eq!(
        wrap_text("isolated mode needs Docker", 12),
        vec!["isolated", "mode needs", "Docker"]
    );
    assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    assert_eq!(wrap_text("", 4), vec![""]);
}

#[test]
fn a_shared_worktree_is_marked_in_the_list_and_only_then() {
    let mut app = test_app(&["feat+one", "feat+two"]);
    let plain = text_of(&draw(&mut app, 100, 10));
    assert!(
        !plain.contains('◈'),
        "no row is shared, so no row pays for the column:\n{plain}"
    );

    crate::tui::app::tests::with_process(
        &mut app,
        "feat+one",
        crate::tui::app::tests::running_phase(),
    );
    crate::tui::app::tests::with_share(&mut app, "feat+one", None);
    let shared = text_of(&draw(&mut app, 120, 10));
    assert!(list_row(&shared, "feat/one").contains('◈'), "{shared}");
    assert!(!list_row(&shared, "feat/two").contains('◈'), "{shared}");
}

#[test]
fn the_detail_pane_shows_the_public_url_under_the_local_one() {
    let mut app = test_app(&["feat+one"]);
    crate::tui::app::tests::with_process(
        &mut app,
        "feat+one",
        crate::tui::app::tests::running_phase(),
    );
    crate::tui::app::tests::with_share(&mut app, "feat+one", Some(17_009));

    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("public"), "{rendered}");
    assert!(
        rendered.contains("https://fake-host.trycloudflare.com"),
        "{rendered}"
    );
    let local = rendered.find("http://localhost").expect("a local url");
    let public = rendered.find("https://fake-host").expect("a public url");
    assert!(local < public, "the public URL goes under the local one");
}

#[test]
fn signals_report_gone_locked_dirty_and_drift() {
    let mut gone = wt("g");
    gone.prunable = true;
    assert_eq!(signal_text(&gone), "gone");

    let mut locked = wt("l");
    locked.locked = true;
    assert_eq!(signal_text(&locked), "locked");

    let mut dirty = wt("d");
    dirty.dirty = Some(true);
    dirty.ahead_behind = Some((2, 3));
    assert_eq!(signal_text(&dirty), "*↑2↓3");

    let mut clean = wt("c");
    clean.dirty = Some(false);
    clean.ahead_behind = Some((0, 0));
    assert_eq!(signal_text(&clean), "");

    let mut lots = wt("m");
    lots.dirty = Some(false);
    lots.ahead_behind = Some((250, 0));
    assert_eq!(signal_text(&lots), "↑99+");
}

#[test]
fn keep_hints_drops_optional_hints_from_the_tail_first() {
    let items = [(5, true), (4, false), (6, false), (4, true)];
    assert_eq!(keep_hints(&items, 3, 100), vec![true, true, true, true]);
    assert_eq!(
        keep_hints(&items, 3, 22),
        vec![true, true, false, true],
        "the last optional hint goes first"
    );
    assert_eq!(keep_hints(&items, 3, 12), vec![true, false, false, true]);
    assert_eq!(
        keep_hints(&items, 3, 1),
        vec![true, false, false, true],
        "essentials survive even when they cannot fit"
    );
}

#[test]
fn truncate_adds_an_ellipsis_only_when_it_cuts() {
    assert_eq!(truncate("short", 10), "short");
    assert_eq!(truncate("abcdefghij", 5), "abcd…");
    assert_eq!(truncate("abc", 0), "");
    assert_eq!(truncate("", 5), "");
}

#[test]
fn truncate_line_keeps_the_surviving_spans_styled() {
    let line = Line::from(vec![
        Span::styled("abc", Style::new().fg(green())),
        Span::styled("defgh", Style::new().fg(red())),
    ]);
    let cut = truncate_line(line, 5);
    assert_eq!(cut.spans.len(), 2);
    assert_eq!(cut.spans[0].content, "abc");
    assert_eq!(cut.spans[1].content, "d…");
    assert_eq!(cut.spans[1].style.fg, Some(red()));
}

#[test]
fn centered_rect_honours_the_minimum_width_and_never_escapes_the_area() {
    let area = Rect::new(0, 0, 100, 40);
    let popup = centered_rect(50, 40, 10, area);
    assert_eq!(popup.width, 50);

    let narrow = Rect::new(0, 0, 20, 40);
    let popup = centered_rect(50, 40, 10, narrow);
    assert_eq!(popup.width, 20, "a popup never grows past its area");

    let short = Rect::new(0, 0, 100, 4);
    let popup = centered_rect(50, 40, 10, short);
    assert!(popup.height <= 4);
}
// ---- the two panes ---------------------------------------------------

#[test]
fn a_wide_body_puts_the_panes_side_by_side() {
    let body = Rect::new(0, 0, 120, 30);
    let [list, detail] = body_layout(body);
    assert_eq!(list.y, detail.y, "same row means side by side");
    assert!(list.width > 20 && detail.width > 20);
    assert_eq!(list.width + detail.width, 120);
}

// A tmux split is the normal case, so the narrow layout is not an edge.
#[test]
fn a_narrow_body_stacks_the_panes() {
    let body = Rect::new(0, 0, 50, 30);
    let [list, detail] = body_layout(body);
    assert_eq!(list.x, detail.x);
    assert_eq!(list.width, detail.width, "stacked panes are full width");
    assert!(detail.y >= list.y + list.height);
    assert!(list.height >= STACK_MIN_LIST_HEIGHT);
    assert!(detail.height >= STACK_MIN_DETAIL_HEIGHT);
}

// Too short to stack: two panes of three rows each help nobody, so a
// cramped side-by-side wins.
#[test]
fn a_narrow_and_short_body_stays_side_by_side() {
    let [list, detail] = body_layout(Rect::new(0, 0, 50, 8));
    assert_eq!(list.y, detail.y);
}

#[test]
fn the_detail_pane_sheds_its_least_useful_rows_first() {
    let rows: Vec<(u8, Line)> = vec![
        (KEEP_ALWAYS, Line::raw("branch")),
        (KEEP_ALWAYS, Line::raw("status")),
        (KEEP_URL, Line::raw("url")),
        (KEEP_PR, Line::raw("pr")),
        (KEEP_HEAD, Line::raw("head")),
        (KEEP_PATH, Line::raw("path")),
    ];
    let kept = |budget: usize| {
        fit_detail_rows(rows.clone(), budget)
            .iter()
            .map(|l| l.spans[0].content.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(kept(6).len(), 6);
    assert_eq!(kept(5), vec!["branch", "status", "url", "pr", "head"]);
    assert_eq!(kept(3), vec!["branch", "status", "url"]);
    assert_eq!(
        kept(1),
        vec!["branch", "status"],
        "the rows the pane exists for are never shed, even when they clip"
    );
}

// ---- what the detail pane says ---------------------------------------

#[test]
fn the_detail_pane_shows_the_url_ports_and_uptime_of_a_running_worktree() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(rendered.contains("running"), "{rendered}");
    assert!(rendered.contains("http://localhost:17342"), "{rendered}");
    assert!(rendered.contains("web 17342"), "{rendered}");
    assert!(rendered.contains("pid 4242"), "{rendered}");
}

#[test]
fn a_stopped_worktree_is_told_how_to_start() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(rendered.contains("○ stopped"), "{rendered}");
    assert!(rendered.contains("⏎ starts it"), "{rendered}");
}

#[test]
fn a_failure_shows_its_reason_in_the_detail_pane() {
    let mut app = test_app(&["feat+one"]);
    with_process(
        &mut app,
        "feat+one",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited; dependencies are missing".into(),
        },
    );
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(rendered.contains("failed"), "{rendered}");
    assert!(rendered.contains("dependencies are missing"), "{rendered}");
}

#[test]
fn the_list_marks_what_each_worktree_is_doing() {
    let mut app = test_app(&["up", "broken"]);
    with_process(&mut app, "up", running_phase());
    with_process(
        &mut app,
        "broken",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited".into(),
        },
    );
    let rendered = text_of(&draw(&mut app, 120, 20));
    let up = list_row(&rendered, "up ");
    assert!(up.contains("● up") && up.contains("running"), "{rendered}");
    let broken = list_row(&rendered, "broken");
    assert!(
        broken.contains("✗ broken") && broken.contains("failed"),
        "{rendered}"
    );
}

#[test]
fn the_tail_paints_the_last_lines_of_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("dev.log");
    std::fs::write(&log, "ready in 412ms\nError: it broke\nwarn: slow\n").unwrap();

    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    // Keyed by worktree *and* process: a worktree has one log per
    // process, and the tail shows one of them at a time.
    let (key, process, _) = app.tail_target().expect("a process to tail");
    assert_eq!(process, "dev");
    app.log_tails.touch(&key, log).poll().unwrap();

    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("ready in 412ms"), "{rendered}");
    assert!(rendered.contains("Error: it broke"), "{rendered}");
    assert!(rendered.contains("3 lines"), "{rendered}");
}

#[test]
fn the_detail_pane_lists_every_process_with_its_phase() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(
        &mut app,
        "feat+one",
        "api",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited".into(),
        },
    );
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(
        rendered.contains("✗ failed") && rendered.contains("api: process exited"),
        "the worktree is failed, and says which process: {rendered}"
    );
    assert!(
        rendered.contains("api") && rendered.contains("dev"),
        "both processes have a row: {rendered}"
    );
    assert!(
        rendered.contains("pid 4242"),
        "each row carries its own pid: {rendered}"
    );
}

// One process has nothing to disambiguate, and a tmux split has no rows
// to spare for saying the same thing twice.
#[test]
fn one_process_gets_no_row_of_its_own() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert_eq!(
        rendered.matches("pid 4242").count(),
        1,
        "the status row is the process's status: {rendered}"
    );
}

#[test]
fn the_list_row_shows_the_aggregate_not_the_first_process() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(
        &mut app,
        "feat+one",
        "api",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "process exited".into(),
        },
    );
    let rendered = text_of(&draw(&mut app, 120, 24));
    let row = list_row(&rendered, "feat/one");
    assert!(
        row.contains("✗ feat/one") && row.contains("failed"),
        "a worktree with a dead process is not running: {rendered}"
    );
}

#[test]
fn the_tail_header_names_the_process_it_is_showing() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("log · api"), "{rendered}");
    assert!(
        rendered.contains("tab switches"),
        "and says how to see the other one: {rendered}"
    );
    assert!(
        rendered.contains("p restarts it"),
        "and how to restart just this one: {rendered}"
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("log · dev"), "{rendered}");
}

// Level colours are what make an error findable in a wall of output.
#[test]
fn log_levels_are_painted_in_their_own_colours() {
    assert_eq!(level_color(LogLevel::Error), red());
    assert_eq!(level_color(LogLevel::Warn), yellow());
    assert_ne!(level_color(LogLevel::Info), red());
}

#[test]
fn a_worktree_with_no_log_yet_says_so_rather_than_painting_nothing() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("no log yet"), "{rendered}");
}

#[test]
fn a_filter_that_hides_everything_leaves_the_detail_pane_saying_so() {
    let mut app = test_app(&["feat+one"]);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(
        rendered.contains("no worktree matches the filter"),
        "{rendered}"
    );
}

// The footer offers what the selected row can do: a stopped one is
// started, a running one is read, stopped or restarted.
#[test]
fn the_footer_offers_the_keys_the_selected_row_needs() {
    let mut app = test_app(&["feat+one"]);
    let stopped = text_of(&draw(&mut app, 120, 20));
    assert!(stopped.contains("⏎ start"), "{stopped}");
    assert!(stopped.contains("i isolated"), "{stopped}");
    assert!(!stopped.contains("x stop"), "{stopped}");

    with_process(&mut app, "feat+one", running_phase());
    let running = text_of(&draw(&mut app, 120, 20));
    assert!(running.contains("⏎ logs"), "{running}");
    assert!(running.contains("x stop"), "{running}");
    assert!(running.contains("r restart"), "{running}");
}

// Finding 9. Sharing is this phase's whole feature and neither of its
// keys was in the footer at any width, while `o open` was.
#[test]
fn the_footer_offers_the_share_keys_on_a_wide_terminal() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let rendered = text_of(&draw(&mut app, 160, 20));
    assert!(rendered.contains("t share"), "{rendered}");
    crate::tui::app::tests::with_share(&mut app, "feat+one", None);
    let shared = text_of(&draw(&mut app, 160, 20));
    assert!(shared.contains("O public"), "{shared}");
    assert!(shared.contains("Y copy URL"), "{shared}");
}

// …and they are optional, so a narrow terminal sheds them rather than
// anything essential, and nothing clips.
#[test]
fn the_share_keys_go_before_anything_essential_when_the_footer_will_not_fit() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let rendered = text_of(&draw(&mut app, 60, 20));
    for essential in ["j/k move", "⏎ logs", "x stop", "? help", "q quit"] {
        assert!(rendered.contains(essential), "{essential}: {rendered}");
    }
    for line in rendered.lines() {
        assert!(line.chars().count() <= 60, "{line:?}");
    }
}

// ---- the log viewer --------------------------------------------------

/// Opens the viewer on the first worktree and paints one frame.
fn viewer_frame(app: &mut App, width: u16, height: u16) -> String {
    app.open_log_viewer();
    text_of(&draw(app, width, height))
}

#[test]
fn the_viewer_names_the_worktree_and_the_source_it_is_showing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["listening on 17342"]);
    let painted = viewer_frame(&mut app, 80, 14);
    assert!(painted.contains("feat/one · dev"), "{painted}");
    assert!(painted.contains("listening on 17342"), "{painted}");
}

#[test]
fn the_viewer_paints_a_tab_per_source_and_marks_the_active_one() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["up"]);
    write_log(&app, "feat+one", "install", &["installed"]);
    let painted = viewer_frame(&mut app, 80, 14);
    assert!(painted.contains("1:dev"), "{painted}");
    assert!(painted.contains("2:install"), "{painted}");
}

#[test]
fn a_single_source_gets_no_tab_bar_at_all() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["up"]);
    let painted = viewer_frame(&mut app, 80, 14);
    assert!(
        !painted.contains("1:dev"),
        "one source needs no tabs:\n{painted}"
    );
}

#[test]
fn narrow_tabs_keep_the_active_label_and_shrink_the_rest_to_digits() {
    let sources = vec![
        "dev".to_string(),
        "api".to_string(),
        "install".to_string(),
        "migrate".to_string(),
    ];
    let wide = source_tabs(&sources, "api", 80);
    let wide_text: String = wide.spans.iter().map(|s| s.content.to_string()).collect();
    assert!(wide_text.contains("1:dev"), "{wide_text}");
    assert!(wide_text.contains("4:migrate"), "{wide_text}");

    let narrow = source_tabs(&sources, "api", 20);
    let narrow_text: String = narrow.spans.iter().map(|s| s.content.to_string()).collect();
    assert!(
        narrow_text.contains("2:api"),
        "the active one keeps its label"
    );
    assert!(!narrow_text.contains("1:dev"), "{narrow_text}");
    assert!(narrow_text.chars().count() <= 20, "{narrow_text}");
}

#[test]
fn the_footer_says_where_the_cursor_is() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let lines: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    write_log(&app, "feat+one", "dev", &lines);
    let painted = viewer_frame(&mut app, 80, 14);
    assert!(
        painted.contains("FOLLOW"),
        "it opens on the live tail:\n{painted}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 14));
    assert!(
        painted.contains("1/30"),
        "one-based, out of what is shown:\n{painted}"
    );
}

#[test]
fn a_missing_log_says_so_rather_than_painting_an_empty_pane() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let painted = viewer_frame(&mut app, 80, 14);
    assert!(painted.contains("no log file"), "{painted}");
}

#[test]
fn the_cursor_line_is_painted_across_the_whole_width() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["short", "also short"]);
    app.open_log_viewer();
    draw(&mut app, 40, 10);
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    let buffer = draw(&mut app, 40, 10);
    // The first body row is the cursor line; every cell of it, padding
    // included, carries the highlight.
    let row = 1;
    let painted: Vec<_> = (1..39)
        .map(|x| buffer.cell((x, row)).unwrap().style().bg)
        .collect();
    assert!(
        painted.iter().all(|bg| *bg == Some(highlight_bg())),
        "the cursor bar spans the viewport: {painted:?}"
    );
}

#[test]
fn a_line_longer_than_the_viewer_wraps_and_truncates_with_w() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["a line".to_string(), "ab".repeat(60)],
    );
    app.open_log_viewer();
    let wrapped = text_of(&draw(&mut app, 40, 14));
    assert!(
        wrapped.lines().filter(|row| row.contains("abab")).count() > 3,
        "the long line wraps over several rows:\n{wrapped}"
    );
    // Wrap off, and the cursor on the *other* line: the cursor line
    // always renders in full, so the long one is the one that cuts.
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    let truncated = text_of(&draw(&mut app, 40, 14));
    assert!(truncated.contains('…'), "wrap off truncates:\n{truncated}");
    assert_eq!(
        truncated.lines().filter(|row| row.contains("abab")).count(),
        1,
        "to exactly one row:\n{truncated}"
    );
}

// ---- yank ------------------------------------------------------------

#[test]
fn a_yank_confirmation_is_visible_in_the_viewers_footer() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a line worth copying"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(
        painted.contains("copied line"),
        "the viewer paints full screen, so the footer has to say it:\n{painted}"
    );
}

#[test]
fn the_footer_returns_to_its_hints_once_the_confirmation_expires() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a line"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(text_of(&draw(&mut app, 80, 12)).contains("copied"));
    app.status.as_mut().unwrap().at =
        std::time::Instant::now() - std::time::Duration::from_secs(60);
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(!painted.contains("copied"), "{painted}");
    assert!(painted.contains("j/k move"), "{painted}");
}

// Phase 2c review, finding 4. The tab list is rebuilt from disk every
// paint, so a deleted file drops out of it — and the viewer was left
// on a tab that no longer existed, with `tab` a no-op because only one
// source was left. The only way out was to close the viewer.
#[test]
fn a_source_whose_file_is_deleted_keeps_its_tab_and_says_the_file_is_gone() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one"]);
    write_log(&app, "feat+one", "install", &["two"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    draw(&mut app, 80, 12);
    assert_eq!(
        app.log_view().expect("the viewer is open").source,
        "install"
    );

    std::fs::remove_file(app.paths.log_file("feat+one", "install")).unwrap();
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(
        painted.contains("log deleted"),
        "the title has to say the file is gone:\n{painted}"
    );
    assert!(
        painted.contains("2:install"),
        "and the tab has to stay, or nothing can leave it:\n{painted}"
    );

    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    draw(&mut app, 80, 12);
    assert_eq!(
        app.log_view().expect("the viewer is open").source,
        "dev",
        "tab leaves the dead source"
    );
}

// Phase 2c review, finding 6. `&` collapses to the search matches; with
// none, the empty body blamed the level filter — while the filter was
// `all` and `f` could not have helped.
#[test]
fn a_grep_collapse_with_no_matches_says_the_query_matched_nothing() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["one", "two"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    for key in ['/', 'z', 'z', 'z', 'z'] {
        app.handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('&'), KeyModifiers::NONE));

    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(
        painted.contains("no line matches"),
        "the query is what hid them:\n{painted}"
    );
    assert!(
        !painted.contains("press f to change the filter"),
        "the level filter is `all`, so `f` cannot help:\n{painted}"
    );
}

// Phase 2c review, finding 7. `new_below` counts arrivals and is never
// reduced when the ring evicts the very lines it counted, so the badge
// offered to jump to more lines than the buffer holds.
#[test]
fn the_new_below_badge_counts_no_more_than_the_lines_under_the_cursor() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["a", "b", "c", "d", "e"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    // What twenty ticks of appends into a full ring buffer leave
    // behind: a count of arrivals with nothing like that many lines
    // below the cursor any more.
    app.log_view_mut().expect("the viewer is open").new_below = 999;

    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(
        painted.contains("↓ 3 new"),
        "three lines are below the cursor:\n{painted}"
    );
    assert!(!painted.contains("999"), "{painted}");
}

#[test]
fn a_yank_from_the_overlay_is_confirmed_on_the_overlay() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["{\"a\":1}"]);
    app.open_log_viewer();
    draw(&mut app, 80, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 20));
    assert!(painted.contains("copied"), "{painted}");
}

// ---- gutters, durations and URL wrapping -----------------------------

#[test]
fn every_row_of_a_line_carries_a_gutter_coloured_by_severity() {
    for (line, want) in [
        ("ERROR boom", red()),
        ("WARN slow", yellow()),
        ("just info", text_muted()),
    ] {
        let (_dir, mut app) = app_with_logs(&["feat+one"]);
        write_log(&app, "feat+one", "dev", &[line]);
        app.open_log_viewer();
        let buffer = draw(&mut app, 40, 10);
        let gutter = buffer.cell((1, 1)).unwrap();
        assert_eq!(gutter.symbol(), "▎", "{line}");
        assert_eq!(gutter.style().fg, Some(want), "{line}");
    }
}

#[test]
fn a_wrapped_line_keeps_one_continuous_gutter() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &[format!("ERROR {}", "x".repeat(90))],
    );
    app.open_log_viewer();
    let buffer = draw(&mut app, 40, 10);
    // Three body rows of one long error line, each with the same bar.
    for row in 1..4 {
        let gutter = buffer.cell((1, row)).unwrap();
        assert_eq!(gutter.symbol(), "▎", "row {row}");
        assert_eq!(gutter.style().fg, Some(red()), "row {row}");
    }
}

#[test]
fn a_json_block_is_bracketed_in_the_gutter() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["{", "  \"msg\": \"hi\",", "  \"n\": 1", "}"],
    );
    app.open_log_viewer();
    let buffer = draw(&mut app, 40, 10);
    let glyphs: Vec<&str> = (1..5)
        .map(|row| buffer.cell((1, row)).unwrap().symbol())
        .collect();
    assert_eq!(
        glyphs,
        vec!["╭", "│", "│", "╰"],
        "one entry reads as one unit"
    );
}

#[test]
fn a_line_that_is_not_in_a_block_keeps_the_plain_bar() {
    let buffer: std::collections::VecDeque<ParsedLine> = std::collections::VecDeque::new();
    assert_eq!(block_glyph(&buffer, 0), "▎", "an index that is not there");
}

// The colouring itself is `log_tail`'s, ported verbatim; this is that
// it survives the trip through the viewer's spans.
#[test]
fn a_duration_reaches_the_viewer_still_coloured_by_speed() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["GET /api/a 200 in 85ms"]);
    app.open_log_viewer();
    let buffer = draw(&mut app, 60, 10);
    let painted = text_of(&buffer);
    assert!(painted.contains("85ms"), "{painted}");
    let colours: Vec<_> = (0..60)
        .map(|x| buffer.cell((x, 1)).unwrap().style().fg)
        .collect();
    assert!(
        colours.contains(&Some(green())),
        "a fast duration is green: {colours:?}"
    );
}

#[test]
fn a_url_wraps_before_itself_rather_than_through_its_host() {
    let line = Line::raw("see https://example.com/a/b for more".to_string());
    // A width that would otherwise split "https://example.com" in two.
    let rows = wrap_line_to_rows(line, 20);
    let texts: Vec<String> = rows
        .iter()
        .map(|r| r.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(
        texts.iter().any(|t| t.starts_with("https://example.com")),
        "the scheme and the host stay together: {texts:?}"
    );
    assert_eq!(
        texts.concat(),
        "see https://example.com/a/b for more",
        "and nothing is lost: {texts:?}"
    );
}

#[test]
fn a_host_wider_than_the_viewport_is_cut_like_anything_else() {
    let host = "a".repeat(40);
    let line = Line::raw(format!("x https://{host}/p"));
    let rows = wrap_line_to_rows(line, 10);
    let texts: Vec<String> = rows
        .iter()
        .map(|r| r.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
    assert!(
        rows.iter().all(|r| r
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            <= 10),
        "no row may exceed the width: {texts:?}"
    );
    assert_eq!(texts.concat(), format!("x https://{host}/p"));
}

#[test]
fn protected_ranges_cover_the_scheme_and_host_only() {
    let text = "get https://example.com/a?b=1 done";
    let ranges = protected_ranges(text);
    assert_eq!(ranges.len(), 1);
    let (from, to) = ranges[0];
    let chars: Vec<char> = text.chars().collect();
    assert_eq!(
        chars[from..to].iter().collect::<String>(),
        "https://example.com"
    );
    assert!(protected_ranges("no urls here").is_empty());
}

// ---- the inspect overlay ---------------------------------------------

#[test]
fn the_overlay_paints_the_pretty_printed_json_over_the_viewer() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(
        &app,
        "feat+one",
        "dev",
        &["request {\"path\":\"/checkout\",\"ms\":30}"],
    );
    app.open_log_viewer();
    draw(&mut app, 80, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 20));
    assert!(painted.contains("inspect"), "{painted}");
    assert!(painted.contains("\"path\": \"/checkout\""), "{painted}");
    assert!(painted.contains("y copy"), "{painted}");
}

#[test]
fn the_overlay_colours_the_json_it_shows() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["{\"path\":\"/x\"}"]);
    app.open_log_viewer();
    draw(&mut app, 80, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    let colours: Vec<_> = app
        .inspect
        .as_ref()
        .unwrap()
        .lines
        .iter()
        .flat_map(|line| line.spans.iter().filter_map(|s| s.style.fg))
        .collect();
    assert!(
        colours.len() > 1,
        "the overlay is syntax-coloured, not one flat colour: {colours:?}"
    );
}

#[test]
fn capital_g_in_the_overlay_lands_on_the_real_last_page() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    let long: String = (0..60)
        .map(|i| format!("\"k{i}\":{i}"))
        .collect::<Vec<_>>()
        .join(",");
    write_log(&app, "feat+one", "dev", &[format!("{{{long}}}")]);
    app.open_log_viewer();
    draw(&mut app, 80, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    assert_eq!(app.inspect.as_ref().unwrap().scroll, usize::MAX);
    draw(&mut app, 80, 20);
    let scroll = app.inspect.as_ref().unwrap().scroll;
    assert!(
        scroll > 0 && scroll < 1000,
        "the paint clamps it to a real page: {scroll}"
    );
}

#[test]
fn the_overlay_survives_a_terminal_too_small_for_it() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["{\"a\":1}"]);
    app.open_log_viewer();
    draw(&mut app, 80, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::NONE));
    for (width, height) in [(1u16, 1u16), (2, 2), (4, 3), (20, 4), (48, 6)] {
        draw(&mut app, width, height);
    }
}

// ---- search ----------------------------------------------------------

fn content(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.to_string()).collect()
}

/// Only the parts of the line that got a background.
fn highlighted(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .filter(|s| s.style.bg.is_some())
        .map(|s| s.content.to_string())
        .collect()
}

fn type_into(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
}

#[test]
fn the_search_bar_shows_the_query_while_it_is_typed() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["compiled in 30ms"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    type_into(&mut app, "comp");
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(painted.contains("/comp"), "{painted}");
}

#[test]
fn an_active_search_shows_which_match_the_cursor_is_on() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["hit a", "plain", "hit b"]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    type_into(&mut app, "hit");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(painted.contains("[1/2]"), "{painted}");
    app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    let painted = text_of(&draw(&mut app, 80, 12));
    assert!(painted.contains("[2/2]"), "{painted}");
}

#[test]
fn a_match_is_painted_and_the_one_under_the_search_cursor_differently() {
    let line = Line::raw("compiled in 30ms");
    let plain = highlight_search_in_line(line.clone(), "compiled in 30ms", "compiled", false);
    assert_eq!(highlighted(&plain), "compiled");
    let backgrounds: Vec<_> = plain.spans.iter().filter_map(|s| s.style.bg).collect();
    assert_eq!(backgrounds, vec![search_match_bg()]);

    let under_cursor = highlight_search_in_line(line, "compiled in 30ms", "compiled", true);
    let backgrounds: Vec<_> = under_cursor
        .spans
        .iter()
        .filter_map(|s| s.style.bg)
        .collect();
    assert_eq!(backgrounds, vec![search_cursor_bg()]);
}

// `to_lowercase` is not length-preserving: `İ` is two bytes and
// lowercases to three. Slicing the original at lowercased offsets cuts
// a character in half and panics.
#[test]
fn highlighting_does_not_panic_when_lowercasing_grows_a_character() {
    let result = highlight_search_in_line(Line::raw("İx"), "İx", "i", false);
    assert_eq!(content(&result), "İx");
    assert_eq!(highlighted(&result), "");
}

#[test]
fn highlighting_lands_on_the_original_range_after_a_growing_character() {
    let result = highlight_search_in_line(Line::raw("İé"), "İé", "é", false);
    assert_eq!(content(&result), "İé");
    assert_eq!(highlighted(&result), "é");
}

// The real crash surface: a coloured line arrives already split into
// spans, so the offset has to be tracked across them.
#[test]
fn highlighting_crosses_spans_after_a_growing_character() {
    let line = Line::from(vec![
        Span::styled("İ ".to_string(), Style::new().fg(text_dim())),
        Span::styled("GET".to_string(), Style::new().fg(cyan())),
    ]);
    let result = highlight_search_in_line(line, "İ GET", "get", false);
    assert_eq!(content(&result), "İ GET");
    assert_eq!(highlighted(&result), "GET");
}

#[test]
fn wrap_line_to_rows_never_returns_nothing() {
    assert_eq!(wrap_line_to_rows(Line::raw(""), 10).len(), 1);
    assert_eq!(wrap_line_to_rows(Line::raw("abcdef"), 2).len(), 3);
    // A zero width would divide by nothing; it is clamped to one.
    assert_eq!(wrap_line_to_rows(Line::raw("ab"), 0).len(), 2);
}

#[test]
fn wrap_keeps_every_span_style_across_the_split() {
    let line = Line::from(vec![
        Span::styled("aaaa", Style::new().fg(red())),
        Span::styled("bbbb", Style::new().fg(green())),
    ]);
    let rows = wrap_line_to_rows(line, 3);
    let flat: Vec<(String, Style)> = rows
        .iter()
        .flat_map(|r| r.spans.iter().map(|s| (s.content.to_string(), s.style)))
        .collect();
    assert_eq!(
        flat.iter().map(|(c, _)| c.as_str()).collect::<String>(),
        "aaaabbbb"
    );
    for (content, style) in &flat {
        let want = if content.starts_with('a') {
            red()
        } else {
            green()
        };
        assert_eq!(style.fg, Some(want), "{content}");
    }
}

// ---- sizes, words and marks -------------------------------------------

/// The rectangle a popup's border draws, found by its top-left corner.
fn popup_size(buf: &Buffer, title: &str) -> (usize, usize) {
    let text = text_of(buf);
    let lines: Vec<&str> = text.lines().collect();
    let (top, line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| line.contains(&format!("╭ {title} ")))
        .expect("the popup is drawn");
    let chars: Vec<char> = line.chars().collect();
    let start = line[..line.find(&format!("╭ {title} ")).unwrap()]
        .chars()
        .count();
    let end = (start + 1..chars.len())
        .find(|&i| chars[i] == '╮')
        .expect("the popup has a right edge");
    let bottom = (top + 1..lines.len())
        .find(|&y| lines[y].chars().nth(start) == Some('╰'))
        .expect("the popup has a bottom edge");
    (end - start + 1, bottom - top + 1)
}

// A two-line confirmation is a small box on a big screen, not half of it.
#[test]
fn a_confirmation_is_sized_to_what_it_says() {
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Remove {
        name: "feat+one".into(),
        created_by_pando: true,
    });
    let (width, height) = popup_size(&draw(&mut app, 200, 50), "remove worktree");
    assert!(width < 70, "{width} columns for two short lines");
    assert!(height <= 9, "{height} rows for four lines and a margin");
}

// The key column is as wide as its widest key: `PgUp PgDn` used to run
// straight into what it does.
#[test]
fn the_help_key_column_never_runs_into_its_description() {
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Help);
    let rendered = text_of(&draw(&mut app, 120, 60));
    let row = rendered
        .lines()
        .find(|line| line.contains("PgUp PgDn"))
        .expect("help lists the page keys");
    assert!(row.contains("PgUp PgDn  "), "{row}");
    assert!(
        rendered.contains("in the list"),
        "and the legend: {rendered}"
    );
}

// The option is the command being chosen: it is never cut to make room
// for the reason it was offered.
#[test]
fn a_question_shows_each_option_whole() {
    let (reply, _rx) = std::sync::mpsc::channel();
    let command =
        "export NVM_DIR=\"$HOME/.nvm\" && . \"$NVM_DIR/nvm.sh\" --no-use && nvm use >/dev/null";
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Question {
        question: crate::actions::Question {
            slot: crate::detect::Slot::Prelude,
            prompt: "Which line should pando run first, so this shell resolves the runtime \
                     the project asks for?"
                .into(),
            options: vec![(
                command.to_string(),
                "nvm is installed here, and a login shell has to source it".to_string(),
            )],
            preselect: Some(0),
            allow_custom: true,
            allow_none: false,
            multi: false,
            checked: Vec::new(),
            details: vec!["this project asks for node 22 (.nvmrc)".into()],
            answer_file: None,
            snippet: String::new(),
        },
        selected: 0,
        custom: None,
        reply,
    });
    for width in [80u16, 140] {
        let rendered = text_of(&draw(&mut app, width, 30));
        let joined: String = rendered
            .lines()
            .map(|line| line.trim_matches(|c| c == '│' || c == ' '))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("resolves the runtime"),
            "the prompt wraps rather than being cut at {width}:\n{rendered}"
        );
        for piece in [
            "export NVM_DIR",
            "nvm use >/dev/null",
            "a login shell has to source it",
        ] {
            assert!(
                joined.contains(piece),
                "{piece:?} is shown at {width}:\n{rendered}"
            );
        }
    }
}

// The end of an error is usually the part that says what to do, so an
// error that does not fit on the header's row takes a second one.
#[test]
fn a_long_error_wraps_onto_more_header_rows() {
    let mut app = test_app(&["feat+one"]);
    app.set_error(
        "could not start feat/one: isolated mode needs Docker, and the Docker daemon \
         is not running — start Docker Desktop and press i again",
    );
    let rendered = text_of(&draw(&mut app, 80, 24));
    let header: Vec<&str> = rendered.lines().take(3).collect();
    assert!(
        header[0].contains('✗'),
        "an error is marked as one: {rendered}"
    );
    assert!(
        header.join(" ").contains("press i again"),
        "and read to its end: {rendered}"
    );
}

#[test]
fn a_success_is_marked_differently_from_an_error() {
    let mut app = test_app(&["feat+one"]);
    app.set_success("started feat/one");
    let first = text_of(&draw(&mut app, 80, 24))
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(first.contains("✓ started feat/one"), "{first}");
}

#[test]
fn a_path_under_home_is_written_with_a_tilde() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    assert_eq!(home_relative(&home.join("code/acme")), "~/code/acme");
    assert_eq!(
        home_relative(std::path::Path::new("/elsewhere/acme")),
        "/elsewhere/acme"
    );
}

#[test]
fn the_tail_header_counts_one_line_as_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("dev.log");
    std::fs::write(&log, "ready\n").unwrap();
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let (key, _, _) = app.tail_target().unwrap();
    app.log_tails.touch(&key, log).poll().unwrap();
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("1 line ·"), "{rendered}");
}

// The public URL is the thing people copy: a narrow pane wraps it, and
// never cuts it.
#[test]
fn a_public_url_is_never_truncated() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    crate::tui::app::tests::with_share(&mut app, "feat+one", None);
    let url = "https://fake-host.trycloudflare.com";
    for width in [60u16, 90, 140] {
        let rendered = text_of(&draw(&mut app, width, 30));
        let detail: String = rendered
            .lines()
            .filter_map(|line| line.rsplit("││").next())
            .map(|part| part.trim_matches(|c| c == '│' || c == ' ').to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            detail.replace(' ', "").contains(url),
            "the whole URL at {width}:\n{rendered}"
        );
    }
}

#[test]
fn the_detail_pane_says_a_start_is_waiting_on_a_question() {
    let (reply, _rx) = std::sync::mpsc::channel();
    let mut app = test_app(&["feat+one"]);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    app.modal = Some(Modal::Question {
        question: crate::actions::Question {
            slot: crate::detect::Slot::DevCmd,
            prompt: "Which command?".into(),
            options: vec![("pnpm dev".into(), String::new())],
            preselect: Some(0),
            allow_custom: true,
            allow_none: false,
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
            answer_file: None,
            snippet: String::new(),
        },
        selected: 0,
        custom: None,
        reply,
    });
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(rendered.contains("waiting for your answer"), "{rendered}");
    assert!(!rendered.contains("○ stopped"), "{rendered}");
}

// First run: what pando is, what it knows, and the key to get going.
#[test]
fn the_first_run_welcomes_rather_than_showing_an_empty_box() {
    let mut app = test_app(&[]);
    app.config.processes.insert(
        "dev".into(),
        crate::config::ProcessConfig {
            cmd: "pnpm dev".into(),
            ..Default::default()
        },
    );
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(
        rendered.contains("one dev environment per branch"),
        "{rendered}"
    );
    assert!(rendered.contains("dev: pnpm dev"), "{rendered}");
    assert!(rendered.contains("create a worktree"), "{rendered}");
    assert!(!rendered.contains("worktrees (0)"), "{rendered}");
}

// A source that has written only blank lines is a source with no output,
// not a gutter mark on an empty row.
#[test]
fn a_source_with_only_blank_lines_says_it_has_no_output() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    write_log(&app, "feat+one", "dev", &["listening"]);
    write_log(&app, "feat+one", "install", &[""]);
    app.open_log_viewer();
    draw(&mut app, 80, 12);
    app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
    app.handle_event(crate::tui::app::AppEvent::Tick);
    let rendered = text_of(&draw(&mut app, 80, 12));
    assert!(rendered.contains("(no output yet)"), "{rendered}");
    assert!(!rendered.contains('▎'), "{rendered}");
}

/// The keys a hint names: `j/k` is two, `/` is itself.
fn hint_keys(hint: &str) -> Vec<&str> {
    if hint == "/" {
        vec![hint]
    } else {
        hint.split('/').collect()
    }
}

// The footer's hints are keys help documents; a hint for a key nothing
// answers would be worse than none.
#[test]
fn every_footer_hint_is_a_key_help_documents() {
    use crate::tui::app::{LIST_KEYS, LOG_KEYS};
    let tokens = |keys: &[crate::tui::app::KeyHelp]| -> Vec<String> {
        keys.iter()
            .flat_map(|k| k.keys.split(' ').map(str::to_string))
            .collect()
    };
    let list = tokens(LIST_KEYS);
    let shared = [("O", "public", false), ("Y", "copy URL", false)];
    for (key, _, _) in RUNNING_HINTS
        .iter()
        .chain(&STOPPED_HINTS)
        .chain(&NOTHING_TO_RUN_HINTS)
        .chain(&EMPTY_HINTS)
        .chain(&shared)
    {
        for part in hint_keys(key) {
            assert!(list.contains(&part.to_string()), "{part} is not in help");
        }
    }
    let log = tokens(LOG_KEYS);
    for (key, _, _) in LOG_HINTS {
        for part in hint_keys(key) {
            assert!(log.contains(&part.to_string()), "{part} is not in help");
        }
    }
}

/// The style of the first cell of `needle` on the first row that shows
/// `row` as well.
fn style_at(buf: &Buffer, row: &str, needle: &str) -> ratatui::style::Style {
    let text = text_of(buf);
    let (y, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(row) && line.contains(needle))
        .unwrap_or_else(|| panic!("{needle} is not on screen:\n{text}"));
    let x = line[..line.find(needle).unwrap()].chars().count();
    buf.cell((x as u16, y as u16)).unwrap().style()
}

// Thirty `stopped` down a column drown out the two rows that matter: the
// word is faint, and what runs or failed is not.
#[test]
fn stopped_is_faint_and_running_is_not() {
    let mut app = test_app(&["feat+up", "feat+down"]);
    with_process(&mut app, "feat+up", running_phase());
    let buf = draw(&mut app, 120, 12);
    let stopped = style_at(&buf, "feat/down", "stopped");
    let running = style_at(&buf, "feat/up", "running");
    use ratatui::style::Modifier;
    assert!(stopped.add_modifier.contains(Modifier::DIM), "{stopped:?}");
    assert!(!running.add_modifier.contains(Modifier::DIM), "{running:?}");
    assert!(running.add_modifier.contains(Modifier::BOLD), "{running:?}");
}

// PANDO_HOME moves the home; the welcome names the one in use.
#[test]
fn the_welcome_names_the_real_pando_home() {
    let mut app = test_app(&[]);
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(
        rendered.contains("/pando-test-does-not-exist/home"),
        "{rendered}"
    );
    assert!(!rendered.contains("~/.pando"), "{rendered}");
}

#[test]
fn the_stop_all_confirmation_lists_what_goes_down() {
    let mut app = test_app(&["feat+one", "feat+two", "feat+three"]);
    with_process(&mut app, "feat+one", running_phase());
    with_process(&mut app, "feat+three", running_phase());
    app.modal = Some(Modal::StopAll {
        names: vec!["feat+one".into(), "feat+three".into()],
    });
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(rendered.contains("stop all 2 worktrees"), "{rendered}");
    assert!(rendered.contains("feat/one"), "{rendered}");
    assert!(rendered.contains("feat/three"), "{rendered}");
}

// Removing a running worktree stops it first; the confirmation says so.
#[test]
fn removing_a_running_worktree_warns_that_it_stops_it() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    app.modal = Some(Modal::Remove {
        name: "feat+one".into(),
        created_by_pando: true,
    });
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(rendered.contains("removing stops it first"), "{rendered}");

    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Remove {
        name: "feat+one".into(),
        created_by_pando: true,
    });
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(!rendered.contains("removing stops it first"), "{rendered}");
}

// ---- git state on screen ---------------------------------------------

// A worktree with uncommitted changes carries a `*` against its label,
// even on a pane too narrow for the drift column.
#[test]
fn a_dirty_worktree_is_marked_in_the_list_at_any_width() {
    let mut app = test_app(&["feat+tui", "feat+clean"]);
    app.worktrees[0].dirty = Some(true);
    for width in [60, 100, 200] {
        let rendered = text_of(&draw(&mut app, width, 12));
        let row = list_row(&rendered, "feat/tui");
        assert!(row.contains('*'), "at {width}:\n{rendered}");
        assert!(
            !list_row(&rendered, "feat/clean").contains('*'),
            "at {width}:\n{rendered}"
        );
    }
}

#[test]
fn the_detail_pane_has_a_git_row() {
    let mut app = test_app(&["feat+tui"]);
    app.worktrees[0].dirty = Some(true);
    app.worktrees[0].ahead_behind = Some((2, 1));
    let rendered = text_of(&draw(&mut app, 140, 30));
    let row = rendered
        .lines()
        .find(|line| line.contains(" git "))
        .unwrap_or_default();
    assert!(row.contains("* uncommitted changes"), "{rendered}");
    assert!(row.contains("↑2 ahead, ↓1 behind of main"), "{rendered}");

    app.worktrees[0].dirty = Some(false);
    app.worktrees[0].ahead_behind = Some((0, 0));
    let rendered = text_of(&draw(&mut app, 140, 30));
    let row = rendered
        .lines()
        .find(|line| line.contains(" git "))
        .unwrap_or_default();
    assert!(row.contains("clean · even with main"), "{rendered}");
}

// One duration style in the pane: the commit's age reads like the uptime.
#[test]
fn the_commit_age_is_compact() {
    let mut app = test_app(&["feat+one"]);
    app.worktrees[0].head_age = Some("82 seconds ago".into());
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(rendered.contains("· 1m ago"), "{rendered}");
    assert!(!rendered.contains("seconds ago"), "{rendered}");
}

// ---- the remove dialog -----------------------------------------------

#[test]
fn the_remove_dialog_states_dirty_and_running_and_offers_f() {
    let mut app = test_app(&["feat+tui"]);
    app.worktrees[0].dirty = Some(true);
    with_process(&mut app, "feat+tui", running_phase());
    app.modal = Some(Modal::Remove {
        name: "feat+tui".into(),
        created_by_pando: true,
    });
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(rendered.contains("uncommitted changes"), "{rendered}");
    assert!(rendered.contains("removing stops it first"), "{rendered}");
    assert!(
        rendered.contains("F remove, discarding changes"),
        "{rendered}"
    );
    assert!(!rendered.contains("--force"), "{rendered}");
}

#[test]
fn a_clean_removal_offers_y_and_f() {
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Remove {
        name: "feat+one".into(),
        created_by_pando: true,
    });
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(rendered.contains("y remove"), "{rendered}");
    assert!(rendered.contains("F force"), "{rendered}");
    assert!(!rendered.contains("uncommitted"), "{rendered}");
}

// ---- nothing to run --------------------------------------------------

#[test]
fn a_project_with_nothing_to_run_says_so_in_the_pane_and_the_footer() {
    let mut app = test_app(&["main-lib"]);
    app.nothing_to_run = true;
    let rendered = text_of(&draw(&mut app, 200, 30));
    assert!(!rendered.contains("⏎ starts it"), "{rendered}");
    assert!(
        rendered.contains("nothing to run: add a [dev] command in"),
        "{rendered}"
    );
    let footer = rendered.lines().last().unwrap_or_default();
    assert!(footer.contains("nothing to run"), "{footer}");
    assert!(!footer.contains("start"), "{footer}");
}

// ---- the header ------------------------------------------------------

#[test]
fn the_shared_service_dots_are_labelled() {
    let mut app = test_app(&["feat+one"]);
    app.service_health = crate::tui::app::ServiceHealth {
        shared: vec![crate::actions::ServiceStatus {
            name: "postgres".into(),
            port: Some(5432),
            up: true,
            logging: false,
        }],
        worktrees: std::collections::BTreeMap::new(),
    };
    let header = text_of(&draw(&mut app, 120, 12))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(header.contains("shared: postgres ● up"), "{header}");
}

// At 80×24 an error takes at most two header rows; the rest is in `m`.
#[test]
fn a_long_error_on_a_short_screen_takes_two_rows_at_most() {
    let mut app = test_app(&["feat+one"]);
    app.set_error("word ".repeat(80));
    assert_eq!(header_height(&app, 80, 24), 2);
    let rendered = text_of(&draw(&mut app, 80, 24));
    assert!(
        rendered.lines().nth(1).unwrap().contains("m shows it all"),
        "{rendered}"
    );
    assert_eq!(header_height(&app, 80, 40), 3, "a tall screen keeps three");
}

// ---- the list's columns ----------------------------------------------

// The status column does not change width when a row goes from starting
// to running, so nothing after it moves.
#[test]
fn the_status_column_keeps_its_width_between_starting_and_running() {
    let url_column = |phase: crate::state::Phase| {
        let mut app = test_app(&["feat+one"]);
        with_process(&mut app, "feat+one", phase);
        let rendered = text_of(&draw(&mut app, 140, 10));
        list_row(&rendered, "feat/one").find("http").unwrap()
    };
    assert_eq!(
        url_column(crate::state::Phase::Starting {
            since: chrono::Utc::now()
        }),
        url_column(running_phase())
    );
}

// On a wide screen the room goes after the columns, not into a gap
// between the branch and whether it runs.
#[test]
fn a_wide_list_puts_the_status_right_after_the_branch() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 200, 10));
    let row = list_row(&rendered, "feat/one");
    let branch_end = row.find("feat/one").unwrap() + "feat/one".len();
    let status = row.find("stopped").unwrap();
    assert!(
        status - branch_end <= 12,
        "{} columns between them:\n{row}",
        status - branch_end
    );
}

// With room to spare, every port of a multi-process worktree shows.
#[test]
fn a_wide_list_shows_every_port_and_a_narrow_one_sheds_them_first() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());
    let wide = text_of(&draw(&mut app, 220, 10));
    assert!(list_row(&wide, "feat/one").contains("api 17344"), "{wide}");
    let narrow = text_of(&draw(&mut app, 90, 10));
    let row = list_row(&narrow, "feat/one");
    assert!(!row.contains("api 17344"), "{narrow}");
    assert!(row.contains("running"), "{narrow}");
}

// Every port is a nicety; a branch cut short is what the list exists to
// show. With a long branch in the list the ports go before it is cut.
#[test]
fn the_ports_give_way_before_a_long_branch_is_cut() {
    let long = "feature+checkout-flow-for-guests";
    let mut app = test_app(&["feat+one", long]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());
    let rendered = text_of(&draw(&mut app, 170, 10));
    assert!(
        rendered.contains("feature/checkout-flow-for-guests"),
        "{rendered}"
    );
    assert!(
        !list_row(&rendered, "feat/one").contains("api 17344"),
        "{rendered}"
    );
}

// ---- help ------------------------------------------------------------

#[test]
fn help_says_which_keys_close_it() {
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Help);
    let tall = text_of(&draw(&mut app, 140, 80));
    assert!(tall.contains("esc/q/? close"), "{tall}");
    assert!(!tall.contains("any key"), "{tall}");
    let short = text_of(&draw(&mut app, 140, 20));
    assert!(short.contains("j/k g/G scroll · esc/q/? close"), "{short}");
}

#[test]
fn help_explains_the_dirty_mark_and_the_shared_dots() {
    let mut app = test_app(&["feat+one"]);
    app.modal = Some(Modal::Help);
    let rendered = text_of(&draw(&mut app, 160, 90));
    assert!(rendered.contains("uncommitted changes"), "{rendered}");
    assert!(rendered.contains("default ports"), "{rendered}");
}

// ---- the create modal ------------------------------------------------

#[test]
fn the_picker_marks_the_main_checkouts_branch_and_shows_the_base() {
    let mut app = test_app(&["feat+one"]);
    let mut main = wt("acme-shop");
    main.branch = Some("main".into());
    app.main = Some(main);
    app.modal = Some(Modal::Create {
        input: String::new(),
        branches: BranchLoadState::Ready(vec![crate::worktree::BranchEntry {
            name: "main".into(),
            source: crate::worktree::BranchSource::Local,
        }]),
        selected: 0,
        base: Some("develop".into()),
    });
    let rendered = text_of(&draw(&mut app, 140, 30));
    assert!(
        rendered.contains("checked out in the main checkout"),
        "{rendered}"
    );
    assert!(rendered.contains("fork from develop"), "{rendered}");
    assert!(rendered.contains("tab"), "{rendered}");
}

// ---- the all tab -----------------------------------------------------

#[test]
fn the_all_tab_paints_each_line_with_its_source() {
    let (_dir, mut app) = app_with_logs(&["feat+one"]);
    app.config.processes.clear();
    for process in ["api", "web"] {
        app.config
            .processes
            .insert(process.to_string(), crate::config::ProcessConfig::default());
    }
    write_log(&app, "feat+one", "api", &["api up"]);
    write_log(&app, "feat+one", "web", &["web up"]);
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    let _ = draw(&mut app, 100, 20);
    app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    let rendered = text_of(&draw(&mut app, 100, 20));
    assert!(rendered.contains("1:all"), "{rendered}");
    assert!(rendered.contains("api │ api up"), "{rendered}");
    assert!(rendered.contains("web │ web up"), "{rendered}");
}

// ---- dogfood ---------------------------------------------------------

// A question that may be answered "none" offers it, and the footer says
// how.
#[test]
fn a_question_that_allows_none_offers_n_in_its_keys() {
    let mut app = test_app(&["feat+one"]);
    let (tx, _rx) = std::sync::mpsc::channel();
    app.handle_event(crate::tui::app::AppEvent::AskQuestion(Box::new((
        crate::actions::Question {
            slot: crate::detect::Slot::SchemaHook,
            prompt: "Which command sets up the database schema?".to_string(),
            options: vec![("pnpm db:push".to_string(), "package.json".to_string())],
            preselect: Some(0),
            allow_custom: true,
            allow_none: true,
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
            answer_file: None,
            snippet: String::new(),
        },
        tx,
    ))));
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(rendered.contains("n none"), "{rendered}");
}

// A port answer is variable names, not a command: the key says which.
#[test]
fn the_port_question_says_to_type_variable_names() {
    let mut app = test_app(&["feat+one"]);
    let (tx, _rx) = std::sync::mpsc::channel();
    app.handle_event(crate::tui::app::AppEvent::AskQuestion(Box::new((
        crate::actions::Question {
            slot: crate::detect::Slot::PortEnv,
            prompt: "Which environment variables carry this project's ports?".to_string(),
            options: vec![("PORT".to_string(), "the Node convention".to_string())],
            preselect: Some(0),
            allow_custom: true,
            allow_none: true,
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
            answer_file: None,
            snippet: String::new(),
        },
        tx,
    ))));
    let rendered = text_of(&draw(&mut app, 120, 30));
    assert!(rendered.contains("c type the variable names"), "{rendered}");
    assert!(!rendered.contains("type a command"), "{rendered}");
}

// While a worker waits on the dialog, the footer behind it says so rather
// than offering keys the dialog has taken.
#[test]
fn the_footer_says_a_question_is_waiting() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    let (_done, rx) = std::sync::mpsc::channel();
    app.pending = Some(crate::tui::app::PendingAction {
        name: "feat+one".into(),
        kind: crate::tui::app::PendingKind::Start,
        rx,
        started_at: std::time::Instant::now(),
        spinner_frame: 0,
        progress_rx: None,
        stage: None,
        label: "feat/one".into(),
    });
    let (tx, _rx) = std::sync::mpsc::channel();
    app.handle_event(crate::tui::app::AppEvent::AskQuestion(Box::new((
        crate::actions::Question {
            slot: crate::detect::Slot::DevCmd,
            prompt: "Which command starts it?".to_string(),
            options: vec![("pnpm dev".to_string(), "package.json".to_string())],
            preselect: Some(0),
            allow_custom: true,
            allow_none: false,
            multi: false,
            checked: Vec::new(),
            details: Vec::new(),
            answer_file: None,
            snippet: String::new(),
        },
        tx,
    ))));
    let rendered = text_of(&draw(&mut app, 120, 30));
    let footer = rendered.lines().last().unwrap_or_default();
    assert!(footer.contains("waiting for your answer"), "{footer}");
    assert!(!footer.contains("logs"), "{footer}");
    assert!(!footer.contains("stop"), "{footer}");
}

// The title describes the source on screen; the worktree's failure, when
// it is another process's, is named apart from it.
#[test]
fn the_viewer_title_describes_the_source_not_the_worktree() {
    let mut app = test_app(&["feat+m"]);
    with_process(&mut app, "feat+m", running_phase());
    with_second_process(
        &mut app,
        "feat+m",
        "worker",
        crate::state::Phase::Failed {
            at: chrono::Utc::now(),
            reason: "exit 1".into(),
        },
    );
    let suffix = |source: &str| viewer_title_suffix(&app, "feat+m", source, false, false);
    assert_eq!(suffix("dev"), " (running) · worker failed");
    assert_eq!(suffix("worker"), " (failed)");
    assert_eq!(suffix("all"), " · worker failed");
    assert_eq!(
        viewer_title_suffix(&app, "feat+m", "dev", true, false),
        " (no log yet)"
    );
}

// Not colour alone: the word says it too.
#[test]
fn the_shared_service_chips_say_up_or_down_in_words() {
    let mut app = test_app(&["feat+one"]);
    app.service_health = crate::tui::app::ServiceHealth {
        shared: vec![
            crate::actions::ServiceStatus {
                name: "postgres".into(),
                port: Some(5432),
                up: true,
                logging: false,
            },
            crate::actions::ServiceStatus {
                name: "redis".into(),
                port: Some(6379),
                up: false,
                logging: false,
            },
        ],
        worktrees: std::collections::BTreeMap::new(),
    };
    let header = text_of(&draw(&mut app, 140, 12))
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(header.contains("postgres ● up"), "{header}");
    assert!(header.contains("redis ● down"), "{header}");
}

// A path longer than the row breaks after a `/`, not mid-name.
#[test]
fn wrap_text_breaks_a_long_path_after_a_slash() {
    let rows = wrap_text(
        "nothing to run: add a [dev] command in /Users/dev/.pando/projects/acme-shop-3f9a2c1d/pando.toml",
        40,
    );
    assert!(rows.iter().all(|r| r.chars().count() <= 40), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.ends_with("pando.toml")),
        "the file name is whole: {rows:?}"
    );
    assert!(
        rows.iter().filter(|r| r.contains('/')).all(|r| {
            let last = r.split(' ').next_back().unwrap_or_default();
            !last.contains('/') || last.ends_with('/') || r.ends_with("pando.toml")
        }),
        "every break in the path is after a slash: {rows:?}"
    );
    // A token with no slash wider than the row is still cut.
    assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
}

// `p` restarting one process does not reset the worktree's uptime.
#[test]
fn the_detail_uptime_counts_from_the_oldest_running_process() {
    let mut app = test_app(&["feat+m"]);
    with_process(
        &mut app,
        "feat+m",
        crate::state::Phase::Running {
            since: chrono::Utc::now() - chrono::Duration::minutes(2),
        },
    );
    with_second_process(&mut app, "feat+m", "worker", running_phase());
    let rendered = text_of(&draw(&mut app, 140, 24));
    let status = rendered
        .lines()
        .find(|l| l.contains("status"))
        .unwrap_or_default();
    // The worker's own row may say it is new; the worktree is not.
    assert!(status.contains("up 2m"), "{rendered}");
}

// A failed worktree keeps its URL (another process may still hold it) but
// does not draw it as a live link.
#[test]
fn a_failed_worktrees_url_is_not_drawn_as_a_live_link() {
    let style_of_url = |phase: crate::state::Phase| {
        let mut app = test_app(&["feat+one"]);
        with_process(&mut app, "feat+one", phase);
        let buffer = draw(&mut app, 140, 20);
        let text = text_of(&buffer);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(" url "))
            .expect("a url row");
        let at = line.find("http://").expect("the url itself");
        let x = line[..at].chars().count() as u16;
        buffer.cell((x, row as u16)).unwrap().style()
    };
    let live = style_of_url(running_phase());
    assert!(
        live.add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
    let failed = style_of_url(crate::state::Phase::Failed {
        at: chrono::Utc::now(),
        reason: "process exited".into(),
    });
    assert!(
        !failed
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED)
    );
}
