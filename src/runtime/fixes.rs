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
/// A `use` line belongs only where the requirement came from a file that
/// manager reads itself: it takes the version from the repository, so one
/// line in a machine-wide file is right for every project on the machine.
/// A manager whose init line switches to nothing on its own, and whose
/// `use` line cannot read the requirement, is not offered at all: its line
/// could never change what resolves.
pub fn fixes(language: &Language, home: &Path, requirement: &Requirement) -> Vec<Fix> {
    language
        .managers
        .iter()
        .filter_map(|manager| {
            let path = manager.installed_at(home)?;
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
pub fn installed(language: &Language, home: &Path) -> Vec<&'static Manager> {
    language
        .managers
        .iter()
        .filter(|manager| manager.installed_at(home).is_some())
        .collect()
}
