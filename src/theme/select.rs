//! Which theme applies, and whether its dark or light half: the
//! environment, a file another tool keeps, the config, then the default.

use std::path::{Path, PathBuf};

use super::catalog::DEFAULT_THEME;

/// Names a theme for one run, over everything else.
pub const THEME_ENV: &str = "PANDO_THEME";

/// `dark` or `light` for one run, over the config and the system.
pub const APPEARANCE_ENV: &str = "PANDO_APPEARANCE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

impl Appearance {
    pub fn parse(text: &str) -> Option<Appearance> {
        match text.trim() {
            "dark" => Some(Appearance::Dark),
            "light" => Some(Appearance::Light),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        }
    }
}

/// What the config says about themes: the `[ui]` section's three keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub theme: Option<String>,
    /// A file whose first line names the theme, kept by another tool — a
    /// terminal theme switcher — and followed while pando runs.
    pub theme_from: Option<PathBuf>,
    /// `dark`, `light`, or `auto` (the default), which asks the system.
    pub appearance: Option<String>,
}

/// Where the theme's name was found, for the picker to say what it would
/// be overriding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Env,
    File(PathBuf),
    Config,
    Default,
}

/// The theme name that applies, and where it came from. Reads the
/// environment and one small file, so it is safe on any thread but meant
/// for a watcher's.
pub fn choose(settings: &Settings) -> (String, Origin) {
    if let Ok(name) = std::env::var(THEME_ENV)
        && !name.trim().is_empty()
    {
        return (name.trim().to_string(), Origin::Env);
    }
    if let Some(path) = &settings.theme_from
        && let Some(name) = first_line(path)
    {
        return (name, Origin::File(path.clone()));
    }
    if let Some(name) = settings.theme.as_deref().map(str::trim)
        && !name.is_empty()
    {
        return (name.to_string(), Origin::Config);
    }
    (DEFAULT_THEME.to_string(), Origin::Default)
}

fn first_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// What decided dark or light, for the picker to say whether the system
/// did or something pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceOrigin {
    Env,
    Config,
    System,
}

/// Dark or light, and what decided it: the environment, then the config,
/// then the system. The system is asked by running a program, so this
/// blocks for a moment — call it at startup or on a watcher, never from a
/// key handler.
pub fn appearance(settings: &Settings) -> (Appearance, AppearanceOrigin) {
    if let Some(pinned) = std::env::var(APPEARANCE_ENV)
        .ok()
        .and_then(|v| Appearance::parse(&v))
    {
        return (pinned, AppearanceOrigin::Env);
    }
    if let Some(pinned) = settings.appearance.as_deref().and_then(Appearance::parse) {
        return (pinned, AppearanceOrigin::Config);
    }
    (system_appearance(), AppearanceOrigin::System)
}

/// macOS says `Dark` for `AppleInterfaceStyle` in dark mode and has no
/// value in light mode. Anything that cannot answer — another system, a
/// failed spawn — is dark, which is what most terminals are.
fn system_appearance() -> Appearance {
    if !cfg!(target_os = "macos") {
        return Appearance::Dark;
    }
    let light = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| !(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Dark")))
        .unwrap_or(false);
    if light {
        Appearance::Light
    } else {
        Appearance::Dark
    }
}
