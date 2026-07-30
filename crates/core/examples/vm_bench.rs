// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The three-way scripting VM benchmark.
//!
//! Run once per backend and compare:
//!
//! ```sh
//! cargo run --release -p tiamot-core --example vm_bench --no-default-features --features vm-lua54
//! cargo run --release -p tiamot-core --example vm_bench --no-default-features --features vm-luajit
//! cargo run --release -p tiamot-core --example vm_bench --no-default-features --features vm-luau
//! ```
//!
//! A plain harness rather than criterion, deliberately: the numbers have to be
//! collated across three separate binaries — the backends are mutually
//! exclusive at the C level and cannot coexist in one process — and a printed
//! table is easier to compare and to paste into the verdict than three criterion
//! runs.
//!
//! # The workloads, and why these four
//!
//! 1. **Worldgen callback** — the realistic case, and the one that decides. A
//!    generator orchestrating native fills. Measured against Task 04's recorded
//!    53.5 µs for the same work called directly from Rust, which is the
//!    denominator that matters: if the VM adds more than the generation costs,
//!    the API shape is wrong.
//! 2. **`on_step` at 1000 calls/tick** — the other realistic case. Many small
//!    calls rather than one large one, which is where call-boundary overhead
//!    dominates and where a JIT has the least to work with.
//! 3. **Sandbox setup per mod** — paid once per mod at startup. A server with a
//!    hundred mods cares.
//! 4. **Trivial call boundary** — the pure crossing cost, isolated.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use tiamot_core::ChunkPos;
use tiamot_core::script::{EngineVm, ScriptVm, VmLimits};

/// Best of N. Same reasoning as the Task 04 harness: the question is what the
/// design can do, and the minimum is the closest estimate with scheduler noise
/// removed.
fn best_of(iterations: u32, mut body: impl FnMut()) -> Duration {
    body();
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed());
    }
    best
}

/// The worldgen mod the benchmark measures. Representative of what a real
/// generator does: ask for a heightmap, hand it to a fill.
const WORLDGEN_MOD: &str = r"
local white = game.register_block{ id = 'white', name = 'White' }

game.register_on_generate(function(buf, pos)
    local heights = game.noise_heightmap(pos, {
        octaves = 4,
        frequency = 0.03,
        amplitude = 6.0,
        base = 8,
    })
    buf:fill_below_heightmap(heights, white)
end)
";

/// A per-entity step callback. Deliberately does a little arithmetic and a
/// table access — a callback that did nothing would measure only the boundary,
/// which workload 4 already isolates.
const ONSTEP_MOD: &str = r"
local state = { x = 0, y = 0, vx = 1, vy = 2 }

function on_step()
    state.x = state.x + state.vx
    state.y = state.y + state.vy
    if state.x > 1000 then state.x = 0 end
    if state.y > 1000 then state.y = 0 end
end

function trivial() end
";

fn main() {
    let backend = <EngineVm as ScriptVm>::backend();
    println!(
        "backend: {} (dialect {})",
        backend.name(),
        backend.dialect()
    );
    println!("integers: {}", backend.has_integers());
    println!();

    let limits = VmLimits::default();
    let dir = Path::new(".");

    // -- 1. worldgen callback ------------------------------------------------
    let mut vm = EngineVm::new(limits).expect("create vm");
    vm.load_mod("bench_worldgen", WORLDGEN_MOD, dir)
        .expect("load worldgen mod");
    vm.freeze().expect("freeze");

    let worldgen = best_of(200, || {
        let chunk = vm
            .generate_chunk(42, ChunkPos::new(0, 0, 0), tiamot_core::MaterialId::AIR)
            .expect("generate");
        black_box(chunk);
    });

    // -- 2. on_step, 1000 calls ----------------------------------------------
    let mut vm = EngineVm::new(limits).expect("create vm");
    vm.load_mod("bench_step", ONSTEP_MOD, dir).expect("load");
    vm.freeze().expect("freeze");

    let on_step = best_of(200, || {
        for _ in 0..1000 {
            vm.call_void("bench_step", "on_step").expect("on_step");
        }
    });

    // -- 3. sandbox setup per mod --------------------------------------------
    let sandbox = best_of(100, || {
        let mut fresh = EngineVm::new(limits).expect("create vm");
        fresh
            .load_mod("bench_empty", "", dir)
            .expect("load empty mod");
        black_box(&fresh);
    });

    // Setup cost with the VM already up — what the hundredth mod costs, as
    // opposed to the first.
    let mut shared = EngineVm::new(limits).expect("create vm");
    let mut counter = 0u32;
    let per_mod = best_of(200, || {
        counter += 1;
        shared
            .load_mod(&format!("bench_mod_{counter}"), "", dir)
            .expect("load empty mod");
    });

    // -- 3b. the SAME work, natively -----------------------------------------
    //
    // Task 04's recorded 53.5 µs is for a similar but not identical recipe, so
    // comparing against it would give an overhead figure that is really a
    // difference between two workloads. This runs exactly what the Lua callback
    // runs, in Rust, in this same binary — which is what makes the overhead
    // number mean what it claims.
    let native = best_of(200, || {
        use tiamot_core::detgen::{ChunkBuffer, Fractal, FractalParams, Region2d, fill_2d};
        let params = FractalParams {
            fractal: Fractal::Fbm,
            octaves: 4,
            frequency: 0.03,
            lacunarity: 2.0,
            gain: 0.5,
        };
        let region = Region2d {
            origin_x: 0.0,
            origin_y: 0.0,
            step_x: 1.0,
            step_y: 1.0,
            width: tiamot_core::CHUNK_BLOCKS as usize,
            height: tiamot_core::CHUNK_BLOCKS as usize,
        };
        let mut samples = vec![0.0f32; region.len()];
        fill_2d(42, &region, &params, &mut samples).expect("fill");
        let heights: Vec<i32> = samples
            .iter()
            .map(|sample| 8 + (sample * 6.0) as i32)
            .collect();
        let mut buffer = ChunkBuffer::new(ChunkPos::new(0, 0, 0), tiamot_core::MaterialId::AIR);
        buffer
            .fill_below_heightmap(&heights, tiamot_core::MaterialId(2))
            .expect("fill");
        black_box(buffer.to_chunk());
    });

    // -- 4. trivial call boundary --------------------------------------------
    let trivial = best_of(500, || {
        for _ in 0..1000 {
            vm.call_void("bench_step", "trivial").expect("trivial");
        }
    });

    // -- report ---------------------------------------------------------------
    let worldgen_us = worldgen.as_secs_f64() * 1e6;
    let native_us = native.as_secs_f64() * 1e6;

    println!("{:<38} {:>14}", "workload", "time");
    println!("{:-<54}", "");
    println!(
        "{:<38} {:>11.2} µs",
        "worldgen callback (per chunk)", worldgen_us
    );
    println!(
        "{:<38} {:>11.2} µs",
        "  same work, native, same binary", native_us
    );
    println!(
        "{:<38} {:>11.1} %   (gate: < 20%)",
        "  orchestration overhead",
        (worldgen_us / native_us - 1.0) * 100.0
    );
    println!(
        "{:<38} {:>11.2} µs",
        "on_step x1000 (one tick)",
        on_step.as_secs_f64() * 1e6
    );
    println!(
        "{:<38} {:>11.3} µs",
        "  per on_step call",
        on_step.as_secs_f64() * 1e6 / 1000.0
    );
    println!(
        "{:<38} {:>11.2} µs",
        "sandbox setup (fresh VM + 1 mod)",
        sandbox.as_secs_f64() * 1e6
    );
    println!(
        "{:<38} {:>11.2} µs",
        "sandbox setup (extra mod, VM up)",
        per_mod.as_secs_f64() * 1e6
    );
    println!(
        "{:<38} {:>11.3} µs",
        "trivial call boundary (per call)",
        trivial.as_secs_f64() * 1e6 / 1000.0
    );
}
