//! What `docker compose config` says, which is compose's own resolution
//! of the file and so beats this module's reading of it.

use super::ComposeFile;
use super::Mount;
use super::Port;
use super::Service;
use super::TopVolume;
use super::yaml::first_of_range;
use anyhow::{Context, Result};

// ---- what compose itself says ---------------------------------------------

/// `docker compose config --format json`, read into the same shape the
/// hand-rolled parser produces.
///
/// Compose has already followed `extends:` and a top-level `include:` here,
/// so nothing is left unresolved — and it has normalised everything else:
/// `published` is a string, `depends_on` is a map, a relative bind source
/// is an absolute path, and *every* volume carries a `name`. A plain
/// volume's name is `<project>_<key>`, which is the project-name prefix
/// pando relies on, so only a name that is something else is pinned.
pub fn parse_config_json(text: &str) -> Result<ComposeFile> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("parse `docker compose config --format json`")?;
    let project = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let mut file = ComposeFile::default();
    if let Some(services) = value.get("services").and_then(|v| v.as_object()) {
        for (name, body) in services {
            file.services.insert(name.clone(), service_from_json(body));
        }
    }
    if let Some(volumes) = value.get("volumes").and_then(|v| v.as_object()) {
        for (name, body) in volumes {
            file.volumes
                .insert(name.clone(), top_volume_from_json(project, name, body));
        }
    }
    Ok(file)
}

fn service_from_json(value: &serde_json::Value) -> Service {
    let string = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let mut out = Service {
        image: string("image"),
        // Always the mapping form here, and always an absolute path:
        // compose has resolved the context against the file's directory.
        build: value
            .get("build")
            .and_then(|build| build.get("context"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        container_name: string("container_name"),
        healthcheck: value.get("healthcheck").is_some_and(declares_healthcheck),
        ..Service::default()
    };
    if let Some(ports) = value.get("ports").and_then(|v| v.as_array()) {
        for entry in ports {
            let Some(container) = entry.get("target").and_then(|v| v.as_u64()) else {
                continue;
            };
            out.ports.push(Port {
                container: container as u16,
                // A string here, and possibly a range: compose renders
                // `9000-9001:6379-6380` as `"9000-9001"`.
                published: entry
                    .get("published")
                    .and_then(|v| v.as_str())
                    .and_then(first_of_range),
                host: entry
                    .get("host_ip")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            });
        }
    }
    if let Some(volumes) = value.get("volumes").and_then(|v| v.as_array()) {
        for entry in volumes {
            let source = entry.get("source").and_then(|v| v.as_str());
            match (entry.get("type").and_then(|v| v.as_str()), source) {
                (Some("bind"), Some(source)) => out.volumes.push(Mount::Bind(source.to_string())),
                (Some("volume"), Some(source)) => {
                    out.volumes.push(Mount::Named(source.to_string()))
                }
                (Some("volume"), None) => out.volumes.push(Mount::Anonymous),
                // tmpfs, npipe, cluster: nothing on the host to isolate.
                _ => {}
            }
        }
    }
    // Always the map form once compose has normalised it.
    if let Some(depends) = value.get("depends_on").and_then(|v| v.as_object()) {
        out.depends_on = depends.keys().cloned().collect();
    }
    out
}

/// Whether a `healthcheck` leaves docker a check to run. Compose keeps
/// `{"disable": true}` and `{"test": ["NONE"]}` as they were written, and
/// docker reports no health for either.
fn declares_healthcheck(value: &serde_json::Value) -> bool {
    value.is_object()
        && value.get("disable").and_then(|v| v.as_bool()) != Some(true)
        && value
            .get("test")
            .and_then(|test| test.get(0))
            .and_then(|v| v.as_str())
            != Some("NONE")
}

fn top_volume_from_json(project: &str, key: &str, value: &serde_json::Value) -> TopVolume {
    let external = value
        .get("external")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let name = value.get("name").and_then(|v| v.as_str());
    // `<project>_<key>` is the prefix compose applies on its own, which is
    // exactly what isolates the data per worktree. Anything else is a name
    // the project pinned, and every worktree would share it.
    let pinned = name.filter(|name| *name != format!("{project}_{key}"));
    let driver_opts = value
        .get("driver_opts")
        .and_then(|v| v.as_object())
        .map(|opts| {
            opts.iter()
                .map(|(key, value)| {
                    let value = match value.as_str() {
                        Some(text) => text.to_string(),
                        None => value.to_string(),
                    };
                    (key.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    TopVolume {
        name: pinned.map(str::to_string),
        external,
        driver: value
            .get("driver")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        driver_opts,
    }
}
