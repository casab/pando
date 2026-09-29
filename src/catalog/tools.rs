//! The programs pando runs itself, and how a developer gets each one.
//!
//! pando never installs any of them. `doctor` prints `get` beside a tool
//! it did not find, and a command that needs one says it when it refuses.
//! A package manager's line is on its own row in
//! [`super::package_managers`], and a native engine's in its recipe's
//! `install =`; [`how_to_get`] answers for the first two.

use super::package_managers;

#[derive(Debug, Clone, Copy)]
pub struct Tool {
    /// The binary on PATH.
    pub program: &'static str,
    /// What a developer runs to have it, as doctor prints it after
    /// "install it with:".
    pub get: &'static str,
}

pub const TOOLS: [Tool; 4] = [
    Tool {
        program: "git",
        get: "xcode-select --install   (or your distribution's git package)",
    },
    // Not a command: on a Mac, Docker is an app, and which one is the
    // developer's choice.
    Tool {
        program: "docker",
        get: "Docker Desktop or OrbStack   (or your distribution's Docker Engine and its \
              compose plugin)",
    },
    Tool {
        program: "cloudflared",
        get: "brew install cloudflared   (or Cloudflare's cloudflared package)",
    },
    Tool {
        program: "gh",
        get: "brew install gh   (or the GitHub CLI's gh package)",
    },
];

/// The line that gets `program`, when pando knows one.
pub fn how_to_get(program: &str) -> Option<&'static str> {
    TOOLS
        .iter()
        .find(|tool| tool.program == program)
        .map(|tool| tool.get)
        .or_else(|| package_managers::for_program(program).map(|manager| manager.get))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_program_has_a_line_to_get_it() {
        let programs = TOOLS.iter().map(|t| (t.program, t.get)).chain(
            package_managers::PACKAGE_MANAGERS
                .iter()
                .map(|m| (m.program, m.get)),
        );
        for (program, get) in programs {
            assert!(!get.trim().is_empty(), "{program} has no line to get it");
            assert_eq!(how_to_get(program), Some(get), "{program}");
        }
    }

    #[test]
    fn no_program_is_in_both_lists() {
        for tool in &TOOLS {
            assert!(
                package_managers::for_program(tool.program).is_none(),
                "{} is a tool and a package manager",
                tool.program
            );
        }
    }

    #[test]
    fn an_unknown_program_has_no_line() {
        assert_eq!(how_to_get("make"), None);
    }
}
