//! The git menu: bringing a branch up to date, in git's own commands.
//!
//! Every move here is one the developer picked and saw the commands of
//! first: a fetch, a fast-forward of the branch to its upstream, a rebase
//! onto its base or a merge of the base into it, and the abort of one left
//! half-done. Nothing else — no push, no reset, no stash, no switch of
//! branch — and nothing on a checkout with uncommitted changes. A rebase
//! or a merge that stops on a conflict is aborted again before it is
//! reported, so the checkout is exactly as it was. `docs/02-principles.md`
//! says why this is outside what Invariant 1 forbids, and what holds it.
//!
//! One file per concern: the table of actions, reading where a checkout
//! stands, what the menu offers and the preview of each, and running one.

mod offer;
mod read;
mod run;
mod table;

pub use offer::{Offer, Plan, offers, plan};
pub use read::{GitRead, base_for, read};
pub use run::{Ran, run};
pub use table::{ACTIONS, ActionRow, GitAction};

#[cfg(test)]
mod tests;
