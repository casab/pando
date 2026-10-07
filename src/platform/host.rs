//! What pando runs on: the OS it was built for.

/// The operating system pando was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
}

impl Os {
    /// The OS this build is for.
    #[cfg(target_os = "macos")]
    pub const HERE: Os = Os::MacOs;
    /// The OS this build is for.
    #[cfg(target_os = "linux")]
    pub const HERE: Os = Os::Linux;
    /// The OS this build is for.
    #[cfg(windows)]
    pub const HERE: Os = Os::Windows;
}

/// The machine pando runs on, as far as anything above this layer may
/// know it. Today that is the OS of the build alone; a fact pando reads
/// about the machine at run time joins it as a field of its own.
///
/// A field rather than a `cfg`, so a decision that depends on the OS is
/// made from a value a test can choose, and both of its branches run on
/// every CI runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub os: Os,
}

impl Host {
    /// This machine. A test sees the OS it was built for, as the
    /// developer's machine does: one built on macOS asks the macOS row of
    /// the desktop table.
    ///
    /// Read only where pando meets the outside: the TUI's launch
    /// environment, `pando open`, and the theme. Everything below them is
    /// handed a `&Host`, which `tests.rs` holds them to.
    pub fn here() -> &'static Host {
        static HERE: Host = Host { os: Os::HERE };
        &HERE
    }
}

impl Default for Host {
    /// This build's OS.
    fn default() -> Host {
        Host { os: Os::HERE }
    }
}
