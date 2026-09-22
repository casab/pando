//! `pando.toml`: load, layer, validate, write.
//!
//! The types mirror the config spec in full even though this phase only
//! consumes `[project]` and `[branches]` — later phases fill fields in
//! rather than restructure.
//!
//! Three layers, highest precedence first:
//!
//! 1. `<pando home>/projects/<id>/pando.toml`, the project layer, and the
//!    only file pando writes config to by default.
//! 2. `<pando home>/config.toml`, the user layer: one file for every project
//!    on this machine. It holds what is true of the laptop rather than of a
//!    repository — which version manager this shell has to initialise.
//! 3. `<root>/pando.toml`, if a team chose to commit one.
//!
//! Neither of the lower two may set `project.root` or
//! `project.worktrees_dir`: a file inside the repository must never be able
//! to redirect where pando writes, and a file shared by every project cannot
//! name one project's directories. Neither is written by pando, so a layer
//! that does not parse, deserialise or validate is dropped with a warning;
//! only the project layer, which pando wrote itself, fails hard.

mod edit;
mod layers;
mod schema;
mod validate;

pub use edit::{
    Layer, Note, patch, prelude_origin, set_detected, set_detected_array_entry, set_detected_table,
    write,
};
pub use layers::{Loaded, load, load_without_home};
pub use schema::{
    BranchRule, BranchesSection, Config, HookConfig, HookPoint, ISOLATION_KINDS, IsolationSection,
    PortsSpec, ProbeConfig, ProcessConfig, ProjectSection, ProvisionMode, ReadySpec,
    RuntimeSection, ServiceConfig, ShareSection,
};
pub use validate::validate;

/// The role `share`, the browser-open key, and a readiness rule all default
/// to. Roles are otherwise free strings.
pub const WEB_ROLE: &str = "web";

#[cfg(test)]
mod tests;
