//! The PANDO wordmark: solid block letters over a dithered drop shadow,
//! and a slow shine across them like sun through aspen leaves.

use super::{Cell, Material, dither};

/// Which wordmark: the tall one, or two rows of half blocks for a screen
/// without the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordmarkSize {
    Big,
    Compact,
}

/// The big wordmark's size: five letters of five pixels, each pixel two
/// cells wide so the letters keep their proportions in a terminal's tall
/// cells, two cells between letters, and a shadow one cell right and one
/// row down.
pub const WORDMARK_WIDTH: usize = 5 * 10 + 4 * 2 + 1;
pub const WORDMARK_HEIGHT: usize = 5 + 1;

/// The compact wordmark, in half blocks.
const COMPACT: [&str; 2] = ["█▀█ ▄▀█ █▄ █ █▀▄ █▀█", "█▀▀ █▀█ █ ▀█ █▄▀ █▄█"];
pub const COMPACT_WIDTH: usize = 20;

/// P, A, N, D, O, five pixels square.
const LETTERS: [[&str; 5]; 5] = [
    ["#####", "#...#", "#####", "#....", "#...."],
    [".###.", "#...#", "#####", "#...#", "#...#"],
    ["#...#", "##..#", "#.#.#", "#..##", "#...#"],
    ["####.", "#...#", "#...#", "#...#", "####."],
    [".###.", "#...#", "#...#", "#...#", ".###."],
];

/// The wordmark on animation frame `frame`: rows top to bottom.
pub fn wordmark(size: WordmarkSize, frame: u64) -> Vec<Vec<Cell>> {
    match size {
        WordmarkSize::Compact => COMPACT
            .iter()
            .map(|row| {
                row.chars()
                    .map(|ch| match ch {
                        ' ' => Cell::SKY,
                        ch => Cell::new(ch, Material::Letter),
                    })
                    .collect()
            })
            .collect(),
        WordmarkSize::Big => big(frame),
    }
}

/// Whether the big wordmark's letters cover cell `(x, y)`.
fn inked(x: usize, y: usize) -> bool {
    if y >= 5 {
        return false;
    }
    let letter = x / 12;
    let within = x % 12;
    if letter >= LETTERS.len() || within >= 10 {
        return false;
    }
    LETTERS[letter][y].as_bytes()[within / 2] == b'#'
}

fn big(frame: u64) -> Vec<Vec<Cell>> {
    // Where the shine is on this frame, along the diagonal: it crosses
    // the letters, then waits off to the side before it comes round.
    let lap = (WORDMARK_WIDTH + 70) as u64;
    let shine = ((frame * 2) % lap) as f64 - 12.0;
    (0..WORDMARK_HEIGHT)
        .map(|y| {
            (0..WORDMARK_WIDTH)
                .map(|x| {
                    if inked(x, y) {
                        // Solid, but where the shine crosses.
                        let along = x as f64 + (y as f64) * 2.0;
                        let ch = match (along - shine).abs() < 2.0 {
                            true => '▓',
                            false => '█',
                        };
                        return Cell::new(ch, Material::Letter);
                    }
                    // The shadow is where the dither shows: a Bayer mix
                    // of the two lightest shades.
                    if x > 0 && y > 0 && inked(x - 1, y - 1) {
                        let level = dither(0.37, x, y).max(1);
                        return Cell::new(super::RAMP[level], Material::Shadow);
                    }
                    Cell::SKY
                })
                .collect()
        })
        .collect()
}
