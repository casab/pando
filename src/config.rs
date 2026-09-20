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
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use toml::{Table, Value};

use crate::paths::PandoPaths;
use crate::project::ProjectRef;

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
    pub cmd: String,
    #[serde(default, skip_serializing_if = "PortsSpec::is_empty")]
    pub ports: PortsSpec,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl PortsSpec {
    fn is_empty(&self) -> bool {
        match self {
            PortsSpec::List(v) => v.is_empty(),
            PortsSpec::Map(m) => m.is_empty(),
        }
    }

    /// Role names in declaration order, whichever form was written.
    pub fn roles(&self) -> Vec<String> {
        match self {
            PortsSpec::List(v) => v.clone(),
            PortsSpec::Map(m) => m.values().cloned().collect(),
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
    let mut warnings = Vec::new();
    let committed_path = paths.root().join("pando.toml");
    let mut merged = Table::new();

    if let Some(mut table) = read_table(&committed_path, &mut warnings) {
        strip_forbidden_committed_keys(&mut table, &committed_path, &mut warnings);
        merge_tables(&mut merged, table);
    }
    if let Some(table) = read_table(&paths.config_file(), &mut warnings) {
        merge_tables(&mut merged, table);
    }

    let config: Config = Value::Table(merged)
        .try_into()
        .context("parse pando.toml")?;
    let config = normalize(config)?;
    validate(&config, &paths.project)?;
    Ok(Loaded { config, warnings })
}

/// Only ever writes the pando-home copy. A committed `pando.toml` is never
/// touched, which is why this takes `PandoPaths` rather than a path.
pub fn write(paths: &PandoPaths, config: &Config) -> Result<()> {
    paths.ensure_home()?;
    let path = paths.config_file();
    let text = toml::to_string_pretty(config).context("serialize pando.toml")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename tmp → {}", path.display()))?;
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
        let resolved = resolve_for_compare(&expand_tilde(dir));
        if resolved.starts_with(&project.root) {
            bail!(
                "worktrees_dir {} is inside the repository {} — pando never writes into your repository",
                dir.display(),
                project.root.display()
            );
        }
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

/// Canonicalises the deepest existing ancestor and re-appends the rest, so a
/// not-yet-created path still compares correctly against a canonical root —
/// on macOS `/var/...` and `/private/var/...` are the same directory.
fn resolve_for_compare(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&cursor) {
            let mut out = canonical;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(name) = cursor.file_name().map(|n| n.to_os_string()) else {
            return path.to_path_buf();
        };
        suffix.push(name);
        if !cursor.pop() {
            return path.to_path_buf();
        }
    }
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
        assert_eq!(dev.ports.roles(), vec!["web".to_string()]);
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
        assert_eq!(web.ports, PortsSpec::List(vec!["web".into()]));
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
