//! The tools section: every program the project runs, probed in the shell a
//! start uses.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::actions::Machine;
use crate::catalog::package_managers;
use crate::config::{Config, ServiceConfig};
use crate::paths::PandoPaths;
use crate::process as proc;
use crate::runtime;
use crate::{detect, services, tunnel};

use super::report::{Finding, Section, Severity, ToolReport};

pub(super) const TOOL_PATH_MARK: &str = "pando-tool-path:";
pub(super) const TOOL_VERSION_MARK: &str = "pando-tool-version:";
pub(super) const TOOL_DETAIL_MARK: &str = "pando-tool-detail:";
pub(super) const TOOL_DONE_MARK: &str = "pando-tool-ok";

/// One executable to look for, and what to ask it.
pub(super) struct ToolProbe {
    pub(super) name: String,
    pub(super) program: String,
    /// What to pass it for a version. Static, and pando's own: nothing a
    /// project wrote reaches a command line here.
    pub(super) args: &'static str,
    /// A second question worth one line, such as which docker context is
    /// active.
    pub(super) detail_args: Option<&'static str>,
    /// How the detail is introduced when there is one.
    pub(super) detail_label: &'static str,
    pub(super) needed_for: String,
    /// What to say when it is not there. `None` means a line and nothing
    /// more — a tool this project has not asked pando to run.
    pub(super) missing: Option<(Severity, String)>,
}

#[derive(Default)]
struct ToolFound {
    path: Option<String>,
    version: Option<String>,
    detail: Option<String>,
}

pub(super) fn tools_report(
    paths: &PandoPaths,
    config: &Config,
    machine: &Machine<'_>,
    findings: &mut Vec<Finding>,
) -> Vec<ToolReport> {
    let probes = tool_probes(paths, config);
    // Run behind the prelude, because every command pando runs is run
    // behind it: a package manager that only exists after `nvm use` is
    // there for a real spawn and would read as missing here.
    let prelude = config.runtime.prelude.clone().unwrap_or_default();
    let (found, failure) = probe_tools(machine.shell, &probes, prelude.trim());
    if let Some(failure) = &failure {
        findings.push(Finding::note(
            Section::Tools,
            format!("pando could not ask this shell what it has: {failure}"),
        ));
    }
    let mut out = Vec::new();
    for (index, probe) in probes.iter().enumerate() {
        let entry = found.get(&index);
        let present = entry.is_some_and(|f| f.path.is_some());
        // Only when the probe itself ran: "not found" is a claim about
        // this machine, and a shell that never answered has not made it.
        if !present
            && failure.is_none()
            && let Some((severity, reason)) = &probe.missing
        {
            findings.push(Finding {
                section: Section::Tools,
                severity: *severity,
                message: format!(
                    "{} is not on the PATH `bash -lc` has — {reason}",
                    probe.name
                ),
                fix: Some(
                    "install it, or set [runtime].prelude so a login bash shell finds it"
                        .to_string(),
                ),
            });
        }
        out.push(ToolReport {
            name: probe.name.clone(),
            path: entry.and_then(|f| f.path.clone()),
            version: entry.and_then(|f| f.version.clone()),
            detail: entry
                .and_then(|f| f.detail.clone())
                .map(|d| format!("{}{d}", probe.detail_label)),
            needed_for: probe.needed_for.clone(),
            found: present,
        });
    }
    out
}

/// Every tool worth asking about: the ones pando itself runs, and the ones
/// this project's own config and lockfiles say it will run.
fn tool_probes(paths: &PandoPaths, config: &Config) -> Vec<ToolProbe> {
    let mut probes = vec![ToolProbe {
        name: "git".to_string(),
        program: "git".to_string(),
        args: "--version",
        detail_args: None,
        detail_label: "",
        needed_for: "everything: worktrees are git's".to_string(),
        missing: Some((
            Severity::Problem,
            "pando has nothing to manage without it".to_string(),
        )),
    }];

    // The shim hook `services::docker_program` already knows about, so a
    // developer whose docker is not on PATH is reported through the same
    // binary an isolated start would use.
    let docker = services::docker_program(paths).display().to_string();
    let isolates = config
        .services
        .iter()
        .any(|s| matches!(s, ServiceConfig::Compose { .. }));
    let needed = "`start --isolated`, which runs private copies of the project's services";
    let missing_docker = isolates.then(|| {
        (
            Severity::Note,
            "this project declares compose services, so `--isolated` cannot run; a plain \
             `start` still can"
                .to_string(),
        )
    });
    probes.push(ToolProbe {
        name: "docker".to_string(),
        program: docker.clone(),
        args: "--version",
        detail_args: Some("context show"),
        detail_label: "context: ",
        needed_for: needed.to_string(),
        missing: missing_docker.clone(),
    });
    probes.push(ToolProbe {
        name: "docker compose".to_string(),
        program: docker,
        args: "compose version --short",
        detail_args: None,
        detail_label: "",
        needed_for: needed.to_string(),
        // Never its own finding: when docker is missing this is the same
        // news twice, and when docker is there a compose plugin that is
        // not is reported by the line.
        missing: None,
    });
    probes.push(ToolProbe {
        name: "cloudflared".to_string(),
        program: tunnel::cloudflared_program(paths).display().to_string(),
        args: "--version",
        detail_args: None,
        detail_label: "",
        needed_for: "`pando share`, which publishes a worktree at a public URL".to_string(),
        // `share` is opt-in and says this itself. A line is enough.
        missing: None,
    });

    for (program, needed_for, named_by_config) in project_programs(paths, config) {
        if probes.iter().any(|p| p.program == program) {
            continue;
        }
        probes.push(ToolProbe {
            name: program.clone(),
            program,
            args: "--version",
            detail_args: None,
            detail_label: "",
            needed_for: needed_for.clone(),
            missing: named_by_config.then_some((Severity::Problem, needed_for)),
        });
    }
    probes
}

/// Programs this project will have pando run, with whether config names
/// them.
///
/// Config naming one is the difference between a line and a problem: a
/// lockfile is a signal that a manager is *probably* wanted, an
/// `install =` is pando being told to run it.
fn project_programs(paths: &PandoPaths, config: &Config) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    if let Some(install) = config.project.install.as_deref()
        && let Some(program) = command_program(install)
    {
        out.push((
            program,
            "the install step every new worktree runs".to_string(),
            true,
        ));
    }
    for hook in &config.hooks {
        let Some(program) = command_program(&hook.cmd) else {
            continue;
        };
        if out.iter().any(|(p, _, _)| *p == program) {
            continue;
        }
        out.push((program, format!("the hook {:?}", hook.name), true));
    }
    let signals = detect::signals(paths.root());
    for lockfile in &signals.lockfiles {
        let Some(manager) = package_managers::for_lockfile(lockfile) else {
            continue;
        };
        let program = manager.program;
        if out.iter().any(|(p, _, _)| p == program) {
            continue;
        }
        out.push((
            program.to_string(),
            format!("{lockfile} is in this repository"),
            false,
        ));
    }
    out
}

/// The program a shell command really runs: the last `&&`-joined step's
/// first word that is not a `KEY=value` prefix.
///
/// The last step, because `corepack enable && pnpm install` is about pnpm.
pub(super) fn command_program(cmd: &str) -> Option<String> {
    let steps: Vec<&str> = cmd.split("&&").filter(|s| !s.trim().is_empty()).collect();
    let step = steps.last()?;
    let word = step.split_whitespace().find(|w| !w.contains('='))?;
    // A path, a template or a quoted word is not a program name worth
    // asking `command -v` about, and it is exactly the shape that would
    // put something surprising on a command line.
    let plain = word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    plain.then(|| word.to_string())
}

/// Asks one shell where every tool is and what it says it is.
///
/// One shell for all of them, and it is the *same* shell the runtime probe
/// and every spawn use — `bash -lc` in the main checkout, output captured,
/// with a deadline on it. A login shell reads a profile, so a dozen of
/// them is a dozen times the wait for one answer; and going through
/// `Machine` is what lets a test report on a laptop it does not have.
///
/// Returns what it learned plus, when the shell never got to the end, why:
/// "not found" is a claim about this machine, and a probe that did not run
/// has not made it.
fn probe_tools(
    shell: runtime::Shell<'_>,
    probes: &[ToolProbe],
    prelude: &str,
) -> (BTreeMap<usize, ToolFound>, Option<String>) {
    if probes.is_empty() {
        return (BTreeMap::new(), None);
    }
    let script = tool_script(probes, prelude);
    let Some(text) = shell(&script) else {
        return (
            BTreeMap::new(),
            Some("the shell did not answer inside its deadline".to_string()),
        );
    };
    let mut found: BTreeMap<usize, ToolFound> = BTreeMap::new();
    let mut done = false;
    for line in text.lines() {
        let line = line.trim();
        if line == TOOL_DONE_MARK {
            done = true;
            continue;
        }
        for (mark, field) in [
            (TOOL_PATH_MARK, 0u8),
            (TOOL_VERSION_MARK, 1),
            (TOOL_DETAIL_MARK, 2),
        ] {
            let Some(rest) = line.strip_prefix(mark) else {
                continue;
            };
            let Some((index, value)) = rest.split_once(' ') else {
                continue;
            };
            let Ok(index) = index.parse::<usize>() else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let entry = found.entry(index).or_default();
            match field {
                0 => entry.path = Some(value.to_string()),
                1 => entry.version = Some(value.to_string()),
                _ => entry.detail = Some(value.to_string()),
            }
        }
    }
    if done {
        return (found, None);
    }
    // The body never ran. With a prelude set that is the prelude's
    // failure, and it is the same one every spawn would hit.
    let last = text
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
        .unwrap_or("no output")
        .to_string();
    (
        found,
        Some(match prelude.is_empty() {
            true => format!("the probe did not finish — {last}"),
            false => format!("the prelude in front of it failed — {last}"),
        }),
    )
}

/// One shell script for every probe, composed the way a real spawn is.
pub(super) fn tool_script(probes: &[ToolProbe], prelude: &str) -> String {
    let mut body = String::new();
    for (index, probe) in probes.iter().enumerate() {
        let program = proc::shell_quote(&probe.program);
        let _ = write!(
            body,
            "if __pando_p=$(command -v {program} 2>/dev/null); then \
             printf '{TOOL_PATH_MARK}{index} %s\\n' \"$__pando_p\"; \
             printf '{TOOL_VERSION_MARK}{index} %s\\n' \
             \"$({program} {} 2>&1 | head -n 1)\"; ",
            probe.args
        );
        if let Some(detail) = probe.detail_args {
            let _ = write!(
                body,
                "printf '{TOOL_DETAIL_MARK}{index} %s\\n' \
                 \"$({program} {detail} 2>&1 | head -n 1)\"; "
            );
        }
        let _ = writeln!(body, "fi");
    }
    let _ = writeln!(body, "echo {TOOL_DONE_MARK}");
    match prelude {
        "" => body,
        prelude => format!("{prelude} && {{\n{body}}}"),
    }
}
