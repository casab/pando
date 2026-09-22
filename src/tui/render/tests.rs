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
use crate::tui::app::{BranchLoadState, Modal, RemoveBlocker};
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
        Modal::Create {
            input: "feat/new".into(),
            branches: BranchLoadState::Loading,
            selected: 0,
        },
        Modal::Remove {
            name: "feat+one".into(),
            blocker: Some(RemoveBlocker::Locked(Some("benchmarking".into()))),
            created_by_pando: false,
        },
        Modal::Unshare {
            name: "feat+one".into(),
            url: "https://a-rather-long-quick-tunnel-hostname.trycloudflare.com".into(),
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
    assert!(text_of(&draw(&mut app, 120, 10)).contains("press n to create one"));

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

#[test]
fn list_columns_shed_branch_then_signals_then_pr() {
    // Wide enough for everything.
    assert_eq!(
        list_columns(80, 20, 6, 4),
        ListColumns {
            branch: true,
            signals: true,
            pr: true
        }
    );
    // Branch goes first.
    assert_eq!(
        list_columns(44, 20, 6, 4),
        ListColumns {
            branch: false,
            signals: true,
            pr: true
        }
    );
    // Then the signal grid.
    assert_eq!(
        list_columns(24, 20, 6, 4),
        ListColumns {
            branch: false,
            signals: false,
            pr: true
        }
    );
    // The PR chip is the last thing standing.
    assert_eq!(
        list_columns(18, 20, 6, 4),
        ListColumns {
            branch: false,
            signals: false,
            pr: false
        }
    );
}

#[test]
fn list_columns_keep_everything_when_the_optional_columns_are_empty() {
    assert_eq!(
        list_columns(20, 0, 0, 0),
        ListColumns {
            branch: true,
            signals: true,
            pr: true
        },
        "columns with no content cost nothing"
    );
}

#[test]
fn a_narrow_list_still_shows_the_name() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 30, 8));
    assert!(rendered.contains("feat+one"), "{rendered}");
}

#[test]
fn the_row_marks_adopted_worktrees_differently() {
    let mut app = test_app(&["mine", "theirs"]);
    app.created_by_pando.insert("theirs".into(), false);
    let rendered = text_of(&draw(&mut app, 100, 10));
    // A run marker sits between the ownership dot and the name now.
    assert!(rendered.contains("●   mine"), "{rendered}");
    assert!(rendered.contains("○   theirs"), "{rendered}");
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
    let shared = text_of(&draw(&mut app, 100, 10));
    assert!(shared.contains("◈ feat+one"), "{shared}");
    assert!(
        shared.contains("  feat+two"),
        "the column is reserved list-wide, so unshared rows keep the edge:\n{shared}"
    );
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
    assert!(rendered.contains("press s to start"), "{rendered}");
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
    assert!(rendered.contains("● ● up"), "{rendered}");
    assert!(rendered.contains("● ✗ broken"), "{rendered}");
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
        rendered.matches("running").count(),
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
    assert!(
        rendered.contains("● ✗ feat+one"),
        "a worktree with a dead process is not running: {rendered}"
    );
}

#[test]
fn the_tail_header_names_the_process_it_is_showing() {
    let mut app = test_app(&["feat+one"]);
    with_process(&mut app, "feat+one", running_phase());
    with_second_process(&mut app, "feat+one", "api", running_phase());
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("log api"), "{rendered}");
    assert!(
        rendered.contains("tab to switch"),
        "and says how to see the other one: {rendered}"
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let rendered = text_of(&draw(&mut app, 120, 24));
    assert!(rendered.contains("log dev"), "{rendered}");
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
fn nothing_selected_leaves_the_detail_pane_saying_so() {
    let mut app = test_app(&[]);
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(rendered.contains("nothing selected"), "{rendered}");
}

#[test]
fn the_footer_offers_the_process_keys() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 120, 20));
    assert!(rendered.contains("s start"), "{rendered}");
    assert!(rendered.contains("x stop"), "{rendered}");
    assert!(rendered.contains("l logs"), "{rendered}");
}

// Finding 9. Sharing is this phase's whole feature and neither of its
// keys was in the footer at any width, while `o open` was.
#[test]
fn the_footer_offers_the_share_keys_on_a_wide_terminal() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 160, 20));
    assert!(rendered.contains("t share"), "{rendered}");
    assert!(rendered.contains("O public"), "{rendered}");
}

// …and they are optional, so a narrow terminal sheds them rather than
// anything essential, and nothing clips.
#[test]
fn the_share_keys_go_before_anything_essential_when_the_footer_will_not_fit() {
    let mut app = test_app(&["feat+one"]);
    let rendered = text_of(&draw(&mut app, 60, 20));
    for essential in ["j/k move", "s start", "x stop", "q quit"] {
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
    assert!(painted.contains("feat+one · dev"), "{painted}");
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
