//! Tier 1: what the repository says about how to run itself.
//!
//! Everything here is read-only over the main checkout, and everything it
//! produces is a *proposal* — a list of candidates a rule found, in rule
//! order, with the reason each one is a candidate. Nothing starts anything.
//! That split is Invariant 2: detection only ever writes `pando.toml`, and
//! the runtime only ever reads it.
//!
//! The rules enumerate; they never invent. A script that is not in
//! `package.json` cannot be proposed, so the worst a wrong rule can do is
//! pick the wrong one of the developer's own commands — one visible edit to
//! a file with a `# detected:` comment next to the line.
//!
//! **Where things are.** `signals` reads the repository: manifests,
//! scripts, make targets, version files, env examples and what is
//! gitignored. `frameworks` picks the framework rule those signals match;
//! the rules themselves live in `catalog::frameworks`. `proposal` holds
//! the types a proposal is made of and [`propose`], which asks every slot;
//! `dev` has the per-slot proposals for install, version files, the dev
//! command, make targets, ports and provision, `workspaces` the apps of a
//! monorepo, and `services` the services a project talks to and its schema
//! hook. `apply` turns a chosen candidate into config and into edits.

pub use crate::catalog::frameworks::{FrameworkRule, PortMechanism, RULES};

mod apply;
mod dev;
mod frameworks;
mod proposal;
mod services;
mod signals;
#[cfg(test)]
mod tests;
mod workspaces;

pub use apply::{
    DEV, Edit, apply, apply_native_services, apply_services, array_edits, custom, edits,
    fills_one_dev_process, join_list, may_fill_dev, native_entry, roles_in, service_entry, snippet,
    still_needed,
};
pub use dev::lockfiles_ignored;
pub use frameworks::framework;
pub use proposal::{
    Candidate, ComposeResolver, Proposal, ServiceHint, Slot, propose, propose_with,
};
pub use services::{
    MachineEvidence, NO_PREFERENCE_EVIDENCE, SCHEMA_HOOK, ServiceChoice, ServiceSource,
    service_choice, service_choice_for,
};
pub use signals::{Signals, Target, is_gitignored, signals};
pub use workspaces::{WorkspaceApp, workspace_apps};
