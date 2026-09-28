//! Artifacts: files a tool writes into a checkout on its own, which git
//! ignores and no worktree needs a copy of.
//!
//! What `provision` proposes is "gitignored and present in the main
//! checkout", and a test run, a type checker or an editor leaves plenty
//! that is both: a coverage database is gitignored and present, and it is
//! not a local setting a worktree is missing. Each row here is one name
//! that is never offered.

/// Tool caches, build output and editor noise, by file or directory name.
///
/// Directories are listed as well as files: `provision` only ever offers
/// files, so a directory here changes nothing today, but this is the one
/// list of what a worktree never needs, and a directory is as much a
/// cache as a file is.
pub const ARTIFACTS: [&str; 24] = [
    // Editors and operating systems.
    ".DS_Store",
    "Thumbs.db",
    // Package managers' own logs.
    "npm-debug.log",
    "yarn-error.log",
    "pnpm-debug.log",
    // JavaScript build output and caches.
    "tsconfig.tsbuildinfo",
    ".eslintcache",
    "node_modules",
    ".next",
    ".nuxt",
    ".output",
    ".turbo",
    ".parcel-cache",
    // Python caches and environments.
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    // Coverage reports, in both ecosystems.
    ".coverage",
    "coverage.xml",
    "htmlcov",
    ".nyc_output",
    "lcov.info",
];

/// Name prefixes that make a file an artifact, for a tool that writes one
/// file per run: coverage.py's parallel mode writes
/// `.coverage.<host>.<pid>.<random>`.
const ARTIFACT_PREFIXES: [&str; 1] = [".coverage."];

/// Whether a file or directory name, the last component of a path, is an
/// artifact.
pub fn is_artifact(name: &str) -> bool {
    ARTIFACTS.contains(&name) || ARTIFACT_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coverage_database_is_an_artifact_and_an_env_file_is_not() {
        assert!(is_artifact(".coverage"));
        assert!(is_artifact(".coverage.laptop.1234.567890"));
        assert!(is_artifact(".pytest_cache"));
        assert!(!is_artifact(".env"));
        assert!(!is_artifact(".env.local"));
        assert!(!is_artifact("coverage"), "a directory of the project's own");
    }

    #[test]
    fn every_artifact_is_listed_once() {
        for (i, name) in ARTIFACTS.iter().enumerate() {
            assert!(!ARTIFACTS[i + 1..].contains(name), "{name} is listed twice");
        }
    }
}
