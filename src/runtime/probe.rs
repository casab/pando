//! What this machine actually resolves: one shell asked for every
//! binary's path and version, compared to what the project asked for.

use super::Requirement;
use super::languages::Language;
use super::languages::language;
use super::version::first_version;
use super::version::satisfies;

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
pub(super) const PATH_MARK: &str = "pando-runtime-path:";
pub(super) const VERSION_MARK: &str = "pando-runtime-version:";
pub(super) const STATUS_MARK: &str = "pando-runtime-status:";
pub(super) const DONE_MARK: &str = "pando-runtime-ok";

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
    /// Why no version came back: the last line of output when the body
    /// never ran, usually the prelude's own error, or the binary's first
    /// line when it exited non-zero, which is how a version manager's shim
    /// says the version it was asked for is not installed.
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

/// Asks the shell what it resolves, and compares it to what the project
/// asked for.
///
/// The composition mirrors a real spawn — `<prelude> && { <command> }`, as
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
            // There, and it failed when asked its version. A pyenv, rbenv
            // or nodenv shim whose pinned version is not installed says
            // "version `3.12.1' is not installed" and exits 1, and every
            // command the project runs would fail the same way. The number
            // in that line is the one asked for, never one that resolved.
            (Some(_), _) if resolved.failure.is_some() => Verdict::Mismatch,
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
/// that exists, its path, its version, and the status it exited with, each
/// behind a marker.
///
/// The output is captured before its first line is taken, because a pipe
/// into `head` would report `head`'s status rather than the binary's; and
/// the status is taken on the right of an `||`, where a prelude's `set -e`
/// does not end the shell over it.
pub fn probe_command(language: &Language, prelude: &str) -> String {
    let body = format!(
        "for __pando_bin in {}; do if command -v \"$__pando_bin\" >/dev/null 2>&1; then \
         echo \"{PATH_MARK}$(command -v \"$__pando_bin\")\"; \
         __pando_s=0; __pando_v=$(\"$__pando_bin\" {} 2>&1) || __pando_s=$?; \
         echo \"{VERSION_MARK}$(printf '%s\\n' \"$__pando_v\" | head -n 1)\"; \
         echo \"{STATUS_MARK}$__pando_s\"; break; fi; done; \
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
    let mut answer = "";
    let mut status = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix(PATH_MARK) {
            resolved.path = Some(path.trim().to_string()).filter(|p| !p.is_empty());
        } else if let Some(version) = line.strip_prefix(VERSION_MARK) {
            answer = version.trim();
            resolved.version = first_version(version);
        } else if let Some(code) = line.strip_prefix(STATUS_MARK) {
            status = code.trim().parse::<i32>().ok();
        } else if line == DONE_MARK {
            resolved.ran = true;
        }
    }
    if let Some(code) = status.filter(|code| *code != 0) {
        resolved.version = None;
        resolved.failure = Some(match answer {
            "" => format!("it exited with status {code}"),
            answer => answer.to_string(),
        });
    } else if !resolved.ran {
        resolved.failure = output
            .lines()
            .map(str::trim)
            .rev()
            .find(|line| !line.is_empty())
            .map(str::to_string);
    }
    resolved
}

/// The output a probe would produce on a machine that resolves `version`
/// at `path`.
///
/// Test-only, and here rather than in the tests that use it: the markers
/// are this module's, and a fake that spelled them out itself would drift
/// from them.
#[cfg(test)]
pub(crate) fn probe_reply(path: &str, version: &str) -> String {
    format!("{PATH_MARK}{path}\n{VERSION_MARK}{version}\n{STATUS_MARK}0\n{DONE_MARK}\n")
}

/// The output a probe would produce on a machine where the binary at
/// `path` answers `-v` with `line` and exits with `status`, as a version
/// manager's shim does when the version it was asked for is not
/// installed.
#[cfg(test)]
pub(crate) fn probe_failure(path: &str, line: &str, status: i32) -> String {
    format!("{PATH_MARK}{path}\n{VERSION_MARK}{line}\n{STATUS_MARK}{status}\n{DONE_MARK}\n")
}
