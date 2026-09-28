//! What a program decided that the rules could not.
//!
//! Every answer that reaches [`crate::actions::resolve`] from a program
//! rather than a person — `pando init --answers` today — is appended to
//! `~/.pando/projects/<id>/decisions.jsonl` with the evidence it was
//! decided from, and a later line records it when a person changes their
//! mind about it.
//!
//! This exists because a skill that answers questions the rules cannot is
//! a crutch unless somebody reads what it answered. The log is the
//! labelled corpus that improves the rules themselves, and the rules are
//! what everybody gets — including every developer who has no agent. A
//! slot that shows up here again and again is a rule waiting to be
//! written; a slot whose answers are overridden as often as they are kept
//! is a rule that should never have been trusted.
//!
//! Three properties it has to have, because nothing else in pando will
//! notice if it loses them:
//!
//! - **Append-only.** One `O_APPEND` write per line, no rewrite, no
//!   truncate. A torn tail from a process that died mid-line costs that
//!   line and nothing else — [`read`] skips what it cannot parse.
//! - **Never fatal.** A log that fails to write must not fail the command
//!   that was writing it. The caller says so out loud and carries on.
//! - **Nothing a person did not already have.** Every field here is
//!   either the answer that went into `pando.toml` or the evidence
//!   `pando signals` already publishes. There is nothing to leak.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::detect::Slot;
use crate::paths::PandoPaths;

/// Shape version for one line of the log, bumped independently of the
/// CLI's `JSON_VERSION`: this file outlives the release that wrote it, and
/// a reader has to know what it is holding.
pub const VERSION: u32 = 1;

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub version: u32,
    pub at: DateTime<Utc>,
    pub slot: Slot,
    #[serde(flatten)]
    pub what: What,
}

/// What happened to the slot. Flattened into the line, with `kind` naming
/// it, so every line is one flat object a shell tool can filter on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum What {
    /// A program answered a question the rules could not decide.
    Answer {
        /// The answer, in exactly the shape an answers file would send:
        /// a string, a list of strings, null, or an object of process
        /// tables. The log can therefore be turned back into an answers
        /// file, which is what makes it replayable rather than only
        /// readable.
        answer: serde_json::Value,
        shape: Shape,
        /// What config said about this slot once the answer was written,
        /// read back from the file rather than reported from memory. It
        /// is the value a later run compares against to notice a person
        /// changing it.
        wrote: Option<String>,
        evidence: Evidence,
    },
    /// A person changed what a program had answered.
    ///
    /// The label this whole file exists for: an answer nobody corrected is
    /// weak evidence that it was right, and an answer somebody replaced is
    /// strong evidence that it was wrong.
    Override {
        was: Option<String>,
        now: Option<String>,
    },
}

/// Which of the answer shapes it was — the same four `--answers` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// One of the options pando offered, named by its own text.
    Choice,
    /// A command or path of the answerer's own, matching no option — or
    /// process tables of their own, at the process list.
    Custom,
    /// Several of the options, at the one question whose answer is a set.
    Set,
    /// "None of them", where that is an answer.
    None,
}

/// What the answerer had to go on: the question as it was asked, and every
/// option with the signal that found it.
///
/// Deliberately the same facts `pando signals` publishes for the slot. A
/// corpus whose evidence column is a summary somebody wrote afterwards
/// cannot be used to test a rule against the case it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub prompt: String,
    /// What the question printed above its options: what the project asks
    /// for, what this machine answered, where from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// For the services question: which mechanism it was about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<String>,
    /// And the facts that chose that mechanism, in the order they were
    /// weighed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weighed: Vec<String>,
    /// The option the rules would have taken, when there was one.
    pub preferred: Option<usize>,
    pub options: Vec<Opt>,
}

/// One option, as it was offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opt {
    pub value: String,
    /// The signal that made it an option.
    pub why: String,
    pub preselected: bool,
    /// Whether only a person may take it — an option a flag is not allowed
    /// to accept on anybody's behalf.
    pub needs_a_human: bool,
}

impl Entry {
    /// A program's answer, stamped now.
    pub fn answered(
        slot: Slot,
        answer: serde_json::Value,
        shape: Shape,
        wrote: Option<String>,
        evidence: Evidence,
    ) -> Entry {
        Entry {
            version: VERSION,
            at: Utc::now(),
            slot,
            what: What::Answer {
                answer,
                shape,
                wrote,
                evidence,
            },
        }
    }

    fn overridden(slot: Slot, was: Option<String>, now: Option<String>) -> Entry {
        Entry {
            version: VERSION,
            at: Utc::now(),
            slot,
            what: What::Override { was, now },
        }
    }

    /// What this line leaves the slot at, whichever kind of line it is.
    fn value(&self) -> Option<&str> {
        match &self.what {
            What::Answer { wrote, .. } => wrote.as_deref(),
            What::Override { now, .. } => now.as_deref(),
        }
    }
}

/// Appends one line.
///
/// Creates the project directory if it is not there: a program may answer
/// questions about a project nothing has started yet, and the directory is
/// pando's own home, which is one of the three places it may write.
pub fn append(paths: &PandoPaths, entry: &Entry) -> Result<()> {
    let path = paths.decisions_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    // One line, one write, appended: two pando processes answering at once
    // interleave lines rather than corrupting each other's.
    let line = serde_json::to_string(entry).context("serialise a decision")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("append to {}", path.display()))?;
    Ok(())
}

/// Every line the file holds, oldest first.
///
/// A line that does not parse is skipped rather than failing the read: an
/// append-only file that a process died halfway through has a torn tail,
/// and losing that line is the whole cost of it.
pub fn read(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .collect()
}

/// What the log believes each slot a program answered is set to.
///
/// The last word per slot: an answer, then whatever overrides came after
/// it. A slot nothing in the log mentions is not in the map, which is what
/// keeps [`note_overrides`] to the slots it has something to say about.
fn last_known(entries: &[Entry]) -> Vec<(Slot, Option<String>)> {
    let mut out: Vec<(Slot, Option<String>)> = Vec::new();
    for entry in entries {
        let value = entry.value().map(str::to_string);
        match out.iter_mut().find(|(slot, _)| *slot == entry.slot) {
            Some(existing) => existing.1 = value,
            None => out.push((entry.slot, value)),
        }
    }
    out
}

/// Records every slot a program answered whose answer is not what config
/// says any more.
///
/// `current` is asked what config says about a slot now; the log holds
/// what it said when the program answered. A difference is a person having
/// changed their mind, which is the one label in this file that cannot be
/// collected any other way.
///
/// The comparison is on the slot's *answer*, not on the file, and the two
/// differ in one place worth knowing about. The `processes` question is
/// answered with a shape — a process per app, or the root script — so its
/// value is the process names: switching between the two forms is
/// recorded, and editing a command inside the form that was chosen is
/// not. That is the right granularity for a corpus about the decision
/// that was made, and it is a limit rather than a feature everywhere
/// else: an edit that leaves the slot's value alone is not noticed.
/// Silence is the error this errs towards, because an override pando
/// invented would poison the corpus it exists to build.
pub fn note_overrides(
    paths: &PandoPaths,
    current: &dyn Fn(Slot) -> Option<String>,
    progress: &dyn Fn(&str),
) -> Vec<Slot> {
    let path = paths.decisions_file();
    let entries = read(&path);
    if entries.is_empty() {
        return Vec::new();
    }
    let mut noted = Vec::new();
    for (slot, was) in last_known(&entries) {
        let now = current(slot);
        if now == was {
            continue;
        }
        let entry = Entry::overridden(slot, was, now);
        match append(paths, &entry) {
            Ok(()) => noted.push(slot),
            // Said out loud, and not fatal: a log that cannot be written
            // is not a reason to fail the command that was writing it.
            Err(e) => progress(&format!("could not record a decision: {e:#}")),
        }
    }
    noted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use tempfile::{TempDir, tempdir};

    /// A pando home in a temp directory, for a project that needs no
    /// repository: nothing here reads one.
    fn fixture_paths() -> (TempDir, PandoPaths) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        (dir, paths)
    }

    fn evidence() -> Evidence {
        Evidence {
            prompt: "Which command starts the local development server?".to_string(),
            details: vec![],
            mechanism: None,
            weighed: vec![],
            preferred: Some(0),
            options: vec![Opt {
                value: "pnpm dev".to_string(),
                why: "package.json scripts.dev".to_string(),
                preselected: true,
                needs_a_human: false,
            }],
        }
    }

    fn answer(slot: Slot, wrote: &str) -> Entry {
        Entry::answered(
            slot,
            serde_json::Value::String(wrote.to_string()),
            Shape::Choice,
            Some(wrote.to_string()),
            evidence(),
        )
    }

    #[test]
    fn a_line_round_trips_through_the_file() {
        let (_dir, paths) = fixture_paths();
        let entry = answer(Slot::DevCmd, "pnpm dev:web");
        append(&paths, &entry).unwrap();
        let back = read(&paths.decisions_file());
        assert_eq!(back, vec![entry]);
    }

    // The shape a program reads: one flat object per line, `kind` naming
    // which kind it is, the slot under the name `signals` publishes.
    #[test]
    fn a_line_is_one_flat_object_with_the_published_names() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::SchemaHook, "prisma migrate deploy")).unwrap();
        let text = std::fs::read_to_string(paths.decisions_file()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["version"], VERSION);
        assert_eq!(v["kind"], "answer");
        assert_eq!(v["slot"], "schema_hook");
        assert_eq!(v["shape"], "choice");
        assert_eq!(v["answer"], "prisma migrate deploy");
        assert_eq!(
            v["evidence"]["options"][0]["why"],
            "package.json scripts.dev"
        );
    }

    #[test]
    fn appending_never_rewrites_what_is_already_there() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::DevCmd, "pnpm dev")).unwrap();
        let first = std::fs::read_to_string(paths.decisions_file()).unwrap();
        append(&paths, &answer(Slot::PortEnv, "PORT")).unwrap();
        let second = std::fs::read_to_string(paths.decisions_file()).unwrap();
        assert!(second.starts_with(&first), "{second}");
        assert_eq!(read(&paths.decisions_file()).len(), 2);
    }

    // A process that died mid-write costs the line it was writing, and
    // nothing else: every complete line before it still reads.
    #[test]
    fn a_torn_last_line_costs_that_line_and_no_other() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::DevCmd, "pnpm dev")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.decisions_file())
            .unwrap();
        write!(file, "{{\"version\":1,\"slot\":\"port_e").unwrap();
        drop(file);
        let back = read(&paths.decisions_file());
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].slot, Slot::DevCmd);
    }

    #[test]
    fn a_missing_file_reads_as_nothing_rather_than_failing() {
        let (_dir, paths) = fixture_paths();
        assert!(read(&paths.decisions_file()).is_empty());
        assert!(note_overrides(&paths, &|_| None, &|_| {}).is_empty());
    }

    // The whole point of the file: a person changing what a program wrote
    // is the label that says the rule behind it was not good enough.
    #[test]
    fn a_value_that_changed_is_recorded_as_an_override() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::DevCmd, "pnpm dev:web")).unwrap();

        let noted = note_overrides(
            &paths,
            &|slot| (slot == Slot::DevCmd).then(|| "pnpm dev:all".to_string()),
            &|_| {},
        );
        assert_eq!(noted, vec![Slot::DevCmd]);
        let back = read(&paths.decisions_file());
        assert_eq!(back.len(), 2);
        assert_eq!(
            back[1].what,
            What::Override {
                was: Some("pnpm dev:web".to_string()),
                now: Some("pnpm dev:all".to_string()),
            }
        );
    }

    // And it is recorded once. The override is itself the last word, so a
    // second pass over an unchanged file has nothing to say.
    #[test]
    fn an_override_is_recorded_once_not_on_every_run() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::DevCmd, "pnpm dev:web")).unwrap();
        let current = |slot: Slot| (slot == Slot::DevCmd).then(|| "pnpm dev:all".to_string());
        assert_eq!(
            note_overrides(&paths, &current, &|_| {}),
            vec![Slot::DevCmd]
        );
        assert!(note_overrides(&paths, &current, &|_| {}).is_empty());
        assert_eq!(read(&paths.decisions_file()).len(), 2);
    }

    // A key deleted by hand is a change too, and the one a comparison
    // against the answer's own text would miss.
    #[test]
    fn an_answer_a_person_deleted_is_an_override_to_nothing() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::Provision, ".env")).unwrap();
        assert_eq!(
            note_overrides(&paths, &|_| None, &|_| {}),
            vec![Slot::Provision]
        );
        assert_eq!(
            read(&paths.decisions_file())[1].what,
            What::Override {
                was: Some(".env".to_string()),
                now: None,
            }
        );
    }

    // Only the slots the log knows about. A question a person answered, or
    // one a rule decided, is not this file's business.
    #[test]
    fn a_slot_no_program_answered_is_never_an_override() {
        let (_dir, paths) = fixture_paths();
        append(&paths, &answer(Slot::DevCmd, "pnpm dev")).unwrap();
        let noted = note_overrides(
            &paths,
            &|slot| match slot {
                Slot::DevCmd => Some("pnpm dev".to_string()),
                // Everything else says something, and none of it was a
                // program's to begin with.
                _ => Some("something a person wrote".to_string()),
            },
            &|_| {},
        );
        assert!(noted.is_empty(), "{noted:?}");
        assert_eq!(read(&paths.decisions_file()).len(), 1);
    }
}
