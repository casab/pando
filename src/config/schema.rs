//! The shape of `pando.toml`: every section and what it holds.

use crate::paths::PandoPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "ProjectSection::is_empty")]
    pub project: ProjectSection,
    #[serde(default, skip_serializing_if = "RuntimeSection::is_empty")]
    pub runtime: RuntimeSection,
    #[serde(default, skip_serializing_if = "IsolationSection::is_empty")]
    pub isolation: IsolationSection,
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
    ///
    /// `None` is "nobody has said yet", which detection may answer;
    /// `Some([])` is "no worktree needs a local file of mine", which is an
    /// answer a developer gave and which is never asked about again. The
    /// same distinction `[dev].ports` makes, for the same reason: a
    /// question nothing can record is asked on every `new`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<Vec<String>>,
    /// Where a provisioned path comes from when the main checkout has no
    /// file to link: destination to source, both relative to the
    /// repository root.
    ///
    /// A fresh clone has no `.env` — it is gitignored, so it never arrives
    /// — while `.env.example` is right there, tracked. Seeding from it is
    /// an answer to the provision question, never an automatic behaviour,
    /// and the file that lands in the worktree is always a **copy**: a
    /// symlink to the tracked example would make the worktree's own edits
    /// writes into the repository.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provision_from: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "ProvisionMode::is_default")]
    pub provision_mode: ProvisionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
}

impl ProjectSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The paths to provision, with "nobody has said" and "nothing, on
    /// purpose" both reading as the empty list.
    pub fn provision_paths(&self) -> &[String] {
        self.provision.as_deref().unwrap_or_default()
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

/// How this worktree's private services are run, and whether it runs any.
///
/// Two keys in two layers, the same split `[runtime]` makes. Which
/// mechanism a developer wants is a property of their laptop — whether
/// they have Docker running all day, whether they already have Postgres
/// installed — so `prefer` is written to the user layer and answered once
/// per machine. Whether this project has private services at all is a
/// property of the repository, so `none` is written to the project layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationSection {
    /// `native` or `compose`, spelled the way `[[services]] kind` is,
    /// because the answer literally selects which kind gets written.
    ///
    /// `None` is "nobody has said", and pando then takes what the project
    /// itself declares — a compose file is the project's own statement
    /// about how to run its services, and preferring it keeps the common
    /// path unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<String>,
    /// The recorded form of "this project runs no private services".
    ///
    /// A compose entry can say that with an empty `include`; a native
    /// entry is one service and has nowhere to put it. Without a place to
    /// record the negative the question returns on every isolated start,
    /// with nowhere to answer it but the TOML by hand — the same gap
    /// `ports = []` and `provision = []` each closed for their own slot.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub none: bool,
}

impl IsolationSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Which mechanism this machine asks for, if it asks for one.
    pub fn preferred(&self) -> Option<&str> {
        self.prefer.as_deref()
    }
}

/// The two spellings `[isolation] prefer` takes, which are the two
/// spellings `[[services]] kind` takes.
pub const ISOLATION_KINDS: [&str; 2] = ["compose", "native"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
        /// How long each of these services gets to become ready. Sixty
        /// seconds by default; a database that restores a dump on first
        /// boot needs to be able to say so.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready_timeout_s: Option<u64>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Which starts run it: `isolated`, `always`, or `never`. Unset means
    /// `isolated` for a hook after `services` in a project with services,
    /// and `always` for the rest — see [`HookConfig::scope`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<HookScope>,
}

/// Which starts a hook runs on.
///
/// A hook after `services` is almost always a migration, and on a start
/// that is not isolating the services it runs after are the developer's
/// shared ones: one branch's migrations applied to the database every
/// other worktree uses. So such a hook runs on isolated starts only unless
/// its entry says `on = "always"`. `never` is the recorded "no" to the
/// schema question — the command pando found stays visible, switched off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookScope {
    Isolated,
    Always,
    Never,
}

impl HookScope {
    pub fn as_str(self) -> &'static str {
        match self {
            HookScope::Isolated => "isolated",
            HookScope::Always => "always",
            HookScope::Never => "never",
        }
    }
}

impl HookConfig {
    /// The scope in force: the entry's own `on`, or the default for its
    /// lifecycle point.
    ///
    /// `has_services` is whether the project has any service pando can run
    /// a private copy of. Without one, no start is ever isolated, and the
    /// database a hook after `services` runs against is the only one there
    /// is — an external one the project points at itself. Defaulting that
    /// hook to isolated would mean it never runs at all, so it defaults to
    /// `always`, which is what it did before scopes existed.
    pub fn scope(&self, has_services: bool) -> HookScope {
        self.on.unwrap_or(match self.after {
            HookPoint::Services if has_services => HookScope::Isolated,
            _ => HookScope::Always,
        })
    }

    /// Whether a start that is (or is not) isolating runs this hook, in a
    /// project that has (or has no) services — see [`HookConfig::scope`].
    pub fn runs_on(&self, isolated: bool, has_services: bool) -> bool {
        match self.scope(has_services) {
            HookScope::Always => true,
            HookScope::Isolated => isolated,
            HookScope::Never => false,
        }
    }
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

pub(super) fn expand_tilde(path: &Path) -> PathBuf {
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
pub(super) fn glob_match(pattern: &str, value: &str) -> bool {
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
