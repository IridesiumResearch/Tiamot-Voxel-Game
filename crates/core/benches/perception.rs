// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Benchmarks for what a mod is allowed to ask about terrain.
//!
//! # Read these as a share of a tick, never on their own
//!
//! Charter rule 18: the budget is **50 ms for all simulation for all players**.
//! Both of these are things a mob does, so the figure that matters is the one
//! multiplied by how many mobs are doing it — and the task's own gate is two
//! hundred entities in one area.
//!
//! - **A sight test** is what a mob does to decide whether it can see you, and
//!   the natural rate is once per mob per tick. Two hundred of these is the
//!   number to compare against 50 ms.
//! - **A search** is far more expensive and must NOT be done every tick by
//!   every mob. The interesting figure is the exhausted-budget case, because
//!   that is the worst a single call can cost and it is what a mod pays when it
//!   asks for somewhere it cannot get to.
//!
//! # The scenes
//!
//! - `open` is flat ground with nothing on it: the cheap case, and what a mob
//!   following a player across a field actually pays.
//! - `sealed` is a wall with no way through: the search expands everything it
//!   can reach and then reports failure, which is the honest worst case for a
//!   goal that exists but cannot be got to.
//! - `budget` is the same wall with a full allowance, so the search runs until
//!   the budget stops it — the ceiling on what one `game.find_path` can cost.

use criterion::{Criterion, criterion_group, criterion_main};
use std::collections::BTreeMap;
use std::hint::black_box;

use tiamot_core::block::BlockValue;
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::{BlockPos, ChunkPos};
use tiamot_core::path::{self, Options};
use tiamot_core::phys::ChunkLookup;
use tiamot_core::{MaterialId, sight};

const STONE: MaterialId = MaterialId(2);

/// A slab of world, eight chunks square and two tall.
///
/// Big enough that a search can spend a real budget inside it: a room that ran
/// out of loaded chunks would measure how fast the search hits a wall rather
/// than how fast it explores.
struct Field(BTreeMap<ChunkPos, Chunk>);

impl Field {
    /// Flat ground filling the chunk layer at y = -1, air above it.
    fn flat() -> Self {
        let mut chunks = BTreeMap::new();
        for z in 0..8 {
            for x in 0..8 {
                let below = ChunkPos::new(x, -1, z);
                let mut solid = Chunk::new(below, MaterialId::AIR);
                let corner = BlockPos::from_chunk_corner(below);
                for by in 0..tiamot_core::CHUNK_BLOCKS as i32 {
                    for bz in 0..tiamot_core::CHUNK_BLOCKS as i32 {
                        for bx in 0..tiamot_core::CHUNK_BLOCKS as i32 {
                            solid
                                .set_block(
                                    BlockPos::new(corner.x + bx, corner.y + by, corner.z + bz),
                                    BlockValue::Uniform(STONE),
                                )
                                .expect("in chunk");
                        }
                    }
                }
                chunks.insert(below, solid);
                let above = ChunkPos::new(x, 0, z);
                chunks.insert(above, Chunk::new(above, MaterialId::AIR));
            }
        }
        Self(chunks)
    }

    /// A wall across the whole slab, two blocks tall, with no way through.
    fn walled(mut self) -> Self {
        for z in 0..(8 * tiamot_core::CHUNK_BLOCKS as i32) {
            for y in 0..2 {
                let block = BlockPos::new(40, y, z);
                let chunk = block.chunk();
                if let Some(held) = self.0.get_mut(&chunk) {
                    held.set_block(block, BlockValue::Uniform(STONE))
                        .expect("in chunk");
                }
            }
        }
        self
    }
}

impl ChunkLookup for Field {
    fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.0.get(&pos)
    }
}

fn perception(c: &mut Criterion) {
    let open = Field::flat();
    let sealed = Field::flat().walled();

    // The mimic's own check: thirty-two yards, which is the radius the task
    // gives it. Once per mob per tick.
    c.bench_function("sight/32 blocks, clear", |b| {
        b.iter(|| {
            black_box(sight::between(
                &open,
                black_box([2.5, 0.5, 2.5]),
                black_box([34.5, 0.5, 2.5]),
            ))
        });
    });

    // The cheap answer, and the common one: something in the way early.
    c.bench_function("sight/32 blocks, blocked at once", |b| {
        b.iter(|| {
            black_box(sight::between(
                &open,
                black_box([2.5, -0.5, 2.5]),
                black_box([34.5, -0.5, 2.5]),
            ))
        });
    });

    // A mob following a player across open ground.
    c.bench_function("path/16 blocks, open ground", |b| {
        b.iter(|| {
            black_box(path::search(
                &open,
                black_box(BlockPos::new(2, 0, 2)),
                black_box(BlockPos::new(18, 0, 2)),
                &Options::default(),
            ))
        });
    });

    c.bench_function("path/64 blocks, open ground", |b| {
        b.iter(|| {
            black_box(path::search(
                &open,
                black_box(BlockPos::new(2, 0, 2)),
                black_box(BlockPos::new(66, 0, 2)),
                &Options::default(),
            ))
        });
    });

    // The ceiling: a goal behind a wall with no way through, searched until the
    // default budget stops it. This is the most one `game.find_path` can cost.
    c.bench_function("path/default budget, exhausted", |b| {
        b.iter(|| {
            black_box(path::search(
                &sealed,
                black_box(BlockPos::new(2, 0, 2)),
                black_box(BlockPos::new(80, 0, 2)),
                &Options::default(),
            ))
        });
    });

    // And the absolute ceiling, for a mod that asks for everything.
    c.bench_function("path/maximum budget, exhausted", |b| {
        b.iter(|| {
            black_box(path::search(
                &sealed,
                black_box(BlockPos::new(2, 0, 2)),
                black_box(BlockPos::new(80, 0, 2)),
                &Options {
                    budget: path::MAX_BUDGET,
                    ..Options::default()
                },
            ))
        });
    });
}

criterion_group!(benches, perception);
criterion_main!(benches);
