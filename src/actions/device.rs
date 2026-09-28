//! How the app of a process a phone, a tablet or a simulator runs is
//! opened: its links, from the settings and the worktree's record.

use std::collections::BTreeMap;

use crate::catalog::frameworks::{self, AppLinks};
use crate::config::Config;
use crate::state::WorktreeRecord;

use super::services::observed_port_for_role;

/// The address the links name. The iOS simulator shares this machine's;
/// a device's is the developer's network's, which the device note tells
/// them to supply, and pando never guesses.
const HOST: &str = "127.0.0.1";

/// The links that open each process's app, by process: every process the
/// settings run whose framework's app runs on a device, once the worktree
/// holds its port.
///
/// Recognised from the settings and the catalog alone, the way
/// `setup::device_note` recognises one: by the framework's port variable
/// or its server in the command. The app's own manifest is never read,
/// so a development build's scheme stays a placeholder.
pub fn app_links(config: &Config, record: &WorktreeRecord) -> BTreeMap<String, AppLinks> {
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
            Some((name.clone(), device.links(HOST, port)))
        })
        .collect()
}
