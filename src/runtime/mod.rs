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

mod fixes;
mod languages;
mod probe;
mod probe_cache;
mod version;

pub use fixes::{Fix, binary_dirs, fixes, installed, path_line};
pub use languages::{Family, LANGUAGES, Language, Manager, Source, SourceKind, language};
pub use probe::{Check, Resolved, Shell, Verdict, check, probe_command};
pub use probe_cache::{ProbeCache, fingerprint, load_cache, save_cache};
pub use version::{first_version, satisfies};

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
    /// The app directory that states it, relative to the root, for a
    /// requirement below it: `backend`. It is where the probe asks, since
    /// that is where the processes that need it run. Absent for the root's
    /// own, which is every requirement a root that is an app states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

impl Requirement {
    fn new(language: &str, spec: &str, source: String) -> Requirement {
        Requirement {
            language: language.to_string(),
            spec: spec.to_string(),
            source,
            pinned: is_pin(spec),
            dir: None,
        }
    }
}

/// Files that state several languages at once, read once each.
const TOOL_VERSIONS: &str = ".tool-versions";
/// mise reads both spellings, and the dot-prefixed one is the common one in
/// the wild. Where both exist, mise itself lets `mise.toml` win, so it is
/// read first and its entries sort first.
const MISE_FILES: [&str; 2] = ["mise.toml", ".mise.toml"];
pub const SHARED_VERSION_FILES: [&str; 3] = [TOOL_VERSIONS, MISE_FILES[0], MISE_FILES[1]];
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
    let mise: Vec<(&str, Vec<(String, String)>)> = MISE_FILES
        .iter()
        .map(|file| (*file, read_mise(root, file)))
        .collect();
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
        let shared = std::iter::once((TOOL_VERSIONS, &tool_versions))
            .chain(mise.iter().map(|(file, entries)| (*file, entries)));
        for (file, entries) in shared {
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

/// What an app directory below the root says about the runtimes it needs:
/// [`requirements`] read in `root/dir`, each source under its path
/// (`backend/.nvmrc`) and each requirement carrying its directory.
pub fn requirements_in(root: &Path, dir: &str) -> Vec<Requirement> {
    requirements(&root.join(dir))
        .into_iter()
        .map(|requirement| Requirement {
            source: format!("{dir}/{}", requirement.source),
            dir: Some(dir.to_string()),
            ..requirement
        })
        .collect()
}

/// What the project needs, as config names it: the root's own
/// requirements, and those of each version file `runtime.version_files`
/// names in an app directory.
///
/// The root is read whatever config says, as it always was. Below it only
/// the files config names are read, because config is what the runtime
/// reads: detection proposes `backend/.nvmrc`, and once it is written the
/// start checks it, in `backend`, where the processes that need it run.
pub fn requirements_for(root: &Path, version_files: &[String]) -> Vec<Requirement> {
    let mut out = requirements(root);
    let mut dirs: Vec<&str> = version_files
        .iter()
        .filter_map(|file| file.rsplit_once('/').map(|(dir, _)| dir))
        // Inside the repository only: config is a file a human edits, and
        // `../` or an absolute path would have a start read a file of
        // somebody else's.
        .filter(|dir| {
            !dir.is_empty()
                && Path::new(dir)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
        })
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    for dir in dirs {
        out.extend(
            requirements_in(root, dir)
                .into_iter()
                .filter(|requirement| version_files.contains(&requirement.source)),
        );
    }
    out
}

/// The requirements to compare this machine against: for each language in
/// the table, the one [`for_language`] would take, at the root and in
/// each app directory that states its own. Two directories may pin two
/// versions, and a report that checked one of them would miss the other.
pub fn to_compare(requirements: &[Requirement]) -> Vec<&Requirement> {
    let mut out: Vec<&Requirement> = Vec::new();
    for language in &LANGUAGES {
        for requirement in requirements.iter().filter(|r| r.language == language.name) {
            if !out
                .iter()
                .any(|held| held.language == requirement.language && held.dir == requirement.dir)
            {
                out.push(requirement);
            }
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

/// `[tools]` in `mise.toml` or `.mise.toml`, whose values are a string, a
/// list, or a table with a `version` key.
fn read_mise(root: &Path, file: &str) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(root.join(file)) else {
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

#[cfg(test)]
pub(crate) use probe::{probe_failure, probe_reply};

#[cfg(test)]
mod tests;
