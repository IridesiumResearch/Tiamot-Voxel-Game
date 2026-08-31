// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What one chunk costs to mesh.
//!
//! # Why this exists
//!
//! The mesher is the client's largest per-frame cost and, until this, the only
//! major one with no standing measurement. Task 02b's spike measured 0.108 ms
//! for a realistic chunk and that figure is quoted where the remesh budget is
//! set, but it was a spike: nothing re-measured it, so a change that made
//! meshing an order of magnitude slower would have shown up only as somebody's
//! frame rate.
//!
//! It was reported from the window as exactly that — a worst frame of 34 ms
//! with 26 of them inside a single chunk's mesh, in release. There was no
//! number to compare it against.
//!
//! # What the cases are
//!
//! Meshing cost tracks the CELLS SCANNED, not the size of the result: every
//! chunk is 110,592 sub-node cells whatever is in it. So the cases here differ
//! in how much *geometry* they produce rather than in how much they scan, which
//! is what separates the scan cost from the merge and shading cost.
//!
//! - `uniform` — solid stone. The scan, with almost nothing to emit.
//! - `terrain` — the shape a real chunk has: ground, air above, a hole.
//! - `chiselled` — every block partial. The sub-node worst case, and the one
//!   charter rule 19 committed to keeping fast.
//!
//! Read against the frame, not in isolation: at 60 fps a frame is 16.7 ms, and
//! the client's remesh budget gives meshing 2 ms of it.

use criterion::{Criterion, criterion_group, criterion_main};

use client::mesher::{self, Absent, Neighbours, NoFluid};
use client::shade::Uniform;
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId};

/// Full daylight everywhere, so the bench measures geometry rather than light.
const DAY: Uniform = Uniform(tiamot_core::light::Light::DAYLIGHT);

const STONE: MaterialId = MaterialId(2);

/// Solid stone, corner to corner.
fn uniform() -> Chunk {
    Chunk::new(ChunkPos::new(0, 0, 0), STONE)
}

/// The shape a chunk of real terrain has: ground below, air above, one hole.
fn terrain() -> Chunk {
    let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
    for x in 0..16 {
        for z in 0..16 {
            for y in 0..8 {
                chunk
                    .set_block(BlockPos::new(x, y, z), BlockValue::Uniform(STONE))
                    .expect("in chunk");
            }
        }
    }
    chunk
        .set_block(BlockPos::new(8, 7, 8), BlockValue::Uniform(MaterialId::AIR))
        .expect("in chunk");
    chunk
}

/// Every block partial: the sub-node worst case.
///
/// One cell missing from each block is the most expensive thing a chunk can be
/// — nothing merges, and every block has to be read cell by cell rather than
/// taken whole by the `Uniform` fast path.
fn chiselled() -> Chunk {
    let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), STONE);
    for x in 0..16 {
        for z in 0..16 {
            for y in 0..16 {
                chunk
                    .set_subnode(BlockPos::new(x, y, z).subnode(1, 1, 1), MaterialId::AIR)
                    .expect("in chunk");
            }
        }
    }
    chunk
}

fn meshing(c: &mut Criterion) {
    let mut group = c.benchmark_group("mesh_chunk");
    for (name, chunk) in [
        ("uniform", uniform()),
        ("terrain", terrain()),
        ("chiselled", chiselled()),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let neighbours = Neighbours::none();
                std::hint::black_box(mesher::mesh_chunk(
                    std::hint::black_box(&chunk),
                    &neighbours,
                    Absent::Air,
                    &DAY,
                    &NoFluid,
                ))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, meshing);
criterion_main!(benches);
