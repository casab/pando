//! What pando knows about the ecosystem, written down once.
//!
//! Detection proposes from these facts, `doctor` checks against them,
//! hooks fingerprint with them, and `share` explains a dev server's
//! refusal with them. Each fact lives in exactly one table here,
//! so teaching pando about a new package manager is one row, and there is
//! no second list somewhere else that silently disagrees with the first.
//!
//! Where to add something:
//!
//! - a package manager or lockfile: a row in [`package_managers`];
//! - a framework: a row in [`frameworks::RULES`];
//! - a service image compose files use: a row in [`images::IMAGES`];
//! - a dev server that refuses unknown hosts: a row in
//!   [`host_checks::HOST_CHECKS`];
//! - a tool cache or build artifact no worktree needs a copy of: a row in
//!   [`artifacts::ARTIFACTS`];
//! - a language or version manager: `runtime::languages`, which the
//!   runtime probe reads directly;
//! - a native service (postgres, redis…): a recipe file, see `recipes`.
//!
//! Nothing in this module reads the disk or runs anything. It is data plus
//! the lookups over it; the modules that act on the data stay where they
//! are.

pub mod artifacts;
pub mod frameworks;
pub mod host_checks;
pub mod images;
pub mod package_managers;
