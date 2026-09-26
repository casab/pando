//! Which framework rule a repository matches.

use std::path::Path;

use crate::catalog::frameworks::{FrameworkRule, Guard, RULES};

use super::signals::Signals;

/// The first rule whose marker file or script body is present.
pub fn framework(root: &Path, signals: &Signals) -> Option<&'static FrameworkRule> {
    RULES.iter().find(|rule| {
        let by_marker = rule
            .markers
            .iter()
            .any(|m| signals.markers.iter().any(|f| f == m));
        // A Cargo.toml with no binary is a library, and a mix.exs with no
        // Phoenix in it is some other Mix project: nothing this rule serves.
        if by_marker && !passes(root, rule.guard) {
            return false;
        }
        by_marker || names_a_script(rule, signals)
    })
}

/// The rule for what `dev`, a project's own `dev` script, runs. For a
/// framework whose scripts only build its assets, that is the rule the
/// script names when it names one: a Laravel app's `dev: vite` is Vite.
/// Only that script: a `css: node build-css.js` beside a Django app's
/// `dev: python manage.py runserver` says nothing about what `dev` runs.
/// None for a script that runs the rule's build and not its server: a
/// library's `vite build --watch` is a watcher, not a Vite server.
pub(super) fn script_framework(
    root: &Path,
    signals: &Signals,
    dev: &str,
) -> Option<&'static FrameworkRule> {
    let rule = framework(root, signals)?;
    let rule = match rule.scripts_build_assets {
        false => rule,
        true => RULES.iter().find(|other| runs(other, dev)).unwrap_or(rule),
    };
    (!only_builds(rule, dev)).then_some(rule)
}

/// Whether `body` runs one of the rule's build markers and, past them,
/// nothing else of the rule's: the framework's build, which serves
/// nothing. A build the server follows, `vite build && vite preview`, is
/// that server's script, and the flag it is handed reaches the server. A
/// file the build is handed, `--config vite.lib.config.ts`, or a path it
/// clears first, `node_modules/.vite`, names the framework and runs none
/// of it.
pub(super) fn only_builds(rule: &FrameworkRule, body: &str) -> bool {
    let builds = rule
        .build_markers
        .iter()
        .any(|needle| mentions(body, needle));
    let rest = rule
        .build_markers
        .iter()
        .fold(body.to_string(), |rest, needle| without(&rest, needle));
    builds
        && !rule
            .script_markers
            .iter()
            .any(|needle| invokes(&rest, needle))
}

/// Whether one of the project's script bodies runs one of the rule's
/// script markers.
fn names_a_script(rule: &FrameworkRule, signals: &Signals) -> bool {
    signals.scripts.values().any(|body| runs(rule, body))
}

/// Whether `body` runs one of the rule's script markers.
pub(super) fn runs(rule: &FrameworkRule, body: &str) -> bool {
    rule.script_markers
        .iter()
        .any(|needle| mentions(body, needle))
}

/// Whether the project at `root` is what a rule's guard asks for.
fn passes(root: &Path, guard: Guard) -> bool {
    match guard {
        Guard::Marker => true,
        Guard::BinaryCrate => binary_crate(root),
        Guard::GoMain => go_main(root) || !go_commands(root).is_empty(),
        Guard::Mentions(files, needle) => files.iter().any(|file| {
            std::fs::read_to_string(root.join(file)).is_ok_and(|text| text.contains(needle))
        }),
    }
}

/// Whether `body` runs `needle` as a word of its own. A plain substring
/// test reads `vitest` as `vite` and `vite-node` as `vite`, and a project
/// whose test runner happens to share a prefix with a dev server would be
/// started as that dev server.
fn mentions(body: &str, needle: &str) -> bool {
    mentioned_at(body, needle).next().is_some()
}

/// Where `body` runs `needle` as a word of its own, as [`mentions`] reads
/// it.
fn mentioned_at<'a>(body: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    body.match_indices(needle).filter_map(move |(at, _)| {
        let before = body[..at].chars().next_back().is_none_or(|c| !word(c));
        let after = body[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !word(c));
        // A needle that ends in a space ("node ") has already said where
        // the word ends.
        (before && (after || needle.ends_with(' '))).then_some(at)
    })
}

/// Whether `body` runs `needle` as [`mentions`] reads it, and not as part
/// of a file's name or path: `vite.lib.config.ts` and `node_modules/.vite`
/// name Vite and run none of it.
fn invokes(body: &str, needle: &str) -> bool {
    mentioned_at(body, needle).any(|at| {
        let before = body[..at].chars().next_back();
        let after = body[at + needle.len()..].chars().next();
        before != Some('.') && (needle.ends_with(' ') || !matches!(after, Some('.' | '/')))
    })
}

/// `body` with each place it runs `needle` cut out, a space left in its
/// stead so the words either side stay apart.
fn without(body: &str, needle: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut from = 0;
    for at in mentioned_at(body, needle) {
        out.push_str(&body[from..at]);
        out.push(' ');
        from = at + needle.len();
    }
    out.push_str(&body[from..]);
    out
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

/// The binaries of a crate that has more than one and names none to run
/// by default, the crate's own `src/main.rs` first. `cargo run` refuses
/// such a crate, "could not determine which binary to run", so each one
/// has to be run by name. Empty for a crate with one binary, or with a
/// `default-run`.
///
/// Cargo's own discovery: every `[[bin]]`, and unless `autobins = false`
/// `src/main.rs` under the package's name and each `src/bin/<name>.rs` or
/// `src/bin/<name>/main.rs` that no `[[bin]]` already names or points at.
pub(super) fn several_binaries(root: &Path) -> Vec<String> {
    let Some(manifest) = std::fs::read_to_string(root.join("Cargo.toml"))
        .ok()
        .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
    else {
        return Vec::new();
    };
    let package = manifest.get("package").and_then(toml::Value::as_table);
    let field = |key: &str| package.and_then(|package| package.get(key));
    if field("default-run").is_some() {
        return Vec::new();
    }
    let own = field("name").and_then(toml::Value::as_str);
    // Each binary's name, and its path where one is known.
    let mut binaries: Vec<(String, Option<String>)> = manifest
        .get("bin")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bin| {
            let name = bin.get("name")?.as_str()?.to_string();
            let path = bin.get("path").and_then(toml::Value::as_str);
            Some((name, path.map(str::to_string)))
        })
        .collect();
    if field("autobins").and_then(toml::Value::as_bool) != Some(false) {
        let mut found: Vec<(String, String)> = Vec::new();
        if let Some(own) = own
            && root.join("src/main.rs").is_file()
        {
            found.push((own.to_string(), "src/main.rs".to_string()));
        }
        let mut in_bin: Vec<(String, String)> = std::fs::read_dir(root.join("src/bin"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let file = entry.file_name().to_str()?.to_string();
                match entry.path().is_dir() {
                    true => entry
                        .path()
                        .join("main.rs")
                        .is_file()
                        .then(|| (file.clone(), format!("src/bin/{file}/main.rs"))),
                    false => {
                        let name = file.strip_suffix(".rs")?.to_string();
                        Some((name, format!("src/bin/{file}")))
                    }
                }
            })
            .collect();
        in_bin.sort();
        found.extend(in_bin);
        for (name, path) in found {
            let declared = binaries
                .iter()
                .any(|(other, at)| *other == name || at.as_deref() == Some(path.as_str()));
            if !declared {
                binaries.push((name, Some(path)));
            }
        }
    }
    if binaries.len() < 2 {
        return Vec::new();
    }
    let crates_own = |(name, path): &(String, Option<String>)| {
        path.as_deref() == Some("src/main.rs") || Some(name.as_str()) == own
    };
    binaries.sort_by_key(|binary| !crates_own(binary));
    binaries.into_iter().map(|(name, _)| name).collect()
}

/// The names a command under `cmd/` has when it is the service's server.
const GO_SERVER_COMMANDS: [&str; 7] = ["server", "api", "web", "app", "http", "serve", "www"];

/// The names a command under `cmd/` has when it is a tool beside the
/// server, which a start would run as the dev server and see exit.
const GO_TOOL_COMMANDS: [&str; 13] = [
    "migrate",
    "migration",
    "seed",
    "cli",
    "admin",
    "gen",
    "generate",
    "tool",
    "tools",
    "worker",
    "job",
    "cron",
    "script",
];

/// The commands of a Go module whose root is not a main package, which
/// `go run .` refuses: each directory under `cmd/` that is one. The first
/// is the one a start takes, so a server leads: a command with a server's
/// name, or with the module's own, `cmd/shop` in `example.com/shop`, then
/// the rest in directory order. Where there are several and none has a
/// server's name, none: `cmd/migrate` and `cmd/seed` are tools, and taken
/// as the dev server one would run against the database and exit. A lone
/// command with a tool's name is no server either. Empty for a module
/// whose root is a main package, and for a library, which is level zero
/// as a library crate is.
pub(super) fn go_commands(root: &Path) -> Vec<String> {
    if go_main(root) {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(root.join("cmd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir() && go_main(&entry.path()))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    if names.len() < 2 {
        names.retain(|name| !GO_TOOL_COMMANDS.contains(&name.as_str()));
        return names;
    }
    let module = go_module_name(root);
    let serves = |name: &str| {
        !GO_TOOL_COMMANDS.contains(&name)
            && (GO_SERVER_COMMANDS.contains(&name) || module.as_deref() == Some(name))
    };
    if !names.iter().any(|name| serves(name)) {
        return Vec::new();
    }
    names.sort_by_key(|name| !serves(name));
    names
}

/// The last element of a Go module's path, past a major version suffix:
/// `shop` for `example.com/shop/v2`.
fn go_module_name(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    let path = text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("module")?;
        rest.starts_with(char::is_whitespace)
            .then(|| rest.split("//").next().unwrap_or(rest).trim())
    })?;
    let mut elements = path.trim_matches('"').rsplit('/');
    let last = elements.next()?;
    let major = last
        .strip_prefix('v')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    match major {
        true => elements.next(),
        false => Some(last),
    }
    .filter(|name| !name.is_empty())
    .map(str::to_string)
}

/// Whether the Go package in `dir` is a main package: one of its `.go`
/// files, tests aside, declares `package main`.
fn go_main(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.ends_with(".go")
            && !name.ends_with("_test.go")
            && std::fs::read_to_string(entry.path())
                .is_ok_and(|text| go_package(&text) == Some("main"))
    })
}

/// The package a Go file declares: the first `package` clause, past the
/// comments and build constraints before it. None for a file its build
/// constraint keeps out of every build, as a library's `gen.go` generator
/// is: `package main` there is what `go run gen.go` runs, and no part of
/// the package `go run .` would.
fn go_package(text: &str) -> Option<&str> {
    let mut in_block = false;
    for line in text.lines() {
        let mut line = line.trim();
        if in_block {
            let Some((_, rest)) = line.split_once("*/") else {
                continue;
            };
            in_block = false;
            line = rest.trim();
        }
        while let Some(rest) = line.strip_prefix("/*") {
            match rest.split_once("*/") {
                Some((_, after)) => line = after.trim(),
                None => {
                    in_block = true;
                    line = "";
                }
            }
        }
        if ignored(line) {
            return None;
        }
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let clause = line.strip_prefix("package")?;
        if !clause.starts_with(char::is_whitespace) {
            return None;
        }
        return clause
            .split_whitespace()
            .next()
            .map(|name| name.trim_end_matches(';'));
    }
    None
}

/// Whether a line is the build constraint that keeps its file out of
/// every build: `//go:build ignore`, or the older `// +build ignore`.
fn ignored(line: &str) -> bool {
    let constraint = line.strip_prefix("//go:build").or_else(|| {
        line.strip_prefix("//")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix("+build"))
    });
    constraint.is_some_and(|expr| expr.starts_with(char::is_whitespace) && expr.trim() == "ignore")
}

#[cfg(test)]
mod tests {
    use super::{Signals, framework, mentions};

    fn with_scripts(pairs: &[(&str, &str)]) -> Signals {
        Signals {
            scripts: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn named(signals: &Signals) -> Option<&'static str> {
        framework(std::path::Path::new("/nonexistent"), signals).map(|rule| rule.name)
    }

    // An Express app tested with vitest is an Express app: read as Vite,
    // it would be started with `--port` appended to `node server.js`.
    #[test]
    fn a_test_runner_does_not_make_a_project_a_dev_server() {
        let signals = with_scripts(&[("dev", "node server.js"), ("test", "vitest run")]);
        assert_eq!(named(&signals), Some("Node"));
    }

    #[test]
    fn astro_and_angular_are_their_own_frameworks() {
        assert_eq!(named(&with_scripts(&[("dev", "astro dev")])), Some("Astro"));
        assert_eq!(
            named(&with_scripts(&[("start", "ng serve")])),
            Some("Angular")
        );
        assert_eq!(named(&with_scripts(&[("dev", "vite")])), Some("Vite"));
        assert_eq!(
            named(&with_scripts(&[("dev", "react-router dev")])),
            Some("Vite")
        );
    }

    #[test]
    fn a_script_marker_matches_a_whole_word_only() {
        assert!(mentions("vite", "vite"));
        assert!(mentions("vite --host", "vite"));
        assert!(mentions("./node_modules/.bin/vite dev", "vite"));
        assert!(mentions("concurrently \"vite\" \"tsc -w\"", "vite"));
        assert!(!mentions("vitest run", "vite"));
        assert!(!mentions("vite-node src/main.ts", "vite"));
        assert!(!mentions("invite-users", "vite"));
        assert!(mentions("node server.js", "node "));
        assert!(!mentions("nodemon server.js", "node "));
        assert!(mentions("nodemon server.js", "nodemon"));
        assert!(mentions("next dev --turbo", "next dev"));
    }
}
