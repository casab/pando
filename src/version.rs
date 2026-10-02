//! What this binary is: pando's version, and the branch a development
//! build was made from.
//!
//! `scripts/dev` builds a branch with `PANDO_BUILD_LABEL=<branch>@<sha>`,
//! so a pando tried on a real project says which one it is in `--version`
//! and the TUI's header. A release sets nothing and reads as it always
//! has. cargo rebuilds when the variable changes.

use std::sync::LazyLock;

/// The branch and commit a development build was made from; `None` for a
/// release, or any build nobody labelled.
pub fn label() -> Option<&'static str> {
    clean(option_env!("PANDO_BUILD_LABEL"))
}

/// `0.6.2`, or `0.6.2 (git-menu@b7bb2b4)` for a development build: what
/// `--version` prints.
pub fn version() -> &'static str {
    static VERSION: LazyLock<String> =
        LazyLock::new(|| with_label(env!("CARGO_PKG_VERSION"), label()));
    VERSION.as_str()
}

fn clean(label: Option<&str>) -> Option<&str> {
    label.map(str::trim).filter(|l| !l.is_empty())
}

fn with_label(version: &str, label: Option<&str>) -> String {
    match clean(label) {
        Some(label) => format!("{version} ({label})"),
        None => version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_reads_as_its_version_alone() {
        assert_eq!(with_label("0.6.2", None), "0.6.2");
        assert_eq!(with_label("0.6.2", Some("  ")), "0.6.2");
    }

    #[test]
    fn a_development_build_names_its_branch_after_the_version() {
        assert_eq!(
            with_label("0.6.2", Some("git-menu@b7bb2b4")),
            "0.6.2 (git-menu@b7bb2b4)"
        );
    }

    #[test]
    fn the_test_build_is_unlabelled_unless_somebody_labelled_it() {
        assert_eq!(
            version(),
            with_label(env!("CARGO_PKG_VERSION"), option_env!("PANDO_BUILD_LABEL"))
        );
    }
}
