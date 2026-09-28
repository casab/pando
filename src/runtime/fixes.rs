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

/// Directories that hold one of the language's binaries: each directory
/// on `path` in its order, then the language's install directories.
///
/// `path` is the PATH pando was started with, which is the developer's own
/// shell's: the binary they get by hand is found there without reading
/// their profile. Whether a directory holds a version the project accepts
/// is the probe's to say, through the line [`path_line`] makes of it.
pub fn binary_dirs(language: &Language, system: &Path, path: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let installs = language.install_dirs_under(system);
    for dir in path.iter().chain(installs.iter()) {
        let holds = language
            .binaries
            .iter()
            .any(|binary| dir.join(binary).is_file());
        if holds && dir.is_absolute() && !out.contains(dir) {
            out.push(dir.clone());
        }
    }
    out
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
