//! Reading the project's own compose file.
//!
//! "Keep the project boring": a team that already describes its services in
//! a compose file should not have to describe them again for pando. So this
//! module reads that file and nothing else — it never writes to it, and the
//! per-worktree override it generates in the next slice lives under pando's
//! home.
//!
//! The parser is a deliberate subset of YAML rather than a dependency. Two
//! reasons. The override pando writes needs the `!override` and `!reset`
//! tags, which no serde YAML crate emits, so the *writing* side is
//! hand-rolled whatever happens. And what pando reads is a handful of keys
//! — `image`, `ports`, `volumes`, `container_name`, `healthcheck`,
//! `depends_on` — whose shapes are fixed by the compose specification.
//! Anything it does not understand is ignored rather than guessed at, and
//! a service pando cannot describe is refused by name rather than started
//! wrong.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod config_json;
mod isolation;
mod yaml;

pub use config_json::parse_config_json;
pub use isolation::{Published, project_name, render_override, resolve_included};
pub use yaml::parse;

/// The file names compose itself looks for, in its own order of
/// precedence. Detection offers the first one that exists.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// The compose file as much of it as pando needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposeFile {
    pub services: BTreeMap<String, Service>,
    /// The top-level `volumes:` block. Only the keys that decide whether
    /// the compose project name isolates a volume are kept.
    pub volumes: BTreeMap<String, TopVolume>,
    /// What this reader knew it could not answer. Empty for a file it read
    /// whole, and for anything `docker compose config` resolved.
    pub unresolved: Unresolved,
}

/// Keys this reader does not follow, recorded rather than ignored.
///
/// `extends:` pulls a service's real definition out of another file, a
/// top-level `include:` adds whole services this file never names, a
/// YAML alias or merge key stands for text written somewhere else in the
/// file, and a tag, a value spread over several lines or a second YAML
/// document is text this reader does not read at all. Any one of them
/// means the ports and volumes pando is reading are not the ones compose
/// would use — so every refusal has to say so rather than tell the
/// developer to add a `ports:` entry their file already has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unresolved {
    /// Services that carry an `extends:` key, in file order.
    pub extends: Vec<String>,
    /// Whether the file has a top-level `include:`.
    pub include: bool,
    /// Whether a service or a top-level volume uses a YAML alias
    /// (`*name`) or a merge key (`<<:`). What either one brings in — a
    /// bind mount among it — is invisible to this reader, so a file with
    /// one is never approved for isolation on this reader's word.
    pub aliases: bool,
    /// Whether a value this reader takes anything from, in a service or a
    /// top-level volume, is one it could not read — one behind a YAML tag
    /// (`!override`, `!reset`), a block scalar (`|`, `>-`) or a plain
    /// scalar folded onto the lines under it, a quoted scalar that goes on
    /// over a line break, alone or inside a flow collection, a flow
    /// collection whose brackets do not pair up, or one on several lines
    /// with a quote in it that neither opens nor closes a quoted scalar,
    /// or a mapping with a line or an entry it finds no key in — or a
    /// value anywhere goes on over lines this reader takes for keys, or the
    /// file goes on past its first YAML document, which compose merges the
    /// others into. A bind mount written in any of them is as invisible as
    /// one behind an alias.
    pub unread: bool,
}

/// What [`Unresolved::unread`] stands for, in the words every message
/// about it uses.
const UNREAD: &str = "YAML tags, values spread over several lines or a second document \
                      (`!override`, a `[` or a quote closed on a later line, `---`)";

impl Unresolved {
    pub fn any(&self) -> bool {
        !self.extends.is_empty() || self.include || self.aliases || self.unread
    }

    /// The keys, named the way the compose specification names them, for a
    /// message. `None` when there is nothing to say.
    pub fn describe(&self) -> Option<String> {
        let mut keys = Vec::new();
        if !self.extends.is_empty() {
            keys.push(format!("`extends:` (on {})", self.extends.join(", ")));
        }
        if self.include {
            keys.push("a top-level `include:`".to_string());
        }
        if self.aliases {
            keys.push("YAML aliases or merge keys (`*`, `<<:`)".to_string());
        }
        if self.unread {
            keys.push(UNREAD.to_string());
        }
        match keys.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Service {
    pub image: Option<String>,
    /// The build context, when this service is built rather than pulled:
    /// the scalar of `build: ./api`, or its `context:`. A `build:` mapping
    /// with no `context:` means `.`, which is compose's own default.
    ///
    /// Kept as written — relative to the compose file's own directory, which
    /// is the directory compose resolves it against — except from
    /// `docker compose config`, which has already made it absolute.
    ///
    /// What it is for: a service built out of this repository *is* the
    /// application, not one of its dependencies, and offering a private copy
    /// of it is the one answer that is certainly wrong. A service may carry
    /// both `image:` and `build:`; the image is then the name to tag, not a
    /// dependency to pull.
    pub build: Option<String>,
    /// Every `ports:` entry, in file order.
    pub ports: Vec<Port>,
    pub volumes: Vec<Mount>,
    pub container_name: Option<String>,
    /// Whether the service declares a `healthcheck` that is not turned
    /// off with `disable: true` or `test: ["NONE"]`. Readiness prefers it:
    /// a connect succeeding says the socket is open, not that the database
    /// will answer a query.
    pub healthcheck: bool,
    pub depends_on: Vec<String>,
}

/// One `ports:` entry. `published` is what the *project* asked for, which
/// an isolated worktree replaces; `container` is the one that survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub container: u16,
    pub published: Option<u16>,
    pub host: Option<String>,
}

/// One `volumes:` entry of a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A named volume, which the compose project name prefixes — which is
    /// what makes a per-worktree project isolate the data for free.
    Named(String),
    /// A host path. One relative to the compose file lands *inside the
    /// repository*, which Invariant 1 forbids pando to write into.
    Bind(String),
    /// `- /var/lib/postgresql/data` with no source: docker makes one up,
    /// and it is per-container, so it is isolated already.
    Anonymous,
}

/// A top-level volume declaration: the keys that can defeat the
/// project-name prefix, so that a service using the volume is refused for
/// isolation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopVolume {
    /// An explicit `name:`, which compose uses verbatim rather than
    /// prefixing.
    pub name: Option<String>,
    /// `external: true`: the volume is expected to exist already, and
    /// every worktree would share the one volume.
    pub external: bool,
    /// `driver:`, when the file names one. None is compose's own `local`.
    pub driver: Option<String>,
    /// `driver_opts:`, each value as written. The project name prefixes
    /// the volume's name, not where its data lives, and these can put it
    /// elsewhere: `o: bind` with a `device:` binds a host directory.
    pub driver_opts: BTreeMap<String, String>,
}

impl Service {
    /// The container port an isolated worktree publishes: the first one the
    /// file declares, else the first one the image table knows.
    ///
    /// One port per service, deliberately. A role is one name and one
    /// number, and a second published port would need a second role and a
    /// second allocation with no name to give it. A service whose extra
    /// ports matter — mailpit's web UI beside its SMTP port — gets the
    /// first of them; the rest stay inside the compose network.
    pub fn container_port(&self) -> Option<u16> {
        if let Some(port) = self.ports.first() {
            return Some(port.container);
        }
        self.image
            .as_deref()
            .and_then(image_ports)
            .and_then(|ports| ports.first().copied())
    }
}

/// The container ports pando knows for an image, from
/// [`crate::catalog::images`].
pub fn image_ports(image: &str) -> Option<&'static [u16]> {
    crate::catalog::images::ports(image)
}

/// The compose file `root` declares, if any, in compose's own precedence
/// order. Returned relative to `root`, which is the form config holds.
pub fn find(root: &Path) -> Option<String> {
    COMPOSE_FILES
        .iter()
        .find(|name| root.join(name).is_file())
        .map(|name| (*name).to_string())
}

/// Reads and parses a compose file, naming the file in any error.
pub fn read(path: &Path) -> Result<ComposeFile> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the compose file {}", path.display()))?;
    parse(&text).with_context(|| format!("in {}", path.display()))
}

/// Where a service's compose file lives inside a worktree, refusing a
/// configured path that would climb out of it.
///
/// `file` comes from config, and config is a file a human edits: an
/// absolute path or a `..` would have pando reading — and, through the
/// project directory compose derives from it, *writing bind mounts* —
/// somewhere it does not own.
pub fn file_in(worktree: &Path, file: &str) -> Result<PathBuf> {
    use std::path::Component;
    let relative = Path::new(file);
    if file.trim().is_empty() {
        bail!("a compose service needs a `file`");
    }
    if relative.is_absolute() {
        bail!("compose file {file:?} must be relative to the worktree");
    }
    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("compose file {file:?} must not escape the worktree");
    }
    Ok(worktree.join(relative))
}

#[cfg(test)]
mod tests;
