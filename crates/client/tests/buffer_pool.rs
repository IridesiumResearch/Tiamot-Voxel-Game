// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! That streaming chunks through the renderer stops allocating GPU buffers.
//!
//! # Why this test counts allocations rather than milliseconds
//!
//! The bug it protects against was reported as a frame-rate drop: ~900 fps
//! walking across the map, falling to ~90 for about a second at a time. The
//! client's own instrumentation attributed it — 15.6 ms of a 17.0 ms worst
//! frame was the remesh — and the meshes involved came to well under a
//! megabyte, so a cost that large could not be bandwidth. It was
//! `create_buffer_init`: two fresh device allocations per chunk and two frees,
//! four chunks a frame, on an RTX 5070 Ti.
//!
//! **None of that reproduces here.** This devcontainer draws with lavapipe,
//! where a GPU buffer is a `malloc` and the entire problem measures as nothing.
//! A timing gate written on this machine would pass on the broken code and
//! carry no information at all.
//!
//! What IS the same on every machine is *how many buffers get created*, because
//! that is a property of the code and not of the driver. So this asserts the
//! mechanism directly: stream far more chunk updates through the renderer than
//! there are chunks resident, and almost all of them must be served from the
//! pool. The number that made a 5070 Ti stall is the number this pins.

use client::config::RenderMode;
use client::mesher::{Absent, Neighbours, mesh_chunk};
use client::render::{Gpu, Renderer};
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BlockValue, Chunk, ChunkPos, MaterialId};

const STONE: MaterialId = MaterialId(2);
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// See `tests/screenshot.rs` for why a missing adapter skips rather than fails.
fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            assert!(
                std::env::var("TIAMOT_REQUIRE_GPU").is_err(),
                "TIAMOT_REQUIRE_GPU is set and no adapter was available: {err}"
            );
            println!("SKIPPING: no graphics adapter on this machine ({err})");
            None
        }
    }
}

/// A chunk with a surface in it, so the mesh is not empty.
///
/// An empty mesh takes the early-out in `set_chunk` and never touches a buffer,
/// which would make this whole test vacuous.
fn surfaced(pos: ChunkPos, height: u32) -> Chunk {
    let mut chunk = Chunk::new(pos, MaterialId::AIR);
    for z in 0..tiamot_core::CHUNK_BLOCKS {
        for x in 0..tiamot_core::CHUNK_BLOCKS {
            for y in 0..height.min(tiamot_core::CHUNK_BLOCKS) {
                chunk.set_block_local(LocalBlock::new(x, y, z), BlockValue::Uniform(STONE));
            }
        }
    }
    chunk
}

#[test]
fn streaming_chunks_reuses_buffers_instead_of_allocating_new_ones() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");

    // A modest resident set, then far more arrivals than residents — which is
    // what walking does: the interest volume stays about the same size, so a
    // chunk arriving means a chunk leaving.
    const RESIDENT: i32 = 32;
    const PASSES: i32 = 12;

    let mesh_of = |pos: ChunkPos, height: u32| {
        let chunk = surfaced(pos, height);
        mesh_chunk(&chunk, &Neighbours::default(), Absent::Air)
    };

    // Warm up: fill the resident set once. These are honest allocations — the
    // pool has nothing to give yet — and are not what this test is about.
    for i in 0..RESIDENT {
        let pos = ChunkPos::new(i, 0, 0);
        renderer.set_chunk(pos, &mesh_of(pos, 8));
    }
    let (after_warmup, _) = renderer.buffer_stats();
    assert!(
        after_warmup > 0,
        "the warm-up allocated nothing, so every mesh must have been empty and this test \
         proves nothing"
    );

    // Now stream: each pass retires the whole resident set and brings in a
    // fresh one, exactly as crossing chunk boundaries does.
    for pass in 1..=PASSES {
        for i in 0..RESIDENT {
            renderer.remove_chunk(&ChunkPos::new(i + (pass - 1) * RESIDENT, 0, 0));
        }
        for i in 0..RESIDENT {
            let pos = ChunkPos::new(i + pass * RESIDENT, 0, 0);
            renderer.set_chunk(pos, &mesh_of(pos, 8));
        }
    }

    let (created, reused) = renderer.buffer_stats();
    let streamed = created - after_warmup;
    let updates = u64::try_from(RESIDENT * PASSES).expect("fits");

    println!(
        "{updates} streamed chunk updates: {streamed} buffers created after warm-up, \
         {reused} served from the pool"
    );

    // Two buffers per chunk, so unpooled this would be `2 * updates` — 768
    // allocations and 768 frees. The pool should serve essentially all of them.
    assert!(
        streamed <= updates / 8,
        "streaming {updates} chunks created {streamed} buffers after warm-up; unpooled that \
         would be {}, and the pool is meant to make this number roughly zero",
        updates * 2
    );
    assert!(
        reused >= updates,
        "only {reused} of {} buffer requests came from the pool",
        updates * 2
    );
}

#[test]
fn remeshing_a_chunk_in_place_allocates_nothing() {
    // The dig case rather than the streaming case: a chunk that is already
    // resident is remeshed after an edit. Its old buffers are handed back
    // before the new ones are asked for, so the same two come straight back and
    // nothing touches the device allocator at all.
    let Some(gpu) = gpu() else { return };
    let mut renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");

    let pos = ChunkPos::new(0, 0, 0);
    let chunk = surfaced(pos, 8);
    renderer.set_chunk(
        pos,
        &mesh_chunk(&chunk, &Neighbours::default(), Absent::Air),
    );
    let (baseline, _) = renderer.buffer_stats();
    assert!(baseline > 0, "the first upload allocated nothing");

    for _ in 0..50 {
        renderer.set_chunk(
            pos,
            &mesh_chunk(&chunk, &Neighbours::default(), Absent::Air),
        );
    }

    let (created, _) = renderer.buffer_stats();
    assert_eq!(
        created,
        baseline,
        "50 in-place remeshes allocated {} new buffers; a remesh in place must reuse the \
         buffers the previous mesh gave back",
        created - baseline
    );
}

#[test]
fn a_mesh_that_outgrows_its_buffer_gets_a_bigger_one() {
    // The pool must not hand back a buffer that is too small — the failure mode
    // would be a silently truncated mesh, which draws as holes in the world
    // rather than as an error.
    let Some(gpu) = gpu() else { return };
    let mut renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");

    let pos = ChunkPos::new(0, 0, 0);
    // A flat slab meshes to almost nothing; a chequerboard defeats the greedy
    // merge and produces orders of magnitude more geometry.
    let small = surfaced(pos, 1);
    let mut large = Chunk::new(pos, MaterialId::AIR);
    for z in 0..tiamot_core::CHUNK_BLOCKS {
        for x in 0..tiamot_core::CHUNK_BLOCKS {
            for y in 0..tiamot_core::CHUNK_BLOCKS {
                if (x + y + z) % 2 == 0 {
                    large.set_block_local(LocalBlock::new(x, y, z), BlockValue::Uniform(STONE));
                }
            }
        }
    }

    let small_mesh = mesh_chunk(&small, &Neighbours::default(), Absent::Air);
    let large_mesh = mesh_chunk(&large, &Neighbours::default(), Absent::Air);
    assert!(
        large_mesh.to_buffers().0.len() > small_mesh.to_buffers().0.len() * 4,
        "the two scenes are not different enough in size to exercise a grow"
    );

    renderer.set_chunk(pos, &small_mesh);
    renderer.set_chunk(pos, &large_mesh);

    // It drew, and it drew the big one: the reported byte count has to match
    // what the large mesh actually needs, not what the small buffer held.
    let (vertices, indices) = large_mesh.to_buffers();
    let expected =
        (std::mem::size_of_val(&vertices[..]) + std::mem::size_of_val(&indices[..])) as u64;
    assert_eq!(
        renderer.mesh_bytes(),
        expected,
        "the renderer is reporting a different amount of geometry than the mesh contains, so \
         the grow either did not happen or was truncated"
    );
}
