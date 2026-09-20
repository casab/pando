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

use crate::config::{Config, PortsSpec};

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
    pub version_files: Vec<String>,
    /// Keys from `.env.example` and friends, in file order.
    pub env_example_keys: Vec<String>,
    /// Framework marker files that exist.
    pub markers: Vec<String>,
    pub compose_files: Vec<String>,
    /// Root-level files that are gitignored and present — what a new
    /// worktree would be missing.
    pub ignored_present: Vec<String>,
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
        env_example_keys: env_example_keys(root),
        markers: present(root, &MARKER_FILES),
        compose_files: present(root, &COMPOSE_FILES),
        ignored_present: ignored_present(root),
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

/// Keys of the first env example file that exists, in file order.
fn env_example_keys(root: &Path) -> Vec<String> {
    for name in ENV_EXAMPLES {
        let Ok(text) = std::fs::read_to_string(root.join(name)) else {
            continue;
        };
        return text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .filter(|k| !k.is_empty())
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
    },
    FrameworkRule {
        name: "Nuxt",
        markers: &["nuxt.config.ts"],
        script_markers: &["nuxt dev"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: Some("npx nuxt dev"),
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
    },
    FrameworkRule {
        name: "Django",
        markers: &["manage.py"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("{runner}python manage.py runserver 127.0.0.1:{port:web}"),
    },
    FrameworkRule {
        name: "Rails",
        markers: &["config.ru", "bin/dev"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 3000,
        command: Some("bin/rails server -p {port:web}"),
    },
    FrameworkRule {
        name: "Phoenix",
        markers: &["mix.exs"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 4000,
        command: Some("mix phx.server"),
    },
    FrameworkRule {
        name: "Laravel",
        markers: &["artisan"],
        script_markers: &[],
        port: PortMechanism::InCommand,
        default_port: 8000,
        command: Some("php artisan serve --port {port:web}"),
    },
    FrameworkRule {
        name: "Go",
        markers: &["go.mod"],
        script_markers: &[],
        port: PortMechanism::Env("PORT"),
        default_port: 8080,
        command: Some("go run ."),
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
    },
    FrameworkRule {
        name: "Node",
        markers: &[],
        script_markers: &["node ", "nodemon", "tsx ", "ts-node", "fastify", "express"],
        port: PortMechanism::Env("PORT"),
        default_port: 3000,
        command: None,
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
    DevCmd,
    PortEnv,
    Provision,
}

impl Slot {
    /// Where the answer is written, as a table path plus a key.
    pub fn key(self) -> (&'static [&'static str], &'static str) {
        match self {
            Slot::Install => (&["project"], "install"),
            Slot::VersionFiles => (&["runtime"], "version_files"),
            Slot::DevCmd => (&["dev"], "cmd"),
            Slot::PortEnv => (&["dev"], "ports"),
            Slot::Provision => (&["project"], "provision"),
        }
    }

    /// The question asked when the rules cannot decide.
    pub fn prompt(self) -> &'static str {
        match self {
            Slot::Install => "Which command installs this project's dependencies?",
            Slot::VersionFiles => "Which file pins this project's runtime version?",
            Slot::DevCmd => "Which command starts the local development server?",
            Slot::PortEnv => "Which environment variable sets the web server's port?",
            Slot::Provision => "Which local files should each worktree get a copy of?",
        }
    }
}

/// One thing a rule found, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    /// The signal that made this a candidate, written into the config as
    /// `# detected: <why>`.
    pub why: String,
    /// Roles this candidate brings with it, for a command that already
    /// carries `{port:<role>}`. When set, the port slot is already answered.
    pub ports: Option<PortsSpec>,
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
}

impl Proposal {
    pub fn preferred(&self) -> Option<&Candidate> {
        self.candidates.first()
    }
}

/// Everything tier 1 has to say, in the order the slots are filled.
pub fn propose(root: &Path, signals: &Signals) -> Vec<Proposal> {
    let rule = framework(root, signals);
    [
        install_proposal(signals),
        version_files_proposal(signals),
        dev_cmd_proposal(signals, rule),
        port_proposal(signals, rule),
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
            ports: None,
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let decided = candidates.len() == 1;
    Some(Proposal {
        slot: Slot::Install,
        candidates,
        decided,
    })
}

fn version_files_proposal(signals: &Signals) -> Option<Proposal> {
    if signals.version_files.is_empty() {
        return None;
    }
    // Informational only — every file that pins a version is worth showing,
    // so there is nothing to choose between.
    Some(Proposal {
        slot: Slot::VersionFiles,
        candidates: vec![Candidate {
            value: signals.version_files.join(","),
            why: signals.version_files.join(", "),
            ports: None,
        }],
        decided: true,
    })
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
            ports: None,
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
        });
    }

    dedup_by_value(&mut candidates);
    if candidates.is_empty() {
        // A framework pando recognises but has no command shape for: it
        // knows there is a server here and not how to start it, which is a
        // question, and the only slot in this phase that has one with no
        // options to offer.
        return rule.map(|_| Proposal {
            slot: Slot::DevCmd,
            candidates: Vec::new(),
            decided: false,
        });
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
    Some(Proposal {
        slot: Slot::DevCmd,
        candidates,
        decided,
    })
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
                ports: None,
            });
        }
    }
    out
}

/// Suffixes that name a service's port, never the web server's.
const PORT_DENYLIST: [&str; 8] = [
    "DB_PORT",
    "SMTP_PORT",
    "REDIS_PORT",
    "MAIL_PORT",
    "PG_PORT",
    "MYSQL_PORT",
    "POSTGRES_PORT",
    "DATABASE_PORT",
];

fn is_service_port(key: &str) -> bool {
    PORT_DENYLIST.iter().any(|deny| key.ends_with(deny))
}

fn port_proposal(signals: &Signals, rule: Option<&'static FrameworkRule>) -> Option<Proposal> {
    let framework_env = rule.and_then(|r| match r.port {
        PortMechanism::Env(name) => Some((name, r.name)),
        _ => None,
    });

    let mut candidates: Vec<Candidate> = Vec::new();
    // The framework's own mechanism first: it is a rule, not a guess.
    if let Some((name, framework)) = framework_env {
        candidates.push(Candidate {
            value: name.to_string(),
            why: format!("the {framework} convention"),
            ports: None,
        });
    }
    // Then `PORT`, then anything else port-shaped that is not a service's.
    let keys = signals.env_example_keys.iter();
    let mut others: Vec<&String> = Vec::new();
    for key in keys {
        // Exactly `PORT`, or a `<SOMETHING>_PORT`. A substring test also
        // matches `SUPPORT_EMAIL`, `REPORT_URL`, `IMPORT_PATH` and
        // `PASSPORT_SECRET`, which is not only nonsense to offer: it turns
        // a slot that resolved silently into a question, and a
        // non-interactive `start` that exited 0 into an exit 3.
        if !(key == "PORT" || key.ends_with("_PORT")) || is_service_port(key) {
            continue;
        }
        if key == "PORT" {
            candidates.insert(
                framework_env.is_some().into(),
                Candidate {
                    value: key.clone(),
                    why: "PORT in the env example".to_string(),
                    ports: None,
                },
            );
        } else {
            others.push(key);
        }
    }
    candidates.extend(others.into_iter().map(|key| Candidate {
        value: key.clone(),
        why: format!("{key} in the env example"),
        ports: None,
    }));
    dedup_by_value(&mut candidates);
    if candidates.is_empty() {
        return None;
    }
    let decided = candidates.len() == 1;
    Some(Proposal {
        slot: Slot::PortEnv,
        candidates,
        decided,
    })
}

fn provision_proposal(signals: &Signals) -> Option<Proposal> {
    if signals.ignored_present.is_empty() {
        return None;
    }
    Some(Proposal {
        slot: Slot::Provision,
        candidates: vec![Candidate {
            value: signals.ignored_present.join(","),
            why: "gitignored and present in the main checkout".to_string(),
            ports: None,
        }],
        decided: true,
    })
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
        // Unset, not empty. A process configured with no ports at all — a
        // worker, a watcher, a queue consumer — has answered this question
        // with `ports = []`, and asking again would hand it a port it will
        // never bind and then call it failed for not binding it.
        Slot::PortEnv => config.processes.get(DEV).is_none_or(|p| p.ports.is_none()),
        _ => true,
    }
}

/// A value the developer typed rather than chose.
///
/// A command carrying `{port:web}` brings its roles with it, so the port
/// question that would have followed is already answered.
pub fn custom(slot: Slot, value: &str) -> Candidate {
    let roles = roles_in(value);
    Candidate {
        value: value.to_string(),
        why: String::new(),
        ports: (slot == Slot::DevCmd && !roles.is_empty()).then_some(PortsSpec::List(roles)),
    }
}

/// Writes a chosen candidate into a config. The one place that knows what
/// each slot means, shared by the resolver and the tests.
pub fn apply(slot: Slot, candidate: &Candidate, config: &mut Config) {
    match slot {
        Slot::Install => config.project.install = Some(candidate.value.clone()),
        Slot::VersionFiles => config.runtime.version_files = split_list(&candidate.value),
        Slot::Provision => config.project.provision = split_list(&candidate.value),
        Slot::DevCmd => {
            let process = config.processes.entry(DEV.to_string()).or_default();
            process.cmd = candidate.value.clone();
            if let Some(ports) = &candidate.ports {
                process.ports = Some(ports.clone());
            }
        }
        Slot::PortEnv => {
            let process = config.processes.entry(DEV.to_string()).or_default();
            process.ports = Some(PortsSpec::Map(BTreeMap::from([(
                candidate.value.clone(),
                crate::config::WEB_ROLE.to_string(),
            )])));
        }
    }
}

/// The same choice, as the keys to patch into `pando.toml`.
///
/// Separate from [`apply`] because the file is edited in place rather than
/// re-serialised from the struct: the developer's comments and ordering
/// survive, and only the keys pando decided are touched.
pub fn edits(
    slot: Slot,
    candidate: &Candidate,
) -> Vec<(&'static [&'static str], &'static str, toml_edit::Value)> {
    let (table, key) = slot.key();
    match slot {
        Slot::Install | Slot::DevCmd => {
            let mut out: Vec<(&'static [&'static str], &'static str, toml_edit::Value)> =
                vec![(table, key, candidate.value.clone().into())];
            if let Some(PortsSpec::List(roles)) = &candidate.ports {
                out.push((
                    &["dev"],
                    "ports",
                    toml_edit::Value::Array(toml_edit::Array::from_iter(roles.iter().cloned())),
                ));
            }
            out
        }
        Slot::VersionFiles | Slot::Provision => vec![(
            table,
            key,
            toml_edit::Value::Array(toml_edit::Array::from_iter(split_list(&candidate.value))),
        )],
        Slot::PortEnv => {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert(&candidate.value, crate::config::WEB_ROLE.into());
            vec![(table, key, toml_edit::Value::InlineTable(inline))]
        }
    }
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
        assert_eq!(env_example_keys(dir.path()), vec!["PORT", "DB_PORT"]);
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
            env_example_keys: ["PORT", "API_PORT", "DB_PORT", "SMTP_PORT", "REDIS_PORT"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
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
            env_example_keys: [
                "PORT",
                "SUPPORT_EMAIL",
                "REPORT_URL",
                "IMPORT_PATH",
                "EXPORT_DIR",
                "PASSPORT_SECRET",
                "API_PORT",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
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

    // ---- writing the answer ----------------------------------------------

    #[test]
    fn a_chosen_candidate_becomes_both_config_and_a_patch() {
        let candidate = Candidate {
            value: "PORT".to_string(),
            why: "the Next.js convention".to_string(),
            ports: None,
        };
        let mut config = Config::default();
        apply(Slot::PortEnv, &candidate, &mut config);
        assert_eq!(config.processes["dev"].roles(), vec!["web"]);
        assert_eq!(config.processes["dev"].port_env()["PORT"], "{port:web}");

        let edits = edits(Slot::PortEnv, &candidate);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, &["dev"]);
        assert_eq!(edits[0].1, "ports");
        assert_eq!(edits[0].2.to_string().trim(), "{ PORT = \"web\" }");
    }

    #[test]
    fn a_list_slot_round_trips_through_its_comma_separated_value() {
        let candidate = Candidate {
            value: ".env,.env.local".to_string(),
            why: "gitignored and present".to_string(),
            ports: None,
        };
        let mut config = Config::default();
        apply(Slot::Provision, &candidate, &mut config);
        assert_eq!(config.project.provision, vec![".env", ".env.local"]);
        let edits = edits(Slot::Provision, &candidate);
        assert_eq!(edits[0].2.to_string().trim(), "[\".env\", \".env.local\"]");
    }

    #[test]
    fn a_command_that_carries_its_port_writes_both_keys() {
        let candidate = Candidate {
            value: "python manage.py runserver 127.0.0.1:{port:web}".to_string(),
            why: "the Django rule".to_string(),
            ports: Some(PortsSpec::List(vec!["web".to_string()])),
        };
        let edits = edits(Slot::DevCmd, &candidate);
        assert_eq!(edits.len(), 2, "the command and the role it needs");
        assert_eq!(edits[1].1, "ports");
        assert_eq!(edits[1].2.to_string().trim(), "[\"web\"]");
    }
}
