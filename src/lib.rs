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

pub mod actions;
pub mod cache;
pub mod cli;
pub mod config;
pub mod log_tail;
pub mod paths;
pub mod ports;
pub mod process;
pub mod project;
pub mod state;
#[cfg(test)]
pub(crate) mod testutil;
pub mod theme;
pub mod tui;
pub mod worktree;
