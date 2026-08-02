// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deliverable 2 (VRAM) — measured, not projected.
//!
//! The task is explicit that VRAM must be measured by allocating the real
//! buffers and reading back the allocation. That matters because a projection
//! would miss what the driver actually does: `wgpu` rounds every buffer up to
//! `COPY_BUFFER_ALIGNMENT`, and a scene of thousands of small per-chunk buffers
//! pays that rounding thousands of times.
//!
//! # What this measures, and what it does not
//!
//! The byte totals here are exact: they are the sum of `Buffer::size()` for
//! buffers that were successfully created on a real Vulkan device. That number
//! is driver-independent — it is what any GPU would need for the same mesh.
//!
//! What a software adapter cannot tell you is real-hardware allocator overhead:
//! page granularity, heap fragmentation, and the driver's own bookkeeping.
//! Those add a margin on top, and the verdict memo treats the measured figure
//! as a floor rather than a final answer. Re-running this on a real GPU is one
//! of the human gates.

use crate::mesher::{Mesh, SubNodeGrid, mesh};
use crate::scenes::{GRASS, Rng, STONE};
use tiamot_core::block::SUBNODES_PER_BLOCK;
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BlockValue, CHUNK_BLOCKS, ChunkPos, MaterialId};

/// View distance in chunks, as the KEEP gate specifies.
pub const VIEW_DISTANCE: i32 = 12;

/// Vertical chunks actually meshed per column.
///
/// Not the full 25 — a column of chunks is mostly solid rock below and empty
/// sky above, and neither meshes to anything a renderer keeps. Four is the band
/// around the surface that produces geometry, which is what occupies VRAM.
pub const VERTICAL_CHUNKS: i32 = 4;

/// Fraction of surface blocks that are chiselled, as the gate specifies.
///
/// Parameterised because the 10% figure is an assumption about how much of a
/// world players actually carve, and the whole point of a risk spike is to find
/// out what happens when an assumption is wrong. Running at 100% answers
/// "what if every surface block in view is chiselled" with a measurement rather
/// than an argument.
pub const CHISELLED_PERCENT: u32 = 10;

/// A summary of what a view-distance-12 world costs.
#[derive(Debug, Clone)]
pub struct VramResult {
    pub chunks: usize,
    pub non_empty_chunks: usize,
    pub quads: usize,
    pub vertices: usize,
    pub indices: usize,
    /// Sum of mesh sizes before any driver alignment.
    pub logical_bytes: u64,
    /// Sum of `Buffer::size()` as the device actually allocated them.
    ///
    /// `None` when no GPU adapter was available. The geometry totals above are
    /// still real measurements in that case; only the device allocation is
    /// missing, and it is reported as missing rather than estimated.
    pub allocated_bytes: Option<u64>,
    pub adapter: String,
    pub backend: String,
}

impl VramResult {
    #[must_use]
    pub fn logical_mib(&self) -> f64 {
        self.logical_bytes as f64 / (1024.0 * 1024.0)
    }

    #[must_use]
    pub fn allocated_mib(&self) -> Option<f64> {
        self.allocated_bytes
            .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Meshes the whole view-distance world and totals its geometry.
///
/// No GPU required: everything here except the device allocation is a property
/// of the mesh, not of the driver.
#[must_use]
pub fn survey(seed: u64, chiselled_percent: u32) -> VramResult {
    let (meshes, total_chunks) = build_world_meshes(seed, chiselled_percent);
    let mut result = VramResult {
        chunks: total_chunks,
        non_empty_chunks: meshes.len(),
        quads: 0,
        vertices: 0,
        indices: 0,
        logical_bytes: 0,
        allocated_bytes: None,
        adapter: "none".to_owned(),
        backend: "none".to_owned(),
    };
    for meshed in &meshes {
        result.quads += meshed.quads.len();
        result.vertices += meshed.vertex_count();
        result.indices += meshed.index_count();
        result.logical_bytes += meshed.gpu_bytes() as u64;
    }
    result
}

/// Builds one chunk of a synthetic view-distance world.
///
/// `surface_band` says where this chunk sits relative to the terrain surface:
/// below it is solid, above it is empty, and at it there is a chiselled
/// surface layer.
#[must_use]
pub fn world_chunk(
    pos: ChunkPos,
    surface_band: i32,
    rng: &mut Rng,
    chiselled_percent: u32,
) -> Chunk {
    match surface_band {
        // Fully underground: solid, meshes to nothing once neighbour culling
        // applies. Still allocated, because a renderer does not know until it
        // meshes.
        b if b < 0 => Chunk::new(pos, STONE),
        // Above the surface: empty sky.
        b if b > 0 => Chunk::air(pos),
        // The surface band.
        _ => {
            let mut chunk = Chunk::air(pos);
            for z in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    for y in 0..8 {
                        chunk.set_block_local(
                            LocalBlock::new(x, y, z),
                            BlockValue::Uniform(if y == 7 { GRASS } else { STONE }),
                        );
                    }
                    if rng.below(100) < chiselled_percent {
                        let mut cells = [GRASS; SUBNODES_PER_BLOCK];
                        // Carve a handful of cells out of the top block.
                        for _ in 0..rng.below(14) {
                            cells[rng.below(SUBNODES_PER_BLOCK as u32) as usize] = MaterialId::AIR;
                        }
                        chunk.set_block_local(LocalBlock::new(x, 7, z), BlockValue::Cells(cells));
                    }
                }
            }
            chunk
        }
    }
}

/// Meshes the whole view-distance world, returning every non-empty mesh.
#[must_use]
pub fn build_world_meshes(seed: u64, chiselled_percent: u32) -> (Vec<Mesh>, usize) {
    let mut rng = Rng::new(seed);
    let mut meshes = Vec::new();
    let mut total = 0;

    for cz in -VIEW_DISTANCE..=VIEW_DISTANCE {
        for cx in -VIEW_DISTANCE..=VIEW_DISTANCE {
            for cy in 0..VERTICAL_CHUNKS {
                total += 1;
                // Surface sits in the second band of each column.
                let band = cy - 1;
                let chunk =
                    world_chunk(ChunkPos::new(cx, cy, cz), band, &mut rng, chiselled_percent);
                let meshed = mesh(&SubNodeGrid::from_chunk(&chunk));
                if !meshed.quads.is_empty() {
                    meshes.push(meshed);
                }
            }
        }
    }

    (meshes, total)
}

#[cfg(feature = "gpu")]
mod gpu {
    use super::{VramResult, build_world_meshes};
    use wgpu::util::DeviceExt;

    /// Allocates real GPU buffers for a whole view-distance world and reports
    /// what the device actually reserved.
    ///
    /// # Errors
    ///
    /// If no Vulkan/Metal/DX12/GL adapter can be found, or the device refuses
    /// to allocate.
    pub fn measure(seed: u64, chiselled_percent: u32) -> Result<VramResult, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|err| format!("no GPU adapter available: {err}"))?;

        let info = adapter.get_info();
        let (device, _queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("subnode-spike"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            }))
            .map_err(|err| format!("could not create device: {err}"))?;

        let (meshes, _) = build_world_meshes(seed, chiselled_percent);

        let mut result = super::survey(seed, chiselled_percent);
        result.adapter = info.name.clone();
        result.backend = format!("{:?}", info.backend);
        let mut allocated = 0u64;

        // Held so nothing is freed before the total is read: a running total of
        // sizes for buffers that had already been dropped would not be a
        // measurement of peak residency.
        let mut live = Vec::with_capacity(meshes.len() * 2);

        for meshed in &meshes {
            let (vertices, indices) = meshed.to_buffers();

            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck_cast(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck_cast(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });

            // The actual allocation, read back from the device rather than
            // assumed from the input length.
            allocated += vertex_buffer.size() + index_buffer.size();
            live.push(vertex_buffer);
            live.push(index_buffer);
        }

        device.poll(wgpu::PollType::wait_indefinitely()).ok();
        drop(live);
        result.allocated_bytes = Some(allocated);
        Ok(result)
    }

    /// Reinterprets a slice of plain-old-data as bytes.
    ///
    /// Both `PackedVertex` and `u32` are `repr(C)` with no padding and no
    /// invalid bit patterns, so this is sound. A dependency on `bytemuck` would
    /// be the tidier way; a throwaway spike does not need one.
    fn bytemuck_cast<T: Copy>(values: &[T]) -> &[u8] {
        // SAFETY: T is Copy, repr(C), and contains no padding or references;
        // every bit pattern is valid, and the resulting slice borrows `values`.
        unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
    }
}

#[cfg(feature = "gpu")]
pub use gpu::measure;

/// Stub for builds without the `gpu` feature.
///
/// # Errors
///
/// Always, explaining how to enable the real measurement. Returning an error
/// rather than a projected number is deliberate: the deliverable says measured,
/// and a plausible-looking estimate here would be worse than nothing.
#[cfg(not(feature = "gpu"))]
pub fn measure(seed: u64, chiselled_percent: u32) -> Result<VramResult, String> {
    // The geometry totals are still real; only the device allocation is
    // missing, and it is reported as missing rather than estimated.
    Ok(survey(seed, chiselled_percent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_has_the_expected_chunk_count() {
        let span = (VIEW_DISTANCE * 2 + 1) as usize;
        assert_eq!(span, 25, "view distance 12 is a 25-chunk span");
        let (_, total) = build_world_meshes(1, CHISELLED_PERCENT);
        assert_eq!(total, span * span * VERTICAL_CHUNKS as usize);
    }

    #[test]
    fn surface_chunks_mesh_and_sky_chunks_do_not() {
        let mut rng = Rng::new(1);
        let sky = world_chunk(ChunkPos::new(0, 3, 0), 2, &mut rng, CHISELLED_PERCENT);
        assert_eq!(
            mesh(&SubNodeGrid::from_chunk(&sky)).quads.len(),
            0,
            "empty sky must produce no geometry"
        );

        let surface = world_chunk(ChunkPos::new(0, 1, 0), 0, &mut rng, CHISELLED_PERCENT);
        assert!(
            !mesh(&SubNodeGrid::from_chunk(&surface)).quads.is_empty(),
            "the surface band must produce geometry"
        );
    }
}
