//! Which themes there are: the ones compiled in, and the developer's own
//! files, which replace a built-in of the same name.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::palette::{Palette, Variant};
use super::select::Appearance;

/// The theme a fresh install paints with, and the one anything that cannot
/// be found falls back to.
pub const DEFAULT_THEME: &str = "pando";

/// Every built-in theme, by name, in the order the picker lists them: the
/// default first, the rest by name. Adding one is a TOML file in
/// `theme/builtin/` and a row here.
pub const BUILT_IN: &[(&str, &str)] = &[
    ("pando", include_str!("builtin/pando.toml")),
    ("catppuccin", include_str!("builtin/catppuccin.toml")),
    ("flexoki", include_str!("builtin/flexoki.toml")),
    ("github", include_str!("builtin/github.toml")),
    (
        "github-colorblind",
        include_str!("builtin/github-colorblind.toml"),
    ),
    ("github-dimmed", include_str!("builtin/github-dimmed.toml")),
    (
        "github-high-contrast",
        include_str!("builtin/github-high-contrast.toml"),
    ),
    ("gruvbox", include_str!("builtin/gruvbox.toml")),
    ("kanagawa", include_str!("builtin/kanagawa.toml")),
    ("monokai", include_str!("builtin/monokai.toml")),
    ("onedark", include_str!("builtin/onedark.toml")),
    ("rose-pine", include_str!("builtin/rose-pine.toml")),
    ("tokyonight", include_str!("builtin/tokyonight.toml")),
    ("vscode", include_str!("builtin/vscode.toml")),
    ("zenwritten", include_str!("builtin/zenwritten.toml")),
];

/// Where a theme came from, so the picker can say which ones are yours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    BuiltIn,
    File(PathBuf),
}

/// A theme file as written: a line about it, and its two appearances.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    #[serde(default)]
    description: String,
    dark: Variant,
    light: Variant,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub description: String,
    pub dark: Palette,
    pub light: Palette,
    pub source: Source,
}

impl Theme {
    pub fn parse(name: &str, text: &str, source: Source) -> Result<Theme, String> {
        let file: ThemeFile = toml::from_str(text).map_err(|e| e.to_string())?;
        Ok(Theme {
            name: name.to_string(),
            description: file.description,
            dark: file.dark.palette(),
            light: file.light.palette(),
            source,
        })
    }

    pub fn palette(&self, appearance: Appearance) -> Palette {
        match appearance {
            Appearance::Dark => self.dark,
            Appearance::Light => self.light,
        }
    }
}

/// The built-ins, then every `*.toml` in `dir`, which replaces a built-in
/// whose name it shares and is added after them otherwise. A file that does
/// not parse is left out with a line saying why, rather than taking every
/// other theme down with it.
pub fn themes(dir: Option<&Path>) -> (Vec<Theme>, Vec<String>) {
    let mut themes: Vec<Theme> = BUILT_IN
        .iter()
        .map(|(name, text)| {
            Theme::parse(name, text, Source::BuiltIn)
                .unwrap_or_else(|e| panic!("built-in theme {name} does not parse: {e}"))
        })
        .collect();
    let mut warnings = Vec::new();
    let mut files: Vec<PathBuf> = dir
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    for path in files {
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| Theme::parse(&name, &text, Source::File(path.clone())));
        match parsed {
            Ok(theme) => match themes.iter_mut().find(|t| t.name == name) {
                Some(slot) => *slot = theme,
                None => themes.push(theme),
            },
            Err(e) => warnings.push(format!(
                "theme {} is left out: {}",
                path.display(),
                e.lines().next().unwrap_or_default()
            )),
        }
    }
    (themes, warnings)
}

/// The theme called `name`, if there is one.
pub fn find<'a>(themes: &'a [Theme], name: &str) -> Option<&'a Theme> {
    themes.iter().find(|t| t.name == name)
}
