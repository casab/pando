//! The prelude lines that would make this machine resolve what the
//! project asks for, from the managers it has installed.

use super::Requirement;
use super::languages::Family;
use super::languages::Language;
use super::languages::Manager;
use std::path::Path;

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
/// `reads_version_file` says whether the requirement came from a file the
/// manager itself knows how to read, which is the only case where a `use`
/// line belongs: it takes the version from the repository, so one line in
/// a machine-wide file is right for every project on the machine.
pub fn fixes(language: &Language, home: &Path, reads_version_file: bool) -> Vec<Fix> {
    language
        .managers
        .iter()
        .filter_map(|manager| {
            let path = manager.installed_at(home)?;
            let mut line = manager.init.replace("{path}", &path.display().to_string());
            if let Some(use_line) = manager.use_line
                && reads_version_file
            {
                line = format!("{line} && {use_line}");
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
pub fn installed(language: &Language, home: &Path) -> Vec<&'static Manager> {
    language
        .managers
        .iter()
        .filter(|manager| manager.installed_at(home).is_some())
        .collect()
}

/// Whether the requirement was stated in a file this language's managers
/// read themselves.
pub fn from_version_file(language: &Language, requirement: &Requirement) -> bool {
    language
        .files
        .iter()
        .any(|source| source.file == requirement.source)
}
