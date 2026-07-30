// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Worldgen primitive benchmarks.
//!
//! Two paths, and the gap between them is the point.
//!
//! - **The default path** is block resolution: one 2D noise fill giving a
//!   heightmap, then `fill_below_heightmap` over 16³ blocks. Sub-Node Contract
//!   §5 makes this what a generator pays unless it opts into more. Target:
//!   under 200 µs.
//! - **The opt-in path** is `fill_3d` over the 48³ sub-node grid — 110,592
//!   samples against 256. Target: under 2 ms.
//!
//! Both targets are per chunk, single-threaded. Against the 50 ms tick budget
//! in `docs/performance-targets.md`, 200 µs is 0.4% of a tick, so a server can
//! generate a few hundred chunks a second on one core before generation is the
//! simulation.
//!
//! **Task 05's Lua-overhead budget is measured against these numbers.** If
//! calling a generator through the scripting VM costs more than the generation
//! itself, the API shape is wrong — which is why the bulk fills take a whole
//! buffer per call rather than a sample.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use tiamot_core::coords::LocalBlock;
use tiamot_core::detgen::{
    ChunkBuffer, Fractal, FractalParams, Region2d, Region3d, StreamRng, fill_2d, fill_3d,
    fractal_2d,
};
use tiamot_core::{CHUNK_BLOCKS, CHUNK_SUBNODES, ChunkPos, MaterialId, fingerprint};

/// Fixture material. Numbered, not named.
const FIXTURE: MaterialId = MaterialId(2);

fn origin() -> ChunkPos {
    ChunkPos::new(0, 0, 0)
}

fn params() -> FractalParams {
    FractalParams::default()
}

/// THE DEFAULT PATH. Target: under 200 µs per chunk.
fn bench_block_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("worldgen_block_path");

    let region = Region2d {
        origin_x: 0.0,
        origin_y: 0.0,
        step_x: 1.0,
        step_y: 1.0,
        width: CHUNK_BLOCKS as usize,
        height: CHUNK_BLOCKS as usize,
    };

    group.bench_function("fill_2d_heightmap_256_samples", |b| {
        let mut out = vec![0.0f32; region.len()];
        b.iter(|| {
            fill_2d(black_box(42), &region, &params(), &mut out).expect("fill");
            black_box(&out);
        });
    });

    group.bench_function("fill_below_heightmap_16_cubed", |b| {
        let mut heights = [0i32; 256];
        for (index, height) in heights.iter_mut().enumerate() {
            *height = 8 + (index % 5) as i32;
        }
        b.iter_batched_ref(
            || ChunkBuffer::air(origin()),
            |buffer| {
                buffer
                    .fill_below_heightmap(black_box(&heights), FIXTURE)
                    .expect("fill");
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // The whole default path end to end, which is what the 200 µs target is on.
    group.bench_function("whole_chunk_default_path", |b| {
        b.iter(|| {
            let mut samples = vec![0.0f32; region.len()];
            fill_2d(42, &region, &params(), &mut samples).expect("fill");

            let mut heights = [0i32; 256];
            let mut stream = StreamRng::new(42, origin(), "bench");
            for (index, sample) in samples.iter().enumerate() {
                heights[index] = 8 + (sample * 6.0) as i32 + (stream.below(3) as i32) - 1;
            }

            let mut buffer = ChunkBuffer::air(origin());
            buffer
                .fill_below_heightmap(&heights, FIXTURE)
                .expect("fill");
            black_box(buffer.to_chunk())
        });
    });

    group.finish();
}

/// THE OPT-IN PATH. Target: under 2 ms per chunk.
fn bench_subnode_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("worldgen_subnode_path");

    let region = Region3d {
        origin_x: 0.0,
        origin_y: 0.0,
        origin_z: 0.0,
        step: 1.0 / 3.0,
        width: CHUNK_SUBNODES as usize,
        height: CHUNK_SUBNODES as usize,
        depth: CHUNK_SUBNODES as usize,
    };

    // Swept by octave count, because the 2 ms target does not say how many
    // octaves it assumes and the answer changes the verdict entirely: the cost
    // is very nearly linear in octaves, so the gate is met at one and missed at
    // four. Reporting the sweep is more useful than reporting a single number
    // against an ambiguous target.
    for octaves in [1u32, 2, 4] {
        let params = FractalParams {
            octaves,
            ..FractalParams::default()
        };
        group.bench_with_input(
            BenchmarkId::new("fill_3d_48_cubed_octaves", octaves),
            &params,
            |b, params| {
                let mut out = vec![0.0f32; region.len()];
                b.iter(|| {
                    fill_3d(black_box(42), &region, params, &mut out).expect("fill");
                    black_box(&out);
                });
            },
        );
    }

    // What expansion itself costs, separately from the noise — the number that
    // says whether staying on the block path is worth the trouble.
    group.bench_function("buffer_expansion", |b| {
        b.iter_batched_ref(
            || ChunkBuffer::new(origin(), FIXTURE),
            |buffer| {
                buffer.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, MaterialId::AIR);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.bench_function("expanded_to_chunk", |b| {
        let mut expanded = ChunkBuffer::new(origin(), FIXTURE);
        expanded.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, MaterialId::AIR);
        b.iter(|| black_box(expanded.to_chunk()));
    });

    group.finish();
}

/// Per-octave cost, so a mod author can reason about what they are asking for.
fn bench_noise_shapes(c: &mut Criterion) {
    let mut group = c.benchmark_group("noise");

    for octaves in [1u32, 2, 4, 8] {
        let params = FractalParams {
            octaves,
            ..FractalParams::default()
        };
        group.bench_with_input(
            BenchmarkId::new("fractal_2d_octaves", octaves),
            &params,
            |b, params| {
                b.iter(|| black_box(fractal_2d(1, black_box(12.5), black_box(-7.25), params)));
            },
        );
    }

    for fractal in [Fractal::Fbm, Fractal::Ridged, Fractal::Billow] {
        let params = FractalParams {
            fractal,
            ..FractalParams::default()
        };
        group.bench_with_input(
            BenchmarkId::new("mode", format!("{fractal:?}")),
            &params,
            |b, params| {
                b.iter(|| black_box(fractal_2d(1, black_box(12.5), black_box(-7.25), params)));
            },
        );
    }

    group.finish();
}

/// The determinism gate's own cost — it runs on three CI legs on every push.
fn bench_fingerprint(c: &mut Criterion) {
    c.bench_function("fingerprint", |b| {
        b.iter(|| black_box(fingerprint(black_box(42), origin())));
    });
}

/// Raw stream throughput. Worldgen draws a lot of these.
fn bench_rng(c: &mut Criterion) {
    let mut group = c.benchmark_group("rng");

    group.bench_function("stream_creation", |b| {
        b.iter(|| black_box(StreamRng::new(black_box(42), origin(), "bench")));
    });

    group.bench_function("next_u64_x1000", |b| {
        let mut stream = StreamRng::new(42, origin(), "bench");
        b.iter(|| {
            let mut acc = 0u64;
            for _ in 0..1000 {
                acc ^= stream.next_u64();
            }
            black_box(acc)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_block_path,
    bench_subnode_path,
    bench_noise_shapes,
    bench_fingerprint,
    bench_rng
);
criterion_main!(benches);
