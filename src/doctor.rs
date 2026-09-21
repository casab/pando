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

use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::config::{self, Config, ServiceConfig};
use crate::paths::PandoPaths;
use crate::{detect, ports, state, template};

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
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Project => "project",
            Section::Config => "config",
        }
    }

    /// Every section, in print order.
    const ALL: [Section; 2] = [Section::Project, Section::Config];
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
    if let Some(fix) = &finding.fix {
        let _ = writeln!(out, "      fix: {fix}");
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
fn row(out: &mut String, label: &str, value: &str) {
    let _ = writeln!(out, "  {label:<14}{value}");
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
pub fn run(paths: &PandoPaths) -> Report {
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

    Report {
        project,
        config: config_report,
        findings,
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
        let own = process.roles();
        let mut texts: Vec<(String, String)> = vec![("cmd".to_string(), process.cmd.clone())];
        for (key, value) in process.env.iter().chain(process.port_env().iter()) {
            texts.push((format!("env.{key}"), value.clone()));
        }
        for (what, text) in texts {
            let ctx = template::Context {
                name: "a-worktree",
                branch: Some("a-branch"),
                worktree: Path::new("/worktree"),
                root: Path::new("/root"),
                project: "project",
                ports: &roles,
                default_role: own.first().map(String::as_str),
                log: Some(Path::new("/log")),
            };
            if let Err(e) = template::render(&text, &ctx) {
                findings.push(Finding::problem(
                    Section::Config,
                    format!("process {name:?}: {what} cannot be resolved — {e:#}"),
                    format!(
                        "name a role something owns, or give {name:?} that role in its `ports`"
                    ),
                ));
            }
        }
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
        Fx {
            _dir: dir,
            paths,
            root,
            home,
        }
    }

    fn write_project_config(fx: &Fx, body: &str) {
        let path = fx.paths.config_file();
        std::fs::create_dir_all(path.parent().expect("project dir")).expect("mkdir");
        std::fs::write(&path, body).expect("write config");
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
            let report = run(&fx.paths);
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
            let report = run(&fx.paths);
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
        let report = run(&fx.paths);
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
        let report = run(&fx.paths);
        assert!(!report.healthy(), "{:?}", report.findings);
        assert!(mentions(&report, "env.API"), "{:?}", messages(&report));
        assert!(mentions(&report, "{port:api}"), "{:?}", messages(&report));
    }

    #[test]
    fn a_placeholder_naming_a_service_resolves_because_a_service_is_a_role_too() {
        let fx = fixture();
        write_project_config(
            &fx,
            "[processes.web]\ncmd = \"serve\"\nports = [\"web\"]\n\
             env = { DB = \"postgres://localhost:{port:postgres}\" }\n\n\
             [[services]]\nkind = \"compose\"\nfile = \"docker-compose.yml\"\n\
             include = [\"postgres\"]\n",
        );
        let report = run(&fx.paths);
        assert!(report.healthy(), "{:?}", report.findings);
    }

    #[test]
    fn the_project_section_reports_the_home_mode_and_the_port_window() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        std::fs::set_permissions(&fx.home, std::fs::Permissions::from_mode(0o755))
            .expect("chmod home");
        let report = run(&fx.paths);
        assert_eq!(report.project.home_mode.as_deref(), Some("755"));
        assert!(mentions(&report, "mode 755"), "{:?}", messages(&report));
        assert!(report.healthy(), "a loose home is a note, not a problem");
        assert_eq!(report.project.port_min, ports::PORT_MIN);
        assert_eq!(report.project.port_max, ports::PORT_MAX);
        assert_eq!(report.project.windows_held, 0);
    }

    #[test]
    fn the_report_renders_every_section_even_when_it_has_nothing_to_say() {
        let fx = fixture();
        let text = run(&fx.paths).render();
        for section in Section::ALL {
            assert!(
                text.lines().any(|l| l == section.title()),
                "{} is missing from:\n{text}",
                section.title()
            );
        }
    }
}
