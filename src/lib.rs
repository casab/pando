//! pando: one repo, every branch alive.
//!
//! Modules land one per work item of the current phase plan; the dependency
//! direction is inner to outer with no upward imports:
//!
//! ```text
//! catalog · paths → compose → project · config · ports · process · runtime · state
//!       · template · recipes
//!       → detect · hooks · worktree · cache · log_tail · observe · services
//!         · native · decisions
//!       → tunnel · share_proxy
//!       → actions
//!       → doctor
//!       → cli · tui
//! ```

pub mod actions;
pub mod cache;
pub mod catalog;
pub mod cli;
pub mod compose;
pub mod config;
pub mod decisions;
pub mod detect;
pub mod doctor;
pub mod hooks;
pub mod log_tail;
pub mod native;
pub mod observe;
pub mod paths;
pub mod ports;
pub mod process;
pub mod project;
pub mod recipes;
pub mod runtime;
pub mod services;
pub mod share_proxy;
pub mod state;
pub mod template;
#[cfg(test)]
pub(crate) mod testutil;
pub mod theme;
pub mod tui;
pub mod tunnel;
pub mod worktree;
