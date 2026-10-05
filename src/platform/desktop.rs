//! What each desktop opens a URL with, copies with, and says about dark
//! mode: one row each.

use super::host::{Host, Os};

/// A desktop pando knows how to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    MacOs,
    Linux,
}

impl Desktop {
    /// The desktop `host` has.
    pub fn of(host: &Host) -> Desktop {
        match host.os {
            Os::MacOs => Desktop::MacOs,
            Os::Linux => Desktop::Linux,
        }
    }
}

/// Everything pando asks of one desktop.
#[derive(Debug)]
pub struct DesktopRow {
    pub desktop: Desktop,
    /// The programs that open a URL, tried in order until one succeeds,
    /// each with the URL as its last argument. One argument: an `&` in the
    /// URL is never read by a shell.
    pub open_url: &'static [&'static [&'static str]],
    /// What sets the clipboard for a terminal that ignores OSC 52, which is
    /// written first wherever pando runs.
    pub clipboard: Option<Clipboard>,
    /// A program that says whether the system is in dark mode.
    pub dark_mode: Option<DarkModeProbe>,
    /// Whether a phone simulator may be started here: where Xcode is.
    pub starts_simulators: bool,
    /// The shell `!` opens when `$SHELL` is unset.
    pub shell: &'static str,
}

/// A program that takes text on its stdin and puts it on the clipboard.
#[derive(Debug)]
pub struct Clipboard {
    pub program: &'static str,
    /// It reads only ASCII right: other text is left to OSC 52 alone,
    /// rather than landing mangled over the copy that got it right.
    pub ascii_only: bool,
}

/// A program whose output names dark mode, and the word it names it with.
/// A run that fails, or says something else, is light mode.
#[derive(Debug)]
pub struct DarkModeProbe {
    pub argv: &'static [&'static str],
    pub dark: &'static str,
}

pub const DESKTOPS: [DesktopRow; 2] = [
    DesktopRow {
        desktop: Desktop::MacOs,
        open_url: &[&["open"]],
        clipboard: Some(Clipboard {
            program: "pbcopy",
            ascii_only: false,
        }),
        // `AppleInterfaceStyle` is `Dark` in dark mode and has no value in
        // light mode, which `defaults` reports as a failure.
        dark_mode: Some(DarkModeProbe {
            argv: &["defaults", "read", "-g", "AppleInterfaceStyle"],
            dark: "Dark",
        }),
        starts_simulators: true,
        shell: "/bin/sh",
    },
    DesktopRow {
        desktop: Desktop::Linux,
        open_url: &[&["xdg-open"]],
        clipboard: None,
        dark_mode: None,
        starts_simulators: false,
        shell: "/bin/sh",
    },
];

/// The row of the desktop `host` has.
pub fn row(host: &Host) -> &'static DesktopRow {
    let desktop = Desktop::of(host);
    DESKTOPS
        .iter()
        .find(|row| row.desktop == desktop)
        .expect("every desktop has a row")
}

/// The commands that open `url`, in the order to try them.
pub fn url_openers(host: &Host, url: &str) -> Vec<Vec<String>> {
    row(host)
        .open_url
        .iter()
        .map(|words| {
            words
                .iter()
                .map(|word| word.to_string())
                .chain([url.to_string()])
                .collect()
        })
        .collect()
}

/// The program that puts `text` on the clipboard here, when there is one
/// that reads it right.
pub fn clipboard(host: &Host, text: &str) -> Option<&'static str> {
    let clipboard = row(host).clipboard.as_ref()?;
    (!clipboard.ascii_only || text.is_ascii()).then_some(clipboard.program)
}

/// Whether the system is in dark mode: `None` where nothing says, or the
/// program that would could not be run.
///
/// It runs a program, so it blocks for a moment: call it at startup or on
/// a watcher, never from a key handler.
pub fn is_dark(host: &Host) -> Option<bool> {
    let probe = row(host).dark_mode.as_ref()?;
    let (program, args) = probe.argv.split_first()?;
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(output.status.success() && String::from_utf8_lossy(&output.stdout).contains(probe.dark))
}

/// Whether a phone simulator may be started on `host`.
pub fn starts_simulators(host: &Host) -> bool {
    row(host).starts_simulators
}

/// The shell to open when the developer's `$SHELL` says nothing.
pub fn fallback_shell(host: &Host) -> &'static str {
    row(host).shell
}
