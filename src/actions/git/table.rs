//! Every action the git menu has, as data: the key that picks it in the
//! menu, its word, and what the row says while it runs.

/// One thing the git menu can do to a checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAction {
    Fetch,
    Pull,
    Rebase,
    Merge,
    Abort,
}

/// One row of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRow {
    pub action: GitAction,
    /// The key that picks it while the menu is open.
    pub key: char,
    /// What the menu calls it.
    pub word: &'static str,
    /// What its row in the list says while it runs.
    pub verb: &'static str,
}

/// Every action, in the order the menu lists them. A test holds the
/// TUI's menu keys to this table.
pub const ACTIONS: &[ActionRow] = &[
    ActionRow {
        action: GitAction::Fetch,
        key: 'f',
        word: "fetch",
        verb: "fetching",
    },
    ActionRow {
        action: GitAction::Pull,
        key: 'p',
        word: "pull",
        verb: "pulling",
    },
    ActionRow {
        action: GitAction::Rebase,
        key: 'r',
        word: "rebase",
        verb: "rebasing",
    },
    ActionRow {
        action: GitAction::Merge,
        key: 'm',
        word: "merge",
        verb: "merging",
    },
    ActionRow {
        action: GitAction::Abort,
        key: 'a',
        word: "abort",
        verb: "aborting",
    },
];

impl GitAction {
    pub fn row(self) -> &'static ActionRow {
        ACTIONS
            .iter()
            .find(|row| row.action == self)
            .expect("every action has a row")
    }

    pub fn key(self) -> char {
        self.row().key
    }

    pub fn word(self) -> &'static str {
        self.row().word
    }

    pub fn verb(self) -> &'static str {
        self.row().verb
    }
}
