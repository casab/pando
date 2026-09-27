//! The first run's pictures, for a terminal: the PANDO wordmark over the
//! grove, drawn on stderr in ANSI colour for the moments the CLI has a
//! person's attention for the first time — the first-time tip, and a
//! check that passed.

use crate::art::{self, Material, WordmarkSize};
use crate::term::{Paint, Style};

/// The widest and tallest the CLI draws the grove: a banner, not a
/// screen.
const GROVE_WIDTH: usize = 64;
const GROVE_HEIGHT: usize = 8;

/// The banner's rows for a terminal `columns` wide, indented two cells,
/// each painted by `style` — plain text when colour is off: the
/// wordmark, big where it fits and compact where it does not, who made
/// pando under it, a blank row, and the grove. Empty when the terminal is too narrow for either.
///
/// `alive` is the grove after a check passed: its roots lit. Before that
/// they are dark, as on the setup screen; the leaves are gold either way.
pub(super) fn banner_lines(columns: usize, seed: u64, alive: bool, style: &Style) -> Vec<String> {
    let room = columns.saturating_sub(4);
    let mark = if room >= art::WORDMARK_WIDTH {
        art::wordmark(WordmarkSize::Big, 0)
    } else if room >= art::COMPACT_WIDTH {
        art::wordmark(WordmarkSize::Compact, 0)
    } else {
        Vec::new()
    };
    let grove = art::grove(room.min(GROVE_WIDTH), GROVE_HEIGHT, 0, seed);
    let mut lines: Vec<String> = mark.iter().map(|row| line(row, alive, style)).collect();
    if !mark.is_empty() {
        lines.push(format!("  {}", style.paint(&art::credit(), Paint::Faint)));
    }
    if !mark.is_empty() && !grove.is_empty() {
        lines.push(String::new());
    }
    lines.extend(grove.cells.iter().map(|row| line(row, alive, style)));
    lines
}

/// One row, two cells in, a run of one paint at a time.
fn line(row: &[art::Cell], alive: bool, style: &Style) -> String {
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
}

fn painted(text: &str, paint: Option<Paint>, style: &Style) -> String {
    match paint {
        Some(paint) => style.paint(text, paint),
        None => text.to_string(),
    }
}

/// A cell's paint: the sixteen-colour cousin of the TUI's colours.
fn paint_of(cell: &art::Cell, alive: bool) -> Option<Paint> {
    match (cell.material, alive) {
        (Material::Sky, _) => None,
        (Material::Canopy | Material::Letter, _) => Some(Paint::Warn),
        (Material::Trunk, _) => Some(Paint::Heading),
        (Material::Eye | Material::Ground | Material::Shadow, _) => Some(Paint::Faint),
        (Material::Root, false) => Some(Paint::Faint),
        (Material::Root, true) => Some(Paint::Good),
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
