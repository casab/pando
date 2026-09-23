//! Workspaces: the apps of a monorepo, each with its own dev script.

use std::collections::BTreeMap;
use std::path::Path;

use crate::catalog::frameworks;
use crate::catalog::frameworks::{FrameworkRule, PortMechanism};
use crate::config::{PortsSpec, ProcessConfig, ReadySpec};

use super::apply::DEV;
use super::dev::{is_multiplexer, is_production, script_runner};
use super::frameworks::framework;
use super::proposal::{Candidate, Proposal, Slot};
use super::signals::{Signals, parse_scripts, present};

/// One app of a workspace: a directory with its own manifest and its own
/// dev script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceApp {
    /// The directory's own name, which is the process's name and its role.
    pub name: String,
    /// Relative to the repository root, for example `apps/web`.
    pub dir: String,
    /// The script pando would run, already prefixed with the runner.
    pub cmd: String,
    /// The port this app listens on when nobody tells it otherwise. What
    /// makes a localhost URL in the env example resolvable to *this* app.
    pub default_port: Option<u16>,
    /// How this app takes a port, from its own framework rule.
    pub port: PortMechanism,
}

/// Fewer than this is not a workspace worth splitting up: one app with a
/// dev script is the single-process case pando already handles.
const MIN_WORKSPACE_APPS: usize = 2;

/// Where a workspace says its packages live.
///
/// pnpm keeps them in `pnpm-workspace.yaml`, npm, yarn and bun in the root
/// manifest. turbo and nx describe pipelines rather than membership, so
/// when one of those is the only marker the two conventional directories
/// are tried. Hand-parsed on purpose: one list of globs is not worth a YAML
/// dependency, and anything this cannot read simply is not a signal.
pub(super) fn workspace_globs(root: &Path) -> Vec<String> {
    let mut globs = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        globs.extend(yaml_string_list(&text, "packages"));
    }
    let manifest = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) {
        let workspaces = value.get("workspaces");
        let list = match workspaces {
            // Both shapes yarn and npm accept.
            Some(serde_json::Value::Array(list)) => Some(list.clone()),
            Some(serde_json::Value::Object(map)) => match map.get("packages") {
                Some(serde_json::Value::Array(list)) => Some(list.clone()),
                _ => None,
            },
            _ => None,
        };
        if let Some(list) = list {
            globs.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_string));
        }
    }
    if globs.is_empty()
        && ["turbo.json", "nx.json"]
            .iter()
            .any(|f| root.join(f).exists())
    {
        globs.push("apps/*".to_string());
        globs.push("packages/*".to_string());
    }
    globs.sort();
    globs.dedup();
    globs
}

/// The string items of a top-level YAML list, for one key. Enough for
/// `packages:` followed by `  - 'apps/*'` lines, and nothing more.
fn yaml_string_list(text: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with('#') || trimmed.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            inside = trimmed.trim_end_matches(':') == key && trimmed.ends_with(':');
            continue;
        }
        if !inside {
            continue;
        }
        let Some(item) = trimmed.trim_start().strip_prefix('-') else {
            continue;
        };
        let item = item.trim().trim_matches(['"', '\'']).to_string();
        if !item.is_empty() {
            out.push(item);
        }
    }
    out
}

/// The directories a workspace glob names.
///
/// Only a trailing `*` is expanded, which is the shape every workspace
/// glob in the wild has (`apps/*`, `packages/*`); anything else is treated
/// as a literal directory. A glob pando cannot read finds nothing, which
/// means one fewer proposal rather than a wrong one.
fn expand_glob(root: &Path, glob: &str) -> Vec<String> {
    let glob = glob.trim_end_matches('/');
    let Some(prefix) = glob.strip_suffix("/*") else {
        if glob.contains('*') || !root.join(glob).is_dir() {
            return Vec::new();
        }
        return vec![glob.to_string()];
    };
    if prefix.contains('*') {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root.join(prefix)) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some(format!("{prefix}/{}", e.file_name().to_str()?)))
        .collect();
    out.sort();
    out
}

/// Signals of one app directory: enough for its framework rule, and no
/// more. A full read would shell out to git once per app for files nothing
/// here looks at.
fn app_signals(dir: &Path) -> Signals {
    let manifest = std::fs::read_to_string(dir.join("package.json")).unwrap_or_default();
    Signals {
        scripts: parse_scripts(&manifest),
        markers: present(dir, &frameworks::marker_files()),
        ..Default::default()
    }
}

/// Every app of the workspace that has a dev script of its own.
///
/// The name is the directory's, which is also the role its port is
/// reserved under — so two apps with the same directory name would claim
/// one role, and `config::validate` would refuse the file pando had just
/// written. That is a workspace pando says nothing about.
pub fn workspace_apps(root: &Path, signals: &Signals) -> Vec<WorkspaceApp> {
    let runner = script_runner(signals);
    let mut apps: Vec<WorkspaceApp> = Vec::new();
    for glob in workspace_globs(root) {
        for dir in expand_glob(root, &glob) {
            let path = root.join(&dir);
            let app = app_signals(&path);
            let Some(script) = app.scripts.get("dev") else {
                continue;
            };
            if is_production(script) || is_multiplexer(script) {
                continue;
            }
            let Some(name) = Path::new(&dir).file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let rule = framework(&path, &app);
            let mut cmd = format!("{runner}dev");
            // A framework that takes its port on the command line gets the
            // flag appended to its own script: `pnpm dev -- --port 1234`
            // runs what the app already runs, on the port pando chose.
            if let Some(rule) = rule
                && rule.port == PortMechanism::InCommand
                && let Some(flag) = rule.port_flag
            {
                cmd = format!(
                    "{cmd} -- {}",
                    flag.replace("{port}", &format!("{{port:{name}}}"))
                );
            }
            apps.push(WorkspaceApp {
                default_port: app_default_port(signals, name, rule),
                port: match rule {
                    Some(rule)
                        if rule.port == PortMechanism::InCommand && rule.port_flag.is_none() =>
                    {
                        PortMechanism::Ask
                    }
                    Some(rule) => rule.port,
                    None => PortMechanism::Ask,
                },
                name: name.to_string(),
                dir,
                cmd,
            });
        }
    }
    apps.sort_by(|a, b| a.dir.cmp(&b.dir));
    apps.dedup_by(|a, b| a.dir == b.dir);
    let mut names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable();
    let unique = names.len();
    names.dedup();
    if names.len() != unique {
        // Two apps with the same directory name would claim the same role.
        return Vec::new();
    }
    apps
}

/// The port an app listens on by default: what the root env example says
/// for it, else what its framework does.
fn app_default_port(
    signals: &Signals,
    name: &str,
    rule: Option<&'static FrameworkRule>,
) -> Option<u16> {
    let wanted = format!("{}_PORT", name.to_uppercase().replace('-', "_"));
    let from_example = signals
        .env_example
        .iter()
        .find(|(key, _)| *key == wanted)
        .and_then(|(_, value)| value.parse::<u16>().ok());
    from_example.or_else(|| rule.map(|r| r.default_port))
}

/// The env variables an app reads its port from: a `<APP>_PORT` key the
/// env example declares for it, and its framework's own variable.
///
/// The env example first, as the single-process rule has it — a project
/// that wrote `WEB_PORT=3000` beside its web app has said how that app
/// takes its port, and handing it `PORT` alone ignores that. Both are
/// given when both exist: they carry the same port, so there is nothing
/// for them to disagree about, and an app that reads either one works.
fn app_port_env(signals: &Signals, app: &WorkspaceApp) -> Vec<String> {
    let wanted = format!("{}_PORT", app.name.to_uppercase().replace('-', "_"));
    let declared = signals.env_keys().any(|key| key == wanted);
    let mut out: Vec<String> = Vec::new();
    match app.port {
        // The command already carries the port. A second way of saying it
        // is a second thing that can disagree.
        PortMechanism::InCommand => return out,
        PortMechanism::Env(name) => {
            if declared && wanted != name {
                out.push(wanted);
            }
            out.push(name.to_string());
        }
        PortMechanism::Ask => {
            if declared {
                out.push(wanted);
            }
        }
    }
    out
}

/// Cross-references between apps, read out of the root env example.
///
/// A value like `http://localhost:4000` is one app being told where
/// another one listens. When that port is another app's default, the value
/// becomes a template pointing at that app's role — so every worktree gets
/// its own pair of ports and the two halves still find each other. It is
/// given to every process *except* the one it points at, which is the only
/// one that does not need to be told.
///
/// Only an app that owns a role can be pointed at: `{port:<role>}` for a
/// role nobody owns does not render, and an environment that cannot be
/// rendered is a start that refuses.
fn cross_references(
    signals: &Signals,
    apps: &[WorkspaceApp],
    owns_role: &[bool],
) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (key, value) in &signals.env_example {
        let Some(port) = localhost_url_port(value) else {
            continue;
        };
        let Some(target) = apps
            .iter()
            .zip(owns_role)
            .find(|(app, owns)| **owns && app.default_port == Some(port))
            .map(|(app, _)| app)
        else {
            continue;
        };
        let template = value.replacen(
            &format!(":{port}"),
            &format!(":{{port:{}}}", target.name),
            1,
        );
        out.push((target.name.clone(), key.clone(), template));
    }
    out
}

/// The port of a URL that points at this machine, if that is what this is.
pub(super) fn localhost_url_port(value: &str) -> Option<u16> {
    let (_, after) = value.split_once("://")?;
    let host_port = after.split(['/', '?', '#']).next()?;
    // Credentials, as in `postgres://user:pass@localhost:5432/db`.
    let host_port = host_port.rsplit('@').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    if !matches!(host, "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]") {
        return None;
    }
    port.parse().ok()
}

/// The multi-process form, for a workspace whose apps each have a dev
/// script — and the root script as the one-process fallback beside it.
///
/// Always a question: running two servers instead of one changes what
/// `start`, `stop` and the log tabs do, and that is the developer's call.
/// `--yes` takes the first option, which is the per-app form.
pub(super) fn processes_proposal(root: &Path, signals: &Signals) -> Option<Proposal> {
    let apps = workspace_apps(root, signals);
    if apps.len() < MIN_WORKSPACE_APPS {
        return None;
    }
    // A role is a port, and a port is only worth giving an app that has
    // some way of being told which one it got: its framework reads one
    // from the environment, or takes it on the command line (the flag is
    // already in `cmd` by now), or the env example has an `<APP>_PORT` key
    // for it. An app with none of those — a watcher, a codegen step, a
    // queue consumer, the `dev: "tsc -w"` of a `packages/*` library — gets
    // `ports = []` and no readiness rule. Given a role anyway it would be
    // handed a reserved port it never hears about, and `advance_phases`
    // would wait thirty seconds for it to bind before calling a perfectly
    // healthy process failed — and the whole worktree with it.
    let port_envs: Vec<Vec<String>> = apps.iter().map(|app| app_port_env(signals, app)).collect();
    let owns_role: Vec<bool> = apps
        .iter()
        .zip(&port_envs)
        .map(|(app, port_env)| app.port == PortMechanism::InCommand || !port_env.is_empty())
        .collect();
    let references = cross_references(signals, &apps, &owns_role);
    let mut processes: BTreeMap<String, ProcessConfig> = BTreeMap::new();
    for ((app, port_env), owns_role) in apps.iter().zip(&port_envs).zip(&owns_role) {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        for var in port_env {
            env.insert(var.clone(), format!("{{port:{}}}", app.name));
        }
        for (target, key, template) in &references {
            // The app a reference points at is the one that does not need
            // to be told where it is.
            if *target == app.name {
                continue;
            }
            env.insert(key.clone(), template.clone());
        }
        let roles = if *owns_role {
            vec![app.name.clone()]
        } else {
            Vec::new()
        };
        processes.insert(
            app.name.clone(),
            ProcessConfig {
                cmd: app.cmd.clone(),
                cwd: Some(app.dir.clone()),
                ports: Some(PortsSpec::List(roles)),
                env,
                ready: owns_role.then(|| ReadySpec {
                    role: Some(app.name.clone()),
                    timeout_s: None,
                }),
            },
        );
    }
    let summary = apps
        .iter()
        .map(|app| format!("{}: {} in {}", app.name, app.cmd, app.dir))
        .collect::<Vec<_>>()
        .join("; ");
    // The env example's own port variables, named in the evidence, so the
    // question says where they came from.
    let declared: Vec<String> = apps
        .iter()
        .map(|app| format!("{}_PORT", app.name.to_uppercase().replace('-', "_")))
        .filter(|key| signals.env_keys().any(|k| k == key))
        .collect();
    let mut why = format!("a dev script in each of {} workspace apps", apps.len());
    if !declared.is_empty() {
        why.push_str(&format!("; {} in the env example", declared.join(" and ")));
    }
    let mut candidates = vec![Candidate {
        value: summary,
        why,
        processes: Some(processes),
        ..Candidate::default()
    }];
    // The fallback: whatever the root script is, as it was before. Written
    // as `[dev]`, because one process is what that shorthand is for.
    if let Some(script) = signals.scripts.get("dev") {
        let _ = script;
        let value = format!("{}dev", script_runner(signals));
        candidates.push(Candidate {
            value: value.clone(),
            why: "package.json scripts.dev".to_string(),
            processes: Some(BTreeMap::from([(
                DEV.to_string(),
                ProcessConfig {
                    cmd: value,
                    ..Default::default()
                },
            )])),
            ..Candidate::default()
        });
    }
    // Never decided: two processes instead of one is a change of shape,
    // and "ask just in time, once" is exactly what this is for.
    Some(Proposal::of(Slot::Processes, candidates, false))
}
