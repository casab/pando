//! What pando knows about the ecosystem, written down once.
//!
//! Detection proposes from these facts, `doctor` checks against them, and
//! hooks fingerprint with them. Each fact lives in exactly one table here,
//! so teaching pando about a new package manager is one row, and there is
//! no second list somewhere else that silently disagrees with the first.
//!
//! Nothing in this module reads the disk or runs anything. It is data plus
//! the lookups over it; the modules that act on the data stay where they
//! are.

pub mod frameworks;
pub mod package_managers;
