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
    },
    Language {
        name: "python",
        files: &[Source {
            file: ".python-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
    },
    Language {
        name: "ruby",
        files: &[Source {
            file: ".ruby-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
    },
    Language {
        name: "rust",
        files: &[Source {
            file: "rust-toolchain.toml",
            kind: SourceKind::TomlKey("toolchain", "channel"),
        }],
        aliases: &[],
        engines_key: None,
    },
    Language {
        name: "go",
        files: &[],
        aliases: &["golang"],
        engines_key: None,
    },
    Language {
        name: "java",
        files: &[],
        aliases: &[],
        engines_key: None,
    },
];

/// Files that state several languages at once, read once each.
const TOOL_VERSIONS: &str = ".tool-versions";
const MISE: &str = "mise.toml";
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
        for (file, entries) in [(TOOL_VERSIONS, &tool_versions), (MISE, &mise)] {
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
    let Ok(text) = std::fs::read_to_string(root.join(MISE)) else {
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
}
