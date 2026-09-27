//! The README's pictures, drawn by the same code as the setup screen.
//!
//! `cargo run --example readme-art` writes `assets/pando.svg`: the
//! wordmark over the grove in a terminal window, its leaves quaking, a
//! shine crossing the letters, and the roots lighting up the way they do
//! when `pando check` passes. `cargo run --example readme-art -- text`
//! prints the same scene as plain text, for a fenced block.
//!
//! An SVG draws each cell as a rectangle rather than as a glyph, so the
//! picture does not depend on the reader's fonts having block elements.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use pando::art::{self, Cell, Material, WordmarkSize};

/// The scene, in cells.
const COLUMNS: usize = 76;
const GROVE_WIDTH: usize = 72;
const GROVE_HEIGHT: usize = 10;
const SEED: &str = "pando";

/// The branches written under the stems, as on the setup screen.
const STEMS: [&str; 7] = [
    "main",
    "feat/checkout",
    "fix/login-loop",
    "feat/search",
    "chore/deps",
    "feat/dark-mode",
    "pr-42/typo",
];

/// One cell in pixels: a terminal's cells are twice as tall as wide.
const CELL_W: f64 = 10.0;
const CELL_H: f64 = 20.0;
const TITLE_BAR: f64 = 34.0;

/// pando's own dark palette (`src/theme/builtin/pando.toml`).
const BACKGROUND: &str = "#16181f";
const SURFACE: &str = "#1e212d";
const BORDER: &str = "#373c50";
const TEXT: &str = "#d2d7e1";
const TEXT_DIM: &str = "#6e7387";
const TEXT_MUTED: &str = "#5c6174";
const YELLOW: &str = "#ebc34b";
const GREEN: &str = "#50c878";
const RED: &str = "#eb5a5a";

/// The leaves quake on the TUI's tick. Each leaf trembles on its own
/// phase, so a short loop reads as the same flicker as a long one.
const LEAF_FRAMES: u64 = 10;
const LEAF_TICK: f64 = 0.25;
/// The shine's first pass over the letters, a little faster than the
/// TUI draws it so a reader sees it before scrolling on.
const SHINE_FRAMES: u64 = 65;
const SHINE_TICK: f64 = 0.1;
/// The setup's own loop: waiting, the roots lighting left to right, and
/// alive for a while before it starts again.
const SETUP_LOOP: f64 = 10.0;
const LIGHT_FROM: f64 = 0.35;
const LIGHT_TO: f64 = 0.5;

/// Where each part of the scene starts, in rows.
const WORDMARK_ROW: usize = 1;
const CREDIT_ROW: usize = WORDMARK_ROW + art::WORDMARK_HEIGHT;
const GROVE_ROW: usize = CREDIT_ROW + 2;
const LABEL_ROW: usize = GROVE_ROW + GROVE_HEIGHT;
const STATUS_ROW: usize = LABEL_ROW + 2;
const ROWS: usize = STATUS_ROW + 2;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("text") => print!("{}", text()),
        _ => {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/pando.svg");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, svg()).unwrap();
            eprintln!("wrote {}", path.display());
        }
    }
}

/// A picture placed in the scene: its cells, and its top-left cell.
type Placed = BTreeMap<(usize, usize), Cell>;

fn place(cells: &[Vec<Cell>], left: usize, top: usize) -> Placed {
    let mut placed = Placed::new();
    for (y, row) in cells.iter().enumerate() {
        for (x, cell) in row.iter().enumerate() {
            if cell.material != Material::Sky {
                placed.insert((left + x, top + y), *cell);
            }
        }
    }
    placed
}

fn wordmark_left() -> usize {
    (COLUMNS - art::WORDMARK_WIDTH) / 2
}

fn grove_left() -> usize {
    (COLUMNS - GROVE_WIDTH) / 2
}

/// The branch names under their stems, as (column, name).
fn labels() -> Vec<(usize, String)> {
    let grove = art::grove(GROVE_WIDTH, GROVE_HEIGHT, 0, art::seed_of(SEED));
    let names: Vec<String> = STEMS.iter().map(|s| s.to_string()).collect();
    art::label_stems(&grove.stems, &names, GROVE_WIDTH)
        .into_iter()
        .map(|(x, name)| (grove_left() + x, name))
        .collect()
}

fn centred(text: &str) -> usize {
    (COLUMNS - text.chars().count()) / 2
}

/// The scene as plain text, still.
fn text() -> String {
    let mut scene = place(
        &art::wordmark(WordmarkSize::Big, 0),
        wordmark_left(),
        WORDMARK_ROW,
    );
    let grove = art::grove(GROVE_WIDTH, GROVE_HEIGHT, 0, art::seed_of(SEED));
    scene.extend(place(&grove.cells, grove_left(), GROVE_ROW));
    let mut rows = vec![vec![' '; COLUMNS]; LABEL_ROW + 1];
    for ((x, y), cell) in &scene {
        rows[*y][*x] = cell.ch;
    }
    let mut write = |row: usize, at: usize, text: &str| {
        for (i, ch) in text.chars().enumerate() {
            rows[row][at + i] = ch;
        }
    };
    let credit = art::credit();
    write(CREDIT_ROW, centred(&credit), &credit);
    for (x, name) in labels() {
        write(LABEL_ROW, x, &name);
    }
    rows.iter()
        .skip(WORDMARK_ROW)
        .map(|row| row.iter().collect::<String>().trim_end().to_string() + "\n")
        .collect()
}

/// The scene as an animated SVG.
fn svg() -> String {
    let width = COLUMNS as f64 * CELL_W;
    let height = TITLE_BAR + ROWS as f64 * CELL_H;
    let mut out = String::new();
    let _ = writeln!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="pando: the PANDO wordmark over a grove of aspens that share one root system">"#
    );
    let _ = writeln!(
        out,
        r#"<title>pando — one repo, every branch alive</title>
<defs>
  <linearGradient id="roots" gradientUnits="userSpaceOnUse" x1="{x1}" y1="0" x2="{x2}" y2="0">
    <stop offset="0" stop-color="{GREEN}">{glow}</stop>
    <stop offset="0" stop-color="{TEXT_DIM}">{glow}</stop>
  </linearGradient>
{patterns}  <clipPath id="window"><rect width="{width}" height="{height}" rx="10"/></clipPath>
</defs>
<g clip-path="url(#window)">
<rect width="{width}" height="{height}" fill="{BACKGROUND}"/>
<rect width="{width}" height="{TITLE_BAR}" fill="{SURFACE}"/>
<rect y="{TITLE_BAR}" width="{width}" height="1" fill="{BORDER}"/>
<circle cx="20" cy="17" r="6" fill="{RED}"/>
<circle cx="40" cy="17" r="6" fill="{YELLOW}"/>
<circle cx="60" cy="17" r="6" fill="{GREEN}"/>
<text x="{mid}" y="22" fill="{TEXT_DIM}" text-anchor="middle" {FONT}>pando — setup</text>
</g>
<rect x="0.5" y="0.5" width="{w1}" height="{h1}" rx="10" fill="none" stroke="{BORDER}"/>"#,
        x1 = grove_left() as f64 * CELL_W,
        x2 = (grove_left() + GROVE_WIDTH) as f64 * CELL_W,
        glow = glow(),
        patterns = patterns(),
        mid = width / 2.0,
        w1 = width - 1.0,
        h1 = height - 1.0,
    );

    // The wordmark: its letters shine one frame at a time, its shadow
    // is still.
    let shine: Vec<Placed> = (0..SHINE_FRAMES)
        .map(|frame| {
            place(
                &art::wordmark(WordmarkSize::Big, frame),
                wordmark_left(),
                WORDMARK_ROW,
            )
        })
        .collect();
    out.push_str(&animated(&shine, SHINE_TICK));

    // The grove: stems, ground and roots still, the leaves quaking.
    let seed = art::seed_of(SEED);
    let leaves: Vec<Placed> = (0..LEAF_FRAMES)
        .map(|frame| {
            let grove = art::grove(GROVE_WIDTH, GROVE_HEIGHT, frame, seed);
            place(&grove.cells, grove_left(), GROVE_ROW)
        })
        .collect();
    out.push_str(&animated(&leaves, LEAF_TICK));

    let credit = art::credit();
    let _ = writeln!(
        out,
        r#"<text x="{}" y="{}" fill="{TEXT_MUTED}" text-anchor="middle" {FONT}>{credit}</text>"#,
        width / 2.0,
        baseline(CREDIT_ROW),
    );
    for (i, (x, name)) in labels().into_iter().enumerate() {
        let (fill, weight) = match i {
            0 => (TEXT, r#" font-weight="bold""#),
            _ => (TEXT_DIM, ""),
        };
        let _ = writeln!(
            out,
            r#"<text x="{}" y="{}" fill="{fill}"{weight} {FONT}>{name}</text>"#,
            x as f64 * CELL_W,
            baseline(LABEL_ROW),
        );
    }

    // What the setup screen says while it waits, and once it is ready.
    let waiting = "◌ pando check · a throwaway worktree: install, start, ask for its page…";
    let ready = "✓ ready. every branch alive.";
    let _ = writeln!(
        out,
        r#"<text x="{mid}" y="{y}" fill="{TEXT_DIM}" text-anchor="middle" {FONT}>{waiting}<animate attributeName="opacity" values="1;0;1" keyTimes="0;{LIGHT_FROM};1" calcMode="discrete" dur="{SETUP_LOOP}s" repeatCount="indefinite"/></text>
<text x="{mid}" y="{y}" fill="{GREEN}" font-weight="bold" text-anchor="middle" opacity="0" {FONT}>{ready}<animate attributeName="opacity" values="0;1;0" keyTimes="0;{LIGHT_TO};1" calcMode="discrete" dur="{SETUP_LOOP}s" repeatCount="indefinite"/></text>"#,
        mid = width / 2.0,
        y = baseline(STATUS_ROW),
    );
    out.push_str("</svg>\n");
    out
}

const FONT: &str = r#"font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace" font-size="13""#;

fn baseline(row: usize) -> f64 {
    TITLE_BAR + row as f64 * CELL_H + CELL_H * 0.7
}

/// The shades `░▒▓` as the texture they are in a terminal font rather
/// than as flat tints: a 4×4 tile with one, two or three of its four
/// 2×2 squares filled.
fn patterns() -> String {
    let mut out = String::new();
    for fill in [YELLOW, TEXT, TEXT_MUTED] {
        for (percent, squares) in [
            (28, &[(0, 0)][..]),
            (52, &[(0, 0), (2, 2)]),
            (76, &[(0, 0), (2, 0), (2, 2)]),
        ] {
            let _ = write!(
                out,
                r#"  <pattern id="{}" width="4" height="4" patternUnits="userSpaceOnUse">"#,
                shade_id(fill, percent)
            );
            for (x, y) in squares {
                let _ = write!(
                    out,
                    r#"<rect x="{x}" y="{y}" width="2" height="2" fill="{fill}"/>"#
                );
            }
            out.push_str("</pattern>\n");
        }
    }
    out
}

fn shade_id(fill: &str, percent: u8) -> String {
    format!("shade-{}-{percent}", fill.trim_start_matches('#'))
}

/// The roots' gradient stops sweep from the left edge to the right one.
fn glow() -> String {
    format!(
        r#"<animate attributeName="offset" values="0;0;1;1" keyTimes="0;{LIGHT_FROM};{LIGHT_TO};1" dur="{SETUP_LOOP}s" repeatCount="indefinite"/>"#
    )
}

/// Frames of one picture: the first drawn whole and always shown, and
/// on each later frame's tick only the cells it changes, the old ones
/// wiped first.
fn animated(frames: &[Placed], tick: f64) -> String {
    let base = &frames[0];
    let mut out = draw(base, false);
    let n = frames.len();
    let dur = tick * n as f64;
    for (i, frame) in frames.iter().enumerate().skip(1) {
        let changed: Placed = base
            .keys()
            .chain(frame.keys())
            .filter(|at| base.get(at) != frame.get(at))
            .map(|at| (*at, frame.get(at).copied().unwrap_or(ERASED)))
            .collect();
        if changed.is_empty() {
            continue;
        }
        let at = |k: usize| format!("{:.4}", k as f64 / n as f64);
        let (values, times) = match i + 1 == n {
            true => ("0;1".to_string(), format!("0;{}", at(i))),
            false => ("0;1;0".to_string(), format!("0;{};{}", at(i), at(i + 1))),
        };
        let _ = writeln!(
            out,
            r#"<g opacity="0"><animate attributeName="opacity" values="{values}" keyTimes="{times}" calcMode="discrete" dur="{dur}s" repeatCount="indefinite"/>"#
        );
        out.push_str(&draw(&changed, true));
        out.push_str("</g>\n");
    }
    out
}

/// A cell a frame no longer has: drawn as the background.
const ERASED: Cell = Cell {
    ch: ' ',
    material: Material::Sky,
};

/// Cells as one path per colour and shade, with the background under
/// them first when they are drawn over an earlier frame.
fn draw(cells: &Placed, wipe: bool) -> String {
    let mut paths: BTreeMap<(&str, u8), String> = BTreeMap::new();
    for (&(x, y), &cell) in cells {
        let fill = match cell.material {
            Material::Sky => BACKGROUND,
            Material::Root => "url(#roots)",
            Material::Letter | Material::Canopy => YELLOW,
            Material::Trunk => TEXT,
            _ => TEXT_MUTED,
        };
        let (left, top) = (x as f64 * CELL_W, TITLE_BAR + y as f64 * CELL_H);
        let (mid_x, mid_y) = (left + CELL_W / 2.0, top + CELL_H / 2.0);
        if wipe {
            rect(
                paths.entry((BACKGROUND, 100)).or_default(),
                left,
                top,
                CELL_W,
                CELL_H,
            );
        }
        let shade = |percent: u8| (fill, percent);
        let (key, rects): ((&str, u8), Vec<[f64; 4]>) = match cell.ch {
            ' ' => continue,
            '░' => (shade(28), vec![[left, top, CELL_W, CELL_H]]),
            '▒' => (shade(52), vec![[left, top, CELL_W, CELL_H]]),
            '▓' => (shade(76), vec![[left, top, CELL_W, CELL_H]]),
            '█' => (shade(100), vec![[left, top, CELL_W, CELL_H]]),
            // The roots: heavy box-drawing lines, drawn at their weight.
            '━' => (shade(100), vec![[left, mid_y - 1.5, CELL_W, 3.0]]),
            '┃' => (shade(100), vec![[mid_x - 1.5, top, 3.0, CELL_H]]),
            '┻' => (
                shade(100),
                vec![
                    [left, mid_y - 1.5, CELL_W, 3.0],
                    [mid_x - 1.5, top, 3.0, CELL_H / 2.0],
                ],
            ),
            '╺' => (shade(100), vec![[mid_x, mid_y - 1.5, CELL_W / 2.0, 3.0]]),
            '╸' => (shade(100), vec![[left, mid_y - 1.5, CELL_W / 2.0, 3.0]]),
            other => panic!("no drawing for {other:?}"),
        };
        let path = paths.entry(key).or_default();
        for [x, y, w, h] in rects {
            rect(path, x, y, w, h);
        }
    }
    // The wipe goes under everything else drawn on the same tick.
    let mut order: Vec<_> = paths.into_iter().collect();
    order.sort_by_key(|((fill, _), _)| *fill != BACKGROUND);
    let mut out = String::new();
    for ((fill, percent), d) in order {
        let _ = match percent {
            100 => writeln!(out, r#"<path fill="{fill}" d="{d}"/>"#),
            _ => writeln!(
                out,
                r#"<path fill="url(#{})" d="{d}"/>"#,
                shade_id(fill, percent)
            ),
        };
    }
    out
}

/// A rectangle as path commands.
fn rect(path: &mut String, x: f64, y: f64, w: f64, h: f64) {
    let _ = write!(path, "M{x} {y}h{w}v{h}h-{w}z");
}
