// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Voxel raycasting — what the player is pointing at.
//!
//! Amanatides & Woo's grid traversal (*A Fast Voxel Traversal Algorithm for Ray
//! Tracing*, 1987), at **sub-node resolution**, which is the whole point: a
//! chiselled block is a shape, and pointing at it must name the cell under the
//! crosshair rather than the block containing it. Sub-Node Contract §2 makes
//! collision agree with what you see; this makes targeting agree too.
//!
//! # Determinism
//!
//! Charter rule 4 territory, same as the rest of [`super`]: the traversal is
//! divisions, additions and comparisons. There is one hazard worth naming — a
//! ray parallel to an axis has a zero direction component, and `1.0 / 0.0` is
//! an infinity that later multiplies by zero to give a `NaN`. Zero components
//! are therefore branched out and given a finite sentinel rather than allowed
//! to produce one. Simulation state must never contain a `NaN`, and the payload
//! of one is explicitly non-deterministic.

use crate::coords::{BlockPos, SubNodePos};
use crate::detgen::floor_to_i32;

use super::Solid;

/// How far a player can reach, in cells. 4.5 yards.
pub const REACH: f32 = 13.5;

/// A ray that stopped in a solid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    /// The solid cell the ray entered, in the caller's frame.
    pub cell: [i32; 3],
    /// Unit normal of the face it entered through, pointing back at the ray.
    ///
    /// Exactly one component is non-zero. Placement puts a block in
    /// `cell + normal`, which is why this points *out* of the surface.
    pub normal: [i32; 3],
    /// Which axis the normal is on, as an index. Convenience for callers that
    /// would otherwise search the normal for its non-zero component.
    pub axis: usize,
}

impl Hit {
    /// The sub-node cell, when the caller's frame is world sub-node
    /// coordinates.
    #[must_use]
    pub const fn subnode(&self) -> SubNodePos {
        SubNodePos::new(self.cell[0], self.cell[1], self.cell[2])
    }

    /// The block containing the cell, when the caller's frame is world
    /// sub-node coordinates.
    ///
    /// `div_euclid` rather than a plain division: at negative coordinates
    /// truncation rounds towards zero and puts the cell in the block next door,
    /// which is the bug that only ever shows up west of the origin.
    #[must_use]
    pub const fn block(&self) -> BlockPos {
        let n = crate::SUBNODES_PER_AXIS as i32;
        BlockPos::new(
            self.cell[0].div_euclid(n),
            self.cell[1].div_euclid(n),
            self.cell[2].div_euclid(n),
        )
    }

    /// The cell a block placed against this face would occupy.
    #[must_use]
    pub const fn placement(&self) -> [i32; 3] {
        [
            self.cell[0] + self.normal[0],
            self.cell[1] + self.normal[1],
            self.cell[2] + self.normal[2],
        ]
    }
}

/// Walks the grid from `origin` along `direction` until it enters a solid cell.
///
/// `direction` need not be normalised; `reach` is measured in cells along the
/// normalised direction, so the caller gets the distance limit it asked for
/// whatever the vector's length. Returns `None` if nothing solid is within
/// reach, and `None` for a zero-length direction — there is no sensible answer
/// and the alternative is dividing by zero.
#[must_use]
pub fn cast(solid: &impl Solid, origin: [f32; 3], direction: [f32; 3], reach: f32) -> Option<Hit> {
    let direction = normalise(direction)?;

    let mut cell = [
        floor_to_i32(origin[0]),
        floor_to_i32(origin[1]),
        floor_to_i32(origin[2]),
    ];

    // Standing inside geometry — the eye is in a block. Report the cell it is
    // in, with a normal facing back the way the ray came, so a caller is never
    // handed a hit it cannot place against.
    if solid.solid(cell[0], cell[1], cell[2]) {
        let axis = dominant_axis(direction);
        let mut normal = [0, 0, 0];
        normal[axis] = if direction[axis] > 0.0 { -1 } else { 1 };
        return Some(Hit { cell, normal, axis });
    }

    let mut step = [0i32; 3];
    // Distance along the ray to the first boundary on each axis, and the
    // distance between successive boundaries.
    let mut next = [f32::MAX; 3];
    let mut delta = [f32::MAX; 3];

    for axis in 0..3 {
        if direction[axis] > 0.0 {
            step[axis] = 1;
            let boundary = (cell[axis] + 1) as f32;
            next[axis] = (boundary - origin[axis]) / direction[axis];
            delta[axis] = 1.0 / direction[axis];
        } else if direction[axis] < 0.0 {
            step[axis] = -1;
            let boundary = cell[axis] as f32;
            next[axis] = (boundary - origin[axis]) / direction[axis];
            delta[axis] = -1.0 / direction[axis];
        }
        // A zero component keeps step 0 and both distances at `f32::MAX`, so
        // that axis never wins the comparison below and never divides by zero.
    }

    // Every iteration crosses one cell boundary, so the count is bounded by the
    // reach in cells along the three axes.
    loop {
        // The axis whose next boundary is nearest is the one the ray crosses.
        let axis = if next[0] < next[1] && next[0] < next[2] {
            0
        } else if next[1] < next[2] {
            1
        } else {
            2
        };

        if next[axis] > reach {
            return None;
        }

        cell[axis] += step[axis];
        next[axis] += delta[axis];

        if solid.solid(cell[0], cell[1], cell[2]) {
            let mut normal = [0, 0, 0];
            // Entered through the face on the side the ray came from.
            normal[axis] = -step[axis];
            return Some(Hit { cell, normal, axis });
        }
    }
}

/// Scales a vector to unit length, or `None` if it has none.
fn normalise(v: [f32; 3]) -> Option<[f32; 3]> {
    let square = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    if square <= 0.0 {
        return None;
    }
    let length = square.sqrt();
    Some([v[0] / length, v[1] / length, v[2] / length])
}

/// The axis a direction points along most strongly.
fn dominant_axis(direction: [f32; 3]) -> usize {
    let x = direction[0].abs();
    let y = direction[1].abs();
    let z = direction[2].abs();
    if x >= y && x >= z {
        0
    } else if y >= z {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    struct Cells(BTreeSet<(i32, i32, i32)>);

    impl Cells {
        fn of(cells: &[(i32, i32, i32)]) -> Self {
            Self(cells.iter().copied().collect())
        }
    }

    impl Solid for Cells {
        fn solid(&self, x: i32, y: i32, z: i32) -> bool {
            self.0.contains(&(x, y, z))
        }
    }

    #[test]
    fn a_ray_stops_at_the_first_solid_cell_and_names_the_face_it_entered() {
        let world = Cells::of(&[(5, 0, 0), (7, 0, 0)]);
        let hit = cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], REACH).expect("should hit");

        assert_eq!(hit.cell, [5, 0, 0], "stopped at the wrong cell");
        assert_eq!(
            hit.normal,
            [-1, 0, 0],
            "the normal must point back at the ray, not along it"
        );
        assert_eq!(hit.placement(), [4, 0, 0], "placement goes in front of it");
    }

    #[test]
    fn a_ray_passes_through_the_empty_cells_of_a_chiselled_block() {
        // The reason this is at sub-node resolution at all. The block at
        // (1, 0, 0) in block coordinates spans cells x 3..6; only its far
        // column is left. A block-resolution raycast would stop at the near
        // face of a block that is mostly air.
        let world = Cells::of(&[(5, 0, 0), (5, 1, 0), (5, 2, 0)]);
        let hit = cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], REACH).expect("should hit");

        assert_eq!(
            hit.cell,
            [5, 0, 0],
            "stopped short of the occupied cell, at block resolution"
        );
        assert_eq!(hit.block(), BlockPos::new(1, 0, 0));
        assert_eq!(
            hit.subnode(),
            SubNodePos::new(5, 0, 0),
            "the sub-node under the crosshair is what a chisel needs"
        );
    }

    #[test]
    fn nothing_within_reach_is_a_miss() {
        let world = Cells::of(&[(40, 0, 0)]);
        assert!(
            cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], REACH).is_none(),
            "hit something {} cells away with a reach of {REACH}",
            40
        );
        // And the same cell inside reach is a hit, so the miss above is the
        // reach limit rather than a traversal that never finds anything.
        assert!(cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], 64.0).is_some());
    }

    #[test]
    fn a_ray_along_each_axis_and_direction_finds_its_neighbour() {
        // Six one-cell casts. The negative directions are where an
        // off-by-one in the boundary distance shows up, because the boundary
        // is the cell's own coordinate rather than the next one.
        let world = Cells::of(&[
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ]);
        let origin = [0.5, 0.5, 0.5];

        for (direction, cell, normal) in [
            ([1.0, 0.0, 0.0], [1, 0, 0], [-1, 0, 0]),
            ([-1.0, 0.0, 0.0], [-1, 0, 0], [1, 0, 0]),
            ([0.0, 1.0, 0.0], [0, 1, 0], [0, -1, 0]),
            ([0.0, -1.0, 0.0], [0, -1, 0], [0, 1, 0]),
            ([0.0, 0.0, 1.0], [0, 0, 1], [0, 0, -1]),
            ([0.0, 0.0, -1.0], [0, 0, -1], [0, 0, 1]),
        ] {
            let hit = cast(&world, origin, direction, REACH)
                .unwrap_or_else(|| panic!("{direction:?} hit nothing"));
            assert_eq!(hit.cell, cell, "wrong cell for {direction:?}");
            assert_eq!(hit.normal, normal, "wrong normal for {direction:?}");
        }
    }

    #[test]
    fn a_diagonal_ray_enters_through_the_face_it_actually_crossed() {
        // The case that separates a real traversal from "step along the ray in
        // small increments and floor it": which face was crossed is a fact
        // about the boundary distances, not about sample spacing.
        let world = Cells::of(&[(3, 3, 0)]);
        let hit = cast(&world, [0.5, 0.1, 0.5], [1.0, 1.0, 0.0], REACH).expect("should hit");

        assert_eq!(hit.cell, [3, 3, 0]);
        assert_eq!(
            hit.normal.iter().filter(|c| **c != 0).count(),
            1,
            "a face normal has exactly one non-zero component: {:?}",
            hit.normal
        );
    }

    #[test]
    fn an_eye_inside_geometry_still_gets_a_usable_hit() {
        let world = Cells::of(&[(0, 0, 0)]);
        let hit = cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], REACH).expect("should hit");
        assert_eq!(hit.cell, [0, 0, 0]);
        assert_eq!(hit.normal, [-1, 0, 0], "should face back down the ray");
    }

    #[test]
    fn a_zero_direction_is_a_miss_rather_than_a_nan() {
        // Charter rule 4: simulation code must not produce a NaN. A zero
        // direction is the input that would, via 1.0 / 0.0 and then inf × 0.
        let world = Cells::of(&[(0, 0, 0), (1, 0, 0)]);
        assert!(cast(&world, [0.5, 0.5, 0.5], [0.0, 0.0, 0.0], REACH).is_none());
    }

    #[test]
    fn an_unnormalised_direction_reaches_exactly_as_far() {
        // Reach is in cells, so a longer vector must not buy a longer arm.
        let world = Cells::of(&[(20, 0, 0)]);
        let long = cast(&world, [0.5, 0.5, 0.5], [1000.0, 0.0, 0.0], REACH);
        let unit = cast(&world, [0.5, 0.5, 0.5], [1.0, 0.0, 0.0], REACH);
        assert_eq!(long, unit, "the direction's length changed the reach");
        assert!(long.is_none());
    }

    #[test]
    fn a_block_position_is_correct_west_of_the_origin() {
        // `as i32` truncates towards zero, which puts every negative cell in
        // the wrong block. The symptom is a one-block seam that only exists on
        // one side of the origin.
        let hit = Hit {
            cell: [-1, -3, -4],
            normal: [1, 0, 0],
            axis: 0,
        };
        assert_eq!(hit.block(), BlockPos::new(-1, -1, -2));
    }
}
