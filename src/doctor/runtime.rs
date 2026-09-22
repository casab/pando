//! The runtime section: the language versions the project asks for and what
//! the shell finds.

use std::fmt::Write as _;

use crate::actions::Machine;
use crate::config::{self, Config};
use crate::paths::PandoPaths;
use crate::runtime::{self, Verdict};

use super::report::{Finding, LanguageReport, RuntimeReport, Section, Severity};

pub(super) fn runtime_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> RuntimeReport {
    let prelude = config.runtime.prelude.clone();
    let effective = prelude.clone().unwrap_or_default();
    let effective = effective.trim();
    let prelude_from = config::prelude_origin(paths).map(|p| p.display().to_string());
    let requirements = runtime::requirements(paths.root());

    let mut languages = Vec::new();
    for entry in &runtime::LANGUAGES {
        let Some(requirement) = runtime::for_language(&requirements, entry.name) else {
            continue;
        };
        // `runtime::check`, never `actions`' own first-mismatch walk: that
        // one remembers what passed, and doctor writes nothing. It also
        // stops at the first language, and a report that named one of two
        // problems would be the kind of report this command exists to
        // replace.
        let check = runtime::check(requirement, effective, machine.shell);
        let managers: Vec<&'static str> = runtime::installed(entry, &machine.home)
            .into_iter()
            .map(|manager| manager.name)
            .collect();
        let fixes: Vec<String> = runtime::fixes(
            entry,
            &machine.home,
            runtime::from_version_file(entry, requirement),
        )
        .into_iter()
        .map(|fix| format!("{}  ({})", fix.line, fix.why))
        .collect();
        let report = LanguageReport {
            language: requirement.language.clone(),
            spec: requirement.spec.clone(),
            source: requirement.source.clone(),
            resolved: check.resolved.version.clone(),
            resolved_from: check.resolved.path.clone(),
            verdict: match check.verdict {
                Verdict::Satisfied => "satisfied",
                Verdict::Mismatch => "mismatch",
                Verdict::Unknown => "unknown",
            },
            failure: match check.resolved.ran {
                true => None,
                false => Some(
                    check
                        .resolved
                        .failure
                        .clone()
                        .unwrap_or_else(|| "no output".to_string()),
                ),
            },
            managers,
            fixes,
        };
        if check.verdict == Verdict::Mismatch {
            findings.push(mismatch_finding(&report, prelude.as_deref(), &prelude_from));
        }
        languages.push(report);
    }
    RuntimeReport {
        prelude,
        prelude_from,
        languages,
        requirements,
    }
}

/// A mismatch, at the severity the three outcomes in the plan give it.
///
/// With no prelude set it is a **note**: nobody has been asked yet, and
/// the next `start` asks — exit 3 with the question, which is designed
/// behaviour rather than a break. With one set, empty included, it is a
/// **problem**: somebody has said how this machine resolves the runtime,
/// and it does not, so the next start spawns a process that dies of it.
fn mismatch_finding(
    language: &LanguageReport,
    prelude: Option<&str>,
    prelude_from: &Option<String>,
) -> Finding {
    let wanted = format!(
        "{} {} ({})",
        language.language, language.spec, language.source
    );
    let got = match (&language.resolved, &language.resolved_from) {
        (Some(version), Some(path)) => format!("`bash -lc` here resolves {version}, from {path}"),
        _ => match &language.failure {
            Some(failure) => format!("the prelude never got as far as asking: {failure}"),
            None => format!("`bash -lc` here has no {} at all", language.language),
        },
    };
    let mut fix = String::new();
    let _ = writeln!(
        fix,
        "pando runs every command with `bash -lc`, which is not your interactive shell"
    );
    match language.fixes.as_slice() {
        [] => {
            let _ = writeln!(
                fix,
                "no version manager pando knows about is installed for {} — install one, or \
                 put the right binary on the PATH a login bash shell has",
                language.language
            );
        }
        fixes => {
            let _ = writeln!(fix, "set [runtime].prelude to one of:");
            for line in fixes {
                let _ = writeln!(fix, "  {line}");
            }
        }
    }
    match prelude {
        None => Finding {
            section: Section::Runtime,
            severity: Severity::Note,
            message: format!(
                "this project asks for {wanted}, and {got} — nobody has answered the prelude \
                 question, so the next `start` will ask"
            ),
            fix: Some(fix.trim_end().to_string()),
        },
        Some(line) if line.trim().is_empty() => Finding {
            section: Section::Runtime,
            severity: Severity::Problem,
            message: format!(
                "this project asks for {wanted}, and {got} — `[runtime].prelude` is set to \"\", \
                 which says this machine needs nothing in front of a command"
            ),
            fix: Some(fix.trim_end().to_string()),
        },
        Some(line) => Finding {
            section: Section::Runtime,
            severity: Severity::Problem,
            message: format!(
                "this project asks for {wanted}, and {got} — the prelude {line:?}{} is not \
                 working",
                match prelude_from {
                    Some(from) => format!(" in {from}"),
                    None => String::new(),
                }
            ),
            fix: Some(fix.trim_end().to_string()),
        },
    }
}
