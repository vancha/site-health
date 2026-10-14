// SPDX-License-Identifier: GPL-3.0

//! A tiny Gray-Scott reaction-diffusion simulation.
//!
//! Two chemicals, A and B, cover a grid. Both spread out to neighboring cells
//! (B more slowly than A), and where they meet, B feeds on A and turns it into
//! more B. New A is added everywhere (`feed`), and B slowly disappears
//! (`kill`). Depending on those two knobs, B forms spots, stripes, coral or
//! dividing cells. Only B is drawn.

/// How strongly each chemical spreads to its neighbors.
const SPREAD_A: f32 = 1.0;
const SPREAD_B: f32 = 0.5;

/// Below this much B in the whole dish, the pattern counts as "died out" and
/// gets new seeds, so the dish never stays empty.
const DIED_OUT: f32 = 2.0;

/// Amounts of B smaller than this are rounded down to zero. They're far too
/// small to ever matter, and letting them shrink forever makes them so tiny
/// that the processor handles them in a much slower way ("subnormal" numbers).
const NEGLIGIBLE: f32 = 1e-6;

/// The grid wraps around: something leaving at the right comes back in at the
/// left, and the same for top and bottom. So there are no walls for the
/// pattern to crowd against; it's only *drawn* as a circle.
///
/// To make the wrapping cheap, the grid has a one-cell border all around it,
/// which holds a copy of the cells on the opposite side (see `fill_border`).
/// Then every real cell simply has neighbors on all sides, and the inner loop
/// never has to work out where the grid wraps.
pub struct Dish {
    /// The grid is `size` × `size` real cells.
    size: usize,
    /// The width of one stored row: the real cells plus the border on both sides.
    stride: usize,
    a: Vec<f32>,
    b: Vec<f32>,
    /// Scratch space for the next step, reused to avoid allocating every step.
    next_a: Vec<f32>,
    next_b: Vec<f32>,
    rng: Rng,
}

impl Dish {
    pub fn new(size: usize, seed: u64) -> Self {
        let stride = size + 2;
        let cells = stride * stride;

        let mut dish = Self {
            size,
            stride,
            a: vec![1.0; cells],
            b: vec![0.0; cells],
            next_a: vec![1.0; cells],
            next_b: vec![0.0; cells],
            rng: Rng::new(seed),
        };
        dish.sprinkle_seeds();
        dish
    }

    /// Where real cell (x, y) is stored, both counted from 0 to `size - 1`.
    /// Wraps around, so -1 or `size` also work.
    fn at(&self, x: isize, y: isize) -> usize {
        let n = self.size as isize;
        let (x, y) = (x.rem_euclid(n) as usize, y.rem_euclid(n) as usize);
        (y + 1) * self.stride + x + 1
    }

    /// How much of chemical B there is at a point, where (0, 0) is the top-left
    /// corner of the grid and (1, 1) the bottom-right. Points between cells
    /// blend their four nearest cells, so the pattern can be drawn smoothly at
    /// any size, bigger or smaller than the grid.
    pub fn b_at(&self, x: f32, y: f32) -> f32 {
        let n = self.size as f32;
        // Cell centers sit at half-cell offsets.
        let gx = x * n - 0.5;
        let gy = y * n - 0.5;
        let (x0, y0) = (gx.floor(), gy.floor());
        let (tx, ty) = (gx - x0, gy - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);

        let cell = |cx: isize, cy: isize| self.b[self.at(cx, cy)];

        let top = cell(x0, y0) * (1.0 - tx) + cell(x0 + 1, y0) * tx;
        let bottom = cell(x0, y0 + 1) * (1.0 - tx) + cell(x0 + 1, y0 + 1) * tx;
        top * (1.0 - ty) + bottom * ty
    }

    /// Advances the simulation by one small step, with the given knobs.
    pub fn step(&mut self, feed: f32, kill: f32) {
        let (n, w) = (self.size, self.stride);
        fill_border(&mut self.a, n);
        fill_border(&mut self.b, n);

        for y in 1..=n {
            // Taking the rows as slices of exactly one row lets the compiler
            // see that every neighbor below is in range, so it can leave out
            // its safety checks.
            let [a_up, a_row, a_down] = three_rows(&self.a, y, w);
            let [b_up, b_row, b_down] = three_rows(&self.b, y, w);
            let next_a = &mut self.next_a[y * w..(y + 1) * w];
            let next_b = &mut self.next_b[y * w..(y + 1) * w];

            for x in 1..w - 1 {
                // How much flows in from the neighbors: their weighted average
                // minus the cell itself. Direct neighbors count 0.2, diagonal
                // ones 0.05; together that's 1, so an even area stays even.
                let spread = |up: &[f32], row: &[f32], down: &[f32]| {
                    0.2 * (row[x - 1] + row[x + 1] + up[x] + down[x])
                        + 0.05 * (up[x - 1] + up[x + 1] + down[x - 1] + down[x + 1])
                        - row[x]
                };

                let (a, b) = (a_row[x], b_row[x]);
                let reaction = a * b * b;

                next_a[x] = (a + SPREAD_A * spread(a_up, a_row, a_down) - reaction + feed * (1.0 - a)).clamp(0.0, 1.0);
                let new_b = (b + SPREAD_B * spread(b_up, b_row, b_down) + reaction - (kill + feed) * b).clamp(0.0, 1.0);
                next_b[x] = if new_b < NEGLIGIBLE { 0.0 } else { new_b };
            }
        }

        std::mem::swap(&mut self.a, &mut self.next_a);
        std::mem::swap(&mut self.b, &mut self.next_b);
    }

    /// If the pattern has died out, sprinkles new seeds so the dish never
    /// stays empty. Checking means adding up the whole grid, so this is meant
    /// to be called now and then (like once per frame), not after every step.
    pub fn revive_if_empty(&mut self) {
        // The border only holds copies, so count the real rows only.
        let rows = self.b[self.stride..self.stride * (self.size + 1)].chunks(self.stride);
        let total: f32 = rows.map(|row| row[1..=self.size].iter().sum::<f32>()).sum();

        if total < DIED_OUT {
            self.sprinkle_seeds();
        }
    }

    /// Drops one small blob of B at a random place. Where there's room, a new
    /// spot sprouts from it; where the pattern already is, it just merges in.
    pub fn drop(&mut self) {
        let cx = self.rng.below(self.size as u32) as isize;
        let cy = self.rng.below(self.size as u32) as isize;

        for y in (cy - 1)..=(cy + 1) {
            for x in (cx - 1)..=(cx + 1) {
                let i = self.at(x, y);
                self.a[i] = 0.5;
                self.b[i] = 1.0;
            }
        }
    }

    /// Drops a few blobs of B at random places, for the pattern to grow from.
    fn sprinkle_seeds(&mut self) {
        for _ in 0..4 {
            self.drop();
        }
    }
}

/// Row `y` of a grid with rows `w` cells wide, and the rows above and below it.
fn three_rows(cells: &[f32], y: usize, w: usize) -> [&[f32]; 3] {
    [
        &cells[(y - 1) * w..y * w],
        &cells[y * w..(y + 1) * w],
        &cells[(y + 1) * w..(y + 2) * w],
    ]
}

/// Copies the cells along each edge of an `n` × `n` grid into the border on
/// the opposite side, so the grid wraps around (see `Dish`).
fn fill_border(cells: &mut [f32], n: usize) {
    let w = n + 2;

    // Left and right border of every real row.
    for y in 1..=n {
        let row = y * w;
        cells[row] = cells[row + n];
        cells[row + n + 1] = cells[row + 1];
    }

    // Top and bottom border: whole rows, so this also fills the corners.
    cells.copy_within(n * w..(n + 1) * w, 0);
    cells.copy_within(w..2 * w, (n + 1) * w);
}

/// A tiny random number generator (xorshift). Good enough for scattering
/// seeds and picking species, and saves pulling in a whole crate for it.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Xorshift gets stuck at zero, so make sure the seed never is.
        Self(seed | 1)
    }

    /// A random number from 0 up to (not including) `max`.
    pub fn below(&mut self, max: u32) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % u64::from(max)) as u32
    }
}
