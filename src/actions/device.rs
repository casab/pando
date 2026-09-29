//! How the app of a process a phone, a tablet or a simulator runs is
//! opened: its links, from the settings, the worktree's record and the
//! app's own manifests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalog::frameworks::{self, AppLinks, AppManifest, Device};
use crate::config::Config;
use crate::state::WorktreeRecord;

use super::services::observed_port_for_role;

/// The address the links name. The iOS simulator shares this machine's;
/// a device's is the developer's network's, which the device note tells
/// them to supply, and pando never guesses.
const HOST: &str = "127.0.0.1";

/// How a caller reads an app's manifests: from disk, or from what it
/// read before.
pub type ReadManifest<'a> = &'a dyn Fn(&Device, &Path) -> AppManifest;

/// The links that open each process's app, by process: every process the
/// settings run whose framework's app runs on a device, once the worktree
/// holds its port, with the app's manifests read from the worktree.
pub fn app_links(config: &Config, record: &WorktreeRecord) -> BTreeMap<String, AppLinks> {
    app_links_with(config, record, &read_manifest)
}

/// [`app_links`], with the manifests read by `read`: the TUI's paint reads
/// no file, and hands in what it read once.
pub fn app_links_with(
    config: &Config,
    record: &WorktreeRecord,
    read: ReadManifest<'_>,
) -> BTreeMap<String, AppLinks> {
    device_processes(config, record)
        .into_iter()
        .map(|app| {
            let links = app
                .device
                .links(HOST, app.port, &read(app.device, &app.dir));
            (app.name.to_string(), links)
        })
        .collect()
}

/// What a branch changes of the native code a development build compiles
/// in, for one process's app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeChanges {
    /// What the branch was measured against.
    pub base: String,
    /// The files, relative to the worktree, changed since the branch left
    /// `base`: committed, uncommitted, or new.
    pub changed: Vec<String>,
    /// The command, run in the app's directory, that makes this worktree
    /// a development build of its own on the simulator.
    pub build: String,
}

/// Each device app's native code the worktree's branch changes against
/// `base`, by process, for the processes whose app has any. A build of
/// another branch lacks what these add, and the bundle this worktree's
/// bundler serves reaches for it.
///
/// Empty when git cannot say: a base the worktree does not have, say.
pub fn native_changes(
    config: &Config,
    record: &WorktreeRecord,
    base: &str,
) -> BTreeMap<String, NativeChanges> {
    let apps = device_processes(config, record);
    if apps.is_empty() {
        return BTreeMap::new();
    }
    let Some(changed) = changed_since(&record.path, base) else {
        return BTreeMap::new();
    };
    apps.into_iter()
        .filter_map(|app| {
            let native: Vec<String> = changed
                .iter()
                .filter(|path| {
                    let within = match app.cwd {
                        Some(cwd) => path.strip_prefix(cwd).and_then(|p| p.strip_prefix('/')),
                        None => Some(path.as_str()),
                    };
                    within.is_some_and(|rel| app.device.native.is_native(rel))
                })
                .cloned()
                .collect();
            (!native.is_empty()).then(|| {
                let changes = NativeChanges {
                    base: base.to_string(),
                    changed: native,
                    build: app
                        .device
                        .native
                        .build
                        .replace("{port}", &app.port.to_string()),
                };
                (app.name.to_string(), changes)
            })
        })
        .collect()
}

/// Every file of the worktree at `dir` that differs from where its branch
/// left `base`: committed or not, and new files git does not ignore.
fn changed_since(dir: &Path, base: &str) -> Option<Vec<String>> {
    let git = |args: &[&str]| -> Option<String> {
        let out = crate::project::git(dir, args).ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let fork = git(&["merge-base", base, "HEAD"])?;
    let fork = fork.trim();
    let mut changed: Vec<String> = git(&["diff", "--name-only", "-z", fork, "--"])?
        .split('\0')
        .chain(git(&["ls-files", "--others", "--exclude-standard", "-z"])?.split('\0'))
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();
    changed.sort();
    changed.dedup();
    Some(changed)
}

/// A process whose app a device runs, as the worktree holds it.
struct DeviceProcess<'a> {
    name: &'a str,
    device: &'static Device,
    /// Its bundler's port: the one it listens on, else the one assigned.
    port: u16,
    /// Its directory, as config names it relative to the worktree.
    cwd: Option<&'a str>,
    /// And where that is.
    dir: PathBuf,
}

/// Every process the settings run whose framework's app runs on a device,
/// once the worktree holds its port.
///
/// Recognised from the settings and the catalog alone, the way
/// `setup::device_note` recognises one: by the framework's port variable
/// or its server in the command.
fn device_processes<'a>(config: &'a Config, record: &WorktreeRecord) -> Vec<DeviceProcess<'a>> {
    config
        .runnable_processes()
        .filter_map(|(name, process)| {
            let vars = process.port_vars();
            let names: Vec<&str> = vars.keys().map(String::as_str).collect();
            let (device, var) = frameworks::device(&names, &process.cmd)?;
            // The role whose port the framework's variable holds, else the
            // process's one role: a command that says `--port {port:app}`.
            let role = match vars.get(var) {
                Some(Some(role)) => role.clone(),
                _ => {
                    let owned = record
                        .roles
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| process.roles());
                    let [role] = owned.as_slice() else {
                        return None;
                    };
                    role.clone()
                }
            };
            let assigned = *record.ports.get(&role)?;
            let cwd = process
                .cwd
                .as_deref()
                .map(|cwd| cwd.trim_start_matches("./").trim_end_matches('/'))
                .filter(|cwd| !cwd.is_empty() && *cwd != ".");
            Some(DeviceProcess {
                name,
                device,
                port: observed_port_for_role(record, &role).unwrap_or(assigned),
                dir: cwd.map_or_else(|| record.path.clone(), |cwd| record.path.join(cwd)),
                cwd,
            })
        })
        .collect()
}

/// What the app in `dir` says about opening it: the scheme its
/// development build registers, from the name in its manifest, and
/// whether it depends on the development client.
///
/// A manifest that is missing or does not parse says nothing, and the
/// links keep the scheme's placeholder: an `app.config.ts` is code, and
/// pando does not run it to read a name.
pub fn read_manifest(device: &Device, dir: &Path) -> AppManifest {
    let json = |file: &str| -> Option<serde_json::Value> {
        serde_json::from_str(&std::fs::read_to_string(dir.join(file)).ok()?).ok()
    };
    let scheme = json(device.scheme.manifest).and_then(|manifest| {
        let name = device
            .scheme
            .key
            .iter()
            .try_fold(&manifest, |value, key| value.get(key))?;
        device.scheme.of(name.as_str()?)
    });
    let development_client = json("package.json").is_some_and(|package| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|table| package[table].get(device.development_client).is_some())
    });
    AppManifest {
        scheme,
        development_client,
    }
}
