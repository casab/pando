//! The base question: which branch `new` forks from and `check` tests,
//! asked only when origin/HEAD is far behind the main checkout's branch.

use std::path::Path;

use super::proposal::{Candidate, Proposal, Slot};

/// The base, as a question, when origin/HEAD has fallen so far behind the
/// branch the main checkout is on that it may not be where work starts —
/// `worktree::base_drift` says how far that is. `None` otherwise: then
/// origin/HEAD is the base, as it is for every repository whose default
/// branch is the one its team uses, and there is nothing to ask.
///
/// Never decided, because which branch work starts from is the team's
/// habit, which no ref records. The main checkout's branch is offered
/// first: a hundred commits and a month past the default branch, it is
/// where the work is, and taking it is what makes a first check test the
/// commit a developer would recognise. origin/HEAD is the other option,
/// which keeps things as they are.
pub(super) fn base_proposal(root: &Path) -> Option<Proposal> {
    let drift = crate::worktree::base_drift(root)?;
    let candidates = vec![
        Candidate {
            value: drift.current.clone(),
            why: format!(
                "the main checkout's branch, {} commits ahead of {}",
                drift.ahead, drift.default
            ),
            ..Default::default()
        },
        Candidate {
            value: drift.default_branch.clone(),
            why: format!(
                "origin/HEAD, last committed {} days before {}",
                drift.days_older, drift.current
            ),
            ..Default::default()
        },
    ];
    Some(Proposal::of(Slot::Base, candidates, false))
}
