//! The development builds installed on the booted iOS simulators: which
//! one a worktree's app opens in, and whether it was built for the SDK
//! the worktree's JavaScript needs. Read from the simulators' disks;
//! nothing is booted, launched or installed, and no project code runs.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::catalog::frameworks::{self, Device};
use crate::config::Config;
use crate::paths::PandoPaths;
use crate::state::{Phase, WorktreeRecord};

use super::device::{config_code, config_value, device_processes};

/// How long `xcrun simctl` may take to list the booted simulators.
/// CoreSimulator can stall on its first use after a login, and `status`
/// is not worth waiting on for it.
const SIMCTL_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a simulator keeps its installed apps, below its data directory:
/// one directory per install, the `<Name>.app` bundle inside it.
const INSTALLED_APPS: &str = "Containers/Bundle/Application";

/// One app bundle on a booted simulator that holds a device framework's
/// config.
#[derive(Debug, Clone)]
struct SimulatorApp {
    /// The simulator's name: `iPhone 17 Pro`.
    device: String,
    /// The framework's [`Installed::config`] the bundle holds, which says
    /// whose it is.
    ///
    /// [`Installed::config`]: frameworks::Installed::config
    holds: &'static str,
    /// That config, as the build was made with it.
    config: Value,
}

impl SimulatorApp {
    /// The string at `key`, below the config's root.
    fn value(&self, key: &[&str]) -> Option<&str> {
        key.iter()
            .try_fold(&self.config, |value, key| value.get(key))?
            .as_str()
    }

    /// What tells it apart from another app of `device`'s framework: its
    /// name and its bundle id.
    fn identity(&self, device: &Device) -> (Option<&str>, Option<&str>) {
        (
            self.value(device.scheme.key),
            self.value(device.installed.bundle_id),
        )
    }
}

/// The apps on the booted simulators, read at most once, and only when
/// something asks: one `status` looks once for every worktree it lists.
pub struct Simulators<'a> {
    paths: &'a PandoPaths,
    timeout: Duration,
    apps: OnceCell<Vec<SimulatorApp>>,
}

impl<'a> Simulators<'a> {
    pub fn new(paths: &'a PandoPaths) -> Self {
        Self::within(paths, SIMCTL_TIMEOUT)
    }

    /// With xcrun given `timeout` rather than [`SIMCTL_TIMEOUT`]: a test
    /// waits out a stalled one in a moment.
    #[doc(hidden)]
    pub fn within(paths: &'a PandoPaths, timeout: Duration) -> Self {
        Self {
            paths,
            timeout,
            apps: OnceCell::new(),
        }
    }

    fn apps(&self) -> &[SimulatorApp] {
        self.apps
            .get_or_init(|| simulator_apps(self.paths, self.timeout))
    }
}

/// A development build of a worktree's app, installed on a booted
/// simulator: the build its links open there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBuild {
    /// The simulator it is installed on, by name.
    pub device: String,
    /// The major version of the SDK it was built with, where its config
    /// says.
    pub sdk: Option<u32>,
    /// The major version the worktree's JavaScript needs, where the
    /// worktree says.
    pub expected_sdk: Option<u32>,
    /// The scheme it registers, where its config names the app.
    pub scheme: Option<String>,
    /// The command, run in the app's directory, that builds and installs
    /// the worktree's own over it, pointed at its running bundler.
    pub build: String,
}

impl InstalledBuild {
    /// Whether it was built for another SDK than the worktree's
    /// JavaScript needs, so the bundle calls into native code it lacks.
    /// Only where both say.
    pub fn stale(&self) -> bool {
        matches!((self.sdk, self.expected_sdk), (Some(built), Some(needed)) if built != needed)
    }

    /// What `status` says of a [stale](InstalledBuild::stale) build, and
    /// why `open` does not open the app in it: the SDKs, and the command
    /// that replaces it.
    pub fn mismatch(&self) -> Option<String> {
        let (Some(sdk), Some(needed)) = (self.sdk, self.expected_sdk) else {
            return None;
        };
        (sdk != needed).then(|| {
            format!(
                "the development build on {} is SDK {sdk} and this worktree needs SDK {needed} \
                 — `{}` in the app's directory builds its own",
                self.device, self.build
            )
        })
    }
}

/// The device apps `open` is about to open, as their development builds
/// on the booted simulators leave them: each one's links, with the scheme
/// a build installed for it fills in where the app's own config does not
/// say it, and, apart, each app not to open, with why: its build there
/// was made for another SDK, and would crash on this worktree's
/// JavaScript.
///
/// The simulators are looked at once, for all of them; a machine with
/// none booted, or no xcrun, leaves the links as they were.
pub fn openable_apps(
    paths: &PandoPaths,
    config: &Config,
    record: &WorktreeRecord,
    apps: Vec<(String, frameworks::AppLinks)>,
) -> (Vec<(String, frameworks::AppLinks)>, Vec<String>) {
    let installed = installed_builds(config, record, &Simulators::new(paths));
    if installed.is_empty() {
        return (apps, Vec::new());
    }
    let mut filled = super::device::app_links_installed(config, record, &installed);
    let mut refused = Vec::new();
    let apps = apps
        .into_iter()
        .filter_map(|(process, links)| {
            if let Some(why) = installed.get(&process).and_then(InstalledBuild::mismatch) {
                refused.push(format!("{process}: {why}"));
                return None;
            }
            let links = filled.remove(&process).unwrap_or(links);
            Some((process, links))
        })
        .collect();
    (apps, refused)
}

/// Each running device app's development build on a booted simulator, by
/// process, for the processes one is found for.
///
/// A build is the app's when its name or its bundle id is the one the
/// app's own config gives, or, where that config gives neither as a
/// literal, when the build's name or bundle id is a quoted string in the
/// config's source. Where several builds are the app's, one not known to
/// be made for another SDK is the one taken: a stale build beside a
/// current one of the same app is no reason to warn.
///
/// The simulators are looked at only for a worktree with a device app
/// running, and only once however many ask.
pub fn installed_builds(
    config: &Config,
    record: &WorktreeRecord,
    simulators: &Simulators<'_>,
) -> BTreeMap<String, InstalledBuild> {
    device_processes(config, record)
        .into_iter()
        .filter(|app| {
            record
                .processes
                .get(app.name)
                .is_some_and(|p| matches!(p.phase, Phase::Running { .. }))
        })
        .filter_map(|app| {
            let device = app.device;
            let expected_sdk = expected_sdk(device, &app.dir, &record.path);
            let builds: Vec<InstalledBuild> = builds_of(device, &app.dir, simulators.apps())
                .into_iter()
                .map(|found| InstalledBuild {
                    device: found.device.clone(),
                    sdk: found.value(device.installed.sdk).and_then(major),
                    expected_sdk,
                    scheme: found
                        .value(device.scheme.key)
                        .and_then(|name| device.scheme.of(name)),
                    build: device
                        .native
                        .simulator_build()
                        .replace("{port}", &app.port.to_string()),
                })
                .collect();
            let chosen = builds
                .iter()
                .find(|build| !build.stale())
                .or(builds.first())?
                .clone();
            Some((app.name.to_string(), chosen))
        })
        .collect()
}

/// The builds among `apps` that are the app in `dir`'s.
fn builds_of<'s>(device: &Device, dir: &Path, apps: &'s [SimulatorApp]) -> Vec<&'s SimulatorApp> {
    let ours = apps
        .iter()
        .filter(|app| app.holds == device.installed.config);
    let name = config_value(device, dir, device.scheme.key);
    let bundle_id = config_value(device, dir, device.installed.bundle_id);
    if name.is_some() || bundle_id.is_some() {
        let same = |theirs: Option<&str>, own: &Option<String>| {
            theirs.is_some() && theirs == own.as_deref()
        };
        return ours
            .filter(|app| {
                let (their_name, their_id) = app.identity(device);
                same(their_name, &name) || same(their_id, &bundle_id)
            })
            .collect();
    }
    // A config that computes both: a build whose name or bundle id it
    // quotes somewhere, if that is one app, however many simulators have
    // it.
    let code = config_code(device, dir);
    // An empty name is quoted by every config that has an empty string.
    let quoted = |text: &str| {
        !text.is_empty()
            && code.iter().any(|source| {
                ['"', '\'', '`']
                    .iter()
                    .any(|q| source.contains(&format!("{q}{text}{q}")))
            })
    };
    let found: Vec<&SimulatorApp> = ours
        .filter(|app| {
            let (their_name, their_id) = app.identity(device);
            their_name.into_iter().chain(their_id).any(quoted)
        })
        .collect();
    let one_app = found
        .iter()
        .all(|app| found[0].identity(device) == app.identity(device));
    match one_app {
        true => found,
        false => Vec::new(),
    }
}

/// The major version of the SDK the app in `dir` needs: the one of its
/// SDK package installed in `node_modules`, there or in a directory above
/// it within the worktree at `root`, else the one its `package.json`
/// asks for.
fn expected_sdk(device: &Device, dir: &Path, root: &Path) -> Option<u32> {
    let json = |path: PathBuf| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let package = device.installed.sdk_package;
    let installed = dir
        .ancestors()
        .take_while(|at| at.starts_with(root))
        .find_map(|at| {
            let manifest = json(at.join("node_modules").join(package).join("package.json"))?;
            manifest["version"].as_str().and_then(major)
        });
    installed.or_else(|| {
        let manifest = json(dir.join("package.json"))?;
        ["dependencies", "devDependencies"]
            .iter()
            .find_map(|table| manifest[table][package].as_str())
            .and_then(major)
    })
}

/// The major version in a version or a plain range: `55.0.0`, `~57.0.0`,
/// `^57`, `>=57.1`. `None` for a tag, a path or a protocol.
fn major(version: &str) -> Option<u32> {
    let digits: String = version
        .trim()
        .trim_start_matches(['^', '~', '>', '=', 'v', ' '])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The xcrun pando runs: `<home>/bin/xcrun` when it is there and
/// executable, the hook docker and cloudflared have, else `xcrun` on
/// PATH. Under test, only the stand-in: no test reaches a real
/// simulator.
fn xcrun_program(paths: &PandoPaths) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let shim = paths.home.join("bin").join("xcrun");
    let runnable = std::fs::metadata(&shim)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    match (runnable, cfg!(test)) {
        (true, _) => Some(shim),
        (false, false) => Some(PathBuf::from("xcrun")),
        (false, true) => None,
    }
}

/// Every app bundle on a booted simulator that holds a device framework's
/// config, in the order of the simulators' names. Nothing when there is
/// no xcrun, when it fails or does not answer within `timeout`, or when
/// no simulator is booted.
fn simulator_apps(paths: &PandoPaths, timeout: Duration) -> Vec<SimulatorApp> {
    let Some(xcrun) = xcrun_program(paths) else {
        return Vec::new();
    };
    let mut command = Command::new(xcrun);
    command.args(["simctl", "list", "-j", "devices", "booted"]);
    let listed = crate::project::output_within(command, timeout)
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok());
    let Some(listed) = listed else {
        return Vec::new();
    };
    let mut booted: Vec<(&str, &str)> = listed["devices"]
        .as_object()
        .into_iter()
        .flat_map(|runtimes| runtimes.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter(|device| device["state"] == "Booted")
        .filter_map(|device| Some((device["name"].as_str()?, device["dataPath"].as_str()?)))
        .collect();
    booted.sort_unstable();
    let configs: Vec<&'static str> = frameworks::RULES
        .iter()
        .filter_map(|rule| Some(rule.device.as_ref()?.installed.config))
        .collect();
    let mut apps = Vec::new();
    for (name, data) in booted {
        for bundle in app_bundles(&Path::new(data).join(INSTALLED_APPS)) {
            for &holds in &configs {
                let config = std::fs::read_to_string(bundle.join(holds))
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok());
                if let Some(config) = config {
                    apps.push(SimulatorApp {
                        device: name.to_string(),
                        holds,
                        config,
                    });
                }
            }
        }
    }
    apps
}

/// Each `<Name>.app` below `installed`, one directory per install, in a
/// stable order.
fn app_bundles(installed: &Path) -> Vec<PathBuf> {
    let entries = |dir: &Path| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        paths.sort();
        paths
    };
    entries(installed)
        .iter()
        .flat_map(|install| entries(install))
        .filter(|path| path.extension().is_some_and(|ext| ext == "app"))
        .collect()
}
