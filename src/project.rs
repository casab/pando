//! The repository pando is looking at, and the identity it derives from it.
//!
//! `discover` lands with the next work item; the reference type itself is
//! here first because `PandoPaths` is built from one.

use std::path::PathBuf;

/// A repository pando manages, identified by the canonical path of its main
/// checkout. Path-based so the id is stable across sessions and needs no
/// remote; a moved repository is a new project (known limitation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// `<display_name>-<8 hex chars of a hash of the canonical root path>`.
    pub id: String,
    /// Canonical path of the main checkout.
    pub root: PathBuf,
    /// The main checkout's directory name.
    pub display_name: String,
}
