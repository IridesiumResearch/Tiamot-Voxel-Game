// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Persistent 2D fields: the pre-pass a mod computes once and samples for ever.
//!
//! Implements `docs/subnode-contract.md` §5, at one remove: a map holds no
//! blocks at all. It is the input a generator reads before it writes, and what
//! it writes is block resolution unless that generator opts in itself.
//!
//! # Why this exists, and why a density field could not do it
//!
//! [`super::density`] evaluates an expression at a point. That is enough for
//! terrain and caves, and it is exactly the wrong shape for erosion or a river:
//! where water goes depends on where the land is EVERYWHERE ELSE, so no
//! function of one position can produce it. Those need a whole field computed
//! once, in passes that see all of it.
//!
//! A mod cannot compute one — charter rule 4 forbids the per-sample arithmetic,
//! and a map is millions of samples besides. So the engine holds the array and
//! the operations, the mod says which operations in what order, and the result
//! is stored with the world.
//!
//! # Bounded on purpose
//!
//! The world is 120,000 blocks across. One sample a block is 1.4 x 10^10
//! values, which is not a map, it is a second world. A map covers a REGION at a
//! chosen resolution — [`Map::MAX_SIDE`] samples a side, each spanning `scale`
//! blocks — and a generator outside it gets the map's edge value rather than a
//! hole. 1,024 samples at 16 blocks each is a 16 km square, which is a
//! landscape; anything larger is a different mechanism and should say so.
//!
//! # Every operation is content-free
//!
//! `noise`, `add`, `scale`, `min`, `max`, `blur`. There is no `erode` and no
//! `river`, because those are opinions about what a landscape is and charter
//! rule 1 puts opinions in mods. Erosion is a loop of blur and min against a
//! second map; a mod that wants a particular erosion writes that loop.
//!
//! # Determinism
//!
//! Every operation is `+ - * /` and comparison over a fixed traversal order,
//! and the noise is [`super::noise`]'s. Nothing here is order-dependent, so the
//! same script produces the same map on every supported target — which matters
//! because the map is generated once on the server and everything downstream
//! is derived from it.

use super::noise::{FractalParams, Region2d, fill_2d};

/// What a map operation can be wrong about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    /// A side outside `1..=`[`Map::MAX_SIDE`].
    #[error("a map may be 1 to {} samples a side, not {found}", Map::MAX_SIDE)]
    BadSide {
        /// What was asked for.
        found: u32,
    },
    /// A scale of zero blocks a sample, which spans nothing.
    #[error("a map's scale must be at least one block a sample")]
    BadScale,
    /// Two maps of different shapes cannot be combined.
    #[error("maps must match to combine: {left} x {left} against {right} x {right}")]
    Mismatched {
        /// This map's side.
        left: u32,
        /// The other's.
        right: u32,
    },
    /// A blur wider than the map.
    #[error("a blur radius of {found} is wider than a map {side} samples across")]
    BadRadius {
        /// What was asked for.
        found: u32,
        /// The map's side.
        side: u32,
    },
}

/// How the values of two maps are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    /// Sum.
    Add,
    /// Product.
    Multiply,
    /// The smaller of the two — a valley floor, or a ceiling on a hill.
    Minimum,
    /// The larger.
    Maximum,
}

/// A square field of values over a region of the world.
#[derive(Debug, Clone, PartialEq)]
pub struct Map {
    side: u32,
    /// Blocks per sample.
    scale: u32,
    /// World block coordinate of sample `(0, 0)`.
    origin: [i32; 2],
    values: Vec<f32>,
}

impl Map {
    /// The largest map, in samples a side.
    ///
    /// 1,024 is four megabytes of `f32` and, at the default scale, a sixteen
    /// kilometre square. A bound rather than a target: it is here so a script
    /// asking for a million samples a side is refused with a message instead of
    /// allocating until something dies.
    pub const MAX_SIDE: u32 = 1024;

    /// An empty map, all zeroes.
    ///
    /// # Errors
    ///
    /// [`MapError::BadSide`] or [`MapError::BadScale`].
    pub fn new(side: u32, scale: u32, origin: [i32; 2]) -> Result<Self, MapError> {
        if side == 0 || side > Self::MAX_SIDE {
            return Err(MapError::BadSide { found: side });
        }
        if scale == 0 {
            return Err(MapError::BadScale);
        }
        Ok(Self {
            side,
            scale,
            origin,
            values: vec![0.0; (side as usize) * (side as usize)],
        })
    }

    /// Samples a side.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.side
    }

    /// Blocks per sample.
    #[must_use]
    pub const fn scale(&self) -> u32 {
        self.scale
    }

    /// World block coordinate of sample `(0, 0)`.
    #[must_use]
    pub const fn origin(&self) -> [i32; 2] {
        self.origin
    }

    /// The values, row-major, x fastest.
    #[must_use]
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Replaces the values, for loading one back.
    ///
    /// # Errors
    ///
    /// [`MapError::BadSide`] if the count does not match this map's shape.
    pub fn set_values(&mut self, values: Vec<f32>) -> Result<(), MapError> {
        if values.len() != self.values.len() {
            return Err(MapError::BadSide { found: self.side });
        }
        self.values = values;
        Ok(())
    }

    /// Fills the whole map with fractal noise.
    ///
    /// Sampled in WORLD coordinates, so two maps of the same region with the
    /// same seed and shape agree, and a map is not tied to where it was made.
    pub fn noise(&mut self, seed: u64, params: &FractalParams, amplitude: f32) {
        let region = Region2d {
            origin_x: self.origin[0] as f32,
            origin_y: self.origin[1] as f32,
            step_x: self.scale as f32,
            step_y: self.scale as f32,
            width: self.side as usize,
            height: self.side as usize,
        };
        let mut samples = vec![0.0f32; self.values.len()];
        if fill_2d(seed, &region, params, &mut samples).is_err() {
            return;
        }
        for (value, sample) in self.values.iter_mut().zip(samples) {
            *value = sample * amplitude;
        }
    }

    /// Adds a constant to every sample.
    pub fn offset(&mut self, by: f32) {
        for value in &mut self.values {
            *value += by;
        }
    }

    /// Multiplies every sample by a constant.
    pub fn scale_by(&mut self, by: f32) {
        for value in &mut self.values {
            *value *= by;
        }
    }

    /// Bounds every sample to a range.
    pub fn clamp(&mut self, low: f32, high: f32) {
        for value in &mut self.values {
            *value = value.clamp(low, high);
        }
    }

    /// Combines another map into this one, sample for sample.
    ///
    /// # Errors
    ///
    /// [`MapError::Mismatched`] if the two are different sizes.
    pub fn combine(&mut self, other: &Self, how: Combine) -> Result<(), MapError> {
        if self.side != other.side {
            return Err(MapError::Mismatched {
                left: self.side,
                right: other.side,
            });
        }
        for (value, source) in self.values.iter_mut().zip(&other.values) {
            *value = match how {
                Combine::Add => *value + *source,
                Combine::Multiply => *value * *source,
                Combine::Minimum => value.min(*source),
                Combine::Maximum => value.max(*source),
            };
        }
        Ok(())
    }

    /// Smooths the map with a box blur of the given radius.
    ///
    /// **The primitive erosion is built from.** A mod's erosion is a loop of
    /// blur and `combine(Minimum)` against a second map; the engine holds no
    /// opinion about how many passes or how hard, because that is what a
    /// landscape looks like and charter rule 1 puts that in a mod.
    ///
    /// Separable — a horizontal pass then a vertical one — which is the same
    /// answer as the square kernel and costs `2r` per sample rather than `r²`.
    /// The edges clamp to the border sample rather than wrapping: a map is a
    /// region of a world and the world does not wrap.
    ///
    /// # Errors
    ///
    /// [`MapError::BadRadius`] if the radius is wider than the map.
    pub fn blur(&mut self, radius: u32) -> Result<(), MapError> {
        if radius == 0 {
            return Ok(());
        }
        if radius >= self.side {
            return Err(MapError::BadRadius {
                found: radius,
                side: self.side,
            });
        }
        let side = i64::from(self.side);
        let radius = i64::from(radius);
        let span = (radius * 2 + 1) as f32;
        let at = |values: &[f32], x: i64, y: i64| -> f32 {
            let x = x.clamp(0, side - 1) as usize;
            let y = y.clamp(0, side - 1) as usize;
            values[y * self.side as usize + x]
        };

        // **Summed in a fixed order, both passes.** Charter rule 4 bans float
        // accumulation over a non-deterministic order, and while a loop is
        // already ordered, saying so is what stops somebody parallelising a
        // reduction that would then give a different answer.
        let mut horizontal = vec![0.0f32; self.values.len()];
        for y in 0..side {
            for x in 0..side {
                let mut total = 0.0;
                for offset in -radius..=radius {
                    total += at(&self.values, x + offset, y);
                }
                horizontal[(y * side + x) as usize] = total / span;
            }
        }
        let mut vertical = vec![0.0f32; self.values.len()];
        for y in 0..side {
            for x in 0..side {
                let mut total = 0.0;
                for offset in -radius..=radius {
                    total += at(&horizontal, x, y + offset);
                }
                vertical[(y * side + x) as usize] = total / span;
            }
        }
        self.values = vertical;
        Ok(())
    }

    /// The value at a world block position, bilinearly interpolated.
    ///
    /// Outside the map, the nearest edge value — a generator that ran past the
    /// region gets the coast rather than a cliff into zero.
    #[must_use]
    pub fn sample(&self, x: i32, z: i32) -> f32 {
        let side = i64::from(self.side);
        let scale = self.scale as f32;
        let fx = (x - self.origin[0]) as f32 / scale;
        let fz = (z - self.origin[1]) as f32 / scale;
        let x0 = i64::from(super::floor_to_i32(fx));
        let z0 = i64::from(super::floor_to_i32(fz));
        let tx = fx - x0 as f32;
        let tz = fz - z0 as f32;
        let at = |x: i64, z: i64| -> f32 {
            let x = x.clamp(0, side - 1) as usize;
            let z = z.clamp(0, side - 1) as usize;
            self.values[z * self.side as usize + x]
        };
        // Bilinear from multiplies and adds only — all in the deterministic
        // subset, and the same expression on every target.
        let top = at(x0, z0) + (at(x0 + 1, z0) - at(x0, z0)) * tx;
        let bottom = at(x0, z0 + 1) + (at(x0 + 1, z0 + 1) - at(x0, z0 + 1)) * tx;
        top + (bottom - top) * tz
    }

    /// Replaces every value with a density program's, sampled on one plane.
    ///
    /// # Why a map wants to read a density
    ///
    /// It closes the loop the map operations were written for. A generator
    /// whose surface is a density can now take that surface INTO a map, run
    /// blurs and combines over it — which is what erosion is — and read the
    /// result back through [`super::density::Op::Map`]. Without one of the two
    /// directions the field can be computed and never used, or used and never
    /// computed.
    ///
    /// `height` is the plane the program is asked about, because a map is a
    /// surface and a density is a volume, and something has to say where the
    /// two meet. For a field of the usual shape — noise minus `y` — sampling at
    /// `y = 0` gives exactly the height at which that field changes sign.
    ///
    /// Sampled at the map's own resolution, one evaluation per cell, in world
    /// coordinates: two maps of the same region with the same program and seed
    /// agree, as they do for [`Self::noise`].
    ///
    /// # Errors
    ///
    /// [`super::density::DensityError`] if the program will not evaluate.
    pub fn fill_from_density(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        height: f32,
    ) -> Result<(), super::density::DensityError> {
        let side = self.side as usize;
        let region = super::noise::Region3d {
            origin_x: self.origin[0] as f32,
            origin_y: height,
            origin_z: self.origin[1] as f32,
            step: self.scale as f32,
            width: side,
            height: 1,
            depth: side,
        };
        // Straight into the map's own storage: the layout `Region3d` produces
        // with a height of one is x-fastest rows of z, which is exactly how
        // `values` is indexed by `sample`.
        density.evaluate(seed, &region, &mut self.values)
    }

    /// A chunk's worth of heights, sampled from this map.
    ///
    /// **The whole point of the type, and why a script never sees a sample.**
    /// A generator wants 256 heights for the columns of one chunk; asking for
    /// them one at a time from Lua is the per-sample loop charter rule 4
    /// forbids. This produces the lot natively, in the order
    /// `ChunkBuffer::fill_below_heightmap` consumes.
    #[must_use]
    pub fn heightmap(&self, chunk_x: i32, chunk_z: i32) -> Vec<i32> {
        let span = crate::CHUNK_BLOCKS as i32;
        let mut heights = Vec::with_capacity((span * span) as usize);
        for z in 0..span {
            for x in 0..span {
                let value = self.sample(chunk_x * span + x, chunk_z * span + z);
                heights.push(super::floor_to_i32(value));
            }
        }
        heights
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_map_filled_from_a_density_holds_what_the_density_says() {
        // The round trip both new pieces exist for: a surface described as a
        // density becomes a field, which map operations can then work on.
        use super::super::density::{Axis, Density, Op};
        let density = Density::compile(vec![
            Op::Coordinate(Axis::X),
            Op::Constant(0.5),
            Op::Multiply,
            Op::Coordinate(Axis::Z),
            Op::Add,
        ])
        .expect("compile");

        let mut map = Map::new(8, 4, [0, 0]).expect("map");
        map.fill_from_density(&density, 1, 0.0).expect("fill");

        // x fastest, and the values are the program's: x/2 + z at the world
        // position of each cell, which is the cell index times the scale.
        for z in 0..8usize {
            for x in 0..8usize {
                let expected = (x * 4) as f32 * 0.5 + (z * 4) as f32;
                let found = map.values()[z * 8 + x];
                assert!(
                    (found - expected).abs() < 1e-3,
                    "cell ({x}, {z}) holds {found}, not {expected} — a transposed fill is \
                     exactly what this looks like"
                );
            }
        }
    }

    #[test]
    fn a_density_reading_a_map_sees_what_the_map_holds() {
        // The other direction, and the one erosion needs: whatever passes a
        // mod ran over the map, the terrain follows them.
        use super::super::density::{Density, Op};
        let mut map = Map::new(4, 16, [0, 0]).expect("map");
        map.set_values(vec![5.0; 16]).expect("values");
        // A ridge down one row, so a wrong axis shows up as a value in the
        // wrong place rather than as no difference at all.
        let mut values = map.values().to_vec();
        values[4] = 40.0;
        map.set_values(values).expect("values");

        let range = (5.0, 40.0);
        let density = Density::compile(vec![Op::Map {
            map: std::sync::Arc::new(map.clone()),
            range,
        }])
        .expect("compile");

        // Exactly on the cell the ridge is in: (0, 16) at scale 16 is cell
        // (0, 1), which holds 40.
        let on_the_cell = super::super::noise::Region3d {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 16.0,
            step: 1.0,
            width: 1,
            height: 1,
            depth: 1,
        };
        let mut out = [0.0];
        density
            .evaluate(3, &on_the_cell, &mut out)
            .expect("evaluate");
        assert!(
            (out[0] - 40.0).abs() < 1e-3,
            "reading the map at its own cell gave {}, not the 40 it holds there",
            out[0]
        );

        // And one block along, where `Map::sample` interpolates towards the
        // neighbouring 5 — the behaviour that makes a map a smooth field
        // rather than a grid of steps, and worth pinning so a switch to
        // nearest-neighbour cannot pass unnoticed.
        let between = super::super::noise::Region3d {
            origin_x: 1.0,
            ..on_the_cell
        };
        let mut nearby = [0.0];
        density
            .evaluate(3, &between, &mut nearby)
            .expect("evaluate");
        assert!(
            nearby[0] < 40.0 && nearby[0] > 5.0,
            "one block from the ridge gave {}, which is neither interpolated nor plausible",
            nearby[0]
        );

        // And a bound over the map is the map's own range, so a chunk under a
        // flat part of an eroded field can still be decided.
        assert!(
            !density.bounds(&on_the_cell).is_undecided(),
            "a bound over a map is the map's own range, so this had to be decidable"
        );
    }

    #[test]
    fn a_density_keeps_the_map_it_compiled_with() {
        // **The seam this design exists to prevent.** A program that pointed at
        // a live map would generate different terrain after the mod blurred it
        // once more, and the boundary between the two would be permanent and
        // invisible.
        use super::super::density::{Density, Op};
        let mut map = Map::new(2, 16, [0, 0]).expect("map");
        map.set_values(vec![1.0; 4]).expect("values");
        let density = Density::compile(vec![Op::Map {
            map: std::sync::Arc::new(map.clone()),
            range: (1.0, 1.0),
        }])
        .expect("compile");

        map.set_values(vec![99.0; 4]).expect("values");

        let region = super::super::noise::Region3d {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            step: 1.0,
            width: 1,
            height: 1,
            depth: 1,
        };
        let mut out = [0.0];
        density.evaluate(1, &region, &mut out).expect("evaluate");
        assert!(
            (out[0] - 1.0).abs() < 1e-6,
            "the program saw {}, so it is reading the map as it is now rather than as it was \
             when it compiled",
            out[0]
        );
    }
    use super::*;

    fn flat(side: u32, value: f32) -> Map {
        let mut map = Map::new(side, 16, [0, 0]).expect("map");
        map.offset(value);
        map
    }

    #[test]
    fn a_map_is_bounded_rather_than_allocated_on_request() {
        // It comes from a script, so an absurd size is a message and not an
        // allocation that takes the server with it.
        assert_eq!(
            Map::new(Map::MAX_SIDE + 1, 16, [0, 0]),
            Err(MapError::BadSide {
                found: Map::MAX_SIDE + 1
            })
        );
        assert_eq!(Map::new(0, 16, [0, 0]), Err(MapError::BadSide { found: 0 }));
        assert_eq!(Map::new(8, 0, [0, 0]), Err(MapError::BadScale));
    }

    #[test]
    fn sampling_outside_the_map_gives_the_edge_and_not_a_hole() {
        // **A generator running past the region is the normal case**, not an
        // error: a map covers a landscape and the world is bigger than one.
        // Zero out there would be a cliff into the sea all the way round.
        let map = flat(8, 30.0);
        assert!((map.sample(-100_000, -100_000) - 30.0).abs() < f32::EPSILON);
        assert!((map.sample(100_000, 100_000) - 30.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_blur_keeps_the_average_and_pulls_a_spike_down() {
        // The primitive erosion is built from, so what matters is that it
        // MOVES material rather than inventing or destroying it, and that a
        // peak comes down.
        let mut map = flat(16, 0.0);
        let centre = 8 * 16 + 8;
        map.values[centre] = 100.0;
        let before: f32 = map.values.iter().sum();

        map.blur(2).expect("blur");
        let after: f32 = map.values.iter().sum();
        assert!(
            (before - after).abs() < 1.0,
            "a blur moved {} of material: {before} became {after}",
            (before - after).abs()
        );
        assert!(
            map.values[centre] < 20.0,
            "the spike did not come down: {}",
            map.values[centre]
        );
        assert!(
            map.values[centre - 1] > 0.0,
            "nothing spread to the neighbour"
        );
    }

    #[test]
    fn a_blur_wider_than_the_map_is_refused_rather_than_clamped() {
        let mut map = flat(8, 1.0);
        assert_eq!(map.blur(8), Err(MapError::BadRadius { found: 8, side: 8 }));
        assert!(
            map.blur(0).is_ok(),
            "a radius of zero is a no-op, not an error"
        );
    }

    #[test]
    fn combining_needs_two_maps_of_the_same_shape() {
        let mut map = flat(8, 5.0);
        let other = flat(16, 1.0);
        assert_eq!(
            map.combine(&other, Combine::Add),
            Err(MapError::Mismatched { left: 8, right: 16 })
        );

        let same = flat(8, 3.0);
        map.combine(&same, Combine::Minimum).expect("combine");
        assert!((map.values[0] - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_heightmap_is_one_chunks_worth_in_the_order_the_fill_wants() {
        // 256 columns, x fastest — what `ChunkBuffer::fill_below_heightmap`
        // indexes as `x + CHUNK_BLOCKS * z`. A transposed heightmap is a world
        // mirrored about its diagonal, which looks plausible until two chunks
        // meet.
        let mut map = Map::new(64, 16, [0, 0]).expect("map");
        // A ramp in x only, so a transpose would be visible.
        for z in 0..64 {
            for x in 0..64 {
                map.values[z * 64 + x] = x as f32;
            }
        }
        let heights = map.heightmap(1, 0);
        assert_eq!(heights.len(), 256);
        let span = crate::CHUNK_BLOCKS as usize;
        // Along x the value climbs; along z it does not move at all.
        assert!(
            heights[1] >= heights[0],
            "x is not the fast axis: {:?}",
            &heights[..4]
        );
        assert_eq!(
            heights[0], heights[span],
            "z changed a value that only varies along x, so the axes are swapped"
        );
    }

    #[test]
    fn the_same_seed_and_shape_give_the_same_map() {
        // Charter rule 4. The map is generated once on the server and the whole
        // world is derived from it, so this is the property everything else
        // rests on.
        let params = super::super::default_params();
        let mut first = Map::new(32, 16, [0, 0]).expect("map");
        let mut second = Map::new(32, 16, [0, 0]).expect("map");
        first.noise(7, &params, 10.0);
        second.noise(7, &params, 10.0);
        assert_eq!(first.values, second.values);

        let mut elsewhere = Map::new(32, 16, [4096, 0]).expect("map");
        elsewhere.noise(7, &params, 10.0);
        assert_ne!(
            first.values, elsewhere.values,
            "a map is sampled in WORLD coordinates, so a different region is a \
             different landscape"
        );
    }
}
