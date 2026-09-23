//! CLI remedies rewritten as the TUI keys that do the same thing.
//!
//! An action's error ends with what to do next, and the actions are shared
//! with the CLI, so that advice is sometimes a flag: `pass --force`,
//! `start without --isolated`. In the TUI nobody types a flag. Each row
//! below is one phrase and the key that stands for it; a phrase the
//! actions no longer use simply never matches, so the table can only go
//! stale, never break a message.

/// Phrase, replacement. Longest first where one contains another, so the
/// specific rewrite wins over the general one.
const REMEDIES: &[(&str, &str)] = &[
    // The neutral spellings in `crate::remedy`: "confirm" is a flag on the
    // command line and a key here.
    (
        "confirm to let git discard them",
        "press F in the remove dialog to let git discard them",
    ),
    (
        "confirm to remove it without Docker",
        "press F in the remove dialog to remove it without Docker",
    ),
    (
        "confirm to remove it anyway",
        "press y in the remove dialog to remove it anyway",
    ),
    (
        "name the branch to fork it from",
        "pick the base with tab in the new-worktree dialog",
    ),
    (
        "start it on the project's shared services instead",
        "press S to start it on the project's shared services instead",
    ),
    (
        "or pass --force to let git discard them",
        "or press F in the remove dialog to remove it anyway",
    ),
    (
        "pass --force to let git discard them",
        "press F in the remove dialog to remove it anyway",
    ),
    (
        "pass --force",
        "press F in the remove dialog to remove it anyway",
    ),
    ("start without --isolated", "press S to start it shared"),
    ("start it without --isolated", "press S to start it shared"),
    ("start --shared", "S (start it shared)"),
    ("start --isolated", "i (start it isolated)"),
    (
        "pass --yes to remove it anyway",
        "confirm in the remove dialog",
    ),
    ("--force", "F in the remove dialog"),
    ("--isolated", "i"),
    ("--shared", "S"),
];

/// `message` with every CLI remedy it contains said as a TUI key.
pub fn as_tui_remedy(message: &str) -> String {
    let mut out = message.to_string();
    for (phrase, key) in REMEDIES {
        if out.contains(phrase) {
            out = out.replace(phrase, key);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::as_tui_remedy;

    // A neutral remedy with no row here would reach the screen as
    // "confirm", which in the TUI means nothing until it names the key.
    #[test]
    fn every_neutral_remedy_is_said_as_a_key() {
        for remedy in crate::remedy::ALL {
            assert_ne!(
                as_tui_remedy(remedy.neutral),
                remedy.neutral,
                "{:?} has no TUI spelling",
                remedy.neutral
            );
        }
    }
}
