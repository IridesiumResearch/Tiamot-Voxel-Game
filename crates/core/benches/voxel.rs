// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Benchmarks for the voxel data model.
//!
//! No regression gate yet — CI runs these in smoke mode (`--test`), which
//! executes each benchmark once to prove it still compiles and runs but records
//! no timings. A gate needs a stable baseline to compare against and shared CI
//! runners do not provide one; Task 02b takes real measurements on real
//! hardware, and those numbers feed the keep / keep-with-limits / fallback
//! decision.
//!
//! What matters here is the *shape* of the cost, and there are three shapes
//! worth watching:
//!
//! - `set_subnode` is the chisel inner loop. Every write reads 27 cells,
//!   modifies one, re-canonicalises, and re-interns — so a chisel stroke is
//!   materially more expensive than a block placement, and this is where that
//!   shows up.
//! - Filling a chunk exercises palette growth and the index-array widening that
//!   comes with it.
//! - `repack` is the compaction path, which runs automatically whenever an
//!   index width narrows.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use tiamot_core::block::{EMPTY_CELLS, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BLOCKS_PER_CHUNK, BlockPos, BlockValue, ChunkPos, MaterialId};

const STONE: MaterialId = MaterialId(2);
const DIRT: MaterialId = MaterialId(3);

fn origin() -> ChunkPos {
    ChunkPos::new(0, 0, 0)
}

/// The chisel inner loop: single sub-node writes scattered across a chunk.
fn bench_set_subnode(c: &mut Criterion) {
    let mut group = c.benchmark_group("set_subnode");

    // Into air: each write creates a Partial, and repeated writes to one block
    // widen its mask rather than allocating anything new.
    group.bench_function("into_air", |b| {
        b.iter_batched_ref(
            || Chunk::air(origin()),
            |chunk| {
                for index in 0..512 {
                    let block = BlockPos::new(index % 16, (index / 16) % 16, index / 256);
                    let pos = block.subnode(1, 1, 1);
                    chunk.set_subnode(pos, black_box(STONE)).expect("in chunk");
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Carving air out of solid stone — the actual chiselling motion, and the
    // one that turns Uniform blocks into Partial ones.
    group.bench_function("carving_solid", |b| {
        b.iter_batched_ref(
            || Chunk::new(origin(), STONE),
            |chunk| {
                for index in 0..512 {
                    let block = BlockPos::new(index % 16, (index / 16) % 16, index / 256);
                    let pos = block.subnode(1, 1, 1);
                    chunk
                        .set_subnode(pos, black_box(MaterialId::AIR))
                        .expect("in chunk");
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // The expensive shape: a second material in the block forces a Mixed slot,
    // so every write interns a 54-byte array.
    group.bench_function("forcing_mixed", |b| {
        b.iter_batched_ref(
            || {
                let mut chunk = Chunk::new(origin(), STONE);
                // Pre-mix every block so no write can take a cheap path.
                for index in 0..BLOCKS_PER_CHUNK {
                    let mut cells = [STONE; SUBNODES_PER_BLOCK];
                    cells[0] = DIRT;
                    chunk.set_block_local(LocalBlock::from_index(index), BlockValue::Cells(cells));
                }
                chunk
            },
            |chunk| {
                for index in 0..512 {
                    let block = BlockPos::new(index % 16, (index / 16) % 16, index / 256);
                    let pos = block.subnode(2, 2, 2);
                    chunk.set_subnode(pos, black_box(DIRT)).expect("in chunk");
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Whole-chunk fills at several palette sizes, so index-width growth is visible.
fn bench_fill(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_fill");

    for distinct in [1usize, 2, 16, 256] {
        group.bench_with_input(
            BenchmarkId::new("set_block", distinct),
            &distinct,
            |b, &distinct| {
                b.iter_batched_ref(
                    || Chunk::air(origin()),
                    |chunk| {
                        for index in 0..BLOCKS_PER_CHUNK {
                            let material = MaterialId((index % distinct) as u16 + 2);
                            chunk.set_block_local(
                                LocalBlock::from_index(index),
                                BlockValue::Uniform(black_box(material)),
                            );
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    // The region path, which should beat per-block writes by taking the
    // whole-block fast path.
    group.bench_function("fill_region_whole_chunk", |b| {
        b.iter_batched_ref(
            || Chunk::air(origin()),
            |chunk| {
                chunk.fill_region(
                    tiamot_core::SubNodePos::new(0, 0, 0),
                    tiamot_core::SubNodePos::new(47, 47, 47),
                    black_box(STONE),
                );
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Sub-node-aligned region that does NOT cover whole blocks, so every block
    // takes the slow cell-by-cell path.
    group.bench_function("fill_region_unaligned", |b| {
        b.iter_batched_ref(
            || Chunk::air(origin()),
            |chunk| {
                chunk.fill_region(
                    tiamot_core::SubNodePos::new(1, 1, 1),
                    tiamot_core::SubNodePos::new(46, 46, 46),
                    black_box(STONE),
                );
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Palette compaction, at the palette sizes where it actually costs something.
fn bench_repack(c: &mut Criterion) {
    let mut group = c.benchmark_group("repack");

    for distinct in [16usize, 256, 4096] {
        group.bench_with_input(
            BenchmarkId::new("uniform_entries", distinct),
            &distinct,
            |b, &distinct| {
                b.iter_batched_ref(
                    || {
                        let mut chunk = Chunk::air(origin());
                        for index in 0..BLOCKS_PER_CHUNK {
                            chunk.set_block_local(
                                LocalBlock::from_index(index),
                                BlockValue::Uniform(MaterialId((index % distinct) as u16 + 2)),
                            );
                        }
                        chunk
                    },
                    Chunk::repack,
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    // Repacking a chunk full of mixed blocks also rebuilds the interning table,
    // which is the more expensive half.
    group.bench_function("mixed_entries", |b| {
        b.iter_batched_ref(
            || {
                let mut chunk = Chunk::air(origin());
                for index in 0..BLOCKS_PER_CHUNK {
                    let mut cells = EMPTY_CELLS;
                    cells[0] = MaterialId(100 + (index % 64) as u16);
                    cells[1] = MaterialId(200 + (index / 64) as u16);
                    chunk.set_block_local(LocalBlock::from_index(index), BlockValue::Cells(cells));
                }
                chunk
            },
            Chunk::repack,
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Full-chunk reads — what meshing and lighting do, and far more often than
/// anything writes.
fn bench_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_read");

    let uniform = Chunk::new(origin(), STONE);
    group.bench_function("iterate_uniform", |b| {
        b.iter(|| {
            let mut units = 0u32;
            for (_, view) in uniform.blocks() {
                units += black_box(view.occupied_units());
            }
            units
        });
    });

    let mut mixed = Chunk::air(origin());
    for index in 0..BLOCKS_PER_CHUNK {
        let mut cells = EMPTY_CELLS;
        cells[0] = MaterialId(100 + (index % 64) as u16);
        cells[1] = MaterialId(200 + (index / 64) as u16);
        mixed.set_block_local(LocalBlock::from_index(index), BlockValue::Cells(cells));
    }
    group.bench_function("iterate_all_mixed", |b| {
        b.iter(|| {
            let mut units = 0u32;
            for (_, view) in mixed.blocks() {
                units += black_box(view.occupied_units());
            }
            units
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_set_subnode,
    bench_fill,
    bench_repack,
    bench_read
);
criterion_main!(benches);
