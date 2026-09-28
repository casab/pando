//! The block a coding agent keeps about how to run this project with
//! pando, and the copies of it pando keeps in its own home.
//!
//! One text, three readers: the job `init --agent` prints ends with it,
//! `init --agent --reference memory` prints it alone for an agent to save
//! in its own memory once the developer says yes, and `new` writes it under the project's pando
//! directory as `CLAUDE.md` and `AGENTS.md`. Claude Code reads a
//! `CLAUDE.md` in every directory above the one it runs in, so an agent
//! opened in a worktree pando made finds it with nothing saved at all.
//! Never anywhere else: the repository is the developer's (Invariant 1).
//!
//! Only the project's name and root, and the commands: nothing that
//! changes with the settings, so a copy an agent saved once stays true.
//! The one line more is a fact about the project's app rather than its
//! settings: [`device_note`], for an app a phone or a tablet runs.

use std::fmt::Write as _;

use anyhow::{Context, Result};

use crate::catalog::frameworks;
use crate::config::{Config, ProcessConfig};
use crate::detect::{self, Slot};
use crate::paths::PandoPaths;

/// The line above the block in the files pando writes: they are pando's,
/// and the next `new` writes them again.
pub const MEMORY_FILE_HEADER: &str = "<!-- pando wrote this file and rewrites it each time it makes a worktree: \
     edits here are lost. -->";

/// What the developer has to be told about the device the project's app
/// runs on, when a phone or a tablet runs it: the processes config runs,
/// or, when it runs none yet, the ones the rules propose, take their port
/// the way a framework whose app a device reaches does.
///
/// Every address pando gives is this machine's `127.0.0.1`, which a
/// device cannot reach, and nothing else in a passing setup says so.
pub fn device_note(config: &Config, proposals: &[detect::Proposal]) -> Option<&'static str> {
    let of = |process: &ProcessConfig| {
        let vars = process.port_vars();
        let vars: Vec<&str> = vars.keys().map(String::as_str).collect();
        frameworks::device_note(&vars, &process.cmd)
    };
    if config.runnable_processes().next().is_some() {
        return config
            .runnable_processes()
            .find_map(|(_, process)| of(process));
    }
    proposals
        .iter()
        .flat_map(|proposal| proposal.candidates.iter().map(move |c| (proposal.slot, c)))
        .find_map(|(slot, candidate)| match slot {
            Slot::Processes => candidate
                .processes
                .iter()
                .flat_map(|tables| tables.values())
                .find_map(of),
            Slot::DevCmd => frameworks::device_note(&[], &candidate.value),
            // The option naming several variables is one string of them.
            Slot::PortEnv => {
                let vars: Vec<&str> = candidate.value.split(',').map(str::trim).collect();
                frameworks::device_note(&vars, "")
            }
            _ => None,
        })
}

/// How to run this project's worktrees, and its main checkout, with pando:
/// a markdown section headed with the project's name and root, which is
/// how an agent tells an earlier copy for the same project to replace.
/// `device_note` is [`device_note`]'s, one line more when there is one.
pub fn memory_block(paths: &PandoPaths, device_note: Option<&str>) -> String {
    let name = &paths.project.display_name;
    let root = paths.root().display();
    let mut out = String::new();
    let _ = writeln!(out, "## pando runs {name} ({root})\n");
    let _ = writeln!(
        out,
        "Every branch of {name} runs in a worktree of its own through pando, with its own\n\
         ports, logs and services, and the main checkout runs the same way. Start and stop\n\
         them with pando, never with the dev command by hand: the ports and services are\n\
         pando's to give.\n"
    );
    out.push_str(
        "`<name>` is a worktree's branch or directory, or the main checkout's. Always pass it:\n\
         `pando stop` with none, outside every worktree, stops them all.\n\n\
         - start: `pando start <name> --wait`, which returns once every process is ready\n\
         - stop: `pando stop <name>`; restart: `pando restart <name> --wait`\n\
         - what runs, on which ports, at which URL: `pando status <name> --json`; for a\n  \
         person, `pando open <name>`\n\
         - logs: `pando logs <name>`, `--follow` to follow, `--source <process>` for one process\n\
         - a new branch: `pando new <branch>`, then `pando start <branch> --wait`\n\
         - every worktree: `pando ls --json`\n\
         - exit 3: pando has a question, on stderr. Put it to the developer; never add `--yes`.\n",
    );
    if let Some(note) = device_note {
        let _ = writeln!(
            out,
            "- {note}. Ask the developer for the address; never guess it."
        );
    }
    out.push_str("- the rest: `pando init --agent --reference brief`\n");
    out
}

/// Writes the block, under [`MEMORY_FILE_HEADER`], to both of
/// [`PandoPaths::agent_memory_files`], replacing what was there. A file
/// that already says exactly this is left alone, so an agent watching it
/// is not told it changed.
pub fn write_memory_files(paths: &PandoPaths, device_note: Option<&str>) -> Result<()> {
    paths.ensure_home()?;
    let text = format!(
        "{MEMORY_FILE_HEADER}\n\n{}",
        memory_block(paths, device_note)
    );
    for path in paths.agent_memory_files() {
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
            continue;
        }
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, &text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename tmp → {}", path.display()))?;
    }
    Ok(())
}
