//! Signals: everything tier 1 can see in the main checkout.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::catalog::frameworks;
use crate::catalog::package_managers;

/// Everything tier 1 can see. Serialisable because `pando signals` prints it
/// for an agent to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signals {
    /// `package.json` scripts: name to body.
    pub scripts: BTreeMap<String, String>,
    /// Makefile or justfile targets: name to the target.
    pub targets: BTreeMap<String, Target>,
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

const WORKSPACE_MARKERS: [&str; 3] = ["pnpm-workspace.yaml", "turbo.json", "nx.json"];

/// Every file that pins a runtime version. Spelled out rather than derived
/// from [`crate::runtime::LANGUAGES`] because the order is written into
/// config as `runtime.version_files`, and doctor compares that array in
/// order: a reordering would call every config written before it stale. A
/// test holds this list to the table instead.
pub(super) const VERSION_FILES: [&str; 7] = [
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    "mise.toml",
    ".python-version",
    "rust-toolchain.toml",
    ".ruby-version",
];

const ENV_EXAMPLES: [&str; 3] = [".env.example", ".env.sample", ".env.template"];

/// The compose file names `signals` reports, in the order it reports them.
/// The same names as [`crate::compose::COMPOSE_FILES`], whose order is
/// compose's own precedence instead; `signals --json` publishes this order,
/// so the two stay separate lists and a test holds them to one set.
pub(super) const COMPOSE_FILES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];

/// Reads every tier 1 signal from the main checkout.
pub fn signals(root: &Path) -> Signals {
    let manifest = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    Signals {
        scripts: parse_scripts(&manifest),
        targets: parse_targets(root),
        lockfiles: present(root, &package_managers::lockfiles()),
        workspace_markers: present(root, &WORKSPACE_MARKERS),
        version_files: present(root, &VERSION_FILES),
        runtime_requirements: crate::runtime::requirements(root),
        env_example: env_example(root),
        markers: present(root, &frameworks::marker_files()),
        compose_files: present(root, &COMPOSE_FILES),
        ignored_present: ignored_present(root),
        provision_seeds: provision_seeds(root),
    }
}

pub(super) fn present(root: &Path, names: &[&str]) -> Vec<String> {
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
pub(super) fn parse_scripts(manifest: &str) -> BTreeMap<String, String> {
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

/// A `Makefile` or `justfile` target, as much of it as deciding how to run
/// it takes.
///
/// The whole recipe and the prerequisites, not one line: which of the two
/// ways to start a target is right — `make <target>` or the command itself
/// — cannot be decided from a fragment. See `target_candidates`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The tool that runs it: `make` or `just`.
    pub tool: String,
    /// What follows the colon: the targets that run *first*.
    #[serde(default)]
    pub prereqs: Vec<String>,
    /// Every command line of the recipe, in order, with continuations
    /// joined and the runner's own `@`, `-` and `+` prefixes left on —
    /// they are part of what the recipe says. Blank lines and comment-only
    /// lines are dropped: they are not commands.
    #[serde(default)]
    pub recipe: Vec<String>,
}

impl Target {
    /// The command line this target *is*, when proposing it instead of the
    /// runner loses nothing. See `target_candidates` for why the bar is
    /// this high.
    pub fn sole_command(&self) -> Option<&str> {
        if !self.prereqs.is_empty() {
            return None;
        }
        let [only] = self.recipe.as_slice() else {
            return None;
        };
        // `-` and `+` are directives about how the runner treats the
        // command rather than part of it; `@` only suppresses the echo,
        // which is what running the line directly does anyway.
        let body = only.trim_start_matches(['@', '-', '+']);
        if only[..only.len() - body.len()].contains(['-', '+']) {
            return None;
        }
        let runner_expands = match self.tool.as_str() {
            // just passes `$VAR` through to the shell unchanged; `{{ … }}`
            // is its own interpolation.
            "just" => body.contains("{{"),
            // In a makefile every `$` is make's, including `$$`, which is
            // how a makefile escapes one *for* the shell.
            _ => body.contains('$'),
        };
        (!runner_expands).then_some(body.trim())
    }

    /// What starts this target: its own command when it is representable by
    /// one, otherwise the runner and the target's name.
    pub fn command(&self, name: &str) -> String {
        match self.sole_command() {
            Some(command) => command.to_string(),
            None => format!("{} {name}", self.tool),
        }
    }
}

/// Targets of a `Makefile` or `justfile` that might start something.
pub(super) fn parse_targets(root: &Path) -> BTreeMap<String, Target> {
    let mut out: BTreeMap<String, Target> = BTreeMap::new();
    for (file, tool) in [
        ("Makefile", "make"),
        ("makefile", "make"),
        ("justfile", "just"),
        ("Justfile", "just"),
    ] {
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            // A target starts at column zero and its name is one word.
            if line.starts_with([' ', '\t']) {
                continue;
            }
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                continue;
            }
            // `.PHONY`, `.DEFAULT_GOAL` and the rest are directives to make
            // about other targets, never something to run.
            if name.starts_with('.') {
                continue;
            }
            // `x := 1` is an assignment, not a target.
            if rest.trim_start().starts_with('=') {
                continue;
            }
            // `dev:: …` is make's double-colon rule. The second colon is
            // part of the operator, not a prerequisite.
            let rest = rest.strip_prefix(':').unwrap_or(rest);
            // What follows the colon is the prerequisite list — the targets
            // that run *first* — in both make and just, never the recipe.
            // Only make's one-liner form, a semicolon after the
            // prerequisites, puts a command on the target line.
            let (before, inline) = match rest.split_once(';') {
                Some((before, command)) => (before, command.trim()),
                None => (rest, ""),
            };
            let mut recipe: Vec<String> = Vec::new();
            if !inline.is_empty() {
                recipe.push(inline.to_string());
            }
            recipe.extend(recipe_below(&lines, i));
            if recipe.is_empty() {
                // A target whose next line is not indented has no recipe.
                continue;
            }
            out.entry(name.to_string()).or_insert(Target {
                tool: tool.to_string(),
                prereqs: before.split_whitespace().map(str::to_string).collect(),
                recipe,
            });
        }
    }
    out
}

/// The indented recipe under the target on line `at`, one entry per command.
///
/// Four details of the format, each of which the one-line version got
/// wrong. A trailing `\` continues the command onto the next physical line,
/// however many times. Blank lines and comment-only lines sit *among*
/// recipe lines without ending the recipe, and are not commands — except a
/// `#!` shebang, which is just's way of handing the whole recipe to another
/// interpreter and is very much part of it. A comment may also be at column
/// zero and still not end the recipe. Everything else at column zero does.
fn recipe_below(lines: &[&str], at: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut continued: Option<String> = None;
    for line in lines.iter().skip(at + 1) {
        let trimmed = line.trim();
        // A continuation swallows the next physical line whatever it is,
        // including one that would otherwise end the recipe.
        if let Some(mut started) = continued.take() {
            started.push(' ');
            match trimmed.strip_suffix('\\') {
                Some(head) => {
                    started.push_str(head.trim_end());
                    continued = Some(started);
                }
                None => {
                    started.push_str(trimmed);
                    out.push(started);
                }
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            if trimmed.starts_with('#') {
                continue;
            }
            break;
        }
        if is_comment_line(trimmed) {
            continue;
        }
        match trimmed.strip_suffix('\\') {
            Some(head) => continued = Some(head.trim_end().to_string()),
            None => out.push(trimmed.to_string()),
        }
    }
    out.extend(continued);
    out
}

/// A recipe line that is only a comment, `@#` included. A `#!` shebang is
/// not one: just runs the recipe through it.
fn is_comment_line(trimmed: &str) -> bool {
    let body = trimmed.trim_start_matches(['@', '-', '+']);
    body.starts_with('#') && !body.starts_with("#!")
}

impl Signals {
    /// The env example's keys, for the rules that only care about names.
    pub fn env_keys(&self) -> impl Iterator<Item = &str> {
        self.env_example.iter().map(|(key, _)| key.as_str())
    }
}

/// Entries of the first env example file that exists, in file order.
pub(super) fn env_example(root: &Path) -> Vec<(String, String)> {
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
pub(super) fn provision_seeds(root: &Path) -> Vec<(String, String)> {
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
pub fn is_gitignored(root: &Path, rel: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--", rel])
        .output()
        .map(|out| out.status.code() == Some(0))
        .unwrap_or(false)
}
