// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Task 02b sub-node risk spike.
//!
//! Throwaway measurement code. Its only product is the numbers in
//! `docs/subnode-verdict.md`, which decide keep / keep-with-limits / fallback
//! for the whole 3×3×3 sub-node design.
//!
//! Every probe runs headless with one command and prints a table:
//!
//! ```text
//! cargo run -p subnode-spike --release -- mesh
//! cargo run -p subnode-spike --release -- collision
//! cargo run -p subnode-spike --release -- lighting
//! cargo run -p subnode-spike --release -- storage
//! cargo run -p subnode-spike --release --features gpu -- vram
//! cargo run -p subnode-spike --release --features gpu -- all
//! ```
//!
//! Always `--release`. A debug build measures LLVM's absence, not the design.

mod collision;
mod export;
mod lighting;
mod mesher;
mod scenes;
mod storage;
mod vram;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use crate::mesher::SubNodeGrid;
use crate::scenes::Scene;

/// Ticks per second the server runs at, from the charter's fixed timestep.
const TICK_RATE: usize = 20;

/// Fixed default so every run of the spike produces comparable numbers.
const DEFAULT_SEED: u64 = 0x5AB_0DE5;

/// Samples per timed measurement. See [`best_of`].
const ITERATIONS: u32 = 500;

#[derive(Parser)]
#[command(name = "subnode-spike", about = "Task 02b sub-node risk spike")]
struct Cli {
    #[command(subcommand)]
    probe: Probe,

    /// Seed for scene generation. Every scene is deterministic in this.
    #[arg(long, default_value_t = DEFAULT_SEED, global = true)]
    seed: u64,

    /// Where to write OBJ artefacts for the human gates.
    #[arg(long, default_value = "spikes/subnode/out", global = true)]
    out: PathBuf,

    /// Percentage of surface blocks chiselled in the VRAM world. The gate
    /// specifies 10; 100 answers the worst case.
    #[arg(long, default_value_t = vram::CHISELLED_PERCENT, global = true)]
    chiselled_percent: u32,
}

#[derive(Subcommand)]
enum Probe {
    /// Deliverable 2: mesh time, geometry counts, remesh after one edit.
    Mesh,
    /// Deliverable 3: swept-AABB collision for 100 players.
    Collision,
    /// Deliverable 4: light BFS with and without the 3×3 mask test.
    Lighting,
    /// Deliverable 5: compressed chunk sizes and chiselling delta bandwidth.
    Storage,
    /// Deliverable 2 (VRAM): real GPU buffers for a view-distance-12 world.
    Vram,
    /// Every probe, in order.
    All,
}

fn main() {
    let cli = Cli::parse();
    match cli.probe {
        Probe::Mesh => probe_mesh(cli.seed),
        Probe::Collision => probe_collision(cli.seed, &cli.out),
        Probe::Lighting => probe_lighting(cli.seed),
        Probe::Storage => probe_storage(cli.seed),
        Probe::Vram => probe_vram(cli.seed, cli.chiselled_percent),
        Probe::All => {
            probe_mesh(cli.seed);
            probe_collision(cli.seed, &cli.out);
            probe_lighting(cli.seed);
            probe_storage(cli.seed);
            // Both the gate's assumption and the worst case, because the gate's
            // 10% is an assumption about player behaviour and the spike exists
            // to find out what happens when an assumption is wrong.
            probe_vram(cli.seed, vram::CHISELLED_PERCENT);
            probe_vram(cli.seed, 100);
        }
    }
}

/// Runs `f` repeatedly and returns the best time.
///
/// Best rather than mean, deliberately. The question a gate asks is "can this
/// design go fast enough", and the minimum is the closest estimate of the true
/// cost with scheduler noise and cache-cold effects removed. Means on a shared
/// machine measure the machine.
fn best_of(iterations: u32, mut f: impl FnMut()) -> Duration {
    // These probes measure operations in the tens of microseconds, where 20
    // samples leaves enough scheduler noise to show a *negative* delta between
    // two runs of strictly more work. ITERATIONS is set high enough that the
    // minimum is stable to the reported precision.
    // One warm-up, discarded: the first run pays page faults for buffers every
    // later run reuses.
    f();
    let mut best = Duration::MAX;
    for _ in 0..iterations {
        let start = Instant::now();
        f();
        best = best.min(start.elapsed());
    }
    best
}

fn header(title: &str) {
    println!();
    println!("=== {title} ===");
}

fn probe_mesh(seed: u64) {
    header("Deliverable 2 — binary greedy meshing (48³ sub-node grid)");
    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>10} {:>10} {:>11} {:>10}",
        "scene", "extract", "mesh", "total", "quads", "vertices", "indices", "gpu KiB"
    );

    let mut remesh_rows = Vec::new();

    for scene in Scene::ALL {
        let chunk = scene.build(seed);

        let extract = best_of(ITERATIONS, || {
            std::hint::black_box(SubNodeGrid::from_chunk(&chunk));
        });
        let grid = SubNodeGrid::from_chunk(&chunk);
        let mesh_only = best_of(ITERATIONS, || {
            std::hint::black_box(mesher::mesh(&grid));
        });
        let total = best_of(ITERATIONS, || {
            std::hint::black_box(mesher::mesh(&SubNodeGrid::from_chunk(&chunk)));
        });

        let meshed = mesher::mesh(&grid);
        println!(
            "{:<18} {:>8.3}ms {:>8.3}ms {:>8.3}ms {:>10} {:>10} {:>11} {:>10.1}",
            scene.label(),
            extract.as_secs_f64() * 1000.0,
            mesh_only.as_secs_f64() * 1000.0,
            total.as_secs_f64() * 1000.0,
            meshed.quads.len(),
            meshed.vertex_count(),
            meshed.index_count(),
            meshed.gpu_bytes() as f64 / 1024.0,
        );

        // Remesh after a single sub-node edit: the cost a player pays per
        // chisel stroke. A real engine would remesh only the affected region;
        // this is the whole-chunk figure, so it is an upper bound.
        let mut edited = chunk.clone();
        edited
            .set_subnode(
                tiamot_core::coords::SubNodePos::new(24, 23, 24),
                tiamot_core::MaterialId::AIR,
            )
            .expect("in chunk");
        let remesh = best_of(ITERATIONS, || {
            std::hint::black_box(mesher::mesh(&SubNodeGrid::from_chunk(&edited)));
        });
        remesh_rows.push((scene, remesh));
    }

    println!();
    println!("{:<18} {:>12}", "scene", "remesh after 1 sub-node edit");
    for (scene, remesh) in remesh_rows {
        println!(
            "{:<18} {:>11.3}ms",
            scene.label(),
            remesh.as_secs_f64() * 1000.0
        );
    }

    // Scene (b) vertex count against scene (a)'s equivalent surface, which is
    // the KEEP gate's geometry-inflation test.
    let flat = mesher::mesh(&SubNodeGrid::from_chunk(&Scene::Flat.build(seed)));
    let chiselled = mesher::mesh(&SubNodeGrid::from_chunk(&Scene::Chiselled.build(seed)));
    let realistic = mesher::mesh(&SubNodeGrid::from_chunk(&Scene::Realistic.build(seed)));
    println!();
    println!(
        "geometry inflation, scene (b) vs (a): {:.1}x vertices  (gate: < 8x)",
        chiselled.vertex_count() as f64 / flat.vertex_count().max(1) as f64
    );
    println!(
        "  scene (a) absolute: {} vertices — a flat 48x48 plane greedy-merges to a\n           handful of quads, so this denominator is near-zero and the ratio above is\n           unbounded by construction. Reported as specified; see the verdict memo.",
        flat.vertex_count()
    );
    println!(
        "  scene (b) vs (d):   {:.1}x vertices   (the comparison that discriminates)",
        chiselled.vertex_count() as f64 / realistic.vertex_count().max(1) as f64
    );
}

fn probe_collision(seed: u64, out: &std::path::Path) {
    header("Deliverable 3 — swept-AABB collision at sub-node resolution");
    println!(
        "{:<18} {:>7} {:>7} {:>12} {:>11} {:>9} {:>9}",
        "scene", "bodies", "ticks", "ms/tick(100)", "us/body", "step-ups", "embedded"
    );

    for scene in [Scene::Chiselled, Scene::Realistic] {
        let grid = SubNodeGrid::from_chunk(&scene.build(seed));
        let ticks = TICK_RATE * 5;

        let elapsed = best_of(3, || {
            std::hint::black_box(collision::simulate(&grid, 100, ticks, seed, None));
        });
        let result = collision::simulate(&grid, 100, ticks, seed, None);

        let per_tick = elapsed.as_secs_f64() * 1000.0 / ticks as f64;
        println!(
            "{:<18} {:>7} {:>7} {:>11.4}ms {:>10.3}us {:>9} {:>9}",
            scene.label(),
            result.bodies,
            result.ticks,
            per_tick,
            per_tick * 1000.0 / 100.0,
            result.step_ups,
            result.embedded,
        );
    }

    // Artefacts for the [H] gate: the geometry and one body's path through it.
    if let Err(err) = std::fs::create_dir_all(out) {
        println!("could not create {}: {err}", out.display());
        return;
    }

    let chunk = Scene::Chiselled.build(seed);
    let grid = SubNodeGrid::from_chunk(&chunk);
    let mut trace = Vec::new();
    let wander = collision::simulate(&grid, 1, TICK_RATE * 30, seed, Some(&mut trace));

    let meshed = mesher::mesh(&grid);
    let _ = export::write_mesh_obj(
        &out.join("chiselled-scene.obj"),
        &meshed,
        "scene (b) chiselled city",
    );
    let _ = export::write_path_obj(
        &out.join("player-path.obj"),
        &trace,
        "player wandering scene (b), 30s at 20 tps",
    );

    // A random walk over a chiselled field barely triggers step-up — it found
    // only a handful of lips in 30 seconds, which is not enough to judge how
    // the mechanic feels. So the artefact for the human gate is a purpose-built
    // staircase of rising step heights: the body walks east and climbs steps of
    // 1, 2, and 3 sub-nodes in turn. Exactly which of those it surmounts is the
    // whole question, and it is visible at a glance in the exported path.
    let stairs = build_staircase();
    let stair_grid = SubNodeGrid::from_chunk(&stairs);
    let mut stair_trace = Vec::new();
    let stair_result = walk_east(&stair_grid, &mut stair_trace);
    let _ = export::write_mesh_obj(
        &out.join("staircase-scene.obj"),
        &mesher::mesh(&stair_grid),
        "step-up test: 1, 2 and 3 sub-node rises",
    );
    let _ = export::write_path_obj(
        &out.join("staircase-path.obj"),
        &stair_trace,
        "player walking east up rising steps",
    );

    println!();
    println!("[H] step-up feel — artefacts written for you to inspect:");
    for name in [
        "chiselled-scene.obj",
        "player-path.obj",
        "staircase-scene.obj",
        "staircase-path.obj",
    ] {
        println!("      {}", out.join(name).display());
    }
    println!(
        "    Units are yards. A player is 1.8 tall, so one sub-node (1/3 yard) is\n    \
         a sixth of body height. Wandering scene (b) produced {} step-ups in 30s;\n    \
         the staircase produced {}, and the body reached x={:.2} yards.",
        wander.step_ups,
        stair_result.0,
        stair_result.1 / 3.0,
    );
    println!(
        "    Three separate platforms 1, 2 and 3 sub-nodes tall sit at x = 8, 10.7 and\n    \
         13.3 yards, with flat floor between them. Design intent: clear the 1,\n    \
         stop at the 2. Your call whether that is the right ceiling."
    );
}

/// A flat floor with rising steps of 1, 2 and 3 sub-nodes, for the step-up gate.
fn build_staircase() -> tiamot_core::Chunk {
    use tiamot_core::coords::SubNodePos;

    let mut chunk = tiamot_core::Chunk::air(tiamot_core::ChunkPos::new(0, 0, 0));
    let n = mesher::N as i32;
    for z in 0..n {
        for x in 0..n {
            chunk
                .set_subnode(SubNodePos::new(x, 0, z), scenes::STONE)
                .expect("in chunk");
        }
    }
    // Three separate platforms on flat ground, 1, 2 and 3 sub-nodes tall.
    //
    // Separate, not cumulative: a staircase of rising terraces would present a
    // 1-sub-node rise at every step regardless of the terrace heights, since
    // the body is already standing on the previous one. Isolated platforms with
    // floor between them are what actually tests "how tall a lip can be
    // climbed".
    for (start, height) in [(24, 1), (32, 2), (40, 3)] {
        for z in 0..n {
            for x in start..(start + 4).min(n) {
                for y in 1..=height {
                    chunk
                        .set_subnode(SubNodePos::new(x, y, z), scenes::STONE)
                        .expect("in chunk");
                }
            }
        }
    }
    chunk
}

/// Walks one body east across the staircase, returning `(step-ups, final x)`.
fn walk_east(grid: &SubNodeGrid, trace: &mut Vec<[f32; 3]>) -> (u32, f32) {
    let solid = collision::Solid::new(grid);
    let mut body = collision::Body {
        position: [4.0, 1.0, 24.0],
        velocity: [0.0; 3],
        on_ground: true,
        steps: 0,
    };
    for _ in 0..(TICK_RATE * 20) {
        body.velocity[0] = 0.35;
        body = collision::step(&solid, body, 0.08);
        trace.push(body.position);
    }
    (body.steps, body.position[0])
}

fn probe_lighting(seed: u64) {
    header("Deliverable 4 — light BFS, 3×3 mask test vs solid-Partial baseline");
    println!(
        "{:<18} {:>11} {:>11} {:>8} {:>13} {:>11} {:>11}",
        "scene", "baseline", "mask test", "delta", "propagations", "lit blocks", "light delta"
    );

    for scene in Scene::ALL {
        let chunk = scene.build(seed);

        let baseline_time = best_of(ITERATIONS, || {
            std::hint::black_box(lighting::propagate(
                &chunk,
                lighting::Permeability::SolidPartial,
            ));
        });
        let masked_time = best_of(ITERATIONS, || {
            std::hint::black_box(lighting::propagate(
                &chunk,
                lighting::Permeability::SubNodeMask,
            ));
        });

        let baseline = lighting::propagate(&chunk, lighting::Permeability::SolidPartial);
        let masked = lighting::propagate(&chunk, lighting::Permeability::SubNodeMask);

        let delta = (masked_time.as_secs_f64() / baseline_time.as_secs_f64() - 1.0) * 100.0;
        println!(
            "{:<18} {:>9.3}ms {:>9.3}ms {:>7.1}% {:>13} {:>11} {:>10.1}%",
            scene.label(),
            baseline_time.as_secs_f64() * 1000.0,
            masked_time.as_secs_f64() * 1000.0,
            delta,
            masked.propagations,
            masked.lit_blocks,
            (masked.total_light as f64 / baseline.total_light.max(1) as f64 - 1.0) * 100.0,
        );
    }
    println!();
    println!("gate: mask-test delta < 25% over baseline on scenes (b) and (d)");
}

fn probe_storage(seed: u64) {
    header("Deliverable 5 — storage and bandwidth");
    println!(
        "{:<18} {:>10} {:>12} {:>10} {:>10}",
        "scene", "raw KiB", "zstd bytes", "ratio", "palette"
    );

    for scene in Scene::ALL {
        let chunk = scene.build(seed);
        let raw = storage::serialize(&chunk);
        let compressed = storage::compress(&raw);
        println!(
            "{:<18} {:>9.1} {:>12} {:>9.1}x {:>10}",
            scene.label(),
            raw.len() as f64 / 1024.0,
            compressed.len(),
            raw.len() as f64 / compressed.len() as f64,
            chunk.palette_len(),
        );
    }

    println!();
    println!("gate: scene (b) compressed chunk < 8 KiB (8192 bytes)");

    // 60 seconds of continuous chiselling at 20 tps.
    let ticks = TICK_RATE * 60;
    let mut chunk = Scene::Flat.build(seed);
    let edits = storage::record_session(&mut chunk, ticks, seed);
    let sizes = storage::encode_deltas(&edits, &chunk);

    println!();
    println!(
        "60s continuous chiselling ({} edits at {TICK_RATE} tps):",
        sizes.edits
    );
    println!(
        "{:<28} {:>12} {:>12}",
        "encoding", "raw KiB/min", "zstd KiB/min"
    );
    println!(
        "{:<28} {:>11.2} {:>11.2}",
        "dedicated sub-node delta",
        sizes.subnode_raw as f64 / 1024.0,
        sizes.subnode_compressed as f64 / 1024.0,
    );
    println!(
        "{:<28} {:>11.2} {:>11.2}",
        "block path (resend 27 cells)",
        sizes.block_path_raw as f64 / 1024.0,
        sizes.block_path_compressed as f64 / 1024.0,
    );
    println!();
    println!("gate: chiselling delta stream < 32 KiB/min/player");
}

fn probe_vram(seed: u64, chiselled_percent: u32) {
    header(&format!(
        "Deliverable 2 (VRAM) — measured GPU allocation, view distance 12, \
         {chiselled_percent}% chiselled"
    ));
    match vram::measure(seed, chiselled_percent) {
        Ok(result) => {
            println!(
                "adapter:            {} ({})",
                result.adapter, result.backend
            );
            println!("chunks in view:     {}", result.chunks);
            println!("chunks with mesh:   {}", result.non_empty_chunks);
            println!("quads:              {}", result.quads);
            println!("vertices:           {}", result.vertices);
            println!("indices:            {}", result.indices);
            println!("logical mesh bytes: {:.1} MiB", result.logical_mib());
            match (result.allocated_mib(), result.allocated_bytes) {
                (Some(mib), Some(bytes)) => {
                    println!("ALLOCATED (device): {mib:.1} MiB   (gate: < 1536 MiB)");
                    println!(
                        "alignment overhead: {:.2}%",
                        (bytes as f64 / result.logical_bytes.max(1) as f64 - 1.0) * 100.0
                    );
                }
                _ => println!(
                    "ALLOCATED (device): NOT MEASURED — rebuild with --features gpu \
                     on a machine with a GPU adapter"
                ),
            }
        }
        Err(err) => {
            println!("NOT MEASURED: {err}");
        }
    }
}
