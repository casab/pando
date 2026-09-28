//! The prelude lines that would make this machine resolve what the
//! project asks for: from the managers it has installed, and from the
//! directories a binary of the language sits in outside them.

use super::Requirement;
use super::languages::Family;
use super::languages::Language;
use super::languages::Manager;
use std::path::{Path, PathBuf};

/// A prelude line that would make this machine resolve what the project
/// asks for, and why it is on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    pub line: String,
    pub manager: &'static str,
    pub why: String,
}

/// The lines worth offering, for the managers that are actually installed.
///
/// Ordered as the table lists them: the language's own manager first, then
/// the general-purpose ones, then the families that need sourcing — a
/// PATH line is the sturdier fix in a non-interactive shell, so it is the
/// one a question preselects.
///
/// A `use` line belongs only where the requirement came from a file that
/// manager reads itself: it takes the version from the repository, so one
/// line in a machine-wide file is right for every project on the machine.
/// A manager whose init line switches to nothing on its own, and whose
/// `use` line cannot read the requirement, is not offered at all: its line
/// could never change what resolves.
pub fn fixes(
    language: &Language,
    home: &Path,
    system: &Path,
    requirement: &Requirement,
) -> Vec<Fix> {
    language
        .managers
        .iter()
        .filter_map(|manager| {
            let path = manager.installed_at(home, system)?;
            let mut line = manager.init.replace("{path}", &path.display().to_string());
            let use_line = manager
                .use_line
                .filter(|_| manager.use_reads.contains(&requirement.source.as_str()));
            match use_line {
                Some(use_line) => line = format!("{line} && {use_line}"),
                None if !manager.init_activates => return None,
                None => {}
            }
            let why = match manager.family {
                Family::Shim => format!(
                    "{} is installed here, and its shims resolve the version per directory",
                    manager.name
                ),
                Family::SourceEval => format!(
                    "{} is installed here, and a login shell has to source it",
                    manager.name
                ),
            };
            Some(Fix {
                line,
                manager: manager.name,
                why,
            })
        })
        .collect()
}

/// Every manager for this language that this machine has.
pub fn installed(language: &Language, home: &Path, system: &Path) -> Vec<&'static Manager> {
    language
        .managers
        .iter()
        .filter(|manager| manager.installed_at(home, system).is_some())
        .collect()
}

/// Directories that hold one of the language's binaries, narrowest
/// first: the ones that hold only this runtime, then the
/// [shared ones](super::languages::SHARED_BIN_DIRS) that hold every
/// tool. Within each, the directories on `path` in its order, then the
/// language's install directories.
///
/// `path` is the PATH pando was started with, which is the developer's own
/// shell's: the binary they get by hand is found there without reading
/// their profile. Whether a directory holds a version the project accepts
/// is the probe's to say, through the line [`path_line`] makes of it.
///
/// Two directories that hold the same binary, through a link, are one
/// option: the narrowest is kept. A directory whose name states a version
/// the pin rejects is left out before that, so a stale `node@24` link to
/// a node 25 never stands for it.
pub fn binary_dirs(
    language: &Language,
    system: &Path,
    path: &[PathBuf],
    requirement: &Requirement,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let installs = language.install_dirs_under(system);
    for dir in path.iter().chain(installs.iter()) {
        if binary_in(language, dir).is_some()
            && dir.is_absolute()
            && !out.contains(dir)
            && !named_for_another(language, dir, system, requirement)
        {
            out.push(dir.clone());
        }
    }
    // Stable, so each group keeps the order it was found in.
    out.sort_by_key(|dir| super::languages::is_shared_bin(dir, system));
    let mut seen: Vec<PathBuf> = Vec::new();
    out.retain(|dir| {
        let Some(real) = binary_in(language, dir).and_then(|b| std::fs::canonicalize(b).ok())
        else {
            return true;
        };
        if seen.contains(&real) {
            return false;
        }
        seen.push(real);
        true
    });
    out
}

/// The first of the language's binaries `dir` holds.
fn binary_in(language: &Language, dir: &Path) -> Option<PathBuf> {
    language
        .binaries
        .iter()
        .map(|binary| dir.join(binary))
        .find(|path| path.is_file())
}

/// Whether `dir`'s name states a version the requirement's pin rejects:
/// `node@24` for a pin of 25. Only a pin is held to it; a range is left
/// to the probe, since a name like `node@24` states a whole major.
fn named_for_another(
    language: &Language,
    dir: &Path,
    system: &Path,
    requirement: &Requirement,
) -> bool {
    if !requirement.pinned {
        return false;
    }
    let Some(stated) = language.stated_version(dir, system) else {
        return false;
    };
    let pin = super::normalize_spec(&requirement.spec);
    // Compared as far as both go: `python@3.12` holds a pin of `3.12.1`,
    // and a pin of `3` as well.
    stated
        .split('.')
        .zip(pin.split('.'))
        .any(|(a, b)| a.parse::<u64>().ok() != b.parse::<u64>().ok())
}

/// Whether `dir`'s own binary says, asked directly, that it is a version
/// `spec` rejects: a directory ruled out for the price of one exec rather
/// than a login shell, which on a developer's machine loads their whole
/// profile. Anything short of a version read and refused rules nothing
/// out, and the directory goes to the probe as before.
pub fn rules_out(language: &Language, dir: &Path, spec: &str) -> bool {
    let Some(binary) = binary_in(language, dir) else {
        return false;
    };
    let Some(output) = version_output(&binary, language.version_flag) else {
        return false;
    };
    super::first_version(&output)
        .is_some_and(|version| super::satisfies(spec, &version) == super::Verdict::Mismatch)
}

/// What `binary <flag>` prints, given two seconds: a version flag answers
/// at once, and one that does not is no answer.
fn version_output(binary: &Path, flag: &str) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(binary)
        .arg(flag)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait().ok()? {
            Some(_) => break,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    if text.trim().is_empty() {
        child.stderr.take()?.read_to_string(&mut text).ok()?;
    }
    Some(text)
}

/// The prelude line that puts `dir` first on PATH, for a directory whose
/// name survives double quotes as it is. `None` for one that does not: a
/// line pando would have to escape is not one to hand a developer.
pub fn path_line(dir: &Path) -> Option<String> {
    let dir = dir.to_str()?;
    if dir.contains(['"', '$', '`', '\\', '\n']) {
        return None;
    }
    Some(format!("export PATH=\"{dir}:$PATH\""))
}
