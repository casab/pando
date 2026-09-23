//! Light/dark UI palette. Each color is a function (not a const) so every
//! call site follows the appearance detected once at startup — log lines are
//! colorized at ingest and cached, so a mid-session appearance change only
//! takes effect on the next launch.

use ratatui::style::Color;
use std::sync::OnceLock;

static IS_LIGHT: OnceLock<bool> = OnceLock::new();

fn is_light() -> bool {
    *IS_LIGHT.get_or_init(|| match std::env::var("PANDO_APPEARANCE").ok().as_deref() {
        Some("light") => true,
        Some("dark") => false,
        _ => macos_appearance_is_light(),
    })
}

/// The system appearance is the same signal Ghostty (`light:`/`dark:` theme
/// pair) and tmux (`auto-appearance.sh`) follow, so it matches what the
/// terminal actually shows. `defaults read -g AppleInterfaceStyle` prints
/// "Dark" in dark mode and exits nonzero in light mode; any spawn failure
/// falls back to the dark palette (the pre-theme behavior).
fn macos_appearance_is_light() -> bool {
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .map(|o| !(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("Dark")))
        .unwrap_or(false)
}

fn pick(dark: Color, light: Color) -> Color {
    if is_light() { light } else { dark }
}

pub fn blue() -> Color {
    pick(Color::Rgb(110, 155, 235), Color::Rgb(50, 95, 205))
}

pub fn orange() -> Color {
    pick(Color::Rgb(230, 150, 60), Color::Rgb(185, 95, 10))
}

pub fn magenta() -> Color {
    pick(Color::Rgb(180, 130, 230), Color::Rgb(140, 60, 200))
}

pub fn green() -> Color {
    pick(Color::Rgb(80, 200, 120), Color::Rgb(25, 135, 70))
}

pub fn red() -> Color {
    pick(Color::Rgb(235, 90, 90), Color::Rgb(200, 45, 45))
}

pub fn yellow() -> Color {
    pick(Color::Rgb(235, 195, 75), Color::Rgb(160, 115, 0))
}

pub fn cyan() -> Color {
    pick(Color::Rgb(95, 210, 210), Color::Rgb(10, 125, 140))
}

pub fn text() -> Color {
    pick(Color::Rgb(210, 215, 225), Color::Rgb(35, 40, 55))
}

pub fn text_dim() -> Color {
    pick(Color::Rgb(110, 115, 135), Color::Rgb(105, 110, 130))
}

/// The faintest text, for labels and key hints — still text, so it has to
/// stay readable on `surface()`, not only on the terminal's background.
pub fn text_muted() -> Color {
    pick(Color::Rgb(92, 97, 116), Color::Rgb(130, 135, 155))
}

pub fn surface() -> Color {
    pick(Color::Rgb(30, 33, 45), Color::Rgb(232, 234, 242))
}

pub fn highlight_bg() -> Color {
    pick(Color::Rgb(45, 50, 70), Color::Rgb(213, 220, 240))
}

pub fn border() -> Color {
    pick(Color::Rgb(55, 60, 80), Color::Rgb(196, 201, 216))
}

pub fn search_cursor_bg() -> Color {
    pick(Color::Rgb(230, 150, 60), Color::Rgb(245, 190, 110))
}

pub fn search_match_bg() -> Color {
    pick(Color::Rgb(80, 80, 130), Color::Rgb(205, 208, 245))
}
