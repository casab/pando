//! The report's types: what doctor found, section by section, and the
//! findings that decide its exit code.

use serde::Serialize;

use crate::runtime;

use super::adopt::Adoptable;

/// Exit 1, with nothing more to say.
///
/// The report has already printed every problem it found, with its fix.
/// `pando: <something>` under it would be a reason the command did not
/// print — so this error's whole content is its type, which `main`
/// downcasts beside `NeedsAnswer` and `UsageError`.
#[derive(Debug, Clone, Copy)]
pub struct Unhealthy;

impl std::fmt::Display for Unhealthy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never printed by `main`, and still not empty: an error whose
        // `Display` is blank is one that shows up as `pando: ` somewhere.
        write!(f, "doctor found something that will break a command")
    }
}

impl std::error::Error for Unhealthy {}

/// Which part of the report a finding belongs to, in the order the report
/// prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Project,
    Config,
    Runtime,
    Tools,
    Worktrees,
    Services,
    Hooks,
    Adoption,
}

impl Section {
    pub(super) fn title(self) -> &'static str {
        match self {
            Section::Project => "project",
            Section::Config => "config",
            Section::Runtime => "runtime",
            Section::Tools => "tools",
            Section::Worktrees => "worktrees",
            Section::Services => "services",
            Section::Hooks => "hooks",
            Section::Adoption => "adoption",
        }
    }

    /// Every section, in print order. Runtime is high, deliberately: a
    /// process started under the wrong toolchain dies of it, and what the
    /// shell pando uses resolves is the least guessable thing here.
    pub(super) const ALL: [Section; 8] = [
        Section::Project,
        Section::Config,
        Section::Runtime,
        Section::Tools,
        Section::Worktrees,
        Section::Services,
        Section::Hooks,
        Section::Adoption,
    ];
}

/// How much a finding matters, and nothing finer.
///
/// Two levels, because the exit code has two values. A `Problem` is
/// something that will break a command, which is what exit 1 means; a
/// `Note` is something a stranger reading this at 2am should know and that
/// breaks nothing on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Note,
    Problem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub section: Section,
    pub severity: Severity,
    /// What is wrong, in one sentence, naming the thing it is about.
    pub message: String,
    /// What to do about it. `None` when there is nothing to do but know.
    pub fix: Option<String>,
}

impl Finding {
    pub(super) fn problem(
        section: Section,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) -> Finding {
        Finding {
            section,
            severity: Severity::Problem,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    pub(super) fn note(section: Section, message: impl Into<String>) -> Finding {
        Finding {
            section,
            severity: Severity::Note,
            message: message.into(),
            fix: None,
        }
    }

    pub(super) fn with_fix(mut self, fix: impl Into<String>) -> Finding {
        self.fix = Some(fix.into());
        self
    }
}

/// The whole report. Serialised as it stands for `--json`, rendered by
/// [`Report::render`] for a terminal — one computation, two shapes, so the
/// two can never disagree.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub project: ProjectReport,
    pub config: ConfigReport,
    pub runtime: RuntimeReport,
    pub tools: Vec<ToolReport>,
    pub worktrees: Vec<WorktreeReport>,
    pub services: ServicesReport,
    pub hooks: Vec<HookReport>,
    /// Project folders under pando's home that look like this repository
    /// from before it moved.
    pub adoption: Vec<Adoptable>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectReport {
    pub id: String,
    pub root: String,
    pub home: String,
    pub worktrees_dir: String,
    /// The home's permission bits in octal, `null` when it does not exist
    /// yet — which is the ordinary state of a project nothing has started.
    pub home_mode: Option<String>,
    pub port_min: u16,
    pub port_max: u16,
    pub base_step: u16,
    /// How many worktree windows the port range holds in total. Machine
    /// wide: another project's worktrees draw from the same range.
    pub bases_in_range: u32,
    /// How many of this project's worktrees hold a window right now.
    pub windows_held: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigReport {
    /// Every layer, lowest precedence first.
    pub layers: Vec<LayerReport>,
    /// Why the merged config does not load, when it does not. The project
    /// layer is pando's own, so this stops `new`, `start`, `restart` and
    /// the TUI.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LayerReport {
    /// `committed`, `user`, or `project`.
    pub layer: &'static str,
    pub path: String,
    pub present: bool,
    /// Every key the file sets, with the provenance comment beside it.
    pub keys: Vec<KeyReport>,
    /// Why the file could not be read at all, when it could not.
    pub error: Option<String>,
}

/// One key of one layer, as the file itself writes it.
#[derive(Debug, Clone, Serialize)]
pub struct KeyReport {
    /// A dotted path, with an index for an array of tables:
    /// `project.install`, `services[0].include`.
    pub key: String,
    /// `null` for a table header, whose own line carries the note for
    /// every key under it.
    pub value: Option<String>,
    /// The same value before it was rendered, for the one question that
    /// cannot be asked of text: whether this is still a value pando's own
    /// rules would write here. Quote style, whitespace and the order an
    /// inline table happens to be written in are formatting, and a
    /// comparison that tripped over them would report every project.
    ///
    /// Not serialised: a reader already has `value`, and a second
    /// spelling of the same thing in the shape agents read is one more
    /// thing that can fall out of step with the first.
    #[serde(skip)]
    pub raw: Option<toml_edit::Value>,
    /// The trailing comment: `# detected: pnpm-lock.yaml`,
    /// `# answered: 2026-09-21`, or whatever a developer wrote there.
    pub note: Option<String>,
    /// Whether pando strips this key from this layer. Only the two keys
    /// that decide where pando writes, and only below the project layer.
    pub ignored: bool,
}

/// What the project asks of a toolchain, what this machine answers, and
/// the line that would reconcile the two.
///
/// The section the original plan predates, and the one that pays for
/// itself: pando spawns everything with `bash -lc`, which is not the
/// developer's interactive shell, so a manager initialised in `.zshrc` is
/// invisible here while everything works when they type it by hand. Naming
/// the absolute path the binary resolved from is what makes that
/// diagnosable at all.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeReport {
    /// `null` when nobody has answered the prelude question, `""` when
    /// somebody answered "this machine needs nothing".
    pub prelude: Option<String>,
    /// The file the prelude came from, highest precedence first.
    pub prelude_from: Option<String>,
    /// One per language the repository pins that pando can probe.
    pub languages: Vec<LanguageReport>,
    /// Every requirement the repository states, probed or not:
    /// `engines.pnpm` is a fact about the project with no language entry
    /// behind it.
    pub requirements: Vec<runtime::Requirement>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LanguageReport {
    pub language: String,
    /// Exactly as the project wrote it: `22`, `>=18 <21`, `lts/*`.
    pub spec: String,
    /// The file it came from.
    pub source: String,
    /// What the version that resolved reported, when one did.
    pub resolved: Option<String>,
    /// **The absolute path it resolved from.** The line that turns "node
    /// 24" into a diagnosis.
    pub resolved_from: Option<String>,
    /// `satisfied`, `mismatch`, or `unknown`.
    pub verdict: &'static str,
    /// Why the probe never got as far as asking, when it did not; or what
    /// the binary answered instead of a version, when it exited non-zero.
    pub failure: Option<String>,
    /// The version managers for this language that this machine has.
    pub managers: Vec<&'static str>,
    /// The prelude lines that would fix a mismatch, one per installed
    /// manager. Printed, never run.
    pub fixes: Vec<String>,
}

/// One executable pando runs, as the shell pando runs it in resolves it.
#[derive(Debug, Clone, Serialize)]
pub struct ToolReport {
    /// `git`, `docker`, `docker compose`, `pnpm`.
    pub name: String,
    /// The absolute path `command -v` printed, `null` when nothing did.
    pub path: Option<String>,
    /// The first line of what it printed when asked its version.
    pub version: Option<String>,
    /// Anything else worth one line about it: which docker context is
    /// active, for instance.
    pub detail: Option<String>,
    /// Why pando looked for it.
    pub needed_for: String,
    pub found: bool,
    /// Whether the shell answered when asked about it. When it did not, a
    /// tool with no path was never looked for rather than not found.
    #[serde(skip)]
    pub asked: bool,
}

/// One worktree, as git sees it and as pando's own records do.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeReport {
    pub name: String,
    pub path: String,
    /// `running`, `starting`, `failed`, or `stopped` when nothing is up.
    pub phase: &'static str,
    /// Whether pando created it, or adopted one that was already there.
    /// `rm` asks before removing an adopted one.
    pub created_by_pando: bool,
    /// The main checkout, which pando runs too and never removes.
    pub main: bool,
    /// Which services it talks to: `shared`, `namespaced` or `isolated`.
    pub mode: crate::state::ServiceMode,
    /// Whether this worktree runs private copies of the project's
    /// services: `mode` is `isolated`. Kept beside `mode`, which came
    /// later, for every program that reads it.
    pub isolated: bool,
    pub locked: bool,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    /// Whether git still lists it. A record for a directory git has
    /// forgotten is state pando is carrying for nothing.
    pub known_to_git: bool,
    pub processes: Vec<ProcessReport>,
    pub services: Vec<WorktreeServiceReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessReport {
    pub name: String,
    pub phase: &'static str,
    /// Why it failed, as the record holds it — which already carries the
    /// classifier's hint when there was one at the time.
    pub reason: Option<String>,
    /// What the tail of its log says now, when the record's reason does
    /// not already say it.
    pub hint: Option<String>,
    pub log: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeServiceReport {
    pub name: String,
    pub port: Option<u16>,
    /// Whether something answers on that port right now.
    pub up: bool,
    /// Whether a log pump is still filling this service's log.
    pub logging: bool,
    /// Whether the config still includes it. A record config has dropped
    /// still owns a container and a volume.
    pub declared: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServicesReport {
    pub compose: Vec<ComposeEntryReport>,
    pub native: Vec<NativeServiceReport>,
    pub isolation: IsolationReport,
}

/// How this project's private services would be run, and why.
///
/// Never a bare verdict: `evidence` is every fact that went into the
/// answer, in the order they were weighed. "pando will use native" with
/// nothing behind it is a thing to argue with; the same line with the
/// compose file, the env example, this machine and the preference under
/// it is a thing to act on.
#[derive(Debug, Clone, Serialize)]
pub struct IsolationReport {
    /// `compose`, `native`, or `null` when the project declares nothing
    /// to isolate.
    pub mechanism: Option<String>,
    /// What this machine asked for, from `[isolation] prefer`.
    pub prefer: Option<String>,
    /// Whether the answer is already recorded, so none of this is a
    /// prediction: `[[services]]` entries, or `[isolation] none`.
    pub answered: bool,
    pub evidence: Vec<String>,
}

/// What a `[[services]] kind = "native"` block resolved to.
///
/// The first question about a native service is always "which recipe is
/// this actually running, and whose?" — a file in the recipes directory
/// replaces a built-in outright, and a `[[services]]` entry can override
/// any field of either. Nothing here is a guess: it is what the start
/// path would resolve, computed the same way.
#[derive(Debug, Clone, Serialize)]
pub struct NativeServiceReport {
    pub name: String,
    /// The recipe the entry asks for: its `preset`, or its own name.
    pub preset: String,
    /// Where the recipe came from: `built-in`, the path of the file that
    /// replaced it, or the entry itself.
    pub source: Option<String>,
    /// Fields the `[[services]]` entry overrode on top of the recipe.
    pub overrides: Vec<String>,
    /// The data directory, with `<worktree>` where the worktree's name
    /// goes. Under pando's home, never in the repository.
    pub datadir: String,
    /// Where the Unix socket directories live. One fixed-length hashed
    /// name per worktree and service, because `sun_path` is 104 bytes.
    pub socket_root: String,
    /// The engine binaries the recipe needs and where each resolved.
    pub engine: Vec<EngineBinary>,
    /// Whether the shell answered when asked where the engine is. When it
    /// did not, a binary with no path was never looked for rather than
    /// not found.
    #[serde(skip)]
    pub engine_asked: bool,
    /// The first line the engine printed when asked for its version.
    pub version: Option<String>,
    /// How to install it. Printed, never run.
    pub install: Option<String>,
    /// The environment key the app reads to find it.
    pub env_key: Option<String>,
    /// Something the recipe says about itself that is worth knowing —
    /// Postgres's trust authentication, for one.
    pub notes: Option<String>,
    /// Whether this recipe has ever been run against a real server.
    pub untested: bool,
    /// The worktrees that already have data for this service.
    pub instances: Vec<NativeInstance>,
    /// Why the recipe could not be resolved, when it could not.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineBinary {
    pub name: String,
    /// Where it resolved on PATH. `None` means this machine has no such
    /// binary, and pando will refuse the start rather than install one.
    pub path: Option<String>,
}

/// One worktree's copy of a native service, as it is on disk.
#[derive(Debug, Clone, Serialize)]
pub struct NativeInstance {
    pub worktree: String,
    pub datadir: String,
    pub socket_dir: String,
    /// `initialised`, `adopted`, or `None` when the data directory has no
    /// marker — which pando would adopt on the next start.
    pub initialised: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComposeEntryReport {
    pub file: String,
    pub file_exists: bool,
    pub services: Vec<IncludedService>,
    /// Services carrying `extends:`, whose real definition is in a file
    /// this reader does not follow.
    pub extends: Vec<String>,
    /// Whether the file has a top-level `include:`, which brings in
    /// services this list does not have.
    pub include: bool,
    /// Why the file could not be read, when it could not.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IncludedService {
    pub name: String,
    /// `healthcheck` when the compose entry declares one, else `connect`
    /// — the weaker probe, which only proves something is behind the port.
    pub ready: &'static str,
    /// Whether the compose file declares it at all.
    pub declared: bool,
    /// The environment key the app reads to find it, when one is mapped.
    pub env_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookReport {
    pub name: String,
    /// `create`, `install`, `services`, or `dev`.
    pub after: &'static str,
    pub cmd: String,
    /// The globs whose content decides whether it has to run again.
    pub fingerprint: Vec<String>,
    /// How many files those globs match in the main checkout. `null` when
    /// the hook is keyed on nothing, which means it runs every start by
    /// design.
    pub matches: Option<usize>,
    /// Where it has run, and whether its inputs have changed since.
    pub runs: Vec<HookRunReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookRunReport {
    pub worktree: String,
    pub ran_at: chrono::DateTime<chrono::Utc>,
    /// Whether the next start will run it again.
    pub will_run_again: bool,
}
