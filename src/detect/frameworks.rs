//! Which framework rule a repository matches.

use std::path::Path;

use crate::catalog::frameworks::{FrameworkRule, RULES};

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
            .any(|needle| signals.scripts.values().any(|body| body.contains(needle)));
        // A Cargo.toml with no binary is a library: nothing to serve.
        if rule.binary_only && by_marker && !binary_crate(root) {
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
