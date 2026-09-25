//! Which key does what, as data: the one table the help overlay prints and
//! a test holds to the handlers, so help never lists a key nothing answers
//! and no key is answered that help leaves out.

use ratatui::crossterm::event::KeyCode;

/// One row of help: what its key column says, every key code the row
/// stands for, and what they do.
pub struct KeyHelp {
    pub keys: &'static str,
    /// What the contract test presses. Empty for a row that is not a bare
    /// key press (`ctrl-c`), which the test cannot drive the same way.
    pub codes: &'static [KeyCode],
    pub action: &'static str,
}

const fn key(keys: &'static str, codes: &'static [KeyCode], action: &'static str) -> KeyHelp {
    KeyHelp {
        keys,
        codes,
        action,
    }
}

/// Every key the list answers to, in the order help shows them.
pub const LIST_KEYS: &[KeyHelp] = &[
    key(
        "j k ↓ ↑",
        &[
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ],
        "move down / up",
    ),
    key(
        "g G",
        &[
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Home,
            KeyCode::End,
        ],
        "first / last",
    ),
    key(
        "⏎",
        &[KeyCode::Enter],
        "open the logs when it runs, start it when stopped",
    ),
    key(
        "s",
        &[KeyCode::Char('s')],
        "start it, in the mode it last ran in",
    ),
    key(
        "i",
        &[KeyCode::Char('i')],
        "start it isolated: private copies of the services (asks when it runs shared)",
    ),
    key(
        "S",
        &[KeyCode::Char('S')],
        "start it shared: the project's own services (asks when it runs isolated)",
    ),
    key("x", &[KeyCode::Char('x')], "stop it (x twice when it runs)"),
    key(
        "X",
        &[KeyCode::Char('X')],
        "stop everything that runs (asks first)",
    ),
    key(
        "r",
        &[KeyCode::Char('r')],
        "restart it: r twice when it runs, once when stopped starts it",
    ),
    key(
        "P",
        &[KeyCode::Char('P')],
        "restart only the ▸ process (tab picks it; P twice)",
    ),
    key("o", &[KeyCode::Char('o')], "open its URL in the browser"),
    key(
        "t",
        &[KeyCode::Char('t')],
        "share it publicly, or stop sharing (asks first)",
    ),
    key("O", &[KeyCode::Char('O')], "open its public URL"),
    key("c", &[KeyCode::Char('c')], "copy its local URL"),
    key("C", &[KeyCode::Char('C')], "copy its public URL"),
    key("y", &[KeyCode::Char('y')], "copy its path"),
    key(
        "!",
        &[KeyCode::Char('!')],
        "a shell in it (a tmux window inside tmux)",
    ),
    key("e", &[KeyCode::Char('e')], "open it in $VISUAL / $EDITOR"),
    key(
        "l",
        &[KeyCode::Char('l'), KeyCode::Char('L')],
        "open the log viewer",
    ),
    key(
        "PgUp PgDn",
        &[KeyCode::PageUp, KeyCode::PageDown],
        "scroll the log preview",
    ),
    key("tab", &[KeyCode::Tab], "preview the next process's log"),
    key("n", &[KeyCode::Char('n')], "new worktree"),
    key(
        "p",
        &[KeyCode::Char('p')],
        "open pull requests: ⏎ makes a worktree for one",
    ),
    key("d", &[KeyCode::Char('d')], "remove it"),
    key("/", &[KeyCode::Char('/')], "filter by branch or name"),
    key(
        "m",
        &[KeyCode::Char('m')],
        "messages: what pando said, in full",
    ),
    key("R", &[KeyCode::Char('R')], "refresh now"),
    key("?", &[KeyCode::Char('?')], "this help"),
    key("q esc", &[KeyCode::Char('q'), KeyCode::Esc], "quit"),
    key("ctrl-c", &[], "quit from anywhere"),
];

/// What the marks in a list row mean, shown under the keys.
pub const LIST_LEGEND: &[(&str, &str)] = &[
    ("● running", "the dev server is up"),
    ("◌ starting", "waiting for its port"),
    ("✗ failed", "⏎ shows the log that says why"),
    ("○ stopped", "s starts it"),
    ("◈", "shared publicly — O opens, C copies the URL"),
    ("isolated", "runs private copies of the services"),
    (
        "*",
        "uncommitted changes — the detail pane's git row says more",
    ),
    ("↑n ↓n", "commits ahead of / behind the base branch"),
    ("adopted", "a worktree pando did not create"),
    (
        "gh @login",
        "the GitHub account gh uses in this project's directory; R asks again",
    ),
    (
        "shared: pg ● up",
        "the project's shared services at their default ports: up answers, down does not",
    ),
    (
        "d … F",
        "in the remove dialog, F removes it even with uncommitted changes",
    ),
];

/// Every key the log viewer answers to, shown instead of the list's while
/// it is open.
pub const LOG_KEYS: &[KeyHelp] = &[
    key(
        "j k ↓ ↑",
        &[
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ],
        "move the cursor down / up",
    ),
    key("ctrl-d ctrl-u", &[], "half a page down / up"),
    key(
        "g G",
        &[
            KeyCode::Char('g'),
            KeyCode::Home,
            KeyCode::Char('G'),
            KeyCode::End,
        ],
        "top / follow the live tail",
    ),
    key(
        "1-9",
        &[KeyCode::Char('1'), KeyCode::Char('2'), KeyCode::Char('3')],
        "show that source (the numbers on the tabs)",
    ),
    key(
        "tab S-tab",
        &[KeyCode::Tab, KeyCode::BackTab],
        "next / previous source",
    ),
    key(
        "/",
        &[KeyCode::Char('/')],
        "search (⏎ keeps it, esc clears it)",
    ),
    key(
        "n N",
        &[KeyCode::Char('n'), KeyCode::Char('N')],
        "next / previous match (ctrl-n ctrl-p too)",
    ),
    key(
        "&",
        &[KeyCode::Char('&')],
        "show only the matches, grep-style",
    ),
    key(
        "f",
        &[KeyCode::Char('f')],
        "level filter: all, warnings and up, errors",
    ),
    key(
        "e E",
        &[KeyCode::Char('e'), KeyCode::Char('E')],
        "next / previous error",
    ),
    key("w", &[KeyCode::Char('w')], "wrap long lines, or cut them"),
    key(
        "⏎ J",
        &[KeyCode::Enter, KeyCode::Char('J')],
        "inspect the line (pretty-printed JSON)",
    ),
    key(
        "y Y",
        &[KeyCode::Char('y'), KeyCode::Char('Y')],
        "copy the line / the URL on it",
    ),
    key("?", &[KeyCode::Char('?')], "this help"),
    key(
        "q esc",
        &[KeyCode::Char('q'), KeyCode::Esc],
        "back to the list",
    ),
    key("ctrl-c", &[], "quit from anywhere"),
];

/// The keys that scroll help and messages rather than closing them. Any
/// other key closes either; the footer of each says so.
pub const OVERLAY_KEYS: &[KeyHelp] = &[
    key(
        "j k ↓ ↑",
        &[
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Down,
            KeyCode::Up,
        ],
        "scroll a line",
    ),
    key(
        "PgUp PgDn space",
        &[KeyCode::PageUp, KeyCode::PageDown, KeyCode::Char(' ')],
        "scroll a page",
    ),
    key(
        "g G",
        &[
            KeyCode::Char('g'),
            KeyCode::Home,
            KeyCode::Char('G'),
            KeyCode::End,
        ],
        "top / bottom",
    ),
];

/// The inspect overlay's own keys, shown under the viewer's.
pub const INSPECT_LEGEND: &[(&str, &str)] = &[
    ("j k g G", "scroll the inspected block"),
    ("y", "copy the whole block"),
    ("q esc", "close it"),
];
