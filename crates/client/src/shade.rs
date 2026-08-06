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

/// The light at a quad's four corners, in the order the mesher emits them.
///
/// `[(0,0), (1,0), (1,1), (0,1)]` in the quad's own `(u, v)` plane — the same
/// circulation [`crate::mesher::Mesh::to_buffers`] walks, so corner `n` here is
/// vertex `n` there.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shade(pub [Light; 4]);

impl Shade {
    /// One level at every corner.
    #[must_use]
    pub const fn flat(level: Light) -> Self {
        Self([level; 4])
    }
}

/// The light at the four corners of one cell's face.
///
/// `cell` is the solid cell being drawn and `(axis, positive)` its face, so the
/// light is sampled one cell *outside* it — the air in front of a surface,
/// never the surface itself. Sampling the solid cell would read the darkness
/// inside the block and make every lit wall black.
#[must_use]
pub fn face_shade(
    light: &impl BlockLight,
    axis: usize,
    positive: bool,
    cell: (i32, i32, i32),
) -> Shade {
    let (u_axis, v_axis) = plane_axes(axis);
    let outside = step(cell, axis, if positive { 1 } else { -1 });

    let mut corners = [Light::DARK; 4];
    // The mesher's corner order. `(du, dv)` picks which of the four lattice
    // points around the cell face this corner sits on.
    for (index, (du, dv)) in [(0, 0), (1, 0), (1, 1), (0, 1)].into_iter().enumerate() {
        corners[index] = corner_light(light, outside, u_axis, v_axis, du, dv);
    }
    Shade(corners)
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
    let mut totals = [0u32; CHANNELS];
    for i in 0..2 {
        for j in 0..2 {
            // The four cells sharing this lattice point, in the plane of the
            // face and on its outside layer.
            let cell = step(step(outside, u_axis, du - 1 + i), v_axis, dv - 1 + j);
            let level = light.at(block_of(cell.0), block_of(cell.1), block_of(cell.2));
            for (channel, total) in totals.iter_mut().enumerate() {
                *total += u32::from(level.channel(channel));
            }
        }
    }

    let mut out = Light::DARK;
    for (channel, total) in totals.into_iter().enumerate() {
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
        let a = face_shade(&light, 1, true, (0, 0, 0));
        let b = face_shade(&light, 1, true, (30, 0, 17));
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
        let shade = face_shade(&light, 1, true, (1, 2, 1));
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
        let shade = face_shade(&light, 1, true, (1, 2, 5));
        let levels: Vec<u8> = shade.0.iter().map(|level| level.sun()).collect();
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
        let shade = face_shade(&light, 1, true, (0, 2, 3));
        assert!(
            shade
                .0
                .iter()
                .any(|level| level.red() > 0 && level.green() > 0),
            "no corner mixed the two lamps: {:?}",
            shade.0
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
        let shade = face_shade(&light, 1, true, (-2, -1, -2));
        assert!(
            shade.0.iter().any(|level| level.sun() > 0),
            "a face west of the origin sampled the wrong block: {:?}",
            shade.0
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
