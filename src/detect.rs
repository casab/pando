//! Tier 1: what the repository says about how to run itself.
//!
//! Everything here is read-only over the main checkout, and everything it
//! produces is a *proposal* — a list of candidates a rule found, in rule
//! order, with the reason each one is a candidate. Nothing starts anything.
//! That split is Invariant 2: detection only ever writes `pando.toml`, and
//! the runtime only ever reads it.
//!
//! The rules enumerate; they never invent. A script that is not in
//! `package.json` cannot be proposed, so the worst a wrong rule can do is
//! pick the wrong one of the developer's own commands — one visible edit to
//! a file with a `# detected:` comment next to the line.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::config::{Config, PortsSpec, ProcessConfig, ReadySpec};

// ---- signals --------------------------------------------------------------

/// Everything tier 1 can see. Serialisable because `pando signals` prints it
/// for an agent to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals {
    /// `package.json` scripts: name to body.
    pub scripts: BTreeMap<String, String>,
    /// Makefile or justfile targets: name to the first line of the recipe.
    pub targets: BTreeMap<String, String>,
    /// Lockfiles present at the root, in the order pando checks them.
    pub lockfiles: Vec<String>,
    pub workspace_markers: Vec<String>,
    /// Which files that pin a runtime exist. What they *say* is
    /// `runtime_requirements`; this stays the list `doctor` shows.
    pub version_files: Vec<String>,
    /// What those files, `.tool-versions`, `mise.toml` and `engines` in
    /// `package.json` actually ask for: one entry per (language, spec,
    /// source), a pin before a range.
    ///
    /// Defaulted rather than required, so a dump written by an older pando
    /// still deserialises.
    #[serde(default)]
    pub runtime_requirements: Vec<crate::runtime::Requirement>,
    /// `.env.example` entries, in file order. The values matter as well as
    /// the keys: a value that is a localhost URL is how one app says where
    /// another one listens.
    pub env_example: Vec<(String, String)>,
    /// Framework marker files that exist.
    pub markers: Vec<String>,
    pub compose_files: Vec<String>,
    /// Root-level files that are gitignored and present — what a new
    /// worktree would be missing.
    pub ignored_present: Vec<String>,
    /// Local files the repository does not have but ships an example of:
    /// `(destination, source)`, sorted by destination.
    ///
    /// A fresh clone is the case. `.env` is gitignored, so it never arrives
    /// with the clone, and there is nothing for a worktree to be given a
    /// copy of — while `.env.example` sits beside it, tracked and unused.
    /// A pair is here only when the destination is gitignored in the main
    /// checkout and really absent, so nothing pando offers from this could
    /// ever show as untracked.
    ///
    /// Defaulted rather than required, so a dump written by an older pando
    /// still deserialises.
    #[serde(default)]
    pub provision_seeds: Vec<(String, String)>,
}

/// Also the install hook's fingerprint: a lockfile changing is what means
/// the dependencies changed.
pub const LOCKFILES: [&str; 11] = [
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "bun.lockb",
    "bun.lock",
    "uv.lock",
    "poetry.lock",
    "Gemfile.lock",
    "mix.lock",
    "go.sum",
    "Cargo.lock",
];

const WORKSPACE_MARKERS: [&str; 3] = ["pnpm-workspace.yaml", "turbo.json", "nx.json"];

const VERSION_FILES: [&str; 7] = [
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    "mise.toml",
    ".python-version",
    "rust-toolchain.toml",
    ".ruby-version",
];

const ENV_EXAMPLES: [&str; 3] = [".env.example", ".env.sample", ".env.template"];

const COMPOSE_FILES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];

/// Files that identify a framework, checked before script bodies.
const MARKER_FILES: [&str; 14] = [
    "next.config.js",
    "next.config.mjs",
    "next.config.ts",
    "vite.config.ts",
    "vite.config.js",
    "nuxt.config.ts",
    "manage.py",
    "mix.exs",
    "artisan",
    "config.ru",
    "bin/dev",
    "go.mod",
    "Cargo.toml",
    "pyproject.toml",
];

/// Reads every tier 1 signal from the main checkout.
pub fn signals(root: &Path) -> Signals {
    let manifest = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    Signals {
        scripts: parse_scripts(&manifest),
        targets: parse_targets(root),
        lockfiles: present(root, &LOCKFILES),
        workspace_markers: present(root, &WORKSPACE_MARKERS),
        version_files: present(root, &VERSION_FILES),
        runtime_requirements: crate::runtime::requirements(root),
        env_example: env_example(root),
        markers: present(root, &MARKER_FILES),
        compose_files: present(root, &COMPOSE_FILES),
        ignored_present: ignored_present(root),
        provision_seeds: provision_seeds(root),
    }
}

fn present(root: &Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| root.join(name).exists())
        .map(|name| (*name).to_string())
        .collect()
}

/// The `scripts` object of a `package.json`.
///
/// Hand-rolled rather than a JSON dependency pulled in for one object: the
/// shape is fixed, and anything it cannot read is simply not a signal.
fn parse_scripts(manifest: &str) -> BTreeMap<String, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(manifest) else {
        return BTreeMap::new();
    };
    let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) else {
        return BTreeMap::new();
    };
    scripts
        .iter()
        .filter_map(|(name, body)| Some((name.clone(), body.as_str()?.to_string())))
        .collect()
}

/// Targets of a `Makefile` or `justfile` that might start something, with
/// the first line of their recipe.
fn parse_targets(root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for file in ["Makefile", "makefile", "justfile", "Justfile"] {
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            // A target starts at column zero and its name is one word.
            if line.starts_with([' ', '\t']) || name.trim().is_empty() {
                continue;
            }
            let name = name.trim();
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                continue;
            }
            // `x := 1` is an assignment, not a target.
            if rest.trim_start().starts_with('=') {
                continue;
            }
            // What follows the colon is the prerequisite list — the targets
            // that run *first* — in both make and just, never the recipe.
            // Only make's one-liner form, a semicolon after the
            // prerequisites, puts a command on the target line. Otherwise
            // the recipe is the indented line below, and a target whose
            // next line is not indented has no recipe at all.
            let inline = rest.split_once(';').map(|(_, cmd)| cmd.trim());
            let recipe = match inline {
                Some(cmd) if !cmd.is_empty() => cmd.to_string(),
                _ => lines
                    .get(i + 1)
                    .filter(|l| l.starts_with([' ', '\t']))
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_default()
                    .to_string(),
            };
            // make's "do not echo this line" prefix is not part of the
            // command.
            let recipe = recipe.trim_start_matches('@').trim().to_string();
            if !recipe.is_empty() {
                out.entry(name.to_string()).or_insert(recipe);
            }
        }
    }
    out
}

impl Signals {
    /// The env example's keys, for the rules that only care about names.
    pub fn env_keys(&self) -> impl Iterator<Item = &str> {
        self.env_example.iter().map(|(key, _)| key.as_str())
    }
}

/// Entries of the first env example file that exists, in file order.
fn env_example(root: &Path) -> Vec<(String, String)> {
    for name in ENV_EXAMPLES {
        let Ok(text) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        return text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .filter(|(key, _)| !key.is_empty())
            .collect();
    }
    Vec::new()
}

/// Build output and editor noise: present and ignored, but nothing a new
/// worktree needs a copy of.
const PROVISION_DENYLIST: [&str; 6] = [
    ".DS_Store",
    "Thumbs.db",
    "npm-debug.log",
    "yarn-error.log",
    "pnpm-debug.log",
    "tsconfig.tsbuildinfo",
];

/// Root-level files that git ignores and that exist — the local files a
/// fresh worktree would be missing.
///
/// Only the root, and only files: an ignored directory is build output or a
/// dependency tree, which a worktree builds for itself.
fn ignored_present(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let mut found: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .filter(|p| !p.contains('/'))
        .filter(|p| !PROVISION_DENYLIST.contains(p))
        .filter(|p| root.join(p).is_file())
        .map(str::to_string)
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Suffixes that make a tracked file the example of a local one.
const EXAMPLE_SUFFIXES: [&str; 3] = [".example", ".sample", ".template"];

/// Root-level example files whose real file is gitignored and not here:
/// `(destination, source)`.
///
/// The fresh-clone case. `.env.example` is tracked and arrives with the
/// clone; `.env` is gitignored and never does, so `ignored_present` — which
/// only lists files that exist — has nothing to offer and a worktree is
/// created without the file the app needs.
///
/// Both conditions are checked here rather than at the question, because
/// proposing a seed that `new` would then refuse is worse than proposing
/// none: `git check-ignore` is what authorises a write into a worktree, and
/// a destination that is not ignored can never be one.
fn provision_seeds(root: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let source = entry.file_name().to_string_lossy().to_string();
        let Some(destination) = EXAMPLE_SUFFIXES
            .iter()
            .find_map(|suffix| source.strip_suffix(suffix))
        else {
            continue;
        };
        if destination.is_empty()
            || PROVISION_DENYLIST.contains(&destination)
            || root.join(destination).exists()
            || !is_gitignored(root, destination)
        {
            continue;
        }
        out.push((destination.to_string(), source));
    }
    out.sort();
    out
}

/// Whether the project's own gitignore covers a path, whether or not it
/// exists. Exit 0 means ignored; anything else — including a git that could
/// not run — means it is not, because only a definite yes may authorise a
/// write.
fn is_gitignored(root: &Path, rel: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--", rel])
        .output()
        .map(|out| out.status.code() == Some(0))
        .unwrap_or(false)
}

// ---- framework rules ------------------------------------------------------

/// How a framework is told which port to listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortMechanism {
    /// An environment variable, named here.
    Env(&'static str),
    /// A placeholder already inside the command pando proposes.
    InCommand,
    /// Nothing pando knows about; the developer says.
    Ask,
}

/// One framework: how to recognise it, how to start it, and how it takes a
/// port. Data, not code paths — a wrong entry is one table row to fix.
#[derive(Debug, Clone, Copy)]
pub struct FrameworkRule {
    pub name: &'static str,
    /// Any one of these files identifies it.
    pub markers: &'static [&'static str],
    /// Any one of these substrings in a script body identifies it.
    pub script_markers: &'static [&'static str],
    pub port: PortMechanism,
    pub default_port: u16,
    /// The command to propose when the project has no script to run.
    /// `{runner}` is replaced with the project's package or venv runner.
    pub command: Option<&'static str>,
    /// The flag that tells this framework its port, for an app whose own
    /// script pando runs rather than the command above: `pnpm dev --
    /// --port 1234`. `{port}` is replaced with the role template. `None`
    /// for a framework that takes its port some other way — Django's
    /// positional `host:port` cannot be appended to somebody's script.
    pub port_flag: Option<&'static str>,
}

/// The ten rules v1 ships with. Order matters: the first match wins, so the
/// specific frameworks come before the conventions they are built on.
pub const RULES: [FrameworkRule; 10] = [
    FrameworkRule {
        name: "Next.js",
        markers: &["next.config.js", "next.config.mjs", "next.config.ts"],
        script_markers: &["next dev"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: Some("npx next dev"),
        port_flag: Some("--port {port}"),
    },
    FrameworkRule {
        name: "Nuxt",
        markers: &["nuxt.config.ts"],
        script_markers: &["nuxt dev"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: Some("npx nuxt dev"),
        port_flag: Some("--port {port}"),
    },
    FrameworkRule {
        name: "Vite",
        markers: &["vite.config.ts", "vite.config.js"],
        script_markers: &["vite", "astro dev", "svelte-kit dev"],
        // Vite reads PORT only through its config, so the flag is the
        // reliable route — and it is one pando can put in the command.
        port: PortMechanism::InCommand,
        default_port: 5173,
        command: Some("npx vite --port {port:web}"),
        port_flag: Some("--port {port}"),
    },
    FrameworkRule {
        name: "Django",
        markers: &["manage.py"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("{runner}python manage.py runserver 127.0.0.1:{port:web}"),
        port_flag: None,
    },
    FrameworkRule {
        name: "Rails",
        markers: &["config.ru", "bin/dev"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 3000,
        command: Some("bin/rails server -p {port:web}"),
        port_flag: Some("-p {port}"),
    },
    FrameworkRule {
        name: "Phoenix",
        markers: &["mix.exs"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 4000,
        command: Some("mix phx.server"),
        port_flag: None,
    },
    FrameworkRule {
        name: "Laravel",
        markers: &["artisan"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("php artisan serve --port {port:web}"),
        port_flag: Some("--port {port}"),
    },
    FrameworkRule {
        name: "Go",
        markers: &["go.mod"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 8080,
        command: Some("go run ."),
        port_flag: None,
    },
    FrameworkRule {
        name: "Rust",
        markers: &["Cargo.toml"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 8080,
        // Only for a crate that builds a binary; a library has nothing to
        // run, which `binary_crate` decides.
        command: Some("cargo run"),
        port_flag: None,
    },
    FrameworkRule {
        name: "Node",
        markers: &[],
        script_markers: &["node ", "nodemon", "tsx ", "ts-node", "fastify", "express"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: None,
        port_flag: None,
    },
];

/// The first rule whose marker file or script body is present.
pub fn framework(root: &Path, signals: &Signals) -> Option<&'static FrameworkRule> {
    RULES.iter().find(|rule| {
        let by_marker = rule
            .markers
            .iter()
            .any(|m| signals.markers.iter().any(|f| f == m));
        let by_script = rule
            .script_markers
            .iter()
            .any(|needle| signals.scripts.values().any(|body| body.contains(needle)));
        // A Cargo.toml with no binary is a library: nothing to serve.
        if rule.name == "Rust" && by_marker && !binary_crate(root) {
            return false;
        }
        by_marker || by_script
    })
}

/// Whether a Cargo project builds something runnable. A `[lib]`-only crate
/// is level zero: pando lists and creates worktrees for it and proposes no
/// dev server at all.
fn binary_crate(root: &Path) -> bool {
    if root.join("src/main.rs").exists() || root.join("src/bin").is_dir() {
        return true;
    }
    std::fs::read_to_string(root.join("Cargo.toml"))
        .map(|text| text.contains("[[bin]]"))
        .unwrap_or(false)
}

// ---- proposals ------------------------------------------------------------

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
    /// key; [`edits`] is what knows how to write that.
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceHint {
    pub file: String,
    /// `None` when nothing in the env example points at this service. Such
    /// a service can still be run, but the app is never told where it is.
    pub env_key: Option<String>,
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
                .map(|hint| hint.file.as_str())
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
    propose_with(root, signals, None)
}

/// [`propose`] with a way to resolve a compose file the parser is a subset
/// of.
pub fn propose_with(
    root: &Path,
    signals: &Signals,
    resolve: Option<ComposeResolver<'_>>,
) -> Vec<Proposal> {
    let rule = framework(root, signals);
    [
        install_proposal(signals),
        version_files_proposal(signals),
        // Before the single-process slots: it decides whether there is one
        // process or several, and the slots below only fill a single one.
        processes_proposal(root, signals),
        dev_cmd_proposal(signals, rule),
        port_proposal(signals, rule),
        // After the processes, because a service is only worth proposing
        // once there is something to talk to it; before the schema hook,
        // whose whole point is to run once the services are up.
        services_proposal(root, signals, resolve),
        schema_hook_proposal(root, signals),
        provision_proposal(signals),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Lockfile to frozen install command.
///
/// Always the frozen variant: an install that can rewrite a lockfile would
/// be pando writing into the repository, which Invariant 1 forbids. Go and
/// Rust get nothing — `go run` and `cargo run` resolve their own modules,
/// and proposing a warm-up step for them is noise.
fn install_command(lockfile: &str) -> Option<(&'static str, &'static str)> {
    Some(match lockfile {
        "pnpm-lock.yaml" => ("pnpm install --frozen-lockfile", "pnpm-lock.yaml"),
        "package-lock.json" => ("npm ci", "package-lock.json"),
        "yarn.lock" => ("yarn install --immutable", "yarn.lock"),
        "bun.lockb" | "bun.lock" => ("bun install --frozen-lockfile", "a bun lockfile"),
        "uv.lock" => ("uv sync --frozen", "uv.lock"),
        // Poetry has no `--frozen`, and needs none: `install` refuses a
        // lockfile that no longer matches `pyproject.toml` rather than
        // regenerating it.
        "poetry.lock" => ("poetry install --sync", "poetry.lock"),
        // The environment variable rather than `bundle config`, which would
        // write `.bundle/config` into the repository.
        "Gemfile.lock" => ("BUNDLE_FROZEN=true bundle install", "Gemfile.lock"),
        // Deliberately nothing for `mix.lock`: `mix deps.get` writes the
        // lockfile for a dependency that is not in it yet, and an install
        // that can rewrite a lockfile is an Invariant 1 break. An Elixir
        // project configures its own install command.
        _ => return None,
    })
}

fn install_proposal(signals: &Signals) -> Option<Proposal> {
    let candidates: Vec<Candidate> = signals
        .lockfiles
        .iter()
        .filter_map(|lock| install_command(lock))
        .map(|(cmd, why)| Candidate {
            value: cmd.to_string(),
            why: why.to_string(),
            ..Candidate::default()
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let decided = candidates.len() == 1;
    Some(Proposal::of(Slot::Install, candidates, decided))
}

fn version_files_proposal(signals: &Signals) -> Option<Proposal> {
    if signals.version_files.is_empty() {
        return None;
    }
    // Informational only — every file that pins a version is worth showing,
    // so there is nothing to choose between.
    Some(Proposal::of(
        Slot::VersionFiles,
        vec![Candidate {
            value: signals.version_files.join(","),
            why: signals.version_files.join(", "),
            ..Candidate::default()
        }],
        true,
    ))
}

/// Scripts that are never a dev server, whatever they are called.
const EXCLUDED_SCRIPTS: [&str; 6] = ["build", "lint", "test", "preview", "typecheck", "format"];

/// Bodies that mean "serve the production build", not "develop".
const PRODUCTION_SHAPES: [&str; 6] = [
    "next start",
    "node dist",
    "serve out",
    "serve dist",
    "node build",
    "start:prod",
];

/// Bodies that run several processes at once. The command works, but it
/// gives one log and one readiness rule for two servers, so it is not
/// something to accept silently — the developer is asked.
const MULTIPLEXERS: [&str; 6] = [
    "concurrently",
    "npm-run-all",
    "run-p",
    "pnpm -r",
    "turbo run",
    "npm:",
];

fn is_production(body: &str) -> bool {
    PRODUCTION_SHAPES.iter().any(|shape| body.contains(shape))
}

fn is_multiplexer(body: &str) -> bool {
    MULTIPLEXERS.iter().any(|shape| body.contains(shape))
}

/// How this project runs a `package.json` script.
fn script_runner(signals: &Signals) -> &'static str {
    for lock in &signals.lockfiles {
        return match lock.as_str() {
            "pnpm-lock.yaml" => "pnpm ",
            "yarn.lock" => "yarn ",
            "bun.lockb" | "bun.lock" => "bun run ",
            "package-lock.json" => "npm run ",
            _ => continue,
        };
    }
    "npm run "
}

/// How this project runs a Python command.
fn python_runner(signals: &Signals) -> &'static str {
    for lock in &signals.lockfiles {
        return match lock.as_str() {
            "uv.lock" => "uv run ",
            "poetry.lock" => "poetry run ",
            _ => continue,
        };
    }
    ""
}

/// Scripts that could be a dev server, best first.
///
/// Exact `dev` wins; then `serve`, then `start` unless its body is the
/// production one; then anything else dev-shaped, alphabetically. Build,
/// lint, test and preview never qualify.
fn ranked_scripts(signals: &Signals) -> Vec<(String, String)> {
    let mut named: Vec<(String, String)> = Vec::new();
    let mut rest: Vec<(String, String)> = Vec::new();
    for (name, body) in &signals.scripts {
        if EXCLUDED_SCRIPTS.contains(&name.as_str()) || is_production(body) {
            continue;
        }
        let dev_shaped = name == "dev"
            || name == "serve"
            || name == "start"
            || name.starts_with("dev:")
            || name.starts_with("start:");
        if !dev_shaped {
            continue;
        }
        match name.as_str() {
            "dev" | "serve" | "start" => named.push((name.clone(), body.clone())),
            _ => rest.push((name.clone(), body.clone())),
        }
    }
    let rank = |name: &str| match name {
        "dev" => 0,
        "serve" => 1,
        _ => 2,
    };
    named.sort_by_key(|(name, _)| rank(name));
    named.extend(rest);
    named
}

fn dev_cmd_proposal(signals: &Signals, rule: Option<&'static FrameworkRule>) -> Option<Proposal> {
    let runner = script_runner(signals);
    let mut candidates: Vec<Candidate> = ranked_scripts(signals)
        .into_iter()
        .map(|(name, _body)| Candidate {
            value: format!("{runner}{name}"),
            why: format!("package.json scripts.{name}"),
            ..Candidate::default()
        })
        .chain(target_candidates(signals))
        .collect();

    // A framework's own command, for a project with no script to run it.
    if candidates.is_empty()
        && let Some(rule) = rule
        && let Some(command) = rule.command
    {
        let value = command.replace("{runner}", python_runner(signals));
        let ports = command
            .contains("{port:")
            .then(|| PortsSpec::List(vec![crate::config::WEB_ROLE.to_string()]));
        candidates.push(Candidate {
            value,
            why: format!("the {} rule", rule.name),
            ports,
            ..Candidate::default()
        });
    }

    dedup_by_value(&mut candidates);
    if candidates.is_empty() {
        // A framework pando recognises but has no command shape for: it
        // knows there is a server here and not how to start it, which is a
        // question, and the only slot in this phase that has one with no
        // options to offer.
        return rule.map(|_| Proposal::of(Slot::DevCmd, Vec::new(), false));
        // With no rule at all — a library, or a repository with nothing to
        // serve — there is no proposal. Asking about a dev server that does
        // not exist is worse than saying nothing.
    }
    // One candidate is certain. So is a script named exactly `dev` that is
    // really one dev server: a body that runs several at once is the
    // multi-process shape, which is a question, not an assumption.
    let sure_script = signals
        .scripts
        .get("dev")
        .is_some_and(|body| !is_multiplexer(body) && !is_production(body));
    let decided =
        candidates.len() == 1 || (sure_script && candidates[0].value == format!("{runner}dev"));
    Some(Proposal::of(Slot::DevCmd, candidates, decided))
}

/// Makefile or justfile targets that look like they start something. The
/// recipe is proposed, not `make run`: the recipe is the command, and `make`
/// swallows signals and output in between.
fn target_candidates(signals: &Signals) -> Vec<Candidate> {
    let mut out = Vec::new();
    for name in ["dev", "run", "serve", "start"] {
        if let Some(recipe) = signals.targets.get(name)
            && !is_production(recipe)
        {
            out.push(Candidate {
                value: recipe.clone(),
                why: format!("the {name} target"),
                ..Candidate::default()
            });
        }
    }
    out
}

/// The service families a `*PORT*` key can belong to, matched on the key's
/// stem — everything before the trailing `PORT`.
///
/// A key in one of these names a *service's* port, never one of the
/// application's own. It is the services slot that gives it a meaning: an
/// entry in `[[services]] env` pointing the key at the compose service it
/// addresses, which is where `DATABASE_PORT` and `REDIS_PORT` end up. Here
/// they only have to be kept out of the app's roles, because a role pando
/// allocated for `DATABASE_PORT` would hand the app a port with no database
/// behind it.
///
/// Matched as the whole stem (`DB_PORT`), as a prefix of it
/// (`DATABASE_REPLICA_PORT`), or as anything the stem *ends* with
/// (`READ_DB_PORT`, `INFLUXDB_PORT`, `COUCHDB_PORT`). The suffix arm is
/// deliberately not word-bounded: that is what the `ends_with` list this
/// replaced did, and requiring a boundary quietly turned every
/// `<something>DB_PORT` into one of the app's roles — a port allocated with
/// no database behind it. It is coarse at the far edge, where `GMAIL_PORT`
/// and `JPG_PORT` read as a mail and a Postgres port, and it was coarse
/// there before. The prefix arm *is* bounded, so `DBX_PORT` and
/// `PGADMIN_PORT` stay the application's own.
///
/// `REPLICA` earns its own entry for the bare `REPLICA_PORT` a project with
/// a read replica writes; the `<FAMILY>_REPLICA_PORT` spelling is already
/// caught by its family.
const SERVICE_PORT_FAMILIES: [&str; 13] = [
    "DATABASE",
    "DB",
    "POSTGRESQL",
    "POSTGRES",
    "PG",
    "MYSQL",
    "MARIADB",
    "REDIS",
    "MONGODB",
    "MONGO",
    "REPLICA",
    "SMTP",
    "MAIL",
];

/// Whether a key is port-shaped at all: exactly `PORT`, or `<SOMETHING>_PORT`.
///
/// A substring test also matches `SUPPORT_EMAIL`, `REPORT_URL`,
/// `IMPORT_PATH` and `PASSPORT_SECRET`, which is not only nonsense to offer:
/// it turns a slot that resolved silently into a question, and a
/// non-interactive `start` that exited 0 into an exit 3.
fn port_stem(key: &str) -> Option<&str> {
    if key == "PORT" {
        return Some("");
    }
    key.strip_suffix("_PORT")
}

fn is_service_port(key: &str) -> bool {
    let Some(stem) = port_stem(key) else {
        return false;
    };
    SERVICE_PORT_FAMILIES.iter().any(|family| {
        stem.strip_prefix(family)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
            || stem.ends_with(family)
    })
}

/// The role a port variable owns. `WEB_PORT` owns `web`; a bare `PORT` owns
/// `web` too, which is the role `share` and the browser key default to.
///
/// `None` for a stem that would not survive as a template argument, since
/// the role has to come back out of `{port:<role>}`.
fn port_role(key: &str) -> Option<String> {
    let stem = port_stem(key)?;
    if stem.is_empty() {
        return Some(crate::config::WEB_ROLE.to_string());
    }
    let role = stem.to_ascii_lowercase();
    role.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .then_some(role)
}

/// `a`, `a and b`, `a, b and c` — a list in a sentence.
fn listed<S: AsRef<str>>(names: &[S]) -> String {
    match names.split_last() {
        None => String::new(),
        Some((last, [])) => last.as_ref().to_string(),
        Some((last, rest)) => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn port_proposal(signals: &Signals, rule: Option<&'static FrameworkRule>) -> Option<Proposal> {
    let framework_env = rule.and_then(|r| match r.port {
        PortMechanism::Env(name) => Some((name, r.name)),
        _ => None,
    });

    // The project's own port variables, in file order: port-shaped keys of
    // the env example that are not a service's.
    let mut declared: Vec<&str> = Vec::new();
    for key in signals.env_keys() {
        if port_stem(key).is_some() && !is_service_port(key) && !declared.contains(&key) {
            declared.push(key);
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    // The framework's own mechanism first: it is a rule, not a guess.
    if let Some((name, framework)) = framework_env {
        candidates.push(Candidate {
            value: name.to_string(),
            why: format!("the {framework} convention"),
            ..Candidate::default()
        });
    }
    // Then `PORT`, then anything else port-shaped that is not a service's.
    let mut others: Vec<&str> = Vec::new();
    for key in &declared {
        if *key == "PORT" {
            candidates.insert(
                framework_env.is_some().into(),
                Candidate {
                    value: (*key).to_string(),
                    why: "PORT in the env example".to_string(),
                    ..Candidate::default()
                },
            );
        } else {
            others.push(key);
        }
    }
    candidates.extend(others.into_iter().map(|key| Candidate {
        value: key.to_string(),
        why: format!("{key} in the env example"),
        ..Candidate::default()
    }));
    dedup_by_value(&mut candidates);
    // A project that names two or more ports by role, and no bare `PORT` to
    // say which of them is the web server's, has declared several roles
    // rather than offered several guesses at one. That reading leads,
    // because it is the project's own statement about itself and the
    // framework rule underneath it is a convention pando brought with it —
    // but the single keys stay on offer below, since pando cannot know that
    // one process really owns them all.
    if let Some(candidate) = roles_from_env(&declared, framework_env.map(|(_, name)| name)) {
        candidates.insert(0, candidate);
    }
    if candidates.is_empty() {
        return None;
    }
    let decided = candidates.len() == 1;
    Some(Proposal::of(Slot::PortEnv, candidates, decided))
}

/// The one candidate that answers the port slot with a whole `ports` map:
/// every port variable the project declared, each owning a role named after
/// it.
fn roles_from_env(declared: &[&str], framework: Option<&str>) -> Option<Candidate> {
    if declared.len() < 2 || declared.contains(&"PORT") {
        return None;
    }
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for key in declared {
        map.insert((*key).to_string(), port_role(key)?);
    }
    let names = listed(declared);
    Some(Candidate {
        value: declared.join(", "),
        why: match framework {
            Some(framework) => {
                format!("{names} in the env example, over the {framework} convention")
            }
            None => format!("{names} in the env example"),
        },
        ports: Some(PortsSpec::Map(map)),
        ..Candidate::default()
    })
}

// ---- workspaces -----------------------------------------------------------

/// One app of a workspace: a directory with its own manifest and its own
/// dev script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceApp {
    /// The directory's own name, which is the process's name and its role.
    pub name: String,
    /// Relative to the repository root, for example `apps/web`.
    pub dir: String,
    /// The script pando would run, already prefixed with the runner.
    pub cmd: String,
    /// The port this app listens on when nobody tells it otherwise. What
    /// makes a localhost URL in the env example resolvable to *this* app.
    pub default_port: Option<u16>,
    /// How this app takes a port, from its own framework rule.
    pub port: PortMechanism,
}

/// Fewer than this is not a workspace worth splitting up: one app with a
/// dev script is the single-process case pando already handles.
const MIN_WORKSPACE_APPS: usize = 2;

/// Where a workspace says its packages live.
///
/// pnpm keeps them in `pnpm-workspace.yaml`, npm, yarn and bun in the root
/// manifest. turbo and nx describe pipelines rather than membership, so
/// when one of those is the only marker the two conventional directories
/// are tried. Hand-parsed on purpose: one list of globs is not worth a YAML
/// dependency, and anything this cannot read simply is not a signal.
fn workspace_globs(root: &Path) -> Vec<String> {
    let mut globs = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        globs.extend(yaml_string_list(&text, "packages"));
    }
    let manifest = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) {
        let workspaces = value.get("workspaces");
        let list = match workspaces {
            // Both shapes yarn and npm accept.
            Some(serde_json::Value::Array(list)) => Some(list.clone()),
            Some(serde_json::Value::Object(map)) => match map.get("packages") {
                Some(serde_json::Value::Array(list)) => Some(list.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(list) = list {
            globs.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_string));
        }
    }
    if globs.is_empty()
        && ["turbo.json", "nx.json"]
            .iter()
            .any(|f| root.join(f).exists())
    {
        globs.push("apps/*".to_string());
        globs.push("packages/*".to_string());
    }
    globs.sort();
    globs.dedup();
    globs
}

/// The string items of a top-level YAML list, for one key. Enough for
/// `packages:` followed by `  - 'apps/*'` lines, and nothing more.
fn yaml_string_list(text: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') || trimmed.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            inside = trimmed.trim_end_matches(':') == key && trimmed.ends_with(':');
            continue;
        }
        if !inside {
            continue;
        }
        let Some(item) = trimmed.trim_start().strip_prefix('-') else {
            continue;
        };
        let item = item.trim().trim_matches(['"', '\'']).to_string();
        if !item.is_empty() {
            out.push(item);
        }
    }
    out
}

/// The directories a workspace glob names.
///
/// Only a trailing `*` is expanded, which is the shape every workspace
/// glob in the wild has (`apps/*`, `packages/*`); anything else is treated
/// as a literal directory. A glob pando cannot read finds nothing, which
/// means one fewer proposal rather than a wrong one.
fn expand_glob(root: &Path, glob: &str) -> Vec<String> {
    let glob = glob.trim_end_matches('/');
    let Some(prefix) = glob.strip_suffix("/*") else {
        if glob.contains('*') || !root.join(glob).is_dir() {
            return Vec::new();
        }
        return vec![glob.to_string()];
    };
    if prefix.contains('*') {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root.join(prefix)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some(format!("{prefix}/{}", e.file_name().to_str()?)))
        .collect();
    out.sort();
    out
}

/// Signals of one app directory: enough for its framework rule, and no
/// more. A full read would shell out to git once per app for files nothing
/// here looks at.
fn app_signals(dir: &Path) -> Signals {
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap_or_default();
    Signals {
        scripts: parse_scripts(&manifest),
        markers: present(dir, &MARKER_FILES),
        ..Default::default()
    }
}

/// Every app of the workspace that has a dev script of its own.
///
/// The name is the directory's, which is also the role its port is
/// reserved under — so two apps with the same directory name would claim
/// one role, and `config::validate` would refuse the file pando had just
/// written. That is a workspace pando says nothing about.
pub fn workspace_apps(root: &Path, signals: &Signals) -> Vec<WorkspaceApp> {
    let runner = script_runner(signals);
    let mut apps: Vec<WorkspaceApp> = Vec::new();
    for glob in workspace_globs(root) {
        for dir in expand_glob(root, &glob) {
            let path = root.join(&dir);
            let app = app_signals(&path);
            let Some(script) = app.scripts.get("dev") else {
                continue;
            };
            if is_production(script) || is_multiplexer(script) {
                continue;
            }
            let Some(name) = Path::new(&dir).file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let rule = framework(&path, &app);
            let mut cmd = format!("{runner}dev");
            // A framework that takes its port on the command line gets the
            // flag appended to its own script: `pnpm dev -- --port 1234`
            // runs what the app already runs, on the port pando chose.
            if let Some(rule) = rule
                && rule.port == PortMechanism::InCommand
                && let Some(flag) = rule.port_flag
            {
                cmd = format!(
                    "{cmd} -- {}",
                    flag.replace("{port}", &format!("{{port:{name}}}"))
                );
            }
            apps.push(WorkspaceApp {
                default_port: app_default_port(signals, name, rule),
                port: match rule {
                    Some(rule)
                        if rule.port == PortMechanism::InCommand && rule.port_flag.is_none() =>
                    {
                        PortMechanism::Ask
                    }
                    Some(rule) => rule.port,
                    None => PortMechanism::Ask,
                },
                name: name.to_string(),
                dir,
                cmd,
            });
        }
    }
    apps.sort_by(|a, b| a.dir.cmp(&b.dir));
    apps.dedup_by(|a, b| a.dir == b.dir);
    let mut names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    if names.len() != unique {
        // Two apps with the same directory name would claim the same role.
        return Vec::new();
    }
    apps
}

/// The port an app listens on by default: what the root env example says
/// for it, else what its framework does.
fn app_default_port(
    signals: &Signals,
    name: &str,
    rule: Option<&'static FrameworkRule>,
) -> Option<u16> {
    let wanted = format!("{}_PORT", name.to_uppercase().replace('-', "_"));
    let from_example = signals
        .env_example
        .iter()
        .find(|(key, _)| *key == wanted)
        .and_then(|(_, value)| value.parse::<u16>().ok());
    from_example.or_else(|| rule.map(|r| r.default_port))
}

/// The env variable an app reads its port from, when its framework uses
/// one: the rule's own name, else a `<APP>_PORT` key in the env example.
fn app_port_env(signals: &Signals, app: &WorkspaceApp) -> Option<String> {
    match app.port {
        PortMechanism::Env(name) => Some(name.to_string()),
        // The command already carries the port. A second way of saying it
        // is a second thing that can disagree.
        PortMechanism::InCommand => None,
        PortMechanism::Ask => {
            let wanted = format!("{}_PORT", app.name.to_uppercase().replace('-', "_"));
            signals
                .env_keys()
                .any(|key| key == wanted)
                .then_some(wanted)
        }
    }
}

/// Cross-references between apps, read out of the root env example.
///
/// A value like `http://localhost:4000` is one app being told where
/// another one listens. When that port is another app's default, the value
/// becomes a template pointing at that app's role — so every worktree gets
/// its own pair of ports and the two halves still find each other. It is
/// given to every process *except* the one it points at, which is the only
/// one that does not need to be told.
///
/// Only an app that owns a role can be pointed at: `{port:<role>}` for a
/// role nobody owns does not render, and an environment that cannot be
/// rendered is a start that refuses.
fn cross_references(
    signals: &Signals,
    apps: &[WorkspaceApp],
    owns_role: &[bool],
) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (key, value) in &signals.env_example {
        let Some(port) = localhost_url_port(value) else {
            continue;
        };
        let Some(target) = apps
            .iter()
            .zip(owns_role)
            .find(|(app, owns)| **owns && app.default_port == Some(port))
            .map(|(app, _)| app)
        else {
            continue;
        };
        let template = value.replacen(
            &format!(":{port}"),
            &format!(":{{port:{}}}", target.name),
            1,
        );
        out.push((target.name.clone(), key.clone(), template));
    }
    out
}

/// The port of a URL that points at this machine, if that is what this is.
fn localhost_url_port(value: &str) -> Option<u16> {
    let (_, after) = value.split_once("://")?;
    let host_port = after.split(['/', '?', '#']).next()?;
    // Credentials, as in `postgres://user:pass@localhost:5432/db`.
    let host_port = host_port.rsplit('@').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]") {
        return None;
    }
    port.parse().ok()
}

/// The multi-process form, for a workspace whose apps each have a dev
/// script — and the root script as the one-process fallback beside it.
///
/// Always a question: running two servers instead of one changes what
/// `start`, `stop` and the log tabs do, and that is the developer's call.
/// `--yes` takes the first option, which is the per-app form.
fn processes_proposal(root: &Path, signals: &Signals) -> Option<Proposal> {
    let apps = workspace_apps(root, signals);
    if apps.len() < MIN_WORKSPACE_APPS {
        return None;
    }
    // A role is a port, and a port is only worth giving an app that has
    // some way of being told which one it got: its framework reads one
    // from the environment, or takes it on the command line (the flag is
    // already in `cmd` by now), or the env example has an `<APP>_PORT` key
    // for it. An app with none of those — a watcher, a codegen step, a
    // queue consumer, the `dev: "tsc -w"` of a `packages/*` library — gets
    // `ports = []` and no readiness rule. Given a role anyway it would be
    // handed a reserved port it never hears about, and `advance_phases`
    // would wait thirty seconds for it to bind before calling a perfectly
    // healthy process failed — and the whole worktree with it.
    let port_envs: Vec<Option<String>> =
        apps.iter().map(|app| app_port_env(signals, app)).collect();
    let owns_role: Vec<bool> = apps
        .iter()
        .zip(&port_envs)
        .map(|(app, port_env)| app.port == PortMechanism::InCommand || port_env.is_some())
        .collect();
    let references = cross_references(signals, &apps, &owns_role);
    let mut processes: BTreeMap<String, ProcessConfig> = BTreeMap::new();
    for ((app, port_env), owns_role) in apps.iter().zip(&port_envs).zip(&owns_role) {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        if let Some(var) = port_env {
            env.insert(var.clone(), format!("{{port:{}}}", app.name));
        }
        for (target, key, template) in &references {
            // The app a reference points at is the one that does not need
            // to be told where it is.
            if *target == app.name {
                continue;
            }
            env.insert(key.clone(), template.clone());
        }
        let roles = if *owns_role {
            vec![app.name.clone()]
        } else {
            Vec::new()
        };
        processes.insert(
            app.name.clone(),
            ProcessConfig {
                cmd: app.cmd.clone(),
                cwd: Some(app.dir.clone()),
                ports: Some(PortsSpec::List(roles)),
                env,
                ready: owns_role.then(|| ReadySpec {
                    role: Some(app.name.clone()),
                    timeout_s: None,
                }),
            },
        );
    }
    let summary = apps
        .iter()
        .map(|app| format!("{}: {} in {}", app.name, app.cmd, app.dir))
        .collect::<Vec<_>>()
        .join("; ");
    let mut candidates = vec![Candidate {
        value: summary,
        why: format!("a dev script in each of {} workspace apps", apps.len()),
        processes: Some(processes),
        ..Candidate::default()
    }];
    // The fallback: whatever the root script is, as it was before. Written
    // as `[dev]`, because one process is what that shorthand is for.
    if let Some(script) = signals.scripts.get("dev") {
        let _ = script;
        let value = format!("{}dev", script_runner(signals));
        candidates.push(Candidate {
            value: value.clone(),
            why: "package.json scripts.dev".to_string(),
            processes: Some(BTreeMap::from([(
                DEV.to_string(),
                ProcessConfig {
                    cmd: value,
                    ..Default::default()
                },
            )])),
            ..Candidate::default()
        });
    }
    // Never decided: two processes instead of one is a change of shape,
    // and "ask just in time, once" is exactly what this is for.
    Some(Proposal::of(Slot::Processes, candidates, false))
}

// ---- services and the schema hook -----------------------------------------

/// Images an application talks to, and the env-key prefixes that name one
/// when the service's own name does not.
///
/// A service whose image is on this list and that nothing in the env
/// example points at is the ambiguous case: the developer may want a
/// private copy of it and pando cannot tell, so it asks. A service whose
/// image is *not* on this list — a mail catcher, a dashboard, something
/// pando has never heard of — is left unticked without a question, because
/// an app that never reads an address for it is not talking to it.
const SERVICE_IMAGES: [(&str, &[&str]); 12] = [
    (
        "postgres",
        &["DATABASE", "DB", "POSTGRES", "PG", "POSTGRESQL"],
    ),
    ("postgis", &["DATABASE", "DB", "POSTGRES", "PG"]),
    ("mysql", &["DATABASE", "DB", "MYSQL"]),
    ("mariadb", &["DATABASE", "DB", "MYSQL", "MARIADB"]),
    ("redis", &["REDIS", "CACHE"]),
    ("valkey", &["REDIS", "VALKEY", "CACHE"]),
    ("mongo", &["MONGO", "MONGODB", "DATABASE"]),
    ("elasticsearch", &["ELASTIC", "ELASTICSEARCH", "SEARCH"]),
    ("rabbitmq", &["RABBITMQ", "AMQP", "QUEUE", "BROKER"]),
    ("kafka", &["KAFKA", "BROKER"]),
    ("minio", &["MINIO", "S3", "STORAGE"]),
    ("clickhouse", &["CLICKHOUSE"]),
];

/// Prefixes for images an app usually does not address, so a missing key
/// is not a question. A mail catcher is the classic: it exists so nothing
/// leaves the machine, and half of them are never configured at all.
const UTILITY_IMAGES: [(&str, &[&str]); 3] = [
    ("mailpit", &["SMTP", "MAIL", "MAILER"]),
    ("mailhog", &["SMTP", "MAIL", "MAILER"]),
    ("maildev", &["SMTP", "MAIL", "MAILER"]),
];

/// Key suffixes that hold an address pando can rewrite. `_NAME`, `_USER`
/// and `_PASSWORD` are about the same service and hold nothing pando can
/// point anywhere, so they are not candidates.
///
/// `_HOST` is not one either, and deliberately: `services::rewrite` can put
/// a port into a URL or replace a bare number, and a bare `localhost` has
/// nowhere to put one. Proposing it wrote a mapping that could never be
/// satisfied and failed the start that used it.
///
/// In preference order: a URL carries everything, a DSN nearly as much, and
/// a port is a number pando can simply replace.
const ADDRESS_SUFFIXES: [&str; 3] = ["_URL", "_DSN", "_PORT"];

/// What a rule can say about which env key names one compose service.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvKey {
    /// This key, and no other service is using it.
    Found(String),
    /// The only key a rule would have used is already pointed at another
    /// service of the same file. Two databases from one image is the
    /// ordinary case; pando cannot tell which one the app means, so it asks.
    TakenBy { key: String, service: String },
    /// Nothing in the env example names this service at all.
    Nothing,
}

/// The env key the app reads to find one compose service, when a rule can
/// say which.
///
/// `claimed` is every key an earlier service of the same file already owns.
/// An env map with one key pointing at two services is not a thing that can
/// be written down: the later one silently wins, the earlier one runs with
/// nothing addressing it, and the app talks to whichever pando happened to
/// write last.
fn env_key_for(
    service: &str,
    image: Option<&str>,
    env: &[(String, String)],
    claimed: &BTreeMap<String, String>,
) -> EnvKey {
    let mut prefixes: Vec<String> = vec![service.to_uppercase()];
    if let Some(image) = image {
        let family = image_family(image);
        for (known, keys) in SERVICE_IMAGES.iter().chain(UTILITY_IMAGES.iter()) {
            if Some(*known) == family {
                prefixes.extend(keys.iter().map(|k| (*k).to_string()));
            }
        }
    }
    // By suffix first, then by prefix: the best *kind* of key wins over
    // the best-matching name, because a `_PORT` that merely belongs to a
    // differently named prefix for the same service is a worse answer than
    // the `_URL` that carries the credentials too.
    let mut taken: Option<EnvKey> = None;
    for suffix in ADDRESS_SUFFIXES {
        for prefix in &prefixes {
            let Some((key, _)) = env
                .iter()
                .find(|(key, _)| key == &format!("{prefix}{suffix}"))
            else {
                continue;
            };
            match claimed.get(key) {
                None => return EnvKey::Found(key.clone()),
                // Remembered rather than returned: a later suffix may still
                // find this service a key of its own, and only when none
                // does is "somebody else has it" the answer.
                Some(owner) if taken.is_none() => {
                    taken = Some(EnvKey::TakenBy {
                        key: key.clone(),
                        service: owner.clone(),
                    });
                }
                Some(_) => {}
            }
        }
    }
    taken.unwrap_or(EnvKey::Nothing)
}

/// The image's last path segment with its tag stripped, when pando knows
/// it as either kind of service.
fn image_family(image: &str) -> Option<&'static str> {
    let image = image.split('@').next().unwrap_or(image);
    let last = image.rsplit('/').next().unwrap_or(image);
    let name = last.split(':').next().unwrap_or(last);
    SERVICE_IMAGES
        .iter()
        .chain(UTILITY_IMAGES.iter())
        .map(|(known, _)| *known)
        .find(|known| *known == name)
}

fn is_app_service(image: Option<&str>) -> bool {
    let Some(family) = image.and_then(image_family) else {
        return false;
    };
    SERVICE_IMAGES.iter().any(|(known, _)| *known == family)
}

/// Every service the project's compose file declares, with the env key
/// that names it where a rule found one.
///
/// Decided — no question at all — only when every service is resolved:
/// either an env key points at it, or its image is not one an application
/// talks to. One redis with nothing pointing at it is enough to ask,
/// because pando cannot tell whether the project wants a private copy.
fn services_proposal(
    root: &Path,
    signals: &Signals,
    resolve: Option<ComposeResolver<'_>>,
) -> Option<Proposal> {
    let file = crate::compose::find(root)?;
    let path = root.join(&file);
    let mut parsed = crate::compose::read(&path).ok()?;
    // Half a file read is not a proposal. Compose resolves `extends:` and a
    // top-level `include:`; ask it when it is available.
    if parsed.unresolved.any()
        && let Some(resolve) = resolve
        && let Some(resolved) = resolve(&path)
    {
        parsed = resolved;
    }
    if parsed.services.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    let mut resolved = true;
    // Which service owns which key so far, in file order. A key belongs to
    // the first service a rule gave it to; the second one is a question.
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    // The services that are this project rather than something it depends
    // on, kept so the refusal can name them.
    let mut built_here: Vec<String> = Vec::new();
    for (name, service) in &parsed.services {
        // A service compose builds out of this repository is the
        // application. Running a private copy of it per worktree is not
        // isolation, it is a second copy of the thing being developed, and
        // it is the one answer that is certainly wrong — so it is not on
        // offer at all.
        if let Some(context) = &service.build
            && built_from_project(root, &file, context)
        {
            built_here.push(name.clone());
            continue;
        }
        let image = service.image.as_deref();
        let found = env_key_for(name, image, &signals.env_example, &claimed);
        let app_service = is_app_service(image);
        if !matches!(found, EnvKey::Found(_)) && app_service {
            resolved = false;
        }
        let of_the_image = match image {
            Some(image) => format!("{file}, {image}"),
            None => file.clone(),
        };
        let why = match &found {
            EnvKey::Found(key) => format!("{of_the_image} → {key}"),
            EnvKey::TakenBy { key, service } => {
                format!("{of_the_image}; {key} already points at {service}")
            }
            EnvKey::Nothing => format!("{of_the_image}; nothing in the env example names it"),
        };
        let env_key = match found {
            EnvKey::Found(key) => {
                claimed.insert(key.clone(), name.clone());
                Some(key)
            }
            _ => None,
        };
        candidates.push(Candidate {
            value: name.clone(),
            why,
            preselected: env_key.is_some(),
            service: Some(ServiceHint {
                file: file.clone(),
                env_key,
            }),
            ..Candidate::default()
        });
    }
    // A file that could not be read whole — no Docker to ask, or compose
    // refused it — is never decided: the ports and images here may not be
    // the ones compose uses, and an `include:` brings in services this list
    // does not even have. It still proposes, because a project with a
    // compose file silently looking like a project with none is worse than
    // a question.
    if let Some(keys) = parsed.unresolved.describe() {
        resolved = false;
        for candidate in &mut candidates {
            if parsed.unresolved.include || parsed.unresolved.extends.contains(&candidate.value) {
                candidate.preselected = false;
            }
            candidate.why = format!(
                "{}; pando does not follow {keys} in this file, so check this one",
                candidate.why
            );
        }
    }
    // Every service in the file is this project's own. There is nothing to
    // ask — a question whose only answer is wrong is worse than silence —
    // but the *answer* still has to be written down, or the next start
    // works it out again and a developer reading the file never learns
    // that pando looked. It is the same empty answer a developer gives by
    // declining the question, recorded the same way: an entry naming the
    // file with an empty `include`.
    //
    // Unless pando could not read the file whole. "Half a file read is not
    // a proposal" applies hardest here: a top-level `include:` brings in
    // services this list does not have, and recording "none of them" about
    // a file pando has not finished reading would answer the slot for good
    // — silencing, permanently, whatever the next run with Docker present
    // would have found. Nothing is written, and it is asked again.
    if candidates.is_empty() {
        if parsed.unresolved.any() {
            return None;
        }
        let mut proposal = Proposal::of(Slot::Services, Vec::new(), true);
        proposal.none_because = Some(format!(
            "{file} declares only {}, built from this repository",
            listed(&built_here.iter().map(String::as_str).collect::<Vec<_>>())
        ));
        proposal.file = Some(file);
        return Some(proposal);
    }
    // Nothing to propose when no service resolved at all: a compose file
    // full of things pando cannot address is not an isolation offer.
    if candidates.iter().all(|c| !c.preselected) && resolved {
        return None;
    }
    Some(Proposal::of(Slot::Services, candidates, resolved))
}

/// Whether a compose service's build context points inside the project.
///
/// Resolved the way compose resolves it: relative to the directory the
/// compose file is in, which is where `build: ./api` looks. A context
/// `docker compose config` already made absolute is used as it stands.
///
/// [`crate::paths::resolve_for_compare`] rather than `canonicalize`: a
/// context directory that does not exist still says where compose *would*
/// look, and on macOS the repository's own `/var/...` and the canonical
/// `/private/var/...` are the same directory and have to compare equal.
fn built_from_project(root: &Path, file: &str, context: &str) -> bool {
    let compose_file = root.join(file);
    let dir = compose_file.parent().unwrap_or(root);
    let context = Path::new(context);
    let resolved = if context.is_absolute() {
        context.to_path_buf()
    } else {
        dir.join(context)
    };
    crate::paths::resolve_for_compare(&resolved)
        .starts_with(crate::paths::resolve_for_compare(root))
}

/// The command that brings a fresh database up to the current schema.
///
/// Each rule carries the globs whose change means it has to run again,
/// because a hook without them runs on every start and one with the wrong
/// ones never runs at all.
fn schema_hook_proposal(root: &Path, signals: &Signals) -> Option<Proposal> {
    let candidates = schema_candidates(root, signals);
    if candidates.is_empty() {
        return None;
    }
    // One candidate is one answer; several is a question.
    let decided = candidates.len() == 1;
    Some(Proposal::of(Slot::SchemaHook, candidates, decided))
}

/// The name every proposed schema hook gets. One name, so a second run
/// recognises the hook it wrote rather than appending another one.
pub const SCHEMA_HOOK: &str = "migrate";

fn schema_candidates(root: &Path, signals: &Signals) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut push = |cmd: String, globs: &[&str], why: &str| {
        out.push(Candidate {
            value: cmd.clone(),
            why: why.to_string(),
            hook: Some(crate::config::HookConfig {
                name: SCHEMA_HOOK.to_string(),
                after: crate::config::HookPoint::Services,
                fingerprint: globs.iter().map(|g| (*g).to_string()).collect(),
                cmd,
                cwd: None,
                fallback: None,
            }),
            ..Candidate::default()
        })
    };
    let exec = package_runner(signals);
    if root.join("prisma/schema.prisma").is_file() {
        push(
            format!("{exec} prisma migrate deploy"),
            &["prisma/migrations/**"],
            "prisma/schema.prisma",
        );
    }
    if root.join("drizzle.config.ts").is_file() || root.join("drizzle.config.js").is_file() {
        push(
            format!("{exec} drizzle-kit migrate"),
            &["drizzle/**"],
            "drizzle.config",
        );
    }
    if root.join("manage.py").is_file() {
        // `python_runner` carries its own trailing space, or is empty for
        // a project whose interpreter is simply on PATH.
        push(
            format!("{}python manage.py migrate", python_runner(signals)),
            &["*/migrations/*.py"],
            "manage.py",
        );
    }
    if root.join("alembic.ini").is_file() {
        push(
            format!("{}alembic upgrade head", python_runner(signals)),
            &["**/versions/*.py"],
            "alembic.ini",
        );
    }
    if root.join("config/database.yml").is_file() {
        push(
            "bin/rails db:prepare".to_string(),
            &["db/migrate/*.rb"],
            "config/database.yml",
        );
    }
    out
}

/// How this project runs a binary from its dependencies. `pnpm foo` and
/// `bunx foo` run it; `npm foo` does not, which is what `npx` is for.
fn package_runner(signals: &Signals) -> &'static str {
    match signals.lockfiles.first().map(String::as_str) {
        Some("pnpm-lock.yaml") => "pnpm",
        Some("yarn.lock") => "yarn",
        Some("bun.lockb") | Some("bun.lock") => "bunx",
        _ => "npx",
    }
}

/// Which local files each worktree needs a copy of.
///
/// Two shapes. A file that is gitignored and present is a file pando can
/// link, and there is nothing to decide about it. A file the repository
/// only ships an *example* of is a different offer: nothing is copied from
/// a tracked file into a worktree unless a developer says so, because the
/// example's defaults are not their local settings and pando has no way to
/// know whether that is close enough. So a proposal with a seed in it is
/// never decided, and the question below it is the plain answer: only what
/// is already here.
fn provision_proposal(signals: &Signals) -> Option<Proposal> {
    let present = &signals.ignored_present;
    let seeds = &signals.provision_seeds;
    if present.is_empty() && seeds.is_empty() {
        return None;
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    // The files that are already here lead, and are what `--yes` takes.
    if !present.is_empty() {
        candidates.push(Candidate {
            value: present.join(","),
            why: "gitignored and present in the main checkout".to_string(),
            ..Candidate::default()
        });
    }
    if !seeds.is_empty() {
        let mut paths = present.clone();
        for (destination, _) in seeds {
            if !paths.contains(destination) {
                paths.push(destination.clone());
            }
        }
        // Naming the source in the option text is the whole safeguard on
        // this path: a developer reading the prompt can open the file and
        // judge what is in it, which is the judgement pando cannot make.
        let seeded = listed(
            &seeds
                .iter()
                .map(|(destination, source)| format!("{destination} copied from {source}"))
                .collect::<Vec<_>>(),
        );
        candidates.push(Candidate {
            value: paths.join(","),
            why: match present.is_empty() {
                true => format!("{seeded} — this clone has none of its own"),
                false => format!("the same, plus {seeded}"),
            },
            provision_from: seeds.iter().cloned().collect(),
            needs_a_human: true,
            ..Candidate::default()
        });
    }
    let decided = seeds.is_empty();
    Some(Proposal::of(Slot::Provision, candidates, decided))
}

// ---- turning a choice into config -----------------------------------------

/// The process name `[dev]` is shorthand for.
pub const DEV: &str = "dev";

/// Whether a slot still has a question to ask, given what is already
/// decided.
///
/// A command that carries `{port:web}` has answered the port question by
/// existing — Django takes its port on the command line, so there is no
/// environment variable to choose.
pub fn still_needed(slot: Slot, config: &Config) -> bool {
    match slot {
        // A config that already declares any process has answered the
        // question of how many there are.
        Slot::Processes => config.processes.is_empty(),
        Slot::DevCmd => config
            .processes
            .get(DEV)
            .is_none_or(|dev| dev.cmd.trim().is_empty()),
        // Unset, not empty. A process configured with no ports at all — a
        // worker, a watcher, a queue consumer — has answered this question
        // with `ports = []`, and asking again would hand it a port it will
        // never bind and then call it failed for not binding it.
        Slot::PortEnv => config.processes.get(DEV).is_none_or(|p| p.ports.is_none()),
        // Unset, not empty, exactly as for the port slot: `prelude = ""`
        // is "this machine needs nothing", which is an answer, and asking
        // again would be asking a developer to say no twice.
        Slot::Prelude => config.runtime.prelude.is_none(),
        // And again: `provision = []` is "no worktree needs a local file
        // of mine", which a developer may have said by declining.
        Slot::Provision => config.project.provision.is_none(),
        // Anything already in `[[services]]` is an answer about every
        // service: a developer who listed two has said the third is not
        // wanted, and a second run must not offer it again.
        Slot::Services => config.services.is_empty(),
        // Likewise for hooks: one the developer wrote is their schema
        // step, whatever it is called.
        Slot::SchemaHook => config.hooks.is_empty(),
        _ => true,
    }
}

/// Whether detection may write into the single `dev` process at all, given
/// the config **as it was loaded**.
///
/// Two shapes qualify: a project with no processes configured, and the one
/// a developer writes when they want pando to fill something in — a lone
/// `[dev]` holding a `cwd` or an `env` and no command. Any other process
/// present, or a `[dev]` whose command they wrote themselves, means
/// detection stays out: a `[dev]` a developer wrote is an answer about its
/// ports too, and `[dev]` written beside `[processes]` is a file pando's
/// own loader refuses.
///
/// Asked once, of the config as loaded, and carried through the run:
/// the moment detection writes `[dev].cmd`, the config is indistinguishable
/// from one written by hand, and re-reading it would give the opposite
/// answer.
pub fn may_fill_dev(config: &Config) -> bool {
    match config.processes.len() {
        0 => true,
        1 => config
            .processes
            .get(DEV)
            .is_some_and(|dev| dev.cmd.trim().is_empty()),
        _ => false,
    }
}

/// The same permission, after an answer to [`Slot::Processes`] has been
/// applied: the single-process slots still have work to do when the answer
/// was the single-process form, and nothing to do when it was not.
pub fn fills_one_dev_process(config: &Config) -> bool {
    config.processes.len() == 1 && config.processes.contains_key(DEV)
}

/// A value the developer typed rather than chose.
///
/// A command carrying `{port:web}` brings its roles with it, so the port
/// question that would have followed is already answered.
pub fn custom(slot: Slot, value: &str) -> Candidate {
    let roles = roles_in(value);
    let ports = (matches!(slot, Slot::DevCmd | Slot::Processes) && !roles.is_empty())
        .then_some(PortsSpec::List(roles));
    Candidate {
        value: value.to_string(),
        why: String::new(),
        // A command typed at the processes question is one process, named
        // `dev`: every question has a custom answer, and the custom answer
        // to "several processes?" is "no, this one".
        service: None,
        hook: None,
        preselected: false,
        // A list the developer typed is a list of files they have. Seeding
        // from an example is an offer, and they did not take it.
        provision_from: BTreeMap::new(),
        // Typed by a human, by definition.
        needs_a_human: false,
        processes: (slot == Slot::Processes).then(|| {
            BTreeMap::from([(
                DEV.to_string(),
                ProcessConfig {
                    cmd: value.to_string(),
                    ports: ports.clone(),
                    ..Default::default()
                },
            )])
        }),
        ports,
    }
}

/// The `ports` an answer to the port slot writes: the whole map a candidate
/// carries, when a rule read several roles out of the project's own env
/// example, else the single variable the candidate named owning `web`.
fn port_spec(candidate: &Candidate) -> PortsSpec {
    candidate.ports.clone().unwrap_or_else(|| {
        PortsSpec::Map(BTreeMap::from([(
            candidate.value.clone(),
            crate::config::WEB_ROLE.to_string(),
        )]))
    })
}

/// Writes a chosen candidate into a config. The one place that knows what
/// each slot means, shared by the resolver and the tests.
pub fn apply(slot: Slot, candidate: &Candidate, config: &mut Config) {
    match slot {
        Slot::Install => config.project.install = Some(candidate.value.clone()),
        Slot::VersionFiles => config.runtime.version_files = split_list(&candidate.value),
        Slot::Prelude => config.runtime.prelude = Some(candidate.value.clone()),
        Slot::Provision => {
            config.project.provision = Some(split_list(&candidate.value));
            // Only what this answer brought: an answer with no seeds in it
            // is not a statement that the developer's own mapping is wrong.
            if !candidate.provision_from.is_empty() {
                config.project.provision_from = candidate.provision_from.clone();
            }
        }
        Slot::Processes => {
            for (name, process) in candidate.processes.iter().flatten() {
                config.processes.insert(name.clone(), process.clone());
            }
        }
        Slot::DevCmd => {
            let process = config.processes.entry(DEV.to_string()).or_default();
            process.cmd = candidate.value.clone();
            // Only when nobody has said: a `ports` the developer wrote is
            // an answer, and a command that carries `{port:web}` must not
            // overwrite it.
            if let Some(ports) = &candidate.ports
                && process.ports.is_none()
            {
                process.ports = Some(ports.clone());
            }
        }
        Slot::PortEnv => {
            let process = config.processes.entry(DEV.to_string()).or_default();
            process.ports = Some(port_spec(candidate));
        }
        Slot::SchemaHook => {
            if let Some(hook) = &candidate.hook {
                config.hooks.push(hook.clone());
            }
        }
        // A set, not a value: it goes through [`apply_services`].
        Slot::Services => {}
    }
}

/// Writes a chosen *set* of compose services as one `[[services]]` entry.
///
/// An empty set is written too, as an entry with an empty `include`:
/// "none of them" is an answer, and an answer nothing records is asked
/// again on every start. It is the same shape the port slot uses for a
/// process that really has no ports.
pub fn apply_services(file: &str, chosen: &[&Candidate], config: &mut Config) {
    let file = file.to_string();
    let include: Vec<String> = chosen.iter().map(|c| c.value.clone()).collect();
    let env: BTreeMap<String, String> = chosen
        .iter()
        .filter_map(|c| {
            let hint = c.service.as_ref()?;
            Some((hint.env_key.clone()?, c.value.clone()))
        })
        .collect();
    config.services.push(crate::config::ServiceConfig::Compose {
        file,
        include,
        env,
        ready_timeout_s: None,
    });
}

/// The keys of the one `[[services]]` entry a set answer appends.
///
/// Takes the file rather than reading it off a candidate, because the empty
/// answer has no candidates to read it from and still has to be written:
/// "none of them" is about a compose file that exists.
pub fn service_entry(
    file: &str,
    chosen: &[&Candidate],
) -> (&'static str, Vec<(String, toml_edit::Value)>) {
    let array = Slot::Services
        .array()
        .expect("the services slot appends a [[table]] entry");
    let mut entries: Vec<(String, toml_edit::Value)> = vec![
        ("kind".to_string(), "compose".into()),
        ("file".to_string(), file.to_string().into()),
        (
            "include".to_string(),
            toml_edit::Value::Array(toml_edit::Array::from_iter(
                chosen.iter().map(|c| c.value.clone()),
            )),
        ),
    ];
    let mut env = toml_edit::InlineTable::new();
    for candidate in chosen {
        if let Some(key) = candidate.service.as_ref().and_then(|h| h.env_key.as_ref()) {
            env.insert(key, candidate.value.as_str().into());
        }
    }
    if !env.is_empty() {
        entries.push(("env".to_string(), toml_edit::Value::InlineTable(env)));
    }
    (array, entries)
}

/// The keys of the one `[[table]]` entry a set answer appends, or `None`
/// for a slot whose answer is a key in a table.
pub fn array_edits(
    slot: Slot,
    chosen: &[&Candidate],
) -> Option<(&'static str, Vec<(String, toml_edit::Value)>)> {
    let array = slot.array()?;
    let mut entries: Vec<(String, toml_edit::Value)> = Vec::new();
    match slot {
        Slot::Services => {
            let hint = chosen.iter().find_map(|c| c.service.as_ref())?;
            return Some(service_entry(&hint.file, chosen));
        }
        Slot::SchemaHook => {
            let hook = chosen.first()?.hook.as_ref()?;
            entries.push(("name".to_string(), hook.name.clone().into()));
            entries.push(("after".to_string(), "services".into()));
            if !hook.fingerprint.is_empty() {
                entries.push((
                    "fingerprint".to_string(),
                    toml_edit::Value::Array(toml_edit::Array::from_iter(
                        hook.fingerprint.iter().cloned(),
                    )),
                ));
            }
            entries.push(("cmd".to_string(), hook.cmd.clone().into()));
        }
        _ => return None,
    }
    Some((array, entries))
}

/// The same choice, as the keys to patch into `pando.toml`.
///
/// Separate from [`apply`] because the file is edited in place rather than
/// re-serialised from the struct: the developer's comments and ordering
/// survive, and only the keys pando decided are touched.
pub fn edits(slot: Slot, candidate: &Candidate) -> Vec<Edit> {
    // Owned, not `&'static`: `[processes.<app>]` is a table whose name
    // detection only learns by reading the repository.
    let single = |table: &[&str], key: &str, value: toml_edit::Value| Edit {
        table: table.iter().map(|t| t.to_string()).collect(),
        key: key.to_string(),
        value,
    };
    let keyed = slot.key();
    match slot {
        Slot::Prelude => {
            let (table, key) = keyed.expect("this slot writes one key");
            vec![single(table, key, candidate.value.clone().into())]
        }
        Slot::Install | Slot::DevCmd => {
            let (table, key) = keyed.expect("this slot writes one key");
            let mut out = vec![single(table, key, candidate.value.clone().into())];
            if let Some(PortsSpec::List(roles)) = &candidate.ports {
                out.push(single(
                    &["dev"],
                    "ports",
                    toml_edit::Value::Array(toml_edit::Array::from_iter(roles.iter().cloned())),
                ));
            }
            out
        }
        Slot::VersionFiles => {
            let (table, key) = keyed.expect("this slot writes one key");
            vec![single(
                table,
                key,
                toml_edit::Value::Array(toml_edit::Array::from_iter(split_list(&candidate.value))),
            )]
        }
        Slot::Provision => {
            let (table, key) = keyed.expect("this slot writes one key");
            let mut out = vec![single(
                table,
                key,
                toml_edit::Value::Array(toml_edit::Array::from_iter(split_list(&candidate.value))),
            )];
            // The list says which files a worktree gets; this says where
            // the ones the main checkout does not have come from. Two keys,
            // because the second is only true of some of the first.
            if !candidate.provision_from.is_empty() {
                let mut inline = toml_edit::InlineTable::new();
                for (destination, source) in &candidate.provision_from {
                    inline.insert(destination, source.as_str().into());
                }
                out.push(single(
                    &["project"],
                    "provision_from",
                    toml_edit::Value::InlineTable(inline),
                ));
            }
            out
        }
        Slot::PortEnv => {
            let (table, key) = keyed.expect("this slot writes one key");
            let value = match port_spec(candidate) {
                PortsSpec::Map(map) => {
                    let mut inline = toml_edit::InlineTable::new();
                    for (var, role) in &map {
                        inline.insert(var, role.as_str().into());
                    }
                    toml_edit::Value::InlineTable(inline)
                }
                PortsSpec::List(roles) => {
                    toml_edit::Value::Array(toml_edit::Array::from_iter(roles.iter().cloned()))
                }
            };
            vec![single(table, key, value)]
        }
        Slot::Processes => process_edits(candidate),
        // Both of these append a whole `[[table]]` entry; [`array_edits`]
        // is what knows how to write one.
        Slot::Services | Slot::SchemaHook => Vec::new(),
    }
}

/// One key pando is about to write, and the table it belongs in.
#[derive(Debug, Clone)]
pub struct Edit {
    pub table: Vec<String>,
    pub key: String,
    pub value: toml_edit::Value,
}

/// A whole `[processes]` table, written out.
///
/// One process named `dev` takes the `[dev]` shorthand instead: that is
/// what the shorthand is for, and it keeps the single-process file the
/// shape every example in the docs has.
fn process_edits(candidate: &Candidate) -> Vec<Edit> {
    let mut out = Vec::new();
    let processes = candidate.processes.clone().unwrap_or_default();
    let shorthand = processes.len() == 1 && processes.contains_key(DEV);
    for (name, process) in &processes {
        let table: Vec<String> = if shorthand {
            vec![DEV.to_string()]
        } else {
            vec!["processes".to_string(), name.clone()]
        };
        let mut push = |key: &str, value: toml_edit::Value| {
            out.push(Edit {
                table: table.clone(),
                key: key.to_string(),
                value,
            })
        };
        push("cmd", process.cmd.clone().into());
        if let Some(cwd) = &process.cwd {
            push("cwd", cwd.clone().into());
        }
        match &process.ports {
            Some(PortsSpec::List(roles)) => push(
                "ports",
                toml_edit::Value::Array(toml_edit::Array::from_iter(roles.iter().cloned())),
            ),
            Some(PortsSpec::Map(map)) => {
                let mut inline = toml_edit::InlineTable::new();
                for (var, role) in map {
                    inline.insert(var, role.as_str().into());
                }
                push("ports", toml_edit::Value::InlineTable(inline));
            }
            None => {}
        }
        if !process.env.is_empty() {
            let mut inline = toml_edit::InlineTable::new();
            for (var, value) in &process.env {
                inline.insert(var, value.as_str().into());
            }
            push("env", toml_edit::Value::InlineTable(inline));
        }
        if let Some(ready) = &process.ready {
            let mut inline = toml_edit::InlineTable::new();
            if let Some(role) = &ready.role {
                inline.insert("role", role.as_str().into());
            }
            if let Some(timeout) = ready.timeout_s {
                inline.insert("timeout_s", (timeout as i64).into());
            }
            push("ready", toml_edit::Value::InlineTable(inline));
        }
    }
    out
}

/// The roles a command asks for by carrying `{port:<role>}`.
///
/// A developer who types their own command with a placeholder in it has
/// declared the role by using it; there is no second question to ask.
pub fn roles_in(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for piece in cmd.split("{port:").skip(1) {
        let Some((role, _)) = piece.split_once('}') else {
            continue;
        };
        if !role.is_empty() && !out.contains(&role.to_string()) {
            out.push(role.to_string());
        }
    }
    out
}

/// Multi-valued slots carry their list as one comma-separated string, so a
/// `Candidate` stays one value whatever the slot holds.
fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

/// Keeps the first of each repeated value: the Makefile target and the Go
/// rule both say `go run .`, and that is one candidate, not two.
fn dedup_by_value(candidates: &mut Vec<Candidate>) {
    let mut seen: Vec<String> = Vec::new();
    candidates.retain(|c| {
        if seen.contains(&c.value) {
            return false;
        }
        seen.push(c.value.clone());
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn scripts(pairs: &[(&str, &str)]) -> Signals {
        Signals {
            scripts: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    /// Env example keys with values nothing reads, for the port rules.
    fn env_pairs(keys: &[&str]) -> Vec<(String, String)> {
        keys.iter()
            .map(|k| (k.to_string(), "1".to_string()))
            .collect()
    }

    fn with_lock(mut signals: Signals, lock: &str) -> Signals {
        signals.lockfiles.push(lock.to_string());
        signals
    }

    fn values(proposal: &Proposal) -> Vec<&str> {
        proposal
            .candidates
            .iter()
            .map(|c| c.value.as_str())
            .collect()
    }

    fn dev_of(signals: &Signals, rule: Option<&'static FrameworkRule>) -> Proposal {
        dev_cmd_proposal(signals, rule).expect("a dev command")
    }

    // ---- parsing ---------------------------------------------------------

    #[test]
    fn scripts_come_out_of_a_package_json() {
        let manifest = r#"{"name":"x","scripts":{"dev":"next dev","build":"next build"}}"#;
        let parsed = parse_scripts(manifest);
        assert_eq!(parsed["dev"], "next dev");
        assert_eq!(parsed.len(), 2);
        assert!(parse_scripts("not json").is_empty());
        assert!(parse_scripts(r#"{"name":"x"}"#).is_empty());
    }

    #[test]
    fn make_and_just_targets_yield_their_recipes() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            ".PHONY: run\nrun:\n\tgo run .\n\nbuild:\n\tgo build ./...\n\nCFLAGS := -O2\n",
        )
        .unwrap();
        let targets = parse_targets(dir.path());
        assert_eq!(targets["run"], "go run .");
        assert_eq!(targets["build"], "go build ./...");
        assert!(
            !targets.contains_key("CFLAGS"),
            "a variable assignment is not a target: {targets:?}"
        );
    }

    // Text after the colon is the prerequisite list in make and in just,
    // never the recipe. `run: build fmt` used to yield `build fmt` as the
    // command that starts the dev server — accepted silently, and written
    // to config with a comment claiming the run target said so.
    #[test]
    fn what_follows_a_target_name_is_prerequisites_not_the_recipe() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "run: build fmt\n\tgo run .\n\nbuild:\n\tgo build ./...\n\n\
             serve: ; python3 -m http.server\n\nall: build run\nnext:\n\techo hi\n",
        )
        .unwrap();
        let targets = parse_targets(dir.path());
        assert_eq!(
            targets["run"], "go run .",
            "the indented line below the target is the recipe: {targets:?}"
        );
        assert_eq!(
            targets["serve"], "python3 -m http.server",
            "make's one-liner form puts the recipe after a semicolon: {targets:?}"
        );
        assert!(
            !targets.contains_key("all"),
            "a target whose next line is another target has no recipe: {targets:?}"
        );
    }

    #[test]
    fn env_example_keys_are_read_in_file_order() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env.example"),
            "# a comment\nPORT=3000\n\nDB_PORT=5432\nEMPTY\n",
        )
        .unwrap();
        assert_eq!(
            env_example(dir.path()),
            vec![
                ("PORT".to_string(), "3000".to_string()),
                ("DB_PORT".to_string(), "5432".to_string())
            ]
        );
    }

    // ---- the dev command -------------------------------------------------

    #[test]
    fn a_production_start_script_is_not_a_dev_server() {
        let signals = with_lock(
            scripts(&[
                ("dev", "next dev"),
                ("start", "next start"),
                ("build", "next build"),
                ("lint", "next lint"),
            ]),
            "pnpm-lock.yaml",
        );
        let proposal = dev_of(&signals, None);
        assert_eq!(values(&proposal), vec!["pnpm dev"]);
        assert!(proposal.decided);
    }

    // Several candidates with an unambiguous `dev` is still a decision: the
    // developer named it, and pando is only confirming.
    #[test]
    fn an_exact_dev_script_wins_over_its_siblings() {
        let signals = with_lock(
            scripts(&[
                ("dev", "next dev"),
                ("serve", "http-server"),
                ("start", "node server.js"),
            ]),
            "pnpm-lock.yaml",
        );
        let proposal = dev_of(&signals, None);
        assert_eq!(
            values(&proposal),
            vec!["pnpm dev", "pnpm serve", "pnpm start"],
            "serve outranks start, and both stay on offer"
        );
        assert!(proposal.decided, "the one named dev is the answer");
    }

    // A `dev` that starts several servers gives one log and one readiness
    // rule for two processes. It works, so it is offered first — but it is
    // not assumed.
    #[test]
    fn a_dev_script_that_fans_out_is_asked_about() {
        for body in [
            "concurrently \"npm:dev:*\"",
            "npm-run-all -p dev:*",
            "turbo run dev",
            "pnpm -r --parallel dev",
        ] {
            let signals = with_lock(
                scripts(&[("dev", body), ("dev:web", "next dev")]),
                "pnpm-lock.yaml",
            );
            let proposal = dev_of(&signals, None);
            assert!(!proposal.decided, "{body:?} should be asked about");
            assert_eq!(proposal.preferred().unwrap().value, "pnpm dev");
        }
    }

    #[test]
    fn the_runner_comes_from_the_lockfile() {
        for (lock, expected) in [
            ("pnpm-lock.yaml", "pnpm dev"),
            ("package-lock.json", "npm run dev"),
            ("yarn.lock", "yarn dev"),
            ("bun.lockb", "bun run dev"),
        ] {
            let signals = with_lock(scripts(&[("dev", "next dev")]), lock);
            assert_eq!(values(&dev_of(&signals, None)), vec![expected]);
        }
        // No lockfile at all: npm is the safe default.
        assert_eq!(
            values(&dev_of(&scripts(&[("dev", "next dev")]), None)),
            vec!["npm run dev"]
        );
    }

    #[test]
    fn a_project_with_nothing_to_run_proposes_nothing() {
        assert!(dev_cmd_proposal(&Signals::default(), None).is_none());
    }

    // ---- the port --------------------------------------------------------

    #[test]
    fn a_service_port_is_never_offered_as_the_web_port() {
        let signals = Signals {
            env_example: env_pairs(&["PORT", "API_PORT", "DB_PORT", "SMTP_PORT", "REDIS_PORT"]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, None).unwrap();
        assert_eq!(values(&proposal), vec!["PORT", "API_PORT"]);
        assert!(!proposal.decided, "two candidates is a question");
    }

    // A substring match made `SUPPORT_EMAIL` and `REPORT_URL` port
    // candidates, which turned a slot that resolved silently into a
    // question — and a non-interactive `start` that exited 0 into an exit 3.
    #[test]
    fn only_a_key_that_is_or_ends_with_port_is_a_port() {
        let signals = Signals {
            env_example: env_pairs(&[
                "PORT",
                "SUPPORT_EMAIL",
                "REPORT_URL",
                "IMPORT_PATH",
                "EXPORT_DIR",
                "PASSPORT_SECRET",
                "API_PORT",
            ]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, None).unwrap();
        assert_eq!(
            values(&proposal),
            vec!["PORT", "API_PORT"],
            "support, report, import, export and passport are not ports"
        );
    }

    #[test]
    fn a_framework_convention_answers_the_port_with_no_env_file_at_all() {
        let go = RULES.iter().find(|r| r.name == "Go").unwrap();
        let proposal = port_proposal(&Signals::default(), Some(go)).unwrap();
        assert_eq!(values(&proposal), vec!["PORT"]);
        assert!(proposal.decided);
    }

    // The monorepo shape this was built for: the project names its ports by
    // role in its own env example, and there is no `PORT` to say which of
    // them is *the* one. Two variables are two roles, not two guesses at a
    // single answer.
    #[test]
    fn port_variables_named_by_role_become_several_roles() {
        let signals = Signals {
            env_example: env_pairs(&["WEB_PORT", "API_PORT", "DATABASE_PORT"]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, None).unwrap();
        let first = proposal.preferred().unwrap();
        assert_eq!(first.value, "WEB_PORT, API_PORT");
        assert_eq!(first.why, "WEB_PORT and API_PORT in the env example");
        assert_eq!(
            first.ports,
            Some(PortsSpec::Map(BTreeMap::from([
                ("WEB_PORT".to_string(), "web".to_string()),
                ("API_PORT".to_string(), "api".to_string()),
            ]))),
            "the database's port belongs to the services slot, never to a role"
        );
        assert_eq!(
            values(&proposal),
            vec!["WEB_PORT, API_PORT", "WEB_PORT", "API_PORT"],
            "the single keys stay on offer: pando cannot know one process owns them all"
        );

        let mut config = Config::default();
        apply(Slot::PortEnv, first, &mut config);
        assert_eq!(config.processes[DEV].roles(), vec!["api", "web"]);
        assert_eq!(
            config.processes[DEV].port_env()["WEB_PORT"],
            "{port:web}",
            "the map form is sugar for the env the app really reads"
        );
    }

    // The project's own declaration beats the convention pando brought with
    // it — and says so, because the note in the file is the only place a
    // developer sees which of the two won.
    #[test]
    fn the_projects_own_port_variables_beat_the_framework_guess() {
        let next = RULES.iter().find(|r| r.name == "Next.js").unwrap();
        let signals = Signals {
            env_example: env_pairs(&["WEB_PORT", "API_PORT"]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, Some(next)).unwrap();
        assert_eq!(
            values(&proposal),
            vec!["WEB_PORT, API_PORT", "PORT", "WEB_PORT", "API_PORT"],
            "the framework's PORT is still an option, just not the first one"
        );
        assert_eq!(
            proposal.preferred().unwrap().why,
            "WEB_PORT and API_PORT in the env example, over the Next.js convention"
        );
        assert!(!proposal.decided);
    }

    // A project that writes `PORT` has said where its web server's port
    // comes from. The role reading is for a project that named its ports
    // instead, so this one keeps the question it always had.
    #[test]
    fn a_project_that_names_port_keeps_the_single_answer() {
        let signals = Signals {
            env_example: env_pairs(&["PORT", "API_PORT"]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, None).unwrap();
        assert_eq!(values(&proposal), vec!["PORT", "API_PORT"]);
        assert!(
            proposal.candidates.iter().all(|c| c.ports.is_none()),
            "no candidate here answers with a whole map"
        );
    }

    // Every spelling of a service's port a real project uses. One of these
    // becoming a role would hand the app a port with no database behind it.
    #[test]
    fn a_service_family_port_is_never_one_of_the_apps_roles() {
        for key in [
            "DATABASE_PORT",
            "DATABASE_REPLICA_PORT",
            "DB_PORT",
            "READ_DB_PORT",
            // Every `<something>DB_PORT`: the suffix list this replaced
            // caught these with a plain `ends_with`, and a word boundary
            // here would hand each of them a role with nothing behind it.
            "INFLUXDB_PORT",
            "COUCHDB_PORT",
            "DYNAMODB_PORT",
            "REPLICA_PORT",
            "REDIS_PORT",
            "MONGO_PORT",
            "MONGODB_PORT",
            "POSTGRES_PORT",
            "PG_PORT",
            "MYSQL_PORT",
            "MARIADB_PORT",
            "SMTP_PORT",
            "MAIL_PORT",
        ] {
            assert!(is_service_port(key), "{key} is a service's port");
        }
        // The prefix arm is word-bounded, so a family that merely *starts*
        // a longer word is not a match.
        for key in [
            "PORT",
            "WEB_PORT",
            "API_PORT",
            "ADMIN_PORT",
            "DBX_PORT",
            "PGADMIN_PORT",
            "METRICS_PORT",
            "GRPC_PORT",
        ] {
            assert!(!is_service_port(key), "{key} is the application's own");
        }
    }

    #[test]
    fn a_multi_role_answer_is_written_as_one_inline_table() {
        let signals = Signals {
            env_example: env_pairs(&["WEB_PORT", "API_PORT"]),
            ..Default::default()
        };
        let proposal = port_proposal(&signals, None).unwrap();
        let edits = edits(Slot::PortEnv, proposal.preferred().unwrap());
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].table, vec!["dev"]);
        assert_eq!(edits[0].key, "ports");
        assert_eq!(
            edits[0].value.to_string().trim(),
            r#"{ API_PORT = "api", WEB_PORT = "web" }"#
        );
    }

    #[test]
    fn a_framework_that_takes_its_port_in_the_command_has_no_env_question() {
        let django = RULES.iter().find(|r| r.name == "Django").unwrap();
        assert!(port_proposal(&Signals::default(), Some(django)).is_none());

        let mut config = Config::default();
        apply(
            Slot::DevCmd,
            &Candidate {
                value: "python manage.py runserver 127.0.0.1:{port:web}".into(),
                why: "the Django rule".into(),
                ports: Some(PortsSpec::List(vec!["web".into()])),
                ..Candidate::default()
            },
            &mut config,
        );
        assert!(
            !still_needed(Slot::PortEnv, &config),
            "the command already carries the port"
        );
    }

    // ---- framework rules -------------------------------------------------

    fn marker_fixture(files: &[(&str, &str)]) -> (TempDir, Signals) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let signals = signals(dir.path());
        (dir, signals)
    }

    #[test]
    fn a_marker_file_names_the_framework() {
        let (dir, s) = marker_fixture(&[("manage.py", "")]);
        assert_eq!(framework(dir.path(), &s).unwrap().name, "Django");
        let (dir, s) = marker_fixture(&[("nuxt.config.ts", "")]);
        assert_eq!(framework(dir.path(), &s).unwrap().name, "Nuxt");
    }

    #[test]
    fn a_script_body_names_the_framework_when_no_marker_file_does() {
        let signals = scripts(&[("dev", "next dev")]);
        let dir = tempdir().unwrap();
        assert_eq!(framework(dir.path(), &signals).unwrap().name, "Next.js");
    }

    // A crate with no binary has nothing to serve, and proposing
    // `cargo run` for it would be an invention.
    #[test]
    fn a_library_crate_matches_no_framework() {
        let (dir, s) = marker_fixture(&[("Cargo.toml", "[package]\nname = \"x\"\n\n[lib]\n")]);
        assert!(framework(dir.path(), &s).is_none());

        let (dir, s) = marker_fixture(&[
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
            ("src/main.rs", "fn main() {}"),
        ]);
        assert_eq!(framework(dir.path(), &s).unwrap().name, "Rust");
    }

    // ---- install ---------------------------------------------------------

    // Invariant 1: a lockfile can never change because pando ran an
    // install. Every command here has been checked against its tool's
    // documentation, so the list is the assertion — a new entry has to be
    // added here deliberately.
    #[test]
    fn every_install_command_is_a_frozen_one() {
        const KNOWN_FROZEN: [&str; 7] = [
            "pnpm install --frozen-lockfile",
            "npm ci",
            "yarn install --immutable",
            "bun install --frozen-lockfile",
            "uv sync --frozen",
            "poetry install --sync",
            "BUNDLE_FROZEN=true bundle install",
        ];
        for lock in LOCKFILES {
            let Some((cmd, _)) = install_command(lock) else {
                continue;
            };
            assert!(
                KNOWN_FROZEN.contains(&cmd),
                "{lock} proposes {cmd:?}, which has not been checked against Invariant 1"
            );
        }
    }

    #[test]
    fn a_build_that_resolves_its_own_modules_gets_no_install_step() {
        assert!(install_command("go.sum").is_none());
        assert!(install_command("Cargo.lock").is_none());
        let signals = Signals {
            lockfiles: vec!["go.sum".to_string()],
            ..Default::default()
        };
        assert!(install_proposal(&signals).is_none());
    }

    #[test]
    fn two_lockfiles_are_a_question() {
        let signals = Signals {
            lockfiles: vec![
                "pnpm-lock.yaml".to_string(),
                "package-lock.json".to_string(),
            ],
            ..Default::default()
        };
        let proposal = install_proposal(&signals).unwrap();
        assert_eq!(
            values(&proposal),
            vec!["pnpm install --frozen-lockfile", "npm ci"]
        );
        assert!(!proposal.decided, "pando does not guess which one is live");
    }

    // ---- workspaces ------------------------------------------------------

    /// A workspace with a web app and an api app, the shape the fixture
    /// catalogue's `mono-web-api` has.
    fn workspace(dir: &Path) {
        std::fs::write(
            dir.join("package.json"),
            r#"{ "workspaces": ["apps/*"], "scripts": { "dev": "pnpm -r --parallel dev" } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::create_dir_all(dir.join("apps/web")).unwrap();
        std::fs::write(
            dir.join("apps/web/package.json"),
            r#"{ "scripts": { "dev": "vite" } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("apps/web/vite.config.ts"), "export default {}\n").unwrap();
        std::fs::create_dir_all(dir.join("apps/api")).unwrap();
        std::fs::write(
            dir.join("apps/api/package.json"),
            r#"{ "scripts": { "dev": "node --watch src/index.js" } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".env.example"),
            "WEB_PORT=5173\nAPI_PORT=4000\nVITE_API_URL=http://localhost:4000\n\
             DATABASE_URL=postgres://app:app@localhost:5432/app\n",
        )
        .unwrap();
    }

    fn proposed_processes(root: &Path) -> Proposal {
        let signals = signals(root);
        propose(root, &signals)
            .into_iter()
            .find(|p| p.slot == Slot::Processes)
            .expect("a processes proposal")
    }

    #[test]
    fn a_workspace_proposes_one_process_per_app() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        let proposal = proposed_processes(dir.path());
        assert!(
            !proposal.decided,
            "two processes instead of one is the developer's call"
        );
        let processes = proposal.candidates[0]
            .processes
            .clone()
            .expect("the per-app form carries processes");
        assert_eq!(
            processes.keys().cloned().collect::<Vec<_>>(),
            vec!["api", "web"]
        );

        let web = &processes["web"];
        assert_eq!(web.cwd.as_deref(), Some("apps/web"));
        assert_eq!(
            web.cmd, "pnpm dev -- --port {port:web}",
            "Vite takes its port on the command line, so the flag goes on its own script"
        );
        assert_eq!(web.roles(), vec!["web"]);
        assert_eq!(
            web.env["VITE_API_URL"], "http://localhost:{port:api}",
            "a localhost URL pointing at another app becomes that app's role"
        );
        assert!(
            !web.env.contains_key("WEB_PORT"),
            "and nothing says the port twice: {:?}",
            web.env
        );
        assert_eq!(web.ready.clone().unwrap().role.as_deref(), Some("web"));

        let api = &processes["api"];
        assert_eq!(api.cwd.as_deref(), Some("apps/api"));
        assert_eq!(api.cmd, "pnpm dev");
        assert_eq!(
            api.env["PORT"], "{port:api}",
            "Node reads its port from the environment"
        );
        assert!(
            !api.env.contains_key("VITE_API_URL"),
            "the app a reference points at is the one that need not be told"
        );
        assert_eq!(api.ready.clone().unwrap().role.as_deref(), Some("api"));

        // The question shows each process with its directory and command.
        let summary = &proposal.candidates[0].value;
        for needle in ["api", "web", "apps/api", "apps/web", "pnpm dev"] {
            assert!(summary.contains(needle), "{summary}");
        }
    }

    #[test]
    fn the_root_script_is_offered_beside_the_per_app_form() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        let proposal = proposed_processes(dir.path());
        assert_eq!(proposal.candidates.len(), 2);
        let fallback = &proposal.candidates[1];
        assert_eq!(fallback.value, "pnpm dev");
        let processes = fallback.processes.clone().expect("one process");
        assert_eq!(processes.keys().cloned().collect::<Vec<_>>(), vec!["dev"]);
        assert_eq!(processes["dev"].cmd, "pnpm dev");
        assert!(
            processes["dev"].ports.is_none(),
            "declining leaves the port question to be asked"
        );
    }

    #[test]
    fn one_app_is_not_a_workspace_worth_splitting_up() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        std::fs::remove_dir_all(dir.path().join("apps/api")).unwrap();
        let signals = signals(dir.path());
        assert!(
            propose(dir.path(), &signals)
                .iter()
                .all(|p| p.slot != Slot::Processes),
            "one app with a dev script is the single-process case"
        );
    }

    #[test]
    fn an_app_with_no_dev_script_is_not_a_process() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        std::fs::create_dir_all(dir.path().join("apps/tools")).unwrap();
        std::fs::write(
            dir.path().join("apps/tools/package.json"),
            r#"{ "scripts": { "build": "tsc" } }"#,
        )
        .unwrap();
        let apps = workspace_apps(dir.path(), &signals(dir.path()));
        assert_eq!(
            apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            vec!["api", "web"]
        );
    }

    // Two apps with one directory name would claim one role, and
    // `config::validate` would refuse the file pando had just written.
    #[test]
    fn two_apps_with_the_same_name_are_not_proposed_at_all() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "workspaces": ["apps/*", "packages/*"] }"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("packages/web")).unwrap();
        std::fs::write(
            dir.path().join("packages/web/package.json"),
            r#"{ "scripts": { "dev": "vite" } }"#,
        )
        .unwrap();
        assert!(workspace_apps(dir.path(), &signals(dir.path())).is_empty());
    }

    #[test]
    fn a_repository_that_is_not_a_workspace_proposes_nothing() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "dev": "next dev" } }"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        std::fs::write(
            dir.path().join("apps/web/package.json"),
            r#"{ "scripts": { "dev": "vite" } }"#,
        )
        .unwrap();
        assert!(
            workspace_apps(dir.path(), &signals(dir.path())).is_empty(),
            "an apps/ directory is not a workspace; the manifest has to say so"
        );
    }

    #[test]
    fn workspace_globs_are_read_from_every_convention() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'apps/*'\n",
        )
        .unwrap();
        assert_eq!(workspace_globs(dir.path()), vec!["apps/*"]);

        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "workspaces": { "packages": ["services/*"] } }"#,
        )
        .unwrap();
        assert_eq!(workspace_globs(dir.path()), vec!["services/*"]);

        // turbo and nx describe pipelines, not membership.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("turbo.json"), "{}").unwrap();
        assert_eq!(workspace_globs(dir.path()), vec!["apps/*", "packages/*"]);
    }

    #[test]
    fn only_a_localhost_url_names_another_apps_port() {
        assert_eq!(localhost_url_port("http://localhost:4000"), Some(4000));
        assert_eq!(
            localhost_url_port("http://127.0.0.1:4000/api/v1"),
            Some(4000)
        );
        assert_eq!(
            localhost_url_port("postgres://app:app@localhost:5432/app"),
            Some(5432)
        );
        assert_eq!(localhost_url_port("https://api.example.com:443"), None);
        assert_eq!(localhost_url_port("http://localhost"), None);
        assert_eq!(localhost_url_port("4000"), None);
    }

    #[test]
    fn a_url_that_matches_no_app_is_left_alone() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        let processes = proposed_processes(dir.path()).candidates[0]
            .processes
            .clone()
            .unwrap();
        for (name, process) in &processes {
            assert!(
                !process.env.contains_key("DATABASE_URL"),
                "{name} was told about a database that is not one of the apps: {:?}",
                process.env
            );
        }
    }

    #[test]
    fn the_processes_slot_writes_a_table_per_app() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        let candidate = proposed_processes(dir.path()).candidates[0].clone();
        let edits = edits(Slot::Processes, &candidate);
        let web: Vec<(&str, String)> = edits
            .iter()
            .filter(|e| e.table == vec!["processes", "web"])
            .map(|e| (e.key.as_str(), e.value.to_string().trim().to_string()))
            .collect();
        assert_eq!(
            web,
            vec![
                ("cmd", "\"pnpm dev -- --port {port:web}\"".to_string()),
                ("cwd", "\"apps/web\"".to_string()),
                ("ports", "[\"web\"]".to_string()),
                (
                    "env",
                    "{ VITE_API_URL = \"http://localhost:{port:api}\" }".to_string()
                ),
                ("ready", "{ role = \"web\" }".to_string()),
            ]
        );
        assert!(
            edits.iter().any(|e| e.table == vec!["processes", "api"]),
            "and one for the api"
        );
    }

    // The single-process fallback keeps the `[dev]` shorthand: that is
    // what it is for, and it is the shape every example is written in.
    #[test]
    fn the_single_process_answer_is_written_as_the_dev_shorthand() {
        let dir = tempdir().unwrap();
        workspace(dir.path());
        let candidate = proposed_processes(dir.path()).candidates[1].clone();
        let edits = edits(Slot::Processes, &candidate);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].table, vec!["dev"]);
        assert_eq!(edits[0].key, "cmd");
    }

    #[test]
    fn a_command_typed_at_the_process_question_is_one_process() {
        let candidate = custom(Slot::Processes, "./scripts/dev.sh --port {port:web}");
        let processes = candidate.processes.clone().expect("one process");
        assert_eq!(processes.keys().cloned().collect::<Vec<_>>(), vec!["dev"]);
        assert_eq!(processes["dev"].roles(), vec!["web"]);
        let mut config = Config::default();
        apply(Slot::Processes, &candidate, &mut config);
        assert_eq!(
            config.processes["dev"].cmd,
            "./scripts/dev.sh --port {port:web}"
        );
    }

    // ---- compose services ------------------------------------------------

    /// A repository with a compose file and an env example, which is all
    /// the services rule reads.
    fn compose_fixture(compose: &str, env: &[(&str, &str)]) -> (TempDir, Signals) {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("docker-compose.yml"), compose).unwrap();
        let signals = Signals {
            env_example: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        (dir, signals)
    }

    // The first-contact failure this rule exists for: the compose file's
    // only candidate was the app itself, so the only answer on offer was
    // "run a second copy of the thing you are developing".
    #[test]
    fn a_service_built_from_this_repository_is_not_a_dependency() {
        let (dir, signals) = compose_fixture(
            r#"
services:
  web:
    image: acme/web:dev
    build: .
    ports: ["3000:3000"]
  worker:
    build:
      context: ./services/worker
  postgres:
    image: postgres:16
    ports: ["5432:5432"]
"#,
            &[("DATABASE_URL", "postgres://acme@localhost:5432/acme")],
        );
        let proposal = services_proposal(dir.path(), &signals, None).unwrap();
        assert_eq!(
            values(&proposal),
            vec!["postgres"],
            "the app and its worker are this repository, not things it depends on"
        );
        assert!(
            proposal.decided,
            "one resolved dependency is not a question"
        );
    }

    // A question whose only answer is wrong is worse than silence — but the
    // answer still has to be recorded, or the next start works it out again
    // and nothing in the file says pando ever looked.
    #[test]
    fn a_compose_file_of_only_this_project_is_answered_without_being_asked() {
        let (dir, signals) = compose_fixture(
            "services:\n  web:\n    build: .\n  worker:\n    build: ./worker\n",
            &[],
        );
        let proposal = services_proposal(dir.path(), &signals, None).unwrap();
        assert!(
            proposal.candidates.is_empty(),
            "nothing survived the filter"
        );
        assert!(proposal.decided, "so there is nothing to ask about");
        assert_eq!(
            proposal.service_file(),
            Some("docker-compose.yml"),
            "the empty answer is still about a file, and has to be writable"
        );
        let why = proposal
            .none_because
            .expect("a reason for the empty answer");
        assert!(why.contains("web and worker"), "{why}");
        assert!(why.contains("built from this repository"), "{why}");
    }

    // The empty answer is permanent — `already_answered` is true from then
    // on — so it may only be recorded about a file pando read whole. A
    // top-level `include:` brings in services this list does not even have,
    // and "none of them" about those is an answer nobody gave.
    #[test]
    fn a_file_pando_could_not_read_whole_records_no_empty_answer() {
        let (dir, signals) = compose_fixture(
            r#"
include:
  - infra/compose.yml
services:
  web:
    build: .
"#,
            &[],
        );
        assert!(
            services_proposal(dir.path(), &signals, None).is_none(),
            "nothing is offered and nothing is written down: the next run \
             with Docker present gets to decide"
        );
    }

    // The filter is about *this project*, not about `build:` existing. A
    // service built out of a sibling checkout is a dependency like any
    // other, and a worktree wants its own copy.
    #[test]
    fn a_build_context_outside_the_project_is_still_a_dependency() {
        let (dir, signals) = compose_fixture(
            r#"
services:
  vendor:
    build: ../vendor-service
    ports: ["9000:9000"]
"#,
            &[("VENDOR_URL", "http://localhost:9000")],
        );
        let proposal = services_proposal(dir.path(), &signals, None).unwrap();
        assert_eq!(values(&proposal), vec!["vendor"]);
        assert!(proposal.preferred().unwrap().preselected);
    }

    #[test]
    fn a_build_context_is_resolved_against_the_compose_files_own_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("deploy")).unwrap();
        for (file, context, inside) in [
            ("docker-compose.yml", ".", true),
            ("docker-compose.yml", "./services/api", true),
            ("docker-compose.yml", "../elsewhere", false),
            // From a file one level down, `..` is still the project.
            ("deploy/docker-compose.yml", "..", true),
            ("deploy/docker-compose.yml", "../..", false),
        ] {
            assert_eq!(
                built_from_project(root, file, context),
                inside,
                "{file} + {context}"
            );
        }
        // What `docker compose config` hands over: already absolute.
        assert!(built_from_project(
            root,
            "docker-compose.yml",
            root.join("apps/api").to_str().unwrap()
        ));
        assert!(!built_from_project(root, "docker-compose.yml", "/tmp"));
    }

    // ---- what detection may fill in --------------------------------------

    #[test]
    fn a_lone_dev_with_no_command_may_be_filled_and_anything_else_may_not() {
        let mut empty = Config::default();
        assert!(
            may_fill_dev(&empty),
            "nothing configured is pando's to fill"
        );

        empty.processes.insert(
            DEV.to_string(),
            ProcessConfig {
                cwd: Some("apps/web".to_string()),
                ..Default::default()
            },
        );
        assert!(
            may_fill_dev(&empty),
            "a [dev] with no command is an invitation"
        );
        assert!(still_needed(Slot::DevCmd, &empty));
        assert!(still_needed(Slot::PortEnv, &empty));
        assert!(
            !still_needed(Slot::Processes, &empty),
            "but the shape question has been answered by writing [dev] at all"
        );

        let mut written = Config::default();
        written.processes.insert(
            DEV.to_string(),
            ProcessConfig {
                cmd: "sleep 300".to_string(),
                ..Default::default()
            },
        );
        assert!(
            !may_fill_dev(&written),
            "a command the developer wrote is an answer about its ports too"
        );

        let mut named = Config::default();
        named.processes.insert(
            "web".to_string(),
            ProcessConfig {
                cmd: "vite".to_string(),
                ..Default::default()
            },
        );
        assert!(
            !may_fill_dev(&named),
            "a process under another name means [dev] would land beside [processes]"
        );

        // And after an answer to the shape question.
        let mut per_app = Config::default();
        per_app
            .processes
            .insert("web".to_string(), ProcessConfig::default());
        per_app
            .processes
            .insert("api".to_string(), ProcessConfig::default());
        assert!(!fills_one_dev_process(&per_app));
        let mut single = Config::default();
        single
            .processes
            .insert(DEV.to_string(), ProcessConfig::default());
        assert!(fills_one_dev_process(&single));
    }

    // ---- writing the answer ----------------------------------------------

    #[test]
    fn a_chosen_candidate_becomes_both_config_and_a_patch() {
        let candidate = Candidate {
            value: "PORT".to_string(),
            why: "the Next.js convention".to_string(),
            ..Candidate::default()
        };
        let mut config = Config::default();
        apply(Slot::PortEnv, &candidate, &mut config);
        assert_eq!(config.processes["dev"].roles(), vec!["web"]);
        assert_eq!(config.processes["dev"].port_env()["PORT"], "{port:web}");

        let edits = edits(Slot::PortEnv, &candidate);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].table, vec!["dev"]);
        assert_eq!(edits[0].key, "ports");
        assert_eq!(edits[0].value.to_string().trim(), "{ PORT = \"web\" }");
    }

    #[test]
    fn a_list_slot_round_trips_through_its_comma_separated_value() {
        let candidate = Candidate {
            value: ".env,.env.local".to_string(),
            why: "gitignored and present".to_string(),
            ..Candidate::default()
        };
        let mut config = Config::default();
        apply(Slot::Provision, &candidate, &mut config);
        assert_eq!(
            config.project.provision.as_deref(),
            Some(&[".env".to_string(), ".env.local".to_string()][..])
        );
        let edits = edits(Slot::Provision, &candidate);
        assert_eq!(
            edits[0].value.to_string().trim(),
            "[\".env\", \".env.local\"]"
        );
    }

    // ---- provisioning from an example ------------------------------------

    /// A repository that ships an example of a local file, with or without
    /// the local file itself.
    fn seed_fixture(files: &[(&str, &str)]) -> TempDir {
        let dir = tempdir().unwrap();
        crate::testutil::git(dir.path(), &["init", "--quiet", "--initial-branch=main"]);
        for (rel, contents) in files {
            std::fs::write(dir.path().join(rel), contents).unwrap();
        }
        dir
    }

    // The fresh-clone case: `.env` is gitignored so it never arrives, and
    // the example beside it is the only thing that says what it looks like.
    #[test]
    fn an_example_of_a_missing_gitignored_file_is_a_provision_source() {
        let dir = seed_fixture(&[
            (".gitignore", ".env\n.env.local\n"),
            (".env.example", "PORT=3000\n"),
            (".env.local.example", "FLAG=1\n"),
        ]);
        assert_eq!(
            provision_seeds(dir.path()),
            vec![
                (".env".to_string(), ".env.example".to_string()),
                (".env.local".to_string(), ".env.local.example".to_string()),
            ]
        );
    }

    #[test]
    fn a_file_the_checkout_already_has_is_not_seeded_from_its_example() {
        let dir = seed_fixture(&[
            (".gitignore", ".env\n"),
            (".env.example", "PORT=3000\n"),
            (".env", "PORT=3001\n"),
        ]);
        assert!(
            provision_seeds(dir.path()).is_empty(),
            "the developer's own file is the source; the example is the fallback"
        );
    }

    // `git check-ignore` is what authorises a write into a worktree, so a
    // destination it would refuse must never be offered — proposing a seed
    // `new` then refuses is worse than proposing none.
    #[test]
    fn an_example_of_a_file_that_is_not_gitignored_is_never_offered() {
        let dir = seed_fixture(&[
            (".gitignore", "node_modules/\n"),
            ("config.yml.example", "debug: true\n"),
        ]);
        assert!(provision_seeds(dir.path()).is_empty());
    }

    #[test]
    fn a_seed_makes_the_provision_slot_a_question_with_the_plain_answer_under_it() {
        let signals = Signals {
            ignored_present: vec![".env.local".to_string()],
            provision_seeds: vec![(".env".to_string(), ".env.example".to_string())],
            ..Default::default()
        };
        let proposal = provision_proposal(&signals).unwrap();
        assert!(
            !proposal.decided,
            "copying a tracked example into a worktree is the developer's call"
        );
        assert_eq!(values(&proposal), vec![".env.local", ".env.local,.env"]);

        let plain = proposal.preferred().unwrap();
        assert!(
            plain.provision_from.is_empty() && !plain.needs_a_human,
            "only what is already here leads, and it is what --yes takes"
        );

        let seeded = &proposal.candidates[1];
        assert_eq!(
            seeded.provision_from,
            BTreeMap::from([(".env".to_string(), ".env.example".to_string())])
        );
        assert!(
            seeded.needs_a_human,
            "copying a file pando did not write is not a flag's decision"
        );
        assert!(
            seeded.why.contains(".env copied from .env.example"),
            "the option names the source, so a human can go and read it: {}",
            seeded.why
        );
    }

    // The case the whole feature is for — a clone with nothing local at all
    // — has no plain answer to lead with, so there is nothing `--yes` may
    // take and the question is what an unattended run gets.
    #[test]
    fn a_clone_with_only_a_seed_to_offer_has_nothing_yes_may_take() {
        let signals = Signals {
            provision_seeds: vec![(".env".to_string(), ".env.example".to_string())],
            ..Default::default()
        };
        let proposal = provision_proposal(&signals).unwrap();
        assert_eq!(values(&proposal), vec![".env"]);
        assert!(proposal.candidates[0].needs_a_human);
        assert!(!proposal.decided);
    }

    #[test]
    fn with_nothing_to_seed_the_provision_slot_is_decided_as_it_always_was() {
        let signals = Signals {
            ignored_present: vec![".env".to_string(), ".env.local".to_string()],
            ..Default::default()
        };
        let proposal = provision_proposal(&signals).unwrap();
        assert!(proposal.decided);
        assert_eq!(values(&proposal), vec![".env,.env.local"]);
    }

    #[test]
    fn a_seeded_answer_writes_the_list_and_where_the_missing_file_comes_from() {
        let candidate = Candidate {
            value: ".env".to_string(),
            why: "seeded".to_string(),
            provision_from: BTreeMap::from([(".env".to_string(), ".env.example".to_string())]),
            ..Candidate::default()
        };
        let mut config = Config::default();
        apply(Slot::Provision, &candidate, &mut config);
        assert_eq!(config.project.provision_paths(), [".env".to_string()]);
        assert_eq!(config.project.provision_from[".env"], ".env.example");

        let edits = edits(Slot::Provision, &candidate);
        assert_eq!(edits.len(), 2, "the list, and where the file comes from");
        assert_eq!(edits[1].table, vec!["project"]);
        assert_eq!(edits[1].key, "provision_from");
        assert_eq!(
            edits[1].value.to_string().trim(),
            r#"{ ".env" = ".env.example" }"#
        );
    }

    #[test]
    fn a_command_that_carries_its_port_writes_both_keys() {
        let candidate = Candidate {
            value: "python manage.py runserver 127.0.0.1:{port:web}".to_string(),
            why: "the Django rule".to_string(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
            ..Candidate::default()
        };
        let edits = edits(Slot::DevCmd, &candidate);
        assert_eq!(edits.len(), 2, "the command and the role it needs");
        assert_eq!(edits[1].key, "ports");
        assert_eq!(edits[1].value.to_string().trim(), "[\"web\"]");
    }
}
