//! Processes that run a framework's server and were given no port.

use crate::catalog::frameworks::PortMechanism;
use crate::config::Config;
use crate::detect::{self, Slot};
use crate::paths::PandoPaths;

use super::config::value_repr;
use super::report::{Finding, Section};
use super::stale::answers_command;

/// A process that binds its framework's default port in every worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portless {
    /// The process, as config names it.
    pub process: String,
    /// What is wrong, naming the framework and its port.
    pub message: String,
    /// The one line that gives it a port of its own in each worktree.
    pub fix: String,
}

/// Every process with no `ports` at all whose command runs a framework
/// that takes its port from a variable or a flag.
///
/// Unset, not empty: `ports = []` is a process that has said it has none,
/// a worker or a watcher. One that has said nothing is given no port, and
/// its server listens on the framework's own — the same number in every
/// worktree, so the second worktree started finds it taken. Nothing asked
/// about it either: a process that is declared has answered the port
/// question by existing. The framework is recognised by the rules
/// detection proposes from, from the command or the `package.json` script
/// it runs in its directory.
pub fn portless_processes(paths: &PandoPaths, config: &Config) -> Vec<Portless> {
    let lone_dev = detect::fills_one_dev_process(config);
    let mut out = Vec::new();
    for (name, process) in config.runnable_processes() {
        if process.ports.is_some()
            || process.cmd.contains("{port")
            || !process.port_vars().is_empty()
        {
            continue;
        }
        let dir = match process.cwd.as_deref() {
            Some(cwd) => paths.root().join(cwd),
            None => paths.root().to_path_buf(),
        };
        let Some(served) = detect::served_by(&dir, &process.cmd) else {
            continue;
        };
        let rule = served.rule;
        // A rule no file marks names a runtime, not a server: `node
        // worker.js` is a worker as often as it is an app, and a worker
        // with no port is right.
        if rule.markers.is_empty() {
            continue;
        }
        // The role a single port variable owns, as an answer to the port
        // question gives it: the device's, else the browser's — unless
        // another process owns that one already.
        let wanted = rule
            .device
            .map(|device| device.role)
            .unwrap_or(crate::config::WEB_ROLE);
        let taken = config
            .processes
            .iter()
            .any(|(other, p)| other != name && p.roles().iter().any(|role| role == wanted));
        let role = match taken {
            true => name.as_str(),
            false => wanted,
        };
        let table = format!("[processes.{name}] in {}", paths.config_file().display());
        let fix = match (rule.port, rule.port_flag) {
            (PortMechanism::Env(var), _) if lone_dev => format!(
                "`{}` gives it a port of its own in each worktree",
                answers_command(Slot::PortEnv, serde_json::Value::String(var.to_string()))
            ),
            (PortMechanism::Env(var), _) => {
                format!("add `ports = {{ {var} = \"{role}\" }}` to {table}")
            }
            (PortMechanism::InCommand, Some(flag)) => {
                let flag = flag.replace("{port}", &format!("{{port:{role}}}"));
                // npm's `-- ` once: a command that hands the script
                // arguments already has it, and a second would reach the
                // server as an argument of its own.
                let separated = process.cmd.split_whitespace().any(|word| word == "--");
                let cmd = match served.script {
                    Some(_) if !separated => {
                        format!("{} {}{flag}", process.cmd.trim(), served.script_args)
                    }
                    _ => format!("{} {flag}", process.cmd.trim()),
                };
                match lone_dev {
                    true => format!(
                        "`{}` gives it a port of its own in each worktree",
                        answers_command(Slot::DevCmd, serde_json::Value::String(cmd))
                    ),
                    false => format!(
                        "set `cmd = {}` and `ports = [\"{role}\"]` in {table}",
                        value_repr(&toml_edit::Value::from(cmd))
                    ),
                }
            }
            // A port nobody can hand it: Django's is a positional
            // argument, and a rule that asks has nothing to say here.
            _ => continue,
        };
        let through = match &served.script {
            Some(script) => format!(" through package.json's {script:?} script"),
            None => String::new(),
        };
        out.push(Portless {
            process: name.clone(),
            message: format!(
                "process {name:?} runs {}{through} and has no `ports`, so it listens on {}'s own \
                 {} in every worktree — two worktrees running at once clash on it",
                rule.name, rule.name, rule.default_port
            ),
            fix,
        });
    }
    out
}

/// [`portless_processes`], as problems: a second worktree's start fails
/// on the port the first one holds.
pub(super) fn portless_findings(paths: &PandoPaths, config: &Config, findings: &mut Vec<Finding>) {
    for portless in portless_processes(paths, config) {
        findings.push(Finding::problem(
            Section::Config,
            portless.message,
            portless.fix,
        ));
    }
}
