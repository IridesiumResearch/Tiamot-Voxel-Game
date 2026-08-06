// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Turning block-resolution light into per-vertex colour.
//!
//! # Smooth lighting, and why it does not ruin greedy meshing
//!
//! A vertex takes the average of the four block light values touching its
//! corner on the *outside* of the face — the classic smooth-lighting sample.
//! Hardware interpolation across the quad does the rest, so a surface gets a
//! gradient rather than a per-block staircase.
//!
//! That forces a rule on the mesher: **two cell faces may only merge if their
//! corner light agrees.** Merging across a lighting discontinuity would
//! interpolate straight through it and smear a shadow edge into a soft gradient
//! several blocks wide.
//!
//! The cost of that rule is smaller than it looks, because light is stored per
//! **block** and a block is three cells across. Every cell face inside one
//! block samples the same four neighbouring blocks, so it merges freely; and on
//! a uniformly lit surface — open ground under daylight, which is most of a
//! world — every block agrees too, so merging is completely unaffected.
//! Splitting only happens where light genuinely varies, which is at shadow
//! edges, which is exactly where the extra vertices are worth having.
//!
//! # This is presentation, not simulation
//!
//! Charter rule 4's float rules do not reach here: nothing in this file feeds
//! the determinism gate or the tick. The averaging is integer arithmetic anyway,
//! which is a happy accident of light being four-bit rather than a requirement.

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
            occlusion: self.occlusion[index],
        }
    }

    /// One level at every corner, nothing occluded.
    #[must_use]
    pub const fn flat(level: Light) -> Self {
        Self {
            light: [level; 4],
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
        shade.light[index] = corner_light(light, outside, u_axis, v_axis, du, dv);
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

/// The average of the four blocks touching one corner of a face.
fn corner_light(
    light: &impl BlockLight,
    outside: (i32, i32, i32),
    u_axis: usize,
    v_axis: usize,
    du: i32,
    dv: i32,
) -> Light {
    let mut samples = [Light::DARK; 4];
    for i in 0..2 {
        for j in 0..2 {
            // The four cells sharing this lattice point, in the plane of the
            // face and on its outside layer.
            let cell = step(step(outside, u_axis, du - 1 + i), v_axis, dv - 1 + j);
            samples[(i * 2 + j) as usize] =
                light.at(block_of(cell.0), block_of(cell.1), block_of(cell.2));
        }
    }

    // **The overwhelmingly common case, and worth its own path.** Light is per
    // block and a block is three cells across, so most corners sit inside one
    // block's neighbourhood and all four samples are the same value. Averaging
    // four copies of a number channel by channel — unpacking, summing, rounding
    // and repacking sixteen nibbles — costs more than the comparison that
    // avoids it, and this runs for every corner of every face in a chunk.
    if samples[1] == samples[0] && samples[2] == samples[0] && samples[3] == samples[0] {
        return samples[0];
    }

    let mut out = Light::DARK;
    for channel in 0..CHANNELS {
        let total: u32 = samples
            .iter()
            .map(|level| u32::from(level.channel(channel)))
            .sum();
        // Rounded rather than truncated: four corners each losing three
        // quarters of a level is a visible step darker across a whole surface.
        out = out.with_channel(channel, ((total + 2) / 4) as u8);
    }
    out
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
        let shade = face_shade(&light, &Empty, 1, true, (1, 2, 1));
        assert_eq!(
            shade,
            Shade::flat(Light::DAYLIGHT),
            "a top face read the dark block it belongs to instead of the lit air above it"
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
        assert!(
            levels.contains(&MAX_LEVEL),
            "no corner was fully lit: {levels:?}"
        );
        assert!(
            levels.iter().any(|level| *level < MAX_LEVEL && *level > 0),
            "no corner landed between lit and dark, so there is no gradient: {levels:?}"
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
