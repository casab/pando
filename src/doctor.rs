//! `doctor`: what pando found, from where, and what is wrong.
//!
//! Read-only. It is the command a developer runs on a machine nobody else
//! can see, so it is written to be read by a stranger: every fact names the
//! file or the path it came from, and every problem says what to do about
//! it. It exits 0 when nothing found will break a command and 1 when
//! something will, and it never fails the shell for a reason it has not
//! printed.
//!
//! **Where this sits.** Above `actions`, not beneath it:
//! `paths → … → actions → doctor → cli · tui`. doctor reports what the rest
//! of pando already knows — the slots the resolver would ask about, the
//! services a record holds, the shell the start path probes — so a module
//! below `actions` would have to keep a second copy of all of it.
//!
//! **What it must never do.** Write. Not a config, not a cache, not a state
//! file, not pando's home. `actions::refresh` is therefore out of bounds
//! here: it takes the lock, advances phases and saves. doctor loads state,
//! advances a *copy* in memory, and reports the difference.

use anyhow::Context as _;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::actions::Machine;
use crate::config::{self, Config, ServiceConfig};
use crate::paths::PandoPaths;
use crate::process as proc;
use crate::runtime::{self, Verdict};
use crate::{actions, detect, native, ports, recipes, services, state, template, tunnel};

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
    fn title(self) -> &'static str {
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
    const ALL: [Section; 8] = [
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
    fn problem(section: Section, message: impl Into<String>, fix: impl Into<String>) -> Finding {
        Finding {
            section,
            severity: Severity::Problem,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    fn note(section: Section, message: impl Into<String>) -> Finding {
        Finding {
            section,
            severity: Severity::Note,
            message: message.into(),
            fix: None,
        }
    }

    fn with_fix(mut self, fix: impl Into<String>) -> Finding {
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
    /// Why the probe never got as far as asking, when it did not.
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
    /// Whether this worktree runs private copies of the project's
    /// services.
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
    /// The first line the engine printed when asked for its version.
    pub version: Option<String>,
    /// How to install it. Printed, never run.
    pub install: Option<String>,
    /// The environment key the app reads to find it.
    pub env_key: Option<String>,
    /// Something the recipe says about itself that is worth knowing —
    /// Postgres's trust authentication, for one.
    pub notes: Option<String>,
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

impl Report {
    /// Whether nothing found will break a command. The exit code, and the
    /// only thing that decides it.
    pub fn healthy(&self) -> bool {
        !self
            .findings
            .iter()
            .any(|f| f.severity == Severity::Problem)
    }

    fn of(&self, section: Section) -> Vec<&Finding> {
        let mut out: Vec<&Finding> = self
            .findings
            .iter()
            .filter(|f| f.section == section)
            .collect();
        // Problems first inside a section: the thing that is broken should
        // not be below three lines about something that merely is.
        out.sort_by(|a, b| b.severity.cmp(&a.severity));
        out
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for section in Section::ALL {
            let _ = writeln!(out, "{}", section.title());
            match section {
                Section::Project => render_project(&mut out, &self.project),
                Section::Config => render_config(&mut out, &self.config),
                Section::Runtime => render_runtime(&mut out, &self.runtime),
                Section::Tools => render_tools(&mut out, &self.tools),
                Section::Worktrees => render_worktrees(&mut out, &self.worktrees),
                Section::Services => render_services(&mut out, &self.services),
                Section::Hooks => render_hooks(&mut out, &self.hooks),
                Section::Adoption => render_adoption(&mut out, &self.adoption),
            }
            for finding in self.of(section) {
                render_finding(&mut out, finding);
            }
            out.push('\n');
        }
        render_summary(&mut out, &self.findings);
        out
    }
}

fn render_finding(out: &mut String, finding: &Finding) {
    let mark = match finding.severity {
        Severity::Problem => '!',
        Severity::Note => '-',
    };
    let _ = writeln!(out, "  {mark} {}", finding.message);
    let Some(fix) = &finding.fix else { return };
    // A fix can be several lines — the prelude candidates for a runtime
    // mismatch are one line each — and each of them is a line a developer
    // may want to copy, so none of them is folded into the one above.
    for (index, line) in fix.lines().enumerate() {
        match index {
            0 => {
                let _ = writeln!(out, "      fix: {line}");
            }
            _ => {
                let _ = writeln!(out, "           {line}");
            }
        }
    }
}

fn render_summary(out: &mut String, findings: &[Finding]) {
    let problems = findings
        .iter()
        .filter(|f| f.severity == Severity::Problem)
        .count();
    let notes = findings.len() - problems;
    if problems == 0 && notes == 0 {
        let _ = writeln!(out, "nothing to report");
        return;
    }
    let _ = writeln!(
        out,
        "{problems} {}, {notes} {}",
        plural(problems, "problem"),
        plural(notes, "note")
    );
}

fn plural(n: usize, word: &str) -> String {
    match n {
        1 => word.to_string(),
        _ => format!("{word}s"),
    }
}

/// A fact row: a label, padded, and its value.
///
/// A label longer than the column — a project id is — still gets two
/// spaces after it rather than running into what it is labelling.
fn row(out: &mut String, label: &str, value: &str) {
    const COLUMN: usize = 14;
    match label.chars().count() < COLUMN {
        true => {
            let _ = writeln!(out, "  {label:<COLUMN$}{value}");
        }
        false => {
            let _ = writeln!(out, "  {label}  {value}");
        }
    }
}

fn render_project(out: &mut String, project: &ProjectReport) {
    row(out, "id", &project.id);
    row(out, "root", &project.root);
    row(out, "home", &project.home);
    row(out, "worktrees", &project.worktrees_dir);
    row(
        out,
        "home mode",
        &match &project.home_mode {
            Some(mode) => mode.clone(),
            None => "not created yet".to_string(),
        },
    );
    row(
        out,
        "ports",
        &format!(
            "{}-{} in windows of {}; {} bases on this machine, {} held here",
            project.port_min,
            project.port_max,
            project.base_step,
            project.bases_in_range,
            project.windows_held
        ),
    );
}

/// How wide a key column is allowed to get before the notes stop lining up
/// and start pushing the line over a terminal's edge.
const KEY_COLUMN_MAX: usize = 56;

fn render_config(out: &mut String, config: &ConfigReport) {
    for layer in &config.layers {
        let suffix = match (&layer.error, layer.present) {
            (Some(e), _) => format!(" — {e}"),
            (None, false) => " — not there".to_string(),
            (None, true) if layer.keys.is_empty() => " — empty".to_string(),
            (None, true) => String::new(),
        };
        let _ = writeln!(out, "  {:<10}{}{suffix}", layer.layer, layer.path);
        let width = layer
            .keys
            .iter()
            .map(|k| lhs(k).chars().count())
            .max()
            .unwrap_or(0)
            .min(KEY_COLUMN_MAX);
        for key in &layer.keys {
            let text = lhs(key);
            match &key.note {
                Some(note) => {
                    let _ = writeln!(out, "      {text:<width$}  {note}");
                }
                None => {
                    let _ = writeln!(out, "      {text}");
                }
            }
        }
    }
}

fn render_runtime(out: &mut String, runtime: &RuntimeReport) {
    row(
        out,
        "prelude",
        &match (&runtime.prelude, &runtime.prelude_from) {
            (None, _) => "nobody has answered that question yet".to_string(),
            (Some(line), _) if line.trim().is_empty() => {
                "\"\" — answered: this machine needs nothing in front of a command".to_string()
            }
            (Some(line), Some(from)) => format!("{line}  (from {from})"),
            (Some(line), None) => line.clone(),
        },
    );
    if runtime.languages.is_empty() && runtime.requirements.is_empty() {
        row(out, "pinned", "nothing — this repository states no runtime");
        return;
    }
    for language in &runtime.languages {
        row(
            out,
            &language.language,
            &format!("wants {} ({})", language.spec, language.source),
        );
        let has = match (&language.resolved, &language.resolved_from) {
            (Some(version), Some(path)) => {
                format!("`bash -lc` resolves {version}, from {path}")
            }
            (Some(version), None) => format!("`bash -lc` resolves {version}"),
            _ => match &language.failure {
                Some(failure) => format!("the probe never ran: {failure}"),
                None => format!("`bash -lc` here has no {} at all", language.language),
            },
        };
        row(out, "", &has);
        row(
            out,
            "",
            &match language.managers.as_slice() {
                [] => "no version manager pando knows about is installed for it".to_string(),
                managers => format!("version managers here: {}", managers.join(", ")),
            },
        );
    }
    // A requirement with no language entry behind it is still a fact the
    // repository states, and an agent reading this should see it.
    for requirement in &runtime.requirements {
        if runtime
            .languages
            .iter()
            .any(|l| l.language == requirement.language)
        {
            continue;
        }
        row(
            out,
            &requirement.language,
            &format!(
                "wants {} ({}) — nothing here probes it",
                requirement.spec, requirement.source
            ),
        );
    }
}

fn render_tools(out: &mut String, tools: &[ToolReport]) {
    if tools.is_empty() {
        row(out, "", "nothing to look for");
        return;
    }
    // Two spaces past the longest name, so `docker compose` does not run
    // into its own version number.
    let width = tools
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(12)
        + 2;
    for tool in tools {
        let mut text = match (&tool.version, &tool.path) {
            // `command -v` prints a bare word for a shell builtin, which
            // is not a path and is not a version to ask for either.
            (_, Some(path)) if !path.starts_with('/') => format!("a shell builtin ({path})"),
            (Some(version), Some(path)) => format!("{version}  ({path})"),
            (None, Some(path)) => format!("found, and said nothing  ({path})"),
            _ => format!("not found — {}", tool.needed_for),
        };
        if let Some(detail) = &tool.detail {
            text.push_str(&format!("  {detail}"));
        }
        let _ = writeln!(out, "  {:<width$}{text}", tool.name);
    }
}

fn render_worktrees(out: &mut String, worktrees: &[WorktreeReport]) {
    if worktrees.is_empty() {
        row(out, "", "none — `pando new <branch>` makes one");
        return;
    }
    for worktree in worktrees {
        let mut flags: Vec<&str> = vec![match worktree.created_by_pando {
            true => "pando-created",
            false => "adopted",
        }];
        if worktree.isolated {
            flags.push("isolated");
        }
        if worktree.locked {
            flags.push("locked");
        }
        if worktree.prunable {
            flags.push("prunable");
        }
        if !worktree.known_to_git {
            flags.push("git does not list it");
        }
        row(
            out,
            &worktree.name,
            &format!("{}  [{}]", worktree.phase, flags.join(", ")),
        );
        row(out, "", &worktree.path);
        for process in &worktree.processes {
            row(
                out,
                "",
                &match &process.reason {
                    Some(reason) => format!("{}: {} — {reason}", process.name, process.phase),
                    None => format!("{}: {}", process.name, process.phase),
                },
            );
        }
        for service in &worktree.services {
            let port = match service.port {
                Some(port) => port.to_string(),
                None => "no port".to_string(),
            };
            let up = match service.up {
                true => "answering",
                false => "not answering",
            };
            let pump = match service.logging {
                true => "",
                false => ", no log pump",
            };
            let dropped = match service.declared {
                true => "",
                false => ", config no longer includes it",
            };
            row(
                out,
                "",
                &format!("{}: {port}, {up}{pump}{dropped}", service.name),
            );
        }
    }
}

fn render_services(out: &mut String, services: &ServicesReport) {
    if services.compose.is_empty() && services.native.is_empty() {
        row(out, "", "none configured");
        return;
    }
    for entry in &services.compose {
        row(
            out,
            "compose",
            &match (&entry.error, entry.file_exists) {
                (Some(e), _) => format!("{} — {e}", entry.file),
                (None, false) => format!("{} — not in this repository", entry.file),
                (None, true) => entry.file.clone(),
            },
        );
        for service in &entry.services {
            let env = match &service.env_key {
                Some(key) => format!(", addressed by {key}"),
                None => ", nothing in the env example points at it".to_string(),
            };
            row(
                out,
                "",
                &match service.declared {
                    true => format!("{}: ready by {}{env}", service.name, service.ready),
                    false => format!("{}: this compose file does not declare it", service.name),
                },
            );
        }
        if !entry.extends.is_empty() {
            row(
                out,
                "",
                &format!("`extends:` on {} — not followed", entry.extends.join(", ")),
            );
        }
        if entry.include {
            row(out, "", "a top-level `include:` — not followed");
        }
    }
    for native in &services.native {
        row(
            out,
            "native",
            &match (&native.error, &native.source) {
                (Some(e), _) => format!("{}: {e}", native.name),
                (None, Some(source)) => format!(
                    "{}: the {:?} recipe, {source}{}",
                    native.name,
                    native.preset,
                    match native.overrides.is_empty() {
                        true => String::new(),
                        false => format!(" ({} from this entry)", native.overrides.join(", ")),
                    }
                ),
                (None, None) => native.name.clone(),
            },
        );
        if native.error.is_some() {
            continue;
        }
        for binary in &native.engine {
            row(
                out,
                "",
                &match &binary.path {
                    Some(path) => format!("{} at {path}", binary.name),
                    None => format!("{} is not on PATH", binary.name),
                },
            );
        }
        if let Some(version) = &native.version {
            row(out, "", version);
        }
        if let Some(install) = &native.install
            && native.engine.iter().any(|b| b.path.is_none())
        {
            row(
                out,
                "",
                &format!("install it with: {install} — pando never will"),
            );
        }
        row(out, "", &format!("data in {}", native.datadir));
        row(out, "", &format!("sockets under {}", native.socket_root));
        match &native.env_key {
            Some(key) => row(out, "", &format!("addressed by {key}")),
            None => row(out, "", "nothing in the env points at it"),
        }
        if let Some(notes) = &native.notes {
            row(out, "", notes);
        }
        for instance in &native.instances {
            row(
                out,
                "",
                &format!(
                    "{}: {}",
                    instance.worktree,
                    match &instance.initialised {
                        Some(how) => format!("{how}, socket in {}", instance.socket_dir),
                        None =>
                            "a data directory with no marker — the next start adopts it as it is"
                                .to_string(),
                    }
                ),
            );
        }
    }
}

fn render_hooks(out: &mut String, hooks: &[HookReport]) {
    if hooks.is_empty() {
        row(out, "", "none configured");
        return;
    }
    for hook in hooks {
        row(
            out,
            &hook.name,
            &format!("after {}: {}", hook.after, hook.cmd),
        );
        row(
            out,
            "",
            &match (&hook.matches, hook.fingerprint.is_empty()) {
                (_, true) => "keyed on nothing, so it runs on every start".to_string(),
                (Some(0) | None, false) => format!(
                    "keyed on {}, which matches nothing here",
                    hook.fingerprint.join(", ")
                ),
                (Some(n), false) => format!(
                    "keyed on {}, matching {n} {}",
                    hook.fingerprint.join(", "),
                    plural(*n, "file")
                ),
            },
        );
        for run in &hook.runs {
            row(
                out,
                "",
                &format!(
                    "{}: ran {}{}",
                    run.worktree,
                    run.ran_at.format("%Y-%m-%d %H:%M"),
                    match run.will_run_again {
                        true => ", and will run again — its inputs changed",
                        false => "",
                    }
                ),
            );
        }
    }
}

fn render_adoption(out: &mut String, adoption: &[Adoptable]) {
    if adoption.is_empty() {
        row(out, "", "nothing to adopt");
        return;
    }
    for entry in adoption {
        row(out, &entry.id, &entry.path);
        row(
            out,
            "",
            &match &entry.old_root {
                Some(root) => format!("its repository was at {root}, and is not there now"),
                None => "nothing in it says which repository it belonged to".to_string(),
            },
        );
        if !entry.worktrees.is_empty() {
            row(
                out,
                "",
                &format!(
                    "{} {} still in it: {}",
                    entry.worktrees.len(),
                    plural(entry.worktrees.len(), "worktree"),
                    entry.worktrees.join(", ")
                ),
            );
        }
    }
}

/// A key row's left-hand side: `key = value`, or a bare table header.
fn lhs(key: &KeyReport) -> String {
    let ignored = if key.ignored { "  (ignored)" } else { "" };
    match &key.value {
        Some(value) => format!("{} = {value}{ignored}", key.key),
        None => format!("{}{ignored}", key.key),
    }
}

// ---- building the report --------------------------------------------------

/// Everything doctor has to say about this project, gathered without
/// writing anything anywhere.
///
/// The shell is the one a real spawn uses — `bash -lc`, in the main
/// checkout — built here the way `actions::resolve_silencing` builds it,
/// so what doctor reports about this machine is what the start path would
/// have found.
pub fn run(paths: &PandoPaths) -> Report {
    let shell = actions::runtime_shell(paths.root());
    let machine = Machine {
        shell: &shell,
        home: actions::user_home(),
    };
    run_on(paths, &machine)
}

/// [`run`] with the machine injected, so a test can report on a laptop it
/// does not have.
pub fn run_on(paths: &PandoPaths, machine: &Machine<'_>) -> Report {
    let mut findings: Vec<Finding> = Vec::new();

    // Its own load, not the one `main` did: `main` hands every command the
    // merged config and throws away the error, and the error is the fact
    // doctor exists to report. `Command::needs_config` is false for
    // `Doctor` for the same reason — a project layer pando cannot read is
    // exactly when this command is worth running.
    let (config, error, warnings) = match config::load(paths) {
        Ok(loaded) => (loaded.config, None, loaded.warnings),
        Err(e) => {
            let fallback = config::load_without_home(paths);
            (fallback.config, Some(format!("{e:#}")), fallback.warnings)
        }
    };

    let config_report = config_report(paths, error, warnings, &mut findings);
    validate_config(paths, &config, &mut findings);
    let project = project_report(paths, &config, &mut findings);
    let runtime = runtime_report(paths, &config, machine, &mut findings);
    let tools = tools_report(paths, &config, machine, &mut findings);
    // Once, and shared: every group's listening sockets are scanned to
    // advance a phase, and that is a real cost to pay twice.
    let view = actions::inspect(paths);
    let worktrees = worktrees_report(paths, &config, &view, &mut findings);
    let services = services_report(paths, &config, machine, &mut findings);
    let hooks = hooks_report(paths, &config, &view, &worktrees, &mut findings);
    let adoption = adoption_report(paths, &mut findings);

    Report {
        project,
        config: config_report,
        runtime,
        tools,
        worktrees,
        services,
        hooks,
        adoption,
        findings,
    }
}

fn adoption_report(paths: &PandoPaths, findings: &mut Vec<Finding>) -> Vec<Adoptable> {
    let found = adoptable(paths);
    for entry in &found {
        findings.push(
            Finding::note(
                Section::Adoption,
                format!(
                    "{} holds a project folder for a repository called {:?} that is no longer \
                     where it was{} — the id is a hash of the path, so this repository moving \
                     gave it a new one and left that folder behind",
                    entry.id,
                    paths.project.display_name,
                    match &entry.old_root {
                        Some(root) => format!(" ({root})"),
                        None => String::new(),
                    }
                ),
            )
            .with_fix(format!(
                "`pando doctor --adopt {}` moves it under this repository's id, with its \
                 config, its state and its worktrees",
                entry.id
            )),
        );
    }
    found
}

// ---- worktrees ------------------------------------------------------------

/// How many log lines the failure classifier reads. The same budget the
/// read path uses.
const FAILURE_TAIL_LINES: usize = 40;

fn worktrees_report(
    paths: &PandoPaths,
    config: &Config,
    view: &actions::Refreshed,
    findings: &mut Vec<Finding>,
) -> Vec<WorktreeReport> {
    if let Some(warning) = &view.warning {
        findings.push(Finding::problem(
            Section::Worktrees,
            format!("pando's state file cannot be used: {warning}"),
            "move it aside to start over — worktrees pando created will then read as adopted",
        ));
    }
    let listed = crate::worktree::discover(&paths.project).unwrap_or_default();
    let declared = declared_services(config);

    let mut out = Vec::new();
    for (name, record) in &view.state.worktrees {
        let git = listed.iter().find(|w| &w.name == name);
        let phase = state::aggregate_phase(record);
        let mut processes = Vec::new();
        for (process, p) in &record.processes {
            let reason = match &p.phase {
                state::Phase::Failed { reason, .. } => Some(reason.clone()),
                _ => None,
            };
            let hint = failure_hint(&p.log_path, reason.as_deref());
            if let Some(reason) = &reason {
                let tail = format!("`pando logs {name} --source {process}` has the last of it");
                findings.push(Finding::problem(
                    Section::Worktrees,
                    format!("{name}: the process {process:?} failed — {reason}"),
                    match &hint {
                        Some(hint) => format!("{hint}\n{tail}"),
                        None => format!("{tail}; `pando start {name}` tries again"),
                    },
                ));
            }
            processes.push(ProcessReport {
                name: process.clone(),
                phase: match p.phase {
                    state::Phase::Starting { .. } => "starting",
                    state::Phase::Running { .. } => "running",
                    state::Phase::Failed { .. } => "failed",
                },
                reason,
                hint,
                log: p.log_path.display().to_string(),
            });
        }

        let mut services = Vec::new();
        for status in actions::service_statuses(record) {
            let is_declared = declared.contains(&status.name);
            if status.up && !status.logging {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: the service {:?} is up and nothing is filling its log — \
                             its log pump died",
                            status.name
                        ),
                    )
                    .with_fix(format!(
                        "`pando start {name}` puts it back; a read path never respawns one"
                    )),
                );
            }
            if !is_declared {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: pando still has a record for the service {:?}, which this \
                             project's config no longer includes — its container and its volume \
                             are still there",
                            status.name
                        ),
                    )
                    .with_fix(format!(
                        "`pando rm {name}` takes them down with the worktree, or `pando start \
                         {name} --shared` stops them and leaves the data"
                    )),
                );
            }
            services.push(WorktreeServiceReport {
                name: status.name,
                port: status.port,
                up: status.up,
                logging: status.logging,
                declared: is_declared,
            });
        }

        match git {
            None => findings.push(
                Finding::note(
                    Section::Worktrees,
                    format!(
                        "{name}: pando has a record for it and git does not list it — the \
                         directory is gone, or `git worktree prune` has been run"
                    ),
                )
                .with_fix(format!("`pando rm {name}` forgets it")),
            ),
            Some(git) => {
                if git.prunable {
                    findings.push(
                        Finding::note(
                            Section::Worktrees,
                            format!(
                                "{name}: git calls this worktree prunable{}",
                                match &git.prunable_reason {
                                    Some(reason) => format!(" — {reason}"),
                                    None => String::new(),
                                }
                            ),
                        )
                        .with_fix(format!("`pando rm {name}`, or `git worktree prune`")),
                    );
                }
                if git.locked {
                    findings.push(
                        Finding::note(
                            Section::Worktrees,
                            format!(
                                "{name}: git has this worktree locked, so `pando rm` refuses it"
                            ),
                        )
                        .with_fix(format!("`git worktree unlock {}`", git.path.display())),
                    );
                }
            }
        }
        if let Some(share) = &record.share {
            let tunnel = proc::is_alive(share.tunnel_pid);
            let proxy = share.proxy_pid.map(proc::is_alive).unwrap_or(true);
            if !tunnel || !proxy {
                findings.push(
                    Finding::note(
                        Section::Worktrees,
                        format!(
                            "{name}: half of its share is gone — {} is not running, and {} is \
                             still published",
                            if tunnel { "the proxy" } else { "the tunnel" },
                            share.public_url
                        ),
                    )
                    .with_fix(format!("`pando unshare {name}` takes the rest of it down")),
                );
            }
        }

        out.push(WorktreeReport {
            name: name.clone(),
            path: record.path.display().to_string(),
            phase: phase
                .as_ref()
                .map(state::Aggregate::word)
                .unwrap_or("stopped"),
            created_by_pando: record.created_by_pando,
            isolated: record.isolated,
            locked: git.is_some_and(|g| g.locked),
            prunable: git.is_some_and(|g| g.prunable),
            prunable_reason: git.and_then(|g| g.prunable_reason.clone()),
            known_to_git: git.is_some(),
            processes,
            services,
        });
    }
    // A worktree git lists that pando has no record of is not reported
    // here: `pando ls` shows those, and this section is about what pando
    // is carrying.
    out
}

/// What the tail of a failed process's log says now, when the record's own
/// reason does not already say it.
///
/// The reason usually carries the hint already — the read path writes it in
/// when the failure is first seen. A record whose log only became
/// explanatory afterwards has nothing, and that is the case worth reading
/// the file for.
fn failure_hint(log: &Path, reason: Option<&str>) -> Option<String> {
    let reason = reason?;
    let lines = crate::log_tail::snapshot(log, FAILURE_TAIL_LINES).unwrap_or_default();
    let hint = crate::observe::classify_failure(&lines)?;
    (!reason.contains(&hint.hint)).then_some(hint.hint)
}

/// Every service name the config declares, whatever kind it is.
fn declared_services(config: &Config) -> Vec<String> {
    config
        .services
        .iter()
        .flat_map(|service| match service {
            ServiceConfig::Compose { include, .. } => include.clone(),
            ServiceConfig::Native { name, .. } => vec![name.clone()],
        })
        .collect()
}

// ---- services -------------------------------------------------------------

fn services_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> ServicesReport {
    let mut compose = Vec::new();
    let native = native_report(paths, config, machine, findings);
    for service in &config.services {
        let ServiceConfig::Compose {
            file, include, env, ..
        } = service
        else {
            continue;
        };
        let path = paths.root().join(file);
        let parsed = path.is_file().then(|| crate::compose::read(&path));
        if parsed.is_none() {
            findings.push(Finding::problem(
                Section::Services,
                format!(
                    "the compose file {file:?} is not in this repository — `start --isolated` \
                     has nothing to bring up"
                ),
                "point `[[services]] file` at a file that is there, or drop the entry",
            ));
        }
        let read = match &parsed {
            Some(Ok(read)) => Some(read),
            Some(Err(e)) => {
                findings.push(Finding::problem(
                    Section::Services,
                    format!("the compose file {file:?} could not be read: {e:#}"),
                    "fix the file, or drop the `[[services]]` entry that names it",
                ));
                None
            }
            None => None,
        };
        let mut services = Vec::new();
        for name in include {
            let declared = read.is_some_and(|r| r.services.contains_key(name));
            let healthcheck = read
                .and_then(|r| r.services.get(name))
                .is_some_and(|s| s.healthcheck);
            if read.is_some() && !declared {
                findings.push(Finding::problem(
                    Section::Services,
                    format!(
                        "`include` names the service {name:?}, which {file} does not declare — \
                         `start --isolated` would fail"
                    ),
                    format!("drop {name:?} from `include`, or add it to {file}"),
                ));
            }
            if declared && !healthcheck {
                findings.push(
                    Finding::note(
                        Section::Services,
                        format!(
                            "the service {name:?} declares no healthcheck, so readiness is a \
                             connect that only proves something is behind the port — a database \
                             still initialising can pass it"
                        ),
                    )
                    .with_fix(format!("add a `healthcheck:` to {name} in {file}")),
                );
            }
            services.push(IncludedService {
                name: name.clone(),
                ready: if healthcheck {
                    "healthcheck"
                } else {
                    "connect"
                },
                declared,
                env_key: env
                    .iter()
                    .find(|(_, service)| *service == name)
                    .map(|(key, _)| key.clone()),
            });
        }
        let unresolved = read.map(|r| r.unresolved.clone()).unwrap_or_default();
        if let Some(described) = unresolved.describe() {
            findings.push(
                Finding::note(
                    Section::Services,
                    format!(
                        "{file} carries {described}, which pando's own reader does not follow — \
                         the ports and volumes it read may not be the ones compose would use"
                    ),
                )
                .with_fix(
                    "with Docker installed pando asks `docker compose config`, which resolves \
                     them; without it, inline what the key brings in",
                ),
            );
        }
        compose.push(ComposeEntryReport {
            file: file.clone(),
            file_exists: parsed.is_some(),
            services,
            extends: unresolved.extends.clone(),
            include: unresolved.include,
            error: match &parsed {
                Some(Err(e)) => Some(format!("{e:#}")),
                _ => None,
            },
        });
    }
    name_collisions(paths, config, findings);
    ServicesReport { compose, native }
}

/// What each `[[services]] kind = "native"` block resolved to, and
/// whether this machine can run it.
///
/// A missing engine is a line, not a crash: doctor reports, and pando
/// never installs one. The same goes for a recipe file that does not
/// parse — it is reported once, as a problem, and the recipes it shadows
/// are named.
fn native_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> Vec<NativeServiceReport> {
    let entries = native::Entry::all(config);
    if entries.is_empty() {
        return Vec::new();
    }
    let recipes = recipes::Recipes::load(&paths.recipes_dir());
    for (name, broken) in recipes.broken() {
        findings.push(Finding::problem(
            Section::Services,
            format!(
                "the recipe file {} does not load: {}",
                broken.path.display(),
                broken.error
            ),
            format!(
                "fix it, or delete it — until then nothing can run the recipe {name:?}, not \
                 even a built-in of that name, because a file that shadows one must not fail \
                 quietly back to it"
            ),
        ));
    }
    let bin_dir = paths.home.join("bin");
    let mut out = Vec::new();
    for entry in &entries {
        let datadir = paths.service_data_dir(WORKTREE_PLACEHOLDER, entry.name);
        let socket_root = paths
            .service_socket_dir(WORKTREE_PLACEHOLDER, entry.name)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let mut report = NativeServiceReport {
            name: entry.name.to_string(),
            preset: entry.preset().to_string(),
            source: None,
            overrides: Vec::new(),
            datadir: datadir.display().to_string(),
            socket_root,
            engine: Vec::new(),
            version: None,
            install: None,
            env_key: None,
            notes: None,
            instances: native_instances(paths, entry.name),
            error: None,
        };
        let resolved = match native::resolve(&recipes, entry) {
            Ok(resolved) => resolved,
            Err(e) => {
                report.error = Some(format!("{e:#}"));
                findings.push(Finding::problem(
                    Section::Services,
                    format!("{e:#}"),
                    format!(
                        "name a `preset` that exists, give the entry its own `cmd`, or write \
                         {}.toml in {}",
                        entry.preset(),
                        paths.recipes_dir().display()
                    ),
                ));
                out.push(report);
                continue;
            }
        };
        report.source = Some(resolved.source.describe());
        report.overrides = resolved.overrides.iter().map(|o| o.to_string()).collect();
        report.install = resolved.recipe.install.clone();
        report.notes = resolved.recipe.notes.clone();
        let (mapping, _) = entry.env_map(Some(&resolved.recipe));
        report.env_key = mapping.keys().next().cloned();
        let (engine, version) = probe_engine(machine, &bin_dir, &resolved.recipe);
        let missing: Vec<&str> = engine
            .iter()
            .filter(|b| b.path.is_none())
            .map(|b| b.name.as_str())
            .collect();
        if !missing.is_empty() {
            findings.push(Finding::problem(
                Section::Services,
                format!(
                    "the service {:?} runs the {:?} recipe, and {} not on PATH — \
                     `start --isolated` would refuse rather than start it",
                    entry.name,
                    resolved.recipe.name,
                    match missing.len() {
                        1 => format!("{} is", missing[0]),
                        _ => format!("{} are", missing.join(", ")),
                    }
                ),
                match &resolved.recipe.install {
                    Some(hint) => format!("install it: {hint} — pando never will"),
                    None => format!(
                        "install it, or put a shim in {} — pando never installs an engine",
                        bin_dir.display()
                    ),
                },
            ));
        }
        report.engine = engine;
        report.version = version;
        out.push(report);
    }
    out
}

/// Stands where a worktree's name goes, so the data directory can be
/// reported for a project rather than for one worktree.
const WORKTREE_PLACEHOLDER: &str = "<worktree>";

/// Marks the engine probe's two answers, the way the tools probe marks
/// its three.
const NATIVE_BIN_MARK: &str = "pando-native-bin ";
const NATIVE_VERSION_MARK: &str = "pando-native-version ";

/// Where each of a recipe's binaries resolved, and what the first of them
/// says its version is.
///
/// Asked through the same injected shell the tools probe uses, and with
/// the same PATH the start path would give the recipe — pando's own `bin`
/// first, so a shim a developer put there is what doctor reports.
fn probe_engine(
    machine: &Machine<'_>,
    bin_dir: &Path,
    recipe: &recipes::Recipe,
) -> (Vec<EngineBinary>, Option<String>) {
    let binaries = recipe.binaries.clone();
    if binaries.is_empty() {
        return (Vec::new(), None);
    }
    let mut script = format!(
        "export PATH={}:\"$PATH\"\n",
        proc::shell_quote(&bin_dir.display().to_string())
    );
    for binary in &binaries {
        let quoted = proc::shell_quote(binary);
        let _ = writeln!(
            script,
            "if __pando_b=$(command -v {quoted} 2>/dev/null); then \
             printf '{NATIVE_BIN_MARK}%s %s\\n' {quoted} \"$__pando_b\"; fi"
        );
    }
    if let Some(version) = recipe.version_cmd() {
        let _ = writeln!(
            script,
            "printf '{NATIVE_VERSION_MARK}%s\\n' \"$({version} 2>&1 | head -n 1)\""
        );
    }
    let Some(text) = (machine.shell)(&script) else {
        // The shell did not answer. That is not evidence about the
        // engine, so nothing is claimed about it.
        return (
            binaries
                .iter()
                .map(|name| EngineBinary {
                    name: name.clone(),
                    path: None,
                })
                .collect(),
            None,
        );
    };
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(NATIVE_BIN_MARK)
            && let Some((name, path)) = rest.split_once(' ')
        {
            found.insert(name.to_string(), path.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix(NATIVE_VERSION_MARK)
            && !rest.trim().is_empty()
        {
            version = Some(rest.trim().to_string());
        }
    }
    let engine = binaries
        .iter()
        .map(|name| EngineBinary {
            name: name.clone(),
            path: found.get(name).cloned(),
        })
        .collect();
    // A version printed by a binary that is not there is the shell
    // reporting its own "command not found", not an engine.
    let version = version.filter(|_| found.contains_key(&binaries[0]));
    (engine, version)
}

/// The worktrees that already have data for this service, and what their
/// data directories say about themselves.
fn native_instances(paths: &PandoPaths, service: &str) -> Vec<NativeInstance> {
    let Ok(entries) = std::fs::read_dir(paths.project_dir().join("data")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter(|worktree| paths.service_data_dir(worktree, service).is_dir())
        .map(|worktree| {
            let datadir = paths.service_data_dir(&worktree, service);
            let marker = native::marker(&datadir);
            NativeInstance {
                initialised: marker.map(|marker| {
                    format!(
                        "{} {}",
                        match marker.adopted {
                            true => "adopted",
                            false => "initialised",
                        },
                        marker.at.format("%Y-%m-%d")
                    )
                }),
                socket_dir: paths
                    .service_socket_dir(&worktree, service)
                    .display()
                    .to_string(),
                datadir: datadir.display().to_string(),
                worktree,
            }
        })
        .collect()
}

/// A compose service whose name is already a role of one of this project's
/// processes.
///
/// A role is one port and belongs to one thing, so the two cannot both
/// have it: `validate_services` refuses the pair, and a config that holds
/// it does not load. pando now refuses to *write* that pair — the answer is
/// checked against the loader before a key reaches disk — but the two
/// halves are still sitting in the repository waiting to be offered, and a
/// developer who answers the services question is told "no" without ever
/// having been told why. This is the why, at rest, before the question.
///
/// Only for services that have not been answered for yet: once one is in
/// `include` the config does not load at all, and that is the Config
/// section's headline problem rather than a second copy of it here.
fn name_collisions(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    for (process, spec) in &config.processes {
        for role in spec.roles() {
            owner.insert(role, format!("the process {process:?}"));
        }
    }
    if owner.is_empty() {
        return;
    }
    let answered = declared_services(config);
    let signals = detect::signals(paths.root());
    for file in &signals.compose_files {
        let Ok(read) = crate::compose::read(&paths.root().join(file)) else {
            continue;
        };
        for name in read.services.keys() {
            if answered.contains(name) {
                continue;
            }
            let Some(owner) = owner.get(name) else {
                continue;
            };
            findings.push(
                Finding::note(
                    Section::Services,
                    format!(
                        "{file} declares a service called {name:?}, and {owner} already \
                         owns the role {name:?} — a role is one port and belongs to one \
                         thing, so pando will refuse to run a private copy of it under \
                         that name"
                    ),
                )
                .with_fix(format!(
                    "rename the role in that process's `ports`, or rename the service in \
                     {file} — answering the services question with {name:?} is refused \
                     until one of them moves"
                )),
            );
        }
    }
}

// ---- hooks ----------------------------------------------------------------

fn hooks_report(
    paths: &PandoPaths,
    config: &Config,
    view: &actions::Refreshed,
    worktrees: &[WorktreeReport],
    findings: &mut Vec<Finding>,
) -> Vec<HookReport> {
    schema_slot_finding(paths, config, findings);
    let mut out = Vec::new();
    for hook in &config.hooks {
        let matches = (!hook.fingerprint.is_empty())
            .then(|| crate::hooks::matched(paths.root(), &hook.fingerprint).len());
        if matches == Some(0) {
            findings.push(
                Finding::note(
                    Section::Hooks,
                    // The sentence a start prints while it happens, so the
                    // two cannot drift apart.
                    actions::matched_nothing(paths.root(), hook)
                        .trim_start_matches("warning: ")
                        .to_string(),
                )
                .with_fix("key it on the files it really depends on, or leave `fingerprint` out"),
            );
        }
        let mut runs = Vec::new();
        for worktree in worktrees {
            let Some(record) = view.state.worktrees.get(&worktree.name) else {
                continue;
            };
            let Some(run) = record.hooks.get(&hook.name) else {
                continue;
            };
            let current = crate::hooks::fingerprint(&record.path, &hook.fingerprint, &hook.cmd);
            runs.push(HookRunReport {
                worktree: worktree.name.clone(),
                ran_at: run.ran_at,
                will_run_again: current.is_none() || current != run.fingerprint,
            });
        }
        out.push(HookReport {
            name: hook.name.clone(),
            after: match hook.after {
                config::HookPoint::Create => "create",
                config::HookPoint::Install => "install",
                config::HookPoint::Services => "services",
                config::HookPoint::Dev => "dev",
            },
            cmd: hook.cmd.clone(),
            fingerprint: hook.fingerprint.clone(),
            matches,
            runs,
        });
    }
    out
}

/// The schema question has no way to record "this project has no schema
/// step", so an undecided one comes back on every start.
///
/// Reported, not fixed: giving the slot an empty form is the same piece of
/// design the services and provision slots each had done for them, and it
/// belongs to whoever owns that question rather than to the command that
/// noticed it.
fn schema_slot_finding(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    if actions::already_answered(detect::Slot::SchemaHook, config) {
        return;
    }
    let signals = detect::signals(paths.root());
    let undecided = detect::propose(paths.root(), &signals)
        .into_iter()
        .any(|p| p.slot == detect::Slot::SchemaHook && !p.decided && !p.candidates.is_empty());
    if !undecided {
        return;
    }
    findings.push(
        Finding::note(
            Section::Hooks,
            "pando has a question about the schema step that its rules cannot settle, and no \
             way to record \"this project has none\" — so it comes back on every start until a \
             hook is written down"
                .to_string(),
        )
        .with_fix(
            "answer it once with `pando init`, or write a `[[hooks]]` entry by hand — one whose \
             `cmd` is `true` is the shape that means \"nothing to do\"",
        ),
    );
}

// ---- runtime --------------------------------------------------------------

fn runtime_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> RuntimeReport {
    let prelude = config.runtime.prelude.clone();
    let effective = prelude.clone().unwrap_or_default();
    let effective = effective.trim();
    let prelude_from = config::prelude_origin(paths).map(|p| p.display().to_string());
    let requirements = runtime::requirements(paths.root());

    let mut languages = Vec::new();
    for entry in &runtime::LANGUAGES {
        let Some(requirement) = runtime::for_language(&requirements, entry.name) else {
            continue;
        };
        // `runtime::check`, never `actions`' own first-mismatch walk: that
        // one remembers what passed, and doctor writes nothing. It also
        // stops at the first language, and a report that named one of two
        // problems would be the kind of report this command exists to
        // replace.
        let check = runtime::check(requirement, effective, machine.shell);
        let managers: Vec<&'static str> = runtime::installed(entry, &machine.home)
            .into_iter()
            .map(|manager| manager.name)
            .collect();
        let fixes: Vec<String> = runtime::fixes(
            entry,
            &machine.home,
            runtime::from_version_file(entry, requirement),
        )
        .into_iter()
        .map(|fix| format!("{}  ({})", fix.line, fix.why))
        .collect();
        let report = LanguageReport {
            language: requirement.language.clone(),
            spec: requirement.spec.clone(),
            source: requirement.source.clone(),
            resolved: check.resolved.version.clone(),
            resolved_from: check.resolved.path.clone(),
            verdict: match check.verdict {
                Verdict::Satisfied => "satisfied",
                Verdict::Mismatch => "mismatch",
                Verdict::Unknown => "unknown",
            },
            failure: match check.resolved.ran {
                true => None,
                false => Some(
                    check
                        .resolved
                        .failure
                        .clone()
                        .unwrap_or_else(|| "no output".to_string()),
                ),
            },
            managers,
            fixes,
        };
        if check.verdict == Verdict::Mismatch {
            findings.push(mismatch_finding(&report, prelude.as_deref(), &prelude_from));
        }
        languages.push(report);
    }
    RuntimeReport {
        prelude,
        prelude_from,
        languages,
        requirements,
    }
}

/// A mismatch, at the severity the three outcomes in the plan give it.
///
/// With no prelude set it is a **note**: nobody has been asked yet, and
/// the next `start` asks — exit 3 with the question, which is designed
/// behaviour rather than a break. With one set, empty included, it is a
/// **problem**: somebody has said how this machine resolves the runtime,
/// and it does not, so the next start spawns a process that dies of it.
fn mismatch_finding(
    language: &LanguageReport,
    prelude: Option<&str>,
    prelude_from: &Option<String>,
) -> Finding {
    let wanted = format!(
        "{} {} ({})",
        language.language, language.spec, language.source
    );
    let got = match (&language.resolved, &language.resolved_from) {
        (Some(version), Some(path)) => format!("`bash -lc` here resolves {version}, from {path}"),
        _ => match &language.failure {
            Some(failure) => format!("the prelude never got as far as asking: {failure}"),
            None => format!("`bash -lc` here has no {} at all", language.language),
        },
    };
    let mut fix = String::new();
    let _ = writeln!(
        fix,
        "pando runs every command with `bash -lc`, which is not your interactive shell"
    );
    match language.fixes.as_slice() {
        [] => {
            let _ = writeln!(
                fix,
                "no version manager pando knows about is installed for {} — install one, or \
                 put the right binary on the PATH a login bash shell has",
                language.language
            );
        }
        fixes => {
            let _ = writeln!(fix, "set [runtime].prelude to one of:");
            for line in fixes {
                let _ = writeln!(fix, "  {line}");
            }
        }
    }
    match prelude {
        None => Finding {
            section: Section::Runtime,
            severity: Severity::Note,
            message: format!(
                "this project asks for {wanted}, and {got} — nobody has answered the prelude \
                 question, so the next `start` will ask"
            ),
            fix: Some(fix.trim_end().to_string()),
        },
        Some(line) if line.trim().is_empty() => Finding {
            section: Section::Runtime,
            severity: Severity::Problem,
            message: format!(
                "this project asks for {wanted}, and {got} — `[runtime].prelude` is set to \"\", \
                 which says this machine needs nothing in front of a command"
            ),
            fix: Some(fix.trim_end().to_string()),
        },
        Some(line) => Finding {
            section: Section::Runtime,
            severity: Severity::Problem,
            message: format!(
                "this project asks for {wanted}, and {got} — the prelude {line:?}{} is not \
                 working",
                match prelude_from {
                    Some(from) => format!(" in {from}"),
                    None => String::new(),
                }
            ),
            fix: Some(fix.trim_end().to_string()),
        },
    }
}

fn project_report(
    paths: &PandoPaths,
    config: &Config,
    findings: &mut Vec<Finding>,
) -> ProjectReport {
    let home_mode = mode_of(&paths.home);
    if let Some(mode) = &home_mode
        && mode != "700"
    {
        findings.push(
            Finding::note(
                Section::Project,
                format!(
                    "pando's home is mode {mode}, not 700 — it holds command lines, and a \
                     project's config can carry a credential"
                ),
            )
            .with_fix(format!("chmod 700 {}", paths.home.display())),
        );
    }
    // Read straight, with no lock and no save: `actions::refresh` would
    // take the lock, advance phases and write the file back, and doctor
    // writes nothing.
    let windows_held = state::load(&paths.state_file())
        .map(|store| {
            store
                .worktrees
                .values()
                .filter(|record| !record.ports.is_empty())
                .count()
        })
        .unwrap_or(0);
    ProjectReport {
        id: paths.project_id().to_string(),
        root: paths.root().display().to_string(),
        home: paths.home.display().to_string(),
        worktrees_dir: config.worktrees_dir(paths).display().to_string(),
        home_mode,
        port_min: ports::PORT_MIN,
        port_max: ports::PORT_MAX,
        base_step: ports::BASE_STEP,
        bases_in_range: ports::BASE_COUNT,
        windows_held,
    }
}

fn mode_of(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(format!("{:o}", meta.permissions().mode() & 0o777))
}

fn config_report(
    paths: &PandoPaths,
    error: Option<String>,
    warnings: Vec<String>,
    findings: &mut Vec<Finding>,
) -> ConfigReport {
    if let Some(error) = &error {
        findings.push(Finding::problem(
            Section::Config,
            format!("the config does not load: {error}"),
            "fix the file the message names — `new`, `start`, `restart` and the TUI need it, \
             and every other command is running without it",
        ));
    }
    for warning in &warnings {
        findings.push(Finding::note(Section::Config, warning.clone()));
    }
    let layers = vec![
        layer_report("committed", &paths.root().join("pando.toml"), true),
        layer_report("user", &paths.user_config_file(), true),
        layer_report("project", &paths.config_file(), false),
    ];
    ConfigReport { layers, error }
}

/// The two keys only pando's own layer may set. A committed file belongs to
/// a team and a user file to a machine; neither gets to decide where pando
/// writes for this project.
const STRIPPED_BELOW_PROJECT: [&str; 2] = ["project.root", "project.worktrees_dir"];

fn layer_report(layer: &'static str, path: &Path, strips: bool) -> LayerReport {
    let display = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LayerReport {
                layer,
                path: display,
                present: false,
                keys: Vec::new(),
                error: None,
            };
        }
        Err(e) => {
            return LayerReport {
                layer,
                path: display,
                present: true,
                keys: Vec::new(),
                error: Some(format!("cannot be read: {e}")),
            };
        }
    };
    let doc: toml_edit::DocumentMut = match text.parse() {
        Ok(doc) => doc,
        Err(e) => {
            return LayerReport {
                layer,
                path: display,
                present: true,
                keys: Vec::new(),
                error: Some(format!("is not valid TOML: {e}")),
            };
        }
    };
    let mut keys = Vec::new();
    walk_table(doc.as_table(), "", &mut keys);
    if strips {
        for key in &mut keys {
            key.ignored = STRIPPED_BELOW_PROJECT.contains(&key.key.as_str());
        }
    }
    LayerReport {
        layer,
        path: display,
        present: true,
        keys,
        error: None,
    }
}

/// Every key of a document, with the comment the file itself carries beside
/// it.
///
/// Read out of the file rather than re-derived: `set_detected` puts its note
/// in a value's trailing decor, `set_detected_table` on a table's own
/// header, and `set_detected_array_entry` on an entry's. Reading them back
/// shows what is there — including a comment a developer wrote by hand,
/// which no model of what pando would have written could produce.
fn walk_table(table: &toml_edit::Table, prefix: &str, out: &mut Vec<KeyReport>) {
    for (key, item) in table.iter() {
        let path = match prefix.is_empty() {
            true => key.to_string(),
            false => format!("{prefix}.{key}"),
        };
        match item {
            toml_edit::Item::Value(value) => out.push(KeyReport {
                key: path,
                value: Some(value_repr(value)),
                note: comment(value.decor().suffix().and_then(|s| s.as_str())),
                ignored: false,
            }),
            toml_edit::Item::Table(inner) => {
                if let Some(note) = comment(inner.decor().suffix().and_then(|s| s.as_str())) {
                    out.push(KeyReport {
                        key: path.clone(),
                        value: None,
                        note: Some(note),
                        ignored: false,
                    });
                }
                walk_table(inner, &path, out);
            }
            toml_edit::Item::ArrayOfTables(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let path = format!("{path}[{index}]");
                    out.push(KeyReport {
                        key: path.clone(),
                        value: None,
                        note: comment(entry.decor().suffix().and_then(|s| s.as_str())),
                        ignored: false,
                    });
                    walk_table(entry, &path, out);
                }
            }
            toml_edit::Item::None => {}
        }
    }
}

/// A value without the whitespace and comments around it: the report lines
/// those up itself.
fn value_repr(value: &toml_edit::Value) -> String {
    let mut bare = value.clone();
    bare.decor_mut().clear();
    bare.to_string().trim().to_string()
}

/// A decor suffix as a comment, or `None` when there is nothing but
/// whitespace in it.
fn comment(suffix: Option<&str>) -> Option<String> {
    let text = suffix?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

// ---- tools ----------------------------------------------------------------

const TOOL_PATH_MARK: &str = "pando-tool-path:";
const TOOL_VERSION_MARK: &str = "pando-tool-version:";
const TOOL_DETAIL_MARK: &str = "pando-tool-detail:";
const TOOL_DONE_MARK: &str = "pando-tool-ok";

/// One executable to look for, and what to ask it.
struct ToolProbe {
    name: String,
    program: String,
    /// What to pass it for a version. Static, and pando's own: nothing a
    /// project wrote reaches a command line here.
    args: &'static str,
    /// A second question worth one line, such as which docker context is
    /// active.
    detail_args: Option<&'static str>,
    /// How the detail is introduced when there is one.
    detail_label: &'static str,
    needed_for: String,
    /// What to say when it is not there. `None` means a line and nothing
    /// more — a tool this project has not asked pando to run.
    missing: Option<(Severity, String)>,
}

#[derive(Default)]
struct ToolFound {
    path: Option<String>,
    version: Option<String>,
    detail: Option<String>,
}

fn tools_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> Vec<ToolReport> {
    let probes = tool_probes(paths, config);
    // Run behind the prelude, because every command pando runs is run
    // behind it: a package manager that only exists after `nvm use` is
    // there for a real spawn and would read as missing here.
    let prelude = config.runtime.prelude.clone().unwrap_or_default();
    let (found, failure) = probe_tools(machine.shell, &probes, prelude.trim());
    if let Some(failure) = &failure {
        findings.push(Finding::note(
            Section::Tools,
            format!("pando could not ask this shell what it has: {failure}"),
        ));
    }
    let mut out = Vec::new();
    for (index, probe) in probes.iter().enumerate() {
        let entry = found.get(&index);
        let present = entry.is_some_and(|f| f.path.is_some());
        // Only when the probe itself ran: "not found" is a claim about
        // this machine, and a shell that never answered has not made it.
        if !present
            && failure.is_none()
            && let Some((severity, reason)) = &probe.missing
        {
            findings.push(Finding {
                section: Section::Tools,
                severity: *severity,
                message: format!(
                    "{} is not on the PATH `bash -lc` has — {reason}",
                    probe.name
                ),
                fix: Some(
                    "install it, or set [runtime].prelude so a login bash shell finds it"
                        .to_string(),
                ),
            });
        }
        out.push(ToolReport {
            name: probe.name.clone(),
            path: entry.and_then(|f| f.path.clone()),
            version: entry.and_then(|f| f.version.clone()),
            detail: entry
                .and_then(|f| f.detail.clone())
                .map(|d| format!("{}{d}", probe.detail_label)),
            needed_for: probe.needed_for.clone(),
            found: present,
        });
    }
    out
}

/// Every tool worth asking about: the ones pando itself runs, and the ones
/// this project's own config and lockfiles say it will run.
fn tool_probes(paths: &PandoPaths, config: &Config) -> Vec<ToolProbe> {
    let mut probes = vec![ToolProbe {
        name: "git".to_string(),
        program: "git".to_string(),
        args: "--version",
        detail_args: None,
        detail_label: "",
        needed_for: "everything: worktrees are git's".to_string(),
        missing: Some((
            Severity::Problem,
            "pando has nothing to manage without it".to_string(),
        )),
    }];

    // The shim hook `services::docker_program` already knows about, so a
    // developer whose docker is not on PATH is reported through the same
    // binary an isolated start would use.
    let docker = services::docker_program(paths).display().to_string();
    let isolates = config
        .services
        .iter()
        .any(|s| matches!(s, ServiceConfig::Compose { .. }));
    let needed = "`start --isolated`, which runs private copies of the project's services";
    let missing_docker = isolates.then(|| {
        (
            Severity::Note,
            "this project declares compose services, so `--isolated` cannot run; a plain \
             `start` still can"
                .to_string(),
        )
    });
    probes.push(ToolProbe {
        name: "docker".to_string(),
        program: docker.clone(),
        args: "--version",
        detail_args: Some("context show"),
        detail_label: "context: ",
        needed_for: needed.to_string(),
        missing: missing_docker.clone(),
    });
    probes.push(ToolProbe {
        name: "docker compose".to_string(),
        program: docker,
        args: "compose version --short",
        detail_args: None,
        detail_label: "",
        needed_for: needed.to_string(),
        // Never its own finding: when docker is missing this is the same
        // news twice, and when docker is there a compose plugin that is
        // not is reported by the line.
        missing: None,
    });
    probes.push(ToolProbe {
        name: "cloudflared".to_string(),
        program: tunnel::cloudflared_program(paths).display().to_string(),
        args: "--version",
        detail_args: None,
        detail_label: "",
        needed_for: "`pando share`, which publishes a worktree at a public URL".to_string(),
        // `share` is opt-in and says this itself. A line is enough.
        missing: None,
    });

    for (program, needed_for, named_by_config) in project_programs(paths, config) {
        if probes.iter().any(|p| p.program == program) {
            continue;
        }
        probes.push(ToolProbe {
            name: program.clone(),
            program,
            args: "--version",
            detail_args: None,
            detail_label: "",
            needed_for: needed_for.clone(),
            missing: named_by_config.then_some((Severity::Problem, needed_for)),
        });
    }
    probes
}

/// The lockfile a package manager writes, and the binary that writes it.
const LOCKFILE_PROGRAMS: [(&str, &str); 11] = [
    ("pnpm-lock.yaml", "pnpm"),
    ("package-lock.json", "npm"),
    ("yarn.lock", "yarn"),
    ("bun.lockb", "bun"),
    ("bun.lock", "bun"),
    ("uv.lock", "uv"),
    ("poetry.lock", "poetry"),
    ("Gemfile.lock", "bundle"),
    ("mix.lock", "mix"),
    ("go.sum", "go"),
    ("Cargo.lock", "cargo"),
];

/// Programs this project will have pando run, with whether config names
/// them.
///
/// Config naming one is the difference between a line and a problem: a
/// lockfile is a signal that a manager is *probably* wanted, an
/// `install =` is pando being told to run it.
fn project_programs(paths: &PandoPaths, config: &Config) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    if let Some(install) = config.project.install.as_deref()
        && let Some(program) = command_program(install)
    {
        out.push((
            program,
            "the install step every new worktree runs".to_string(),
            true,
        ));
    }
    for hook in &config.hooks {
        let Some(program) = command_program(&hook.cmd) else {
            continue;
        };
        if out.iter().any(|(p, _, _)| *p == program) {
            continue;
        }
        out.push((program, format!("the hook {:?}", hook.name), true));
    }
    let signals = detect::signals(paths.root());
    for lockfile in &signals.lockfiles {
        let Some((_, program)) = LOCKFILE_PROGRAMS.iter().find(|(l, _)| l == lockfile) else {
            continue;
        };
        if out.iter().any(|(p, _, _)| p == program) {
            continue;
        }
        out.push((
            (*program).to_string(),
            format!("{lockfile} is in this repository"),
            false,
        ));
    }
    out
}

/// The program a shell command really runs: the last `&&`-joined step's
/// first word that is not a `KEY=value` prefix.
///
/// The last step, because `corepack enable && pnpm install` is about pnpm.
fn command_program(cmd: &str) -> Option<String> {
    let steps: Vec<&str> = cmd.split("&&").filter(|s| !s.trim().is_empty()).collect();
    let step = steps.last()?;
    let word = step.split_whitespace().find(|w| !w.contains('='))?;
    // A path, a template or a quoted word is not a program name worth
    // asking `command -v` about, and it is exactly the shape that would
    // put something surprising on a command line.
    let plain = word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    plain.then(|| word.to_string())
}

/// Asks one shell where every tool is and what it says it is.
///
/// One shell for all of them, and it is the *same* shell the runtime probe
/// and every spawn use — `bash -lc` in the main checkout, output captured,
/// with a deadline on it. A login shell reads a profile, so a dozen of
/// them is a dozen times the wait for one answer; and going through
/// `Machine` is what lets a test report on a laptop it does not have.
///
/// Returns what it learned plus, when the shell never got to the end, why:
/// "not found" is a claim about this machine, and a probe that did not run
/// has not made it.
fn probe_tools(
    shell: runtime::Shell<'_>,
    probes: &[ToolProbe],
    prelude: &str,
) -> (BTreeMap<usize, ToolFound>, Option<String>) {
    if probes.is_empty() {
        return (BTreeMap::new(), None);
    }
    let script = tool_script(probes, prelude);
    let Some(text) = shell(&script) else {
        return (
            BTreeMap::new(),
            Some("the shell did not answer inside its deadline".to_string()),
        );
    };
    let mut found: BTreeMap<usize, ToolFound> = BTreeMap::new();
    let mut done = false;
    for line in text.lines() {
        let line = line.trim();
        if line == TOOL_DONE_MARK {
            done = true;
            continue;
        }
        for (mark, field) in [
            (TOOL_PATH_MARK, 0u8),
            (TOOL_VERSION_MARK, 1),
            (TOOL_DETAIL_MARK, 2),
        ] {
            let Some(rest) = line.strip_prefix(mark) else {
                continue;
            };
            let Some((index, value)) = rest.split_once(' ') else {
                continue;
            };
            let Ok(index) = index.parse::<usize>() else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let entry = found.entry(index).or_default();
            match field {
                0 => entry.path = Some(value.to_string()),
                1 => entry.version = Some(value.to_string()),
                _ => entry.detail = Some(value.to_string()),
            }
        }
    }
    if done {
        return (found, None);
    }
    // The body never ran. With a prelude set that is the prelude's
    // failure, and it is the same one every spawn would hit.
    let last = text
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string();
    (
        found,
        Some(match prelude.is_empty() {
            true => format!("the probe did not finish — {last}"),
            false => format!("the prelude in front of it failed — {last}"),
        }),
    )
}

/// One shell script for every probe, composed the way a real spawn is.
fn tool_script(probes: &[ToolProbe], prelude: &str) -> String {
    let mut body = String::new();
    for (index, probe) in probes.iter().enumerate() {
        let program = proc::shell_quote(&probe.program);
        let _ = write!(
            body,
            "if __pando_p=$(command -v {program} 2>/dev/null); then \
             printf '{TOOL_PATH_MARK}{index} %s\\n' \"$__pando_p\"; \
             printf '{TOOL_VERSION_MARK}{index} %s\\n' \
             \"$({program} {} 2>&1 | head -n 1)\"; ",
            probe.args
        );
        if let Some(detail) = probe.detail_args {
            let _ = write!(
                body,
                "printf '{TOOL_DETAIL_MARK}{index} %s\\n' \
                 \"$({program} {detail} 2>&1 | head -n 1)\"; "
            );
        }
        let _ = writeln!(body, "fi");
    }
    let _ = writeln!(body, "echo {TOOL_DONE_MARK}");
    match prelude {
        "" => body,
        prelude => format!("{prelude} && {{\n{body}}}"),
    }
}

// ---- adoption -------------------------------------------------------------

/// A project folder under pando's home whose repository is no longer where
/// it was.
///
/// The project id is the main checkout's directory name plus a hash of its
/// canonical path, so moving a repository gives it a new id and an empty
/// folder beside the old one — with the old one still holding the config,
/// the state and, by default, the worktrees.
#[derive(Debug, Clone, Serialize)]
pub struct Adoptable {
    pub id: String,
    pub path: String,
    /// The repository that folder belonged to, when it can be worked out —
    /// from a worktree's own `.git` file, which names the repository it
    /// was linked from.
    pub old_root: Option<String>,
    /// The worktree directories still sitting inside it.
    pub worktrees: Vec<String>,
}

/// What `--adopt` would do, for the confirmation.
#[derive(Debug, Clone)]
pub struct AdoptPlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub old_root: Option<PathBuf>,
    pub worktrees: Vec<String>,
}

/// What `--adopt` did.
#[derive(Debug, Clone)]
pub struct Adoption {
    pub from: PathBuf,
    pub to: PathBuf,
    /// How many recorded paths moved with the folder.
    pub rewritten: usize,
    /// Anything worth saying that is not a failure: what git had to be
    /// told, and what it said back.
    pub notices: Vec<String>,
}

fn adoptable(paths: &PandoPaths) -> Vec<Adoptable> {
    let Ok(entries) = std::fs::read_dir(paths.projects_dir()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        if id == paths.project_id() || !entry.path().is_dir() {
            continue;
        }
        // The id is `<directory name>-<8 hex>`, so the name in front of
        // the hash is the repository's own directory name — which is what
        // survives a move, and the only evidence that this folder is about
        // *this* repository rather than some other project.
        let Some((display, hash)) = id.rsplit_once('-') else {
            continue;
        };
        if display != paths.project.display_name
            || hash.len() != 8
            || !hash.chars().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        let old_root = recorded_root(&entry.path());
        // A folder whose repository is still there belongs to a live
        // project — two checkouts of the same repository under different
        // directories is an ordinary thing, and adopting one of them would
        // be stealing it.
        if old_root.as_deref().is_some_and(Path::exists) {
            continue;
        }
        out.push(Adoptable {
            id,
            path: entry.path().display().to_string(),
            old_root: old_root.map(|p| p.display().to_string()),
            worktrees: worktree_names(&entry.path()),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The repository a project folder belonged to.
///
/// Nothing writes `project.root`, so the honest source is a worktree's own
/// `.git` file: git writes `gitdir: <repository>/.git/worktrees/<name>`
/// into it, and that names the checkout it was linked from. A folder with
/// no worktrees left has nothing to say, and says `None` rather than a
/// guess.
fn recorded_root(project_dir: &Path) -> Option<PathBuf> {
    if let Some(root) = std::fs::read_to_string(project_dir.join("pando.toml"))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            Some(PathBuf::from(
                doc.get("project")?.get("root")?.as_str()?.to_string(),
            ))
        })
    {
        return Some(root);
    }
    for name in worktree_names(project_dir) {
        let marker = project_dir.join("worktrees").join(&name).join(".git");
        let Ok(text) = std::fs::read_to_string(&marker) else {
            continue;
        };
        let Some(gitdir) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
            continue;
        };
        let gitdir = PathBuf::from(gitdir.trim());
        // `<root>/.git/worktrees/<name>` — three components back to the
        // checkout it was linked from. Absolute, because git writes it
        // absolute and a relative one climbing out of three components
        // leaves an empty path that would read as "its repository was at
        // , and is not there now".
        if let Some(root) = gitdir.ancestors().nth(3)
            && root.is_absolute()
        {
            return Some(root.to_path_buf());
        }
    }
    None
}

fn worktree_names(project_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(project_dir.join("worktrees")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// Moves a project folder under this repository's current id.
///
/// The one thing `doctor` does rather than reports, which is why it asks
/// first. Nothing inside the folder changes except the paths that named the
/// folder itself: the config is the same file, the state is the same
/// records, and the worktrees are the same directories.
///
/// Two things have to be put back afterwards, and both are consequences of
/// the folder having moved rather than decisions of pando's. pando's own
/// records hold absolute paths, so every one that pointed inside the old
/// folder is rewritten to point inside the new one. And git holds absolute
/// paths in both directions — `.git/worktrees/<name>/gitdir` in the
/// repository, and the `.git` file in each worktree — so `git worktree
/// repair` is run to reconnect them. That is the one command git documents
/// for exactly this, and Invariant 1 names what `git worktree` records
/// inside `.git` as pando's to touch.
pub fn adopt(
    paths: &PandoPaths,
    old_id: &str,
    confirm: &dyn Fn(&AdoptPlan) -> anyhow::Result<bool>,
) -> anyhow::Result<Adoption> {
    if old_id == paths.project_id() {
        anyhow::bail!("{old_id} is this repository's own project folder");
    }
    // Exactly one ordinary directory name. `.` and `..` are each one
    // component too, and either of them would make the destination a
    // subdirectory of the source.
    let mut parts = Path::new(old_id).components();
    let one_plain_name =
        matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none();
    if old_id.is_empty() || !one_plain_name {
        anyhow::bail!("{old_id:?} is not a project id — it is one directory name under projects/");
    }
    let from = paths.projects_dir().join(old_id);
    if !from.is_dir() {
        anyhow::bail!(
            "there is no project folder {old_id:?} in {} — `pando doctor` lists the ones there \
             are",
            paths.projects_dir().display()
        );
    }
    let to = paths.project_dir();
    if to.exists() {
        anyhow::bail!(
            "{} already exists — pando will not merge two project folders, and moving {old_id} \
             on top of it would lose whichever it overwrote",
            to.display()
        );
    }
    let old_root = recorded_root(&from);
    if let Some(root) = &old_root
        && root.exists()
    {
        anyhow::bail!(
            "{old_id} belongs to the repository at {} — it is still there, so this is not a \
             repository that moved",
            root.display()
        );
    }
    // Before anything moves, because everything after the rename is
    // reconnecting and none of it can be retried: `--adopt` looks for the
    // folder by its old id, and after the move there is no folder by that
    // name to look for. A state file this build cannot read is exactly
    // the shape an *old* folder has — it is by construction from before
    // the move — and it is the one thing that would leave every recorded
    // path pointing at a directory that is no longer there.
    if let Err(e) = state::load(&from.join("state.json")) {
        anyhow::bail!(
            "{e:#} — that file has to be readable before the folder moves, because the paths in \
             it all name the folder it is in; fix it, or move it aside and adopt again"
        );
    }
    let plan = AdoptPlan {
        from: from.clone(),
        to: to.clone(),
        old_root,
        worktrees: worktree_names(&from),
    };
    if !confirm(&plan)? {
        anyhow::bail!("nothing was moved");
    }

    // Creates `projects/` the one way that makes the home 0700 and refuses
    // one inside the repository.
    paths.ensure_home()?;
    // `ensure_home` creates the *new* project directory, and a rename onto
    // an existing directory fails. It is pando's own, it was created a
    // moment ago, and it is empty.
    if to.is_dir() {
        std::fs::remove_dir(&to)
            .with_context(|| format!("clear the empty {} to move onto it", to.display()))?;
    }
    std::fs::rename(&from, &to)
        .with_context(|| format!("move {} to {}", from.display(), to.display()))?;

    // Past this point the folder has moved, which is the part that
    // cannot be done by hand. Everything left is reconnecting, and a
    // failure in it is reported rather than raised: an error after a
    // successful move would read as "it did not happen".
    let mut notices = Vec::new();
    let rewritten = match rewrite_recorded_paths(paths, &from, &to) {
        Ok(rewritten) => rewritten,
        Err(e) => {
            notices.push(format!(
                "the folder moved, and pando's own records still point inside the old one:                  {e:#} — `pando rm` and `pando start` will not find those worktrees until the                  state file is fixed or moved aside"
            ));
            0
        }
    };
    notices.extend(repair_worktrees(paths, &to));
    Ok(Adoption {
        from,
        to,
        rewritten,
        notices,
    })
}

/// Every recorded path that pointed inside the old folder, pointed inside
/// the new one.
///
/// Worktree directories, process logs and a share's log all live under the
/// project folder by default, and all three are recorded absolute. A
/// `worktrees_dir` the developer put somewhere else is not under the old
/// folder, so it is not touched — which is the right answer for it too.
fn rewrite_recorded_paths(paths: &PandoPaths, from: &Path, to: &Path) -> anyhow::Result<usize> {
    let _lock = state::lock(&paths.lock_file())?;
    let mut store = state::load(&paths.state_file())?;
    let mut moved_count = 0usize;
    for record in store.worktrees.values_mut() {
        if let Some(path) = moved(&record.path, from, to) {
            record.path = path;
            moved_count += 1;
        }
        for process in record.processes.values_mut() {
            if let Some(path) = moved(&process.log_path, from, to) {
                process.log_path = path;
                moved_count += 1;
            }
        }
        if let Some(share) = record.share.as_mut()
            && let Some(path) = moved(&share.log_path, from, to)
        {
            share.log_path = path;
            moved_count += 1;
        }
    }
    if moved_count > 0 {
        state::save(&paths.state_file(), &store)?;
    }
    Ok(moved_count)
}

/// `path` with the `from` prefix replaced by `to`, or `None` when it was
/// never inside `from`.
///
/// Compared through `resolve_for_compare`, because a recorded path was
/// canonicalised when it was written and macOS spells the same directory
/// `/var/…` and `/private/var/…`.
fn moved(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let resolved = crate::paths::resolve_for_compare(path);
    let prefix = crate::paths::resolve_for_compare(from);
    // Rebuilt from the canonical destination, so a record keeps the
    // spelling every other path in the file has — `record_is_for` compares
    // these against a canonicalised worktree path.
    let destination = crate::paths::resolve_for_compare(to);
    resolved
        .strip_prefix(&prefix)
        .ok()
        .map(|rest| destination.join(rest))
}

/// Tells git where the worktrees are now.
///
/// After a move, both halves of git's bookkeeping are wrong: the `.git`
/// file in each worktree names a repository that is not there, and
/// `.git/worktrees/<name>/gitdir` in the repository names a worktree that
/// is not there. `git worktree repair` is git's own command for exactly
/// this, and it writes only inside `.git`, which Invariant 1 allows.
///
/// A failure here is reported, never fatal: the folder has already moved,
/// which is the part that cannot be done by hand.
fn repair_worktrees(paths: &PandoPaths, project_dir: &Path) -> Vec<String> {
    let names = worktree_names(project_dir);
    if names.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec!["worktree".to_string(), "repair".to_string()];
    args.extend(names.iter().map(|name| {
        project_dir
            .join("worktrees")
            .join(name)
            .display()
            .to_string()
    }));
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(paths.root())
        .args(&args)
        .output();
    match out {
        Ok(out) if out.status.success() => {
            let mut notices = vec![format!(
                "told git where {} {} now: git worktree repair",
                names.len(),
                plural(names.len(), "worktree")
            )];
            notices.extend(
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(|l| format!("git: {l}")),
            );
            notices
        }
        Ok(out) => vec![format!(
            "git could not repair the worktrees: {} — run `git worktree repair` in {} by hand",
            String::from_utf8_lossy(&out.stderr).trim(),
            paths.root().display()
        )],
        Err(e) => vec![format!(
            "git could not be run to repair the worktrees ({e}) — run `git worktree repair` in \
             {} by hand",
            paths.root().display()
        )],
    }
}

// ---- validation -----------------------------------------------------------

fn validate_config(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    check_provision(paths, config, findings);
    check_install(config, findings);
    check_templates(config, findings);
}

/// Every path a worktree is given a copy of has to be gitignored in the
/// main checkout, or `new` refuses it. Checked with git itself, the way
/// `new` checks it.
fn check_provision(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    for entry in config.project.provision_paths() {
        if detect::is_gitignored(paths.root(), entry) {
            continue;
        }
        findings.push(Finding::problem(
            Section::Config,
            format!(
                "`project.provision` names {entry:?}, which this repository does not ignore — \
                 `pando new` refuses to write a file that would show up in `git status`"
            ),
            format!("add {entry} to the repository's .gitignore, or drop it from provision"),
        ));
    }
    // A statement about a file nobody asked for. Dangling, not unsafe:
    // `provision_from` only ever answers "where does this provisioned file
    // come from", and a source for a destination nothing provisions is
    // never read.
    for destination in config.project.provision_from.keys() {
        if config
            .project
            .provision_paths()
            .iter()
            .any(|p| p == destination)
        {
            continue;
        }
        findings.push(Finding::note(
            Section::Config,
            format!(
                "`project.provision_from` has a source for {destination:?}, which is not in \
                 `project.provision` — nothing reads it"
            ),
        ));
    }
}

/// A package-manager install and the flag that stops it rewriting a
/// lockfile, plus the spelling pando would have written.
///
/// Only the shapes pando itself proposes. A project whose install step is
/// `make setup` has said something pando has no opinion about, and guessing
/// at it would make this check noise.
const INSTALL_SHAPES: [(&str, &str, &[&str], &str); 8] = [
    (
        "pnpm",
        "install",
        &["--frozen-lockfile"],
        "pnpm install --frozen-lockfile",
    ),
    ("npm", "install", &[], "npm ci"),
    ("npm", "i", &[], "npm ci"),
    (
        "yarn",
        "install",
        &["--immutable", "--frozen-lockfile"],
        "yarn install --immutable",
    ),
    (
        "bun",
        "install",
        &["--frozen-lockfile"],
        "bun install --frozen-lockfile",
    ),
    ("uv", "sync", &["--frozen", "--locked"], "uv sync --frozen"),
    ("cargo", "fetch", &["--locked"], "cargo fetch --locked"),
    (
        "bundle",
        "install",
        &["BUNDLE_FROZEN", "--deployment", "--frozen"],
        "BUNDLE_FROZEN=true bundle install",
    ),
];

fn check_install(config: &Config, findings: &mut Vec<Finding>) {
    let Some(install) = config.project.install.as_deref() else {
        return;
    };
    // Every step of a chained command, because `cd apps/web && pnpm
    // install` is one of the shapes a developer writes.
    for step in install.split("&&").flat_map(|s| s.split(';')) {
        let words: Vec<&str> = step.split_whitespace().collect();
        // Leading `KEY=value` is how a frozen bundle is spelled, so the
        // program is the first word that is not one.
        let program = words.iter().find(|w| !w.contains('='));
        let Some(program) = program else { continue };
        let index = words.iter().position(|w| w == program).unwrap_or(0);
        let sub = words.get(index + 1).copied().unwrap_or("");
        let Some((_, _, markers, frozen)) = INSTALL_SHAPES
            .iter()
            .find(|(p, s, _, _)| p == program && *s == sub)
        else {
            continue;
        };
        if markers.iter().any(|marker| step.contains(marker)) {
            continue;
        }
        findings.push(Finding::problem(
            Section::Config,
            format!(
                "`project.install` runs {:?}, which can rewrite the project's lockfile — pando \
                 never runs a non-frozen install",
                step.trim()
            ),
            format!("use `{frozen}`"),
        ));
    }
}

/// Every `{…}` a process's command and environment carry, resolved against
/// the roles the config itself declares.
///
/// A `{port:<role>}` naming a role nothing owns is a start that fails after
/// the worktree exists and the install has run. Nothing before this said so
/// at rest.
fn check_templates(config: &Config, findings: &mut Vec<Finding>) {
    let roles = declared_roles(config);
    for (name, process) in &config.processes {
        // What a bare `{port}` means for this process, exactly as `start`
        // resolves it: the role it owns.
        let own = process.roles();
        let default_role = own.first().map(String::as_str);
        let mut texts: Vec<(String, String)> = vec![("cmd".to_string(), process.cmd.clone())];
        if let Some(cwd) = &process.cwd {
            texts.push(("cwd".to_string(), cwd.clone()));
        }
        for (key, value) in process.env.iter().chain(process.port_env().iter()) {
            texts.push((format!("env.{key}"), value.clone()));
        }
        for (what, text) in texts {
            check_template(
                &roles,
                default_role,
                &format!("process {name:?}"),
                &what,
                &text,
                &format!("name a role something owns, or give {name:?} that role in its `ports`"),
                findings,
            );
        }
    }
    // A hook owns no role, so a bare `{port}` in one is an error naming
    // the roles it could have used — which is what `run_hook` passes too.
    for hook in &config.hooks {
        let mut texts: Vec<(String, String)> = vec![("cmd".to_string(), hook.cmd.clone())];
        if let Some(cwd) = &hook.cwd {
            texts.push(("cwd".to_string(), cwd.clone()));
        }
        if let Some(fallback) = &hook.fallback {
            texts.push(("fallback".to_string(), fallback.clone()));
        }
        for (what, text) in texts {
            check_template(
                &roles,
                None,
                &format!("hook {:?}", hook.name),
                &what,
                &text,
                "name a role something owns — a hook owns none of its own, so `{port:<role>}` \
                 has to say which",
                findings,
            );
        }
    }
}

/// One template, rendered against a worktree that could exist.
///
/// The paths are stand-ins: what is being checked is whether every
/// placeholder *resolves*, and the only ones that can fail are the ones
/// naming a role.
#[allow(clippy::too_many_arguments)]
fn check_template(
    roles: &BTreeMap<String, u16>,
    default_role: Option<&str>,
    owner: &str,
    what: &str,
    text: &str,
    fix: &str,
    findings: &mut Vec<Finding>,
) {
    let ctx = template::Context {
        name: "a-worktree",
        branch: Some("a-branch"),
        worktree: Path::new("/worktree"),
        root: Path::new("/root"),
        project: "project",
        ports: roles,
        default_role,
        log: Some(Path::new("/log")),
    };
    if let Err(e) = template::render(text, &ctx) {
        findings.push(Finding::problem(
            Section::Config,
            format!("{owner}: {what} cannot be resolved — {e:#}"),
            fix,
        ));
    }
}

/// Every role this config declares, mapped to a number that is only there
/// so a template can render.
///
/// Both kinds: a process's own roles and a service's name, which is a role
/// too — that is what lets a process be told the port of the database
/// beside it.
fn declared_roles(config: &Config) -> BTreeMap<String, u16> {
    let mut out = BTreeMap::new();
    let mut next = ports::PORT_MIN;
    let mut give = |role: String, out: &mut BTreeMap<String, u16>| {
        out.entry(role).or_insert_with(|| {
            next = next.saturating_add(1);
            next
        });
    };
    for process in config.processes.values() {
        for role in process.roles() {
            give(role, &mut out);
        }
    }
    for service in &config.services {
        if let ServiceConfig::Compose { include, .. } = service {
            for name in include {
                give(name.clone(), &mut out);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::testutil::{git, init_repo};
    use tempfile::TempDir;

    struct Fx {
        _dir: TempDir,
        paths: PandoPaths,
        root: std::path::PathBuf,
        home: std::path::PathBuf,
        /// The *developer's* home, where version managers live — never
        /// the real one, so no test reads what this laptop has installed.
        machine_home: std::path::PathBuf,
    }

    fn fixture() -> Fx {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("repo");
        init_repo(&root);
        // Canonical, because `ProjectRef` canonicalises and macOS prints
        // `/var` where git prints `/private/var`: a test comparing the two
        // spellings is comparing the platform, not the report.
        let root = std::fs::canonicalize(&root).expect("canonical root");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        // 0700, the way `PandoPaths::ensure_home` creates it. A fixture
        // that leaves it at whatever the umask says would make every test
        // here read a note about the test's own directory.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
                .expect("chmod home");
        }
        let home = std::fs::canonicalize(&home).expect("canonical home");
        let paths = PandoPaths::new(&home, ProjectRef::from_root(&root).expect("project"));
        let machine_home = dir.path().join("developer-home");
        std::fs::create_dir_all(&machine_home).expect("machine home");
        Fx {
            _dir: dir,
            paths,
            root,
            home,
            machine_home,
        }
    }

    fn write_compose(fx: &Fx, body: &str) {
        std::fs::write(fx.root.join("docker-compose.yml"), body).expect("write compose");
    }

    fn write_project_config(fx: &Fx, body: &str) {
        let path = fx.paths.config_file();
        std::fs::create_dir_all(path.parent().expect("project dir")).expect("mkdir");
        std::fs::write(&path, body).expect("write config");
    }

    /// A shell that finds every tool the script asks about and reports
    /// nothing for the runtime.
    ///
    /// Injected rather than spawned: a unit test that ran a real login
    /// shell would be slow, would read whatever this laptop has installed,
    /// and would fail on a runner with a different PATH.
    fn every_tool(script: &str) -> Option<String> {
        if !script.contains(TOOL_DONE_MARK) {
            return Some("pando-runtime-ok\n".to_string());
        }
        let mut out = String::new();
        for index in 0.. {
            if !script.contains(&format!("{TOOL_PATH_MARK}{index} ")) {
                break;
            }
            let _ = writeln!(out, "{TOOL_PATH_MARK}{index} /usr/bin/thing{index}");
            let _ = writeln!(out, "{TOOL_VERSION_MARK}{index} 1.2.3");
        }
        let _ = writeln!(out, "{TOOL_DONE_MARK}");
        Some(out)
    }

    /// A shell that finds nothing at all, for the tests that are about a
    /// tool pando cannot find.
    fn no_tools(script: &str) -> Option<String> {
        if !script.contains(TOOL_DONE_MARK) {
            return Some("pando-runtime-ok\n".to_string());
        }
        Some(format!("{TOOL_DONE_MARK}\n"))
    }

    fn report_of(fx: &Fx, shell: &dyn Fn(&str) -> Option<String>) -> Report {
        run_on(
            &fx.paths,
            &Machine {
                shell,
                home: fx.machine_home.clone(),
            },
        )
    }

    fn report(fx: &Fx) -> Report {
        report_of(fx, &every_tool)
    }

    fn messages(report: &Report) -> Vec<String> {
        report.findings.iter().map(|f| f.message.clone()).collect()
    }

    fn mentions(report: &Report, needle: &str) -> bool {
        messages(report).iter().any(|m| m.contains(needle))
    }

    #[test]
    fn a_project_with_no_config_at_all_is_healthy_and_says_where_everything_is() {
        let fx = fixture();
        let report = report(&fx);
        assert!(report.healthy(), "{:?}", report.findings);
        assert_eq!(report.project.root, fx.root.display().to_string());
        assert_eq!(report.project.home, fx.home.display().to_string());
        assert_eq!(report.project.id, fx.paths.project_id());
        let text = report.render();
        assert!(text.contains("nothing to report"), "{text}");
        assert!(text.contains("not there"), "every layer is named: {text}");
    }

    #[test]
    fn every_layer_is_named_with_its_path_whether_or_not_it_is_there() {
        let fx = fixture();
        let report = report(&fx);
        let layers: Vec<&str> = report.config.layers.iter().map(|l| l.layer).collect();
        assert_eq!(layers, vec!["committed", "user", "project"]);
        assert_eq!(
            report.config.layers[0].path,
            fx.root.join("pando.toml").display().to_string()
        );
        assert_eq!(
            report.config.layers[1].path,
            fx.paths.user_config_file().display().to_string()
        );
        assert_eq!(
            report.config.layers[2].path,
            fx.paths.config_file().display().to_string()
        );
        assert!(report.config.layers.iter().all(|l| !l.present));
    }

    #[test]
    fn every_key_is_reported_with_the_comment_the_file_carries() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[project]\ninstall = \"pnpm install --frozen-lockfile\"  # detected: pnpm-lock.yaml\n\
             \n[dev]\ncmd = \"pnpm dev\"  # answered: 2026-09-21\nports = { PORT = \"web\" }\n",
        );
        let report = report(&fx);
        let project = report
            .config
            .layers
            .iter()
            .find(|l| l.layer == "project")
            .expect("the project layer");
        assert!(project.present);
        let install = project
            .keys
            .iter()
            .find(|k| k.key == "project.install")
            .expect("project.install");
        assert_eq!(
            install.value.as_deref(),
            Some("\"pnpm install --frozen-lockfile\"")
        );
        assert_eq!(install.note.as_deref(), Some("# detected: pnpm-lock.yaml"));
        let cmd = project
            .keys
            .iter()
            .find(|k| k.key == "dev.cmd")
            .expect("dev.cmd");
        assert_eq!(cmd.note.as_deref(), Some("# answered: 2026-09-21"));
        let ports = project
            .keys
            .iter()
            .find(|k| k.key == "dev.ports")
            .expect("dev.ports");
        assert_eq!(ports.note, None, "a key with no comment has no note");
    }

    #[test]
    fn an_array_of_tables_reports_its_entry_note_and_every_key_under_it() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]  # detected: docker-compose.yml names postgres\n\
             kind = \"compose\"\nfile = \"docker-compose.yml\"\ninclude = [\"postgres\"]\n",
        );
        let report = report(&fx);
        let project = &report.config.layers[2];
        let header = project
            .keys
            .iter()
            .find(|k| k.key == "services[0]")
            .expect("the entry header");
        assert_eq!(header.value, None);
        assert_eq!(
            header.note.as_deref(),
            Some("# detected: docker-compose.yml names postgres")
        );
        assert!(
            project.keys.iter().any(|k| k.key == "services[0].include"),
            "{:?}",
            project.keys
        );
    }

    #[test]
    fn a_key_a_committed_layer_may_not_set_is_marked_ignored_and_warned_about() {
        let fx = fixture();
        std::fs::write(
            fx.root.join("pando.toml"),
            "[project]\nroot = \"/somewhere/else\"\ninstall = \"make setup\"\n",
        )
        .expect("write committed config");
        let report = report(&fx);
        let committed = &report.config.layers[0];
        let root = committed
            .keys
            .iter()
            .find(|k| k.key == "project.root")
            .expect("project.root");
        assert!(root.ignored, "a committed project.root is stripped");
        let install = committed
            .keys
            .iter()
            .find(|k| k.key == "project.install")
            .expect("project.install");
        assert!(!install.ignored, "an ordinary key is not");
        assert!(
            mentions(&report, "ignoring project.root"),
            "{:?}",
            messages(&report)
        );
        // In the config section, below the layer it names.
        let text = report.render();
        let layer_line = text
            .lines()
            .position(|l| l.contains("committed"))
            .expect("the committed line");
        let warning_line = text
            .lines()
            .position(|l| l.contains("ignoring project.root"))
            .expect("the warning");
        assert!(warning_line > layer_line, "{text}");
        assert_eq!(
            text.matches("ignoring project.root").count(),
            1,
            "said once, not twice:\n{text}"
        );
        assert!(report.healthy(), "a stripped key is a note, not a problem");
    }

    #[test]
    fn a_project_layer_that_does_not_load_is_the_headline_problem() {
        let fx = fixture();
        write_project_config(&fx, "[project]\nnonsense_key = 1\n");
        let report = report(&fx);
        assert!(!report.healthy());
        assert!(report.config.error.is_some());
        assert!(
            mentions(&report, "the config does not load"),
            "{:?}",
            messages(&report)
        );
        // And the file is still shown, key by key: the point is to see
        // what is in it.
        assert!(
            report.config.layers[2]
                .keys
                .iter()
                .any(|k| k.key == "project.nonsense_key")
        );
    }

    #[test]
    fn a_layer_that_is_not_valid_toml_says_so_instead_of_pretending_it_is_empty() {
        let fx = fixture();
        write_project_config(&fx, "[project\n");
        let report = report(&fx);
        let project = &report.config.layers[2];
        assert!(project.present);
        assert!(
            project
                .error
                .as_deref()
                .is_some_and(|e| e.contains("not valid TOML")),
            "{:?}",
            project.error
        );
    }

    #[test]
    fn a_provision_path_the_repository_does_not_ignore_is_a_problem() {
        let fx = fixture();
        std::fs::write(fx.root.join(".gitignore"), ".env\n").expect("gitignore");
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "ignore"]);
        write_project_config(&fx, "[project]\nprovision = [\".env\", \"secrets.json\"]\n");
        let report = report(&fx);
        assert!(!report.healthy());
        assert!(
            mentions(&report, "\"secrets.json\""),
            "{:?}",
            messages(&report)
        );
        assert!(
            !mentions(&report, "\".env\""),
            "an ignored path is fine: {:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_provision_from_entry_for_a_path_nothing_provisions_is_a_note() {
        let fx = fixture();
        std::fs::write(fx.root.join(".gitignore"), ".env\n").expect("gitignore");
        git(&fx.root, &["add", "."]);
        git(&fx.root, &["commit", "--quiet", "-m", "ignore"]);
        write_project_config(
            &fx,
            "[project]\nprovision = [\".env\"]\n\n[project.provision_from]\n\
             \".env.local\" = \".env.example\"\n",
        );
        let report = report(&fx);
        assert!(report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "\".env.local\""),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn an_install_command_that_can_rewrite_a_lockfile_is_a_problem() {
        for (install, expected) in [
            ("pnpm install", "pnpm install --frozen-lockfile"),
            ("npm install", "npm ci"),
            ("yarn install", "yarn install --immutable"),
            ("uv sync", "uv sync --frozen"),
            ("bundle install", "BUNDLE_FROZEN=true bundle install"),
        ] {
            let fx = fixture();
            write_project_config(&fx, &format!("[project]\ninstall = {install:?}\n"));
            let report = report(&fx);
            assert!(!report.healthy(), "{install} should be a problem");
            let fix = report
                .findings
                .iter()
                .find(|f| f.message.contains("non-frozen install"))
                .and_then(|f| f.fix.clone())
                .unwrap_or_default();
            assert!(fix.contains(expected), "{install}: {fix}");
        }
    }

    #[test]
    fn a_frozen_install_and_a_command_pando_has_no_opinion_about_are_both_fine() {
        for install in [
            "pnpm install --frozen-lockfile",
            "npm ci",
            "BUNDLE_FROZEN=true bundle install",
            "make setup",
            "cd apps/web && pnpm install --frozen-lockfile",
        ] {
            let fx = fixture();
            write_project_config(&fx, &format!("[project]\ninstall = {install:?}\n"));
            let report = report(&fx);
            assert!(report.healthy(), "{install}: {:?}", report.findings);
        }
    }

    #[test]
    fn a_chained_install_whose_second_step_is_not_frozen_is_still_caught() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[project]\ninstall = \"corepack enable && pnpm install\"\n",
        );
        let report = report(&fx);
        assert!(!report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn a_port_placeholder_naming_a_role_nothing_owns_is_a_problem() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[processes.web]\ncmd = \"serve --port {port:web}\"\nports = [\"web\"]\n\
             env = { API = \"http://localhost:{port:api}\" }\n",
        );
        let report = report(&fx);
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(mentions(&report, "env.API"), "{:?}", messages(&report));
        assert!(mentions(&report, "{port:api}"), "{:?}", messages(&report));
    }

    #[test]
    fn a_hooks_command_is_checked_too_and_a_hook_owns_no_role_of_its_own() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[processes.dev]\ncmd = \"serve\"\nports = [\"web\"]\n\n\
             [[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
             cmd = \"migrate --at {port:db}\"\n",
        );
        let report = report(&fx);
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "hook \"migrate\": cmd cannot be resolved"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_process_cwd_that_cannot_be_resolved_is_caught_with_its_command() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[processes.dev]\ncmd = \"serve\"\nports = [\"web\"]\ncwd = \"apps/{nope}\"\n",
        );
        let report = report(&fx);
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "cwd cannot be resolved"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_placeholder_naming_a_service_resolves_because_a_service_is_a_role_too() {
        let fx = fixture();
        write_compose(
            &fx,
            "services:\n  postgres:\n    image: postgres:16\n    healthcheck:\n      \
             test: [\"CMD\", \"true\"]\n",
        );
        write_project_config(
            &fx,
            "[processes.web]\ncmd = \"serve\"\nports = [\"web\"]\n\
             env = { DB = \"postgres://localhost:{port:postgres}\" }\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\"]\n",
        );
        let report = report(&fx);
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn the_project_section_reports_the_home_mode_and_the_port_window() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        std::fs::set_permissions(&fx.home, std::fs::Permissions::from_mode(0o755))
            .expect("chmod home");
        let report = report(&fx);
        assert_eq!(report.project.home_mode.as_deref(), Some("755"));
        assert!(mentions(&report, "mode 755"), "{:?}", messages(&report));
        assert!(report.healthy(), "a loose home is a note, not a problem");
        assert_eq!(report.project.port_min, ports::PORT_MIN);
        assert_eq!(report.project.port_max, ports::PORT_MAX);
        assert_eq!(report.project.windows_held, 0);
    }

    // ---- tools ------------------------------------------------------

    fn tool(report: &Report, name: &str) -> ToolReport {
        report
            .tools
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("no {name} in {:?}", report.tools))
            .clone()
    }

    #[test]
    fn a_missing_tool_is_a_line_and_only_a_problem_when_this_project_needs_it() {
        let fx = fixture();
        let report = report_of(&fx, &no_tools);
        // git is not optional: worktrees are git's.
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "git is not on the PATH"),
            "{:?}",
            messages(&report)
        );
        // cloudflared is, and `share` says so itself.
        assert!(!tool(&report, "cloudflared").found);
        assert!(
            !mentions(&report, "cloudflared is not on the PATH"),
            "{:?}",
            messages(&report)
        );
        // Every tool still gets a line, whether or not it is there.
        let text = report.render();
        assert!(text.contains("cloudflared"), "{text}");
        assert!(text.contains("`pando share`"), "{text}");
    }

    #[test]
    fn docker_missing_is_a_note_only_when_the_project_declares_compose_services() {
        let fx = fixture();
        let plain = report_of(&fx, &no_tools);
        assert!(
            !mentions(&plain, "docker is not on the PATH"),
            "{:?}",
            messages(&plain)
        );
        write_project_config(
            &fx,
            "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\"]\n",
        );
        let with_services = report_of(&fx, &no_tools);
        let docker = with_services
            .findings
            .iter()
            .find(|f| f.message.starts_with("docker is not on the PATH"))
            .unwrap_or_else(|| panic!("{:?}", messages(&with_services)));
        // A note, not a problem: a plain `start` still works, and only
        // `--isolated` does not.
        assert_eq!(docker.severity, Severity::Note);
        assert!(
            docker.message.contains("a plain `start` still can"),
            "{}",
            docker.message
        );
    }

    #[test]
    fn a_tool_reports_the_path_it_resolved_from_and_anything_else_worth_a_line() {
        let fx = fixture();
        let shell = |script: &str| -> Option<String> {
            if !script.contains(TOOL_DONE_MARK) {
                return Some("pando-runtime-ok\n".to_string());
            }
            Some(format!(
                "{TOOL_PATH_MARK}1 /usr/local/bin/docker\n\
                 {TOOL_VERSION_MARK}1 Docker version 27.0.3, build 1234\n\
                 {TOOL_DETAIL_MARK}1 desktop-linux\n\
                 {TOOL_DONE_MARK}\n"
            ))
        };
        let report = report_of(&fx, &shell);
        let docker = tool(&report, "docker");
        assert!(docker.found);
        assert_eq!(docker.path.as_deref(), Some("/usr/local/bin/docker"));
        assert_eq!(
            docker.version.as_deref(),
            Some("Docker version 27.0.3, build 1234")
        );
        assert_eq!(docker.detail.as_deref(), Some("context: desktop-linux"));
        let text = report.render();
        assert!(text.contains("/usr/local/bin/docker"), "{text}");
        assert!(text.contains("context: desktop-linux"), "{text}");
    }

    #[test]
    fn the_program_a_config_tells_pando_to_run_is_a_problem_when_it_is_missing() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[project]\ninstall = \"pnpm install --frozen-lockfile\"\n",
        );
        let report = report_of(&fx, &no_tools);
        assert!(!report.healthy());
        assert!(
            mentions(&report, "pnpm is not on the PATH"),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "the install step"),
            "and why pando wanted it: {:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_probe_that_never_finished_claims_nothing_about_the_machine() {
        let fx = fixture();
        // No done mark: the shell died, or the prelude in front of it did.
        let shell = |_: &str| Some("bash: line 1: nvm: command not found\n".to_string());
        let report = report_of(&fx, &shell);
        assert!(
            report.healthy(),
            "a shell that did not answer is not evidence that git is missing: {:?}",
            report.findings
        );
        assert!(
            mentions(&report, "could not ask this shell what it has"),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "command not found"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn the_tool_script_runs_behind_the_prelude_a_real_spawn_would_use() {
        let probes = vec![ToolProbe {
            name: "git".to_string(),
            program: "git".to_string(),
            args: "--version",
            detail_args: None,
            detail_label: "",
            needed_for: "worktrees".to_string(),
            missing: None,
        }];
        let script = tool_script(&probes, "nvm use 22");
        assert!(script.starts_with("nvm use 22 && {"), "{script}");
        assert!(script.contains("command -v 'git'"), "{script}");
        assert!(tool_script(&probes, "").starts_with("if "));
    }

    #[test]
    fn a_program_name_comes_from_the_last_step_of_a_chained_command() {
        assert_eq!(command_program("pnpm dev").as_deref(), Some("pnpm"));
        assert_eq!(
            command_program("corepack enable && pnpm install").as_deref(),
            Some("pnpm")
        );
        assert_eq!(
            command_program("BUNDLE_FROZEN=true bundle install").as_deref(),
            Some("bundle")
        );
        // A path or a template is not a name worth asking about, and is
        // exactly the shape that would put something odd on a command line.
        assert_eq!(command_program("./scripts/setup.sh"), None);
        assert_eq!(command_program("{worktree}/run"), None);
    }

    // ---- runtime ----------------------------------------------------

    /// A shell that resolves node, for the tests about what this machine
    /// answers. The marks are `runtime`'s own: a fake that stopped
    /// matching them would read as "could not parse" and fail loudly.
    fn shell_resolving_node<'a>(
        version: &'a str,
        path: &'a str,
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |script: &str| {
            // Tools are not what these tests are about, so they are all
            // there: a missing git would be a problem in every one of them.
            if script.contains(TOOL_DONE_MARK) {
                return every_tool(script);
            }
            Some(format!(
                "pando-runtime-path:{path}\npando-runtime-version:v{version}\n\
                 pando-runtime-ok\n"
            ))
        }
    }

    fn pin_node(fx: &Fx, version: &str) {
        std::fs::write(fx.root.join(".nvmrc"), format!("{version}\n")).expect("write .nvmrc");
    }

    #[test]
    fn a_runtime_the_shell_resolves_says_nothing_and_still_shows_its_working() {
        let fx = fixture();
        pin_node(&fx, "22");
        let report = report_of(&fx, &shell_resolving_node("22.14.0", "/n/bin/node"));
        assert!(report.healthy(), "{:?}", report.findings);
        let node = &report.runtime.languages[0];
        assert_eq!(node.verdict, "satisfied");
        assert_eq!(node.spec, "22");
        assert_eq!(node.source, ".nvmrc");
        assert_eq!(node.resolved.as_deref(), Some("22.14.0"));
        assert_eq!(node.resolved_from.as_deref(), Some("/n/bin/node"));
        let text = report.render();
        assert!(text.contains("wants 22 (.nvmrc)"), "{text}");
        assert!(
            text.contains("/n/bin/node"),
            "the path it resolved from is the diagnosis:\n{text}"
        );
    }

    #[test]
    fn a_mismatch_nobody_has_been_asked_about_is_a_note_with_the_question_coming() {
        let fx = fixture();
        pin_node(&fx, "22");
        let report = report_of(&fx, &shell_resolving_node("24.21.0", "/n/bin/node"));
        assert!(
            report.healthy(),
            "the next start asks; that is designed, not broken: {:?}",
            report.findings
        );
        assert!(
            mentions(&report, "nobody has answered the prelude question"),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "24.21.0, from /n/bin/node"),
            "{:?}",
            messages(&report)
        );
        assert_eq!(report.runtime.languages[0].verdict, "mismatch");
    }

    #[test]
    fn a_mismatch_with_a_prelude_that_says_nothing_is_needed_is_a_problem() {
        let fx = fixture();
        pin_node(&fx, "22");
        write_project_config(&fx, "[runtime]\nprelude = \"\"\n");
        let report = report_of(&fx, &shell_resolving_node("24.21.0", "/n/bin/node"));
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "which says this machine needs nothing"),
            "{:?}",
            messages(&report)
        );
    }

    #[test]
    fn a_mismatch_with_a_prelude_set_names_the_prelude_and_the_file_it_is_in() {
        let fx = fixture();
        pin_node(&fx, "22");
        write_project_config(&fx, "[runtime]\nprelude = \"nvm use 18\"\n");
        let report = report_of(&fx, &shell_resolving_node("24.21.0", "/n/bin/node"));
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(
            mentions(&report, "\"nvm use 18\""),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, &fx.paths.config_file().display().to_string()),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "is not working"),
            "{:?}",
            messages(&report)
        );
        assert_eq!(
            report.runtime.prelude_from.as_deref(),
            Some(fx.paths.config_file().display().to_string().as_str())
        );
    }

    #[test]
    fn a_mismatch_offers_the_prelude_of_a_manager_this_machine_really_has() {
        let fx = fixture();
        pin_node(&fx, "22");
        // Under the *injected* machine home, so what this laptop has
        // installed never decides the assertion.
        std::fs::create_dir_all(fx.machine_home.join(".nvm")).expect("nvm dir");
        std::fs::write(fx.machine_home.join(".nvm/nvm.sh"), "# fake\n").expect("nvm.sh");
        let report = report_of(&fx, &shell_resolving_node("24.21.0", "/n/bin/node"));
        let node = &report.runtime.languages[0];
        assert!(node.managers.contains(&"nvm"), "{:?}", node.managers);
        let from_home = node
            .fixes
            .iter()
            .find(|fix| fix.contains(&fx.machine_home.display().to_string()))
            .expect("a fix built from the injected home");
        assert!(from_home.contains("nvm use"), "{from_home}");
        let fix = report
            .findings
            .iter()
            .find(|f| f.section == Section::Runtime)
            .and_then(|f| f.fix.clone())
            .unwrap_or_default();
        assert!(fix.contains("set [runtime].prelude to one of:"), "{fix}");
        assert!(fix.contains("nvm"), "{fix}");
    }

    #[test]
    fn a_requirement_with_no_language_behind_it_is_still_reported() {
        let fx = fixture();
        std::fs::write(
            fx.root.join("package.json"),
            "{\"engines\": {\"pnpm\": \">=9\"}}\n",
        )
        .expect("write package.json");
        let report = report(&fx);
        assert!(
            report
                .runtime
                .requirements
                .iter()
                .any(|r| r.language == "pnpm" && r.spec == ">=9"),
            "{:?}",
            report.runtime.requirements
        );
        let text = report.render();
        assert!(text.contains("nothing here probes it"), "{text}");
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn a_repository_that_pins_nothing_says_so_and_probes_no_language() {
        let fx = fixture();
        let report = report(&fx);
        assert!(report.runtime.languages.is_empty());
        assert!(
            report.render().contains("states no runtime"),
            "{}",
            report.render()
        );
    }

    // ---- services ---------------------------------------------------

    fn services_config(include: &str) -> String {
        format!(
            "[[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\ninclude = [{include}]\n"
        )
    }

    #[test]
    fn a_service_without_a_healthcheck_is_flagged_as_connect_probed() {
        let fx = fixture();
        write_compose(
            &fx,
            "services:\n  postgres:\n    image: postgres:16\n  redis:\n    image: redis:7\n    \
             healthcheck:\n      test: [\"CMD\", \"redis-cli\", \"ping\"]\n",
        );
        write_project_config(&fx, &services_config("\"postgres\", \"redis\""));
        let report = report(&fx);
        let entry = &report.services.compose[0];
        let postgres = entry
            .services
            .iter()
            .find(|s| s.name == "postgres")
            .unwrap();
        let redis = entry.services.iter().find(|s| s.name == "redis").unwrap();
        assert_eq!(postgres.ready, "connect");
        assert_eq!(redis.ready, "healthcheck");
        assert!(
            mentions(&report, "\"postgres\" declares no healthcheck"),
            "{:?}",
            messages(&report)
        );
        assert!(
            !mentions(&report, "\"redis\" declares no healthcheck"),
            "{:?}",
            messages(&report)
        );
        // A weaker probe is a note: it works, it is just less sure.
        assert!(report.healthy(), "{:?}", report.findings);
        assert!(
            report.render().contains("ready by connect"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn a_compose_file_pando_could_not_follow_whole_says_which_key_stopped_it() {
        let fx = fixture();
        write_compose(
            &fx,
            "include:\n  - ./other.yml\nservices:\n  db:\n    extends:\n      file: base.yml\n      \
             service: db\n",
        );
        write_project_config(&fx, &services_config("\"db\""));
        let report = report(&fx);
        let entry = &report.services.compose[0];
        assert_eq!(entry.extends, vec!["db".to_string()]);
        assert!(entry.include);
        assert!(mentions(&report, "`extends:`"), "{:?}", messages(&report));
        assert!(
            mentions(&report, "top-level `include:`"),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "does not follow"),
            "{:?}",
            messages(&report)
        );
        // Reported, not fixed.
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn a_compose_file_that_is_not_there_is_a_problem() {
        let fx = fixture();
        write_project_config(&fx, &services_config("\"postgres\""));
        let report = report(&fx);
        assert!(!report.healthy());
        assert!(
            mentions(&report, "is not in this repository"),
            "{:?}",
            messages(&report)
        );
        assert!(!report.services.compose[0].file_exists);
    }

    #[test]
    fn a_service_the_compose_file_does_not_declare_is_a_problem() {
        let fx = fixture();
        write_compose(&fx, "services:\n  postgres:\n    image: postgres:16\n");
        write_project_config(&fx, &services_config("\"postgres\", \"mysql\""));
        let report = report(&fx);
        assert!(!report.healthy());
        assert!(
            mentions(&report, "`include` names the service \"mysql\""),
            "{:?}",
            messages(&report)
        );
    }

    /// A shell that finds every engine binary a native recipe asks about
    /// and lets the tools probe through unchanged.
    ///
    /// Injected for the same reason the tools shell is: this laptop
    /// really does have PostgreSQL installed, and a test that asked the
    /// real PATH would pass here and fail on a runner without it.
    fn every_tool_and_engine(script: &str) -> Option<String> {
        engine_answer(script, true).or_else(|| every_tool(script))
    }

    /// The same shell on a machine with no engine at all.
    fn no_engine(script: &str) -> Option<String> {
        engine_answer(script, false).or_else(|| every_tool(script))
    }

    fn engine_answer(script: &str, found: bool) -> Option<String> {
        if !script.contains(NATIVE_BIN_MARK) {
            return None;
        }
        let mut out = String::new();
        if found {
            for line in script.lines() {
                let Some(rest) = line.split("command -v ").nth(1) else {
                    continue;
                };
                let Some(name) = rest.split('\'').nth(1) else {
                    continue;
                };
                let _ = writeln!(out, "{NATIVE_BIN_MARK}{name} /usr/local/bin/{name}");
            }
            let _ = writeln!(
                out,
                "{NATIVE_VERSION_MARK}postgres (PostgreSQL) 16.10 (Homebrew)"
            );
        }
        Some(out)
    }

    fn native_of(report: &Report, name: &str) -> NativeServiceReport {
        report
            .services
            .native
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("no native report for {name}"))
            .clone()
    }

    #[test]
    fn a_native_service_says_which_recipe_it_resolved_to_and_from_where() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             env = { DATABASE_URL = \"postgres\" }\n",
        );
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "postgres");
        assert_eq!(native.preset, "postgres", "a preset defaults to the name");
        assert_eq!(native.source.as_deref(), Some("built-in"));
        assert!(native.overrides.is_empty());
        assert_eq!(native.env_key.as_deref(), Some("DATABASE_URL"));
        assert!(native.error.is_none());
        // Where its data goes, with the worktree left as a placeholder,
        // and under pando's home rather than in the repository.
        assert!(
            native.datadir.ends_with("data/<worktree>/postgres"),
            "{native:?}"
        );
        assert!(native.datadir.starts_with(&fx.home.display().to_string()));
        // And where the sockets go, which is the one thing that is not
        // under the home — because the home's own path is what overflows
        // `sun_path`.
        assert!(
            !native
                .socket_root
                .starts_with(&fx.home.display().to_string())
        );

        assert_eq!(
            native
                .engine
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            vec!["postgres", "initdb", "pg_isready", "psql", "createdb"]
        );
        assert!(native.engine.iter().all(|b| b.path.is_some()));
        assert_eq!(
            native.version.as_deref(),
            Some("postgres (PostgreSQL) 16.10 (Homebrew)")
        );

        let text = report.render();
        assert!(text.contains("the \"postgres\" recipe, built-in"), "{text}");
        assert!(
            text.contains("postgres at /usr/local/bin/postgres"),
            "{text}"
        );
        assert!(text.contains("addressed by DATABASE_URL"), "{text}");
        // The authentication choice, said out loud rather than buried in
        // a recipe nobody reads.
        assert!(text.contains("trust authentication on 127.0.0.1"), "{text}");
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn an_engine_this_machine_does_not_have_is_a_line_and_not_a_crash() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n",
        );
        let report = report_of(&fx, &no_engine);
        let native = native_of(&report, "postgres");
        assert!(native.engine.iter().all(|b| b.path.is_none()));
        assert_eq!(
            native.version, None,
            "a version from a binary that is not there"
        );
        assert!(mentions(&report, "not on PATH"), "{:?}", messages(&report));
        let text = report.render();
        assert!(text.contains("postgres is not on PATH"), "{text}");
        assert!(text.contains("brew install postgresql"), "{text}");
        assert!(text.contains("pando never will"), "{text}");
        assert!(!report.healthy());
    }

    #[test]
    fn a_users_own_recipe_is_named_by_its_path_and_a_broken_one_is_a_problem() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n",
        );
        let recipes = fx.paths.recipes_dir();
        std::fs::create_dir_all(&recipes).unwrap();
        std::fs::write(
            recipes.join("postgres.toml"),
            "kind = \"service\"\nname = \"postgres\"\nbinaries = [\"postgres\"]\n\n\
             [service]\ncmd = \"mine -p {port}\"\n",
        )
        .unwrap();
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "postgres");
        assert_eq!(
            native.source.as_deref(),
            Some(recipes.join("postgres.toml").display().to_string().as_str()),
            "a replaced built-in has to say which file replaced it"
        );

        // And a file that does not parse is a problem, not a silent
        // fallback to the built-in it shadows.
        std::fs::write(recipes.join("postgres.toml"), "kind = \"service\"\nname =").unwrap();
        let report = report_of(&fx, &every_tool_and_engine);
        assert!(
            mentions(&report, "does not load"),
            "{:?}",
            messages(&report)
        );
        assert!(
            messages(&report)
                .iter()
                .any(|m| m.contains("postgres.toml")),
            "{:?}",
            messages(&report)
        );
        assert!(native_of(&report, "postgres").error.is_some());
        assert!(!report.healthy());
    }

    #[test]
    fn a_preset_no_recipe_answers_to_is_a_problem_naming_the_ones_there_are() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"db\"\npreset = \"cassandra\"\n",
        );
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "db");
        assert!(
            native.error.as_deref().unwrap().contains("cassandra"),
            "{native:?}"
        );
        assert!(
            mentions(&report, "no recipe named \"cassandra\""),
            "{:?}",
            messages(&report)
        );
        assert!(mentions(&report, "postgres"), "{:?}", messages(&report));
        assert!(!report.healthy());
    }

    #[test]
    fn an_entry_that_overrides_the_recipe_says_which_fields_it_overrode() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n\
             ready = \"pg_isready -p {port}\"\nready_timeout_s = 120\n",
        );
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "postgres");
        assert_eq!(native.overrides, vec!["ready", "ready_timeout_s"]);
        assert!(
            report
                .render()
                .contains("(ready, ready_timeout_s from this entry)"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn a_worktree_that_already_has_data_is_listed_with_where_it_is() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[services]]\nkind = \"native\"\nname = \"postgres\"\n",
        );
        let datadir = fx.paths.service_data_dir("feat+one", "postgres");
        std::fs::create_dir_all(&datadir).unwrap();
        // No marker yet: a directory pando did not initialise is one it
        // would adopt, and saying so is the point.
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "postgres");
        assert_eq!(native.instances.len(), 1);
        assert_eq!(native.instances[0].worktree, "feat+one");
        assert_eq!(native.instances[0].initialised, None);
        assert!(
            report
                .render()
                .contains("the next start adopts it as it is")
        );

        std::fs::write(
            crate::native::marker_file(&datadir),
            "recipe = \"postgres\"\nat = \"2026-09-21T10:00:00Z\"\nadopted = false\n",
        )
        .unwrap();
        let report = report_of(&fx, &every_tool_and_engine);
        let native = native_of(&report, "postgres");
        assert_eq!(
            native.instances[0].initialised.as_deref(),
            Some("initialised 2026-09-21")
        );
        assert_eq!(
            native.instances[0].socket_dir,
            fx.paths
                .service_socket_dir("feat+one", "postgres")
                .display()
                .to_string()
        );
    }

    #[test]
    fn a_compose_service_that_shares_a_name_with_a_role_is_reported_before_the_question() {
        let fx = fixture();
        write_compose(
            &fx,
            "services:\n  api:\n    image: kong:3\n  postgres:\n    image: postgres:16\n",
        );
        write_project_config(
            &fx,
            "[processes.dev]\ncmd = \"serve\"\nports = { WEB_PORT = \"web\", API_PORT = \"api\" }\n",
        );
        let report = report(&fx);
        assert!(
            mentions(&report, "declares a service called \"api\""),
            "{:?}",
            messages(&report)
        );
        assert!(
            mentions(&report, "already owns the role \"api\""),
            "{:?}",
            messages(&report)
        );
        assert!(
            !mentions(&report, "\"postgres\""),
            "only the one that collides: {:?}",
            messages(&report)
        );
        // A note: nothing is broken until somebody answers the question,
        // and answering it is now refused.
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn no_collision_is_reported_for_a_service_the_config_already_includes() {
        let fx = fixture();
        write_compose(&fx, "services:\n  postgres:\n    image: postgres:16\n");
        write_project_config(
            &fx,
            "[processes.dev]\ncmd = \"serve\"\nports = [\"web\"]\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\"]\n",
        );
        let report = report(&fx);
        assert!(
            !mentions(&report, "already owns the role"),
            "{:?}",
            messages(&report)
        );
    }

    // ---- hooks ------------------------------------------------------

    #[test]
    fn a_hook_whose_globs_match_nothing_is_flagged_as_running_every_start() {
        let fx = fixture();
        std::fs::create_dir_all(fx.root.join("prisma/migrations")).expect("migrations dir");
        write_project_config(
            &fx,
            "[[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
             fingerprint = [\"prisma/migrations\"]\ncmd = \"true\"\n",
        );
        let report = report(&fx);
        assert_eq!(report.hooks[0].matches, Some(0));
        assert!(
            mentions(&report, "matches nothing in this worktree"),
            "{:?}",
            messages(&report)
        );
        // The shape of the mistake, named: a glob matches files.
        assert!(
            mentions(&report, "prisma/migrations/**"),
            "{:?}",
            messages(&report)
        );
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn a_hook_keyed_on_files_that_are_there_counts_them_and_says_nothing() {
        let fx = fixture();
        std::fs::create_dir_all(fx.root.join("prisma/migrations")).expect("migrations dir");
        std::fs::write(fx.root.join("prisma/migrations/001.sql"), "select 1;\n").expect("sql");
        write_project_config(
            &fx,
            "[[hooks]]\nname = \"migrate\"\nafter = \"services\"\n\
             fingerprint = [\"prisma/migrations/**\"]\ncmd = \"true\"\n",
        );
        let report = report(&fx);
        assert_eq!(report.hooks[0].matches, Some(1));
        assert_eq!(report.hooks[0].after, "services");
        assert!(report.healthy(), "{:?}", report.findings);
        assert!(
            report.render().contains("matching 1 file"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn a_hook_with_no_fingerprint_at_all_is_a_fact_and_not_a_finding() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[[hooks]]\nname = \"seed\"\nafter = \"dev\"\ncmd = \"true\"\n",
        );
        let report = report(&fx);
        assert_eq!(report.hooks[0].matches, None);
        assert!(report.healthy(), "{:?}", report.findings);
        assert!(
            report
                .render()
                .contains("keyed on nothing, so it runs on every start"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn the_schema_question_with_no_way_to_say_no_is_reported_once() {
        let fx = fixture();
        // Two candidates and nothing to choose between them is what makes
        // the slot undecided, and it has no empty form to record.
        std::fs::write(
            fx.root.join("package.json"),
            "{\"scripts\": {\"migrate\": \"x\", \"db:migrate\": \"y\"}}\n",
        )
        .expect("manifest");
        std::fs::create_dir_all(fx.root.join("prisma")).expect("prisma");
        std::fs::write(fx.root.join("prisma/schema.prisma"), "// schema\n").expect("schema");
        let report = report(&fx);
        let hits = messages(&report)
            .iter()
            .filter(|m| m.contains("no way to record"))
            .count();
        assert!(hits <= 1, "said at most once: {:?}", messages(&report));
    }

    // ---- worktrees --------------------------------------------------

    /// A state file, written through the real types: doctor reads state
    /// and never writes it, so a fixture may — and building it from
    /// `state::State` means a field that moves breaks here loudly instead
    /// of parsing into nothing.
    fn write_state(fx: &Fx, store: &state::State) {
        let path = fx.paths.state_file();
        std::fs::create_dir_all(path.parent().expect("project dir")).expect("mkdir");
        state::save(&path, store).expect("write state");
    }

    fn one_worktree(name: &str, record: state::WorktreeRecord) -> state::State {
        let mut store = state::State::new();
        store.worktrees.insert(name.to_string(), record);
        store
    }

    #[test]
    fn a_worktree_pando_has_a_record_for_that_git_has_forgotten_is_reported() {
        let fx = fixture();
        write_state(
            &fx,
            &one_worktree(
                "feat+one",
                state::WorktreeRecord::new(fx.root.join("gone"), true),
            ),
        );
        let report = report(&fx);
        assert_eq!(report.worktrees.len(), 1);
        assert_eq!(report.worktrees[0].phase, "stopped");
        assert!(report.worktrees[0].created_by_pando);
        assert!(!report.worktrees[0].known_to_git);
        assert!(
            mentions(&report, "git does not list it"),
            "{:?}",
            messages(&report)
        );
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn a_failed_process_is_a_problem_carrying_its_reason_and_a_hint_from_the_log() {
        let fx = fixture();
        let log = fx.paths.log_file("feat+one", "dev");
        std::fs::create_dir_all(log.parent().expect("logs dir")).expect("mkdir");
        std::fs::write(&log, "listen EADDRINUSE: address already in use :::17342\n")
            .expect("write log");
        let mut record = state::WorktreeRecord::new(&fx.root, true);
        record.processes.insert(
            "dev".to_string(),
            state::ProcessRecord {
                pid: 999_999,
                pgid: 999_999,
                started_at: chrono::Utc::now(),
                log_path: log.clone(),
                ready_port: None,
                ready_timeout_s: None,
                observed_ports: Vec::new(),
                swept: false,
                phase: state::Phase::Failed {
                    at: chrono::Utc::now(),
                    reason: "process exited".to_string(),
                },
            },
        );
        write_state(&fx, &one_worktree("feat+one", record));
        let report = report(&fx);
        assert!(!report.healthy(), "{:?}", report.findings);
        assert_eq!(report.worktrees[0].phase, "failed");
        assert!(
            mentions(&report, "the process \"dev\" failed — process exited"),
            "{:?}",
            messages(&report)
        );
        let fix = report
            .findings
            .iter()
            .find(|f| f.section == Section::Worktrees && f.severity == Severity::Problem)
            .and_then(|f| f.fix.clone())
            .unwrap_or_default();
        assert!(fix.contains("17342"), "the classifier's hint: {fix}");
        assert!(fix.contains("pando logs feat+one --source dev"), "{fix}");
    }

    #[test]
    fn a_service_record_the_config_no_longer_includes_is_reported_with_its_volume() {
        let fx = fixture();
        write_compose(&fx, "services:\n  postgres:\n    image: postgres:16\n");
        write_project_config(&fx, &services_config("\"postgres\""));
        let mut record = state::WorktreeRecord::new(&fx.root, true);
        record.isolated = true;
        for (name, port) in [("postgres", 17_001u16), ("mailpit", 17_002)] {
            record.services.push(state::ServiceRecord {
                name: name.to_string(),
                kind: state::ServiceKind::Compose,
                port: Some(port),
                pid: None,
                pgid: None,
                compose_project: None,
            });
        }
        write_state(&fx, &one_worktree("feat+one", record));
        let report = report(&fx);
        let mailpit = report.worktrees[0]
            .services
            .iter()
            .find(|s| s.name == "mailpit")
            .expect("the dropped service");
        assert!(!mailpit.declared);
        assert!(
            mentions(&report, "still has a record for the service \"mailpit\""),
            "{:?}",
            messages(&report)
        );
        assert!(
            !mentions(&report, "record for the service \"postgres\""),
            "the one config still includes is not news: {:?}",
            messages(&report)
        );
        assert!(report.healthy(), "{:?}", report.findings);
    }

    // ---- adoption ---------------------------------------------------

    /// A project folder for a repository that is not where it was: the
    /// shape a move leaves behind, built by hand because making one for
    /// real means moving a repository.
    fn stale_project_folder(fx: &Fx, id: &str, old_root: &Path, worktree: &str) -> PathBuf {
        let dir = fx.home.join("projects").join(id);
        let wt = dir.join("worktrees").join(worktree);
        std::fs::create_dir_all(&wt).expect("worktree dir");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}/.git/worktrees/{worktree}\n", old_root.display()),
        )
        .expect("git marker");
        std::fs::write(dir.join("pando.toml"), "[project]\ninstall = \"true\"\n").expect("config");
        dir
    }

    #[test]
    fn a_project_folder_whose_repository_moved_is_offered_for_adoption() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        stale_project_folder(&fx, &old_id, &gone, "feat+one");
        let report = report(&fx);
        assert_eq!(report.adoption.len(), 1, "{:?}", report.adoption);
        assert_eq!(report.adoption[0].id, old_id);
        assert_eq!(report.adoption[0].worktrees, vec!["feat+one".to_string()]);
        assert_eq!(
            report.adoption[0].old_root.as_deref(),
            Some(gone.display().to_string().as_str())
        );
        let fix = report
            .findings
            .iter()
            .find(|f| f.section == Section::Adoption)
            .and_then(|f| f.fix.clone())
            .unwrap_or_default();
        assert!(
            fix.contains(&format!("`pando doctor --adopt {old_id}`")),
            "{fix}"
        );
        assert!(report.healthy(), "a folder to adopt breaks nothing");
    }

    #[test]
    fn a_folder_whose_repository_is_still_there_is_left_alone() {
        let fx = fixture();
        let other = fx.root.parent().expect("a parent").join("still-here");
        std::fs::create_dir_all(&other).expect("the other checkout");
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        stale_project_folder(&fx, &old_id, &other, "feat+one");
        assert!(
            report(&fx).adoption.is_empty(),
            "two checkouts of one repository is ordinary, and adopting one would steal it"
        );
    }

    #[test]
    fn a_git_marker_that_does_not_name_an_absolute_repository_says_it_does_not_know() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let dir = fx.home.join("projects").join(&old_id);
        let wt = dir.join("worktrees/feat+one");
        std::fs::create_dir_all(&wt).expect("worktree dir");
        std::fs::write(wt.join(".git"), "gitdir: .git/worktrees/feat+one\n").expect("marker");
        let report = report(&fx);
        assert_eq!(report.adoption.len(), 1);
        assert_eq!(
            report.adoption[0].old_root, None,
            "an empty path is not a repository it can name"
        );
        assert!(
            report
                .render()
                .contains("nothing in it says which repository it belonged to"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn a_folder_for_a_differently_named_repository_is_not_this_one_moved() {
        let fx = fixture();
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        stale_project_folder(&fx, "other-project-deadbeef", &gone, "feat+one");
        assert!(report(&fx).adoption.is_empty());
    }

    fn yes(_: &AdoptPlan) -> anyhow::Result<bool> {
        Ok(true)
    }

    #[test]
    fn adopting_moves_the_folder_and_everything_in_it() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        let from = stale_project_folder(&fx, &old_id, &gone, "feat+one");
        let mut record = state::WorktreeRecord::new(from.join("worktrees/feat+one"), true);
        record.ports.insert("web".to_string(), 17_001);
        let path = from.join("state.json");
        state::save(&path, &one_worktree("feat+one", record)).expect("state");

        let adoption = adopt(&fx.paths, &old_id, &yes).expect("adopt");
        assert_eq!(adoption.to, fx.paths.project_dir());
        assert!(!from.exists(), "the old folder is gone");
        assert!(fx.paths.config_file().exists(), "the config came with it");
        assert!(
            fx.paths.project_dir().join("worktrees/feat+one").is_dir(),
            "and so did the worktrees"
        );

        // Every recorded path that pointed inside the old folder points
        // inside the new one, or nothing would find the worktree again.
        assert_eq!(adoption.rewritten, 1);
        let store = state::load(&fx.paths.state_file()).expect("state");
        assert_eq!(
            store.worktrees["feat+one"].path,
            fx.paths.project_dir().join("worktrees/feat+one")
        );
        assert_eq!(store.worktrees["feat+one"].ports["web"], 17_001);
        // And it is not offered a second time.
        assert!(report(&fx).adoption.is_empty());
    }

    #[test]
    fn adopting_refuses_when_this_repository_already_has_a_folder() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        stale_project_folder(&fx, &old_id, &gone, "feat+one");
        write_project_config(&fx, "[project]\ninstall = \"true\"\n");

        let err = adopt(&fx.paths, &old_id, &yes).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("already exists"), "{text}");
        assert!(
            fx.home.join("projects").join(&old_id).is_dir(),
            "and nothing moved"
        );
    }

    #[test]
    fn adopting_refuses_a_repository_that_is_still_where_it_was() {
        let fx = fixture();
        let other = fx.root.parent().expect("a parent").join("still-here");
        std::fs::create_dir_all(&other).expect("the other checkout");
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        stale_project_folder(&fx, &old_id, &other, "feat+one");
        let err = adopt(&fx.paths, &old_id, &yes).unwrap_err();
        assert!(format!("{err:#}").contains("is still there"), "{err:#}");
    }

    #[test]
    fn declining_the_confirmation_moves_nothing() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        let from = stale_project_folder(&fx, &old_id, &gone, "feat+one");
        let err = adopt(&fx.paths, &old_id, &|_| Ok(false)).unwrap_err();
        assert!(format!("{err:#}").contains("nothing was moved"));
        assert!(from.is_dir());
        assert!(!fx.paths.project_dir().exists());
    }

    #[test]
    fn a_state_file_this_build_cannot_read_stops_the_move_before_it_happens() {
        let fx = fixture();
        let old_id = format!("{}-deadbeef", fx.paths.project.display_name);
        let gone = fx.root.parent().expect("a parent").join("somewhere-else");
        let from = stale_project_folder(&fx, &old_id, &gone, "feat+one");
        // The shape an *old* folder really has: a state file from before
        // a version bump. Every path in it names the folder it is in, so
        // a move that could not rewrite them would leave every worktree
        // pointing at a directory that is no longer there — and `--adopt`
        // finds a folder by its old id, so there is nothing to retry.
        std::fs::write(
            from.join("state.json"),
            "{\"version\": 99, \"worktrees\": {}}",
        )
        .expect("state");

        let err = adopt(&fx.paths, &old_id, &yes).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("before the folder moves"), "{text}");
        assert!(from.is_dir(), "and nothing moved");
        assert!(!fx.paths.project_dir().exists());
    }

    #[test]
    fn adopting_something_that_is_not_a_project_folder_says_so() {
        let fx = fixture();
        // `projects/` exists, so a name that is not one directory name is
        // caught by the check that is about that rather than by an
        // accident of the fixture: `..` and `.` are each exactly one
        // component, and either would make the destination a
        // subdirectory of the source.
        std::fs::create_dir_all(fx.paths.projects_dir()).expect("projects dir");
        for id in ["", ".", "..", "a/b", "./x"] {
            let err = adopt(&fx.paths, id, &yes).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains("not a project id"), "{id:?}: {text}");
        }
        let err = adopt(&fx.paths, "no-such-project-0badc0de", &yes).unwrap_err();
        assert!(
            format!("{err:#}").contains("there is no project folder"),
            "{err:#}"
        );
        let err = adopt(&fx.paths, fx.paths.project_id(), &yes).unwrap_err();
        assert!(format!("{err:#}").contains("own project folder"), "{err:#}");
    }

    #[test]
    fn the_report_renders_every_section_even_when_it_has_nothing_to_say() {
        let fx = fixture();
        let text = report(&fx).render();
        for section in Section::ALL {
            assert!(
                text.lines().any(|l| l == section.title()),
                "{} is missing from:\n{text}",
                section.title()
            );
        }
    }
}
