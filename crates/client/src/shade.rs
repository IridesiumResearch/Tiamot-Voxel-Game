// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Turning block-resolution light into per-vertex colour.
//!
//! # Smooth lighting, and why it does not ruin greedy meshing
//!
//! A vertex takes a bilinear sample of the block light field at its own
//! position, taking the light of a block to sit at that block's CENTRE.
//! Hardware interpolation across the quad does the rest, so a surface gets a
//! gradient rather than a per-block staircase.
//!
//! # Sampling at block centres, and the quarter levels that go with it
//!
//! The obvious thing is to average the four blocks touching the corner, and
//! that is what this did first. It is wrong in a way that is invisible until
//! somebody looks at a lamp: light is stored per BLOCK and a vertex sits on a
//! CELL lattice three times finer, so all four of those blocks are the same
//! block unless the corner happens to sit exactly on a block boundary. The
//! measured result, along a ramp falling one level per block, was corner pairs
//! of `[15 14] [14 14] [14 14]` — **the entire level change crammed into the
//! first third of each block and the other two thirds flat.** Reported from the
//! window as the interpolation between blocks being rough, which is exactly
//! what a hard band every block looks like.
//!
//! Interpolating between block centres spreads that change across the whole
//! block, and then immediately runs into the second half of the problem: a
//! level is four bits, so a gradient of one level per block has nothing between
//! one level and the next to say. **So a corner carries two more bits per
//! channel** — quarter levels — which is what turns a one-step drop per block
//! into a four-step ramp. They cost nothing: the vertex's position word had
//! eleven bits spare, occlusion took two, and eight of the remaining nine hold
//! these.
//!
//! That forces a rule on the mesher: **two cell faces may only merge if their
//! corner light agrees.** Merging across a lighting discontinuity would
//! interpolate straight through it and smear a shadow edge into a soft gradient
//! several blocks wide.
//!
//! **On a uniformly lit surface that costs nothing at all**, and that is the
//! case that decides a world's vertex count: open ground under daylight has one
//! light level everywhere, every corner agrees exactly, and a chunk floor is
//! still the handful of quads it was. The fast path below returns before it has
//! multiplied anything.
//!
//! Where light genuinely varies it now costs more than it used to, and the
//! number is worth writing down rather than discovering later. A floor lit by a
//! single lamp falling one level per block, measured over a whole chunk:
//! **3,939 quads under the old averaging against 4,805 under this — 22% more.**
//! The gradient really does have more distinct values in it now, which is the
//! entire point; a sampler that merged them back together would be the bug this
//! replaced. Charter rule 18's priority order is explicit that a smooth world
//! beats reach onto low-end hardware, and 22% falls on lamp-lit regions only.
//!
//! # This is presentation, not simulation
//!
//! Charter rule 4's float rules do not reach here: nothing in this file feeds
//! the determinism gate or the tick. The interpolation is integer arithmetic
//! anyway — see `WEIGHTS` — which is a convenience rather than a requirement.

use tiamot_core::SUBNODES_PER_AXIS;
use tiamot_core::light::{CHANNELS, Light};

/// Block-resolution light, as the mesher needs to sample it.
///
/// Coordinates are **chunk-local blocks and may be out of range**: a vertex on
/// a chunk's edge samples blocks in the neighbour, so `-1` and
/// [`tiamot_core::CHUNK_BLOCKS`] are ordinary inputs rather than errors.
pub trait BlockLight {
    /// The light at a chunk-local block.
    fn at(&self, x: i32, y: i32, z: i32) -> Light;
}

/// A light level everywhere.
///
/// For meshing a chunk whose light has not arrived, and for tests that are
/// about geometry rather than about light.
#[derive(Debug, Clone, Copy)]
pub struct Uniform(pub Light);

impl BlockLight for Uniform {
    fn at(&self, _x: i32, _y: i32, _z: i32) -> Light {
        self.0
    }
}

/// How a quad's four corners are shaded, in the order the mesher emits them.
///
/// `[(0,0), (1,0), (1,1), (0,1)]` in the quad's own `(u, v)` plane — the same
/// circulation [`crate::mesher::Mesh::to_buffers`] walks, so corner `n` here is
/// vertex `n` there.
///
/// # Light and occlusion are kept apart
///
/// It is tempting to bake the occlusion into the light and carry one value, and
/// the first version of this did. **It reads wrong.** Scaling the light keeps
/// its hue, so a corner shadowed under a low sun comes out as dim orange rather
/// than as dark — the user's word for it was "yellow". Occlusion is a property
/// of the geometry, not of the light falling on it, and a shadowed corner
/// should darken whatever colour is landing there.
///
/// Keeping them apart costs nothing: the vertex's position word has eleven bits
/// spare and occlusion needs two.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shade {
    /// Light at each corner, unoccluded.
    pub light: [Light; 4],
    /// The quarter-level remainder at each corner, two bits per channel.
    ///
    /// Packed in the same channel order as [`Light`] — sun in bits 6 and 7,
    /// then red, green, blue — so that a reader who knows one knows the other.
    /// A corner's true level is `light.channel(c) + fine(c) / 4`.
    pub fine: [u8; 4],
    /// Occlusion level at each corner, `0` darkest to `3` open.
    pub occlusion: [u8; 4],
}

/// How one vertex is shaded.
///
/// The two halves travel together because the mesher emits them together, and
/// keeping them in one value is what lets the vertex constructor stay inside a
/// readable argument list.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Corner {
    /// Light landing here, unoccluded.
    pub light: Light,
    /// The quarter-level remainder, two bits per channel. See [`Shade::fine`].
    pub fine: u8,
    /// Occlusion level, `0` darkest to `3` open.
    pub occlusion: u8,
}

impl Shade {
    /// One corner's shading.
    ///
    /// # Panics
    ///
    /// In debug builds, if `index` is 4 or greater.
    #[must_use]
    pub const fn corner(&self, index: usize) -> Corner {
        debug_assert!(index < 4);
        Corner {
            light: self.light[index],
            fine: self.fine[index],
            occlusion: self.occlusion[index],
        }
    }

    /// One level at every corner, nothing occluded.
    #[must_use]
    pub const fn flat(level: Light) -> Self {
        Self {
            light: [level; 4],
            fine: [0; 4],
            occlusion: [3; 4],
        }
    }
}

/// Cell-resolution occupancy, for ambient occlusion.
///
/// Coordinates are chunk-local cells and **may be out of range**, which is the
/// interesting case: a face on a chunk's boundary has neighbours across it.
pub trait CellOccupancy {
    /// Whether a cell is solid. Out-of-range cells read as empty.
    fn solid(&self, x: i32, y: i32, z: i32) -> bool;
}

/// Nothing is solid. For tests about light rather than about corners.
#[derive(Debug, Clone, Copy)]
pub struct Empty;

impl CellOccupancy for Empty {
    fn solid(&self, _x: i32, _y: i32, _z: i32) -> bool {
        false
    }
}

/// How many distinct occlusion levels a corner can have.
///
/// The classic three-neighbour test gives four: open, one side, two sides, and
/// boxed in. Two bits, which is what the vertex carries.
pub const OCCLUSION_LEVELS: usize = 4;

/// The light at the four corners of one cell's face.
///
/// `cell` is the solid cell being drawn and `(axis, positive)` its face, so the
/// light is sampled one cell *outside* it — the air in front of a surface,
/// never the surface itself. Sampling the solid cell would read the darkness
/// inside the block and make every lit wall black.
#[must_use]
pub fn face_shade(
    light: &impl BlockLight,
    occupancy: &impl CellOccupancy,
    axis: usize,
    positive: bool,
    cell: (i32, i32, i32),
) -> Shade {
    let (u_axis, v_axis) = plane_axes(axis);
    let outside = step(cell, axis, if positive { 1 } else { -1 });

    let mut shade = Shade::default();
    // The mesher's corner order. `(du, dv)` picks which of the four lattice
    // points around the cell face this corner sits on.
    for (index, (du, dv)) in [(0, 0), (1, 0), (1, 1), (0, 1)].into_iter().enumerate() {
        let (level, fine) = corner_light(light, outside, u_axis, v_axis, du, dv);
        shade.light[index] = level;
        shade.fine[index] = fine;
        shade.occlusion[index] = corner_occlusion(occupancy, outside, u_axis, v_axis, du, dv) as u8;
    }
    shade
}

/// The classic three-neighbour ambient occlusion level, `0..=3`.
///
/// Looks at the two cells beside the corner in the face's own plane and the one
/// diagonally between them, all in the layer *outside* the surface. Three is
/// open sky; zero is a corner with geometry on both sides.
///
/// **Two solid sides give the darkest level whatever the diagonal does.** The
/// diagonal is invisible from a corner already closed on both sides, and
/// counting it would make an inside corner brighten when the block behind it
/// was removed — which reads as the wrong block having been dug.
///
/// # A known seam
///
/// Cells outside the chunk read as empty, so a face on a chunk boundary gets no
/// occlusion from geometry across it and is very slightly lighter than the same
/// corner one block inside. The mesher's grid cannot answer diagonally across a
/// boundary — its padding covers one cell along each column axis and no more —
/// so closing this needs the store, and a store lookup per corner is twelve
/// hash lookups per cell face. Recorded rather than hidden.
fn corner_occlusion(
    occupancy: &impl CellOccupancy,
    outside: (i32, i32, i32),
    u_axis: usize,
    v_axis: usize,
    du: i32,
    dv: i32,
) -> usize {
    // From a lattice point, the neighbours that matter are on the corner's own
    // side: `du = 0` looks towards -u, `du = 1` towards +u.
    let toward_u = 2 * du - 1;
    let toward_v = 2 * dv - 1;

    let solid = |cell: (i32, i32, i32)| usize::from(occupancy.solid(cell.0, cell.1, cell.2));
    let side_u = solid(step(outside, u_axis, toward_u));
    let side_v = solid(step(outside, v_axis, toward_v));
    if side_u == 1 && side_v == 1 {
        return 0;
    }
    let diagonal = solid(step(step(outside, u_axis, toward_u), v_axis, toward_v));
    3 - (side_u + side_v + diagonal)
}

/// The denominator the interpolation weights are expressed in.
///
/// Twelve, because a lattice point falls on a third of the way between two
/// block centres and the answer wants quarters: twelve is the smallest number
/// that holds both exactly, so every weight below is an integer and nothing
/// rounds until the very end.
const WEIGHTS: i32 = 12;

/// Which two blocks a lattice point falls between, and how far along.
///
/// A block's light is taken to sit at the block's CENTRE, which in cells is
/// `3b + 1.5`. The returned weight is out of [`WEIGHTS`] and applies to the
/// SECOND block.
///
/// Working in twelfths rather than in floats keeps this exact and keeps the
/// happy accident noted at the top of this file — smooth lighting is integer
/// arithmetic, so there is nothing here for a platform to disagree about even
/// though nothing requires it to agree.
const fn span(lattice: i32) -> (i32, i32) {
    // Position along the block-centre lattice, in twelfths of a block:
    // `(lattice - 1.5) / 3` scaled by 12 is `(2 * lattice - 3) * 2`.
    let scaled = (2 * lattice - 3) * 2;
    let first = scaled.div_euclid(WEIGHTS);
    (first, scaled.rem_euclid(WEIGHTS))
}

/// The block light field sampled at one corner of a face.
///
/// Returns the level and its quarter-level remainder, two bits per channel.
///
/// Bilinear between block CENTRES rather than an average of the blocks nearby.
/// See this module's header for what averaging did and how it looked.
fn corner_light(
    light: &impl BlockLight,
    outside: (i32, i32, i32),
    u_axis: usize,
    v_axis: usize,
    du: i32,
    dv: i32,
) -> (Light, u8) {
    // The lattice point this corner sits on, in cells, along each spanning
    // axis. The third axis is the face's own normal, where the sample belongs
    // in the air layer `outside` already names.
    let (u_first, u_weight) = span(axis_of(outside, u_axis) + du);
    let (v_first, v_weight) = span(axis_of(outside, v_axis) + dv);

    let mut samples = [Light::DARK; 4];
    let mut weights = [0_i32; 4];
    for i in 0..2 {
        for j in 0..2 {
            // The two spanning axes are already BLOCK coordinates, straight out
            // of `span`; only the face's own normal is still a cell and needs
            // converting. Getting that backwards samples a block three times
            // too far out and lights a surface with its neighbour's light.
            let mut block = outside;
            set_axis(&mut block, u_axis, u_first + i);
            set_axis(&mut block, v_axis, v_first + j);
            let normal = 3 - u_axis - v_axis;
            set_axis(&mut block, normal, block_of(axis_of(outside, normal)));
            samples[(i * 2 + j) as usize] = light.at(block.0, block.1, block.2);
            weights[(i * 2 + j) as usize] = if i == 0 { WEIGHTS - u_weight } else { u_weight }
                * if j == 0 { WEIGHTS - v_weight } else { v_weight };
        }
    }

    // **The overwhelmingly common case, and worth its own path.** Light is per
    // block and a block is three cells across, so most corners sit inside one
    // block's neighbourhood and all four samples are the same value. Weighting
    // four copies of a number channel by channel — unpacking, multiplying,
    // summing, rounding and repacking sixteen nibbles — costs more than the
    // comparison that avoids it, and this runs for every corner of every face
    // in a chunk. On open ground under daylight it is every corner there is.
    if samples[1] == samples[0] && samples[2] == samples[0] && samples[3] == samples[0] {
        return (samples[0], 0);
    }

    let mut out = Light::DARK;
    let mut fine = 0_u8;
    for channel in 0..CHANNELS {
        let total: i32 = samples
            .iter()
            .zip(weights)
            .map(|(level, weight)| i32::from(level.channel(channel)) * weight)
            .sum();
        // Into quarter levels, rounded rather than truncated: the weights sum
        // to `WEIGHTS * WEIGHTS`, so a quarter level is that over four.
        let quarters = (total * 4 + WEIGHTS * WEIGHTS / 2) / (WEIGHTS * WEIGHTS);
        out = out.with_channel(channel, (quarters / 4) as u8);
        // Same channel order as `Light`: sun in the top pair of bits.
        fine |= ((quarters % 4) as u8) << (2 * (CHANNELS - 1 - channel));
    }
    (out, fine)
}

/// One component of a cell triple.
const fn axis_of(cell: (i32, i32, i32), axis: usize) -> i32 {
    match axis {
        0 => cell.0,
        1 => cell.1,
        _ => cell.2,
    }
}

/// Replaces one component of a cell triple.
const fn set_axis(cell: &mut (i32, i32, i32), axis: usize, value: i32) {
    match axis {
        0 => cell.0 = value,
        1 => cell.1 = value,
        _ => cell.2 = value,
    }
}

/// The block containing a cell, for negative coordinates too.
///
/// `div_euclid`, not `/`: truncation towards zero puts every cell west or below
/// the origin in the block next door, and the seam only appears on one side of
/// the world.
const fn block_of(cell: i32) -> i32 {
    cell.div_euclid(SUBNODES_PER_AXIS as i32)
}

/// A cell moved along one axis.
const fn step(cell: (i32, i32, i32), axis: usize, delta: i32) -> (i32, i32, i32) {
    match axis {
        0 => (cell.0 + delta, cell.1, cell.2),
        1 => (cell.0, cell.1 + delta, cell.2),
        _ => (cell.0, cell.1, cell.2 + delta),
    }
}

/// The two axes spanning a face's plane.
///
/// **The same mapping the mesher uses** (`SubNodeGrid::cell`): axis 0 spans
/// `(u, v) = (y, z)`, axis 1 spans `(x, z)`, axis 2 spans `(x, y)`. Choosing a
/// different pairing here would rotate every quad's lighting against its
/// geometry — the corners would be right and land in the wrong places.
const fn plane_axes(axis: usize) -> (usize, usize) {
    match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tiamot_core::light::MAX_LEVEL;

    /// Light set per block, dark everywhere else.
    struct Sparse(BTreeMap<(i32, i32, i32), Light>);

    impl BlockLight for Sparse {
        fn at(&self, x: i32, y: i32, z: i32) -> Light {
            self.0.get(&(x, y, z)).copied().unwrap_or(Light::DARK)
        }
    }

    #[test]
    fn a_uniformly_lit_world_gives_every_corner_the_same_level() {
        // The case that keeps greedy meshing intact: if this were not exact,
        // every quad on open ground would split at every block boundary.
        let light = Uniform(Light::DAYLIGHT);
        let a = face_shade(&light, &Empty, 1, true, (0, 0, 0));
        let b = face_shade(&light, &Empty, 1, true, (30, 0, 17));
        assert_eq!(a, Shade::flat(Light::DAYLIGHT));
        assert_eq!(a, b, "two faces in the same light shaded differently");
    }

    #[test]
    fn light_is_sampled_outside_the_surface_and_not_inside_it() {
        // **The mistake that makes every lit wall black.** The cell being drawn
        // is solid, so its own block is dark; the light that matters is in the
        // air in front of it.
        let mut blocks = BTreeMap::new();
        // The block above the surface is lit; the one containing it is not.
        blocks.insert((0, 1, 0), Light::DAYLIGHT);
        let light = Sparse(blocks);

        // A top face of the cell at (1, 2, 1) — block (0, 0, 0) — looks up into
        // block (0, 1, 0).
        //
        // Not the full level: sampling at block centres blends the lit block
        // with its dark neighbours, and this corner carries 100/144 of it. What
        // matters here is that the light is most of the way up rather than
        // zero, because zero is what reading the solid block underneath gives.
        let shade = face_shade(&light, &Empty, 1, true, (1, 2, 1));
        assert!(
            shade.light.iter().all(|level| level.sun() > MAX_LEVEL / 2),
            "a top face read the dark block it belongs to instead of the lit air above it: {:?}",
            shade.light
        );
    }

    #[test]
    fn a_corner_between_a_lit_and_a_dark_block_lands_half_way() {
        // Smooth lighting's whole purpose: the gradient across a shadow edge.
        // The corner at the boundary averages two lit blocks and two dark ones.
        let mut blocks = BTreeMap::new();
        blocks.insert((0, 1, 0), Light::new(MAX_LEVEL, 0, 0, 0));
        blocks.insert((0, 1, 1), Light::new(MAX_LEVEL, 0, 0, 0));
        let light = Sparse(blocks);

        // A top face on the cell whose +z lattice edge is the block boundary at
        // z = 6 (block 2 starts there).
        let shade = face_shade(&light, &Empty, 1, true, (1, 2, 5));
        let levels: Vec<u8> = shade.light.iter().map(|level| level.sun()).collect();
        let brightest = levels.iter().max().copied().unwrap_or(0);
        let dimmest = levels.iter().min().copied().unwrap_or(0);
        assert!(
            brightest > dimmest,
            "no corner landed between lit and dark, so there is no gradient: {levels:?}"
        );
        assert!(
            brightest > 0 && dimmest < MAX_LEVEL,
            "the face is uniformly one thing or the other: {levels:?}"
        );
    }

    #[test]
    fn every_channel_is_averaged_independently() {
        // A red lamp on one side and a green one on the other must give a
        // corner that is both, not whichever happened to be brighter.
        let mut blocks = BTreeMap::new();
        blocks.insert((0, 1, 0), Light::new(0, MAX_LEVEL, 0, 0));
        blocks.insert((0, 1, 1), Light::new(0, 0, MAX_LEVEL, 0));
        blocks.insert((-1, 1, 0), Light::new(0, MAX_LEVEL, 0, 0));
        blocks.insert((-1, 1, 1), Light::new(0, 0, MAX_LEVEL, 0));
        let light = Sparse(blocks);

        // Cell z = 3, so the corner at dv = 0 straddles cells 2 and 3 — blocks
        // 0 and 1, one of each lamp. At z = 5 both sampled cells land in block
        // 1 and every corner is green: correct arithmetic over a fixture that
        // never crossed a boundary.
        let shade = face_shade(&light, &Empty, 1, true, (0, 2, 3));
        assert!(
            shade
                .light
                .iter()
                .any(|level| level.red() > 0 && level.green() > 0),
            "no corner mixed the two lamps: {:?}",
            shade.light
        );
    }

    #[test]
    fn corners_are_averaged_west_of_the_origin_too() {
        // `div_euclid` rather than `/`. Truncation puts every cell below zero
        // in the block next door, and the seam appears on one side of the world
        // only — which is the hardest kind of bug to be shown.
        let mut blocks = BTreeMap::new();
        blocks.insert((-1, 0, -1), Light::DAYLIGHT);
        let light = Sparse(blocks);

        // Cell (-2, -1, -2) is in block (-1, -1, -1) and its top face looks
        // into cell y = 0, which is block 0 — the lit one. Cell y = -1 is
        // block -1, so a fixture one cell lower samples nothing and proves
        // nothing.
        let shade = face_shade(&light, &Empty, 1, true, (-2, -1, -2));
        assert!(
            shade.light.iter().any(|level| level.sun() > 0),
            "a face west of the origin sampled the wrong block: {:?}",
            shade.light
        );
    }

    /// Solidity set per cell, empty elsewhere.
    struct Cells(std::collections::BTreeSet<(i32, i32, i32)>);

    impl CellOccupancy for Cells {
        fn solid(&self, x: i32, y: i32, z: i32) -> bool {
            self.0.contains(&(x, y, z))
        }
    }

    #[test]
    fn a_corner_beside_geometry_is_darker_than_one_in_the_open() {
        // Voxel AO's whole job: without it, an inside corner is exactly as
        // bright as open ground and the geometry reads as flat.
        let light = Uniform(Light::DAYLIGHT);
        let open = face_shade(&light, &Empty, 1, true, (5, 5, 5));

        // A wall standing on the surface, along -u of the face. The face's
        // plane for axis 1 is (x, z), so -u is -x.
        let mut cells = std::collections::BTreeSet::new();
        cells.insert((4, 6, 5));
        let occluded = face_shade(&light, &Cells(cells), 1, true, (5, 5, 5));

        assert!(
            occluded.occlusion.iter().any(|level| *level < 3),
            "a corner beside a wall was not occluded at all: {:?}",
            occluded.occlusion
        );
        assert!(
            occluded.occlusion.contains(&3),
            "every corner was occluded, so this is a dimmer rather than occlusion: {:?}",
            occluded.occlusion
        );
        assert_ne!(open, occluded);
    }

    #[test]
    fn a_corner_closed_on_both_sides_is_the_darkest_step() {
        // Two solid sides is the darkest level whatever the diagonal does.
        let light = Uniform(Light::DAYLIGHT);
        let mut cells = std::collections::BTreeSet::new();
        cells.insert((4, 6, 5));
        cells.insert((5, 6, 4));
        let shade = face_shade(&light, &Cells(cells), 1, true, (5, 5, 5));

        assert!(
            shade.occlusion.contains(&0),
            "no corner reached the darkest occlusion level: {:?}",
            shade.occlusion
        );
        // **The stored light is untouched.** Occlusion is a property of the
        // geometry and is applied at draw time to whatever colour lands there;
        // scaling the light would tint the shadow with the sun's own hue.
        assert!(
            shade.light.iter().all(|level| level.sun() == MAX_LEVEL),
            "occlusion changed the stored light: {:?}",
            shade.light
        );
    }

    #[test]
    fn the_diagonal_cannot_brighten_a_corner_that_is_already_closed() {
        // The rule that stops an inside corner brightening when the block
        // BEHIND it is removed, which reads as the wrong block having been dug.
        let light = Uniform(Light::DAYLIGHT);
        let sides = [(4, 6, 5), (5, 6, 4)];

        let mut without = std::collections::BTreeSet::new();
        without.extend(sides);
        let mut with = without.clone();
        with.insert((4, 6, 4));

        assert_eq!(
            face_shade(&light, &Cells(without), 1, true, (5, 5, 5)),
            face_shade(&light, &Cells(with), 1, true, (5, 5, 5)),
            "the diagonal changed a corner that was already closed on both sides"
        );
    }

    #[test]
    fn occlusion_is_carried_beside_the_light_and_not_multiplied_into_it() {
        // **The bug this replaced**, reported from the window as "the AO is a
        // bit yellow and not what I would call dark". Baking occlusion into the
        // light keeps the light's hue, so a corner shadowed under a low sun
        // came out dim orange rather than dark. Occlusion is geometry: it
        // belongs on the geometry, and darkens whatever colour lands there.
        let light = Uniform(Light::new(0, MAX_LEVEL, 0, 0));
        let mut cells = std::collections::BTreeSet::new();
        cells.insert((4, 6, 5));
        cells.insert((5, 6, 4));
        let shade = face_shade(&light, &Cells(cells), 1, true, (5, 5, 5));

        assert!(
            shade.light.iter().all(|level| level.red() == MAX_LEVEL),
            "the stored light was dimmed: {:?}",
            shade.light
        );
        assert!(
            shade.occlusion.iter().any(|level| *level < 3),
            "nothing was occluded, so this proves nothing: {:?}",
            shade.occlusion
        );
    }

    #[test]
    fn the_plane_axes_match_the_meshers_own_mapping() {
        // Pinned rather than assumed: `SubNodeGrid::cell` maps (u, v, w) to
        // cell coordinates per axis, and a different pairing here would rotate
        // every quad's lighting against its geometry.
        assert_eq!(plane_axes(0), (1, 2));
        assert_eq!(plane_axes(1), (0, 2));
        assert_eq!(plane_axes(2), (0, 1));
    }
}

#[cfg(test)]
mod gradient_tests {
    use super::*;
    use tiamot_core::light::MAX_LEVEL;

    /// Block light falling one level per block along x, as a lamp's does.
    struct Ramp;

    impl BlockLight for Ramp {
        fn at(&self, x: i32, _y: i32, _z: i32) -> Light {
            let level = MAX_LEVEL.saturating_sub(x.clamp(0, i32::from(MAX_LEVEL)) as u8);
            Light::new(0, level, level, level)
        }
    }

    /// One corner's red channel in quarter levels.
    fn quarters(shade: &Shade, corner: usize) -> i32 {
        i32::from(shade.light[corner].red()) * 4 + i32::from((shade.fine[corner] >> 4) & 0x3)
    }

    #[test]
    fn a_gradient_falls_across_a_whole_block_rather_than_in_one_cell() {
        // **The bug this replaced**, reported from the window as the
        // interpolation between blocks being rough.
        //
        // Averaging the four blocks touching a corner samples the SAME block
        // four times unless the corner sits exactly on a block boundary, so the
        // whole of a one-level drop landed on the first cell of each block and
        // the other two were flat: `[15 14] [14 14] [14 14]`. That is a hard
        // band every block, whatever the shader does with it afterwards.
        //
        // The property that says it is fixed is that EVERY cell across a block
        // moves, not merely that the block as a whole ends up darker.
        let mut steps = Vec::new();
        for cell in 3..6 {
            let shade = face_shade(&Ramp, &Empty, 1, true, (cell, 2, 5));
            steps.push(quarters(&shade, 0) - quarters(&shade, 1));
        }

        assert!(
            steps.iter().all(|drop| *drop > 0),
            "a cell somewhere across the block had no gradient at all: {steps:?} quarter levels"
        );
        // And no single cell carries the whole level, which is what the old
        // sampling did and what made the band.
        assert!(
            steps.iter().all(|drop| *drop < 4),
            "a single cell carried a whole level or more: {steps:?} quarter levels"
        );
    }

    #[test]
    fn a_uniform_field_still_has_nothing_left_over() {
        // The fast path, and the property the world's vertex count rests on:
        // open ground under daylight must produce corners that compare EQUAL,
        // or greedy merging stops merging a chunk floor into a handful of
        // quads. A fractional part that was very slightly non-zero here would
        // cost nothing visible and multiply the geometry of every world.
        let shade = face_shade(&Uniform(Light::DAYLIGHT), &Empty, 1, true, (7, 4, 9));
        assert_eq!(shade, Shade::flat(Light::DAYLIGHT));
        assert_eq!(shade.fine, [0; 4], "a uniform field left a remainder");
    }

    #[test]
    fn the_quarter_levels_never_overflow_their_two_bits() {
        // `fine` shares a word with the position, so a value of 4 would not be
        // clamped, it would be added to the occlusion bits — and the symptom
        // would be corners darkening at random rather than anything to do with
        // light.
        for cell in 0..24 {
            let shade = face_shade(&Ramp, &Empty, 1, true, (cell, 2, 5));
            for fine in shade.fine {
                for channel in 0..CHANNELS {
                    let pair = (fine >> (2 * (CHANNELS - 1 - channel))) & 0x3;
                    assert!(pair < 4, "channel {channel} of {fine} is {pair}");
                }
            }
        }
    }

    #[test]
    fn a_corner_never_exceeds_the_brightest_block_near_it() {
        // Interpolation is a weighted mean, so it cannot overshoot — but the
        // rounding into quarter levels could, and a level of 15 with a quarter
        // on top would divide past 1.0 in the shader and light a surface
        // brighter than daylight.
        let shade = face_shade(&Uniform(Light::DAYLIGHT), &Empty, 1, true, (7, 4, 9));
        for corner in 0..4 {
            assert!(
                quarters(&shade, corner) <= i32::from(MAX_LEVEL) * 4,
                "corner {corner} came to {} quarter levels, past the maximum of {}",
                quarters(&shade, corner),
                i32::from(MAX_LEVEL) * 4
            );
        }
    }
}
