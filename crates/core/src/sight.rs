// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Whether one point in the world can see another.
//!
//! # Why the engine answers this and not the mod
//!
//! A mod could walk the line itself if it could read terrain, and it
//! deliberately cannot: `game.get_block_id` is a registry lookup and
//! `game.set_block` queues a write, so before this module there was no way for
//! a script to ask what is at a place at all. That was not an oversight to
//! correct by adding `game.is_solid(x, y, z)` — a per-cell reader would put a
//! Lua call and a chunk lookup in the inner loop of every perception check a
//! mob makes, at 20 Hz, per mob. The task's own wording is "perception helpers
//! implemented natively for cheapness", and this is that: **the mod names two
//! points and the engine walks the cells**, so the traversal runs at Rust speed
//! and the mod never holds a cell coordinate.
//!
//! It also keeps charter rule 1 honest in the other direction. Nothing here
//! decides what a mob does about what it can see; it answers one question about
//! geometry, and every judgement built on the answer stays in the mod.
//!
//! # A ray, at sub-node resolution
//!
//! [`crate::phys::ray::cast`] already walks the grid a cell at a time, and it is
//! the same traversal the player's crosshair uses. Reusing it means a mob and
//! a pickaxe agree about what a chiselled block blocks, which is Sub-Node
//! Contract §2 applied to sight — a slab you can see over is a slab you can
//! stand on, and there is one implementation deciding both.
//!
//! # Range is capped, and the cap is the engine's
//!
//! A sight test costs cells walked, so an uncapped one lets a mod spend
//! unbounded tick budget from Lua by naming a distant point. [`MAX_RANGE_BLOCKS`]
//! bounds it, and anything beyond reads as blocked rather than as an error: a
//! mod asking whether it can see something 400 yards away is asking about
//! terrain that is not loaded anyway, and "no" is both the cheap answer and the
//! true one.
//!
//! # Determinism
//!
//! Charter rule 4 applies in full — a mob deciding to move because it saw
//! something changes the world, so two servers must see the same things. The
//! traversal is [`crate::phys::ray`]'s, already inside the Deterministic Float
//! Subset, and the frame conversion here is multiplication, subtraction and
//! `sqrt`. No transcendental, no accumulation over an unordered iteration.

use crate::coords::ChunkPos;
use crate::phys::{ChunkLookup, Voxels, ray};

/// How far a sight test will look, in blocks.
///
/// Sixty-four yards, comfortably past any distance a mob has business caring
/// about and well inside a normal view distance. The number is a budget rather
/// than a taste: the traversal crosses up to three cell boundaries per cell of
/// travel, so this bounds one call at roughly six hundred iterations, and two
/// hundred mobs each asking once a tick at full range at the top of that budget.
pub const MAX_RANGE_BLOCKS: f64 = 64.0;

/// What the engine can tell a mod about a line between two points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sighting {
    /// Nothing solid stands between them.
    Clear,
    /// Something does — or the line is longer than [`MAX_RANGE_BLOCKS`], or it
    /// crosses terrain that is not loaded. All three are "no" to the only
    /// question a mob is really asking, and none of them is an error.
    Blocked,
    /// There is no world to look through right now.
    ///
    /// **Not the same as blocked**, and the difference is the whole reason this
    /// variant exists: a mod that gets `Blocked` learned something about the
    /// world, and a mod that gets this learned only that it asked at a moment
    /// when the engine could not answer — during worldgen, or in a test with no
    /// server behind the VM. See [`Access`] for when that is.
    Unavailable,
}

/// The seam a mod reaches terrain through.
///
/// The same arrangement [`crate::fluid::Access`] and [`crate::ent::Access`] use,
/// with one difference that matters: those stores sit behind their own locks for
/// the whole run, and the world does not. The tick thread owns `World` by value
/// and holds it mutably through chunk generation and every edit it applies, so
/// there is no moment-independent handle to hand out — which is why this trait
/// has [`Sighting::Unavailable`] and the others have no equivalent.
///
/// The server implementation lends the world into a slot for exactly the part of
/// the tick that runs mod callbacks and takes it back afterwards, so a mod that
/// asks from `on_tick` or an entity's `on_step` gets a real answer and a mod
/// that asks from `on_generate` gets `Unavailable`. The second case is not a
/// limitation to apologise for: a generator is *making* the chunk it would be
/// asking about, and any answer would be about a world that does not exist yet.
pub trait Access: Send + Sync {
    /// Whether `from` can see `to`. Both in world blocks, as a mod speaks them.
    fn line_of_sight(&self, from: [f64; 3], to: [f64; 3]) -> Sighting;

    /// What one block holds.
    ///
    /// **The question every world-aware mod starts with.** A chest, a furnace,
    /// a crop, a door, a wire: each of them begins with "what is at this
    /// position", and until this existed a mod could WRITE a block and never
    /// read one back.
    fn block_at(&self, pos: crate::coords::BlockPos) -> Reading;
}

/// What the engine can tell a mod about one block.
///
/// **Air is an answer and absence is not.** A mod that could not tell "there is
/// nothing here" from "I cannot say" would eventually build into terrain that
/// was merely unloaded, and the mistake would be a hole in somebody's house
/// rather than an error anybody could catch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// One material, filling the cells set in `occupancy`.
    ///
    /// Covers all of the contract's `Uniform` and `Partial`, and air: air is
    /// [`crate::MaterialId::AIR`] with an empty mask. Sub-Node Contract §0.
    Single {
        /// What it is made of.
        material: crate::MaterialId,
        /// Which of its 27 cells are filled, indexed `x + 3y + 9z`.
        occupancy: u32,
    },

    /// Two or more materials, one entry per cell in the same index order.
    ///
    /// Boxed because this variant is the rare one and the enum is returned by
    /// value from a call a mod may make thousands of times a tick.
    Mixed(Box<[crate::MaterialId; crate::block::SUBNODES_PER_BLOCK]>),

    /// The chunk is not loaded, so the engine does not know.
    ///
    /// **Never generated to find out.** A mod asking about somewhere far away
    /// must not be able to make the server generate a chunk inside the tick
    /// budget, one call at a time, with nobody near it.
    Absent,

    /// There is no world to read right now — during worldgen, or in a test with
    /// no server behind the VM. See [`Access`].
    Unavailable,
}

/// Whether nothing solid stands between two world points.
///
/// Points, not bodies: this is a single line between two positions, and a caller
/// that means "can this mob see that one" is responsible for raising both ends
/// to eye height. The engine does not know where a mod's mob keeps its eyes, and
/// a line drawn between two sets of feet clips the floor they are standing on.
///
/// Absent chunks read as solid, which is [`crate::phys::Solid`]'s rule
/// everywhere else and the conservative one here: a mob does not get to see
/// through terrain the server has not loaded.
#[must_use]
pub fn between(chunks: &impl ChunkLookup, from: [f64; 3], to: [f64; 3]) -> bool {
    let Some((origin, start, end)) = frame(from, to) else {
        return false;
    };

    let direction = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
    let distance =
        (direction[0] * direction[0] + direction[1] * direction[1] + direction[2] * direction[2])
            .sqrt();

    // Two points in the same cell see each other trivially, and a zero-length
    // direction has no axis for the traversal to step along — `cast` answers
    // `None` for one, which would read as "clear" anyway, but saying so here
    // means the answer does not depend on that.
    if distance <= f32::EPSILON {
        return true;
    }

    let voxels = Voxels::new(chunks, origin);
    ray::cast(&voxels, start, direction, distance).is_none()
}

/// Puts two world points in one chunk-anchored frame, or refuses.
///
/// Charter rule 7: the two points are converted **once**, into cells measured
/// from the first point's chunk, and every step of the traversal after that is
/// small-number arithmetic. Casting world blocks straight to `f32` and
/// subtracting would lose the sub-node part of the answer sixty thousand blocks
/// out, where the world still has fifty thousand blocks to go.
///
/// `None` when either point is not finite, or when they are further apart than
/// [`MAX_RANGE_BLOCKS`]. Both mean the caller gets "blocked" — see [`Sighting`].
fn frame(from: [f64; 3], to: [f64; 3]) -> Option<(ChunkPos, [f32; 3], [f32; 3])> {
    if !from.iter().chain(to.iter()).all(|value| value.is_finite()) {
        return None;
    }

    // Squared, so the range test needs no root of its own.
    let span = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    let square = span[0] * span[0] + span[1] * span[1] + span[2] * span[2];
    if square > MAX_RANGE_BLOCKS * MAX_RANGE_BLOCKS {
        return None;
    }

    let anchor = crate::ent::Transform::from_world(from[0], from[1], from[2]);
    let origin = anchor.chunk;

    let blocks = f64::from(crate::CHUNK_BLOCKS);
    let cells = f64::from(crate::SUBNODES_PER_AXIS);
    let corner = [
        f64::from(origin.x) * blocks,
        f64::from(origin.y) * blocks,
        f64::from(origin.z) * blocks,
    ];
    let local = |point: [f64; 3]| {
        [
            ((point[0] - corner[0]) * cells) as f32,
            ((point[1] - corner[1]) * cells) as f32,
            ((point[2] - corner[2]) * cells) as f32,
        ]
    };

    Some((origin, local(from), local(to)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::block::BlockValue;
    use crate::chunk::Chunk;
    use crate::coords::BlockPos;
    use crate::material::MaterialId;

    const STONE: MaterialId = MaterialId(2);

    /// Chunks a test put there, and nothing else. Absent means absent, which is
    /// half of what these tests are checking.
    #[derive(Default)]
    struct Loaded(BTreeMap<ChunkPos, Chunk>);

    impl Loaded {
        /// An empty chunk, so the ray has somewhere loaded to travel through.
        fn air(mut self, pos: ChunkPos) -> Self {
            self.0.insert(pos, Chunk::new(pos, MaterialId::AIR));
            self
        }

        /// One block of stone, in a chunk that must already be present.
        fn wall(mut self, block: BlockPos) -> Self {
            let pos = block.chunk();
            let chunk = self
                .0
                .entry(pos)
                .or_insert_with(|| Chunk::new(pos, MaterialId::AIR));
            chunk
                .set_block(block, BlockValue::Uniform(STONE))
                .expect("the block is in the chunk it names");
            self
        }
    }

    impl ChunkLookup for Loaded {
        fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
            self.0.get(&pos)
        }
    }

    #[test]
    fn nothing_between_two_points_is_clear() {
        let world = Loaded::default().air(ChunkPos::new(0, 0, 0));
        assert!(between(&world, [1.0, 1.0, 1.0], [9.0, 1.0, 1.0]));
    }

    #[test]
    fn a_wall_blocks() {
        let world = Loaded::default()
            .air(ChunkPos::new(0, 0, 0))
            .wall(BlockPos::new(5, 1, 1));
        assert!(!between(&world, [1.5, 1.5, 1.5], [9.5, 1.5, 1.5]));
    }

    #[test]
    fn a_wall_past_the_target_does_not_block() {
        // The reach is the distance to the target, not the ray's natural end:
        // a mob must not be hidden by geometry standing behind it.
        let world = Loaded::default()
            .air(ChunkPos::new(0, 0, 0))
            .wall(BlockPos::new(9, 1, 1));
        assert!(between(&world, [1.5, 1.5, 1.5], [7.5, 1.5, 1.5]));
    }

    #[test]
    fn an_unloaded_chunk_blocks() {
        // Charter-wide rule: absence reads as solid. A mob does not get to see
        // through terrain the server has not loaded — and this is the case that
        // makes a follower stop at the edge of what is streamed rather than
        // chase through it.
        let world = Loaded::default().air(ChunkPos::new(0, 0, 0));
        assert!(!between(&world, [1.5, 1.5, 1.5], [20.0, 1.5, 1.5]));
    }

    #[test]
    fn a_point_sees_itself() {
        let world = Loaded::default().air(ChunkPos::new(0, 0, 0));
        assert!(between(&world, [3.25, 4.5, 6.75], [3.25, 4.5, 6.75]));
    }

    #[test]
    fn beyond_the_range_cap_is_blocked() {
        // Nothing in the way at all, and still no: the cap is what stops a mod
        // spending unbounded tick budget by naming a distant point.
        let mut world = Loaded::default();
        for chunk in 0..8 {
            world = world.air(ChunkPos::new(chunk, 0, 0));
        }
        let far = MAX_RANGE_BLOCKS + 1.0;
        assert!(!between(&world, [1.5, 1.5, 1.5], [1.5 + far, 1.5, 1.5]));
        assert!(between(
            &world,
            [1.5, 1.5, 1.5],
            [1.5 + MAX_RANGE_BLOCKS - 1.0, 1.5, 1.5]
        ));
    }

    #[test]
    fn a_point_that_is_not_a_number_is_blocked() {
        // Charter rule 4: `0/0` in Lua is a quiet NaN, and it reaches here the
        // same way it reached the entity patch. Refused rather than fed to the
        // traversal, where it would compare false against everything and walk
        // until the reach ran out.
        let world = Loaded::default().air(ChunkPos::new(0, 0, 0));
        assert!(!between(&world, [1.5, f64::NAN, 1.5], [9.0, 1.5, 1.5]));
        assert!(!between(&world, [1.5, 1.5, 1.5], [f64::INFINITY, 1.5, 1.5]));
    }

    #[test]
    fn sight_is_symmetric() {
        // Not free: the two directions traverse different cells in a different
        // order, and the frame is anchored to whichever end asked. A mob and
        // the player it is watching disagreeing about whether they can see each
        // other is the kind of thing that only shows up as a mob that stares
        // through a wall.
        let world = Loaded::default()
            .air(ChunkPos::new(0, 0, 0))
            .air(ChunkPos::new(1, 0, 0))
            .wall(BlockPos::new(14, 2, 7));
        let a = [3.5, 2.5, 7.5];
        let b = [22.5, 2.5, 7.5];
        assert_eq!(between(&world, a, b), between(&world, b, a));
        assert!(!between(&world, a, b), "the wall is between them");
    }

    #[test]
    fn a_chiselled_block_is_seen_through_where_it_is_open() {
        // Sub-Node Contract §2 applied to sight: solidity is per cell, so the
        // empty half of a partial block is empty to a ray as well as to a body.
        let block = BlockPos::new(5, 1, 1);
        let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
        // The lower cell row only, leaving the two above it open.
        for z in 0..crate::SUBNODES_PER_AXIS as i32 {
            for x in 0..crate::SUBNODES_PER_AXIS as i32 {
                chunk
                    .set_subnode(
                        crate::coords::SubNodePos::new(
                            block.x * crate::SUBNODES_PER_AXIS as i32 + x,
                            block.y * crate::SUBNODES_PER_AXIS as i32,
                            block.z * crate::SUBNODES_PER_AXIS as i32 + z,
                        ),
                        STONE,
                    )
                    .expect("the cell is in the chunk");
            }
        }
        let world = Loaded(BTreeMap::from([(ChunkPos::new(0, 0, 0), chunk)]));

        // Along the bottom of the block: stopped.
        assert!(!between(&world, [1.5, 1.1, 1.5], [9.5, 1.1, 1.5]));
        // Through the open top of the same block: clear.
        assert!(between(&world, [1.5, 1.9, 1.5], [9.5, 1.9, 1.5]));
    }
}
