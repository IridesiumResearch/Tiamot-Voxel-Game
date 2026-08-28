// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Choosing which blocks get a turn each tick.
//!
//! **The mechanism behind everything that happens on its own.** A crop growing,
//! grass spreading onto bare earth, a sapling becoming a tree, leaves decaying,
//! fire going out, ice melting: none of them is a thing a player did, and none
//! of them can be driven by a mod's own list, because the blocks that need it
//! were mostly made by worldgen and never passed through a hook.
//!
//! The engine's part is small and is only this: pick cells, deterministically,
//! at a rate somebody can reason about. What a chosen block DOES is a mod's
//! (charter rule 1) — the engine has no idea what a crop is.
//!
//! # Determinism
//!
//! Charter rule 4. The cells come from the same seeded stream everything else
//! does — `world_seed`, the chunk, a stream name — mixed with the tick number
//! so the same chunk does not get the same cells every tick. Two servers
//! running the same world at the same tick choose the same blocks.
//!
//! Which CHUNKS are resident is not deterministic across two servers with
//! different players in them, and cannot be: a world evolves around the people
//! in it. What this guarantees is the narrower and useful thing — given the
//! same chunk and the same tick, the same cells.

use crate::coords::{BlockPos, ChunkPos};
use crate::detgen::StreamRng;

/// How many blocks of each chunk get a turn per tick.
///
/// **Three, which is Minecraft's number for a 16³ section**, and the shape of
/// the thing is the same: a chunk here is 16³ blocks. It is low on purpose —
/// what makes a crop grow in a minute rather than a second is how rarely its
/// block comes up, and a mod cannot slow the engine down but can always decide
/// to do nothing.
pub const PER_CHUNK: usize = 3;

/// Blocks in a chunk, which is what a cell index is drawn below.
const BLOCKS_PER_CHUNK: u64 = SIDE.pow(3);

/// Blocks along one axis of a chunk, as the arithmetic below wants it.
const SIDE: u64 = crate::CHUNK_BLOCKS as u64;

/// The stream every random tick is drawn from.
const STREAM: &str = "engine:random_tick";

/// Which blocks of one chunk get a turn on one tick.
///
/// The same chunk and tick always give the same list, on every machine.
#[must_use]
pub fn cells(world_seed: u64, chunk: ChunkPos, tick: u64) -> Vec<BlockPos> {
    // **The tick goes into the SEED, not into the draw.** A stream opened once
    // per chunk would hand out the same first three cells every tick; mixing
    // the tick in gives each one its own sequence, and `StreamRng::seed_for`
    // already puts everything it is given through SplitMix64 so neighbouring
    // ticks are not neighbouring sequences.
    let mut rng = StreamRng::new(world_seed.wrapping_add(tick), chunk, STREAM);
    let corner = BlockPos::from_chunk_corner(chunk);
    (0..PER_CHUNK)
        .map(|_| {
            let index = rng.below(BLOCKS_PER_CHUNK);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "an index below 16^3, which fits an i32 many times over"
            )]
            let (x, y, z) = (
                (index % SIDE) as i32,
                ((index / SIDE) % SIDE) as i32,
                (index / (SIDE * SIDE)) as i32,
            );
            BlockPos::new(corner.x + x, corner.y + y, corner.z + z)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_chunk_and_tick_always_choose_the_same_blocks() {
        // Charter rule 4: two servers running the same world at the same tick
        // must grow the same crop. A random tick writes to the world, so an
        // answer that differed would be two worlds.
        let chunk = ChunkPos::new(3, -1, 7);
        assert_eq!(cells(99, chunk, 40), cells(99, chunk, 40));
    }

    #[test]
    fn a_different_tick_chooses_different_blocks() {
        // The failure this rules out is silent and total: a stream opened once
        // per chunk hands out the same three cells for ever, so exactly three
        // blocks of each chunk would ever grow.
        let chunk = ChunkPos::new(0, 0, 0);
        let first = cells(99, chunk, 1);
        let same = (2..12).all(|tick| cells(99, chunk, tick) == first);
        assert!(!same, "every tick chose the same three blocks: {first:?}");
    }

    #[test]
    fn a_different_seed_or_chunk_chooses_differently() {
        let chunk = ChunkPos::new(0, 0, 0);
        assert_ne!(
            cells(1, chunk, 5),
            cells(2, chunk, 5),
            "the seed did nothing"
        );
        assert_ne!(
            cells(1, chunk, 5),
            cells(1, ChunkPos::new(1, 0, 0), 5)
                .iter()
                .map(|pos| BlockPos::new(pos.x - crate::CHUNK_BLOCKS as i32, pos.y, pos.z))
                .collect::<Vec<_>>(),
            "neighbouring chunks got the same sequence"
        );
    }

    #[test]
    fn every_chosen_block_is_inside_the_chunk_it_was_chosen_for() {
        // An index unpacked wrongly puts a random tick in the chunk next door,
        // which is a mod's crop growing somewhere nobody planted one.
        for tick in 0..64 {
            for chunk in [
                ChunkPos::new(0, 0, 0),
                ChunkPos::new(-3, 2, -9),
                ChunkPos::new(1000, -4, 55),
            ] {
                for pos in cells(7, chunk, tick) {
                    assert_eq!(
                        pos.chunk(),
                        chunk,
                        "a cell chosen for {chunk:?} landed in another chunk at tick {tick}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_whole_chunk_is_reachable_given_enough_ticks() {
        // A generator that only ever picked from one corner would pass every
        // test above and leave most of a chunk dead.
        let chunk = ChunkPos::new(0, 0, 0);
        let mut seen = std::collections::BTreeSet::new();
        for tick in 0..4000 {
            for pos in cells(11, chunk, tick) {
                seen.insert((pos.x, pos.y, pos.z));
            }
        }
        let total = usize::pow(crate::CHUNK_BLOCKS as usize, 3);
        assert!(
            seen.len() > total / 2,
            "only {} of {total} blocks were ever chosen",
            seen.len()
        );
    }
}
