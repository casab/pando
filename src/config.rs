//! `pando.toml`: load, layer, validate, write.
//!
//! The types mirror the config spec in full even though this phase only
//! consumes `[project]` and `[branches]` — later phases fill fields in
//! rather than restructure.
//!
//! Two layers, highest precedence first:
//!
//! 1. `<pando home>/projects/<id>/pando.toml`, the only file pando writes.
//! 2. `<root>/pando.toml`, if a team chose to commit one. Read-only, and it
//!    may not set `project.root` or `project.worktrees_dir`: a file inside
//!    the repository must never be able to redirect where pando writes.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use toml::{Table, Value};
use toml_edit::{DocumentMut, Item, Table as EditTable};

use crate::paths::PandoPaths;
use crate::project::ProjectRef;

/// The role `share`, the browser-open key, and a readiness rule all default
/// to. Roles are otherwise free strings.
pub const WEB_ROLE: &str = "web";

/// Keys the committed layer may not set, because they decide where pando
/// writes and are machine-specific.
const COMMITTED_FORBIDDEN: [&str; 2] = ["root", "worktrees_dir"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "ProjectSection::is_empty")]
    pub project: ProjectSection,
    #[serde(default, skip_serializing_if = "RuntimeSection::is_empty")]
    pub runtime: RuntimeSection,
    /// Shorthand for a single process named `dev`. Normalised into
    /// `processes` at load time; never both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev: Option<ProcessConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub processes: BTreeMap<String, ProcessConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ProbeConfig>,
    #[serde(default, skip_serializing_if = "BranchesSection::is_empty")]
    pub branches: BranchesSection,
    #[serde(default, skip_serializing_if = "ShareSection::is_empty")]
    pub share: ShareSection,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktrees_dir: Option<PathBuf>,
    /// Default base branch for `new`. `None` means "resolve it from the repo".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Paths linked or copied into each new worktree. Every entry must be
    /// gitignored in the main checkout; `new` refuses otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provision: Vec<String>,
    #[serde(default, skip_serializing_if = "ProvisionMode::is_default")]
    pub provision_mode: ProvisionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
}

impl ProjectSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisionMode {
    #[default]
    Link,
    Copy,
}

impl ProvisionMode {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSection {
    /// Sourced before every command pando runs for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prelude: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_files: Vec<String>,
}

impl RuntimeSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// The command that starts the process. Defaulted rather than required
    /// so that a `[dev]` table holding only `cwd` or `env` — the shape a
    /// developer writes when they want pando to fill the command in — is a
    /// file every other command can still read. `start` is the one that
    /// refuses, by name.
    #[serde(default)]
    pub cmd: String,
    /// Roles this process owns, when it says. `None` is "nobody has said
    /// yet", which detection may answer; `Some([])` is "this process has no
    /// ports", which it may not. A worker with a port it never binds is
    /// reported as failed for the whole of its healthy life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<PortsSpec>,
    /// Relative to the worktree. `None` means the worktree root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<ReadySpec>,
}

/// Roles a process owns. The map form `{ ENV = "role" }` is sugar for the
/// list plus an env template, expanded when a process is started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PortsSpec {
    List(Vec<String>),
    Map(BTreeMap<String, String>),
}

impl Default for PortsSpec {
    fn default() -> Self {
        PortsSpec::List(Vec::new())
    }
}

impl ProcessConfig {
    /// The roles this process owns; none when nothing has said.
    pub fn roles(&self) -> Vec<String> {
        self.ports
            .as_ref()
            .map(PortsSpec::roles)
            .unwrap_or_default()
    }

    /// The environment the map form of `ports` is sugar for.
    pub fn port_env(&self) -> BTreeMap<String, String> {
        self.ports
            .as_ref()
            .map(PortsSpec::env_templates)
            .unwrap_or_default()
    }
}

impl PortsSpec {
    /// Role names in declaration order, whichever form was written.
    ///
    /// Deduplicated: two environment variables may point at the same role
    /// (`PORT` and `NEXT_PUBLIC_PORT` both meaning `web`), and that is one
    /// port, not two.
    pub fn roles(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let names: Vec<String> = match self {
            PortsSpec::List(v) => v.clone(),
            PortsSpec::Map(m) => m.values().cloned().collect(),
        };
        for name in names {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// The environment the map form is sugar for: `ports = { PORT = "web" }`
    /// means `env.PORT = "{port:web}"`.
    ///
    /// Expanded here, once, so the rest of pando only ever sees roles plus
    /// env templates and never has to know which form was written.
    pub fn env_templates(&self) -> BTreeMap<String, String> {
        match self {
            PortsSpec::List(_) => BTreeMap::new(),
            PortsSpec::Map(m) => m
                .iter()
                .map(|(var, role)| (var.clone(), format!("{{port:{role}}}")))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadySpec {
    /// Role whose port must bind. A process with no ports is ready once alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ServiceConfig {
    Compose {
        file: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        include: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    Native {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port_env: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        init: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cmd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready_timeout_s: Option<u64>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    pub name: String,
    pub after: HookPoint,
    /// Globs relative to the worktree; the hook runs when their content hash
    /// changes. Empty means "every start".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fingerprint: Vec<String>,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookPoint {
    Create,
    Install,
    Services,
    Dev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeConfig {
    pub name: String,
    pub cmd: String,
    /// Stderr substring that makes a failure fatal. A non-matching failure is
    /// ignored, so a probe never blocks a project it does not understand.
    #[serde(rename = "match")]
    pub match_: String,
    pub hint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchesSection {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<BranchRule>,
}

impl BranchesSection {
    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRule {
    #[serde(rename = "match")]
    pub match_: String,
    pub base: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Prints a header value injected into proxied requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_cmd: Option<String>,
}

impl ShareSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl Config {
    /// Where worktrees are created: the configured directory if any, else
    /// pando's own. The single helper every caller uses, so a configured
    /// value is honoured by `actions` and the TUI watcher alike.
    pub fn worktrees_dir(&self, paths: &PandoPaths) -> PathBuf {
        match &self.project.worktrees_dir {
            Some(dir) => expand_tilde(dir),
            None => paths.worktrees_dir(),
        }
    }

    /// The base branch `new` forks from for `branch`, if config decides it:
    /// `[branches].rules` first, then `[project].base`. `None` leaves the
    /// choice to the repository's own default.
    pub fn base_for_branch(&self, branch: &str) -> Option<&str> {
        for rule in &self.branches.rules {
            if glob_match(&rule.match_, branch) {
                return Some(&rule.base);
            }
        }
        self.project.base.as_deref()
    }
}

/// A loaded config plus anything pando decided to ignore. There is no
/// `doctor` yet, so the warnings have to reach the caller some other way.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

pub fn load(paths: &PandoPaths) -> Result<Loaded> {
    load_layers(paths, true)
}

/// Everything except pando's own layer.
///
/// A home `pando.toml` that cannot be parsed, deserialised or validated is
/// fatal for the commands that act on it — `new`, `start`, `restart` and
/// the TUI, which can do both. Every other command needs none of it, and
/// `stop` is the one you need most when that file is broken, so those run
/// on what is left.
pub fn load_without_home(paths: &PandoPaths) -> Loaded {
    load_layers(paths, false).unwrap_or_default()
}

fn load_layers(paths: &PandoPaths, use_home: bool) -> Result<Loaded> {
    let mut warnings = Vec::new();
    let committed_path = paths.root().join("pando.toml");
    let home_path = paths.config_file();

    let mut committed = read_table(&committed_path, &mut warnings);
    if let Some(table) = committed.as_mut() {
        strip_forbidden_committed_keys(table, &committed_path, &mut warnings);
    }
    // Strictly, and only when it is wanted: a file pando wrote and cannot
    // parse is not one to carry on past the way a committed one is. The
    // caller decides whether this command can do without it.
    let home = if use_home {
        read_home_table(&home_path)?
    } else {
        None
    };

    // A committed file belongs to the team, and to whichever pando wrote
    // it — a key this build has never heard of is what a *newer* pando's
    // config looks like. Dropping that layer with a warning is the same
    // treatment a file that does not even parse already gets; bricking
    // `pando ls` for everyone who pulled it is not.
    if let Some(table) = &committed
        && let Err(e) = build(table.clone(), &paths.project)
    {
        warnings.push(format!("ignoring {}: {e:#}", committed_path.display()));
        committed = None;
    }

    let mut merged = Table::new();
    if let Some(table) = committed.clone() {
        merge_tables(&mut merged, table);
    }
    if let Some(table) = home.clone() {
        merge_tables(&mut merged, table);
    }

    match build(merged, &paths.project) {
        Ok(config) => Ok(Loaded { config, warnings }),
        // The home layer is pando's own file, so it still fails hard — but
        // when each layer is fine alone and only the merge is not, neither
        // file explains it and both are named.
        Err(e) => {
            let layers_conflict = committed.is_some()
                && home.is_some_and(|table| build(table, &paths.project).is_ok());
            if layers_conflict {
                bail!(
                    "{e:#} — {} and {} cannot both apply",
                    committed_path.display(),
                    home_path.display()
                );
            }
            bail!("{e:#} — in {}", home_path.display())
        }
    }
}

/// One layer on its own: deserialise, normalise, validate. A layer that
/// cannot survive this is not merged into anything.
fn build(table: Table, project: &ProjectRef) -> Result<Config> {
    let config: Config = Value::Table(table).try_into().context("parse pando.toml")?;
    let config = normalize(config)?;
    validate(&config, project)?;
    Ok(config)
}

/// Only ever writes the pando-home copy. A committed `pando.toml` is never
/// touched, which is why this takes `PandoPaths` rather than a path.
///
/// Kept for tests and for writing a config from scratch. No user-facing path
/// calls it: detection answers go through [`patch`], which preserves whatever
/// the developer wrote around them.
pub fn write(paths: &PandoPaths, config: &Config) -> Result<()> {
    paths.ensure_home()?;
    let text = toml::to_string_pretty(config).context("serialize pando.toml")?;
    write_private_atomic(&paths.config_file(), &text)
}

/// Header for a `pando.toml` pando is creating from nothing. Written once, so
/// the first thing a developer opening the file sees is that the comments
/// below are pando's and deleting them costs nothing.
const NEW_FILE_HEADER: &str = "\
# pando.toml — pando's own config for this project.
# Lines marked \"# detected:\" or \"# answered:\" were written by pando.
# Everything here is yours to edit; pando only ever adds keys it is missing.

";

/// The mode pando's config is written with. It can carry a command line, and
/// later phases put service credentials next to it.
const CONFIG_MODE: u32 = 0o600;

/// Why pando wrote a key, rendered as the trailing comment on its line.
///
/// Every value pando puts in the file explains itself, which is what makes a
/// wrong guess one visible edit away rather than hidden state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// A rule decided it, and this is the signal that decided: `# detected:
    /// package.json scripts.dev`.
    Detected(String),
    /// A human answered the question: `# answered: 2026-09-20`.
    Answered,
    /// `--yes` took the first of this many options. Deliberately not
    /// `Detected`: no rule decided this, a flag did, and the file has to
    /// say so or it claims a confidence nothing had.
    TookFirst(usize),
}

impl Note {
    /// Two spaces so the comment does not crowd the value, matching what
    /// `toml_edit` leaves between a value and a hand-written comment.
    fn comment(&self) -> String {
        match self {
            Note::Detected(why) => format!("  # detected: {why}"),
            Note::Answered => format!("  # answered: {}", Utc::now().format("%Y-%m-%d")),
            Note::TookFirst(options) => {
                format!("  # answered: --yes took the first of {options} options")
            }
        }
    }
}

/// Edits the pando-home `pando.toml` in place, preserving comments, key
/// order, and formatting.
///
/// Re-serialising the struct would be simpler and would throw away everything
/// the developer wrote: their comments, their ordering, and any key a newer
/// pando understands and this one does not. So the document is parsed as a
/// document, edited, and written back.
pub fn patch<F>(paths: &PandoPaths, edit: F) -> Result<()>
where
    F: FnOnce(&mut DocumentMut) -> Result<()>,
{
    paths.ensure_home()?;
    let path = paths.config_file();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::Error::from(e).context(format!("read {}", path.display())));
        }
    };
    let mut doc: DocumentMut = match &existing {
        // A file pando cannot parse is a file someone is editing. Overwriting
        // it with a document built from the half of it that parsed would lose
        // their work; refusing costs them one fix.
        Some(text) => text.parse().with_context(|| {
            format!(
                "{} is not valid TOML — fix it, or move it aside, and run again",
                path.display()
            )
        })?,
        None => DocumentMut::new(),
    };
    // What the document looks like before the edit, not what the file's own
    // bytes look like: `toml_edit` normalises some things on the way through
    // (a file with no trailing newline gains one), and comparing against the
    // raw bytes would make an edit that changed nothing rewrite the file
    // anyway.
    let before = doc.to_string();
    edit(&mut doc)?;
    let body = doc.to_string();
    if existing.is_some() && body == before {
        return Ok(());
    }
    // A header parsed into an empty document becomes its *trailing* trivia,
    // so the first table pando adds would land above it. Prepending the text
    // is the one way it stays at the top; every later patch then carries it
    // as the first table's leading comment and preserves it untouched.
    let rendered = match &existing {
        Some(_) => body,
        // Nothing to say and nothing to write: a patch that added no keys to
        // a project with no config leaves it without one.
        None if body.trim().is_empty() => return Ok(()),
        None => format!("{NEW_FILE_HEADER}{body}"),
    };
    // And a file whose bytes already say exactly this is left alone too, so
    // mtime-driven watchers stay quiet and a re-run stays quiet with them.
    if existing.as_deref() == Some(rendered.as_str()) {
        return Ok(());
    }
    write_private_atomic(&path, &rendered)
}

/// Writes one key with the note explaining where it came from. The
/// convenience every detection answer uses.
///
/// `table_path` is the table the key belongs to, for example `["dev"]` or
/// `["project"]`; missing tables are created.
pub fn set_detected(
    paths: &PandoPaths,
    table_path: &[&str],
    key: &str,
    value: impl Into<toml_edit::Value>,
    note: Note,
) -> Result<()> {
    let value = value.into();
    let comment = note.comment();
    patch(paths, move |doc| {
        let target = write_target(doc, table_path);
        let table = ensure_table(doc, &target)?;
        table.insert(key, Item::Value(value));
        if let Some(v) = table.get_mut(key).and_then(Item::as_value_mut) {
            v.decor_mut().set_suffix(comment);
        }
        Ok(())
    })
}

/// Where a key really goes in *this* document.
///
/// `[dev]` is shorthand for `[processes.dev]`, and the two forms may not
/// both appear in one file. A document that already has a `[processes]`
/// table therefore gets the long form: writing the shorthand beside it
/// would produce a file pando's own loader refuses, taking every later
/// command down with it.
fn write_target<'a>(doc: &DocumentMut, path: &'a [&'a str]) -> Vec<&'a str> {
    if path == ["dev"] && doc.as_table().contains_key("processes") {
        return vec!["processes", "dev"];
    }
    path.to_vec()
}

/// The table at `path`, creating any level that is missing. A path that runs
/// into a non-table (`dev = 3`) is an error naming it rather than a silent
/// overwrite of whatever was there.
fn ensure_table<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> Result<&'a mut EditTable> {
    let mut table = doc.as_table_mut();
    for (depth, part) in path.iter().enumerate() {
        let entry = table
            .entry(part)
            .or_insert_with(|| Item::Table(EditTable::new()));
        table = entry.as_table_mut().with_context(|| {
            format!(
                "[{}] in pando.toml is not a table — pando will not overwrite it",
                path[..=depth].join(".")
            )
        })?;
    }
    Ok(table)
}

/// Atomic, and 0600: the temp file is locked down before the rename, so there
/// is never a moment where a world-readable config exists at the final path.
fn write_private_atomic(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(CONFIG_MODE))
        .with_context(|| format!("chmod 0600 {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename tmp → {}", path.display()))?;
    Ok(())
}

/// `[dev]` is shorthand for `processes.dev`; the two forms may not both be
/// present, which is a validation error rather than a merge.
fn normalize(mut config: Config) -> Result<Config> {
    if let Some(dev) = config.dev.take() {
        if !config.processes.is_empty() {
            bail!(
                "[dev] and [processes] may not both be set — [dev] is shorthand for one process named dev"
            );
        }
        config.processes.insert("dev".to_string(), dev);
    }
    Ok(config)
}

pub fn validate(config: &Config, project: &ProjectRef) -> Result<()> {
    if let Some(dir) = &config.project.worktrees_dir {
        // Against the repository root only: the fuller check, which also
        // knows about linked worktrees, needs git and runs once at startup.
        crate::paths::ensure_outside_repository(
            "worktrees_dir",
            &expand_tilde(dir),
            &project.root,
            &[],
        )?;
    }
    for entry in &config.project.provision {
        let path = Path::new(entry);
        if path.is_absolute() {
            bail!("provision path {entry:?} must be relative to the repository root");
        }
        if path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
        {
            bail!("provision path {entry:?} must not escape the repository root");
        }
        if entry.trim().is_empty() {
            bail!("provision paths must not be empty");
        }
    }
    Ok(())
}

/// pando's own layer. A file that is not there is not an error; one that is
/// there and does not parse is.
fn read_home_table(path: &Path) -> Result<Option<Table>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let table =
        toml::from_str::<Table>(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(table))
}

fn read_table(path: &Path, warnings: &mut Vec<String>) -> Option<Table> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<Table>(&text) {
        Ok(t) => Some(t),
        Err(e) => {
            // A broken file — ours or the project's — must not brick every
            // command; it is reported and skipped.
            warnings.push(format!("ignoring {}: {e}", path.display()));
            None
        }
    }
}

fn strip_forbidden_committed_keys(table: &mut Table, path: &Path, warnings: &mut Vec<String>) {
    let Some(Value::Table(project)) = table.get_mut("project") else {
        return;
    };
    for key in COMMITTED_FORBIDDEN {
        if project.remove(key).is_some() {
            warnings.push(format!(
                "ignoring project.{key} in {}: a committed config may not decide where pando writes",
                path.display()
            ));
        }
    }
}

/// Tables merge per key; everything else, arrays of tables included, is
/// replaced whole by the higher layer.
fn merge_tables(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base_table)), Value::Table(over_table)) => {
                merge_tables(base_table, over_table);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    home.join(rest)
}

/// Minimal glob for `[branches].rules`: `*` matches any run of characters,
/// `?` exactly one. Small enough not to be worth a dependency.
fn glob_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    let (mut star, mut star_vi) = (None, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        paths: PandoPaths,
    }

    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(&root).unwrap();
        let project = ProjectRef::from_root(&root).unwrap();
        let paths = PandoPaths::new(dir.path().join("pando-home"), project);
        Fixture {
            root: paths.root().to_path_buf(),
            paths,
            _dir: dir,
        }
    }

    fn write_committed(f: &Fixture, text: &str) {
        std::fs::write(f.root.join("pando.toml"), text).unwrap();
    }

    fn write_home(f: &Fixture, text: &str) {
        std::fs::create_dir_all(f.paths.project_dir()).unwrap();
        std::fs::write(f.paths.config_file(), text).unwrap();
    }

    fn home_text(f: &Fixture) -> String {
        std::fs::read_to_string(f.paths.config_file()).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A file a developer wrote by hand: comments above and beside keys,
    /// tables out of alphabetical order, and a key pando is about to change.
    const HANDWRITTEN: &str = r#"# my project
# two comment lines

[share]
provider = "cloudflared"

[project]
# the base everything forks from
base = "trunk"
install = "npm ci"   # frozen on purpose
provision = [".env"]

[runtime]
prelude = "nvm use"
"#;

    #[test]
    fn a_patch_leaves_every_line_it_did_not_touch_byte_for_byte() {
        let f = fixture();
        write_home(&f, HANDWRITTEN);
        set_detected(
            &f.paths,
            &["project"],
            "install",
            "pnpm install --frozen-lockfile",
            Note::Detected("pnpm-lock.yaml".into()),
        )
        .unwrap();

        let after = home_text(&f);
        let before_lines: Vec<&str> = HANDWRITTEN.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(
            before_lines.len(),
            after_lines.len(),
            "a patch must not add or remove lines:\n{after}"
        );
        for (before, after) in before_lines.iter().zip(&after_lines) {
            if before.starts_with("install") {
                assert_eq!(
                    *after,
                    "install = \"pnpm install --frozen-lockfile\"  # detected: pnpm-lock.yaml",
                    "the patched line carries the new value and its note"
                );
            } else {
                assert_eq!(before, after, "an untouched line changed");
            }
        }
        // And it is still the config pando reads back.
        let loaded = load(&f.paths).unwrap();
        assert_eq!(
            loaded.config.project.install.as_deref(),
            Some("pnpm install --frozen-lockfile")
        );
        assert_eq!(loaded.config.project.base.as_deref(), Some("trunk"));
        assert_eq!(loaded.config.runtime.prelude.as_deref(), Some("nvm use"));
    }

    #[test]
    fn a_new_key_lands_in_its_table_without_disturbing_the_others() {
        let f = fixture();
        write_home(&f, HANDWRITTEN);
        set_detected(
            &f.paths,
            &["runtime"],
            "version_files",
            toml_edit::Array::from_iter([".nvmrc"]),
            Note::Detected(".nvmrc".into()),
        )
        .unwrap();

        let after = home_text(&f);
        assert!(
            after.contains("version_files = [\".nvmrc\"]  # detected: .nvmrc"),
            "{after}"
        );
        assert!(after.contains("prelude = \"nvm use\""), "{after}");
        assert!(
            after.contains("# the base everything forks from"),
            "{after}"
        );
        assert!(after.starts_with("# my project\n"), "{after}");
        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config.runtime.version_files, vec![".nvmrc"]);
    }

    #[test]
    fn a_missing_table_is_created_and_the_file_gets_a_header() {
        let f = fixture();
        set_detected(
            &f.paths,
            &["dev"],
            "cmd",
            "pnpm dev",
            Note::Detected("package.json scripts.dev".into()),
        )
        .unwrap();

        let after = home_text(&f);
        assert!(after.starts_with("# pando.toml"), "{after}");
        assert!(
            after.contains("[dev]\n"),
            "the table is not inline: {after}"
        );
        assert!(
            after.contains("cmd = \"pnpm dev\"  # detected: package.json scripts.dev"),
            "{after}"
        );
        let loaded = load(&f.paths).unwrap();
        assert_eq!(
            loaded.config.processes["dev"].cmd, "pnpm dev",
            "[dev] normalises into processes.dev"
        );
    }

    #[test]
    fn an_answered_note_records_the_date() {
        let f = fixture();
        set_detected(&f.paths, &["dev"], "cmd", "pnpm dev:web", Note::Answered).unwrap();
        let after = home_text(&f);
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(after.contains(&format!("# answered: {today}")), "{after}");
    }

    #[test]
    fn patching_the_same_value_twice_replaces_the_note_and_not_the_file() {
        let f = fixture();
        set_detected(&f.paths, &["dev"], "cmd", "a", Note::Detected("one".into())).unwrap();
        let first = home_text(&f);
        set_detected(&f.paths, &["dev"], "cmd", "b", Note::Answered).unwrap();
        let second = home_text(&f);
        assert!(first.contains("cmd = \"a\"  # detected: one"));
        assert!(second.contains("cmd = \"b\"  # answered:"), "{second}");
        assert!(
            !second.contains("detected: one"),
            "the stale note must go with the stale value: {second}"
        );
    }

    #[test]
    fn the_config_pando_writes_is_private() {
        let f = fixture();
        set_detected(&f.paths, &["dev"], "cmd", "pnpm dev", Note::Answered).unwrap();
        assert_eq!(mode_of(&f.paths.config_file()), 0o600);
        // Writing again over an existing file keeps it that way.
        set_detected(&f.paths, &["dev"], "cwd", "apps/web", Note::Answered).unwrap();
        assert_eq!(mode_of(&f.paths.config_file()), 0o600);
        assert!(
            !f.paths.config_file().with_extension("toml.tmp").exists(),
            "the temp file must be renamed away"
        );
    }

    // The no-op path compares the rendered document with the file's own
    // bytes, so anything `toml_edit` normalises on the way through would
    // make an empty patch rewrite the file. These are the shapes that
    // normalisation would show up in.
    #[test]
    fn an_empty_patch_rewrites_nothing_whatever_the_file_looks_like() {
        for (label, original) in [
            ("no trailing newline", "[project]\nbase = \"main\""),
            ("crlf line endings", "[project]\r\nbase = \"main\"\r\n"),
            (
                "blank lines and indentation",
                "\n\n[project]\n  base = \"main\"\n\n\n",
            ),
            ("comments only", "# nothing but a comment\n"),
            ("an empty file", ""),
        ] {
            let f = fixture();
            write_home(&f, original);
            patch(&f.paths, |_doc| Ok(())).unwrap();
            assert_eq!(home_text(&f), original, "{label}");
        }
    }

    #[test]
    fn patching_never_touches_a_committed_pando_toml() {
        let f = fixture();
        let committed = "[project]\nbase = \"main\"\n";
        write_committed(&f, committed);
        set_detected(
            &f.paths,
            &["project"],
            "install",
            "pnpm install --frozen-lockfile",
            Note::Detected("pnpm-lock.yaml".into()),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(f.root.join("pando.toml")).unwrap(),
            committed,
            "the repository's own file is read-only to pando"
        );
        assert!(f.paths.config_file().starts_with(&f.paths.home));
    }

    #[test]
    fn a_home_file_that_is_not_valid_toml_is_never_overwritten() {
        let f = fixture();
        let broken = "[project\nbase = \"main\"\n";
        write_home(&f, broken);
        let err = set_detected(&f.paths, &["dev"], "cmd", "x", Note::Answered).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid TOML"), "{msg}");
        assert_eq!(
            home_text(&f),
            broken,
            "a file pando cannot parse is a file someone is editing"
        );
    }

    // `[dev]` is shorthand for `[processes.dev]` and the two forms may not
    // both be in one file, so a document that already has a `[processes]`
    // table gets the long form — otherwise pando writes a file it then
    // refuses to read.
    #[test]
    fn a_dev_key_takes_the_long_form_when_the_file_already_has_processes() {
        let f = fixture();
        write_home(&f, "[processes.dev]\ncwd = \"apps/web\"\n");
        set_detected(
            &f.paths,
            &["dev"],
            "cmd",
            "pnpm dev",
            Note::Detected("package.json scripts.dev".into()),
        )
        .unwrap();

        let text = home_text(&f);
        assert!(
            !text.contains("[dev]"),
            "the shorthand beside [processes] is a file pando cannot load: {text}"
        );
        assert!(text.contains("[processes.dev]"), "{text}");
        assert!(text.contains("cmd = \"pnpm dev\""), "{text}");
        let loaded = load(&f.paths).expect("the file pando wrote must load");
        assert_eq!(loaded.config.processes["dev"].cmd, "pnpm dev");
        assert_eq!(
            loaded.config.processes["dev"].cwd.as_deref(),
            Some("apps/web"),
            "and what was already there is untouched"
        );
    }

    #[test]
    fn a_key_whose_table_is_a_scalar_is_refused_by_name() {
        let f = fixture();
        write_home(&f, "dev = 3\n");
        let err = set_detected(&f.paths, &["dev"], "cmd", "x", Note::Answered).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("[dev]"), "{msg}");
        assert_eq!(home_text(&f), "dev = 3\n");
    }

    #[test]
    fn a_patch_that_changes_nothing_leaves_the_file_alone() {
        let f = fixture();
        write_home(&f, HANDWRITTEN);
        let before = std::fs::metadata(f.paths.config_file())
            .unwrap()
            .modified()
            .unwrap();
        patch(&f.paths, |_doc| Ok(())).unwrap();
        assert_eq!(home_text(&f), HANDWRITTEN);
        assert_eq!(
            std::fs::metadata(f.paths.config_file())
                .unwrap()
                .modified()
                .unwrap(),
            before,
            "an empty patch must not rewrite the file"
        );
    }

    #[test]
    fn defaults_load_when_no_file_exists() {
        let f = fixture();
        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
        assert_eq!(
            loaded.config.worktrees_dir(&f.paths),
            f.paths.worktrees_dir()
        );
    }

    #[test]
    fn the_pando_home_layer_overrides_the_committed_layer() {
        let f = fixture();
        write_committed(
            &f,
            "[project]\nbase = \"main\"\ninstall = \"pnpm install --frozen-lockfile\"\n",
        );
        write_home(&f, "[project]\nbase = \"develop\"\n");

        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config.project.base.as_deref(), Some("develop"));
        assert_eq!(
            loaded.config.project.install.as_deref(),
            Some("pnpm install --frozen-lockfile"),
            "keys the home layer does not mention survive the merge"
        );
    }

    #[test]
    fn a_committed_file_cannot_set_root_or_worktrees_dir() {
        let f = fixture();
        let inside = f.root.join("worktrees");
        write_committed(
            &f,
            &format!(
                "[project]\nroot = \"/somewhere/else\"\nworktrees_dir = \"{}\"\nbase = \"main\"\n",
                inside.display()
            ),
        );

        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config.project.root, None);
        assert_eq!(loaded.config.project.worktrees_dir, None);
        assert_eq!(
            loaded.config.project.base.as_deref(),
            Some("main"),
            "the rest of the committed section still applies"
        );
        assert_eq!(
            loaded.warnings.len(),
            2,
            "both keys warn: {:?}",
            loaded.warnings
        );
        assert!(
            loaded
                .warnings
                .iter()
                .all(|w| w.contains("committed config"))
        );
    }

    #[test]
    fn a_worktrees_dir_inside_the_repository_is_refused() {
        let f = fixture();
        write_home(
            &f,
            &format!(
                "[project]\nworktrees_dir = \"{}\"\n",
                f.root.join(".pando-worktrees").display()
            ),
        );
        let err = load(&f.paths).unwrap_err();
        assert!(
            format!("{err:#}").contains("inside the repository"),
            "unexpected error: {err:#}"
        );
    }

    // The repository root is canonical; a configured path that reaches it
    // through a symlinked ancestor (/var vs /private/var on macOS) must be
    // refused just the same.
    #[test]
    fn a_non_canonical_worktrees_dir_inside_the_repository_is_refused() {
        let f = fixture();
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("link-to-root");
        std::os::unix::fs::symlink(&f.root, &link).unwrap();
        write_home(
            &f,
            &format!(
                "[project]\nworktrees_dir = \"{}\"\n",
                link.join("wt").display()
            ),
        );
        let err = load(&f.paths).unwrap_err();
        assert!(
            format!("{err:#}").contains("inside the repository"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn a_worktrees_dir_outside_the_repository_is_accepted() {
        let f = fixture();
        let outside = f.root.parent().unwrap().join("trees");
        write_home(
            &f,
            &format!("[project]\nworktrees_dir = \"{}\"\n", outside.display()),
        );
        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config.worktrees_dir(&f.paths), outside);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let f = fixture();
        write_home(&f, "[project]\nbaze = \"main\"\n");
        let err = load(&f.paths).unwrap_err();
        assert!(
            format!("{err:#}").contains("baze"),
            "the error should name the unknown key: {err:#}"
        );

        // Every table denies unknown fields, including a service variant
        // behind the `kind` tag and a whole unknown section.
        write_home(
            &f,
            "[[services]]\nkind = \"compose\"\nfile = \"c.yml\"\nincldue = [\"db\"]\n",
        );
        assert!(
            load(&f.paths).is_err(),
            "unknown service key must be rejected"
        );

        write_home(&f, "[nonsense]\nkey = 1\n");
        assert!(load(&f.paths).is_err(), "unknown section must be rejected");
    }

    #[test]
    fn dev_and_processes_together_are_an_error() {
        let f = fixture();
        write_home(
            &f,
            "[dev]\ncmd = \"pnpm dev\"\n\n[processes.api]\ncmd = \"node api\"\n",
        );
        let err = load(&f.paths).unwrap_err();
        assert!(
            format!("{err:#}").contains("may not both be set"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn dev_is_shorthand_for_a_process_named_dev() {
        let f = fixture();
        write_home(
            &f,
            "[dev]\ncmd = \"pnpm dev\"\nports = { PORT = \"web\" }\n",
        );
        let loaded = load(&f.paths).unwrap();
        assert!(loaded.config.dev.is_none(), "[dev] is normalised away");
        let dev = loaded.config.processes.get("dev").expect("processes.dev");
        assert_eq!(dev.cmd, "pnpm dev");
        assert_eq!(dev.roles(), vec!["web".to_string()]);
    }

    #[test]
    fn the_map_form_of_ports_is_sugar_for_a_role_plus_an_env_template() {
        let spec = PortsSpec::Map(BTreeMap::from([("PORT".to_string(), "web".to_string())]));
        assert_eq!(spec.roles(), vec!["web".to_string()]);
        assert_eq!(
            spec.env_templates(),
            BTreeMap::from([("PORT".to_string(), "{port:web}".to_string())])
        );

        let list = PortsSpec::List(vec!["web".to_string(), "api".to_string()]);
        assert_eq!(list.roles(), vec!["web".to_string(), "api".to_string()]);
        assert!(
            list.env_templates().is_empty(),
            "the list form puts the port in the command, not the environment"
        );
    }

    // Two variables naming one role is one port: a framework that wants both
    // `PORT` and `NEXT_PUBLIC_PORT` must get the same number in each.
    #[test]
    fn two_env_vars_for_one_role_are_still_one_role() {
        let spec = PortsSpec::Map(BTreeMap::from([
            ("PORT".to_string(), "web".to_string()),
            ("NEXT_PUBLIC_PORT".to_string(), "web".to_string()),
        ]));
        assert_eq!(spec.roles(), vec!["web".to_string()]);
        assert_eq!(spec.env_templates().len(), 2);
    }

    #[test]
    fn ports_accept_both_the_list_and_the_map_form() {
        let f = fixture();
        write_home(
            &f,
            "[processes.web]\ncmd = \"uv run manage.py runserver 127.0.0.1:{port:web}\"\nports = [\"web\"]\n",
        );
        let loaded = load(&f.paths).unwrap();
        let web = loaded.config.processes.get("web").unwrap();
        assert_eq!(web.ports, Some(PortsSpec::List(vec!["web".into()])));
    }

    #[test]
    fn the_full_spec_example_round_trips() {
        let f = fixture();
        write_home(
            &f,
            r#"
[project]
base = "main"
provision = [".env", ".env.local"]
provision_mode = "copy"
install = "pnpm install --frozen-lockfile"

[runtime]
prelude = ""
version_files = [".nvmrc"]

[dev]
cmd = "pnpm dev"
cwd = "."
ports = { PORT = "web" }
env = { NODE_ENV = "development" }
ready = { role = "web", timeout_s = 30 }

[[services]]
kind = "compose"
file = "docker-compose.yml"
include = ["postgres", "redis"]
env = { DATABASE_URL = "postgres", REDIS_URL = "redis" }

[[services]]
kind = "native"
name = "postgres"
preset = "postgres"
port_env = "DATABASE_PORT"
init = "initdb -D {datadir}"
cmd = "postgres -D {datadir} -p {port}"
ready = "pg_isready -h 127.0.0.1 -p {port}"

[[hooks]]
name = "migrate"
after = "services"
fingerprint = ["prisma/migrations/**"]
cmd = "pnpm prisma migrate deploy"

[[probes]]
name = "native-abi"
cmd = "node -e 'require(\"better-sqlite3\")'"
match = "NODE_MODULE_VERSION"
hint = "Rebuild native modules under the dev runtime."

[branches]
rules = [{ match = "*-beta", base = "beta" }]

[share]
provider = "cloudflared"
auth_cmd = "./scripts/dev-cookie.sh"
"#,
        );
        let loaded = load(&f.paths).unwrap();
        let c = &loaded.config;
        assert_eq!(c.project.provision_mode, ProvisionMode::Copy);
        assert_eq!(c.runtime.version_files, vec![".nvmrc".to_string()]);
        assert_eq!(c.services.len(), 2);
        assert!(matches!(c.services[0], ServiceConfig::Compose { .. }));
        assert!(matches!(c.services[1], ServiceConfig::Native { .. }));
        assert_eq!(c.hooks[0].after, HookPoint::Services);
        assert_eq!(c.probes[0].match_, "NODE_MODULE_VERSION");
        assert_eq!(c.share.provider.as_deref(), Some("cloudflared"));
        assert_eq!(c.base_for_branch("fix/thing-beta"), Some("beta"));
        assert_eq!(c.base_for_branch("feat/other"), Some("main"));

        // Serialising and reloading must produce the same value, or `write`
        // would quietly drop fields detection put there.
        let text = toml::to_string_pretty(c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(normalize(back).unwrap(), *c);
    }

    #[test]
    fn arrays_of_tables_are_replaced_whole_not_merged() {
        let f = fixture();
        write_committed(
            &f,
            "[[services]]\nkind = \"compose\"\nfile = \"a.yml\"\ninclude = [\"postgres\"]\n\n[[services]]\nkind = \"compose\"\nfile = \"b.yml\"\n",
        );
        write_home(
            &f,
            "[[services]]\nkind = \"compose\"\nfile = \"only.yml\"\n",
        );

        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config.services.len(), 1);
        assert!(matches!(
            &loaded.config.services[0],
            ServiceConfig::Compose { file, .. } if file == "only.yml"
        ));
    }

    #[test]
    fn provision_paths_must_stay_inside_the_repository() {
        let f = fixture();
        for bad in ["/etc/passwd", "../secrets/.env", ".."] {
            write_home(&f, &format!("[project]\nprovision = [\"{bad}\"]\n"));
            assert!(
                load(&f.paths).is_err(),
                "provision path {bad:?} should be refused"
            );
        }
        write_home(
            &f,
            "[project]\nprovision = [\".env\", \"apps/web/.env.local\"]\n",
        );
        assert!(load(&f.paths).is_ok());
    }

    #[test]
    fn write_only_ever_touches_the_pando_home_copy() {
        let f = fixture();
        let mut config = Config::default();
        config.project.base = Some("main".into());
        write(&f.paths, &config).unwrap();

        assert!(f.paths.config_file().is_file());
        assert!(
            !f.root.join("pando.toml").exists(),
            "write must never create a file inside the repository"
        );
        let entries: Vec<_> = std::fs::read_dir(&f.root).unwrap().collect();
        assert!(entries.is_empty(), "the repository must be untouched");

        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config, config);
        assert!(
            !f.paths.config_file().with_extension("toml.tmp").exists(),
            "the temp file must not leak after the rename"
        );
    }

    #[test]
    fn a_broken_file_is_reported_and_skipped() {
        let f = fixture();
        write_committed(&f, "this is not toml {{{");
        let loaded = load(&f.paths).unwrap();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("ignoring"));
    }

    // A committed file is someone else's work, and often a newer pando's.
    // A key this build does not know, or a value it will not accept, must
    // not stop `pando ls` for everyone who pulled it — the layer is dropped
    // with a warning, exactly as a file that does not even parse already is.
    #[test]
    fn a_committed_file_that_does_not_validate_is_dropped_with_a_warning() {
        let f = fixture();
        for bad in [
            "[dev]\ncmd = \"x\"\n\n[processes.api]\ncmd = \"y\"\n",
            "[project]\nprovision = [\"../shared/.env\"]\n",
            "[project]\nbase = \"main\"\nnope = 1\n",
        ] {
            write_committed(&f, bad);
            let loaded = load(&f.paths).unwrap_or_else(|e| panic!("{bad:?} bricked load: {e:#}"));
            assert_eq!(
                loaded.config,
                Config::default(),
                "the whole layer is dropped: {bad:?}"
            );
            assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
            assert!(
                loaded.warnings[0].contains("ignoring"),
                "{:?}",
                loaded.warnings
            );
        }
    }

    // The home layer is pando's own file, so a problem there is pando's bug
    // or the user's edit, and still fails hard.
    #[test]
    fn a_home_file_that_does_not_validate_still_fails() {
        let f = fixture();
        write_home(&f, "[project]\nbase = \"main\"\nnope = 1\n");
        let err = load(&f.paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "{msg}");
        assert!(
            msg.contains(&f.paths.config_file().display().to_string()),
            "the failing file should be named: {msg}"
        );
    }

    // Each layer is fine on its own and only the merge is not, so neither
    // file explains it alone and both are named.
    #[test]
    fn a_conflict_that_only_appears_after_merging_names_both_files() {
        let f = fixture();
        write_committed(&f, "[dev]\ncmd = \"pnpm dev\"\n");
        write_home(&f, "[processes.api]\ncmd = \"node api\"\n");
        let err = load(&f.paths).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("may not both be set"), "{msg}");
        assert!(
            msg.contains(&f.root.join("pando.toml").display().to_string()),
            "{msg}"
        );
        assert!(
            msg.contains(&f.paths.config_file().display().to_string()),
            "{msg}"
        );
    }

    #[test]
    fn glob_match_handles_the_shapes_branch_rules_use() {
        assert!(glob_match("*-beta", "fix/thing-beta"));
        assert!(!glob_match("*-beta", "fix/betamax"));
        assert!(glob_match("release/*", "release/1.2"));
        assert!(glob_match("v?.?", "v4.1"));
        assert!(!glob_match("v?.?", "v4.11"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn branch_rules_win_over_the_project_base_and_first_match_wins() {
        let mut config = Config::default();
        config.project.base = Some("main".into());
        config.branches.rules = vec![
            BranchRule {
                match_: "release/*".into(),
                base: "release".into(),
            },
            BranchRule {
                match_: "*".into(),
                base: "catch-all".into(),
            },
        ];
        assert_eq!(config.base_for_branch("release/1.2"), Some("release"));
        assert_eq!(config.base_for_branch("feat/x"), Some("catch-all"));
    }
}
