//! pando: one repo, every branch alive.
//!
//! Modules land one per work item of the current phase plan; the dependency
//! direction is inner to outer with no upward imports:
//!
//! ```text
//! paths → project · config · ports · process · state
//!       → worktree · cache · log_tail
//!       → actions
//!       → cli · tui
//! ```

pub mod config;
pub mod paths;
pub mod ports;
pub mod process;
pub mod project;
#[cfg(test)]
pub(crate) mod testutil;
pub mod theme;
