//! Commands that run at a point in a worktree's life, at most as often as
//! they need to.
//!
//! This phase has exactly one, the built-in install step, but it is built as
//! the general mechanism rather than a special case: a hook is a name, a
//! command, and a set of globs whose content decides whether it has to run
//! again. `[[hooks]]` in Phase 3 is the same thing with the name and
//! command coming from config.
//!
//! A hook's output goes to its own log file under pando's home, never to
//! the terminal: the TUI paints an alternate screen, and a child that
//! writes to stdout would paint over it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Directories never walked when matching a fingerprint glob. Dependency
/// trees and build output are enormous and are never what a hook is keyed
/// on.
const PRUNED: [&str; 10] = [
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".venv",
    "vendor",
    "__pycache__",
];

/// How deep a wildcard glob is walked. Deep enough for `*/migrations/*.sql`
/// in a monorepo, shallow enough that a pathological tree cannot stall a
/// start.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutcome {
    Ran,
    /// Its fingerprint has not changed since the last run.
    Skipped,
}

/// A content hash over the files the globs match, or `None` when the globs
/// match nothing at all.
///
/// `None` is "no fingerprint", which means the hook runs every time — the
/// same thing an empty `fingerprint` list means. A project with no lockfile
/// has no way to tell pando its dependencies are unchanged.
pub fn fingerprint(worktree: &Path, globs: &[String]) -> Option<String> {
    if globs.is_empty() {
        return None;
    }
    let mut matched: Vec<PathBuf> = Vec::new();
    for glob in globs {
        if is_literal(glob) {
            // The common case by far, and it needs no walk at all.
            let path = worktree.join(glob);
            if path.is_file() {
                matched.push(PathBuf::from(glob));
            }
        } else {
            collect(worktree, Path::new(""), glob, 0, &mut matched);
        }
    }
    if matched.is_empty() {
        return None;
    }
    matched.sort();
    matched.dedup();
    let mut context = md5::Context::new();
    for relative in &matched {
        // The path is hashed too: a file renamed is a change, even when the
        // bytes are the same.
        context.consume(relative.to_string_lossy().as_bytes());
        context.consume([0]);
        match std::fs::read(worktree.join(relative)) {
            Ok(bytes) => context.consume(&bytes),
            // A file that vanished between the walk and the read is a
            // change like any other.
            Err(_) => context.consume(b"<unreadable>"),
        }
        context.consume([0]);
    }
    Some(format!("md5:{:x}", context.finalize()))
}

fn is_literal(glob: &str) -> bool {
    !glob.contains(['*', '?'])
}

fn collect(worktree: &Path, relative: &Path, glob: &str, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(worktree.join(relative)) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if PRUNED.contains(&name.as_ref()) {
            continue;
        }
        let child = relative.join(name.as_ref());
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(worktree, &child, glob, depth + 1, out);
        } else if matches_glob(glob, &child.to_string_lossy()) {
            out.push(child);
        }
    }
}

/// Path glob matching: `*` and `?` inside one segment, `**` across any
/// number of them.
pub fn matches_glob(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // Zero or more segments: try every split point.
            (0..=path.len()).any(|skip| match_segments(&pattern[1..], &path[skip..]))
        }
        Some(segment) => match path.first() {
            Some(name) if match_one(segment, name) => match_segments(&pattern[1..], &path[1..]),
            _ => false,
        },
    }
}

/// `*` matches any run of characters inside a segment, `?` exactly one.
fn match_one(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    let (mut star, mut star_vi) = (None, 0usize);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_vi = vi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_vi += 1;
            vi = star_vi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Runs a hook to completion, appending everything it printed to its own
/// log. Fails with the log path, because that is where the reason is.
///
/// `.output()` rather than an inherited stdio: nothing pando runs may write
/// to the terminal, and a hook started from the TUI would otherwise paint
/// over the frame.
pub fn run(log_file: &Path, shell_cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<()> {
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create log dir {}", parent.display()))?;
    }
    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(shell_cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command
        .output()
        .with_context(|| format!("run {shell_cmd:?}"))?;

    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    append(log_file, &text);

    if out.status.success() {
        return Ok(());
    }
    let reason = last_line(&text);
    anyhow::bail!(
        "exited {}{reason} — the whole log is at {}",
        out.status.code().unwrap_or(-1),
        log_file.display()
    )
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = file.write_all(b"\n");
        }
    }
}

/// The closing line of the output, which is where a build tool puts the
/// reason it stopped.
fn last_line(text: &str) -> String {
    match text.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(line) => format!(": {}", line.trim()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    // ---- globs -----------------------------------------------------------

    #[test]
    fn a_glob_matches_within_one_segment() {
        assert!(matches_glob("pnpm-lock.yaml", "pnpm-lock.yaml"));
        assert!(matches_glob("*.lock", "uv.lock"));
        assert!(matches_glob("a/b.txt", "a/b.txt"));
        assert!(!matches_glob("*.lock", "a/uv.lock"));
        assert!(!matches_glob("a/b.txt", "a/c.txt"));
        assert!(matches_glob("?.txt", "a.txt"));
        assert!(!matches_glob("?.txt", "ab.txt"));
    }

    #[test]
    fn a_double_star_matches_any_number_of_directories() {
        assert!(matches_glob(
            "prisma/migrations/**",
            "prisma/migrations/1/up.sql"
        ));
        assert!(matches_glob("prisma/migrations/**", "prisma/migrations/x"));
        assert!(matches_glob("**/*.py", "app/migrations/0001.py"));
        assert!(matches_glob("**/*.py", "a.py"));
        assert!(!matches_glob("**/*.py", "a.rs"));
        assert!(matches_glob("*/migrations/*.py", "app/migrations/0001.py"));
        assert!(!matches_glob("*/migrations/*.py", "a/b/migrations/0001.py"));
    }

    // ---- fingerprints ----------------------------------------------------

    #[test]
    fn a_fingerprint_changes_only_when_the_content_does() {
        let dir = tempdir().unwrap();
        write(dir.path(), "pnpm-lock.yaml", "one\n");
        let globs = vec!["pnpm-lock.yaml".to_string()];
        let first = fingerprint(dir.path(), &globs).unwrap();
        assert_eq!(fingerprint(dir.path(), &globs).unwrap(), first);

        write(dir.path(), "pnpm-lock.yaml", "two\n");
        assert_ne!(fingerprint(dir.path(), &globs).unwrap(), first);

        // Something else changing is not this hook's business.
        write(dir.path(), "README.md", "hello");
        let second = fingerprint(dir.path(), &globs).unwrap();
        write(dir.path(), "README.md", "goodbye");
        assert_eq!(fingerprint(dir.path(), &globs).unwrap(), second);
    }

    #[test]
    fn nothing_to_fingerprint_means_run_every_time() {
        let dir = tempdir().unwrap();
        assert_eq!(fingerprint(dir.path(), &[]), None);
        assert_eq!(fingerprint(dir.path(), &["absent.lock".to_string()]), None);
    }

    #[test]
    fn a_wildcard_fingerprint_covers_every_file_it_matches() {
        let dir = tempdir().unwrap();
        write(dir.path(), "prisma/migrations/1_init/up.sql", "create;");
        let globs = vec!["prisma/migrations/**".to_string()];
        let first = fingerprint(dir.path(), &globs).unwrap();

        write(dir.path(), "prisma/migrations/2_more/up.sql", "alter;");
        assert_ne!(
            fingerprint(dir.path(), &globs).unwrap(),
            first,
            "a new migration is a reason to run again"
        );
    }

    // A fingerprint walk must never descend into a dependency tree: it would
    // be slower than the install it is trying to skip.
    #[test]
    fn a_walk_never_enters_a_dependency_tree() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/a.js", "x");
        write(dir.path(), "node_modules/pkg/index.js", "y");
        let globs = vec!["**/*.js".to_string()];
        let first = fingerprint(dir.path(), &globs).unwrap();
        write(dir.path(), "node_modules/pkg/index.js", "changed");
        assert_eq!(fingerprint(dir.path(), &globs).unwrap(), first);
    }

    #[test]
    fn a_renamed_file_is_a_change() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.lock", "same");
        let globs = vec!["*.lock".to_string()];
        let first = fingerprint(dir.path(), &globs).unwrap();
        std::fs::remove_file(dir.path().join("a.lock")).unwrap();
        write(dir.path(), "b.lock", "same");
        assert_ne!(fingerprint(dir.path(), &globs).unwrap(), first);
    }

    // ---- running ---------------------------------------------------------

    #[test]
    fn a_hook_writes_what_it_printed_to_its_log() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("logs/feat+one/install.log");
        run(
            &log,
            "echo installing && echo a-warning >&2",
            dir.path(),
            &[],
        )
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("installing"), "{text}");
        assert!(text.contains("a-warning"), "stderr too: {text}");
    }

    #[test]
    fn a_failing_hook_names_its_log_and_its_last_line() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("install.log");
        let err = run(
            &log,
            "echo ERR_PNPM_OUTDATED_LOCKFILE >&2 && exit 3",
            dir.path(),
            &[],
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exited 3"), "{msg}");
        assert!(msg.contains("ERR_PNPM_OUTDATED_LOCKFILE"), "{msg}");
        assert!(msg.contains("install.log"), "{msg}");
        assert!(std::fs::read_to_string(&log).unwrap().contains("ERR_PNPM"));
    }

    #[test]
    fn a_hook_runs_in_its_directory_with_its_environment() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let log = dir.path().join("install.log");
        run(
            &log,
            "pwd && echo \"name=$PANDO_NAME\"",
            &dir.path().join("apps/web"),
            &[("PANDO_NAME".to_string(), "feat+one".to_string())],
        )
        .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("apps/web"), "{text}");
        assert!(text.contains("name=feat+one"), "{text}");
    }
}
