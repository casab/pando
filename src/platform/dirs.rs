//! Where the OS keeps a user's things: the home directory.

use std::path::PathBuf;

/// The developer's home directory, as the OS environment names it: `$HOME`
/// as it is set, unfiltered. `None` when it is not set. Every caller keeps
/// its own fallback, because each means something different without one.
pub fn home() -> Option<PathBuf> {
    imp::home()
}

#[cfg(unix)]
mod imp {
    use std::path::PathBuf;

    pub(super) fn home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    pub(super) fn home() -> Option<PathBuf> {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}
