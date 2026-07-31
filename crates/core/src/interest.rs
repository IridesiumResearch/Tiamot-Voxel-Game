// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Which chunks a player should be sent, and in what order.
//!
//! Pure geometry over chunk coordinates. No world, no socket, no allocation of
//! anything but the result — so the rules that decide what a player can see are
//! testable in microseconds rather than through a network integration test.
//!
//! # A cylinder, not a sphere or a cube
//!
//! Players move horizontally far more than vertically, and a voxel world's
//! interesting content is a thin shell around the surface. The three candidates
//! at horizontal radius 8:
//!
//! - **Cube** (17×17×17): 4913 chunks, most of them straight up or straight
//!   down where nobody is looking.
//! - **Sphere** (r=8): 2145 chunks, but it reaches as far up as it does
//!   sideways, which spends the budget on sky.
//! - **Cylinder** (r=8, ±4): about 1800 chunks, and every one of them is
//!   somewhere a player might actually walk.
//!
//! The cylinder also matches how the vertical axis differs in kind: the world
//! is 120,000 blocks tall, but a player's *useful* vertical range is a few
//! chunks either side of where they stand.
//!
//! # Ordering is nearest-first, and stable
//!
//! A player dropping into a world wants the ground under their feet before the
//! horizon. Chunks are ordered by squared distance — no `sqrt`, which is
//! allowed in the deterministic subset but pointless here since the ordering is
//! identical either way — with a deterministic tie-break so two servers stream
//! in the same order and a test can assert on it.

use crate::coords::ChunkPos;

/// How far a player can see, in chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewDistance {
    /// Horizontal radius, in chunks.
    pub horizontal: u8,
    /// Vertical radius, in chunks.
    pub vertical: u8,
}

impl Default for ViewDistance {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl ViewDistance {
    /// The default: 8 chunks out, 4 up and down.
    ///
    /// 8 chunks is 128 blocks, which at 1 block = 1 yard is a horizon a little
    /// over a tenth of a mile away. Far enough to feel like a landscape, close
    /// enough that 50 players' interest sets fit in the tick budget.
    pub const DEFAULT: Self = Self {
        horizontal: 8,
        vertical: 4,
    };

    /// The smallest useful view: the chunk you are in and its neighbours.
    ///
    /// Anything less means a player can see the edge of the world they are
    /// standing on.
    pub const MINIMUM: Self = Self {
        horizontal: 1,
        vertical: 1,
    };

    /// The largest view an operator may configure.
    ///
    /// Not a matter of taste: interest volume grows with the square of the
    /// horizontal radius, so 32 is sixteen times the work of 8. A server that
    /// let an operator type 64 would appear to accept it and then fail to keep
    /// up in a way that looked like a bug in the engine.
    pub const MAXIMUM: Self = Self {
        horizontal: 32,
        vertical: 16,
    };

    /// Clamps a configured view distance into the supported range.
    ///
    /// Written with `if` rather than `Ord::clamp` because `Ord` is not const
    /// yet, and this wants to be usable in a const context.
    #[must_use]
    pub const fn clamped(horizontal: u8, vertical: u8) -> Self {
        Self {
            horizontal: clamp_u8(
                horizontal,
                Self::MINIMUM.horizontal,
                Self::MAXIMUM.horizontal,
            ),
            vertical: clamp_u8(vertical, Self::MINIMUM.vertical, Self::MAXIMUM.vertical),
        }
    }

    /// Upper bound on how many chunks a set at this distance can hold.
    ///
    /// The bounding box, not the cylinder — cheap to compute and only ever used
    /// to reserve capacity, where overshooting costs one allocation and
    /// undershooting costs several.
    #[must_use]
    pub const fn max_chunks(&self) -> usize {
        let horizontal = (self.horizontal as usize) * 2 + 1;
        let vertical = (self.vertical as usize) * 2 + 1;
        horizontal * horizontal * vertical
    }
}

const fn clamp_u8(value: u8, low: u8, high: u8) -> u8 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// Every chunk within `view` of `centre`, nearest first.
///
/// The order is deterministic: equal distances break by coordinate, so the same
/// centre and distance always produce the same sequence.
#[must_use]
pub fn chunks_around(centre: ChunkPos, view: ViewDistance) -> Vec<ChunkPos> {
    let horizontal = i32::from(view.horizontal);
    let vertical = i32::from(view.vertical);
    // Compared against squared distance, so the radius is squared once here
    // rather than a square root being taken per chunk.
    let radius_squared = i64::from(horizontal) * i64::from(horizontal);

    let mut chunks = Vec::with_capacity(view.max_chunks());
    for dy in -vertical..=vertical {
        for dz in -horizontal..=horizontal {
            for dx in -horizontal..=horizontal {
                // Horizontal distance only: the cylinder's vertical extent is
                // the loop bound, not part of the radius test.
                let planar = i64::from(dx) * i64::from(dx) + i64::from(dz) * i64::from(dz);
                if planar > radius_squared {
                    continue;
                }
                // Saturating: a player near the coordinate limit has a
                // truncated interest set rather than a wrapped one that would
                // stream chunks from the far side of the world.
                chunks.push(ChunkPos::new(
                    centre.x.saturating_add(dx),
                    centre.y.saturating_add(dy),
                    centre.z.saturating_add(dz),
                ));
            }
        }
    }

    chunks.sort_by_key(|pos| {
        (
            squared_distance(centre, *pos),
            // Deterministic tie-break. Without it the order among equidistant
            // chunks depends on iteration order, which makes a "nearest first"
            // test assert on something that is only accidentally true.
            pos.x,
            pos.y,
            pos.z,
        )
    });
    chunks
}

/// Whether a chunk is within `view` of `centre`.
#[must_use]
pub fn contains(centre: ChunkPos, view: ViewDistance, pos: ChunkPos) -> bool {
    let dy = i64::from(pos.y) - i64::from(centre.y);
    if dy.abs() > i64::from(view.vertical) {
        return false;
    }
    let dx = i64::from(pos.x) - i64::from(centre.x);
    let dz = i64::from(pos.z) - i64::from(centre.z);
    let radius = i64::from(view.horizontal);
    dx * dx + dz * dz <= radius * radius
}

/// Squared distance between two chunk positions.
///
/// `i64` throughout: chunk coordinates span the whole 120,000-block world, and
/// squaring an `i32` difference overflows well inside that range.
#[must_use]
pub fn squared_distance(a: ChunkPos, b: ChunkPos) -> i64 {
    let dx = i64::from(a.x) - i64::from(b.x);
    let dy = i64::from(a.y) - i64::from(b.y);
    let dz = i64::from(a.z) - i64::from(b.z);
    dx * dx + dy * dy + dz * dz
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: ChunkPos = ChunkPos::new(0, 0, 0);

    #[test]
    fn the_centre_chunk_comes_first() {
        // A player wants the ground under their feet before the horizon.
        let chunks = chunks_around(ORIGIN, ViewDistance::DEFAULT);
        assert_eq!(chunks[0], ORIGIN);
    }

    #[test]
    fn chunks_arrive_nearest_first() {
        let chunks = chunks_around(ORIGIN, ViewDistance::DEFAULT);
        let mut previous = 0;
        for pos in &chunks {
            let distance = squared_distance(ORIGIN, *pos);
            assert!(
                distance >= previous,
                "chunk {pos:?} at {distance} came after {previous}"
            );
            previous = distance;
        }
    }

    #[test]
    fn the_order_is_deterministic() {
        // Two servers must stream in the same order, and a test must be able to
        // assert on it. Without the coordinate tie-break this holds only by
        // accident of iteration order.
        let first = chunks_around(ChunkPos::new(3, -2, 7), ViewDistance::DEFAULT);
        let second = chunks_around(ChunkPos::new(3, -2, 7), ViewDistance::DEFAULT);
        assert_eq!(first, second);
    }

    #[test]
    fn the_set_is_a_cylinder_not_a_cube() {
        // The corners of the bounding box must be absent, or the "cylinder"
        // is a cube with extra steps and 2.7x the streaming cost.
        let view = ViewDistance {
            horizontal: 8,
            vertical: 4,
        };
        let chunks = chunks_around(ORIGIN, view);

        assert!(
            !chunks.contains(&ChunkPos::new(8, 0, 8)),
            "the horizontal corner is outside a cylinder of radius 8"
        );
        assert!(
            chunks.contains(&ChunkPos::new(8, 0, 0)),
            "but the axis is in"
        );
        assert!(
            chunks.len() < view.max_chunks(),
            "a cylinder must be smaller than its bounding box"
        );
    }

    #[test]
    fn the_vertical_extent_is_a_flat_cap_not_a_sphere() {
        // The vertical bound is the loop, not the radius: at full horizontal
        // reach a player should still see the chunk directly above them.
        let view = ViewDistance {
            horizontal: 8,
            vertical: 4,
        };
        let chunks = chunks_around(ORIGIN, view);
        assert!(
            chunks.contains(&ChunkPos::new(8, 4, 0)),
            "the top of the cylinder wall must be included, not cut off by a sphere"
        );
        assert!(
            !chunks.contains(&ChunkPos::new(0, 5, 0)),
            "but not above it"
        );
    }

    #[test]
    fn contains_agrees_with_the_generated_set() {
        // Two implementations of the same rule. If they disagree, a chunk gets
        // sent and then immediately unloaded, or never sent at all.
        let view = ViewDistance {
            horizontal: 5,
            vertical: 3,
        };
        let centre = ChunkPos::new(-4, 9, 2);
        let generated: std::collections::BTreeSet<_> =
            chunks_around(centre, view).into_iter().collect();

        for x in -12..=4 {
            for y in 3..=15 {
                for z in -6..=10 {
                    let pos = ChunkPos::new(x, y, z);
                    assert_eq!(
                        generated.contains(&pos),
                        contains(centre, view, pos),
                        "disagreement at {pos:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_set_has_no_duplicates() {
        let chunks = chunks_around(ChunkPos::new(1, 2, 3), ViewDistance::DEFAULT);
        let unique: std::collections::BTreeSet<_> = chunks.iter().copied().collect();
        assert_eq!(unique.len(), chunks.len(), "a chunk must not be sent twice");
    }

    #[test]
    fn the_minimum_view_is_still_a_neighbourhood() {
        // 3x3x3 minus the horizontal corners: a player must never see the edge
        // of the chunk they are standing on.
        let chunks = chunks_around(ORIGIN, ViewDistance::MINIMUM);
        for face in [
            ChunkPos::new(1, 0, 0),
            ChunkPos::new(-1, 0, 0),
            ChunkPos::new(0, 0, 1),
            ChunkPos::new(0, 0, -1),
            ChunkPos::new(0, 1, 0),
            ChunkPos::new(0, -1, 0),
        ] {
            assert!(chunks.contains(&face), "{face:?} must be loaded");
        }
    }

    #[test]
    fn a_configured_distance_is_clamped_into_the_supported_range() {
        // Interest volume grows with the square of the horizontal radius, so an
        // operator typing 200 must be corrected rather than obeyed into a
        // server that cannot keep up.
        assert_eq!(ViewDistance::clamped(200, 200), ViewDistance::MAXIMUM);
        assert_eq!(ViewDistance::clamped(0, 0), ViewDistance::MINIMUM);
        assert_eq!(
            ViewDistance::clamped(8, 4),
            ViewDistance::DEFAULT,
            "a value already in range passes through"
        );
    }

    #[test]
    fn a_centre_near_the_coordinate_limit_does_not_wrap() {
        // A wrapped interest set would stream chunks from the far side of the
        // world — visually absurd, and a cheap way to make a server load
        // thousands of unrelated chunks.
        let chunks = chunks_around(ChunkPos::new(i32::MAX, 0, i32::MAX), ViewDistance::MINIMUM);
        assert!(
            chunks.iter().all(|pos| pos.x > 0 && pos.z > 0),
            "coordinates must saturate rather than wrap"
        );
    }

    #[test]
    fn the_default_view_is_a_sane_number_of_chunks() {
        // A number worth knowing rather than discovering under load: this is
        // what one player costs at 50 players a server.
        let chunks = chunks_around(ORIGIN, ViewDistance::DEFAULT);
        assert!(
            (1500..2200).contains(&chunks.len()),
            "the default view is {} chunks, which is outside the range this \
             engine's budgets were sized for",
            chunks.len()
        );
    }
}
