//! The setup screen, and the ready view it turns into: the whole frame,
//! with a header and a footer of its own.
//!
//! What a short or narrow terminal cannot hold is dropped by priority,
//! blank rows first and the prompt and the live line last, so a tmux
//! split still shows what to copy and where the setup stands.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::setup::SETUP_PROMPT;
use crate::theme::{green, orange, red, surface, text, text_dim, text_muted, yellow};
use crate::tui::app::{App, SetupLine, SetupScreen, Trying};

use super::chrome::{hint_line, status_mark};
use super::welcome::{ago, ready_view};
use super::{home_relative, text_width, truncate, truncate_line, wrap_text};

/// A block of rows and how much it matters: the least is dropped first
/// when the screen is too short for all of them.
struct Block {
    priority: u8,
    lines: Vec<Line<'static>>,
}

const BLANK: u8 = 0;
const EXPLANATION: u8 = 1;
const STEPS: u8 = 2;
const HEADING: u8 = 3;
const STEP_ONE: u8 = 4;
const LIVE: u8 = 5;
const PROMPT: u8 = 6;

pub(super) fn render_setup(f: &mut Frame, area: Rect, app: &App) {
    let Some(screen) = app.setup_screen.as_ref() else {
        return;
    };
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    render_setup_header(f, header, app, screen);
    let width = body.width as usize;
    let blocks = if screen.is_ready() {
        ready_blocks(screen, app, width)
    } else {
        setup_blocks(app, screen, width)
    };
    let lines = fit(blocks, body.height as usize);
    f.render_widget(Paragraph::new(lines), body);
    render_setup_footer(f, footer, screen);
}

/// `pando — <project>`, and on the right what the screen is — or the
/// flash, which has nowhere else to go here.
fn render_setup_header(f: &mut Frame, area: Rect, app: &App, screen: &SetupScreen) {
    let width = area.width as usize;
    let title = format!(" pando — {}", app.paths.project.display_name);
    let title_width = text_width(&title);
    let room = width.saturating_sub(title_width + 3);
    let right: Vec<Span<'static>> = match app.flash() {
        Some(status) if room > 4 => {
            let (mark, color) = status_mark(status);
            let message = truncate(&status.message, room.saturating_sub(text_width(mark)));
            vec![
                Span::styled(mark, Style::new().fg(color)),
                Span::styled(message, Style::new().fg(text_dim())),
            ]
        }
        None if !screen.is_ready() && room >= "first time here".len() => {
            vec![Span::styled(
                "first time here",
                Style::new().fg(text_muted()),
            )]
        }
        _ => Vec::new(),
    };
    let right_width: usize = right.iter().map(|s| text_width(&s.content)).sum();
    let gap = width.saturating_sub(title_width + right_width + 1);
    let mut spans = vec![Span::styled(
        title,
        Style::new().fg(text()).add_modifier(Modifier::BOLD),
    )];
    if !right.is_empty() {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.extend(right);
    }
    f.render_widget(
        Paragraph::new(truncate_line(Line::from(spans), width)),
        area,
    );
}

fn render_setup_footer(f: &mut Frame, area: Rect, screen: &SetupScreen) {
    let hints = setup_hints(screen);
    f.render_widget(
        Paragraph::new(hint_line(&hints, area.width as usize)).style(Style::new().bg(surface())),
        area,
    );
}

/// The footer's hints, each a key of `SETUP_KEYS`.
pub(super) fn setup_hints(screen: &SetupScreen) -> Vec<(&'static str, &'static str, bool)> {
    let mut hints = Vec::new();
    if screen.is_ready() {
        hints.push(("⏎", "open pando", true));
        if screen.may_test() {
            hints.push(("v", "test again", false));
        }
    } else {
        hints.push(("a", "copy the prompt", true));
        if screen.may_test() {
            hints.push(("v", "test it", true));
        }
        if screen.may_try() {
            hints.push(("⏎", "let pando try on its own", true));
        }
        hints.push(("esc", "just manage worktrees", true));
    }
    hints.push(("?", "help", false));
    hints.push(("q", "quit", true));
    hints
}

/// The left margin: two cells where there is room for them.
fn indent(width: usize) -> usize {
    if width >= 30 { 2 } else { 0 }
}

fn setup_blocks(app: &App, screen: &SetupScreen, width: usize) -> Vec<Block> {
    let project = &app.paths.project.display_name;
    let pad = indent(width);
    let room = width.saturating_sub(pad * 2).max(1);
    let mut blocks = vec![blank()];

    blocks.push(Block {
        priority: HEADING,
        lines: wrapped(
            &format!("Let's set pando up for {project}."),
            pad,
            room,
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
    });
    blocks.push(blank());
    blocks.push(Block {
        priority: EXPLANATION,
        lines: wrapped(
            "Every project is a little different, so the surest start is to let your coding \
             agent look at it. It sets pando up, tests it, and tells you when you're ready. It \
             never changes a file in your project.",
            pad,
            room,
            Style::new().fg(text_dim()),
        ),
    });
    blocks.push(blank());
    blocks.push(Block {
        priority: STEP_ONE,
        lines: vec![step_one(pad, room)],
    });
    blocks.push(blank());
    // No border either side: selected over SSH, the text is all that
    // comes with it.
    // Set in from the steps where it still fits on one row that way;
    // a row of its own matters more than the indent.
    let prompt_pad = if width >= text_width(SETUP_PROMPT) + pad * 2 + 5 {
        pad + 5
    } else {
        pad
    };
    blocks.push(Block {
        priority: PROMPT,
        lines: wrapped(
            SETUP_PROMPT,
            prompt_pad,
            width.saturating_sub(prompt_pad + pad).max(1),
            Style::new().fg(text()).add_modifier(Modifier::BOLD),
        ),
    });
    blocks.push(blank());
    let root = home_relative(app.paths.root());
    blocks.push(Block {
        priority: STEPS,
        lines: step(
            "2",
            &format!("paste it into Claude Code or Codex, opened in {root}"),
            pad,
            room,
        ),
    });
    blocks.push(Block {
        priority: STEPS,
        lines: step(
            "3",
            "come back here: this screen turns green by itself when it's done",
            pad,
            room,
        ),
    });
    blocks.push(blank());
    blocks.push(Block {
        priority: LIVE,
        lines: live_lines(app, screen, pad, room),
    });
    blocks
}

/// The ready view: the welcome's renderer with the test row, fed from the
/// screen's own config and record, and the key onward.
fn ready_blocks(screen: &SetupScreen, app: &App, width: usize) -> Vec<Block> {
    let pad = indent(width);
    let room = width.saturating_sub(pad * 2).max(1);
    let view = ready_view(app, &screen.config, &screen.setup, room);
    let margin = |lines: Vec<Line<'static>>| -> Vec<Line<'static>> {
        lines
            .into_iter()
            .map(|line| {
                let mut spans = vec![Span::raw(" ".repeat(pad))];
                spans.extend(line.spans);
                truncate_line(Line::from(spans), width)
            })
            .collect()
    };
    let mut blocks = vec![
        blank(),
        Block {
            priority: PROMPT,
            lines: margin(view.headline),
        },
        Block {
            priority: LIVE,
            lines: margin(view.when),
        },
        blank(),
    ];
    // The test row matters more than the facts it tested.
    let test_at = view.facts.len().saturating_sub(1);
    for (i, fact) in view.facts.into_iter().enumerate() {
        blocks.push(Block {
            priority: if i == test_at { HEADING } else { STEPS },
            lines: margin(vec![fact]),
        });
    }
    blocks.push(blank());
    blocks.push(Block {
        priority: EXPLANATION,
        lines: margin(view.settings),
    });
    blocks.push(blank());
    blocks.push(Block {
        priority: STEP_ONE,
        lines: margin(vec![Line::from(vec![
            Span::styled("⏎", Style::new().fg(orange()).add_modifier(Modifier::BOLD)),
            Span::styled(" open pando", Style::new().fg(text())),
        ])]),
    });
    blocks
}

/// The live line, from what the files say.
fn live_lines(app: &App, screen: &SetupScreen, pad: usize, room: usize) -> Vec<Line<'static>> {
    let project = &app.paths.project.display_name;
    let spinner = app.spinner();
    let dim = Style::new().fg(text_dim());
    match screen.line() {
        SetupLine::Reading => marked(
            spinner,
            yellow(),
            &format!("reading {project}…"),
            dim,
            pad,
            room,
        ),
        SetupLine::Waiting => marked(spinner, yellow(), "waiting for the setup…", dim, pad, room),
        SetupLine::Starting => marked(spinner, yellow(), "starting the test…", dim, pad, room),
        SetupLine::SettingsSaved => {
            let when = screen
                .settings_seen_at
                .map(|at| format!(" {}", ago(at.elapsed().as_secs() as i64)))
                .unwrap_or_default();
            marked(
                "✓",
                green(),
                &format!("settings saved{when} · v tests them"),
                dim,
                pad,
                room,
            )
        }
        SetupLine::Testing(progress) => {
            let said = if progress.is_empty() {
                "testing the setup…".to_string()
            } else {
                format!("testing: {}…", progress.join(" · "))
            };
            marked(spinner, yellow(), &said, dim, pad, room)
        }
        SetupLine::Passed => marked("✓", green(), "the test passed", dim, pad, room),
        SetupLine::Failed { reason, by_program } => {
            let mut lines = marked(
                "✗",
                red(),
                &format!(
                    "the test failed: {reason} · a copies the prompt, which now includes this"
                ),
                dim,
                pad,
                room,
            );
            if by_program {
                lines.extend(wrapped(
                    "Your agent is probably on it.",
                    pad + 2,
                    room.saturating_sub(2).max(1),
                    Style::new().fg(text_muted()),
                ));
            }
            lines
        }
        SetupLine::Interrupted => marked(
            "○",
            yellow(),
            "the last test was interrupted · v tests again",
            dim,
            pad,
            room,
        ),
        SetupLine::NotSetUp { slot } => marked(
            "○",
            yellow(),
            &format!("not set up: {slot} is open · a copies the prompt"),
            dim,
            pad,
            room,
        ),
        SetupLine::OwnGuess(trying) => guess_lines(app, &trying, pad, room),
    }
}

/// pando's own guess: working it out, or why it stopped with nothing
/// written.
fn guess_lines(app: &App, trying: &Trying, pad: usize, room: usize) -> Vec<Line<'static>> {
    let project = &app.paths.project.display_name;
    let dim = Style::new().fg(text_dim());
    match trying {
        Trying::Resolving => marked(
            app.spinner(),
            yellow(),
            &format!("pando is trying its own guess for {project}…"),
            dim,
            pad,
            room,
        ),
        Trying::CannotTell => marked(
            "✗",
            red(),
            &format!(
                "pando can't tell how {project} starts; this one needs your agent · a copies \
                 the prompt"
            ),
            dim,
            pad,
            room,
        ),
        // Doctor's line, and its fix a line at a time: each is one a
        // developer may copy.
        Trying::NeedsPrelude { line, fix } => {
            let mut lines = marked("!", yellow(), line, dim, pad, room);
            let muted = Style::new().fg(text_muted());
            for row in fix.iter().flat_map(|fix| fix.lines()) {
                lines.extend(wrapped(row, pad + 2, room.saturating_sub(2).max(1), muted));
            }
            lines.extend(wrapped(
                "pando never sets a runtime prelude on its own: it is for every project on this \
                 machine · a copies the prompt",
                pad + 2,
                room.saturating_sub(2).max(1),
                dim,
            ));
            lines
        }
    }
}

fn blank() -> Block {
    Block {
        priority: BLANK,
        lines: vec![Line::raw("")],
    }
}

/// `1  copy this prompt`, with `a copies it` at the right edge when it
/// fits there and after it otherwise.
fn step_one(pad: usize, room: usize) -> Line<'static> {
    let what = "copy this prompt";
    let key_hint = "a copies it";
    let used = 3 + what.len() + key_hint.len();
    let gap = if room > used + 2 { room - used } else { 3 };
    truncate_line(
        Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(
                "1  ",
                Style::new().fg(orange()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(what, Style::new().fg(text())),
            Span::raw(" ".repeat(gap)),
            Span::styled("a", Style::new().fg(orange()).add_modifier(Modifier::BOLD)),
            Span::styled(" copies it", Style::new().fg(text_muted())),
        ]),
        pad + room,
    )
}

/// A numbered step, its text wrapped under itself.
fn step(number: &str, what: &str, pad: usize, room: usize) -> Vec<Line<'static>> {
    let rows = wrap_text(what, room.saturating_sub(3).max(1));
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let lead = if i == 0 {
                Span::styled(
                    format!("{number}  "),
                    Style::new().fg(orange()).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("   ")
            };
            Line::from(vec![
                Span::raw(" ".repeat(pad)),
                lead,
                Span::styled(row, Style::new().fg(text())),
            ])
        })
        .collect()
}

/// `text` wrapped to `room` cells, each row indented by `pad`.
fn wrapped(text: &str, pad: usize, room: usize, style: Style) -> Vec<Line<'static>> {
    wrap_text(text, room)
        .into_iter()
        .map(|row| Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(row, style)]))
        .collect()
}

/// A mark and its text, the text wrapped under itself.
fn marked(
    mark: &str,
    color: Color,
    text: &str,
    style: Style,
    pad: usize,
    room: usize,
) -> Vec<Line<'static>> {
    let lead = text_width(mark) + 1;
    wrap_text(text, room.saturating_sub(lead).max(1))
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let head = if i == 0 {
                Span::styled(format!("{mark} "), Style::new().fg(color))
            } else {
                Span::raw(" ".repeat(lead))
            };
            Line::from(vec![
                Span::raw(" ".repeat(pad)),
                head,
                Span::styled(row, style),
            ])
        })
        .collect()
}

/// The rows that fit in `height`, dropping the least important blocks
/// first — the last of equals first, so the top of the screen holds.
fn fit(mut blocks: Vec<Block>, height: usize) -> Vec<Line<'static>> {
    let rows = |blocks: &[Block]| blocks.iter().map(|b| b.lines.len()).sum::<usize>();
    while rows(&blocks) > height {
        let Some(least) = blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.priority < PROMPT)
            .min_by_key(|(i, b)| (b.priority, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            break;
        };
        blocks.remove(least);
    }
    blocks.into_iter().flat_map(|b| b.lines).collect()
}
