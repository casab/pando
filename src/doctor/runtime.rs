//! The runtime section: the language versions the project asks for and what
//! the shell finds.

use std::fmt::Write as _;

use crate::actions::{self, Machine};
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
    let requirements = runtime::requirements_for(paths.root(), &config.runtime.version_files);

    let mut languages = Vec::new();
    for requirement in runtime::to_compare(&requirements) {
        let Some(entry) = runtime::language(&requirement.language) else {
            continue;
        };
        // Where its processes run, which is where a runner's lockfile is.
        let dir = match &requirement.dir {
            Some(dir) => paths.root().join(dir),
            None => paths.root().to_path_buf(),
        };
        // `runtime::check`, never `actions`' own first-mismatch walk: that
        // one remembers what passed, and doctor writes nothing. It also
        // stops at the first language, and a report that named one of two
        // problems would be the kind of report this command exists to
        // replace.
        let check = runtime::check(requirement, effective, machine.shell);
        let installed = runtime::installed(entry, &machine.home, &machine.system);
        let managers: Vec<&'static str> = installed.iter().map(|manager| manager.name).collect();
        // For a mismatch, the lines the prelude question would offer, each
        // tried on this machine as the question tries it; writing nothing,
        // so a line that works is not remembered. For anything else the
        // managers' lines as they are: nothing needs fixing, and trying
        // them would be a shell each for no question.
        let offers: Option<Vec<actions::Offer>> = match check.verdict {
            Verdict::Mismatch => Some(
                actions::prelude_offers(paths, config, &requirements, &check, machine, false)
                    .unwrap_or_default(),
            ),
            _ => None,
        };
        let fixes: Vec<String> = match &offers {
            Some(offers) => offers
                .iter()
                .map(|offer| format!("{}  ({})", offer.line, offer.why))
                .collect(),
            None => runtime::fixes(entry, &machine.home, &machine.system, requirement)
                .into_iter()
                .map(|fix| format!("{}  ({})", fix.line, fix.why))
                .collect(),
        };
        // Printed, never run: what would put the version under the first
        // manager here that installs one.
        let install = installed.iter().find_map(|manager| {
            Some((
                manager.name,
                manager.install_command(entry, &requirement.spec)?,
            ))
        });
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
                // Only a binary that exited non-zero leaves one here.
                true => check.resolved.failure.clone(),
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
        // Not where every command goes through a runner that finds the
        // interpreter itself, such as `uv run`: a start skips the check
        // there, so what the shell resolves on its own breaks nothing.
        if check.verdict == Verdict::Mismatch
            && !actions::runs_through_runner(&dir, config, entry, effective, machine.shell)
        {
            let offers = offers.unwrap_or_default();
            let fix = mismatch_fix(&report, &offers, install);
            findings.push(mismatch_finding(
                &report,
                fix,
                prelude.as_deref(),
                &prelude_from,
            ));
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
    fix: String,
    prelude: Option<&str>,
    prelude_from: &Option<String>,
) -> Finding {
    let wanted = format!(
        "{} {} ({})",
        language.language, language.spec, language.source
    );
    let got = match (
        &language.resolved,
        &language.resolved_from,
        &language.failure,
    ) {
        (Some(version), Some(path), _) => {
            format!("`bash -lc` here resolves {version}, from {path}")
        }
        (None, Some(path), Some(failure)) => {
            format!("`bash -lc` here finds {path}, and it fails: {failure}")
        }
        (_, _, Some(failure)) => format!("the prelude never got as far as asking: {failure}"),
        (_, _, None) => format!("`bash -lc` here has no {} at all", language.language),
    };
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

/// What to do about a mismatch: the lines the prelude question would
/// offer, or, when none of them works yet, what would make one.
///
/// "No manager" and "a manager without this version" are different
/// sentences. nvm listed beside "no version manager pando knows about"
/// said two things at once; a manager that is here and gives `bash -lc`
/// nothing it accepts is fixed by its own install command, which pando
/// prints and never runs.
fn mismatch_fix(
    language: &LanguageReport,
    offers: &[actions::Offer],
    install: Option<(&'static str, String)>,
) -> String {
    let wanted = format!("{} {}", language.language, language.spec);
    let mut fix = String::new();
    let _ = writeln!(
        fix,
        "pando runs every command with `bash -lc`, which is not your interactive shell"
    );
    let line = |offer: &actions::Offer| format!("  {}  ({})", offer.line, offer.why);
    if offers.iter().any(|offer| offer.works) {
        let _ = writeln!(fix, "set [runtime].prelude to one of:");
        for offer in offers {
            let _ = writeln!(fix, "{}", line(offer));
        }
    } else {
        let first_on_path =
            format!("set [runtime].prelude to a line that puts a {wanted} first on PATH");
        let _ = match (language.managers.as_slice(), install) {
            ([], _) => writeln!(
                fix,
                "no version manager pando knows about is installed for {} — install one, or \
                 {first_on_path}",
                language.language
            ),
            (managers, Some((manager, command))) => writeln!(
                fix,
                "{} installed here, and {} `bash -lc` no {wanted} — `{command}` puts one under \
                 {manager} (pando never installs one), or {first_on_path}",
                is_are(managers),
                match managers.len() {
                    1 => "gives",
                    _ => "none gives",
                }
            ),
            (managers, None) => writeln!(
                fix,
                "{} installed here, and {} `bash -lc` no {wanted} — install it under one, or \
                 {first_on_path}",
                is_are(managers),
                match managers.len() {
                    1 => "gives",
                    _ => "none gives",
                }
            ),
        };
        if !offers.is_empty() {
            let _ = writeln!(fix, "then [runtime].prelude can be one of:");
            for offer in offers {
                let _ = writeln!(fix, "{}", line(offer));
            }
        }
    }
    fix
}

/// `nvm is`, `nvm and fnm are`: the managers as the subject of a sentence.
fn is_are(managers: &[&str]) -> String {
    match managers {
        [one] => format!("{one} is"),
        [rest @ .., last] => format!("{} and {last} are", rest.join(", ")),
        [] => String::new(),
    }
}
