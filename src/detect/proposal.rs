//! Proposals: the slots detection fills, the candidates for each, and
//! `propose`, which asks every slot.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{PortsSpec, ProcessConfig};

use super::dev::{
    dev_cmd_proposal, install_proposal, port_proposal, provision_proposal, version_files_proposal,
};
use super::frameworks::framework;
use super::services::{MachineEvidence, ServiceSource, schema_hook_proposal, services_proposal};
use super::signals::Signals;
use super::workspaces::processes_proposal;

/// A config slot detection has something to say about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    Install,
    VersionFiles,
    /// The shell line that makes this machine resolve the runtime the
    /// project asks for. The one slot whose answer is about the laptop
    /// rather than the repository, which is why it is written to the user
    /// layer — see [`Slot::layer`].
    Prelude,
    /// The shape of the whole `[processes]` table: one process, or one per
    /// app of a workspace. Asked before the slots that fill a single
    /// process, because it decides whether there is one.
    Processes,
    DevCmd,
    PortEnv,
    /// Which of the compose file's services a worktree runs private
    /// copies of. The one multi-select slot: its answer is a set rather
    /// than a value.
    Services,
    /// The command that brings a fresh database up to the current schema,
    /// and the files whose change means it has to run again.
    SchemaHook,
    Provision,
}

impl Slot {
    /// Whether this slot's answer is a set of the options rather than one
    /// of them.
    pub fn is_multi(self) -> bool {
        self == Slot::Services
    }

    /// Whether "none of them" is an answer here, and one worth recording:
    /// a process with no port, a machine that needs no line in front of
    /// its commands, a worktree that needs no local file, an empty set of
    /// services. An answer nothing can record is asked again on every run.
    pub fn allows_none(self) -> bool {
        // The schema step's "no" is the step pando found, written with
        // `on = "never"`: switched off, still visible.
        matches!(
            self,
            Slot::PortEnv | Slot::Prelude | Slot::Provision | Slot::SchemaHook
        ) || self.is_multi()
    }

    /// Whether a command typed by hand is an answer. Every slot but the
    /// set question: "these three" is a subset of the options, and there
    /// is no command to type in place of "which of these containers".
    pub fn allows_custom(self) -> bool {
        !self.is_multi()
    }

    /// Whether this slot's single answer is a *list* written as one
    /// comma-separated value: which files pin the runtime, which files a
    /// worktree gets a copy of.
    ///
    /// One value rather than a set of options, because a developer may
    /// name a file no rule found — but still a list, which is why
    /// `--answers` takes a JSON array here and [`join_list`](super::join_list) turns it
    /// into the form [`edits`](super::edits) splits again.
    pub fn is_list(self) -> bool {
        matches!(self, Slot::VersionFiles | Slot::Provision)
    }

    /// Which file this slot's answer is written to.
    ///
    /// Everything a project needs goes in the project layer, because it is
    /// true of the repository wherever it is checked out. The prelude does
    /// not: which version manager has to be initialised is a property of
    /// this machine, and a developer answers it once per laptop rather
    /// than once per project. The requirement stays a project fact; only
    /// the mechanism moves.
    pub fn layer(self) -> crate::config::Layer {
        match self {
            Slot::Prelude => crate::config::Layer::User,
            _ => crate::config::Layer::Project,
        }
    }

    /// The array of tables this slot appends to, for the two slots whose
    /// answer is a whole `[[table]]` entry.
    pub fn array(self) -> Option<&'static str> {
        match self {
            Slot::Services => Some("services"),
            Slot::SchemaHook => Some("hooks"),
            _ => None,
        }
    }
    /// Where the answer is written, as a table path plus a key. `None` for
    /// [`Slot::Processes`], whose answer is whole tables rather than one
    /// key; [`edits`](super::edits) is what knows how to write that.
    pub fn key(self) -> Option<(&'static [&'static str], &'static str)> {
        Some(match self {
            Slot::Install => (&["project"], "install"),
            Slot::VersionFiles => (&["runtime"], "version_files"),
            Slot::Prelude => (&["runtime"], "prelude"),
            Slot::DevCmd => (&["dev"], "cmd"),
            Slot::PortEnv => (&["dev"], "ports"),
            Slot::Provision => (&["project"], "provision"),
            Slot::Processes | Slot::Services | Slot::SchemaHook => return None,
        })
    }

    /// What a typed answer to this slot is, for "something else — type
    /// the …": a port answer is variable names, not a command.
    pub fn custom_noun(self) -> &'static str {
        match self {
            Slot::PortEnv => "variable names",
            Slot::VersionFiles | Slot::Provision => "file names",
            Slot::Prelude => "shell line",
            _ => "command",
        }
    }

    /// The question asked when the rules cannot decide.
    pub fn prompt(self) -> &'static str {
        match self {
            Slot::Install => "Which command installs this project's dependencies?",
            Slot::VersionFiles => "Which file pins this project's runtime version?",
            Slot::Prelude => {
                "Which line should pando run first, so this shell resolves the \
                              runtime the project asks for?"
            }
            Slot::Processes => "Run these as separate processes?",
            Slot::DevCmd => "Which command starts the local development server?",
            // Plural: a project whose env example names its ports by role
            // answers this with all of them at once, each owning a role.
            Slot::PortEnv => "Which environment variables carry this project's ports?",
            Slot::Services => "Run private copies of these services for each worktree?",
            Slot::SchemaHook => "Which command brings a fresh database up to the schema?",
            Slot::Provision => "Which local files should each worktree get a copy of?",
        }
    }
}

/// What a compose service candidate carries besides its name: the file it
/// is declared in, and the environment key the app reads to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceHint {
    /// The compose file that declares it, or the recipe that runs it.
    pub source: ServiceSource,
    /// `None` when nothing in the env example points at this service. Such
    /// a service can still be run, but the app is never told where it is.
    pub env_key: Option<String>,
}

impl ServiceHint {
    /// The compose file, when this is a compose service.
    pub fn file(&self) -> Option<&str> {
        match &self.source {
            ServiceSource::Compose { file } => Some(file),
            ServiceSource::Native { .. } => None,
        }
    }

    /// The recipe, when this is a native one.
    pub fn recipe(&self) -> Option<&str> {
        match &self.source {
            ServiceSource::Native { recipe } => Some(recipe),
            ServiceSource::Compose { .. } => None,
        }
    }
}

/// One thing a rule found, and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidate {
    /// What the developer sees at a question, and the value written for
    /// every slot that holds one. For [`Slot::Processes`] it is a summary
    /// of `processes` below, because that answer is several tables.
    pub value: String,
    /// The signal that made this a candidate, written into the config as
    /// `# detected: <why>`.
    pub why: String,
    /// Roles this candidate brings with it, for a command that already
    /// carries `{port:<role>}`. When set, the port slot is already answered.
    pub ports: Option<PortsSpec>,
    /// Whole processes, for the one slot whose answer is not a single
    /// value: the workspace form proposes a table per app.
    pub processes: Option<BTreeMap<String, ProcessConfig>>,
    /// For a compose service: where it is declared and which env key
    /// points at it.
    pub service: Option<ServiceHint>,
    /// Whether a multi-select question starts with this option ticked,
    /// because a rule resolved it rather than guessed at it.
    pub preselected: bool,
    /// For the schema slot: the whole `[[hooks]]` entry, because a hook is
    /// a command *and* the files it is keyed on.
    pub hook: Option<crate::config::HookConfig>,
    /// For the provision slot: which of the paths in `value` have no file
    /// in the main checkout and come from an example instead. Empty for
    /// the ordinary answer, where every path is a file that is already
    /// there.
    pub provision_from: BTreeMap<String, String>,
    /// Whether only a human may take this option. `--yes` takes a
    /// question's preselected option, and every other slot's options are
    /// a command or a name pando itself authored and can vouch for. An
    /// option that makes pando **create a file out of contents it did not
    /// write and cannot read** is not one a flag may accept on a
    /// developer's behalf, so it is never preselected: `--yes` falls
    /// through to the first option that is, and prints the question when
    /// there is none.
    pub needs_a_human: bool,
}

/// What the rules found for one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub slot: Slot,
    /// In rule order: the first is the one a question preselects.
    pub candidates: Vec<Candidate>,
    /// Whether the rules are sure enough to take the first candidate
    /// without asking. One candidate is sure; so is a script named exactly
    /// `dev` that really is one dev server.
    pub decided: bool,
    /// The compose file a services proposal is about, for the one case
    /// where no candidate can carry it: every service the file declares was
    /// filtered out, and the answer — "none of them" — is still about that
    /// file and still has to be written down.
    pub file: Option<String>,
    /// Why there is nothing to choose from, when a rule settled the slot
    /// with the empty answer instead of finding candidates for it. `None`
    /// whenever there are any.
    pub none_because: Option<String>,
    /// For a services proposal: `compose` or `native`, and the evidence
    /// that decided between them.
    pub mechanism: Option<&'static str>,
    pub evidence: Vec<String>,
}

impl Proposal {
    /// What every rule but the services one makes: candidates, and whether
    /// they settle the slot. The two fields below them belong to the one
    /// answer that has no candidates to hang anything on.
    pub fn of(slot: Slot, candidates: Vec<Candidate>, decided: bool) -> Proposal {
        Proposal {
            slot,
            candidates,
            decided,
            file: None,
            none_because: None,
            mechanism: None,
            evidence: Vec::new(),
        }
    }

    pub fn preferred(&self) -> Option<&Candidate> {
        self.candidates.first()
    }

    /// The options a multi-select question pre-checks: the ones a rule
    /// already resolved.
    pub fn preselected(&self) -> Vec<usize> {
        self.candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.preselected)
            .map(|(i, _)| i)
            .collect()
    }

    /// What the rules would take with nobody asked: the pre-checked set
    /// for a multi-select slot, the first candidate for every other one.
    pub fn preferred_set(&self) -> Vec<&Candidate> {
        if self.slot.is_multi() {
            return self.candidates.iter().filter(|c| c.preselected).collect();
        }
        self.preferred().into_iter().collect()
    }

    /// The compose file every candidate of a services proposal came from.
    /// The answer "none of them" is about that file, so it outlives the
    /// candidates a set answer happens to pick — and the proposal's own
    /// `file` outlives having any candidates at all.
    pub fn service_file(&self) -> Option<&str> {
        self.file.as_deref().or_else(|| {
            self.candidates
                .iter()
                .find_map(|c| c.service.as_ref())
                .and_then(ServiceHint::file)
        })
    }
}

/// Reads a compose file this crate's own parser could not follow whole.
///
/// `extends:` and a top-level `include:` are the two, and only compose
/// itself resolves them. Detection is handed one of these so it can ask
/// when Docker is there; [`propose`] passes none and falls back to what the
/// parser could read, which is why a refusal says so.
pub type ComposeResolver<'a> = &'a dyn Fn(&Path) -> Option<crate::compose::ComposeFile>;

/// Everything tier 1 has to say, in the order the slots are filled.
pub fn propose(root: &Path, signals: &Signals) -> Vec<Proposal> {
    propose_with(root, signals, None, &MachineEvidence::unknown(), None)
}

/// [`propose`] with a way to resolve a compose file the parser is a subset
/// of.
pub fn propose_with(
    root: &Path,
    signals: &Signals,
    resolve: Option<ComposeResolver<'_>>,
    evidence: &MachineEvidence,
    prefer: Option<&str>,
) -> Vec<Proposal> {
    let rule = framework(root, signals);
    [
        install_proposal(root, signals),
        version_files_proposal(signals),
        // Before the single-process slots: it decides whether there is one
        // process or several, and the slots below only fill a single one.
        processes_proposal(root, signals),
        dev_cmd_proposal(signals, rule),
        port_proposal(signals, rule),
        // After the processes, because a service is only worth proposing
        // once there is something to talk to it; before the schema hook,
        // whose whole point is to run once the services are up.
        services_proposal(root, signals, resolve, evidence, prefer),
        schema_hook_proposal(root, signals),
        provision_proposal(signals),
    ]
    .into_iter()
    .flatten()
    .collect()
}
