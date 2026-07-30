// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Persistence benchmarks.
//!
//! Two costs matter and they are paid by different things:
//!
//! - **Encode/decode per chunk** is paid on the tick thread when a chunk is
//!   saved or streamed to a player. Against the 50 ms tick budget in
//!   `docs/performance-targets.md`, a chunk save that costs 1 ms means a server
//!   can afford roughly fifty of them per tick before persistence is the
//!   simulation.
//! - **Batch save** is paid on shutdown and on periodic autosave, where the
//!   question is whether saving a large loaded region is a stall a player
//!   notices.
//!
//! As in the voxel benches, CI runs these in smoke mode only. Shared runners
//! have too much variance for a regression gate, and a flaky perf gate teaches
//! people to ignore red builds.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use tiamot_core::block::{EMPTY_CELLS, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::material::MaterialRegistry;
use tiamot_core::persist::codec::{decode_chunk, encode_chunk};
use tiamot_core::persist::idmap::MaterialMap;
use tiamot_core::{BLOCKS_PER_CHUNK, BlockValue, ChunkPos, MaterialId, Registry, WorldDb};

fn origin() -> ChunkPos {
    ChunkPos::new(0, 0, 0)
}

/// A world plus the materials a scene needs.
fn session() -> (WorldDb, Registry) {
    let mut registry = Registry::new();
    for name in ["core:stone", "core:dirt", "core:grass", "core:wood"] {
        registry.register(name).expect("register");
    }
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    (db, registry)
}

/// The Task 02b scenes, rebuilt here so the persistence numbers line up with
/// the meshing and storage numbers in `docs/subnode-verdict.md`.
fn scene(name: &str, registry: &Registry) -> Chunk {
    let stone = registry.id_of("core:stone").expect("registered");
    let dirt = registry.id_of("core:dirt").expect("registered");
    let grass = registry.id_of("core:grass").expect("registered");

    let mut chunk = Chunk::air(origin());
    match name {
        "uniform" => return Chunk::new(origin(), stone),
        "flat" => {
            for z in 0..16 {
                for y in 0..8 {
                    for x in 0..16 {
                        let material = if y == 7 { grass } else { stone };
                        chunk.set_block_local(
                            LocalBlock::new(x, y, z),
                            BlockValue::Uniform(material),
                        );
                    }
                }
            }
        }
        "chiselled" => {
            for z in 0..16 {
                for y in 0..8 {
                    for x in 0..16 {
                        let material = if y == 7 { grass } else { stone };
                        chunk.set_block_local(
                            LocalBlock::new(x, y, z),
                            BlockValue::Uniform(material),
                        );
                    }
                }
            }
            // Every surface block chiselled to a distinct mask.
            for z in 0..16 {
                for x in 0..16 {
                    chunk.set_block_local(
                        LocalBlock::new(x, 7, z),
                        BlockValue::Partial {
                            material: grass,
                            occupancy: (x * 31 + z * 17 + 1) & tiamot_core::block::OCCUPANCY_FULL,
                        },
                    );
                }
            }
        }
        _ => {
            // "mixed": every block a distinct multi-material array, the
            // pathological storage case.
            for index in 0..BLOCKS_PER_CHUNK {
                let mut cells = EMPTY_CELLS;
                cells[index % SUBNODES_PER_BLOCK] = stone;
                cells[(index / 7) % SUBNODES_PER_BLOCK] = dirt;
                cells[(index / 53) % SUBNODES_PER_BLOCK] = MaterialId(100 + (index % 64) as u16);
                chunk.set_block_local(LocalBlock::from_index(index), BlockValue::Cells(cells));
            }
        }
    }
    chunk
}

fn bench_codec(c: &mut Criterion) {
    let (db, registry) = session();
    // The mixed scene invents material ids beyond the registry, so it cannot be
    // translated; it is excluded from codec benches and covered by the meshing
    // ones in `voxel.rs` instead.
    let scenes = ["uniform", "flat", "chiselled"];

    let mut group = c.benchmark_group("chunk_encode");
    for name in scenes {
        let chunk = scene(name, &registry);
        group.bench_with_input(BenchmarkId::from_parameter(name), &chunk, |b, chunk| {
            b.iter(|| black_box(encode_chunk(chunk, db.materials(), None).expect("encode")));
        });
    }
    group.finish();

    let mut group = c.benchmark_group("chunk_decode");
    for name in scenes {
        let chunk = scene(name, &registry);
        let blob = encode_chunk(&chunk, db.materials(), None).expect("encode");
        group.bench_with_input(BenchmarkId::from_parameter(name), &blob, |b, blob| {
            b.iter(|| {
                black_box(decode_chunk(origin(), blob, db.materials(), &[]).expect("decode"))
            });
        });
    }
    group.finish();
}

/// Stored sizes, reported once rather than timed.
///
/// Not a benchmark, but this is where the numbers live and criterion is what
/// runs here. `docs/subnode-verdict.md` quotes the Task 02b equivalents; these
/// are the real format's.
fn report_sizes(c: &mut Criterion) {
    let (db, registry) = session();
    let mut group = c.benchmark_group("stored_size");
    for name in ["uniform", "flat", "chiselled"] {
        let chunk = scene(name, &registry);
        let blob = encode_chunk(&chunk, db.materials(), None).expect("encode");
        println!("stored size: {name:>10} = {} bytes", blob.len());
        group.bench_with_input(BenchmarkId::from_parameter(name), &chunk, |b, chunk| {
            b.iter(|| {
                black_box(
                    encode_chunk(chunk, db.materials(), None)
                        .expect("encode")
                        .len(),
                )
            });
        });
    }
    group.finish();
}

fn bench_batch_save(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_save");
    group.sample_size(10);

    for count in [100usize, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let (db, registry) = session();
                    let template = scene("flat", &registry);
                    let chunks: Vec<(ChunkPos, Chunk)> = (0..count)
                        .map(|i| {
                            let pos = ChunkPos::new(i as i32, 0, 0);
                            (pos, template.clone())
                        })
                        .collect();
                    (db, chunks)
                },
                |(mut db, chunks)| {
                    db.save_chunks_batch(chunks.iter().map(|(pos, chunk)| (*pos, chunk)))
                        .expect("batch save")
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// Reconciliation runs once per world open, and its cost scales with how many
/// materials the world has ever seen — including those from removed mods.
fn bench_open(c: &mut Criterion) {
    c.bench_function("world_open_and_reconcile", |b| {
        b.iter_batched(
            || {
                let mut registry = Registry::new();
                for index in 0..512 {
                    registry
                        .register(&format!("mod:material{index}"))
                        .expect("register");
                }
                registry
            },
            |mut registry| black_box(WorldDb::open_in_memory(&mut registry).expect("open")),
            criterion::BatchSize::SmallInput,
        );
    });
}

/// Silences an unused-import warning in builds where `MaterialMap` is only
/// named in a signature above.
const _: Option<&MaterialMap> = None;

criterion_group!(
    benches,
    bench_codec,
    report_sizes,
    bench_batch_save,
    bench_open
);
criterion_main!(benches);
