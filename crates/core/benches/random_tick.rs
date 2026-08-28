// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What choosing the blocks that get a turn costs.
//!
//! Charter rule 18: 20 Hz means **50 ms shared by all simulation for all fifty
//! players**, so a number here means nothing on its own and everything as a
//! share of that.
//!
//! What is measured is the CHOOSING — the seeded draws that decide which cells
//! get a turn. The block lookups that follow are the server's (`World::
//! material_at`, one per cell) and the Lua call is a mod's, paid only for a
//! material some mod asked about, which is the whole reason the filter sits on
//! the engine's side of the seam.
//!
//! `MAX_RANDOM_TICK_CHUNKS` is 512 on the server, and each chunk costs
//! `PER_CHUNK` draws, so 512 chunks is the number to compare against 50 ms.
//!
//! **Measured 2026-08-28**: 35 ns a chunk, and **7.8 µs for a tick's 512
//! chunks — 0.016% of the budget**. The choosing is not where a random tick
//! costs anything; a handler that does real work on every offer is.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tiamot_core::ChunkPos;
use tiamot_core::tick::random;

/// What the server samples in one tick at its own cap.
const CHUNKS: i32 = 512;

fn choosing(c: &mut Criterion) {
    let mut group = c.benchmark_group("random_tick");

    // One chunk, which is what the per-chunk cost looks like on its own.
    group.bench_function("cells/chunk", |b| {
        let mut tick = 0u64;
        b.iter(|| {
            tick += 1;
            black_box(random::cells(black_box(42), ChunkPos::new(1, 2, 3), tick))
        });
    });

    // And a whole tick's worth, which is the figure that goes against the
    // 50 ms budget.
    group.bench_function("cells/tick", |b| {
        let mut tick = 0u64;
        b.iter(|| {
            tick += 1;
            let mut total = 0usize;
            for index in 0..CHUNKS {
                let chunk = ChunkPos::new(index % 32, index / 32, 0);
                total += random::cells(black_box(42), chunk, tick).len();
            }
            black_box(total)
        });
    });

    group.finish();
}

criterion_group!(benches, choosing);
criterion_main!(benches);
