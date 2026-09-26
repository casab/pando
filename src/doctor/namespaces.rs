//! Namespaced mode's leftovers: databases named like a worktree's of this
//! project that no pando project's record holds.

use crate::actions;
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state;

use super::report::{Finding, Section};

/// A note for each leftover database, with the command that drops it.
///
/// Listed and never dropped: a database pando has no record of is one it
/// cannot prove it made, and the developer is the one who can say. Asked
/// of the servers only for a project that runs namespaced worktrees. While
/// another project's state cannot be read the note says so, and gives no
/// command: that project may hold it.
pub(super) fn leftover_findings(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    // Read straight, like every other section: doctor writes nothing.
    let Ok(store) = state::load(&paths.state_file()) else {
        return;
    };
    for leftover in actions::namespace_leftovers(paths, config, &store) {
        let head = format!(
            "{}: database {} on {} is named like a worktree's of this project",
            leftover.service, leftover.name, leftover.address
        );
        if let Some((project, why)) = &leftover.unread {
            findings.push(Finding::note(
                Section::Services,
                format!(
                    "{head}, and no record pando can read holds it — but project {project}'s \
                     state could not be read, so it may be that project's: {why}"
                ),
            ));
            continue;
        }
        let finding = Finding::note(
            Section::Services,
            format!(
                "{head}, and no pando project's record holds it — one `rm` could not drop, one \
                 whose record was lost, or one that is not pando's at all"
            ),
        );
        findings.push(match &leftover.by_hand {
            Some(command) => finding.with_fix(format!(
                "`{command}` drops it, if nothing of yours is in it — pando never drops what it \
                 has no record of"
            )),
            None => finding,
        });
    }
}
