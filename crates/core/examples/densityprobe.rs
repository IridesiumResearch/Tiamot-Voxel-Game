// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What `Density::bounds` saves a generator, measured on a real-shaped field.
//!
//! Run with `cargo run --release --example densityprobe -p tiamot-core`. Debug
//! numbers are meaningless here — the field evaluation is the whole cost and it
//! is several times slower unoptimised.
//!
//! The field is the shape a world generator actually has: a dome that falls off
//! with height, relief on top of it, and caves cut out with a `min`. What it
//! reports is how much of a streamed column never needs evaluating at all.

use std::time::Instant;

use tiamot_core::detgen::{
    Axis, ChunkBuffer, Density, Detail, Fractal, FractalParams, Op, Region3d,
};
use tiamot_core::{CHUNK_BLOCKS, ChunkPos, MaterialId};

fn noise(stream: u64, frequency: f32, octaves: u32, amplitude: f32) -> Op {
    Op::Noise {
        params: FractalParams {
            fractal: Fractal::Fbm,
            octaves,
            frequency,
            lacunarity: 2.0,
            gain: 0.5,
        },
        amplitude,
        stream,
    }
}

/// Relief over a falling dome, with caves cut out of it.
///
/// **The height coefficient is what decides whether a bound can say anything.**
/// The surface sits where the noise balances `y * k`, so the band the field
/// could possibly cross is the noise's own bound divided by `k` — here
/// `40 * 3.78 / 0.08`, which is nearly two kilometres of "maybe". That is a
/// property of the terrain (this world really does have 500 m of relief), not
/// of the arithmetic.
fn world() -> Density {
    let mut ops = vec![
        // The surface: noise - y * 0.08, so the ground is where they balance.
        noise(1, 0.004, 5, 40.0),
        noise(2, 0.02, 4, 8.0),
        Op::Add,
        Op::Coordinate(Axis::Y),
        Op::Constant(0.08),
        Op::Multiply,
        Op::Subtract,
    ];
    // Caves: solid except where a second field is near zero.
    ops.extend([
        Op::Constant(0.35),
        noise(3, 0.05, 3, 1.0),
        Op::Absolute,
        Op::Subtract,
        Op::Minimum,
    ]);
    Density::compile(ops).expect("compile")
}

/// Relief in the shape the reference fixture uses: a heightmap-like field
/// where one unit of noise is one block, so the band is the relief itself.
fn rolling() -> Density {
    Density::compile(vec![
        noise(1, 0.008, 5, 11.0),
        Op::Coordinate(Axis::Y),
        Op::Subtract,
    ])
    .expect("compile")
}

fn main() {
    for (name, density) in [
        ("dome, relief and caves", world()),
        ("rolling hills", rolling()),
    ] {
        println!("\n{name}:");
        probe(&density);
    }
}

fn probe(density: &Density) {
    let side = CHUNK_BLOCKS as usize;
    let seed = 4242;

    // A column of the world from deep rock to high sky, which is what a player
    // at a large vertical view distance actually asks for.
    let mut decided = 0;
    let mut undecided = 0;
    let mut skipped_time = 0.0f64;
    let mut evaluated_time = 0.0f64;
    let mut unpruned_time = 0.0f64;

    for cy in -24..24 {
        for cx in 0..8 {
            let pos = ChunkPos::new(cx, cy, cx * 3);
            let region = Region3d {
                origin_x: (pos.x * CHUNK_BLOCKS as i32) as f32,
                origin_y: (pos.y * CHUNK_BLOCKS as i32) as f32,
                origin_z: (pos.z * CHUNK_BLOCKS as i32) as f32,
                step: 1.0,
                width: side,
                height: side,
                depth: side,
            };

            // What the fill costs now.
            let started = Instant::now();
            let mut buffer = ChunkBuffer::new(pos, MaterialId::AIR);
            buffer
                .fill_density_detail(density, seed, MaterialId(2), Detail::Sampled)
                .expect("fill");
            let took = started.elapsed().as_secs_f64() * 1000.0;

            // What it would have cost with no bound: the evaluation the fill
            // would have done regardless of what the bound says.
            let before = Instant::now();
            let mut field = vec![0.0f32; region.len()];
            density.evaluate(seed, &region, &mut field).expect("field");
            unpruned_time += before.elapsed().as_secs_f64() * 1000.0;

            if density.bounds(&region).is_undecided() {
                undecided += 1;
                evaluated_time += took;
            } else {
                decided += 1;
                skipped_time += took;
            }
        }
    }

    let total = decided + undecided;
    println!(
        "{total} chunks of a streamed column, field of {} ops",
        density.len()
    );
    println!(
        "  decided by bounds alone : {decided:>4}  ({:.0}%)  {:.3} ms each",
        f64::from(decided) / f64::from(total) * 100.0,
        skipped_time / f64::from(decided.max(1))
    );
    println!(
        "  had to be evaluated     : {undecided:>4}  ({:.0}%)  {:.3} ms each",
        f64::from(undecided) / f64::from(total) * 100.0,
        evaluated_time / f64::from(undecided.max(1))
    );
    println!(
        "  block-resolution evaluation alone, every chunk: {:.3} ms each",
        unpruned_time / f64::from(total)
    );
    println!(
        "  total now {:.1} ms against {:.1} ms of evaluation that no longer happens",
        skipped_time + evaluated_time,
        unpruned_time
    );
}
