//! How the app of a process a phone, a tablet or a simulator runs is
//! opened: its links, from the settings, the worktree's record and the
//! app's own manifests.

use std::collections::BTreeMap;
use std::path::Path;

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
///
/// Recognised from the settings and the catalog alone, the way
/// `setup::device_note` recognises one: by the framework's port variable
/// or its server in the command.
pub fn app_links_with(
    config: &Config,
    record: &WorktreeRecord,
    read: ReadManifest<'_>,
) -> BTreeMap<String, AppLinks> {
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
            let port = observed_port_for_role(record, &role).unwrap_or(assigned);
            let dir = match &process.cwd {
                Some(cwd) => record.path.join(cwd),
                None => record.path.clone(),
            };
            Some((name.clone(), device.links(HOST, port, &read(device, &dir))))
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
