//! The grove a first run opens on: Pando itself, drawn in dithered blocks.
//!
//! Pando is one quaking aspen — some 47,000 white stems over a single root
//! system — and pando is named for it: one repository, every branch alive.
//! So the picture is a row of slender aspen trunks, with the dark "eyes"
//! their bark has, under a canopy of leaves that quakes from frame to
//! frame, standing on the ground over one root system that joins them all.
//!
//! Shaded by ordered dithering: each cell's intensity is compared with a
//! 4×4 Bayer threshold and lands on one of `░▒▓█`, so gradients read as
//! texture rather than bands, in any font that has the block elements.
//!
//! Pure: the same size, frame and seed give the same cells, and only the
//! canopy moves between frames. The TUI and the CLI colour the cells by
//! their [`Material`], each from its own palette.

/// What a cell of the picture is, which decides its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Material {
    /// Nothing: the space around the grove.
    Sky,
    /// The leaves, which quake.
    Canopy,
    /// A white stem.
    Trunk,
    /// One of the dark marks on aspen bark.
    Eye,
    /// The ground line the stems stand on.
    Ground,
    /// The one root system under all of them.
    Root,
}

/// One cell: the character drawn and what it is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub material: Material,
}

impl Cell {
    const SKY: Cell = Cell {
        ch: ' ',
        material: Material::Sky,
    };
}

/// The shades a dithered cell can take, from nothing to solid.
pub const RAMP: [char; 5] = [' ', '░', '▒', '▓', '█'];

/// The smallest picture worth drawing: below it the trunks and the roots
/// have no room to read as a grove, so nothing is drawn at all.
pub const MIN_WIDTH: usize = 24;
pub const MIN_HEIGHT: usize = 6;

/// The 4×4 Bayer matrix: the order in which cells of a block switch on as
/// the shade rises, spread so no two neighbours switch together.
const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// A seed from a name, so each project has a grove of its own and the
/// same grove every time.
pub fn seed_of(name: &str) -> u64 {
    // FNV-1a: stable across runs and builds, unlike the std hasher.
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The grove at `width` × `height` cells on animation frame `frame`, rows
/// top to bottom. Empty below [`MIN_WIDTH`] × [`MIN_HEIGHT`].
pub fn grove(width: usize, height: usize, frame: u64, seed: u64) -> Vec<Vec<Cell>> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return Vec::new();
    }
    let scene = Scene::new(width, height, seed);
    (0..height)
        .map(|y| (0..width).map(|x| scene.cell(x, y, frame)).collect())
        .collect()
}

/// A picture as plain text, one line per row with trailing spaces kept:
/// for tests, and for anything that wants the shape without the colour.
pub fn to_text(cells: &[Vec<Cell>]) -> String {
    cells
        .iter()
        .map(|row| row.iter().map(|c| c.ch).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// One stem: where it stands, and where its crown is.
struct Trunk {
    x: usize,
    /// The row its crown is centred on; the stem shows from there down.
    crown_y: f64,
    /// How far its crown reaches either side, and up and down.
    crown_rx: f64,
    crown_ry: f64,
}

struct Scene {
    seed: u64,
    trunks: Vec<Trunk>,
    /// The row the stems stand on.
    ground: usize,
    /// How many rows of roots are under it.
    roots: usize,
    /// The lowest row a crown reaches.
    canopy_floor: usize,
}

impl Scene {
    fn new(width: usize, height: usize, seed: u64) -> Scene {
        let roots = if height >= 9 { 2 } else { 1 };
        let ground = height - roots - 1;
        let canopy_floor = ((ground as f64) * 0.62).round().max(2.0) as usize;
        let mut rng = Rng(seed ^ 0x9e37_79b9_7f4a_7c15);
        let mut trunks = Vec::new();
        // Stems four to seven cells apart, starting a little way in and
        // stopping before the edge, so the crowns are not cut in half.
        let mut x = 1 + (rng.next() % 3) as usize;
        while x + 2 < width {
            // Crowns of their own, at heights of their own: a grove, not
            // a hedge.
            let crown_y = 1.2 + rng.unit() * (canopy_floor as f64 * 0.55);
            trunks.push(Trunk {
                x,
                crown_y,
                crown_rx: 2.0 + rng.unit() * 1.5,
                crown_ry: (canopy_floor as f64) * (0.38 + rng.unit() * 0.22),
            });
            x += 4 + (rng.next() % 4) as usize;
        }
        Scene {
            seed,
            trunks,
            ground,
            roots,
            canopy_floor,
        }
    }

    fn cell(&self, x: usize, y: usize, frame: u64) -> Cell {
        if y > self.ground {
            return self.root(x, y);
        }
        if y == self.ground {
            return self.ground_cell(x);
        }
        let stem = self.trunks.iter().find(|t| t.x == x);
        // A stem shows below its crown's centre, and through the canopy
        // where the leaves are thin — judged on the still canopy, so a
        // stem never flickers with the quake.
        if let Some(t) = stem
            && (y as f64) >= t.crown_y
            && (self.canopy(x, y, None) < 0.55 || y >= self.canopy_floor)
        {
            // An eye every few rows, in the same places every frame.
            return match hash01(self.seed, x as u64, y as u64 + 7) < 0.22 {
                true => Cell {
                    ch: '▓',
                    material: Material::Eye,
                },
                false => Cell {
                    ch: '█',
                    material: Material::Trunk,
                },
            };
        }
        match dither(self.canopy(x, y, Some(frame)), x, y) {
            0 => Cell::SKY,
            level => Cell {
                ch: RAMP[level],
                material: Material::Canopy,
            },
        }
    }

    /// How dense the leaves are at a cell, 0 to 1: quaking on `frame`, or
    /// still with none.
    fn canopy(&self, x: usize, y: usize, frame: Option<u64>) -> f64 {
        if y > self.canopy_floor {
            return 0.0;
        }
        let (fx, fy) = (x as f64, y as f64);
        let crowns = self
            .trunks
            .iter()
            .map(|t| {
                let dx = (fx - t.x as f64) / t.crown_rx;
                let dy = (fy - t.crown_y) / t.crown_ry;
                1.0 - (dx * dx + dy * dy)
            })
            .fold(0.0_f64, f64::max);
        if crowns <= 0.0 {
            return 0.0;
        }
        // Clumps of leaves rather than smooth domes.
        let clumps = (noise(self.seed, fx * 0.45, fy * 0.8) - 0.5) * 0.55;
        // The quake: each cell trembles on its own phase, a little.
        let phase = hash01(self.seed, x as u64, y as u64) * std::f64::consts::TAU;
        let quake = frame.map_or(0.0, |frame| 0.11 * ((frame as f64) * 1.3 + phase).sin());
        (crowns * 1.25 + clumps + quake).clamp(0.0, 1.0)
    }

    fn ground_cell(&self, x: usize) -> Cell {
        if self.trunks.iter().any(|t| t.x == x) {
            return Cell {
                ch: '█',
                material: Material::Trunk,
            };
        }
        let shade = 0.22 + (noise(self.seed ^ 1, x as f64 * 0.5, 0.0) - 0.5) * 0.3;
        Cell {
            ch: RAMP[dither(shade, x, self.ground).max(1)],
            material: Material::Ground,
        }
    }

    /// The root system: under every stem a tap root, and between each
    /// stem and the next a root that sags and rises again, so one line
    /// joins the first stem to the last.
    fn root(&self, x: usize, y: usize) -> Cell {
        let depth = y - self.ground;
        let rows = self.roots as f64;
        if self.trunks.iter().any(|t| t.x == x) && depth == 1 {
            return Cell {
                ch: '▓',
                material: Material::Root,
            };
        }
        let joined = self.trunks.windows(2).any(|pair| {
            let (a, b) = (pair[0].x as f64, pair[1].x as f64);
            let fx = x as f64;
            if fx <= a || fx >= b {
                return false;
            }
            let t = (fx - a) / (b - a);
            let sag = (std::f64::consts::PI * t).sin() * rows;
            let row = (sag.round() as usize).clamp(1, self.roots);
            row == depth
        });
        match joined {
            true => Cell {
                ch: '░',
                material: Material::Root,
            },
            false => Cell::SKY,
        }
    }
}

/// The dithered shade of `value` (0 to 1) at a cell: an index into
/// [`RAMP`]. The fraction between two shades is resolved against the
/// cell's Bayer threshold, so a smooth value reads as a texture.
fn dither(value: f64, x: usize, y: usize) -> usize {
    let scaled = value.clamp(0.0, 1.0) * (RAMP.len() - 1) as f64;
    let base = scaled.floor();
    let threshold = (f64::from(BAYER[y % 4][x % 4]) + 0.5) / 16.0;
    let level = base as usize + usize::from(scaled - base > threshold);
    level.min(RAMP.len() - 1)
}

/// A deterministic value in 0..1 for a lattice point.
fn hash01(seed: u64, x: u64, y: u64) -> f64 {
    let mut z =
        seed ^ x.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ y.wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Smooth value noise: the lattice's values, eased between.
fn noise(seed: u64, x: f64, y: f64) -> f64 {
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (ease(x - x0), ease(y - y0));
    let at = |dx: f64, dy: f64| hash01(seed, (x0 + dx) as i64 as u64, (y0 + dy) as i64 as u64);
    let top = at(0.0, 0.0) + (at(1.0, 0.0) - at(0.0, 0.0)) * tx;
    let bottom = at(0.0, 1.0) + (at(1.0, 1.0) - at(0.0, 1.0)) * tx;
    top + (bottom - top) * ty
}

fn ease(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// splitmix64: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells_where(picture: &[Vec<Cell>], material: Material) -> Vec<(usize, usize)> {
        let mut at = Vec::new();
        for (y, row) in picture.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                if cell.material == material {
                    at.push((x, y));
                }
            }
        }
        at
    }

    // The same inputs, the same picture: a project's grove is its own and
    // does not change from one run to the next.
    #[test]
    fn a_grove_is_the_same_every_time_and_exactly_its_size() {
        let seed = seed_of("acme-shop");
        for (w, h) in [(24, 6), (40, 7), (78, 11), (160, 14)] {
            let picture = grove(w, h, 3, seed);
            assert_eq!(picture, grove(w, h, 3, seed));
            assert_eq!(picture.len(), h, "{w}×{h}");
            assert!(picture.iter().all(|row| row.len() == w), "{w}×{h}");
            for cell in picture.iter().flatten() {
                assert!(RAMP.contains(&cell.ch), "{:?} is not a shade", cell.ch);
                assert_eq!(cell.ch == ' ', cell.material == Material::Sky, "{cell:?}");
            }
        }
    }

    #[test]
    fn nothing_is_drawn_where_a_grove_would_not_read() {
        assert!(grove(MIN_WIDTH - 1, 20, 0, 1).is_empty());
        assert!(grove(80, MIN_HEIGHT - 1, 0, 1).is_empty());
        assert!(!grove(MIN_WIDTH, MIN_HEIGHT, 0, 1).is_empty());
    }

    // Aspens quake: the leaves move from frame to frame, and nothing else
    // does — a stem or a root that flickered would read as a glitch.
    #[test]
    fn only_the_leaves_quake() {
        let seed = seed_of("acme-shop");
        let still = grove(78, 11, 0, seed);
        let mut moved = 0;
        for frame in 1..8 {
            let next = grove(78, 11, frame, seed);
            for (y, row) in next.iter().enumerate() {
                for (x, cell) in row.iter().enumerate() {
                    let before = still[y][x];
                    if *cell != before {
                        moved += 1;
                        for m in [cell.material, before.material] {
                            assert!(
                                matches!(m, Material::Canopy | Material::Sky),
                                "({x},{y}) moved and is {m:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(moved > 20, "the canopy barely moves: {moved} cells");
    }

    // One organism: every stem stands on the ground, and the roots under
    // the ground join the first stem to the last without a gap.
    #[test]
    fn every_stem_stands_on_one_root_system() {
        for (w, h, name) in [(78, 11, "acme-shop"), (40, 7, "marketplace"), (30, 6, "x")] {
            let picture = grove(w, h, 0, seed_of(name));
            let ground = picture
                .iter()
                .position(|row| row.iter().any(|c| c.material == Material::Ground))
                .expect("a ground row");
            let stems: Vec<usize> = (0..w)
                .filter(|&x| picture[ground][x].material == Material::Trunk)
                .collect();
            assert!(stems.len() >= 3, "{name}: a grove of {}", stems.len());
            let first = *stems.first().unwrap();
            let last = *stems.last().unwrap();
            for x in first..=last {
                assert!(
                    (ground + 1..h).any(|y| picture[y][x].material == Material::Root),
                    "{name}: the roots break at column {x}\n{}",
                    to_text(&picture)
                );
            }
            // A stem runs unbroken from its crown to the ground.
            for &x in &stems {
                let top = (0..ground)
                    .find(|&y| matches!(picture[y][x].material, Material::Trunk | Material::Eye))
                    .expect("a stem above the ground");
                assert!(
                    (top..ground).all(|y| matches!(
                        picture[y][x].material,
                        Material::Trunk | Material::Eye | Material::Canopy
                    )),
                    "{name}: stem {x} has a gap\n{}",
                    to_text(&picture)
                );
            }
        }
    }

    #[test]
    fn each_project_has_a_grove_of_its_own() {
        let a = grove(78, 11, 0, seed_of("acme-shop"));
        let b = grove(78, 11, 0, seed_of("marketplace"));
        assert_ne!(to_text(&a), to_text(&b));
        assert!(
            !cells_where(&a, Material::Eye).is_empty(),
            "aspen bark has eyes"
        );
    }

    // Dithered, not banded: a flat value between two shades lands on both,
    // spread across the Bayer pattern.
    #[test]
    fn a_shade_between_two_is_dithered_across_the_block() {
        let levels: Vec<usize> = (0..4)
            .flat_map(|y| (0..4).map(move |x| dither(0.375, x, y)))
            .collect();
        assert!(levels.contains(&1) && levels.contains(&2), "{levels:?}");
        assert_eq!(levels.iter().filter(|&&l| l == 2).count(), 8, "{levels:?}");
        assert!((0..4).all(|x| dither(0.0, x, 0) == 0 && dither(1.0, x, 0) == 4));
    }
}
