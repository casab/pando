//! The grove, for a terminal: the TUI's first-run picture, drawn on
//! stderr in ANSI colour for the moments the CLI has a person's attention
//! for the first time — the first-time tip, and a check that passed.

use crate::grove::{self, Material};
use crate::term::{Paint, Style};

/// The widest and tallest the CLI draws it: a banner, not a screen.
const WIDTH: usize = 64;
const HEIGHT: usize = 7;

/// The grove's rows for a terminal `columns` wide, indented two cells,
/// each painted by `style` — plain text when colour is off. Empty when
/// the terminal is too narrow for a grove to read.
///
/// `alive` is the grove after a check passed: green leaves, and the roots
/// lit. Before that the leaves are gold and the roots dark, as on the
/// setup screen.
pub(super) fn grove_lines(columns: usize, seed: u64, alive: bool, style: &Style) -> Vec<String> {
    let width = columns.saturating_sub(4).min(WIDTH);
    grove::grove(width, HEIGHT, 0, seed)
        .iter()
        .map(|row| {
            let mut line = String::from("  ");
            let mut run = String::new();
            let mut paint = None;
            for cell in row {
                let next = paint_of(cell, alive);
                if next != paint && !run.is_empty() {
                    line.push_str(&painted(&run, paint, style));
                    run.clear();
                }
                paint = next;
                run.push(cell.ch);
            }
            line.push_str(&painted(&run, paint, style));
            line.trim_end().to_string()
        })
        .collect()
}

fn painted(text: &str, paint: Option<Paint>, style: &Style) -> String {
    match paint {
        Some(paint) => style.paint(text, paint),
        None => text.to_string(),
    }
}

/// A cell's paint: the sixteen-colour cousin of the TUI's grove colours.
fn paint_of(cell: &grove::Cell, alive: bool) -> Option<Paint> {
    match (cell.material, alive) {
        (Material::Sky, _) => None,
        (Material::Canopy, false) => Some(Paint::Warn),
        (Material::Canopy, true) if cell.ch == '█' => Some(Paint::Warn),
        (Material::Canopy, true) => Some(Paint::Good),
        (Material::Trunk, _) => Some(Paint::Heading),
        (Material::Eye | Material::Ground, _) => Some(Paint::Faint),
        (Material::Root, false) => Some(Paint::Faint),
        (Material::Root, true) => Some(Paint::Warn),
    }
}

/// How wide stderr's terminal is: `COLUMNS` when it says, else what the
/// terminal reports, else 80.
pub(super) fn stderr_columns() -> usize {
    use std::io::IsTerminal;
    super::ls::width_from(
        std::io::stderr().is_terminal(),
        std::env::var("COLUMNS").ok().as_deref(),
        crossterm::terminal::size().ok().map(|(cols, _)| cols),
    )
}
