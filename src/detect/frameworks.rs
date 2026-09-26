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
        let by_script = rule
            .script_markers
            .iter()
            .any(|needle| signals.scripts.values().any(|body| mentions(body, needle)));
        // A Cargo.toml with no binary is a library, and a mix.exs with no
        // Phoenix in it is some other Mix project: nothing this rule serves.
        if by_marker && !passes(root, rule.guard) {
            return false;
        }
        by_marker || by_script
    })
}

/// Whether the project at `root` is what a rule's guard asks for.
fn passes(root: &Path, guard: Guard) -> bool {
    match guard {
        Guard::Marker => true,
        Guard::BinaryCrate => binary_crate(root),
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
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    body.match_indices(needle).any(|(at, _)| {
        let before = body[..at].chars().next_back().is_none_or(|c| !word(c));
        let after = body[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !word(c));
        // A needle that ends in a space ("node ") has already said where
        // the word ends.
        before && (after || needle.ends_with(' '))
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
