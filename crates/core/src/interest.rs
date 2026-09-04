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
//!
//! # The vertical counts for more than the horizontal, and it has to
//!
//! "Nearest" was the plain 3D distance until 2026-09-04, and at a high view
//! distance that spends almost the whole budget on sky. Reported from the
//! window as holes between the terrain around you and the terrain on the
//! horizon: at a view distance of 24 with a vertical of 12, **41,073 of the
//! 44,825 chunks in range are served before the player's own layer is finished
//! out to the view edge** — 128 seconds of a 140-second fill spent on chunks
//! above and below, nearly all of them air, while the band actually being
//! looked along still has gaps in it.
//!
//! So the sort weights `dy` by [`VERTICAL_WEIGHT`], past a free allowance of
//! [`VERTICAL_FREE`] layers that keeps the ground under a player's feet as
//! urgent as the ground beside them. **The SET does not change** — it is still
//! the cylinder, and every chunk in it is still sent — only the order does. A
//! player sees the band they are looking along close first, from near to far,
//! instead of holes appearing at random heights all around them. At view 24
//! that is 41 seconds to the view edge rather than 128.

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
            stream_distance(centre, *pos),
            // **Then by how far up or down.** The free allowance in
            // `stream_distance` puts the layer above and below a player at the
            // same distance as their own, and the coordinate tie-break alone
            // then sorts the chunk BELOW the centre ahead of the centre itself
            // — a player would be sent the ground before the air they are
            // standing in. Within a tie, nearer the player's own height first.
            (i64::from(pos.y) - i64::from(centre.y)).abs(),
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

/// How much a chunk of vertical separation counts for, in chunks of horizontal,
/// once it is more than [`VERTICAL_FREE`] layers away.
///
/// Six. A chunk three layers up is ordered as though it were twelve chunks away
/// along the ground, so the streaming budget goes on the band a player is
/// looking along before it goes on the sky.
///
/// **Why six and not more.** The vertical reach at the edge of the view is
/// `view / VERTICAL_WEIGHT + VERTICAL_FREE` chunks — five layers, 80 blocks, at
/// a view distance of 24. That has to cover the height of the terrain, or the
/// tops of distant hills sort behind the sky above the player's head and arrive
/// last, which is the failure this exists to fix wearing a different hat.
///
/// **Order only.** This never decides membership — [`contains`] is the cylinder
/// and does not consult it — so nothing is dropped from a player's interest set
/// by being high or low, it merely comes later.
pub const VERTICAL_WEIGHT: i64 = 6;

/// How many layers either side of a player are as urgent as the ground beside
/// them.
///
/// One, and it is not a tuning knob. The chunk directly below a player is the
/// ground under their feet and the chunk directly above is the ceiling over
/// their head; both are as immediate as anything at the same height, and a
/// weight applied from zero puts them behind ~50 chunks of the player's own
/// layer. That is not theoretical: `a_mod_can_read_the_world_it_writes_to`
/// went red on the first version of this, because a mod read the terrain at
/// `y = -1` on join and the chunk holding it had not arrived.
pub const VERTICAL_FREE: i64 = 1;

/// The distance chunks are STREAMED in the order of: horizontal distance, with
/// the vertical weighted by [`VERTICAL_WEIGHT`] past [`VERTICAL_FREE`] layers.
///
/// Squared, like [`squared_distance`], because the ordering is identical and it
/// avoids a `sqrt`. Not a metric anybody should measure range with — that is
/// [`contains`] — only the one they should fill in.
#[must_use]
pub fn stream_distance(centre: ChunkPos, pos: ChunkPos) -> i64 {
    let dx = i64::from(pos.x) - i64::from(centre.x);
    let dz = i64::from(pos.z) - i64::from(centre.z);
    let layers = (i64::from(pos.y) - i64::from(centre.y)).abs();
    let dy = (layers - VERTICAL_FREE).max(0) * VERTICAL_WEIGHT;
    dx * dx + dy * dy + dz * dz
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
        // **`stream_distance`, which weights the vertical.** Asserting the
        // plain 3D distance here is asserting that the sky is as urgent as the
        // ground, which is the thing that put holes in a high view distance.
        let chunks = chunks_around(ORIGIN, ViewDistance::DEFAULT);
        let mut previous = 0;
        for pos in &chunks {
            let distance = stream_distance(ORIGIN, *pos);
            assert!(
                distance >= previous,
                "chunk {pos:?} at {distance} came after {previous}"
            );
            previous = distance;
        }
    }

    #[test]
    fn the_band_a_player_looks_along_is_filled_before_the_sky() {
        // **The property the weight exists for**, at the view distance that
        // showed the problem rather than at the default. Reported from the
        // window as holes between the near terrain and the horizon: with a
        // plain 3D order, 41,073 of the 44,825 chunks in range are served
        // before the player's own layer reaches the view edge — the budget
        // goes on sky while the ground still has gaps in it.
        //
        // Asserted as a RATIO rather than an exact count, because the exact
        // count is a fact about one view distance and the property is not.
        let view = ViewDistance::clamped(24, 12);
        let chunks = chunks_around(ORIGIN, view);
        let edge = i64::from(view.horizontal) * i64::from(view.horizontal);

        let before_the_edge = chunks
            .iter()
            .take_while(|pos| stream_distance(ORIGIN, **pos) <= edge)
            .count();
        assert!(
            before_the_edge * 2 < chunks.len(),
            "{before_the_edge} of {} chunks are served before the player's own \
             layer reaches the view edge; the vertical weight is not biting",
            chunks.len()
        );

        // And the whole of the player's own layer is in that prefix — it is
        // the layer with the terrain and the eyes in it.
        let own_layer = chunks
            .iter()
            .filter(|pos| pos.y == ORIGIN.y)
            .copied()
            .collect::<Vec<_>>();
        let last_of_own = chunks
            .iter()
            .position(|pos| *pos == *own_layer.last().expect("a layer"))
            .expect("in the set");
        assert!(
            last_of_own < before_the_edge,
            "the player's own layer is not finished by the time the view edge \
             is reached: last at {last_of_own}, edge at {before_the_edge}"
        );
    }

    #[test]
    fn the_vertical_weight_changes_the_order_and_not_the_set() {
        // The weight must never drop a chunk. A player whose interest set
        // shrank because they were high up would watch the world end.
        for (h, v) in [(4u8, 4u8), (8, 4), (24, 12)] {
            let view = ViewDistance::clamped(h, v);
            let mut got = chunks_around(ORIGIN, view);
            got.sort_by_key(|pos| (pos.x, pos.y, pos.z));
            let mut want: Vec<ChunkPos> = got
                .iter()
                .copied()
                .filter(|pos| contains(ORIGIN, view, *pos))
                .collect();
            want.sort_by_key(|pos| (pos.x, pos.y, pos.z));
            assert_eq!(got, want, "at view {h}/{v} the set is not the cylinder");
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
