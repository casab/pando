//! The one layer that talks to the operating system.
//!
//! Everything pando asks of the OS goes through here: starting process
//! groups and stopping them, locks and permission bits, which boot this
//! is, the shell a command string runs in, what a desktop opens a URL
//! with. Nothing above this layer names an OS, and `tests.rs` holds every
//! other module to that.
//!
//! One file per concern. Each is a facade whose functions carry the
//! contract every backend keeps, and call `imp`: an inline
//! `#[cfg(…)] mod imp` when the backend is short, a file per OS when it is
//! long. A difference between macOS and Linux inside a Unix backend stays
//! a small `#[cfg(target_os)]` item; a pure parser is compiled on every OS
//! so its tests run everywhere.
//!
//! What the OS is, and what pando finds at run time, is [`Host`]: read once
//! for this machine, or from files under a root a test chooses.

pub mod cow;
pub mod desktop;
pub mod dirs;
pub mod files;
pub mod host;
pub mod shell;

pub use host::{Host, Os};

#[cfg(test)]
mod tests;
