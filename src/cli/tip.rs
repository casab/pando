//! The first-time tip: `new` and `start` on a project with nothing to run
//! yet hand a person at a terminal the setup prompt, once.

use crate::config::Config;
use crate::paths::PandoPaths;
use crate::setup::{self, SETUP_PROMPT, SetupState};

/// Says the setup prompt through `say` when the project is new, `terminal`
/// is true, and the tip has never been shown for it; returns whether it
/// did. Never a gate: the command goes on the same either way.
///
/// `config` must be the config as loaded, before the command resolves its
/// questions: resolving writes the rules' first choices into the project's
/// config, and after that the project is no longer new.
///
/// Both kinds of new count, skipped or not. `esc` on the TUI's setup
/// screen skipped that screen, not this tip, and a developer who pressed
/// it has still never been told the prompt outside the TUI.
///
/// The memory is saved before anything is said. A tip that cannot be
/// remembered would be printed again on every `new` and `start`, which is
/// worse than not printing it: the same prompt is on the TUI's setup
/// screen and in the README. So a save that fails says nothing, and costs
/// only this once.
///
/// Not on a terminal nothing is read or written: a script or an agent
/// never sees the tip, and its runs do not use it up.
///
/// Above the three lines, through `draw`, the project's grove — the
/// picture the setup screen opens on — so a first run from the CLI
/// starts on the same moment as one from the TUI.
pub(super) fn first_time_tip(
    paths: &PandoPaths,
    config: &Config,
    terminal: bool,
    say: &dyn Fn(&str),
    draw: &dyn Fn(&str),
) -> bool {
    if !terminal {
        return false;
    }
    let setup = setup::read(paths, config);
    if !matches!(setup.state, SetupState::New { .. }) || setup.memory.tip_shown_at.is_some() {
        return false;
    }
    let mut memory = setup.memory;
    memory.tip_shown_at = Some(chrono::Utc::now());
    if memory.save(paths).is_err() {
        return false;
    }
    let grove = super::art::grove_lines(
        super::art::stderr_columns(),
        crate::grove::seed_of(&paths.project.id),
        false,
        &crate::term::Style::for_stderr(),
    );
    if !grove.is_empty() {
        for line in &grove {
            draw(line);
        }
        draw("");
    }
    say(&format!(
        "first time in {}. To set it up with your coding agent, paste:",
        paths.project.display_name
    ));
    say(&format!("  {SETUP_PROMPT}"));
    say("`pando check` tests the setup at any time.");
    true
}
