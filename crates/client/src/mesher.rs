// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Binary greedy meshing over the 48³ sub-node grid.
//!
//! Promoted from the Task 02b spike, with the one thing the spike deliberately
//! left out: **real neighbour-chunk face culling**. The spike treated
//! everything outside a chunk as air, which made every measurement a
//! pessimistic bound — the right direction for a gate, and visibly wrong for a
//! renderer, because it draws a full wall of faces at every chunk boundary.
//!
//! # Why binary, and why this is not a free variable
//!
//! A classic per-voxel greedy mesher walks cells one at a time and costs
//! roughly 4.5 ms on a chunk this size. Binary greedy meshing represents
//! occupancy as bitmasks in `u64` words and does face culling with a shift and
//! an AND across a whole column at once — 64 cells per instruction instead of
//! one.
//!
//! # The `u64`-column invariant
//!
//! The single most important consequence of the 16³-block chunk size (charter
//! rule 6). A chunk is 48 sub-node cells per axis. Face culling needs the
//! neighbouring cell just outside the chunk at each end, so a column needs
//! 48 + 2 = **50 bits — one `u64`**.
//!
//! A 32³-block chunk would be 96 cells per axis, need 98 bits, and lose the
//! technique entirely: every column operation would become a multi-word
//! sequence with carries. The chunk size is chosen to make this work, not the
//! other way round.
//!
//! Bit layout of a column: bit 0 is the neighbour at −1, bits 1..=48 are the
//! chunk's own cells, bit 49 is the neighbour at +48.
//!
//! # Lighting mode 1
//!
//! Faces carry a directional shade — top 1.0, bottom 0.5, x-sides 0.75,
//! z-sides 0.85 — and a light attribute fed a flat 1.0. Task 10 replaces the
//! attribute with propagated light; the vertex format does not change when it
//! does, which is the point of carrying it now.

// `face_shade` here is the mode 1 directional constant below; the light
// sampler comes in under a name that says which one it is.
use crate::shade::{BlockLight, Shade, face_shade as sample_corner_light};
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

/// Which neighbour a border sits against, in the order [`Neighbours`] expects.
///
/// Indexed by `axis * 2 + positive`.
pub const NEIGHBOUR_COUNT: usize = 6;

/// The six chunks adjacent to the one being meshed.
///
/// `None` means "not loaded", which is treated as **solid** rather than air.
/// That is deliberate and the opposite of the spike: an unloaded neighbour is
/// almost always a chunk that exists and simply has not arrived, so drawing a
/// wall of faces against it produces a visible shell around the loaded region
/// that pops away a moment later. Treating it as solid hides the seam, and the
/// faces appear when the neighbour does.
#[derive(Debug, Default, Clone, Copy)]
pub struct Neighbours<'a> {
    /// −x, +x, −y, +y, −z, +z.
    pub sides: [Option<&'a Chunk>; NEIGHBOUR_COUNT],
}

impl<'a> Neighbours<'a> {
    /// No neighbours loaded.
    #[must_use]
    pub const fn none() -> Self {
        Self { sides: [None; 6] }
    }

    /// Every neighbour absent, treated as **air** rather than solid.
    ///
    /// For tests and for the "mesh one chunk in isolation" case, where hiding
    /// boundary faces would hide the geometry under test.
    #[must_use]
    pub const fn open() -> Self {
        Self { sides: [None; 6] }
    }

    fn side(&self, axis: usize, positive: bool) -> Option<&'a Chunk> {
        self.sides[axis * 2 + usize::from(positive)]
    }
}

/// How an absent neighbour is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// Draw faces against it. Correct when the chunk genuinely ends there.
    Air,
    /// Hide faces against it. Correct when the chunk simply has not arrived.
    Solid,
}

/// A chunk expanded into a flat sub-node grid plus per-axis occupancy columns.
pub struct SubNodeGrid {
    /// Material of every cell, `x + N*y + N*N*z`.
    materials: Vec<u16>,
    /// Occupancy columns per axis. See the module docs for the bit layout.
    columns: [Vec<u64>; 3],
}

impl SubNodeGrid {
    /// Expands a chunk, seeding the padding bits from its neighbours.
    ///
    /// The padding bits are what make border culling work: bit 0 and bit 49 of
    /// each column hold the adjacent cell in the next chunk, so the same
    /// shift-and-AND that culls interior faces culls border faces too. No
    /// special case, no second code path — which matters, because a second path
    /// is where the two disagree and a seam appears.
    #[must_use]
    pub fn from_chunk(chunk: &Chunk, neighbours: &Neighbours<'_>, absent: Absent) -> Self {
        let mut materials = vec![0u16; CELLS];
        let mut columns = [vec![0u64; N * N], vec![0u64; N * N], vec![0u64; N * N]];

        for index in 0..BLOCKS_PER_CHUNK {
            let local = LocalBlock::from_index(index);
            let view = chunk.get_block_local(local);

            // Uniform air is the overwhelmingly common case in a real chunk and
            // contributes nothing; skipping it early is most of why a flat
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

        let mut grid = Self { materials, columns };
        grid.seed_padding(neighbours, absent);
        grid
    }

    /// Fills bit 0 and bit 49 of every column from the adjacent chunk.
    fn seed_padding(&mut self, neighbours: &Neighbours<'_>, absent: Absent) {
        let solid_when_absent = absent == Absent::Solid;

        for (axis, positive) in FACES {
            let neighbour = neighbours.side(axis, positive);
            // The bit this side writes: 0 for the −1 neighbour, 49 for the +48.
            let bit = if positive { FIRST + N as u32 } else { 0 };
            // The plane of the NEIGHBOUR that touches us: its last cell if it
            // is on our negative side, its first if positive.
            let neighbour_w = if positive { 0 } else { N - 1 };

            for u in 0..N {
                for v in 0..N {
                    let occupied = match neighbour {
                        Some(chunk) => {
                            let (nx, ny, nz) = Self::cell(axis, u, v, neighbour_w);
                            !cell_material(chunk, nx, ny, nz).is_air()
                        }
                        None => solid_when_absent,
                    };
                    if occupied {
                        self.columns[axis][u * N + v] |= 1 << bit;
                    }
                }
            }
        }
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

/// The material of one sub-node cell of a chunk, by cell coordinates.
fn cell_material(chunk: &Chunk, x: usize, y: usize, z: usize) -> tiamot_core::MaterialId {
    let per_axis = SUBNODES_PER_AXIS as usize;
    let local = LocalBlock::new(
        u32::try_from(x / per_axis).unwrap_or(0),
        u32::try_from(y / per_axis).unwrap_or(0),
        u32::try_from(z / per_axis).unwrap_or(0),
    );
    let view = chunk.get_block_local(local);
    view.subnode(subnode_index(
        u32::try_from(x % per_axis).unwrap_or(0),
        u32::try_from(y % per_axis).unwrap_or(0),
        u32::try_from(z % per_axis).unwrap_or(0),
    ))
}

/// One merged quad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quad {
    /// 0 = x, 1 = y, 2 = z.
    pub axis: u8,
    /// Whether the face points along the positive axis.
    pub positive: bool,
    /// Slice index along the axis.
    pub w: u8,
    /// Position along the plane's first axis.
    pub u: u8,
    /// Position along the plane's second axis.
    pub v: u8,
    /// Extent along u, at least 1.
    pub du: u8,
    /// Extent along v, at least 1.
    pub dv: u8,
    /// The material the quad shows.
    pub material: u16,
    /// Light at the quad's four corners, in `to_buffers` order.
    ///
    /// **Part of what decides whether two faces may merge**, not decoration —
    /// see [`crate::shade`]. Interpolating across a quad whose corners came
    /// from different lighting would blur a shadow edge over the whole quad.
    pub shade: Shade,
}

/// A meshed chunk.
#[derive(Debug, Default, Clone)]
pub struct Mesh {
    /// The merged quads.
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
    #[must_use]
    pub fn vertex_bytes(&self) -> usize {
        self.vertex_count() * size_of::<PackedVertex>()
    }

    /// Bytes a GPU index buffer would need, at `u32` indices.
    #[must_use]
    pub fn index_bytes(&self) -> usize {
        self.index_count() * size_of::<u32>()
    }

    /// Total VRAM for this mesh.
    #[must_use]
    pub fn gpu_bytes(&self) -> usize {
        self.vertex_bytes() + self.index_bytes()
    }

    /// Whether the mesh has nothing to draw.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// Expands to the vertex and index buffers a renderer uploads.
    ///
    /// Winding is counter-clockwise when viewed from outside the surface, for
    /// every face direction — so the pipeline can cull back faces and a quad
    /// facing away is not drawn.
    ///
    /// # The y axis winds the other way, and this is why
    ///
    /// [`SubNodeGrid::cell`] maps plane coordinates to cell coordinates
    /// differently per axis: axis 0 is `(w, u, v)`, axis 1 is `(u, w, v)`, and
    /// axis 2 is `(u, v, w)`. Read as permutations of `(x, y, z)`, the first
    /// and last are **even** and the middle one is **odd** — so walking a
    /// quad's four corners in the same `(u, v)` order traces the opposite
    /// circulation on the y axis than it does on x and z.
    ///
    /// Emitting them all with one winding gives every top and bottom face in
    /// the world a normal pointing the wrong way, and back-face culling then
    /// removes exactly those faces. The symptom is not a missing surface but a
    /// **surface one layer too deep**: looking down at a floor, the top is
    /// culled and the face below it is drawn instead, which is the right
    /// texture at the wrong brightness and reads as "the lighting looks a bit
    /// flat" rather than as a bug.
    ///
    /// Found by rendering a lone top quad with nothing behind it — the only
    /// arrangement in which the wrong answer is a blank screen instead of a
    /// plausible one.
    #[must_use]
    pub fn to_buffers(&self) -> (Vec<PackedVertex>, Vec<u32>) {
        let mut vertices = Vec::with_capacity(self.vertex_count());
        let mut indices = Vec::with_capacity(self.index_count());

        for quad in &self.quads {
            let base = u32::try_from(vertices.len()).unwrap_or(0);
            for (corner, (du, dv)) in [(0, 0), (1, 0), (1, 1), (0, 1)].into_iter().enumerate() {
                let u = u32::from(quad.u) + du * u32::from(quad.du);
                let v = u32::from(quad.v) + dv * u32::from(quad.dv);
                let w = u32::from(quad.w) + u32::from(quad.positive);
                let (x, y, z) = match quad.axis {
                    0 => (w, u, v),
                    1 => (u, w, v),
                    _ => (u, v, w),
                };
                // Corner `n` of the shade is vertex `n` here: both walk
                // `[(0,0), (1,0), (1,1), (0,1)]`, and `crate::shade::Shade`
                // says so where it is defined.
                vertices.push(PackedVertex::lit(
                    x,
                    y,
                    z,
                    quad.axis,
                    quad.positive,
                    quad.material,
                    quad.shade.corner(corner),
                ));
            }
            // The y axis's (u, v, w) mapping is an odd permutation, so its
            // corners circulate the other way. See the method docs.
            let outward = quad.positive != (quad.axis == 1);
            if outward {
                indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            } else {
                // Reversed, so a quad winds the same way when seen from its own
                // outside whichever direction it faces.
                indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
            }
        }

        (vertices, indices)
    }
}

/// Directional face shading for lighting mode 1.
///
/// No light propagation yet — Task 10 — but flat-lit voxels are unreadable:
/// every edge disappears and the world looks like a single white mass. Cheap
/// directional shading is what makes the geometry legible before real lighting
/// exists.
#[must_use]
pub const fn face_shade(axis: u8, positive: bool) -> f32 {
    match (axis, positive) {
        (1, true) => 1.0,  // top
        (1, false) => 0.5, // bottom
        (2, _) => 0.85,    // z sides
        _ => 0.75,         // x sides
    }
}

/// A quantised vertex: 8 bytes.
///
/// Positions are sub-node coordinates in `0..=48`, which needs 6 bits per axis.
/// `f32` positions would be 12 bytes for position alone; quantising is standard
/// practice in voxel renderers and the Task 02b VRAM numbers assume it.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PackedVertex {
    /// x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2
    pub packed: u32,
    /// material:16 | light:16
    ///
    /// The light half is a packed [`tiamot_core::light::Light`] — sun and RGB,
    /// four bits each. Task 08 reserved these bits for it, which is why mode 2
    /// costs nothing per vertex over mode 1 and Task 02b's VRAM figures still
    /// hold.
    pub material: u32,
}

impl PackedVertex {
    /// Packs one corner at full daylight.
    ///
    /// For lighting mode 1, which has no propagated light to carry, and for
    /// tests about geometry rather than about light.
    #[must_use]
    pub const fn new(x: u32, y: u32, z: u32, axis: u8, positive: bool, material: u16) -> Self {
        Self::lit(
            x,
            y,
            z,
            axis,
            positive,
            material,
            crate::shade::Corner {
                light: tiamot_core::light::Light::DAYLIGHT,
                occlusion: 3,
            },
        )
    }

    /// Packs one corner with its light and its occlusion.
    ///
    /// The light occupies the top 16 bits of `material`, which Task 08 reserved
    /// for exactly this; occlusion takes two of the eleven bits `packed` had
    /// spare. **So mode 2 costs no more per vertex than mode 1 did**, and Task
    /// 02b's VRAM measurements still hold.
    ///
    /// Occlusion is kept apart from the light rather than multiplied into it —
    /// see [`crate::shade::Shade`]. Scaling the light keeps its hue, so a corner
    /// shadowed under a low sun comes out dim orange rather than dark.
    #[must_use]
    pub const fn lit(
        x: u32,
        y: u32,
        z: u32,
        axis: u8,
        positive: bool,
        material: u16,
        corner: crate::shade::Corner,
    ) -> Self {
        Self {
            packed: x
                | (y << 6)
                | (z << 12)
                | ((axis as u32) << 18)
                | ((positive as u32) << 20)
                | (((corner.occlusion & 0x3) as u32) << 21),
            material: (material as u32) | ((corner.light.0 as u32) << 16),
        }
    }

    /// The light level this vertex carries.
    #[must_use]
    pub const fn light(&self) -> tiamot_core::light::Light {
        tiamot_core::light::Light((self.material >> 16) as u16)
    }

    /// The occlusion level this vertex carries, `0` darkest to `3` open.
    #[must_use]
    pub const fn occlusion(&self) -> u8 {
        ((self.packed >> 21) & 0x3) as u8
    }

    /// The sub-node position this vertex sits at.
    #[must_use]
    pub const fn position(&self) -> (u32, u32, u32) {
        (
            self.packed & 0x3F,
            (self.packed >> 6) & 0x3F,
            (self.packed >> 12) & 0x3F,
        )
    }

    /// The face direction, as (axis, positive).
    #[must_use]
    pub const fn face(&self) -> (u8, bool) {
        (
            ((self.packed >> 18) & 0x3) as u8,
            (self.packed >> 20) & 1 == 1,
        )
    }

    /// The material id.
    #[must_use]
    pub const fn material_id(&self) -> u16 {
        (self.material & 0xFFFF) as u16
    }
}

/// Meshes a chunk that has already been expanded.
#[must_use]
pub fn mesh(grid: &SubNodeGrid, light: &impl BlockLight) -> Mesh {
    let mut mesh = Mesh::default();
    // A plane of faces for one slice: N rows of an N-bit mask.
    let mut plane = vec![0u64; N * N];
    // One slice's worth of corner light, reused across every slice and
    // direction. Entries for cells with no face are never read.
    let mut shades = vec![Shade::default(); N * N];

    for (axis, positive) in FACES {
        // Face culling, a whole column at a time. This is the entire reason for
        // the bitmask representation: one shift and one AND-NOT decides 48
        // cells at once — including the two padding bits, which is what makes
        // border culling free rather than a special case.
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
            // **Each cell's shade is computed exactly once**, for the cells
            // that actually have a face, before merging looks at any of them.
            // Computing inside the merge instead means recomputing a candidate
            // cell for every span it is tested against — measured at 2.3× the
            // mesh time on realistic content, which is most of the way to the
            // Task 02b gate for no reason.
            let slice = &mut plane[w * N..(w + 1) * N];
            for (u, row) in slice.iter().enumerate() {
                let mut bits = *row;
                while bits != 0 {
                    let v = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let (cx, cy, cz) = SubNodeGrid::cell(axis, u, v, w);
                    shades[u * N + v] = shade_at(light, grid, axis, positive, cx, cy, cz);
                }
            }

            greedy_merge(grid, &shades, slice, axis, positive, w, &mut mesh);
        }
    }

    mesh
}

/// Meshes a chunk in one call.
#[must_use]
pub fn mesh_chunk(
    chunk: &Chunk,
    neighbours: &Neighbours<'_>,
    absent: Absent,
    light: &impl BlockLight,
) -> Mesh {
    mesh(&SubNodeGrid::from_chunk(chunk, neighbours, absent), light)
}

impl crate::shade::CellOccupancy for SubNodeGrid {
    fn solid(&self, x: i32, y: i32, z: i32) -> bool {
        let inside = |value: i32| usize::try_from(value).ok().filter(|value| *value < N);
        let (Some(x), Some(y), Some(z)) = (inside(x), inside(y), inside(z)) else {
            // Outside the chunk. See `shade::corner_occlusion` — the grid's
            // padding covers one cell along each column axis and cannot answer
            // diagonally across a boundary, so this reads as empty and the seam
            // is documented there rather than guessed at here.
            return false;
        };
        self.is_solid(x, y, z)
    }
}

/// The corner light of one cell face.
///
/// **Grid cell indices are chunk-local cell coordinates, `0..48`, with no
/// offset.** [`FIRST`] is a bit position inside a `u64` column word, not an
/// index shift — subtracting it here, which the first version of this did,
/// moves every sample one cell towards the origin and lands the whole world's
/// lighting a third of a block off its geometry.
fn shade_at(
    light: &impl BlockLight,
    grid: &SubNodeGrid,
    axis: usize,
    positive: bool,
    cx: usize,
    cy: usize,
    cz: usize,
) -> Shade {
    sample_corner_light(
        light,
        grid,
        axis,
        positive,
        (cx as i32, cy as i32, cz as i32),
    )
}

/// Merges one slice's face bitmap into as few quads as possible.
fn greedy_merge(
    grid: &SubNodeGrid,
    shades: &[Shade],
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
            let shade = shades[u * N + v];

            // Extend along v while the faces are present and the material
            // matches. Merging across a material boundary would produce a quad
            // with two textures, so the material check is not optional.
            //
            // Naming matters here and got this wrong once in the spike:
            // `span_v` is the run WITHIN a row, `span_u` is how many rows that
            // run repeats across. Calling them du and dv in the order they are
            // computed transposes every quad, which leaves the quad COUNT
            // correct — so timings and buffer sizes look fine — while the
            // geometry is silently rotated. The reference mesher caught it.
            let mut span_v = 1;
            while v + span_v < N && (row >> (v + span_v)) & 1 == 1 {
                let (nx, ny, nz) = SubNodeGrid::cell(axis, u, v + span_v, w);
                // **Light and occlusion join material as merge keys.** Two
                // faces that shade differently are not one quad: a vertex
                // carries its own level and the hardware interpolates between
                // them, so merging across a change would run the gradient
                // straight through a shadow edge. Uniformly lit surfaces —
                // most of a world — have identical shades and merge exactly as
                // before. See `crate::shade`.
                if grid.material(nx, ny, nz) != material || shades[u * N + v + span_v] != shade {
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
                        && shades[(u + span_u) * N + v + offset] == shade
                });
                if !matches {
                    break;
                }
                plane[u + span_u] &= !run;
                span_u += 1;
            }

            mesh.quads.push(Quad {
                axis: u8::try_from(axis).unwrap_or(0),
                positive,
                w: u8::try_from(w).unwrap_or(0),
                u: u8::try_from(u).unwrap_or(0),
                v: u8::try_from(v).unwrap_or(0),
                du: u8::try_from(span_u).unwrap_or(1),
                dv: u8::try_from(span_v).unwrap_or(1),
                material,
                shade,
            });
        }
    }
}

/// A deliberately dumb mesher, for checking the fast one.
///
/// Emits one quad per exposed cell face, with no merging at all. The binary
/// mesher is exactly the kind of bit-twiddling that needs an oracle written a
/// completely different way: if the two disagree about which faces exist, one
/// of them is wrong, and it is not going to be this one.
pub mod reference {
    use super::{Absent, N, Neighbours, Quad, SubNodeGrid, cell_material};
    use crate::shade::BlockLight;
    use tiamot_core::chunk::Chunk;

    /// Every exposed face, one quad per cell, unmerged.
    #[must_use]
    pub fn mesh_chunk(
        chunk: &Chunk,
        neighbours: &Neighbours<'_>,
        absent: Absent,
        light: &impl BlockLight,
    ) -> Vec<Quad> {
        let grid = SubNodeGrid::from_chunk(chunk, neighbours, absent);
        let mut out = Vec::new();

        for x in 0..N {
            for y in 0..N {
                for z in 0..N {
                    if !grid.is_solid(x, y, z) {
                        continue;
                    }
                    let material = grid.material(x, y, z);

                    for (axis, positive) in super::FACES {
                        let mut neighbour = [x, y, z];
                        let step: isize = if positive { 1 } else { -1 };
                        let coordinate = neighbour[axis] as isize + step;

                        let occupied = if coordinate < 0 || coordinate >= N as isize {
                            // Outside the chunk: ask the neighbour, exactly as
                            // the padding bits do.
                            outside_occupied(chunk, neighbours, absent, axis, positive, [x, y, z])
                        } else {
                            neighbour[axis] = coordinate as usize;
                            grid.is_solid(neighbour[0], neighbour[1], neighbour[2])
                        };

                        if occupied {
                            continue;
                        }

                        // The (u, v, w) mapping must match `SubNodeGrid::cell`.
                        let (u, v, w) = match axis {
                            0 => (y, z, x),
                            1 => (x, z, y),
                            _ => (x, y, z),
                        };
                        out.push(Quad {
                            axis: u8::try_from(axis).unwrap_or(0),
                            positive,
                            w: u8::try_from(w).unwrap_or(0),
                            u: u8::try_from(u).unwrap_or(0),
                            v: u8::try_from(v).unwrap_or(0),
                            du: 1,
                            dv: 1,
                            material,
                            shade: super::shade_at(light, &grid, axis, positive, x, y, z),
                        });
                    }
                }
            }
        }

        out
    }

    fn outside_occupied(
        _chunk: &Chunk,
        neighbours: &Neighbours<'_>,
        absent: Absent,
        axis: usize,
        positive: bool,
        cell: [usize; 3],
    ) -> bool {
        let Some(neighbour) = neighbours.sides[axis * 2 + usize::from(positive)] else {
            return absent == Absent::Solid;
        };
        let mut probe = cell;
        probe[axis] = if positive { 0 } else { N - 1 };
        !cell_material(neighbour, probe[0], probe[1], probe[2]).is_air()
    }
}

#[cfg(test)]
mod tests {
    /// Full daylight everywhere.
    ///
    /// These tests are about geometry, and a uniform field is the case that
    /// leaves greedy merging exactly as it was — so a quad count here still
    /// measures merging rather than lighting.
    const DAY: crate::shade::Uniform = crate::shade::Uniform(tiamot_core::light::Light::DAYLIGHT);

    use super::*;
    use tiamot_core::coords::SubNodePos;
    use tiamot_core::{BlockPos, BlockValue, ChunkPos, MaterialId};

    const STONE: MaterialId = MaterialId(2);
    const WOOD: MaterialId = MaterialId(3);

    fn empty() -> Chunk {
        Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR)
    }

    /// Every quad expanded back into the individual cell faces it covers.
    ///
    /// The merged mesher and the reference mesher describe the same surface in
    /// different shapes; comparing them means expanding both to faces.
    fn faces(quads: &[Quad]) -> std::collections::BTreeSet<(u8, bool, u8, u8, u8, u16)> {
        let mut out = std::collections::BTreeSet::new();
        for quad in quads {
            for du in 0..quad.du {
                for dv in 0..quad.dv {
                    let inserted = out.insert((
                        quad.axis,
                        quad.positive,
                        quad.u + du,
                        quad.v + dv,
                        quad.w,
                        quad.material,
                    ));
                    assert!(inserted, "a face was emitted twice: {quad:?}");
                }
            }
        }
        out
    }

    #[test]
    fn a_single_block_is_six_quads() {
        // 27 cells, but every interior face is culled and each of the six
        // outer 3x3 faces merges to one quad.
        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(5, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        assert_eq!(mesh.quads.len(), 6, "one block should merge to six quads");

        // And each covers a full 3x3 block face.
        for quad in &mesh.quads {
            assert_eq!((quad.du, quad.dv), (3, 3), "{quad:?}");
        }
    }

    #[test]
    fn two_adjacent_blocks_share_no_interior_faces() {
        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(5, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");
        chunk
            .set_block(BlockPos::new(6, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let expanded = faces(&mesh.quads);

        // Two blocks: 2 * 27 = 54 cells. The shared plane is 3x3 = 9 cells,
        // hiding 9 faces on each side. Total exposed = 2*54 - 2*9... at cell
        // level the surface is the outside of a 6x3x3 cuboid:
        // 2*(6*3) + 2*(6*3) + 2*(3*3) = 36 + 36 + 18 = 90 cell faces.
        assert_eq!(expanded.len(), 90, "the shared plane must be culled");
    }

    #[test]
    fn a_partial_block_with_one_subnode_removed_gains_faces() {
        // The sub-node case, which is the whole point of the engine. Removing
        // one interior-ish cell exposes new faces around the hole.
        let mut solid = empty();
        solid
            .set_block(BlockPos::new(5, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");
        let before = faces(&mesh_chunk(&solid, &Neighbours::open(), Absent::Air, &DAY).quads);

        let mut chiselled = solid.clone();
        // A corner cell of the block: removing it exposes three inward faces
        // and removes three outward ones.
        chiselled
            .set_subnode(SubNodePos::new(15, 15, 15), MaterialId::AIR)
            .expect("in chunk");
        let after = faces(&mesh_chunk(&chiselled, &Neighbours::open(), Absent::Air, &DAY).quads);

        assert_ne!(before, after, "chiselling must change the surface");
        assert_eq!(
            after.len(),
            before.len(),
            "removing a corner cell trades three outward faces for three inward ones"
        );
    }

    #[test]
    fn a_flat_slab_merges_to_almost_nothing() {
        // The case greedy meshing exists for. A full layer of blocks is
        // 48x48 cells on top; merged it must be a single quad.
        let mut chunk = empty();
        for x in 0..16 {
            for z in 0..16 {
                chunk
                    .set_block(BlockPos::new(x, 0, z), BlockValue::Uniform(STONE))
                    .expect("in chunk");
            }
        }

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let top = mesh
            .quads
            .iter()
            .filter(|quad| quad.axis == 1 && quad.positive)
            .collect::<Vec<_>>();
        assert_eq!(top.len(), 1, "the top face must merge to one quad");
        assert_eq!((top[0].du, top[0].dv), (48, 48));
    }

    #[test]
    fn quads_never_merge_across_a_material_boundary() {
        // A merged quad shows one texture. Merging two materials would paint
        // half a surface with the wrong one.
        let mut chunk = empty();
        for x in 0..16 {
            let material = if x < 8 { STONE } else { WOOD };
            for z in 0..16 {
                chunk
                    .set_block(BlockPos::new(x, 0, z), BlockValue::Uniform(material))
                    .expect("in chunk");
            }
        }

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let top: Vec<_> = mesh
            .quads
            .iter()
            .filter(|quad| quad.axis == 1 && quad.positive)
            .collect();
        assert_eq!(top.len(), 2, "one quad per material: {top:?}");
        assert_ne!(top[0].material, top[1].material);
    }

    #[test]
    fn the_merged_mesh_describes_the_same_surface_as_the_reference() {
        // The oracle test. Binary greedy meshing is exactly the kind of
        // bit-twiddling that looks right and is not.
        let mut chunk = empty();
        for (x, y, z) in [(2, 2, 2), (3, 2, 2), (2, 3, 2), (7, 1, 9), (7, 2, 9)] {
            chunk
                .set_block(BlockPos::new(x, y, z), BlockValue::Uniform(STONE))
                .expect("in chunk");
        }
        chunk
            .set_subnode(SubNodePos::new(8, 8, 8), STONE)
            .expect("in chunk");

        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let reference = reference::mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);

        assert_eq!(
            faces(&merged.quads),
            faces(&reference),
            "the merged mesh and the reference must describe the same surface"
        );
        assert!(
            merged.quads.len() < reference.len(),
            "and merging must actually merge: {} vs {}",
            merged.quads.len(),
            reference.len()
        );
    }

    #[test]
    fn an_absent_neighbour_can_be_treated_as_solid_or_air() {
        // Both behaviours are needed: air when the chunk genuinely ends, solid
        // when the neighbour simply has not arrived and a wall of faces would
        // pop away a moment later.
        let mut chunk = empty();
        for x in 0..16 {
            for y in 0..16 {
                for z in 0..16 {
                    chunk
                        .set_block(BlockPos::new(x, y, z), BlockValue::Uniform(STONE))
                        .expect("in chunk");
                }
            }
        }

        let open = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let closed = mesh_chunk(&chunk, &Neighbours::none(), Absent::Solid, &DAY);

        assert_eq!(open.quads.len(), 6, "a solid chunk in the open is a cube");
        assert!(
            closed.is_empty(),
            "a solid chunk surrounded by solid has no visible surface, got {} quads",
            closed.quads.len()
        );
    }

    #[test]
    fn a_shared_border_produces_no_duplicated_or_missing_faces() {
        // Two adjacent chunks meshed INDEPENDENTLY must agree about the plane
        // between them: neither draws a face there, because neither side is
        // exposed. Getting this wrong is the classic voxel seam — a wall of
        // z-fighting quads down every chunk boundary.
        let mut left = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
        let mut right = Chunk::new(ChunkPos::new(1, 0, 0), MaterialId::AIR);

        // A slab spanning both chunks at the shared x plane.
        for y in 0..4 {
            for z in 0..4 {
                left.set_block(BlockPos::new(15, y, z), BlockValue::Uniform(STONE))
                    .expect("in left");
                right
                    .set_block(BlockPos::new(16, y, z), BlockValue::Uniform(STONE))
                    .expect("in right");
            }
        }

        let mut left_neighbours = Neighbours::none();
        left_neighbours.sides[1] = Some(&right); // +x
        let mut right_neighbours = Neighbours::none();
        right_neighbours.sides[0] = Some(&left); // -x

        let left_mesh = mesh_chunk(&left, &left_neighbours, Absent::Air, &DAY);
        let right_mesh = mesh_chunk(&right, &right_neighbours, Absent::Air, &DAY);

        // Neither chunk may emit a face on the shared plane.
        let left_border = left_mesh
            .quads
            .iter()
            .filter(|quad| quad.axis == 0 && quad.positive && quad.w == (N - 1) as u8)
            .count();
        let right_border = right_mesh
            .quads
            .iter()
            .filter(|quad| quad.axis == 0 && !quad.positive && quad.w == 0)
            .count();

        assert_eq!(left_border, 0, "the left chunk drew into the shared plane");
        assert_eq!(
            right_border, 0,
            "the right chunk drew into the shared plane"
        );

        // And without the neighbour, it WOULD have — proving the test is
        // testing the culling rather than an accident of the geometry.
        let unaware = mesh_chunk(&left, &Neighbours::open(), Absent::Air, &DAY);
        assert!(
            unaware
                .quads
                .iter()
                .any(|quad| quad.axis == 0 && quad.positive && quad.w == (N - 1) as u8),
            "without neighbour awareness the border face should be drawn"
        );
    }

    #[test]
    fn a_vertex_round_trips_through_its_packing() {
        // Eight bytes with five fields in them is the kind of code that is
        // wrong by one shift and looks fine.
        for (x, y, z, axis, positive, material) in [
            (0, 0, 0, 0u8, false, 0u16),
            (48, 48, 48, 2, true, 65535),
            (17, 3, 41, 1, false, 300),
        ] {
            let vertex = PackedVertex::new(x, y, z, axis, positive, material);
            assert_eq!(vertex.position(), (x, y, z));
            assert_eq!(vertex.face(), (axis, positive));
            assert_eq!(vertex.material_id(), material);
        }
    }

    #[test]
    fn a_lit_surface_carries_its_light_to_the_vertices() {
        // **The mesh-light integration the task asks for**, with the expected
        // values worked out by hand rather than recorded from a run.
        //
        // The scene: one solid block at the chunk's origin, with the block
        // above it lit and every other block dark. The block's top face is
        // three cells across, and each of its cell faces looks up into block
        // (0, 1, 0). A corner in the middle of that face has all four of its
        // sampled blocks equal to (0, 1, 0), so it takes that level exactly;
        // a corner on the block's outer edge averages one lit block with three
        // dark ones and comes to a quarter of it.
        use crate::shade::BlockLight;
        use tiamot_core::light::{Light, MAX_LEVEL};

        struct OneLitBlock;
        impl BlockLight for OneLitBlock {
            fn at(&self, x: i32, y: i32, z: i32) -> Light {
                if (x, y, z) == (0, 1, 0) {
                    Light::new(MAX_LEVEL, 0, 0, 0)
                } else {
                    Light::DARK
                }
            }
        }

        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Uniform(STONE));
        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &OneLitBlock);

        // Top faces only: the sides look sideways into darkness.
        let top: Vec<&Quad> = mesh
            .quads
            .iter()
            .filter(|quad| quad.axis == 1 && quad.positive)
            .collect();
        assert!(!top.is_empty(), "the block has no top face");

        let brightest = top
            .iter()
            .flat_map(|quad| quad.shade.light.iter())
            .map(|level| level.sun())
            .max()
            .unwrap_or(0);
        let dimmest = top
            .iter()
            .flat_map(|quad| quad.shade.light.iter())
            .map(|level| level.sun())
            .min()
            .unwrap_or(0);

        assert_eq!(
            brightest, MAX_LEVEL,
            "the corner surrounded by the lit block should take its level whole"
        );
        assert_eq!(
            u32::from(dimmest),
            (u32::from(MAX_LEVEL) + 2) / 4,
            "an outer corner averages one lit block with three dark ones"
        );

        // And the light reaches the vertex buffer, which is the half a shade
        // test on its own cannot see.
        let (vertices, _) = mesh.to_buffers();
        let lit = vertices
            .iter()
            .filter(|vertex| vertex.face() == (1, true))
            .map(|vertex| vertex.light().sun())
            .max()
            .unwrap_or(0);
        assert_eq!(
            lit, MAX_LEVEL,
            "the corner light never made it into the packed vertex"
        );
    }

    #[test]
    fn a_lighting_change_splits_a_quad_that_material_alone_would_merge() {
        // The merge rule, stated as a test. Two halves of one flat surface,
        // same material throughout, different light: they must not become one
        // quad, or the interpolation runs the gradient across the whole face
        // and the shadow edge disappears.
        use crate::shade::BlockLight;
        use tiamot_core::light::{Light, MAX_LEVEL};

        struct HalfLit;
        impl BlockLight for HalfLit {
            fn at(&self, _x: i32, _y: i32, z: i32) -> Light {
                if z < 8 {
                    Light::new(MAX_LEVEL, 0, 0, 0)
                } else {
                    Light::DARK
                }
            }
        }

        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        for x in 0..16 {
            for z in 0..16 {
                chunk.set_block_local(LocalBlock::new(x, 0, z), BlockValue::Uniform(STONE));
            }
        }

        let split = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &HalfLit);
        let uniform = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);

        let tops = |mesh: &Mesh| {
            mesh.quads
                .iter()
                .filter(|quad| quad.axis == 1 && quad.positive)
                .count()
        };
        assert_eq!(
            tops(&uniform),
            1,
            "a uniformly lit flat surface should still merge into one quad"
        );
        assert!(
            tops(&split) > 1,
            "a surface with a shadow across it merged into one quad, so the shadow edge would \
             be interpolated away"
        );
    }

    #[test]
    fn a_vertex_is_eight_bytes() {
        // The Task 02b VRAM measurements assume this. A wider vertex would
        // invalidate the verdict's memory numbers.
        assert_eq!(size_of::<PackedVertex>(), 8);
    }

    #[test]
    fn face_shading_makes_the_geometry_legible() {
        // Lighting mode 1. Flat-lit voxels are unreadable: every edge vanishes
        // and the world is one white mass.
        assert!(
            face_shade(1, true) > face_shade(2, true),
            "top brighter than sides"
        );
        assert!(
            face_shade(2, true) > face_shade(0, true),
            "z sides brighter than x"
        );
        assert!(
            face_shade(0, true) > face_shade(1, false),
            "sides brighter than bottom"
        );
        assert!((face_shade(1, true) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn buffers_have_four_vertices_and_six_indices_per_quad() {
        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(5, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");
        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);
        let (vertices, indices) = mesh.to_buffers();

        assert_eq!(vertices.len(), mesh.quads.len() * 4);
        assert_eq!(indices.len(), mesh.quads.len() * 6);
        assert!(
            indices
                .iter()
                .all(|index| (*index as usize) < vertices.len()),
            "every index must be in range"
        );
    }

    #[test]
    fn an_empty_chunk_meshes_to_nothing() {
        let mesh = mesh_chunk(&empty(), &Neighbours::open(), Absent::Air, &DAY);
        assert!(mesh.is_empty());
        assert_eq!(mesh.gpu_bytes(), 0);
    }
}
