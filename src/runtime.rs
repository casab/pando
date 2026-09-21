//! What a project says about the runtime it needs.
//!
//! The split this module exists for: a version file is a *project* fact —
//! it is the same for everyone who clones the repository — while the
//! version manager that satisfies it is a fact about one laptop. This
//! module owns the first half, and the table it owns is shaped so the
//! second half (the probe, in `actions`) reads the same entries instead of
//! growing its own copy of what a language is.
//!
//! The table is a `const` array rather than a stack of match arms, so a
//! later phase can load the same shape from disk.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One thing the project says it needs, and where it says it.
///
/// Serde-friendly because `signals --json` publishes it: this is the fact
/// an agent reads to know which toolchain a repository wants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// What the requirement is about: a language in [`LANGUAGES`], or the
    /// key an `engines` block used for something that is not one, such as
    /// `pnpm`. Normalised, so `.tool-versions`' `nodejs` is `node` here.
    pub language: String,
    /// Exactly as the project wrote it: `22`, `>=18 <21`, `lts/*`.
    pub spec: String,
    /// The file it came from, with the key when the file holds several:
    /// `.nvmrc`, `.tool-versions`, `package.json engines.node`.
    pub source: String,
    /// Whether the spec names one version rather than a range. A pin beats
    /// a range when a project states both, which is why this is recorded
    /// rather than worked out again at every call site.
    pub pinned: bool,
}

impl Requirement {
    fn new(language: &str, spec: &str, source: String) -> Requirement {
        Requirement {
            language: language.to_string(),
            spec: spec.to_string(),
            source,
            pinned: is_pin(spec),
        }
    }
}

/// How the spec is dug out of a file that pins one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The file is the version, give or take a comment, a `v`, and a
    /// toolchain vendor prefix.
    Plain,
    /// `[toolchain] channel = "1.75.0"`, the `rust-toolchain.toml` shape.
    TomlKey(&'static str, &'static str),
}

/// A file that states one language's requirement.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub file: &'static str,
    pub kind: SourceKind,
}

/// Everything pando knows about one language.
///
/// Entries carry the requirement sources now; the probe command, the
/// version parser and the manager families join them in the same struct,
/// so one lookup answers every question a call site has.
#[derive(Debug, Clone, Copy)]
pub struct Language {
    /// The name used in a `Requirement`, in a report, and in config.
    pub name: &'static str,
    /// Files that pin this language, in the order they are read.
    pub files: &'static [Source],
    /// What it is called in `.tool-versions` and `mise.toml`, where one
    /// file names every tool. `nodejs` and `golang` live here.
    pub aliases: &'static [&'static str],
    /// The `engines` key that describes it, when there is one.
    pub engines_key: Option<&'static str>,
    /// The binaries a project's own commands run, in the order to try
    /// them: a Python project may have `python3`, `python`, or both.
    pub binaries: &'static [&'static str],
    /// What to ask one for its version. Its output goes through
    /// [`first_version`], which every language in the table shares.
    pub version_flag: &'static str,
    /// Who can satisfy this language on a machine, in the order a
    /// question offers them.
    pub managers: &'static [Manager],
}

impl Language {
    /// Whether a `.tool-versions` or `mise.toml` entry is about this
    /// language.
    fn owns(&self, tool: &str) -> bool {
        let tool = tool.trim().to_ascii_lowercase();
        self.name == tool || self.aliases.contains(&tool.as_str())
    }
}

/// The languages pando can read a requirement for.
///
/// Adding one is an entry here and nothing else: no call site enumerates
/// languages, and Phase 6's recipe loader is meant to append to this from
/// disk rather than replace it.
pub const LANGUAGES: [Language; 6] = [
    Language {
        name: "node",
        files: &[
            Source {
                file: ".nvmrc",
                kind: SourceKind::Plain,
            },
            Source {
                file: ".node-version",
                kind: SourceKind::Plain,
            },
        ],
        aliases: &["nodejs"],
        engines_key: Some("node"),
        binaries: &["node"],
        version_flag: "-v",
        managers: &[VOLTA, MISE, ASDF, NVM, FNM],
    },
    Language {
        name: "python",
        files: &[Source {
            file: ".python-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["python3", "python"],
        version_flag: "-V",
        managers: &[PYENV, MISE, ASDF],
    },
    Language {
        name: "ruby",
        files: &[Source {
            file: ".ruby-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["ruby"],
        version_flag: "-v",
        managers: &[RBENV, MISE, ASDF, RVM],
    },
    Language {
        name: "rust",
        files: &[Source {
            file: "rust-toolchain.toml",
            kind: SourceKind::TomlKey("toolchain", "channel"),
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["rustc"],
        version_flag: "-V",
        managers: &[RUSTUP, MISE, ASDF],
    },
    Language {
        name: "go",
        files: &[],
        aliases: &["golang"],
        engines_key: None,
        binaries: &["go"],
        version_flag: "version",
        managers: &[MISE, ASDF],
    },
    Language {
        name: "java",
        files: &[],
        aliases: &[],
        engines_key: None,
        binaries: &["java"],
        version_flag: "-version",
        managers: &[JENV, MISE, ASDF, SDKMAN],
    },
];

/// Files that state several languages at once, read once each.
const TOOL_VERSIONS: &str = ".tool-versions";
const MISE_FILE: &str = "mise.toml";
const MANIFEST: &str = "package.json";

/// Everything the repository says about the runtimes it needs.
///
/// Read-only, and pure file reading: it runs on the start path before
/// anything is spawned, so it may not shell out.
///
/// Within one language a pin sorts before a range, which is what "a pinned
/// file beats a range" means in practice: the first entry for a language is
/// the one to compare against, and the rest are still recorded because a
/// report that says `.nvmrc` wants 22 *and* `engines` wants `>=18` is the
/// one that explains itself.
pub fn requirements(root: &Path) -> Vec<Requirement> {
    let tool_versions = read_tool_versions(root);
    let mise = read_mise(root);
    let engines = read_engines(root);
    let mut out: Vec<Requirement> = Vec::new();

    for language in LANGUAGES {
        let mut found: Vec<Requirement> = Vec::new();
        for source in language.files {
            if let Some(spec) = read_source(root, source) {
                found.push(Requirement::new(
                    language.name,
                    &spec,
                    source.file.to_string(),
                ));
            }
        }
        for (file, entries) in [(TOOL_VERSIONS, &tool_versions), (MISE_FILE, &mise)] {
            for (tool, spec) in entries {
                if language.owns(tool) {
                    found.push(Requirement::new(language.name, spec, file.to_string()));
                }
            }
        }
        if let Some(key) = language.engines_key
            && let Some(spec) = engines.get(key)
        {
            found.push(Requirement::new(
                language.name,
                spec,
                format!("{MANIFEST} engines.{key}"),
            ));
        }
        // Stable: a pin first, and otherwise the order the files were read
        // in, which is the order the table lists them.
        found.sort_by_key(|r| !r.pinned);
        out.extend(found);
    }

    // An `engines` key no language in the table claims — `npm`, `pnpm`,
    // `yarn` — is still something the project stated, and `signals --json`
    // publishes it. Nothing probes it, because the table has no entry that
    // says how.
    for (key, spec) in &engines {
        if !LANGUAGES
            .iter()
            .any(|language| language.engines_key == Some(key.as_str()))
        {
            out.push(Requirement::new(
                key,
                spec,
                format!("{MANIFEST} engines.{key}"),
            ));
        }
    }
    out
}

/// The requirement to compare a language against: its pin if it has one,
/// else the first range anything stated.
pub fn for_language<'a>(
    requirements: &'a [Requirement],
    language: &str,
) -> Option<&'a Requirement> {
    requirements.iter().find(|r| r.language == language)
}

fn read_source(root: &Path, source: &Source) -> Option<String> {
    let text = std::fs::read_to_string(root.join(source.file)).ok()?;
    match source.kind {
        SourceKind::Plain => first_meaningful_line(&text),
        SourceKind::TomlKey(table, key) => Some(
            toml::from_str::<toml::Table>(&text)
                .ok()?
                .get(table)?
                .get(key)?
                .as_str()?
                .trim()
                .to_string(),
        )
        .filter(|s| !s.is_empty()),
    }
}

/// The first line that is neither blank nor a comment. `.python-version`
/// may hold several versions; the first is the one that is used.
fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
}

/// `<tool> <version> [fallback…]` lines, asdf's and mise's shared format.
fn read_tool_versions(root: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(root.join(TOOL_VERSIONS)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let tool = parts.next()?;
            // Only the first: the rest are fallbacks, and a fallback is not
            // what the project is asking for.
            let spec = parts.next()?;
            Some((tool.to_ascii_lowercase(), spec.to_string()))
        })
        .collect()
}

/// `[tools]` in `mise.toml`, whose values are a string, a list, or a table
/// with a `version` key.
fn read_mise(root: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(root.join(MISE_FILE)) else {
        return Vec::new();
    };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else {
        return Vec::new();
    };
    let Some(tools) = table.get("tools").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|(tool, value)| Some((tool.to_ascii_lowercase(), mise_spec(value)?)))
        .collect()
}

fn mise_spec(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(s) => Some(s.trim().to_string()),
        toml::Value::Array(items) => items.first().and_then(mise_spec),
        toml::Value::Table(table) => table.get("version").and_then(mise_spec),
        _ => None,
    }
}

/// `engines` in `package.json`: the one requirement source that is a range
/// by convention, and the one nothing read until now.
fn read_engines(root: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(root.join(MANIFEST)) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeMap::new();
    };
    let Some(engines) = value.get("engines").and_then(|e| e.as_object()) else {
        return BTreeMap::new();
    };
    engines
        .iter()
        .filter_map(|(key, value)| {
            let spec = value.as_str()?.trim();
            (!spec.is_empty()).then(|| (key.to_ascii_lowercase(), spec.to_string()))
        })
        .collect()
}

/// A spec stripped of the decoration a version file is allowed to carry: a
/// leading `v`, an `=`, and a vendor prefix such as `ruby-` or `temurin-`.
///
/// Shared by the pin test and, in the next work item, by the comparison, so
/// both agree on what a version even is.
pub fn normalize_spec(spec: &str) -> &str {
    let spec = spec.trim();
    // `temurin-21.0.1`, `ruby-3.2.2`: a vendor name, then the version.
    let spec = match spec.split_once('-') {
        Some((prefix, rest))
            if !prefix.is_empty()
                && prefix.chars().all(|c| c.is_ascii_alphabetic())
                && rest.starts_with(|c: char| c.is_ascii_digit()) =>
        {
            rest
        }
        _ => spec,
    };
    spec.trim_start_matches(['=', 'v', 'V'])
}

/// Whether a spec names one version rather than a range or an alias.
fn is_pin(spec: &str) -> bool {
    let spec = normalize_spec(spec);
    !spec.is_empty()
        && spec
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

// ---- what this machine actually resolves ----------------------------------

/// How the probe reaches a shell.
///
/// `actions` passes one that runs `bash -lc` exactly as a spawn does, so
/// what is measured is what will run; tests pass a fake. `None` means the
/// command could not be run at all, which is never a mismatch: a runtime
/// check that guesses is worse than no check.
pub type Shell<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Markers the probe prints its findings behind, so a chatty prelude —
/// `nvm use` says which version it took — cannot be mistaken for output.
const PATH_MARK: &str = "pando-runtime-path:";
const VERSION_MARK: &str = "pando-runtime-version:";
const DONE_MARK: &str = "pando-runtime-ok";

/// What the shell pando will really use resolves for one language.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The absolute path `command -v` printed.
    ///
    /// The reason this is recorded at all: pando's shell is not the
    /// developer's shell. "node 24" is not a diagnosis when everything
    /// works by hand; `/opt/homebrew/bin/node` is.
    pub path: Option<String>,
    /// The version that binary reported.
    pub version: Option<String>,
    /// Whether the probe body ran. False means the prelude in front of it
    /// failed, which is a diagnosis of its own.
    pub ran: bool,
    /// The last line of output when the body never ran — usually the
    /// prelude's own error.
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// What resolved satisfies what the project asked for.
    Satisfied,
    /// It does not, and both sides are known. The only verdict that stops
    /// a start.
    Mismatch,
    /// The probe could not run, the spec is one this build cannot
    /// evaluate, or the output was not a version. Never blocks anything.
    Unknown,
}

/// One requirement, what the machine answered, and the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub requirement: Requirement,
    pub resolved: Resolved,
    pub verdict: Verdict,
}

/// The table entry for a language, by name.
pub fn language(name: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.name == name)
}

/// Asks the shell what it resolves, and compares it to what the project
/// asked for.
///
/// The composition mirrors a real spawn — `<prelude> && <command>`, as
/// `actions::with_prelude` writes it — because a prelude that fails in
/// front of a dev server fails in front of this too, and that is exactly
/// what has to be reported.
pub fn check(requirement: &Requirement, prelude: &str, shell: Shell<'_>) -> Check {
    let Some(language) = language(&requirement.language) else {
        // A requirement about something with no probe in the table —
        // `engines.pnpm` — is recorded and never judged.
        return Check {
            requirement: requirement.clone(),
            resolved: Resolved::default(),
            verdict: Verdict::Unknown,
        };
    };
    let Some(output) = shell(&probe_command(language, prelude)) else {
        return Check {
            requirement: requirement.clone(),
            resolved: Resolved::default(),
            verdict: Verdict::Unknown,
        };
    };
    let resolved = parse_probe(&output);
    let verdict = if !resolved.ran {
        // The prelude never got as far as the probe. With no prelude that
        // means the shell itself is broken, which is not this check's
        // business; with one, it is precisely the failure to report.
        if prelude.trim().is_empty() {
            Verdict::Unknown
        } else {
            Verdict::Mismatch
        }
    } else {
        match (&resolved.path, &resolved.version) {
            // Nothing by that name on PATH: whatever the project asked
            // for, its own commands cannot run.
            (None, _) => Verdict::Mismatch,
            // There, but it answered with something that is not a version.
            (Some(_), None) => Verdict::Unknown,
            (Some(_), Some(version)) => satisfies(&requirement.spec, version),
        }
    };
    Check {
        requirement: requirement.clone(),
        resolved,
        verdict,
    }
}

/// The shell line the probe runs: the first of the language's binaries
/// that exists, its path, and its version, each behind a marker.
pub fn probe_command(language: &Language, prelude: &str) -> String {
    let body = format!(
        "for __pando_bin in {}; do if command -v \"$__pando_bin\" >/dev/null 2>&1; then \
         echo \"{PATH_MARK}$(command -v \"$__pando_bin\")\"; \
         echo \"{VERSION_MARK}$(\"$__pando_bin\" {} 2>&1 | head -n 1)\"; break; fi; done; \
         echo {DONE_MARK}",
        language.binaries.join(" "),
        language.version_flag,
    );
    match prelude.trim() {
        "" => body,
        prelude => format!("{prelude} && {{ {body}; }}"),
    }
}

fn parse_probe(output: &str) -> Resolved {
    let mut resolved = Resolved::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix(PATH_MARK) {
            resolved.path = Some(path.trim().to_string()).filter(|p| !p.is_empty());
        } else if let Some(version) = line.strip_prefix(VERSION_MARK) {
            resolved.version = first_version(version);
        } else if line == DONE_MARK {
            resolved.ran = true;
        }
    }
    if !resolved.ran {
        resolved.failure = output
            .lines()
            .map(str::trim)
            .rev()
            .find(|line| !line.is_empty())
            .map(str::to_string);
    }
    resolved
}

/// The first `1.2.3`-shaped run in a line of version output.
///
/// One parser for every language in the table: `v22.14.0`, `Python
/// 3.11.5`, `go version go1.22.0 darwin/arm64`, `ruby 3.2.2p53 (…)` and
/// `openjdk version "21.0.1"` all answer the same way.
pub fn first_version(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|c| c.is_ascii_digit())?;
    let end = chars[start..]
        .iter()
        .position(|c| !c.is_ascii_digit() && *c != '.')
        .map(|offset| start + offset)
        .unwrap_or(chars.len());
    let version: String = chars[start..end].iter().collect();
    let version = version.trim_end_matches('.').to_string();
    (!version.is_empty()).then_some(version)
}

/// Whether a resolved version satisfies a spec.
///
/// Deliberately small: the comparators npm, pyenv and friends actually
/// write, and `Unknown` for everything else. Blocking a start on a range
/// this build only half understands would be worse than not checking.
pub fn satisfies(spec: &str, version: &str) -> Verdict {
    let Some(resolved) = parse_version(normalize_spec(version)) else {
        return Verdict::Unknown;
    };
    let mut unknown = false;
    let mut any = false;
    for alternative in spec.split("||") {
        any = true;
        match satisfies_all(alternative, &resolved) {
            Verdict::Satisfied => return Verdict::Satisfied,
            Verdict::Unknown => unknown = true,
            Verdict::Mismatch => {}
        }
    }
    match (any, unknown) {
        (false, _) | (_, true) => Verdict::Unknown,
        _ => Verdict::Mismatch,
    }
}

/// One `||` alternative: every comparator in it has to hold.
fn satisfies_all(clause: &str, resolved: &[u64]) -> Verdict {
    let comparators: Vec<&str> = clause
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .collect();
    if comparators.is_empty() {
        return Verdict::Unknown;
    }
    let mut verdict = Verdict::Satisfied;
    for comparator in comparators {
        match compare(comparator, resolved) {
            // A comparator that definitely fails fails the clause, however
            // little is understood about the others.
            Verdict::Mismatch => return Verdict::Mismatch,
            Verdict::Unknown => verdict = Verdict::Unknown,
            Verdict::Satisfied => {}
        }
    }
    verdict
}

fn compare(comparator: &str, resolved: &[u64]) -> Verdict {
    let comparator = comparator.trim();
    let (op, rest) = split_op(comparator);
    let rest = normalize_spec(rest);
    if rest.is_empty() || rest == "*" {
        // `*`, `latest`, an empty comparator: anything goes, and only when
        // nothing was asked of it.
        return match op {
            Op::Exact => Verdict::Satisfied,
            _ => Verdict::Unknown,
        };
    }
    let Some(pattern) = parse_pattern(rest) else {
        return Verdict::Unknown;
    };
    match op {
        // Every component the pattern names has to match; the ones it does
        // not name are free, which is what makes `.nvmrc` 22 satisfied by
        // 22.14.0 and not by 24.
        Op::Exact => {
            for (index, part) in pattern.iter().enumerate() {
                let Some(want) = part else { continue };
                if component(resolved, index) != *want {
                    return Verdict::Mismatch;
                }
            }
            Verdict::Satisfied
        }
        Op::Caret | Op::Tilde => {
            let Some(floor) = concrete(&pattern) else {
                return Verdict::Unknown;
            };
            let ceiling = match op {
                Op::Caret => caret_ceiling(&floor),
                _ => tilde_ceiling(&floor, pattern.len()),
            };
            if cmp_versions(resolved, &floor).is_lt() || cmp_versions(resolved, &ceiling).is_ge() {
                Verdict::Mismatch
            } else {
                Verdict::Satisfied
            }
        }
        _ => {
            let Some(against) = concrete(&pattern) else {
                return Verdict::Unknown;
            };
            let ordering = cmp_versions(resolved, &against);
            let held = match op {
                Op::Ge => ordering.is_ge(),
                Op::Gt => ordering.is_gt(),
                Op::Le => ordering.is_le(),
                Op::Lt => ordering.is_lt(),
                _ => unreachable!("every other operator is handled above"),
            };
            if held {
                Verdict::Satisfied
            } else {
                Verdict::Mismatch
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Exact,
    Ge,
    Gt,
    Le,
    Lt,
    Caret,
    Tilde,
}

fn split_op(text: &str) -> (Op, &str) {
    for (prefix, op) in [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("^", Op::Caret),
        ("~", Op::Tilde),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return (op, rest.trim());
        }
    }
    (Op::Exact, text)
}

/// Components, where `x`, `X` and `*` stand for "any".
fn parse_pattern(text: &str) -> Option<Vec<Option<u64>>> {
    let mut out = Vec::new();
    for part in text.split('.') {
        if matches!(part, "x" | "X" | "*") {
            out.push(None);
            continue;
        }
        out.push(Some(part.parse::<u64>().ok()?));
    }
    (!out.is_empty()).then_some(out)
}

/// A pattern with no wildcards in it, for the comparisons that need one.
fn concrete(pattern: &[Option<u64>]) -> Option<Vec<u64>> {
    pattern.iter().copied().collect()
}

fn parse_version(text: &str) -> Option<Vec<u64>> {
    parse_pattern(text).and_then(|pattern| concrete(&pattern))
}

fn component(version: &[u64], index: usize) -> u64 {
    version.get(index).copied().unwrap_or(0)
}

fn cmp_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    for index in 0..a.len().max(b.len()) {
        let ordering = component(a, index).cmp(&component(b, index));
        if ordering.is_ne() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// `^1.2.3` is `< 2.0.0`; `^0.2.3` is `< 0.3.0`; `^0.0.3` is `< 0.0.4`.
/// The leading zero rule is semver's, and npm's `engines` uses it.
fn caret_ceiling(floor: &[u64]) -> Vec<u64> {
    let first_nonzero = floor.iter().position(|part| *part > 0).unwrap_or(0);
    let mut ceiling: Vec<u64> = floor.iter().take(first_nonzero + 1).copied().collect();
    let last = ceiling.len() - 1;
    ceiling[last] += 1;
    ceiling
}

/// `~1.2.3` and `~1.2` are `< 1.3.0`; `~1` is `< 2.0.0`.
fn tilde_ceiling(floor: &[u64], stated: usize) -> Vec<u64> {
    let bump = if stated >= 2 { 1 } else { 0 };
    let mut ceiling: Vec<u64> = floor.iter().take(bump + 1).copied().collect();
    let last = ceiling.len() - 1;
    ceiling[last] += 1;
    ceiling
}

// ---- version managers -----------------------------------------------------

/// How a manager is made to work in a non-interactive login shell, which
/// is the only shell pando has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// volta, asdf, mise, pyenv, rbenv, rustup, jenv. They resolve per
    /// directory by themselves, so when they fail it is a PATH problem and
    /// the fix is a PATH line. Proposing a `use` line for one is wrong.
    Shim,
    /// nvm, fnm, rvm, sdkman. They are shell functions, so they exist only
    /// after an init line has been sourced — and a developer whose shell
    /// sources it in `.zshrc` has no manager at all in pando's bash.
    SourceEval,
}

/// One version manager, and what pando would have to put in front of a
/// command for it to work.
#[derive(Debug, Clone, Copy)]
pub struct Manager {
    pub name: &'static str,
    pub family: Family,
    /// Paths that prove it is installed: absolute, or relative to the home
    /// directory. Any one of them is enough, because Homebrew puts nvm
    /// somewhere else entirely.
    pub markers: &'static [&'static str],
    /// The line that makes it work, with `{path}` standing for whichever
    /// marker was found.
    pub init: &'static str,
    /// The line that switches to the version the repository's own file
    /// names. Only the source-and-eval family has one.
    pub use_line: Option<&'static str>,
    /// How it installs a version, with `{spec}` and `{language}`. Printed,
    /// never run: mutating the developer's machine is not pando's job.
    pub install: Option<&'static str>,
    /// Whether it names a language the way `.tool-versions` does —
    /// `nodejs`, `golang` — rather than the way pando does.
    pub uses_alias: bool,
}

impl Manager {
    /// Where it is installed, if it is.
    pub fn installed_at(&self, home: &Path) -> Option<std::path::PathBuf> {
        self.markers
            .iter()
            .map(|marker| {
                if marker.starts_with('/') {
                    std::path::PathBuf::from(marker)
                } else {
                    home.join(marker)
                }
            })
            .find(|path| path.exists())
    }

    /// The command that installs a version under it. Printed as a hint.
    pub fn install_command(&self, language: &Language, spec: &str) -> Option<String> {
        let name = if self.uses_alias {
            language.aliases.first().copied().unwrap_or(language.name)
        } else {
            language.name
        };
        Some(
            self.install?
                .replace("{spec}", normalize_spec(spec))
                .replace("{language}", name),
        )
    }
}

const VOLTA: Manager = Manager {
    name: "volta",
    family: Family::Shim,
    markers: &[".volta/bin"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("volta install {language}@{spec}"),
    uses_alias: false,
};

const MISE: Manager = Manager {
    name: "mise",
    family: Family::Shim,
    markers: &[".local/share/mise/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("mise install {language}@{spec}"),
    uses_alias: false,
};

const ASDF: Manager = Manager {
    name: "asdf",
    family: Family::Shim,
    markers: &[".asdf/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("asdf install {language} {spec}"),
    uses_alias: true,
};

const NVM: Manager = Manager {
    name: "nvm",
    family: Family::SourceEval,
    markers: &[
        ".nvm/nvm.sh",
        "/opt/homebrew/opt/nvm/nvm.sh",
        "/usr/local/opt/nvm/nvm.sh",
    ],
    init: "export NVM_DIR=\"$HOME/.nvm\" && . \"{path}\" --no-use",
    // No version in it: this line goes in a file every project on the
    // machine shares, and `nvm use` with no argument takes the version
    // from the repository's own `.nvmrc`.
    use_line: Some("nvm use >/dev/null"),
    install: Some("nvm install {spec}"),
    uses_alias: false,
};

const FNM: Manager = Manager {
    name: "fnm",
    family: Family::SourceEval,
    markers: &[
        ".local/share/fnm/fnm",
        ".fnm/fnm",
        "/opt/homebrew/bin/fnm",
        "/usr/local/bin/fnm",
    ],
    init: "eval \"$({path} env)\"",
    use_line: Some("fnm use >/dev/null"),
    install: Some("fnm install {spec}"),
    uses_alias: false,
};

const PYENV: Manager = Manager {
    name: "pyenv",
    family: Family::Shim,
    markers: &[".pyenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("pyenv install {spec}"),
    uses_alias: false,
};

const RBENV: Manager = Manager {
    name: "rbenv",
    family: Family::Shim,
    markers: &[".rbenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("rbenv install {spec}"),
    uses_alias: false,
};

const RVM: Manager = Manager {
    name: "rvm",
    family: Family::SourceEval,
    markers: &[".rvm/scripts/rvm"],
    init: ". \"{path}\"",
    use_line: Some("rvm use . >/dev/null"),
    install: Some("rvm install {spec}"),
    uses_alias: false,
};

const RUSTUP: Manager = Manager {
    name: "rustup",
    family: Family::Shim,
    markers: &[".cargo/bin"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("rustup toolchain install {spec}"),
    uses_alias: false,
};

const JENV: Manager = Manager {
    name: "jenv",
    family: Family::Shim,
    markers: &[".jenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    // jenv manages JDKs that are already on the machine; it installs none.
    install: None,
    uses_alias: false,
};

const SDKMAN: Manager = Manager {
    name: "sdkman",
    family: Family::SourceEval,
    markers: &[".sdkman/bin/sdkman-init.sh"],
    init: ". \"{path}\"",
    use_line: None,
    install: Some("sdk install java {spec}"),
    uses_alias: false,
};

/// A prelude line that would make this machine resolve what the project
/// asks for, and why it is on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub line: String,
    pub manager: &'static str,
    pub why: String,
}

/// The lines worth offering, for the managers that are actually installed.
///
/// Ordered as the table lists them: the language's own manager first, then
/// the general-purpose ones, then the families that need sourcing — a
/// PATH line is the sturdier fix in a non-interactive shell, so it is the
/// one a question preselects.
///
/// `reads_version_file` says whether the requirement came from a file the
/// manager itself knows how to read, which is the only case where a `use`
/// line belongs: it takes the version from the repository, so one line in
/// a machine-wide file is right for every project on the machine.
pub fn fixes(language: &Language, home: &Path, reads_version_file: bool) -> Vec<Fix> {
    language
        .managers
        .iter()
        .filter_map(|manager| {
            let path = manager.installed_at(home)?;
            let mut line = manager.init.replace("{path}", &path.display().to_string());
            if let Some(use_line) = manager.use_line
                && reads_version_file
            {
                line = format!("{line} && {use_line}");
            }
            let why = match manager.family {
                Family::Shim => format!(
                    "{} is installed here, and its shims resolve the version per directory",
                    manager.name
                ),
                Family::SourceEval => format!(
                    "{} is installed here, and a login shell has to source it",
                    manager.name
                ),
            };
            Some(Fix {
                line,
                manager: manager.name,
                why,
            })
        })
        .collect()
}

/// Every manager for this language that this machine has.
pub fn installed(language: &Language, home: &Path) -> Vec<&'static Manager> {
    language
        .managers
        .iter()
        .filter(|manager| manager.installed_at(home).is_some())
        .collect()
}

/// Whether the requirement was stated in a file this language's managers
/// read themselves.
pub fn from_version_file(language: &Language, requirement: &Requirement) -> bool {
    language
        .files
        .iter()
        .any(|source| source.file == requirement.source)
}

// ---- the probe cache ------------------------------------------------------

const CACHE_VERSION: u32 = 1;

/// Probes that came back satisfied, keyed by a fingerprint of what was
/// asked and how.
///
/// Only matches are kept, deliberately. A cached mismatch would keep
/// reporting a failure the developer has just fixed, and a mismatch stops
/// the start anyway, so there is no spawn to save by remembering it. What
/// this buys is the common case: a start costs one extra spawn when the
/// requirement or the prelude changes, and none when neither has.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProbeCache {
    pub version: u32,
    /// Fingerprint to the version that satisfied it, which is what makes
    /// the file readable when something has to be explained.
    pub satisfied: BTreeMap<String, String>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            satisfied: BTreeMap::new(),
        }
    }

    pub fn holds(&self, fingerprint: &str) -> bool {
        self.satisfied.contains_key(fingerprint)
    }

    pub fn remember(&mut self, fingerprint: String, version: String) {
        self.satisfied.insert(fingerprint, version);
    }
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// What a cached probe is keyed on: everything that could change its
/// answer except the machine itself — the requirement, where it came from,
/// the prelude in front of it, and the command that would be run.
pub fn fingerprint(requirement: &Requirement, prelude: &str) -> String {
    let probe = language(&requirement.language)
        .map(|language| probe_command(language, prelude))
        .unwrap_or_default();
    let mut context = md5::Context::new();
    for part in [
        requirement.language.as_str(),
        requirement.spec.as_str(),
        requirement.source.as_str(),
        prelude,
        &probe,
    ] {
        context.consume(part.as_bytes());
        context.consume([0]);
    }
    format!("md5:{:x}", context.finalize())
}

/// The output a probe would produce on a machine that resolves `version`
/// at `path`.
///
/// Test-only, and here rather than in the tests that use it: the markers
/// are this module's, and a fake that spelled them out itself would drift
/// from them.
#[cfg(test)]
pub(crate) fn probe_reply(path: &str, version: &str) -> String {
    format!("{PATH_MARK}{path}\n{VERSION_MARK}{version}\n{DONE_MARK}\n")
}

pub fn load_cache(path: &Path) -> ProbeCache {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProbeCache::new();
    };
    match serde_json::from_str::<ProbeCache>(&text) {
        Ok(cache) if cache.version == CACHE_VERSION => cache,
        _ => ProbeCache::new(),
    }
}

pub fn save_cache(path: &Path, cache: &ProbeCache) -> anyhow::Result<()> {
    use anyhow::Context as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(cache).context("serialize the runtime probe cache")?;
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename tmp → {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn repo(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        dir
    }

    fn found(files: &[(&str, &str)]) -> Vec<(String, String, String, bool)> {
        let dir = repo(files);
        requirements(dir.path())
            .into_iter()
            .map(|r| (r.language, r.spec, r.source, r.pinned))
            .collect()
    }

    #[test]
    fn a_repository_that_says_nothing_requires_nothing() {
        assert!(found(&[("README.md", "hi\n")]).is_empty());
    }

    #[test]
    fn the_node_version_files_pin_node() {
        assert_eq!(
            found(&[(".nvmrc", "22\n")]),
            vec![("node".into(), "22".into(), ".nvmrc".into(), true)]
        );
        // A `v` prefix is decoration, and the spec is recorded as written.
        assert_eq!(
            found(&[(".node-version", "v20.11.0\n")]),
            vec![(
                "node".into(),
                "v20.11.0".into(),
                ".node-version".into(),
                true
            )]
        );
    }

    #[test]
    fn an_alias_that_names_no_version_is_recorded_and_is_not_a_pin() {
        let out = found(&[(".nvmrc", "lts/hydrogen\n")]);
        assert_eq!(out[0].1, "lts/hydrogen");
        assert!(!out[0].3, "an alias is not a pin: {out:?}");
    }

    #[test]
    fn engines_is_read_and_is_a_range() {
        let out = found(&[(
            "package.json",
            r#"{"name":"x","engines":{"node":">=18 <21","pnpm":"9"}}"#,
        )]);
        assert_eq!(
            out,
            vec![
                (
                    "node".into(),
                    ">=18 <21".into(),
                    "package.json engines.node".into(),
                    false
                ),
                // Not a language pando probes, and still a fact about the
                // project that `signals --json` publishes.
                (
                    "pnpm".into(),
                    "9".into(),
                    "package.json engines.pnpm".into(),
                    true
                ),
            ]
        );
    }

    // Both are recorded — a report that names only one of them cannot
    // explain the mismatch — and the pin is the one to compare against.
    #[test]
    fn a_pin_and_a_range_are_both_recorded_with_the_pin_first() {
        let out = found(&[
            (".nvmrc", "22\n"),
            ("package.json", r#"{"engines":{"node":">=18"}}"#),
        ]);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].2, ".nvmrc");
        assert!(out[0].3, "the pin sorts first");
        assert_eq!(out[1].2, "package.json engines.node");

        let dir = repo(&[
            (".nvmrc", "22\n"),
            ("package.json", r#"{"engines":{"node":">=18"}}"#),
        ]);
        let all = requirements(dir.path());
        assert_eq!(
            for_language(&all, "node").map(|r| r.spec.as_str()),
            Some("22"),
            "the pin is what a comparison uses"
        );
        assert!(for_language(&all, "elixir").is_none());
    }

    // Even when the range is written first, because the file order is not
    // the project's opinion about which one is more specific.
    #[test]
    fn a_range_stated_before_a_pin_still_loses_to_it() {
        let out = found(&[
            ("mise.toml", "[tools]\nnode = \">=18\"\n"),
            (".tool-versions", "nodejs 22.11.0\n"),
        ]);
        assert_eq!(out[0].1, "22.11.0", "{out:?}");
        assert_eq!(out[1].1, ">=18", "{out:?}");
    }

    #[test]
    fn tool_versions_names_several_languages_and_normalises_their_names() {
        let out = found(&[(
            ".tool-versions",
            "# a comment\nnodejs 22.11.0\npython 3.12.1 3.11.6\ngolang 1.22.0\n\njava temurin-21.0.1\n",
        )]);
        assert_eq!(
            out,
            vec![
                (
                    "node".into(),
                    "22.11.0".into(),
                    ".tool-versions".into(),
                    true
                ),
                // The first version only: the rest are fallbacks.
                (
                    "python".into(),
                    "3.12.1".into(),
                    ".tool-versions".into(),
                    true
                ),
                ("go".into(), "1.22.0".into(), ".tool-versions".into(), true),
                // A vendor prefix is decoration too, so this is still a pin.
                (
                    "java".into(),
                    "temurin-21.0.1".into(),
                    ".tool-versions".into(),
                    true
                ),
            ]
        );
    }

    #[test]
    fn mise_tools_are_read_in_every_shape_they_are_written_in() {
        let out = found(&[(
            "mise.toml",
            "[tools]\nnode = \"22\"\npython = [\"3.12\", \"3.11\"]\nruby = { version = \"3.2\" }\n",
        )]);
        assert_eq!(
            out,
            vec![
                ("node".into(), "22".into(), "mise.toml".into(), true),
                ("python".into(), "3.12".into(), "mise.toml".into(), true),
                ("ruby".into(), "3.2".into(), "mise.toml".into(), true),
            ]
        );
    }

    #[test]
    fn a_rust_toolchain_file_is_read_through_its_channel_key() {
        assert_eq!(
            found(&[("rust-toolchain.toml", "[toolchain]\nchannel = \"1.75.0\"\n")]),
            vec![(
                "rust".into(),
                "1.75.0".into(),
                "rust-toolchain.toml".into(),
                true
            )]
        );
        let out = found(&[("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n")]);
        assert!(!out[0].3, "a channel name is not a pin: {out:?}");
    }

    #[test]
    fn a_ruby_version_file_carries_its_vendor_prefix_and_is_still_a_pin() {
        let out = found(&[(".ruby-version", "ruby-3.2.2\n")]);
        assert_eq!(out[0].1, "ruby-3.2.2");
        assert!(out[0].3);
        assert_eq!(normalize_spec("ruby-3.2.2"), "3.2.2");
    }

    #[test]
    fn comments_and_blank_lines_are_not_versions() {
        let out = found(&[(
            ".python-version",
            "\n# pyenv writes these\n3.12.1\n3.11.6\n",
        )]);
        assert_eq!(out[0].1, "3.12.1", "{out:?}");
        assert_eq!(out.len(), 1, "only the first line is the version: {out:?}");
    }

    #[test]
    fn a_file_pando_cannot_parse_is_not_a_requirement() {
        assert!(found(&[("package.json", "{ not json")]).is_empty());
        assert!(found(&[("mise.toml", "[tools\nnode =")]).is_empty());
        assert!(found(&[("rust-toolchain.toml", "channel = 3")]).is_empty());
        assert!(found(&[(".nvmrc", "\n\n")]).is_empty());
    }

    // ---- what the machine resolves ---------------------------------------

    fn node() -> &'static Language {
        language("node").expect("node is in the table")
    }

    /// A shell that answers with whatever a test decided this machine is.
    fn machine(path: &str, version: &str) -> impl Fn(&str) -> Option<String> {
        let reply = format!("{PATH_MARK}{path}\n{VERSION_MARK}{version}\n{DONE_MARK}\n");
        move |_cmd: &str| Some(reply.clone())
    }

    fn check_node(spec: &str, shell: Shell<'_>) -> Check {
        check(&Requirement::new("node", spec, ".nvmrc".into()), "", shell)
    }

    #[test]
    fn the_probe_asks_for_the_path_as_well_as_the_version() {
        let command = probe_command(node(), "");
        assert!(command.contains("command -v"), "{command}");
        assert!(command.contains("for __pando_bin in node;"), "{command}");
        assert!(command.contains("-v 2>&1"), "{command}");
        // Both markers, because a prelude that prints something of its own
        // must not be mistaken for the answer.
        assert!(command.contains(PATH_MARK) && command.contains(VERSION_MARK));
        // Every binary the language may go by, in order.
        let python = probe_command(language("python").unwrap(), "");
        assert!(python.contains("python3 python"), "{python}");
    }

    #[test]
    fn a_prelude_is_composed_the_way_a_real_spawn_composes_it() {
        let command = probe_command(node(), "nvm use");
        assert!(
            command.starts_with("nvm use && {"),
            "the probe measures what a spawn would do: {command}"
        );
    }

    #[test]
    fn chatter_from_the_prelude_is_not_mistaken_for_the_answer() {
        let shell = |_: &str| {
            Some(format!(
                "Now using node v22.11.0 (npm v10.9.0)\n\
                 {PATH_MARK}/home/dev/.nvm/versions/node/v22.11.0/bin/node\n\
                 {VERSION_MARK}v22.11.0\n{DONE_MARK}\n"
            ))
        };
        let check = check_node("22", &shell);
        assert_eq!(check.verdict, Verdict::Satisfied);
        assert_eq!(check.resolved.version.as_deref(), Some("22.11.0"));
        assert_eq!(
            check.resolved.path.as_deref(),
            Some("/home/dev/.nvm/versions/node/v22.11.0/bin/node")
        );
    }

    #[test]
    fn a_pin_the_machine_does_not_meet_is_a_mismatch_with_the_path_it_resolved() {
        let shell = machine("/opt/homebrew/bin/node", "v24.21.0");
        let check = check_node("22", &shell);
        assert_eq!(check.verdict, Verdict::Mismatch);
        // The path, not only the version: pando's shell is not the
        // developer's shell, and this is the line that says so.
        assert_eq!(
            check.resolved.path.as_deref(),
            Some("/opt/homebrew/bin/node")
        );
    }

    #[test]
    fn a_binary_that_is_not_there_at_all_is_a_mismatch() {
        let shell = |_: &str| Some(format!("{DONE_MARK}\n"));
        let check = check_node("22", &shell);
        assert_eq!(check.verdict, Verdict::Mismatch);
        assert_eq!(check.resolved.path, None);
        assert!(
            check.resolved.ran,
            "the probe ran; there was nothing to find"
        );
    }

    #[test]
    fn a_prelude_that_fails_never_reaches_the_probe_and_says_so() {
        let shell = |_: &str| Some("bash: nvm: command not found\n".to_string());
        let check = check(
            &Requirement::new("node", "22", ".nvmrc".into()),
            "nvm use 22",
            &shell,
        );
        assert_eq!(check.verdict, Verdict::Mismatch);
        assert!(!check.resolved.ran);
        assert_eq!(
            check.resolved.failure.as_deref(),
            Some("bash: nvm: command not found")
        );
    }

    // Everything pando cannot judge proceeds: a check that guesses is
    // worse than no check.
    #[test]
    fn a_shell_that_cannot_run_or_a_spec_that_cannot_be_read_blocks_nothing() {
        let dead = |_: &str| None;
        assert_eq!(check_node("22", &dead).verdict, Verdict::Unknown);

        let shell = machine("/usr/bin/node", "v24.21.0");
        assert_eq!(check_node("lts/hydrogen", &shell).verdict, Verdict::Unknown);

        // There, but it answered with something that is not a version.
        let odd = |_: &str| {
            Some(format!(
                "{PATH_MARK}/usr/bin/node\n{VERSION_MARK}\n{DONE_MARK}\n"
            ))
        };
        assert_eq!(check_node("22", &odd).verdict, Verdict::Unknown);

        // And a language with no probe in the table is recorded, never
        // judged.
        let pnpm = Requirement::new("pnpm", "9", "package.json engines.pnpm".into());
        assert_eq!(check(&pnpm, "", &shell).verdict, Verdict::Unknown);
    }

    #[test]
    fn every_languages_version_output_reads_the_same_way() {
        for (output, want) in [
            ("v22.14.0", "22.14.0"),
            ("Python 3.11.5", "3.11.5"),
            ("ruby 3.2.2p53 (2023-03-30 revision e51014f9c0)", "3.2.2"),
            ("rustc 1.75.0 (82e1608df 2023-12-21)", "1.75.0"),
            ("go version go1.22.0 darwin/arm64", "1.22.0"),
            ("openjdk version \"21.0.1\" 2023-10-17", "21.0.1"),
        ] {
            assert_eq!(first_version(output).as_deref(), Some(want), "{output}");
        }
        assert_eq!(first_version("command not found"), None);
    }

    #[test]
    fn the_comparisons_a_version_file_and_an_engines_range_actually_use() {
        for (spec, version, want) in [
            // A pin matches on the components it names, and no others.
            ("22", "22.14.0", Verdict::Satisfied),
            ("22", "24.21.0", Verdict::Mismatch),
            ("22.14.0", "22.14.1", Verdict::Mismatch),
            ("v20.11.0", "20.11.0", Verdict::Satisfied),
            ("3.12", "3.12.1", Verdict::Satisfied),
            ("18.x", "18.2.0", Verdict::Satisfied),
            ("18.x", "20.2.0", Verdict::Mismatch),
            // Ranges, as `engines` writes them.
            (">=18", "24.21.0", Verdict::Satisfied),
            (">=18", "16.20.0", Verdict::Mismatch),
            (">=18 <21", "24.21.0", Verdict::Mismatch),
            (">=18 <21", "20.11.0", Verdict::Satisfied),
            (">=18 || >=20", "24.0.0", Verdict::Satisfied),
            ("^18.0.0", "18.20.1", Verdict::Satisfied),
            ("^18.0.0", "19.0.0", Verdict::Mismatch),
            ("^0.2.3", "0.2.9", Verdict::Satisfied),
            ("^0.2.3", "0.3.0", Verdict::Mismatch),
            ("~3.2", "3.2.9", Verdict::Satisfied),
            ("~3.2", "3.3.0", Verdict::Mismatch),
            ("*", "24.0.0", Verdict::Satisfied),
            // And everything this build does not model.
            ("lts/*", "22.0.0", Verdict::Unknown),
            ("stable", "1.75.0", Verdict::Unknown),
            ("", "22.0.0", Verdict::Unknown),
            (">=18 lts/*", "24.0.0", Verdict::Unknown),
            // A definite failure is still a failure, whatever sits beside it.
            ("<18 lts/*", "24.0.0", Verdict::Mismatch),
        ] {
            assert_eq!(
                satisfies(spec, version),
                want,
                "{spec:?} against {version:?}"
            );
        }
    }

    // ---- version managers -------------------------------------------------

    fn home_with(paths: &[&str]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for path in paths {
            let full = dir.path().join(path);
            if path.ends_with(".sh") || path.ends_with("fnm") || path.ends_with("rvm") {
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(full, "#!/bin/sh\n").unwrap();
            } else {
                std::fs::create_dir_all(full).unwrap();
            }
        }
        dir
    }

    #[test]
    fn a_source_and_eval_manager_gets_an_init_line_and_a_use_line() {
        let home = home_with(&[".nvm/nvm.sh"]);
        let requirement = Requirement::new("node", "22", ".nvmrc".into());
        let fixes = fixes(node(), home.path(), from_version_file(node(), &requirement));
        assert_eq!(fixes.len(), 1, "{fixes:?}");
        let line = &fixes[0].line;
        assert!(line.contains("nvm.sh"), "{line}");
        // No version in it: the line lands in a file every project on the
        // machine shares, and `nvm use` reads the repository's own file.
        assert!(line.ends_with("nvm use >/dev/null"), "{line}");
        assert!(!line.contains("nvm use 22"), "{line}");
    }

    // A requirement that came from `engines` is not in a file nvm can
    // read, so there is nothing for a bare `nvm use` to find.
    #[test]
    fn a_requirement_no_manager_can_read_gets_the_init_line_alone() {
        let home = home_with(&[".nvm/nvm.sh"]);
        let requirement = Requirement::new("node", ">=18", "package.json engines.node".into());
        let fixes = fixes(node(), home.path(), from_version_file(node(), &requirement));
        assert!(!fixes[0].line.contains("nvm use"), "{:?}", fixes[0]);
    }

    // A shim manager resolves per directory by itself. When it fails it is
    // a PATH problem in a login bash shell, and a `use` line would be the
    // wrong fix.
    /// Only the fixes that came from the injected home. A manager can also
    /// be installed system-wide — Homebrew puts nvm in its own prefix —
    /// and whether this machine has one is not something a test decides.
    fn from_home(language: &Language, home: &TempDir) -> Vec<Fix> {
        let home_path = home.path().display().to_string();
        fixes(language, home.path(), true)
            .into_iter()
            .filter(|fix| fix.line.contains(&home_path))
            .collect()
    }

    #[test]
    fn a_shim_manager_gets_a_path_line_and_never_a_use_line() {
        let home = home_with(&[".volta/bin"]);
        let fixes = from_home(node(), &home);
        assert_eq!(fixes.len(), 1, "{fixes:?}");
        assert!(fixes[0].line.starts_with("export PATH="), "{:?}", fixes[0]);
        assert!(!fixes[0].line.contains("use"), "{:?}", fixes[0]);
        assert!(fixes[0].line.contains(".volta/bin"), "{:?}", fixes[0]);
    }

    #[test]
    fn only_the_managers_this_machine_has_are_offered() {
        let empty = TempDir::new().unwrap();
        assert!(
            from_home(node(), &empty).is_empty(),
            "a home with no manager in it offers no line"
        );

        // And the order is the table's: a PATH line is the sturdier fix in
        // a non-interactive shell, so it comes first.
        let home = home_with(&[".nvm/nvm.sh", ".volta/bin"]);
        let offered: Vec<&str> = from_home(node(), &home)
            .iter()
            .map(|fix| fix.manager)
            .collect();
        assert_eq!(offered, vec!["volta", "nvm"]);
    }

    // Printed, never run: installing a toolchain is the developer's call.
    #[test]
    fn an_install_command_uses_the_name_the_manager_itself_uses() {
        let home = home_with(&[".asdf/shims"]);
        let asdf = installed(node(), home.path())[0];
        assert_eq!(
            asdf.install_command(node(), "22").as_deref(),
            Some("asdf install nodejs 22"),
            "asdf's plugin is nodejs, not node"
        );
        let volta_home = home_with(&[".volta/bin"]);
        let volta = installed(node(), volta_home.path())[0];
        assert_eq!(
            volta.install_command(node(), "v22").as_deref(),
            Some("volta install node@22")
        );
    }

    // ---- the probe cache --------------------------------------------------

    #[test]
    fn the_cache_keys_on_the_requirement_and_the_prelude() {
        let a = Requirement::new("node", "22", ".nvmrc".into());
        let b = Requirement::new("node", "24", ".nvmrc".into());
        assert_eq!(fingerprint(&a, ""), fingerprint(&a, ""));
        assert_ne!(fingerprint(&a, ""), fingerprint(&b, ""));
        assert_ne!(
            fingerprint(&a, ""),
            fingerprint(&a, "nvm use"),
            "a new prelude is a new question"
        );
    }

    #[test]
    fn the_cache_round_trips_and_a_future_version_is_ignored() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache").join("runtime.json");
        let mut cache = ProbeCache::new();
        cache.remember("md5:abc".into(), "22.11.0".into());
        save_cache(&path, &cache).unwrap();
        assert_eq!(load_cache(&path), cache);
        assert!(load_cache(&path).holds("md5:abc"));

        std::fs::write(&path, r#"{"version":99,"satisfied":{}}"#).unwrap();
        assert!(!load_cache(&path).holds("md5:abc"));
        assert!(!load_cache(&dir.path().join("nothing.json")).holds("md5:abc"));
    }
}
