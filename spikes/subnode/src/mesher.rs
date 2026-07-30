// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Binary greedy meshing over the 48³ sub-node grid.
//!
//! # Why binary, and why this is not a free variable
//!
//! A classic per-voxel greedy mesher walks cells one at a time and costs
//! roughly 4.5 ms on a chunk this size. Binary greedy meshing represents
//! occupancy as bitmasks in `u64` words and does face culling with a shift and
//! an AND across a whole column at once — 64 cells per instruction instead of
//! one. Published implementations land around 50–200 µs for a comparable
//! volume, roughly 7× faster.
//!
//! Measuring a classic mesher here would have measured the wrong thing and
//! could have killed a viable design on an implementation artefact.
//!
//! # The `u64`-column invariant
//!
//! This is the single most important consequence of the 16³-block chunk size
//! (charter rule 6). A chunk is 48 sub-node cells per axis. Face culling needs
//! to know about the neighbouring cell just outside the chunk at each end, so a
//! column needs 48 + 2 = **50 bits — one `u64`**.
//!
//! A 32³-block chunk would be 96 cells per axis, need 98 bits, and lose the
//! technique entirely: every column operation would become a multi-word
//! sequence with carries between words. The chunk size is chosen to make this
//! work, not the other way round.
//!
//! Bit layout of a column: bit 0 is the neighbour at −1, bits 1..=48 are the
//! chunk's own cells, bit 49 is the neighbour at +48.

use tiamot_core::block::subnode_index;
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BLOCKS_PER_CHUNK, CHUNK_SUBNODES, SUBNODES_PER_AXIS};

/// Sub-node cells per axis in a chunk.
pub const N: usize = CHUNK_SUBNODES as usize;

/// Cells in a chunk.
pub const CELLS: usize = N * N * N;

/// Bit offset of the chunk's first cell within a column word.
const FIRST: u32 = 1;

/// The six face directions, as (axis, positive).
const FACES: [(usize, bool); 6] = [
    (0, false),
    (0, true),
    (1, false),
    (1, true),
    (2, false),
    (2, true),
];

/// A chunk expanded into a flat sub-node grid plus per-axis occupancy columns.
///
/// This is the representation the mesher wants; building it is part of the
/// measured cost, because a real renderer would have to do it too.
pub struct SubNodeGrid {
    /// Material of every cell, `x + N*y + N*N*z`.
    materials: Vec<u16>,
    /// Occupancy columns per axis. See the module docs for the bit layout.
    columns: [Vec<u64>; 3],
}

impl SubNodeGrid {
    /// Expands a chunk. Neighbours outside the chunk are treated as air, so
    /// boundary faces are emitted.
    ///
    /// A real renderer would seed the padding bits from adjacent chunks and
    /// cull the shared faces. Leaving them in makes every number here a
    /// pessimistic bound, which is the right direction for a gate.
    #[must_use]
    pub fn from_chunk(chunk: &Chunk) -> Self {
        let mut materials = vec![0u16; CELLS];
        let mut columns = [vec![0u64; N * N], vec![0u64; N * N], vec![0u64; N * N]];

        for index in 0..BLOCKS_PER_CHUNK {
            let local = LocalBlock::from_index(index);
            let view = chunk.get_block_local(local);

            // Uniform air is the overwhelmingly common case in a real chunk and
            // contributes nothing; skipping it early is most of why the flat
            // scene is fast.
            if view.is_air() {
                continue;
            }

            let base_x = local.x as usize * SUBNODES_PER_AXIS as usize;
            let base_y = local.y as usize * SUBNODES_PER_AXIS as usize;
            let base_z = local.z as usize * SUBNODES_PER_AXIS as usize;

            for cz in 0..SUBNODES_PER_AXIS {
                for cy in 0..SUBNODES_PER_AXIS {
                    for cx in 0..SUBNODES_PER_AXIS {
                        let material = view.subnode(subnode_index(cx, cy, cz));
                        if material.is_air() {
                            continue;
                        }
                        let x = base_x + cx as usize;
                        let y = base_y + cy as usize;
                        let z = base_z + cz as usize;

                        materials[x + N * y + N * N * z] = material.get();
                        columns[0][y * N + z] |= 1 << (x as u32 + FIRST);
                        columns[1][x * N + z] |= 1 << (y as u32 + FIRST);
                        columns[2][x * N + y] |= 1 << (z as u32 + FIRST);
                    }
                }
            }
        }

        Self { materials, columns }
    }

    #[must_use]
    fn material(&self, x: usize, y: usize, z: usize) -> u16 {
        self.materials[x + N * y + N * N * z]
    }

    /// Whether a cell is occupied. Occupancy is "not air" (charter rule 5).
    #[must_use]
    pub fn is_solid(&self, x: usize, y: usize, z: usize) -> bool {
        self.materials[x + N * y + N * N * z] != 0
    }

    /// Cell coordinates for a face at `(u, v, w)` on the given axis.
    ///
    /// Each axis picks a different pair of the remaining coordinates as its
    /// plane; this is the one place that mapping lives.
    #[must_use]
    const fn cell(axis: usize, u: usize, v: usize, w: usize) -> (usize, usize, usize) {
        match axis {
            0 => (w, u, v),
            1 => (u, w, v),
            _ => (u, v, w),
        }
    }
}

/// One merged quad.
///
/// Kept so the VRAM measurement allocates real buffers containing real data
/// rather than a zeroed block of the right size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quad {
    pub axis: u8,
    pub positive: bool,
    /// Slice index along the axis.
    pub w: u8,
    pub u: u8,
    pub v: u8,
    /// Extent along u, at least 1.
    pub du: u8,
    /// Extent along v, at least 1.
    pub dv: u8,
    pub material: u16,
}

/// A meshed chunk.
#[derive(Debug, Default)]
pub struct Mesh {
    pub quads: Vec<Quad>,
}

impl Mesh {
    /// Four corners per quad.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.quads.len() * 4
    }

    /// Two triangles per quad.
    #[must_use]
    pub fn index_count(&self) -> usize {
        self.quads.len() * 6
    }

    /// Bytes a GPU vertex buffer would need.
    ///
    /// See [`PackedVertex`]: 8 bytes per vertex, which is what a voxel engine
    /// with quantised positions actually ships.
    #[must_use]
    pub fn vertex_bytes(&self) -> usize {
        self.vertex_count() * size_of::<PackedVertex>()
    }

    /// Bytes a GPU index buffer would need, at `u32` indices.
    #[must_use]
    pub fn index_bytes(&self) -> usize {
        self.index_count() * size_of::<u32>()
    }

    #[must_use]
    pub fn gpu_bytes(&self) -> usize {
        self.vertex_bytes() + self.index_bytes()
    }

    /// Expands to the vertex and index buffers a renderer would upload.
    #[must_use]
    pub fn to_buffers(&self) -> (Vec<PackedVertex>, Vec<u32>) {
        let mut vertices = Vec::with_capacity(self.vertex_count());
        let mut indices = Vec::with_capacity(self.index_count());

        for quad in &self.quads {
            let base = vertices.len() as u32;
            for (du, dv) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
                let u = quad.u as u32 + du * u32::from(quad.du);
                let v = quad.v as u32 + dv * u32::from(quad.dv);
                let w = quad.w as u32 + u32::from(quad.positive);
                let (x, y, z) = match quad.axis {
                    0 => (w, u, v),
                    1 => (u, w, v),
                    _ => (u, v, w),
                };
                vertices.push(PackedVertex::new(
                    x,
                    y,
                    z,
                    quad.axis,
                    quad.positive,
                    quad.material,
                ));
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        (vertices, indices)
    }
}

/// A quantised vertex: 8 bytes.
///
/// Positions are sub-node coordinates in `0..=48`, which needs 6 bits per axis.
/// Storing `f32` positions would be 12 bytes for position alone and is what a
/// naive implementation does; quantising is standard practice in voxel
/// renderers and the VRAM numbers here assume it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedVertex {
    /// x:6 | y:6 | z:6 | axis:2 | positive:1
    pub packed: u32,
    pub material: u32,
}

impl PackedVertex {
    #[must_use]
    pub const fn new(x: u32, y: u32, z: u32, axis: u8, positive: bool, material: u16) -> Self {
        Self {
            packed: x | (y << 6) | (z << 12) | ((axis as u32) << 18) | ((positive as u32) << 20),
            material: material as u32,
        }
    }
}

/// Meshes a chunk that has already been expanded.
#[must_use]
pub fn mesh(grid: &SubNodeGrid) -> Mesh {
    let mut mesh = Mesh::default();
    // A plane of faces for one slice: N rows of an N-bit mask.
    let mut plane = vec![0u64; N * N];

    for (axis, positive) in FACES {
        // Face culling, a whole column at a time. This is the entire reason for
        // the bitmask representation: one shift and one AND-NOT decides 48
        // cells at once.
        let columns = &grid.columns[axis];
        plane.fill(0);

        for u in 0..N {
            for v in 0..N {
                let column = columns[u * N + v];
                let faces = if positive {
                    column & !(column >> 1)
                } else {
                    column & !(column << 1)
                };
                // Scatter the column's faces into per-slice planes.
                let mut remaining = faces >> FIRST;
                while remaining != 0 {
                    let w = remaining.trailing_zeros() as usize;
                    remaining &= remaining - 1;
                    if w < N {
                        plane[w * N + u] |= 1 << v;
                    }
                }
            }
        }

        for w in 0..N {
            greedy_merge(
                grid,
                &mut plane[w * N..(w + 1) * N],
                axis,
                positive,
                w,
                &mut mesh,
            );
        }
    }

    mesh
}

/// Merges one slice's face bitmap into as few quads as possible.
///
/// The classic greedy sweep, but the row scan is done with `trailing_zeros`
/// over the bitmask rather than by testing cells, and clearing a merged run is
/// a single AND-NOT.
fn greedy_merge(
    grid: &SubNodeGrid,
    plane: &mut [u64],
    axis: usize,
    positive: bool,
    w: usize,
    mesh: &mut Mesh,
) {
    for u in 0..N {
        let mut row = plane[u];
        while row != 0 {
            let v = row.trailing_zeros() as usize;
            let (cx, cy, cz) = SubNodeGrid::cell(axis, u, v, w);
            let material = grid.material(cx, cy, cz);

            // Extend along v while the faces are present and the material
            // matches. Merging across a material boundary would produce a quad
            // with two textures, so the material check is not optional.
            //
            // Naming matters here and got this wrong once: `span_v` is the run
            // WITHIN a row, `span_u` is how many rows that run repeats across.
            // Calling them du and dv in the order they are computed transposes
            // every quad, which leaves the quad COUNT correct — so timings and
            // buffer sizes look fine — while the geometry is silently rotated.
            // The reference-mesher test is what caught it.
            let mut span_v = 1;
            while v + span_v < N && (row >> (v + span_v)) & 1 == 1 {
                let (nx, ny, nz) = SubNodeGrid::cell(axis, u, v + span_v, w);
                if grid.material(nx, ny, nz) != material {
                    break;
                }
                span_v += 1;
            }

            let run = if span_v == 64 {
                u64::MAX
            } else {
                ((1u64 << span_v) - 1) << v
            };
            row &= !run;
            plane[u] &= !run;

            // Extend along u: a whole row of the run must be present, with
            // matching materials, or the quad stops here.
            let mut span_u = 1;
            while u + span_u < N && (plane[u + span_u] & run) == run {
                let matches = (0..span_v).all(|offset| {
                    let (nx, ny, nz) = SubNodeGrid::cell(axis, u + span_u, v + offset, w);
                    grid.material(nx, ny, nz) == material
                });
                if !matches {
                    break;
                }
                plane[u + span_u] &= !run;
                span_u += 1;
            }

            mesh.quads.push(Quad {
                axis: axis as u8,
                positive,
                w: w as u8,
                u: u as u8,
                v: v as u8,
                du: span_u as u8,
                dv: span_v as u8,
                material,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::{STONE, Scene};
    use tiamot_core::coords::SubNodePos;
    use tiamot_core::{BlockValue, ChunkPos};

    /// Every quad, expanded back to the individual faces it covers.
    pub(super) fn faces(mesh: &Mesh) -> Vec<(u8, bool, usize, usize, usize, u16)> {
        let mut out = Vec::new();
        for quad in &mesh.quads {
            for du in 0..quad.du as usize {
                for dv in 0..quad.dv as usize {
                    out.push((
                        quad.axis,
                        quad.positive,
                        quad.u as usize + du,
                        quad.v as usize + dv,
                        quad.w as usize,
                        quad.material,
                    ));
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// The obvious mesher: test every cell against every neighbour.
    pub(super) fn reference_faces(grid: &SubNodeGrid) -> Vec<(u8, bool, usize, usize, usize, u16)> {
        let mut out = Vec::new();
        for (axis, positive) in FACES {
            for u in 0..N {
                for v in 0..N {
                    for w in 0..N {
                        let (x, y, z) = SubNodeGrid::cell(axis, u, v, w);
                        let material = grid.material(x, y, z);
                        if material == 0 {
                            continue;
                        }
                        // The neighbour one step along the axis.
                        let step: isize = if positive { 1 } else { -1 };
                        let nw = w as isize + step;
                        let occupied = if (0..N as isize).contains(&nw) {
                            let (nx, ny, nz) = SubNodeGrid::cell(axis, u, v, nw as usize);
                            grid.material(nx, ny, nz) != 0
                        } else {
                            false
                        };
                        if !occupied {
                            out.push((axis as u8, positive, u, v, w, material));
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    #[test]
    fn a_single_cell_produces_six_faces() {
        let mut chunk = tiamot_core::Chunk::air(ChunkPos::new(0, 0, 0));
        chunk
            .set_subnode(SubNodePos::new(5, 5, 5), STONE)
            .expect("in chunk");
        let mesh = mesh(&SubNodeGrid::from_chunk(&chunk));
        assert_eq!(
            mesh.quads.len(),
            6,
            "an isolated cell has six exposed faces"
        );
    }

    #[test]
    fn a_solid_chunk_meshes_to_six_full_quads() {
        // The greedy merge should collapse each 48x48 face into one quad. If it
        // does not, merging is broken and every vertex count in the spike is
        // wrong.
        let chunk = tiamot_core::Chunk::new(ChunkPos::new(0, 0, 0), STONE);
        let mesh = mesh(&SubNodeGrid::from_chunk(&chunk));
        assert_eq!(mesh.quads.len(), 6, "got {} quads", mesh.quads.len());
        for quad in &mesh.quads {
            assert_eq!((quad.du, quad.dv), (48, 48));
        }
    }

    #[test]
    fn an_empty_chunk_meshes_to_nothing() {
        let chunk = tiamot_core::Chunk::air(ChunkPos::new(0, 0, 0));
        assert_eq!(mesh(&SubNodeGrid::from_chunk(&chunk)).quads.len(), 0);
    }

    #[test]
    fn merging_never_crosses_a_material_boundary() {
        let mut chunk = tiamot_core::Chunk::new(ChunkPos::new(0, 0, 0), STONE);
        chunk.set_block_local(
            LocalBlock::new(0, 15, 0),
            BlockValue::Uniform(crate::scenes::DIRT),
        );
        let mesh = mesh(&SubNodeGrid::from_chunk(&chunk));
        for quad in &mesh.quads {
            // Every cell a quad covers must share the quad's material.
            for du in 0..quad.du as usize {
                for dv in 0..quad.dv as usize {
                    let grid = SubNodeGrid::from_chunk(&chunk);
                    let (x, y, z) = SubNodeGrid::cell(
                        quad.axis as usize,
                        quad.u as usize + du,
                        quad.v as usize + dv,
                        quad.w as usize,
                    );
                    assert_eq!(grid.material(x, y, z), quad.material);
                }
            }
        }
    }

    #[test]
    fn every_scene_meshes_the_same_faces_as_the_obvious_mesher() {
        // Correctness per Task 02 shapes: the fast mesher must emit exactly the
        // faces a naive per-cell one would, merged differently but covering the
        // same surface.
        for scene in Scene::ALL {
            let chunk = scene.build(0x5EED);
            let grid = SubNodeGrid::from_chunk(&chunk);
            let meshed = faces(&mesh(&grid));
            let expected = reference_faces(&grid);
            assert_eq!(
                meshed.len(),
                expected.len(),
                "{}: face count differs",
                scene.label()
            );
            assert_eq!(meshed, expected, "{}: faces differ", scene.label());
        }
    }

    #[test]
    fn buffers_are_consistent_with_the_counts() {
        let chunk = Scene::Realistic.build(1);
        let mesh = mesh(&SubNodeGrid::from_chunk(&chunk));
        let (vertices, indices) = mesh.to_buffers();
        assert_eq!(vertices.len(), mesh.vertex_count());
        assert_eq!(indices.len(), mesh.index_count());
        assert_eq!(size_of::<PackedVertex>(), 8);
    }
}
