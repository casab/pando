//! The languages pando can read a requirement for, and the version
//! managers that can satisfy them. Data: adding a language or a manager is
//! an entry here.

use super::normalize_spec;
use std::path::Path;

/// How the spec is dug out of a file that pins one language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The file is the version, give or take a comment, a `v`, and a
    /// toolchain vendor prefix.
    Plain,
    /// `[toolchain] channel = "1.75.0"`, the `rust-toolchain.toml` shape.
    TomlKey(&'static str, &'static str),
}

/// A file that states one language's requirement.
#[derive(Debug, Clone, Copy)]
pub struct Source {
    pub file: &'static str,
    pub kind: SourceKind,
}

/// Everything pando knows about one language.
///
/// Entries carry the requirement sources now; the probe command, the
/// version parser and the manager families join them in the same struct,
/// so one lookup answers every question a call site has.
#[derive(Debug, Clone, Copy)]
pub struct Language {
    /// The name used in a `Requirement`, in a report, and in config.
    pub name: &'static str,
    /// Files that pin this language, in the order they are read.
    pub files: &'static [Source],
    /// What it is called in `.tool-versions` and `mise.toml`, where one
    /// file names every tool. `nodejs` and `golang` live here.
    pub aliases: &'static [&'static str],
    /// The `engines` key that describes it, when there is one.
    pub engines_key: Option<&'static str>,
    /// The binaries a project's own commands run, in the order to try
    /// them: a Python project may have `python3`, `python`, or both.
    pub binaries: &'static [&'static str],
    /// What to ask one for its version. Its output goes through
    /// [`first_version`], which every language in the table shares.
    pub version_flag: &'static str,
    /// Who can satisfy this language on a machine, in the order a
    /// question offers them.
    pub managers: &'static [Manager],
}

impl Language {
    /// Whether a `.tool-versions` or `mise.toml` entry is about this
    /// language.
    pub(super) fn owns(&self, tool: &str) -> bool {
        let tool = tool.trim().to_ascii_lowercase();
        self.name == tool || self.aliases.contains(&tool.as_str())
    }
}

/// The languages pando can read a requirement for.
///
/// Adding one is an entry here and nothing else: no call site enumerates
/// languages, and Phase 6's recipe loader is meant to append to this from
/// disk rather than replace it.
pub const LANGUAGES: [Language; 6] = [
    Language {
        name: "node",
        files: &[
            Source {
                file: ".nvmrc",
                kind: SourceKind::Plain,
            },
            Source {
                file: ".node-version",
                kind: SourceKind::Plain,
            },
        ],
        aliases: &["nodejs"],
        engines_key: Some("node"),
        binaries: &["node"],
        version_flag: "-v",
        managers: &[VOLTA, MISE, ASDF, NVM, FNM],
    },
    Language {
        name: "python",
        files: &[Source {
            file: ".python-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["python3", "python"],
        version_flag: "-V",
        managers: &[PYENV, MISE, ASDF],
    },
    Language {
        name: "ruby",
        files: &[Source {
            file: ".ruby-version",
            kind: SourceKind::Plain,
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["ruby"],
        version_flag: "-v",
        managers: &[RBENV, MISE, ASDF, RVM],
    },
    Language {
        name: "rust",
        files: &[Source {
            file: "rust-toolchain.toml",
            kind: SourceKind::TomlKey("toolchain", "channel"),
        }],
        aliases: &[],
        engines_key: None,
        binaries: &["rustc"],
        version_flag: "-V",
        managers: &[RUSTUP, MISE, ASDF],
    },
    Language {
        name: "go",
        files: &[],
        aliases: &["golang"],
        engines_key: None,
        binaries: &["go"],
        version_flag: "version",
        managers: &[MISE, ASDF],
    },
    Language {
        name: "java",
        files: &[],
        aliases: &[],
        engines_key: None,
        binaries: &["java"],
        version_flag: "-version",
        managers: &[JENV, MISE, ASDF, SDKMAN],
    },
];

/// The table entry for a language, by name.
pub fn language(name: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|language| language.name == name)
}

// ---- version managers -----------------------------------------------------

/// How a manager is made to work in a non-interactive login shell, which
/// is the only shell pando has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// volta, asdf, mise, pyenv, rbenv, rustup, jenv. They resolve per
    /// directory by themselves, so when they fail it is a PATH problem and
    /// the fix is a PATH line. Proposing a `use` line for one is wrong.
    Shim,
    /// nvm, fnm, rvm, sdkman. They are shell functions, so they exist only
    /// after an init line has been sourced — and a developer whose shell
    /// sources it in `.zshrc` has no manager at all in pando's bash.
    SourceEval,
}

/// One version manager, and what pando would have to put in front of a
/// command for it to work.
#[derive(Debug, Clone, Copy)]
pub struct Manager {
    pub name: &'static str,
    pub family: Family,
    /// Paths that prove it is installed: absolute, or relative to the home
    /// directory. Any one of them is enough, because Homebrew puts nvm
    /// somewhere else entirely.
    pub markers: &'static [&'static str],
    /// The line that makes it work, with `{path}` standing for whichever
    /// marker was found.
    pub init: &'static str,
    /// The line that switches to the version the repository's own file
    /// names. Only the source-and-eval family has one.
    pub use_line: Option<&'static str>,
    /// How it installs a version, with `{spec}` and `{language}`. Printed,
    /// never run: mutating the developer's machine is not pando's job.
    pub install: Option<&'static str>,
    /// Whether it names a language the way `.tool-versions` does —
    /// `nodejs`, `golang` — rather than the way pando does.
    pub uses_alias: bool,
}

impl Manager {
    /// Where it is installed, if it is.
    pub fn installed_at(&self, home: &Path) -> Option<std::path::PathBuf> {
        self.markers
            .iter()
            .map(|marker| {
                if marker.starts_with('/') {
                    std::path::PathBuf::from(marker)
                } else {
                    home.join(marker)
                }
            })
            .find(|path| path.exists())
    }

    /// The command that installs a version under it. Printed as a hint.
    pub fn install_command(&self, language: &Language, spec: &str) -> Option<String> {
        let name = if self.uses_alias {
            language.aliases.first().copied().unwrap_or(language.name)
        } else {
            language.name
        };
        Some(
            self.install?
                .replace("{spec}", normalize_spec(spec))
                .replace("{language}", name),
        )
    }
}

const VOLTA: Manager = Manager {
    name: "volta",
    family: Family::Shim,
    markers: &[".volta/bin"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("volta install {language}@{spec}"),
    uses_alias: false,
};

const MISE: Manager = Manager {
    name: "mise",
    family: Family::Shim,
    markers: &[".local/share/mise/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("mise install {language}@{spec}"),
    uses_alias: false,
};

const ASDF: Manager = Manager {
    name: "asdf",
    family: Family::Shim,
    markers: &[".asdf/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("asdf install {language} {spec}"),
    uses_alias: true,
};

const NVM: Manager = Manager {
    name: "nvm",
    family: Family::SourceEval,
    markers: &[
        ".nvm/nvm.sh",
        "/opt/homebrew/opt/nvm/nvm.sh",
        "/usr/local/opt/nvm/nvm.sh",
    ],
    init: "export NVM_DIR=\"$HOME/.nvm\" && . \"{path}\" --no-use",
    // No version in it: this line goes in a file every project on the
    // machine shares, and `nvm use` with no argument takes the version
    // from the repository's own `.nvmrc`.
    use_line: Some("nvm use >/dev/null"),
    install: Some("nvm install {spec}"),
    uses_alias: false,
};

const FNM: Manager = Manager {
    name: "fnm",
    family: Family::SourceEval,
    markers: &[
        ".local/share/fnm/fnm",
        ".fnm/fnm",
        "/opt/homebrew/bin/fnm",
        "/usr/local/bin/fnm",
    ],
    init: "eval \"$({path} env)\"",
    use_line: Some("fnm use >/dev/null"),
    install: Some("fnm install {spec}"),
    uses_alias: false,
};

const PYENV: Manager = Manager {
    name: "pyenv",
    family: Family::Shim,
    markers: &[".pyenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("pyenv install {spec}"),
    uses_alias: false,
};

const RBENV: Manager = Manager {
    name: "rbenv",
    family: Family::Shim,
    markers: &[".rbenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("rbenv install {spec}"),
    uses_alias: false,
};

const RVM: Manager = Manager {
    name: "rvm",
    family: Family::SourceEval,
    markers: &[".rvm/scripts/rvm"],
    init: ". \"{path}\"",
    use_line: Some("rvm use . >/dev/null"),
    install: Some("rvm install {spec}"),
    uses_alias: false,
};

const RUSTUP: Manager = Manager {
    name: "rustup",
    family: Family::Shim,
    markers: &[".cargo/bin"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    install: Some("rustup toolchain install {spec}"),
    uses_alias: false,
};

const JENV: Manager = Manager {
    name: "jenv",
    family: Family::Shim,
    markers: &[".jenv/shims"],
    init: "export PATH=\"{path}:$PATH\"",
    use_line: None,
    // jenv manages JDKs that are already on the machine; it installs none.
    install: None,
    uses_alias: false,
};

const SDKMAN: Manager = Manager {
    name: "sdkman",
    family: Family::SourceEval,
    markers: &[".sdkman/bin/sdkman-init.sh"],
    init: ". \"{path}\"",
    use_line: None,
    install: Some("sdk install java {spec}"),
    uses_alias: false,
};
