//! A group: every process one spawn started.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Every process one spawn started, asked after and stopped as one.
///
/// Opaque above this layer: a group comes from a spawn, or from the state
/// file it was written to, never from a number. On Unix it is a process
/// group id, written to the state file as the bare number it always was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Group(i32);

impl Group {
    /// The group a raw id names. For this layer's backends, and for tests
    /// that describe a record; `tests.rs` keeps it out of everything else.
    pub fn from_raw(raw: i32) -> Group {
        Group(raw)
    }

    /// The raw id, for a backend to hand the OS.
    pub fn as_raw(self) -> i32 {
        self.0
    }
}

impl fmt::Display for Group {
    /// The bare id, so a message that names a group reads as it always did.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
