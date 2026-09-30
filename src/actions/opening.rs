//! Opening the app of a process a device runs, from this machine: on the
//! booted iOS simulator, else on a connected Android device or emulator,
//! else, on a Mac with Xcode, on a simulator pando starts and waits for.
//!
//! What runs is the command `status` prints, filled from the same catalog
//! row, so what is said and what is done cannot drift apart.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::catalog::devices::{Platform, SIMULATOR_APP, TARGETS, Target};
use crate::catalog::frameworks::AppLinks;
use crate::paths::PandoPaths;

/// How long a started simulator app is given to boot a device.
pub const BOOT_WAIT: Duration = Duration::from_secs(120);

/// How long an open that failed because its target is still starting is
/// tried again for.
pub const RETRY_WAIT: Duration = Duration::from_secs(60);

/// How often a wait looks again.
const EVERY: Duration = Duration::from_secs(2);

/// A shell command that ran: whether it exited 0, and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub ok: bool,
    /// Its stdout, then its stderr.
    pub output: String,
}

/// Runs a shell command; `None` when it could not be run at all.
pub type RunCommand<'a> = &'a dyn Fn(&str) -> Option<Ran>;

/// What an opening runs its commands with, and how long it waits.
pub struct Opener<'a> {
    pub run: RunCommand<'a>,
    /// Whether a simulator may be started here: on macOS, where Xcode is.
    pub may_start_simulator: bool,
    pub boot_wait: Duration,
    pub retry_wait: Duration,
    pub every: Duration,
}

impl<'a> Opener<'a> {
    /// This machine's opener, running its commands with `run`.
    pub fn new(run: RunCommand<'a>) -> Opener<'a> {
        Opener {
            run,
            may_start_simulator: cfg!(target_os = "macos"),
            boot_wait: BOOT_WAIT,
            retry_wait: RETRY_WAIT,
            every: EVERY,
        }
    }
}

/// Why an app was not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotOpened {
    /// There is nothing here to open it on, and why: the caller gives the
    /// commands, for the developer to run once there is.
    Nowhere(String),
    /// A command ran on a target and failed, with what it said.
    Failed {
        on: &'static str,
        command: String,
        output: String,
    },
}

impl std::fmt::Display for NotOpened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotOpened::Nowhere(why) => write!(f, "{why}"),
            NotOpened::Failed {
                on,
                command,
                output,
            } => {
                write!(f, "`{command}` failed on {on}")?;
                match output.lines().rev().find(|line| !line.trim().is_empty()) {
                    Some(last) => write!(f, ": {}", last.trim()),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Every target's command for `links`, in the order they are tried, as
/// `(target, command)`: what a developer runs where pando could not.
pub fn open_commands(links: &AppLinks) -> Vec<(&'static str, &str)> {
    TARGETS
        .iter()
        .map(|target| (target.name, command_for(target, links)))
        .collect()
}

/// Opens the app `links` names, and says where: on the first target that
/// is ready, else on a simulator it starts. `say` hears each wait before
/// it begins, with how long it may last.
pub fn open_app(
    links: &AppLinks,
    opener: &Opener<'_>,
    say: &dyn Fn(&str),
) -> Result<&'static str, NotOpened> {
    for target in &TARGETS {
        if ready(target, opener) {
            return open_on(target, links, opener, false, say);
        }
    }
    let nothing = "no iOS simulator is booted and no Android device or emulator is connected";
    if !opener.may_start_simulator {
        return Err(NotOpened::Nowhere(nothing.to_string()));
    }
    let ios = TARGETS
        .iter()
        .find(|target| target.platform == Platform::Ios)
        .expect("the catalog has an iOS target");
    match start_simulator(ios, opener, say) {
        Ok(()) => open_on(ios, links, opener, true, say),
        Err(why) => Err(NotOpened::Nowhere(format!("{nothing}, and {why}"))),
    }
}

fn command_for<'l>(target: &Target, links: &'l AppLinks) -> &'l str {
    match target.platform {
        Platform::Ios => &links.simulator,
        Platform::Android => &links.android,
    }
}

/// Whether `target` lists one ready. A tool that is not installed lists
/// none.
fn ready(target: &Target, opener: &Opener<'_>) -> bool {
    (opener.run)(target.list).is_some_and(|ran| ran.ok && target.lists_one_ready(&ran.output))
}

/// Runs `target`'s command, again while it fails because the target is
/// still starting — or, on one pando has just booted, for any reason —
/// until [`Opener::retry_wait`] is over.
fn open_on(
    target: &'static Target,
    links: &AppLinks,
    opener: &Opener<'_>,
    just_booted: bool,
    say: &dyn Fn(&str),
) -> Result<&'static str, NotOpened> {
    let command = command_for(target, links);
    let deadline = Instant::now() + opener.retry_wait;
    let mut said = false;
    loop {
        let ran = (opener.run)(command);
        let output = match &ran {
            Some(ran) if ran.ok => return Ok(target.name),
            Some(ran) => ran.output.clone(),
            None => "it could not be run".to_string(),
        };
        let again = ran.is_some()
            && (just_booted || target.still_starting(&output))
            && Instant::now() < deadline;
        if !again {
            return Err(NotOpened::Failed {
                on: target.name,
                command: command.to_string(),
                output,
            });
        }
        if !said {
            say(&format!(
                "{} is still starting — trying again for up to {}",
                target.name,
                seconds(opener.retry_wait)
            ));
            said = true;
        }
        std::thread::sleep(opener.every);
    }
}

/// Starts the simulator app of the Xcode in use and waits for it to boot
/// a device, or says why it could not.
fn start_simulator(ios: &Target, opener: &Opener<'_>, say: &dyn Fn(&str)) -> Result<(), String> {
    let developer = (opener.run)(SIMULATOR_APP.developer_dir)
        .filter(|ran| ran.ok)
        .map(|ran| ran.output.trim().to_string())
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| {
            format!(
                "`{}` names no Xcode to start a simulator with",
                SIMULATOR_APP.developer_dir
            )
        })?;
    let apps = Path::new(&developer).join(SIMULATOR_APP.apps_dir);
    let apps = std::fs::canonicalize(&apps).unwrap_or(apps);
    let Some(app) = SIMULATOR_APP
        .names
        .iter()
        .map(|name| apps.join(name))
        .find(|app| app.exists())
    else {
        return Err(format!(
            "the Xcode at {developer} has no {} to start a simulator with",
            SIMULATOR_APP.names.join(" or ")
        ));
    };
    let name = app
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    say(&format!(
        "starting {name} and waiting up to {} for a simulator to boot",
        seconds(opener.boot_wait)
    ));
    let launch = SIMULATOR_APP.launch.replace(
        "{app}",
        &crate::process::shell_quote(&app.display().to_string()),
    );
    match (opener.run)(&launch) {
        Some(ran) if ran.ok => {}
        Some(ran) => return Err(format!("`{launch}` failed: {}", ran.output.trim())),
        None => return Err(format!("`{launch}` could not be run")),
    }
    let deadline = Instant::now() + opener.boot_wait;
    loop {
        if ready(ios, opener) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{name} booted no simulator within {} — boot one there, then open it again",
                seconds(opener.boot_wait)
            ));
        }
        std::thread::sleep(opener.every);
    }
}

fn seconds(wait: Duration) -> String {
    format!("{}s", wait.as_secs())
}

/// Runs a shell command the way an opening does: through `sh`, with
/// pando's own `bin` first on PATH, where a developer puts a shim and a
/// test its stand-ins, and nothing on stdin.
///
/// Under `cfg(test)` it runs nothing and says so: a unit test that opens
/// an app must never reach a real simulator. Tests of the opening drive
/// their own [`RunCommand`].
pub fn run_command(paths: &PandoPaths, command: &str) -> Option<Ran> {
    if cfg!(test) {
        return None;
    }
    let bin = paths.home.join("bin");
    let path = match std::env::var_os("PATH") {
        Some(path) => {
            let mut dirs = vec![bin];
            dirs.extend(std::env::split_paths(&path));
            std::env::join_paths(dirs).ok()?
        }
        None => bin.into_os_string(),
    };
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&out.stderr));
    Some(Ran {
        ok: out.status.success(),
        output,
    })
}
