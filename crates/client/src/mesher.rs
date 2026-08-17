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
    /// Occupancy columns per axis — terrain AND fluid. See the module docs for
    /// the bit layout.
    columns: [Vec<u64>; 3],
    /// The fluid half of that occupancy, or `None` for a chunk with no fluid.
    ///
    /// # Why the two have to be separable
    ///
    /// Face culling is "occupied next to not-occupied", so as long as milk and
    /// stone shared one occupancy set, **the stone face behind a pond did not
    /// exist**. From outside that is invisible — opaque milk covers it — and
    /// from inside the milk it is a hole straight through the world. Reported
    /// from the window as "when under water I just see through the world".
    ///
    /// So terrain is culled against terrain alone, and fluid against
    /// everything: the milk's face where it meets stone is the one that goes,
    /// because the stone's is the one you can end up looking at.
    ///
    /// `None` rather than zeroed columns for a dry chunk, which is nearly all
    /// of them: it skips both the allocation and the second mask per column,
    /// so a world with no milk in it meshes exactly as it did.
    fluid: Option<[Vec<u64>; 3]>,
    /// Each block's fluid surface height, in sixteenths of a cell, `0` for dry.
    ///
    /// **This is what makes the surface smooth rather than a staircase.** The
    /// occupancy above is on the lattice and can only ever be 1, 2 or 3 cells
    /// deep; the real surface of a level-3 pool is 1.0 cells and of a level-5 is
    /// 1.67, and those are not lattice positions. So the lattice carries the
    /// occupancy — which is all face culling needs — and this carries where the
    /// top of the milk actually is, to a sixteenth of a cell.
    ///
    /// A block is three cells, so the range is 0..=48 and it fits a byte.
    /// One byte per block over the PADDED region — the chunk plus one block of
    /// each neighbour, see [`PADDED_BLOCKS`] — and allocated only for a chunk
    /// that has fluid in it.
    heights: Option<Vec<u8>>,
}

/// Blocks per axis in [`SubNodeGrid::heights`]: the chunk and a one-block shell.
///
/// The shell is what makes a pond agree with itself across a chunk seam. Corner
/// heights average the four blocks meeting at a vertex, so a chunk that cannot
/// see past its own edge averages two blocks where its neighbour averages the
/// other two, and the two surfaces meet at different heights — a step down the
/// line of every seam. One block of overlap and both sides average the same
/// four.
const PADDED_BLOCKS: usize = tiamot_core::CHUNK_BLOCKS as usize + 2;

/// Index into [`SubNodeGrid::heights`], for block coordinates in `-1..=16`.
const fn height_index(bx: i32, by: i32, bz: i32) -> Option<usize> {
    let last = tiamot_core::CHUNK_BLOCKS as i32;
    if bx < -1 || by < -1 || bz < -1 || bx > last || by > last || bz > last {
        return None;
    }
    let span = PADDED_BLOCKS as i32;
    Some(((bx + 1) + span * (by + 1) + span * span * (bz + 1)) as usize)
}

/// Fine units per cell in [`SubNodeGrid::heights`] and a fluid vertex's drop.
///
/// Sixteen: a sixteenth of a cell is a 48th of a block, finer than the seven
/// levels a fluid can actually take, so the quantisation here is never the
/// thing a player sees.
const FINE: u32 = 16;

/// The tallest a block's fluid surface can be, in [`FINE`] units. Three cells.
const FULL_BLOCK: u32 = FINE * SUBNODES_PER_AXIS;

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
        Self::from_chunk_with_fluid(chunk, neighbours, absent, &NoFluid)
    }

    /// The same, with fluid filled in.
    ///
    /// # Why fluid needs no render path of its own
    ///
    /// A fluid block's surface height is already ON the sub-node lattice: a
    /// level of `n` fills `n × 24 / 7` of a block's 27 cells, which is always a
    /// whole number of cells. So milk can be laid into the same grid as
    /// terrain, as a slab of the fluid's material occupying the bottom of the
    /// block — and greedy meshing, corner lighting, face culling and the atlas
    /// all work on it unchanged.
    ///
    /// The alternative was a second pass with its own buffers, its own
    /// lighting, and its own merge rules, which is a great deal of machinery to
    /// draw a box.
    ///
    /// **Only in blocks that are empty**, which is Sub-Node Contract §4 and not
    /// an optimisation: a block that holds terrain does not hold fluid, so the
    /// two can never contend for the same cell.
    pub fn from_chunk_with_fluid(
        chunk: &Chunk,
        neighbours: &Neighbours<'_>,
        absent: Absent,
        fluid: &impl FluidFill,
    ) -> Self {
        let mut materials = vec![0u16; CELLS];
        let mut columns = [vec![0u64; N * N], vec![0u64; N * N], vec![0u64; N * N]];
        let mut wet: Option<[Vec<u64>; 3]> = None;
        let mut heights: Option<Vec<u8>> = None;

        for index in 0..BLOCKS_PER_CHUNK {
            let local = LocalBlock::from_index(index);
            let view = chunk.get_block_local(local);

            // Uniform air is the overwhelmingly common case in a real chunk and
            // contributes nothing; skipping it early is most of why a flat
            // scene is fast.
            if view.is_air() {
                // Empty of terrain, so it may hold fluid.
                if let Some((material, depth)) =
                    fluid.fill(local.x as i32, local.y as i32, local.z as i32)
                {
                    fill_fluid(
                        &mut materials,
                        &mut columns,
                        &mut wet,
                        &mut heights,
                        local,
                        material,
                        depth,
                    );
                }
                continue;
            }
            // **And a block with SOME terrain in it may hold fluid too**, which
            // is Contract §4's threshold rather than an extra case: under it a
            // block is more air than anything and the fluid runs through, so
            // milk on a sub-node-smoothed slope sits INSIDE the thinning ground
            // rather than floating on top of it. The fluid is laid down after
            // the terrain below, into whatever cells the terrain left.
            let flooded = fluid.fill(local.x as i32, local.y as i32, local.z as i32);

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

            if let Some((material, depth)) = flooded {
                fill_fluid(
                    &mut materials,
                    &mut columns,
                    &mut wet,
                    &mut heights,
                    local,
                    material,
                    depth,
                );
            }
        }

        // **The shell of neighbouring blocks, for their heights alone.**
        //
        // Only when this chunk has milk of its own: the shell exists to make
        // corner averaging and face culling agree with the chunk next door, and
        // a chunk that draws no fluid has nothing to agree about. That keeps the
        // extra `fill` calls off every dry chunk in the world, which is nearly
        // all of them.
        if heights.is_some() {
            let last = tiamot_core::CHUNK_BLOCKS as i32;
            for bz in -1..=last {
                for by in -1..=last {
                    for bx in -1..=last {
                        let inside = |value: i32| (0..last).contains(&value);
                        if inside(bx) && inside(by) && inside(bz) {
                            continue;
                        }
                        let Some((_, depth)) = fluid.fill(bx, by, bz) else {
                            continue;
                        };
                        let height = (u32::from(depth) * FULL_BLOCK / tiamot_core::UNITS_PER_BLOCK)
                            .min(FULL_BLOCK);
                        if let (Some(heights), Some(index)) =
                            (heights.as_mut(), height_index(bx, by, bz))
                        {
                            heights[index] = u8::try_from(height).unwrap_or(u8::MAX).max(1);
                        }
                    }
                }
            }
        }

        let mut grid = Self {
            materials,
            columns,
            fluid: wet,
            heights,
        };
        grid.seed_padding(neighbours, absent);
        grid
    }

    /// Fills bit 0 and bit 49 of every column from the adjacent chunk.
    fn seed_padding(&mut self, neighbours: &Neighbours<'_>, absent: Absent) {
        let solid_when_absent = absent == Absent::Solid;
        let per_axis = SUBNODES_PER_AXIS as usize;
        let last = tiamot_core::CHUNK_BLOCKS as i32;

        for (axis, positive) in FACES {
            let neighbour = neighbours.side(axis, positive);
            // The bit this side writes: 0 for the −1 neighbour, 49 for the +48.
            let bit = if positive { FIRST + N as u32 } else { 0 };
            // The plane of the NEIGHBOUR that touches us: its last cell if it
            // is on our negative side, its first if positive.
            let neighbour_w = if positive { 0 } else { N - 1 };
            // And the neighbouring BLOCK, which is where its milk is recorded.
            let neighbour_block = if positive { last } else { -1 };

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
                        continue;
                    }

                    // **And the fluid's half of the same padding.**
                    //
                    // A wet block fills every cell terrain left it, so a
                    // neighbouring block that holds milk holds it in the cell
                    // against this face — and the face between the two is
                    // interior to one body of milk, exactly as it would be
                    // inside a chunk. Seeded into BOTH sets: `columns` is what
                    // the fluid culls against, and `wet` is what terrain
                    // subtracts, so the stone the milk covers keeps its face.
                    //
                    // Only where the neighbour's terrain did not already claim
                    // the cell, since terrain wins it in `fill_fluid`.
                    let (bx, by, bz) = block_cell(
                        axis,
                        (u / per_axis) as i32,
                        (v / per_axis) as i32,
                        neighbour_block,
                    );
                    if self.block_height(bx, by, bz).is_none() {
                        continue;
                    }
                    let Some(fluid) = self.fluid.as_mut() else {
                        continue;
                    };
                    fluid[axis][u * N + v] |= 1 << bit;
                    self.columns[axis][u * N + v] |= 1 << bit;
                }
            }
        }
    }

    #[must_use]
    fn material(&self, x: usize, y: usize, z: usize) -> u16 {
        self.materials[x + N * y + N * N * z]
    }

    /// Whether a cell holds fluid. Out-of-range cells are dry.
    #[must_use]
    fn is_fluid(&self, x: i32, y: i32, z: i32) -> bool {
        let Some(fluid) = self.fluid.as_ref() else {
            return false;
        };
        let inside = |value: i32| usize::try_from(value).ok().filter(|value| *value < N);
        let (Some(x), Some(y), Some(z)) = (inside(x), inside(y), inside(z)) else {
            return false;
        };
        fluid[1][x * N + z] >> (y as u32 + FIRST) & 1 == 1
    }

    /// A block's fluid surface height in [`FINE`] units, or `None` if it is dry.
    ///
    /// Block coordinates, valid one block PAST the chunk on every axis — see
    /// [`PADDED_BLOCKS`]. Anything further out is dry, which is right: the shell
    /// is one block wide because a corner is shared by four blocks and no
    /// question here reaches further than that.
    #[must_use]
    fn block_height(&self, bx: i32, by: i32, bz: i32) -> Option<u32> {
        let heights = self.heights.as_ref()?;
        match heights[height_index(bx, by, bz)?] {
            0 => None,
            height => Some(u32::from(height)),
        }
    }

    /// The four corner heights of the block a cell belongs to, packed.
    ///
    /// **The merge key for a fluid face**, six bits a corner. Two faces may only
    /// become one quad when the hardware's interpolation between the merged
    /// quad's four corners is the surface both of them describe — which across a
    /// single block it is exactly, and across two blocks whose corners differ it
    /// is not. A flat pond has one key everywhere and merges whole, so the case
    /// that decides a fluid mesh's size is untouched.
    #[must_use]
    fn surface_key(&self, cx: usize, cy: usize, cz: usize) -> u32 {
        let per_axis = SUBNODES_PER_AXIS as usize;
        let by = (cy / per_axis) as i32;
        let (bx, bz) = (cx / per_axis, cz / per_axis);
        let mut key = 0;
        for (corner, (dx, dz)) in [(0, 0), (1, 0), (0, 1), (1, 1)].into_iter().enumerate() {
            let height = self.surface_at((bx + dx) * per_axis, (bz + dz) * per_axis, by);
            key |= (height & 0x3F) << (corner * 6);
        }
        key
    }

    /// The fluid surface height at a vertex, in [`FINE`] units above the floor
    /// of block row `by`.
    ///
    /// **A pure function of the vertex's own position**, which is the property
    /// that matters and the reason it is written this way rather than per quad.
    /// Two quads that share an edge ask this the same question at the same
    /// coordinates and get the same answer, so a smoothed surface has no cracks
    /// in it — not because anything checks for them, but because there is
    /// nowhere for one to come from.
    ///
    /// The height is averaged over the up-to-four blocks touching this corner
    /// that actually hold fluid. Dry neighbours are left out rather than counted
    /// as zero: counting them would drag every shoreline down to nothing and
    /// leave a pond looking like a shallow dish.
    ///
    /// `cx` and `cz` are CELL coordinates and may sit inside a block rather than
    /// on its corner, which happens when greedy merging splits a face mid-block.
    /// They are rounded to the nearest block corner. That is an approximation
    /// and it is a consistent one — same input, same answer — so it still cannot
    /// crack.
    #[must_use]
    fn surface_at(&self, cx: usize, cz: usize, by: i32) -> u32 {
        let per_axis = SUBNODES_PER_AXIS as usize;
        // Nearest block corner: cell 0..1 belongs to corner 0, 2..4 to corner 1.
        let corner_x = (cx + per_axis / 2) / per_axis;
        let corner_z = (cz + per_axis / 2) / per_axis;

        let mut total = 0;
        let mut count = 0;
        // The four blocks meeting at this corner, in a fixed order.
        for (dx, dz) in [(-1, -1), (0, -1), (-1, 0), (0, 0)] {
            let bx = corner_x as i32 + dx;
            let bz = corner_z as i32 + dz;
            if let Some(height) = self.block_height(bx, by, bz) {
                total += height;
                count += 1;
            }
        }
        if count == 0 {
            // No fluid touches this corner at all. The vertex belongs to a face
            // that exists, so something here is wet — a block whose height row
            // is out of range, at a chunk seam. Full is the least wrong answer:
            // it leaves the surface flat rather than collapsing it.
            return FULL_BLOCK;
        }
        total / count
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

/// [`SubNodeGrid::cell`]'s mapping, in signed block coordinates.
///
/// The padding sits one block OUTSIDE the chunk, so the same permutation has to
/// be expressible at −1.
const fn block_cell(axis: usize, u: i32, v: i32, w: i32) -> (i32, i32, i32) {
    match axis {
        0 => (w, u, v),
        1 => (u, w, v),
        _ => (u, v, w),
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

/// Where a mesher asks what fluid a block holds.
///
/// A trait rather than a table so the mesher does not have to know how a client
/// stores its fluid, and so a test can flood one block without building a
/// registry.
pub trait FluidFill {
    /// The material and depth of the fluid in a block, in cells of 27.
    ///
    /// `None` for a dry block, and for a fluid the caller cannot draw — a
    /// server naming a fluid it never registered is drawn as nothing rather
    /// than guessed at.
    ///
    /// # The coordinates run one block PAST the chunk
    ///
    /// Chunk-local block coordinates, `-1..=CHUNK_BLOCKS` on every axis, which
    /// is the fluid's half of what [`SubNodeGrid::seed_padding`] does for
    /// terrain. Without that one block of overlap a chunk cannot tell milk in
    /// its neighbour from air: it drew a full wall of faces down every seam
    /// through a pond, twice over, and each side worked out a different surface
    /// height there. A caller that genuinely has no neighbour — a test, an
    /// unloaded chunk — answers `None` and gets what it always did.
    fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)>;
}

/// A world with no fluid in it, which is most of them.
pub struct NoFluid;

impl FluidFill for NoFluid {
    fn fill(&self, _x: i32, _y: i32, _z: i32) -> Option<(u16, u8)> {
        None
    }
}

impl<T: FluidFill + ?Sized> FluidFill for &T {
    fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
        (**self).fill(x, y, z)
    }
}

/// Lays a fluid slab into the bottom `depth` cells of a block.
///
/// The cells are ordinary occupancy, so everything downstream treats milk as
/// geometry: it merges, it occludes, it takes corner light. The one thing it is
/// NOT is collision — the physics reads fluid from the fluid layer, never from
/// a mesh.
fn fill_fluid(
    materials: &mut [u16],
    columns: &mut [Vec<u64>; 3],
    wet: &mut Option<[Vec<u64>; 3]>,
    heights: &mut Option<Vec<u8>>,
    local: LocalBlock,
    material: u16,
    depth: u8,
) {
    let base_x = local.x as usize * SUBNODES_PER_AXIS as usize;
    let base_y = local.y as usize * SUBNODES_PER_AXIS as usize;
    let base_z = local.z as usize * SUBNODES_PER_AXIS as usize;

    // **A wet block is FULL on the lattice, and the surface is the drop alone.**
    //
    // `depth` is the fraction of the block's HEIGHT that is full, in
    // twenty-sevenths — level 7 is 24/27, about 0.9 of a block, which is what
    // gives a brim-full block a visible surface. That is almost never a lattice
    // position, so the lattice cannot carry it and never could.
    //
    // What it used to try was to fill the ceiling of the surface in cells and
    // pull the top vertices back down. Two blocks of milk side by side then
    // disagreed about how many cells they occupied — a level 6 fills three, a
    // level 5 fills two — and face culling drew a WALL between them, one cell
    // tall, in the middle of a body of milk. A pond came out as a ziggurat of
    // terraces, each with a step face whose back side showed through the
    // transparent surface in front of it: the "internal faces" reported from
    // the window, and the harsh edge where a fall meets the pool it is filling.
    //
    // Filling every free cell removes the disagreement at the source. Two
    // adjacent wet blocks are now occupied identically whatever their levels, so
    // the face between them is interior and culled, and the ONLY thing that
    // shapes a body of milk is where its surface vertices are pulled down to.
    // That field is continuous by construction (`SubNodeGrid::surface_at` is a
    // pure function of a vertex's own position), so what used to be a staircase
    // is a sheet.
    //
    // It also makes the drop unconditionally non-negative — the lattice top is
    // now the highest a surface can be asked to sit — where the old rounding
    // only nearly did, and clamped a corner that wanted to rise above its own
    // block into a crack.
    let height = (u32::from(depth) * FULL_BLOCK / tiamot_core::UNITS_PER_BLOCK).min(FULL_BLOCK);
    let heights = heights.get_or_insert_with(|| vec![0u8; PADDED_BLOCKS.pow(3)]);
    // At least one unit: zero is how `block_height` says "dry", and a puddle
    // that exists to the physics and to `get_fluid` but not to the mesher is the
    // worst outcome available here.
    if let Some(index) = height_index(local.x as i32, local.y as i32, local.z as i32) {
        heights[index] = u8::try_from(height).unwrap_or(u8::MAX).max(1);
    }

    for cy in 0..SUBNODES_PER_AXIS as usize {
        for cz in 0..SUBNODES_PER_AXIS as usize {
            for cx in 0..SUBNODES_PER_AXIS as usize {
                let x = base_x + cx;
                let y = base_y + cy;
                let z = base_z + cz;
                // **Terrain wins the cell.** A block below the threshold holds
                // both, and milk drawn over the stone that is holding it up
                // would be milk you can see through the hill.
                if materials[x + N * y + N * N * z] != 0 {
                    continue;
                }
                materials[x + N * y + N * N * z] = material;
                columns[0][y * N + z] |= 1 << (x as u32 + FIRST);
                columns[1][x * N + z] |= 1 << (y as u32 + FIRST);
                columns[2][x * N + y] |= 1 << (z as u32 + FIRST);

                // The same bits again, fluid only. Allocated on the first drop
                // of milk in the chunk and never for a dry one.
                let wet = wet.get_or_insert_with(|| {
                    [vec![0u64; N * N], vec![0u64; N * N], vec![0u64; N * N]]
                });
                wet[0][y * N + z] |= 1 << (x as u32 + FIRST);
                wet[1][x * N + z] |= 1 << (y as u32 + FIRST);
                wet[2][x * N + y] |= 1 << (z as u32 + FIRST);
            }
        }
    }
}

/// A meshed chunk.
///
/// # Two quad lists, because fluid is drawn separately
///
/// Milk was laid into the same list as terrain and drawn in the same opaque
/// pass, which is why it could only ever be opaque. It is its own list now, and
/// the renderer draws it after the terrain with blending on and depth writes
/// off — the ordinary way to draw transparent geometry, and the only way a
/// player can see the ground through a pond.
///
/// The split is free in the mesher: the two face sets are already disjoint (a
/// cell holds terrain or fluid, never both) and already separately culled, so
/// this is the same work sorted into two buckets rather than any extra.
#[derive(Debug, Default, Clone)]
pub struct Mesh {
    /// The merged opaque quads.
    pub quads: Vec<Quad>,
    /// The fluid half, **already expanded to vertices**.
    ///
    /// Terrain stays as quads because a quad is self-describing: its four
    /// corners follow from its own position and extent. A fluid quad is not —
    /// where each corner sits depends on the surface height field, and which way
    /// it scrolls on that field's gradient — so it is resolved here, while the
    /// grid it was meshed from is still in hand, rather than being carried in a
    /// form that cannot be expanded later.
    pub fluid_vertices: Vec<FluidVertex>,
    /// Indices into [`Mesh::fluid_vertices`].
    pub fluid_indices: Vec<u32>,
}

impl Mesh {
    /// Four corners per quad, opaque geometry only.
    #[must_use]
    pub fn vertex_count(&self) -> usize {
        self.quads.len() * 4
    }

    /// Two triangles per quad, opaque geometry only.
    #[must_use]
    pub fn index_count(&self) -> usize {
        self.quads.len() * 6
    }

    /// How many fluid quads there are. Four vertices each.
    #[must_use]
    pub fn fluid_quad_count(&self) -> usize {
        self.fluid_vertices.len() / 4
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

    /// Total VRAM for this mesh, fluid included.
    #[must_use]
    pub fn gpu_bytes(&self) -> usize {
        self.vertex_bytes()
            + self.index_bytes()
            + self.fluid_vertices.len() * size_of::<FluidVertex>()
            + self.fluid_indices.len() * size_of::<u32>()
    }

    /// Whether the mesh has nothing to draw **at all**, fluid included.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty() && self.fluid_vertices.is_empty()
    }

    /// Whether there is no opaque geometry. A pond hanging in the air has none.
    #[must_use]
    pub fn has_no_terrain(&self) -> bool {
        self.quads.is_empty()
    }

    /// Whether there is no fluid geometry, which is nearly every chunk.
    #[must_use]
    pub fn has_no_fluid(&self) -> bool {
        self.fluid_vertices.is_empty()
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
            for (corner, (x, y, z)) in quad_corners(quad).into_iter().enumerate() {
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
            push_quad_indices(&mut indices, base, quad);
        }

        (vertices, indices)
    }
}

/// Expands fluid quads into vertices, against the grid they were meshed from.
///
/// **The grid is required and that is the point.** A fluid vertex carries where
/// the surface really is and which way it is running, and neither is a property
/// of the quad — both are read from the height field at the vertex's own
/// coordinates, which is what makes two quads sharing an edge agree about it and
/// so what keeps a smoothed surface free of cracks. See
/// [`SubNodeGrid::surface_at`].
#[must_use]
fn fluid_buffers(quads: &[Quad], grid: &SubNodeGrid) -> (Vec<FluidVertex>, Vec<u32>) {
    {
        let mut vertices = Vec::with_capacity(quads.len() * 4);
        let mut indices = Vec::with_capacity(quads.len() * 6);
        let per_axis = SUBNODES_PER_AXIS as usize;

        for quad in quads {
            let base = u32::try_from(vertices.len()).unwrap_or(0);
            for (corner, (x, y, z)) in quad_corners(quad).into_iter().enumerate() {
                // Which block row this vertex's surface belongs to. A vertex
                // sits ON a plane between two cells, so a vertex at the very
                // bottom of a block row is the top of the row beneath — take the
                // cell below it, which is the one the fluid is actually in.
                let below = y.saturating_sub(1) as usize;
                let by = (below / per_axis) as i32;

                // Only a vertex ON the surface moves. Everything else — the
                // bottom of a waterfall, the side of a column with more milk
                // above it — stays exactly on the lattice, so the drop is zero
                // and the geometry is what it always was.
                //
                // **A vertex is a corner, not a cell.** It is shared by up to
                // four cells horizontally, and the quad it belongs to may sit
                // on either side of it — a block's top face has corners at the
                // block's far edges, where the cell AT the corner coordinate is
                // already the next block along and is usually dry. Asking about
                // the single cell at `(x, z)` therefore answered "not fluid" for
                // every corner on a pond's positive edge, and the surface came
                // out flat because half its vertices never moved.
                //
                // So: any of the four below is milk, none of the four above is.
                let wet_below = fluid_touches(grid, x, below as u32, z);
                let wet_above = fluid_touches(grid, x, y, z);
                let drop = if wet_below && !wet_above {
                    let lattice = ((below % per_axis) as u32 + 1) * FINE;
                    let surface = grid.surface_at(x as usize, z as usize, by);
                    u16::try_from(lattice.saturating_sub(surface)).unwrap_or(u16::MAX)
                } else {
                    0
                };

                // A vertex coordinate runs to 48, so a vertex on the chunk's far
                // face divides to block 16 — which the height shell now answers
                // for. It used to be clamped to block 15 instead, because
                // `flow_at` reads an absent neighbour as the floor on purpose
                // (milk at the edge of a shelf really is running off it) and
                // that invented a current pouring off every seam in the world.
                // With the neighbour's real height in hand the honest answer is
                // available and the clamp would now be the thing that lies.
                let block = |cell: u32| -> i32 { (cell as usize / per_axis) as i32 };
                let flow = flow_at(grid, block(x), by, block(z));
                vertices.push(FluidVertex::new(
                    x,
                    y,
                    z,
                    quad.axis,
                    quad.positive,
                    quad.material,
                    quad.shade.corner(corner),
                    drop,
                    flow,
                ));
            }
            push_quad_indices(&mut indices, base, quad);
        }

        (vertices, indices)
    }
}

/// A quad's four corners in cell coordinates, in `to_buffers` order.
fn quad_corners(quad: &Quad) -> [(u32, u32, u32); 4] {
    let mut corners = [(0, 0, 0); 4];
    for (corner, (du, dv)) in [(0, 0), (1, 0), (1, 1), (0, 1)].into_iter().enumerate() {
        let u = u32::from(quad.u) + du * u32::from(quad.du);
        let v = u32::from(quad.v) + dv * u32::from(quad.dv);
        let w = u32::from(quad.w) + u32::from(quad.positive);
        corners[corner] = match quad.axis {
            0 => (w, u, v),
            1 => (u, w, v),
            _ => (u, v, w),
        };
    }
    corners
}

/// Two triangles, wound so the quad faces outward whichever way it points.
///
/// The y axis's `(u, v, w)` mapping is an odd permutation, so its corners
/// circulate the other way — see [`Mesh::to_buffers`].
fn push_quad_indices(indices: &mut Vec<u32>, base: u32, quad: &Quad) {
    if quad.positive == (quad.axis != 1) {
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    } else {
        indices.extend_from_slice(&[base, base + 2, base + 1, base, base + 3, base + 2]);
    }
}

/// A fluid vertex: 12 bytes.
///
/// [`PackedVertex`]'s two words, plus one that says where the milk's surface
/// actually is and which way it is running. Fluid has its own draw call for
/// transparency, so it can have its own format without costing terrain — which
/// is the overwhelming majority of a world's geometry — a single byte.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct FluidVertex {
    /// x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2 | fine light:8.
    /// Identical to [`PackedVertex::packed`], so one shader can unpack either.
    pub packed: u32,
    /// material:16 | light:16, as [`PackedVertex::material`].
    pub material: u32,
    /// `drop:16` | `flow_x:8` | `flow_z:8`.
    ///
    /// `drop` is how far BELOW the lattice position the vertex really sits, in
    /// [`FINE`] units — always zero for a vertex that is not on the surface, and
    /// never negative, because `fill_fluid` rounds the occupancy up.
    ///
    /// `flow_x` and `flow_z` are a signed direction, `i8`, `±127` for a full
    /// unit. Zero in both is still milk, which ripples rather than scrolls.
    pub surface: u32,
}

impl FluidVertex {
    /// Where this vertex sits on the lattice, in cells, before its drop.
    #[must_use]
    pub const fn position(&self) -> (u32, u32, u32) {
        (
            self.packed & 0x3F,
            (self.packed >> 6) & 0x3F,
            (self.packed >> 12) & 0x3F,
        )
    }

    /// The face's axis and direction.
    #[must_use]
    pub const fn face(&self) -> (u8, bool) {
        (
            ((self.packed >> 18) & 0x3) as u8,
            (self.packed >> 20) & 1 == 1,
        )
    }

    /// The material this vertex draws.
    #[must_use]
    pub const fn material(&self) -> u16 {
        (self.material & 0xFFFF) as u16
    }

    /// How far below the lattice the surface really is, in [`FINE`] units.
    #[must_use]
    pub const fn drop(&self) -> u16 {
        (self.surface & 0xFFFF) as u16
    }

    /// Which way the milk here is running, as the packed signed pair.
    #[must_use]
    pub const fn flow(&self) -> (i8, i8) {
        (
            ((self.surface >> 16) & 0xFF) as u8 as i8,
            ((self.surface >> 24) & 0xFF) as u8 as i8,
        )
    }

    /// Packs one corner.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "a vertex has this many fields; grouping them into a struct \
                  whose only purpose is to be unpacked here would move the \
                  argument list rather than shorten it"
    )]
    pub const fn new(
        x: u32,
        y: u32,
        z: u32,
        axis: u8,
        positive: bool,
        material: u16,
        corner: crate::shade::Corner,
        drop: u16,
        flow: (i8, i8),
    ) -> Self {
        let base = PackedVertex::lit(x, y, z, axis, positive, material, corner);
        Self {
            packed: base.packed,
            material: base.material,
            surface: (drop as u32) | ((flow.0 as u8 as u32) << 16) | ((flow.1 as u8 as u32) << 24),
        }
    }
}

/// Whether any of the four cells meeting at a vertical edge holds fluid.
///
/// `x` and `z` are a VERTEX's coordinates, so the cells that touch it are the
/// four at `x-1..=x` by `z-1..=z`. `y` is a cell row.
fn fluid_touches(grid: &SubNodeGrid, x: u32, y: u32, z: u32) -> bool {
    for dx in [-1, 0] {
        for dz in [-1, 0] {
            if grid.is_fluid(x as i32 + dx, y as i32, z as i32 + dz) {
                return true;
            }
        }
    }
    false
}

/// Which way the milk at a block is running, as a unit-ish `i8` pair.
///
/// **Downhill, from the surface itself.** The solver knows which neighbours it
/// pushed into, but none of that is on the wire and none of it needs to be: a
/// fluid's surface already slopes the way it flows, so the direction is the
/// negative gradient of the height field the smoothing pass built. A spring on a
/// slope has a surface falling away from it and scrolls outward; a settled pond
/// is flat, its gradient is zero, and it does not scroll at all — which is the
/// distinction the effect exists to draw, arrived at without a protocol change.
fn flow_at(grid: &SubNodeGrid, bx: i32, by: i32, bz: i32) -> (i8, i8) {
    let sample = |x: i32, z: i32| -> i32 {
        // A dry neighbour reads as the floor rather than as "no data": milk at
        // the edge of a shelf is running OFF it, and treating the empty side as
        // equal height would say it was still.
        grid.block_height(x, by, z).unwrap_or(0) as i32
    };
    let here = sample(bx, bz);
    let _ = here;
    let gradient_x = sample(bx + 1, bz) - sample(bx - 1, bz);
    let gradient_z = sample(bx, bz + 1) - sample(bx, bz - 1);
    // Downhill is the negative gradient. These are CENTRAL differences, taken
    // over two blocks, so a surface falling a full cell per block comes to two
    // cells across the difference — which is the divisor, and which makes such a
    // slope saturate exactly. Anything gentler is proportional; anything steeper
    // was already running as fast as this can say.
    let scale = |value: i32| -> i8 {
        let full = 2 * i32::try_from(FINE).unwrap_or(16);
        let scaled = -value * 127 / full;
        scaled.clamp(-127, 127) as i8
    };
    (scale(gradient_x), scale(gradient_z))
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
    /// x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2 | fine light:8
    ///
    /// The fine half is two more bits per light channel — quarter levels — in
    /// the same channel order as [`tiamot_core::light::Light`]. See
    /// [`crate::shade`] for why four bits alone cannot describe a gradient of
    /// one level per block, and note that this still leaves one bit spare.
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
                fine: 0,
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
                | (((corner.occlusion & 0x3) as u32) << 21)
                | ((corner.fine as u32) << 23),
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
    // The same for the fluid faces, which merge into their own quad list and
    // are drawn in their own pass. Allocated only for a chunk that has fluid.
    let mut wet_plane = if grid.fluid.is_some() {
        vec![0u64; N * N]
    } else {
        Vec::new()
    };
    // One slice's worth of corner light, reused across every slice and
    // direction. Entries for cells with no face are never read.
    let mut shades = vec![Shade::default(); N * N];
    // And the same for the fluid surface's merge keys, which only the fluid
    // pass fills and only a chunk with fluid in it allocates.
    let mut keys = if grid.fluid.is_some() {
        vec![0u32; N * N]
    } else {
        Vec::new()
    };
    // Merged into a local list and expanded at the end, once, against the grid.
    let mut fluid: Vec<Quad> = Vec::new();

    for (axis, positive) in FACES {
        // Face culling, a whole column at a time. This is the entire reason for
        // the bitmask representation: one shift and one AND-NOT decides 48
        // cells at once — including the two padding bits, which is what makes
        // border culling free rather than a special case.
        let columns = &grid.columns[axis];
        let wet = grid.fluid.as_ref().map(|fluid| &fluid[axis]);
        plane.fill(0);
        wet_plane.fill(0);

        for u in 0..N {
            for v in 0..N {
                let column = columns[u * N + v];
                // **Terrain is culled against terrain, fluid against
                // everything**, and the asymmetry is the whole point.
                //
                // While the two shared one occupancy set, a face between milk
                // and stone was interior and neither side drew it — so the
                // stone behind a pond did not exist. Opaque milk hides that
                // from outside and it is a hole straight through the world from
                // inside, which is what "under water I just see through the
                // world" was.
                //
                // The milk's face against the stone is the one that goes, since
                // the stone's is the one a swimmer can end up looking at. The
                // cost is drawing terrain a pond covers; it is behind opaque
                // geometry, so it is overdraw rather than anything visible.
                //
                // The two masks are disjoint by construction — a cell holds
                // terrain or fluid, never both, because terrain wins the cell in
                // `fill_fluid` — so OR-ing them is exactly the set of faces to
                // draw, and everything downstream reads the material per cell as
                // it always did.
                let solid = match wet {
                    Some(wet) => column & !wet[u * N + v],
                    None => column,
                };
                let faces = if positive {
                    solid & !(solid >> 1)
                } else {
                    solid & !(solid << 1)
                };
                // **The fluid's own faces go to their own plane**, rather than
                // being OR-ed into the terrain's as they were. They are drawn in
                // a separate, blended pass so that a pond can be seen through,
                // and a transparent surface cannot share a draw call with the
                // opaque world behind it.
                //
                // The two sets are disjoint by construction — a cell holds
                // terrain or fluid, never both, because terrain wins the cell in
                // `fill_fluid` — so this is the same faces sorted into two
                // buckets and not any extra work.
                let wet_faces = match wet {
                    Some(wet) => {
                        let wet = wet[u * N + v];
                        if positive {
                            wet & !(column >> 1)
                        } else {
                            wet & !(column << 1)
                        }
                    }
                    None => 0,
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
                let mut remaining = wet_faces >> FIRST;
                while remaining != 0 {
                    let w = remaining.trailing_zeros() as usize;
                    remaining &= remaining - 1;
                    if w < N {
                        wet_plane[w * N + u] |= 1 << v;
                    }
                }
            }
        }

        for w in 0..N {
            shade_and_merge(
                grid,
                light,
                &mut plane[w * N..(w + 1) * N],
                &mut shades,
                None,
                (axis, positive, w),
                &mut mesh.quads,
            );

            if wet_plane.is_empty() {
                continue;
            }
            shade_and_merge(
                grid,
                light,
                &mut wet_plane[w * N..(w + 1) * N],
                &mut shades,
                Some(&mut keys),
                (axis, positive, w),
                &mut fluid,
            );
        }
    }

    // Resolved here rather than carried as quads: see `Mesh::fluid_vertices`.
    let (vertices, indices) = fluid_buffers(&fluid, grid);
    mesh.fluid_vertices = vertices;
    mesh.fluid_indices = indices;
    mesh
}

/// Shades one slice's faces, then merges them.
///
/// **Each cell's shade is computed exactly once**, for the cells that actually
/// have a face, before merging looks at any of them. Computing inside the merge
/// instead means recomputing a candidate cell for every span it is tested
/// against — measured at 2.3× the mesh time on realistic content, which is most
/// of the way to the Task 02b gate for no reason. The fluid's merge keys ride
/// the same pass for the same reason, and only the fluid pass asks for them.
#[allow(
    clippy::too_many_arguments,
    reason = "the slice, its two scratch buffers, and which face of which slice \
              it is; grouping them would move the argument list rather than \
              shorten it"
)]
fn shade_and_merge(
    grid: &SubNodeGrid,
    light: &impl BlockLight,
    slice: &mut [u64],
    shades: &mut [Shade],
    keys: Option<&mut [u32]>,
    (axis, positive, w): (usize, bool, usize),
    out: &mut Vec<Quad>,
) {
    let mut keys = keys;
    for (u, row) in slice.iter().enumerate() {
        let mut bits = *row;
        while bits != 0 {
            let v = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let (cx, cy, cz) = SubNodeGrid::cell(axis, u, v, w);
            shades[u * N + v] = shade_at(light, grid, axis, positive, cx, cy, cz);
            if let Some(keys) = keys.as_deref_mut() {
                keys[u * N + v] = grid.surface_key(cx, cy, cz);
            }
        }
    }
    greedy_merge(grid, shades, slice, axis, positive, w, keys.as_deref(), out);
}

/// Meshes a chunk in one call.
#[must_use]
pub fn mesh_chunk(
    chunk: &Chunk,
    neighbours: &Neighbours<'_>,
    absent: Absent,
    light: &impl BlockLight,
    fluid: &impl FluidFill,
) -> Mesh {
    mesh(
        &SubNodeGrid::from_chunk_with_fluid(chunk, neighbours, absent, fluid),
        light,
    )
}

impl crate::shade::CellOccupancy for SubNodeGrid {
    fn solid(&self, x: i32, y: i32, z: i32) -> bool {
        // **Milk does not occlude.** Ambient occlusion is a statement about
        // light that geometry blocked, and a fluid you can see the world through
        // did not block it.
        //
        // It also decouples the shading from how deep the fluid lattice happens
        // to be filled: `fill_fluid` occupies every free cell of a wet block, so
        // reading fluid as solid here would darken the corners of the stone
        // around a puddle a ninth of a block deep as if it were a wall.
        if self.is_fluid(x, y, z) {
            return false;
        }
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
#[allow(
    clippy::too_many_arguments,
    reason = "the merge needs the grid, the shading, the plane it consumes, and \
              which face of which slice it is merging; every one of them is a \
              distinct input and none groups naturally with another"
)]
fn greedy_merge(
    grid: &SubNodeGrid,
    shades: &[Shade],
    plane: &mut [u64],
    axis: usize,
    positive: bool,
    w: usize,
    keys: Option<&[u32]>,
    out: &mut Vec<Quad>,
) {
    // **A fluid face carries its block's four CORNER heights as a merge key.**
    //
    // Precomputed per cell by the caller for the same reason the shades are:
    // working it out inside the merge re-derives a candidate cell once for every
    // span it is tested against.
    //
    // The block's own height is not enough and was the earlier version of this.
    // A corner is the average of the four blocks meeting at it, so two blocks at
    // the same level whose neighbours differ have different corners — and
    // merging them draws a straight line between the two ends of the run,
    // through corners the quad has no vertex for.
    let height_key = |u: usize, v: usize| -> u32 {
        match keys {
            Some(keys) => keys[u * N + v],
            None => 0,
        }
    };

    for u in 0..N {
        let mut row = plane[u];
        while row != 0 {
            let v = row.trailing_zeros() as usize;
            let (cx, cy, cz) = SubNodeGrid::cell(axis, u, v, w);
            let material = grid.material(cx, cy, cz);
            let shade = shades[u * N + v];
            let height = height_key(u, v);

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
                if grid.material(nx, ny, nz) != material
                    || shades[u * N + v + span_v] != shade
                    || height_key(u, v + span_v) != height
                {
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
                        && height_key(u + span_u, v + offset) == height
                });
                if !matches {
                    break;
                }
                plane[u + span_u] &= !run;
                span_u += 1;
            }

            out.push(Quad {
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

    /// Milk in one named block, at a level, and nothing anywhere else.
    struct Pond {
        block: LocalBlock,
        depth: u8,
        material: u16,
    }

    impl FluidFill for Pond {
        fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
            (x == self.block.x as i32 && y == self.block.y as i32 && z == self.block.z as i32)
                .then_some((self.material, self.depth))
        }
    }

    #[test]
    fn the_ground_under_a_pond_still_has_a_surface() {
        // **The face that was not there, and the hole it left.**
        //
        // Face culling is "occupied next to not-occupied". While milk and stone
        // shared one occupancy set, the boundary between them was interior and
        // neither side drew it — so the top of the ground under a pond did not
        // exist as geometry. Opaque milk hides that from above; from inside the
        // milk it is a hole straight through the world, reported from the
        // window as "under water I just see through the world".

        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(1, 0, 1), BlockValue::Uniform(STONE))
            .expect("in chunk");
        let pond = Pond {
            block: LocalBlock::new(1, 1, 1),
            // Brim full: 24 of 27, which lays two of the block's three cell
            // layers, so the milk sits on the stone with air above it.
            depth: 24,
            material: MILK,
        };

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &pond);
        let faces = faces(&mesh.quads);

        // The stone block is cells y 0..=2, the milk cells y 3..=4.
        // Axis 1, positive, w = 2 is the ground's top surface.
        assert!(
            faces.iter().any(|(axis, positive, _, _, w, material)| {
                *axis == 1 && *positive && *w == 2 && *material == STONE.get()
            }),
            "the ground under the pond has no top face, so a swimmer looks \
             straight through it"
        );

        // **And no milk at all is in the opaque list.** It is drawn in its own
        // blended pass now, which is both what makes a pond see-through and
        // what keeps the two occupancy sets from arguing over a shared face.
        assert!(
            !faces
                .iter()
                .any(|(_, _, _, _, _, material)| *material == MILK),
            "milk is still in the opaque quad list, so it cannot be transparent"
        );

        // The milk's own surface is there, in the fluid list, or there is no
        // pond to see. A brim-full block is 24 of 27, which is 2.625 cells, so
        // the occupancy rounds up to three layers and the surface vertices are
        // dropped back down to where the milk really is.
        let surface: Vec<_> = mesh
            .fluid_vertices
            .iter()
            .filter(|vertex| vertex.face() == (1, true) && vertex.material() == MILK)
            .collect();
        assert!(!surface.is_empty(), "the pond has no surface");
        for vertex in &surface {
            let (_, y, _) = vertex.position();
            assert_eq!(y, 6, "the milk's top face is not on the lattice top");
            // Three cells of lattice, 2.625 cells of milk: 0.375 of a cell, and
            // FINE is sixteenths, so six.
            assert_eq!(
                vertex.drop(),
                6,
                "a brim-full block's surface was not pulled down to where the \
                 milk actually is"
            );
        }
    }

    /// A material id for milk in the fluid fixtures below. Any non-zero value:
    /// the mesher never resolves it, it only carries it.
    const MILK: u16 = 9;

    #[test]
    fn a_flat_pond_is_flat_and_a_sloping_one_is_not() {
        // The two halves of the smoothing, in one fixture. A settled pond must
        // come out perfectly level — every vertex dropped by the same amount —
        // or a still surface shimmers as the eye moves along it. And a pond
        // whose blocks hold different amounts must NOT, or the smoothing is
        // doing nothing and this is the old staircase with extra arithmetic.
        struct Sloped;
        impl FluidFill for Sloped {
            fn fill(&self, x: i32, y: i32, _z: i32) -> Option<(u16, u8)> {
                if y != 4 {
                    return None;
                }
                // Deep at one end, shallow at the other.
                match x {
                    4 => Some((MILK, 24)),
                    5 => Some((MILK, 18)),
                    6 => Some((MILK, 9)),
                    _ => None,
                }
            }
        }
        struct Level;
        impl FluidFill for Level {
            fn fill(&self, x: i32, y: i32, _z: i32) -> Option<(u16, u8)> {
                if y == 4 && (4..=6).contains(&x) {
                    Some((MILK, 18))
                } else {
                    None
                }
            }
        }

        fn drops(chunk: &Chunk, fill: &impl FluidFill) -> Vec<u16> {
            let mesh = mesh_chunk(chunk, &Neighbours::open(), Absent::Air, &DAY, fill);
            let mut drops: Vec<u16> = mesh
                .fluid_vertices
                .iter()
                .filter(|vertex| vertex.face() == (1, true))
                .map(super::FluidVertex::drop)
                .collect();
            drops.sort_unstable();
            drops.dedup();
            drops
        }

        let chunk = empty();
        let flat = drops(&chunk, &Level);
        assert_eq!(
            flat.len(),
            1,
            "a level pond's surface came out at {flat:?} different heights"
        );

        let sloped = drops(&chunk, &Sloped);
        assert!(
            sloped.len() > 1,
            "a pond three blocks deep at one end and one at the other came out \
             perfectly flat ({sloped:?}), so nothing is being smoothed"
        );
    }

    #[test]
    fn still_milk_does_not_scroll_and_falling_milk_does() {
        // The flow direction is the negative gradient of the surface, so this
        // is the same fixture asking a different question: a level pond has no
        // gradient and must not scroll, and a surface that falls away must.
        struct Level;
        impl FluidFill for Level {
            fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
                if y == 4 && (4..=8).contains(&x) && (4..=8).contains(&z) {
                    Some((MILK, 24))
                } else {
                    None
                }
            }
        }
        struct Slope;
        impl FluidFill for Slope {
            fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
                if y != 4 || !(4..=8).contains(&z) {
                    return None;
                }
                match x {
                    4 => Some((MILK, 27)),
                    5 => Some((MILK, 18)),
                    6 => Some((MILK, 9)),
                    _ => None,
                }
            }
        }

        fn flows(chunk: &Chunk, fill: &impl FluidFill) -> Vec<(i8, i8)> {
            let mesh = mesh_chunk(chunk, &Neighbours::open(), Absent::Air, &DAY, fill);
            mesh.fluid_vertices
                .iter()
                .map(super::FluidVertex::flow)
                .collect()
        }

        let chunk = empty();

        // The middle of a level pond: every neighbour agrees, so the gradient
        // is zero everywhere it is measured against milk on both sides.
        assert!(
            flows(&chunk, &Level).contains(&(0, 0)),
            "a level pond has nowhere with zero flow, so still milk will scroll"
        );

        // And the slope runs downhill, in +x: deeper at x=4, so the surface
        // falls toward +x and the milk runs that way.
        let sloped = flows(&chunk, &Slope);
        assert!(
            sloped.iter().any(|(x, z)| *x > 0 && *z == 0),
            "milk on a surface falling toward +x, level in z, is not running \
             that way anywhere: {sloped:?}"
        );
        // The pond's own z edges DO have a gradient — a shore is a slope — so
        // this cannot ask that no vertex runs in z. What it can ask is that the
        // dominant direction is the one the surface actually falls in.
        let downhill = sloped.iter().filter(|(x, _)| *x > 0).count();
        let sideways = sloped.iter().filter(|(x, _)| *x < 0).count();
        assert!(
            downhill > sideways,
            "more milk is running uphill ({sideways}) than downhill ({downhill})"
        );
    }

    #[test]
    fn a_chunk_with_no_fluid_meshes_exactly_as_it_did() {
        // The other half of the change: separating the two occupancy sets must
        // cost a dry chunk nothing at all, in faces or in allocation. Nearly
        // every chunk in a world is dry.
        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(2, 2, 2), BlockValue::Uniform(STONE))
            .expect("in chunk");
        chunk
            .set_block(BlockPos::new(2, 3, 2), BlockValue::Uniform(STONE))
            .expect("in chunk");

        let grid = SubNodeGrid::from_chunk(&chunk, &Neighbours::open(), Absent::Air);
        assert!(
            grid.fluid.is_none(),
            "a dry chunk allocated fluid occupancy columns"
        );

        let dry = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
        assert_eq!(
            dry.quads.len(),
            6,
            "two stacked blocks should still merge to six quads"
        );
    }

    #[test]
    fn a_single_block_is_six_quads() {
        // 27 cells, but every interior face is culled and each of the six
        // outer 3x3 faces merges to one quad.
        let mut chunk = empty();
        chunk
            .set_block(BlockPos::new(5, 5, 5), BlockValue::Uniform(STONE))
            .expect("in chunk");

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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
        let before =
            faces(&mesh_chunk(&solid, &Neighbours::open(), Absent::Air, &DAY, &NoFluid).quads);

        let mut chiselled = solid.clone();
        // A corner cell of the block: removing it exposes three inward faces
        // and removes three outward ones.
        chiselled
            .set_subnode(SubNodePos::new(15, 15, 15), MaterialId::AIR)
            .expect("in chunk");
        let after =
            faces(&mesh_chunk(&chiselled, &Neighbours::open(), Absent::Air, &DAY, &NoFluid).quads);

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

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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

        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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

        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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

        let open = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
        let closed = mesh_chunk(&chunk, &Neighbours::none(), Absent::Solid, &DAY, &NoFluid);

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

        let left_mesh = mesh_chunk(&left, &left_neighbours, Absent::Air, &DAY, &NoFluid);
        let right_mesh = mesh_chunk(&right, &right_neighbours, Absent::Air, &DAY, &NoFluid);

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
        let unaware = mesh_chunk(&left, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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
        // above it lit and every other block dark.
        //
        // A block's light sits at the block's CENTRE, which in cells is 1.5,
        // and the lattice points along a block's own face are at 1 and 2 — a
        // sixth of a block either side of that centre. So the brightest corner
        // of the face carries `5/6 × 5/6 = 100/144` of the lit block and takes
        // the rest from its dark neighbours: `15 × 100/144`, which is 10.42, or
        // ten levels and a quarter-level remainder of two.
        //
        // **It used to be 15 exactly**, and that was the bug rather than the
        // specification: averaging the four blocks touching a corner samples
        // the same block four times everywhere except on a block boundary, so
        // the interior of every face was flat and the whole of each level
        // change landed on one cell. See [`crate::shade`] for the measurement.
        // An isolated lit block having its peak rounded off is what smoothing
        // means; a lamp's falloff is close to linear, and bilinear
        // interpolation of a linear field is exact.
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
        let mesh = mesh_chunk(
            &chunk,
            &Neighbours::open(),
            Absent::Air,
            &OneLitBlock,
            &NoFluid,
        );

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

        // 15 × 100/144 = 10.42: ten whole levels.
        assert_eq!(
            u32::from(brightest),
            u32::from(MAX_LEVEL) * 100 / 144,
            "the corner over the middle of the lit block should carry 100/144 of it"
        );
        // And the quarter-level remainder on that same corner is really there,
        // which is the half of this that four bits alone could not have said.
        // Paired with its level rather than maximised separately: the maximum
        // remainder anywhere is 3, on some dimmer corner, and comparing it with
        // this corner's expected 2 measures nothing.
        let fine_sun = top
            .iter()
            .flat_map(|quad| quad.shade.light.iter().zip(quad.shade.fine.iter()))
            .filter(|(level, _)| level.sun() == brightest)
            .map(|(_, fine)| (fine >> 6) & 0x3)
            .max()
            .unwrap_or(0);
        assert_eq!(
            fine_sun, 2,
            "the quarter levels never reached the brightest corner"
        );
        assert!(
            dimmest < brightest,
            "every corner of the face came to {brightest}, so there is no gradient across it"
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
            lit, brightest,
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

        let split = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &HalfLit, &NoFluid);
        let uniform = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);

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
        let mesh = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
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
        let mesh = mesh_chunk(&empty(), &Neighbours::open(), Absent::Air, &DAY, &NoFluid);
        assert!(mesh.is_empty());
        assert_eq!(mesh.gpu_bytes(), 0);
    }
}
