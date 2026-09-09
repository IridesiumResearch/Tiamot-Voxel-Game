// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The scratch buffer worldgen writes into.
//!
//! # The whole point: block-level is the cheap default
//!
//! Sub-Node Contract §5 says generators write at **block resolution** by
//! default, and sub-node detail is opt-in per generator. This type is what makes
//! that real rather than aspirational.
//!
//! A buffer starts backed by 16³ = 4,096 block slots. The moment a sub-node
//! operation touches it, and not before, it expands to 48³ = 110,592 cells. A
//! generator that only ever places blocks — which is nearly all of them — never
//! allocates or touches 27× the memory, and never pays 27× the fill cost.
//!
//! Expansion is one-way. There is no attempt to detect that a buffer has become
//! uniform again and collapse back: it would cost a scan on every write to catch
//! a case that arises rarely, and [`ChunkBuffer::to_chunk`] already compresses
//! the result properly on the way out.
//!
//! This is the object handed to Lua generator callbacks in Task 05.

use crate::block::{BlockValue, Cells, EMPTY_CELLS, SUBNODES_PER_BLOCK, subnode_index};
use crate::chunk::Chunk;
use crate::coords::{ChunkPos, LocalBlock};
use crate::material::MaterialId;
use crate::{BLOCKS_PER_CHUNK, CHUNK_BLOCKS, CHUNK_SUBNODES, SUBNODES_PER_AXIS};

/// Sub-node cells in a fully expanded buffer.
pub const CELLS_PER_CHUNK: usize = (CHUNK_SUBNODES * CHUNK_SUBNODES * CHUNK_SUBNODES) as usize;

/// How a buffer is currently backed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Storage {
    /// One material per block. The default and the cheap path.
    Blocks(Vec<MaterialId>),
    /// One material per sub-node cell. Entered only on demand.
    SubNodes(Vec<MaterialId>),
}

/// A scratch chunk that worldgen fills and then converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkBuffer {
    pos: ChunkPos,
    storage: Storage,
    /// Fluid placed during generation, if the generator placed any.
    ///
    /// **Beside the terrain rather than in it.** A block holds a material and,
    /// separately, a volume of one fluid — see `fluid::Fluid` — so an ocean is
    /// not a material a generator fills with, it is a layer over the same
    /// blocks. Kept empty until something asks for it, because the great
    /// majority of chunks have no fluid at all and an empty layer costs
    /// nothing.
    fluid: crate::fluid::FluidLayer,
}

/// How much resolution a density fill gives the surface.
///
/// Block resolution is the default and costs what it always did. The other two
/// are the opt-in of Sub-Node Contract §5, and they differ in what they can
/// SHOW rather than only in what they cost — see
/// [`ChunkBuffer::fill_density_detail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Ask the field about all 27 cells of a surface block.
    ///
    /// The finest answer available, and the only one that can show a term the
    /// mod added at sub-node scale.
    Sampled,
    /// Interpolate the block-resolution samples down to the 27 cells.
    ///
    /// Nearly free, and it cannot show anything the block-resolution field did
    /// not already contain: it removes staircases rather than adding detail.
    Smooth,
}

/// Trilinearly interpolates a block's eight corner samples to its 27 cells.
///
/// The samples are at block ORIGINS, so a block's eight corners are the samples
/// at its own position and the seven one step along each axis — which is why
/// the caller's field is padded on the positive side as well as the negative.
///
/// Cell centres rather than cell corners: cell `c` spans `c/3 .. (c+1)/3` of
/// the block, so its middle is `(c + 0.5) / 3`. Sampling the corner instead
/// would bias every surface half a cell towards the block's origin, which over
/// a whole world reads as terrain sitting slightly too low.
fn trilinear_cells(field: &[f32], pitch: usize, at: [usize; 3], out: &mut [f32]) {
    let [px, py, pz] = at;
    let corner = |dx: usize, dy: usize, dz: usize| {
        field[(px + dx) + pitch * ((py + dy) + pitch * (pz + dz))]
    };
    let c000 = corner(0, 0, 0);
    let c100 = corner(1, 0, 0);
    let c010 = corner(0, 1, 0);
    let c110 = corner(1, 1, 0);
    let c001 = corner(0, 0, 1);
    let c101 = corner(1, 0, 1);
    let c011 = corner(0, 1, 1);
    let c111 = corner(1, 1, 1);

    let axis = SUBNODES_PER_AXIS as usize;
    for z in 0..axis {
        let tz = (z as f32 + 0.5) / axis as f32;
        for y in 0..axis {
            let ty = (y as f32 + 0.5) / axis as f32;
            for x in 0..axis {
                let tx = (x as f32 + 0.5) / axis as f32;
                // Written as explicit multiply-adds rather than `mul_add`:
                // charter rule 4 bans the latter, because it uses a hardware
                // FMA where there is one and a software fallback where there
                // is not, and the two round differently.
                let x00 = c000 + (c100 - c000) * tx;
                let x10 = c010 + (c110 - c010) * tx;
                let x01 = c001 + (c101 - c001) * tx;
                let x11 = c011 + (c111 - c011) * tx;
                let y0 = x00 + (x10 - x00) * ty;
                let y1 = x01 + (x11 - x01) * ty;
                out[subnode_index(x as u32, y as u32, z as u32)] = y0 + (y1 - y0) * tz;
            }
        }
    }
}

impl ChunkBuffer {
    /// A buffer full of one material, at block resolution.
    #[must_use]
    pub fn new(pos: ChunkPos, fill: MaterialId) -> Self {
        Self {
            pos,
            storage: Storage::Blocks(vec![fill; BLOCKS_PER_CHUNK]),
            fluid: crate::fluid::FluidLayer::default(),
        }
    }

    /// An empty buffer.
    #[must_use]
    pub fn air(pos: ChunkPos) -> Self {
        Self::new(pos, MaterialId::AIR)
    }

    /// The chunk this buffer is for.
    #[must_use]
    pub const fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Whether the buffer has expanded to sub-node resolution.
    ///
    /// Exposed so a generator, or a test, can assert it stayed on the cheap
    /// path — which is the kind of thing that regresses silently.
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        matches!(self.storage, Storage::SubNodes(_))
    }

    /// Heap bytes currently used.
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        match &self.storage {
            Storage::Blocks(cells) | Storage::SubNodes(cells) => {
                cells.capacity() * size_of::<MaterialId>()
            }
        }
    }

    // -- block-level operations (the cheap, default path) ------------------

    /// Fills every block.
    ///
    /// Collapses an expanded buffer back to block storage, because after this
    /// there is by definition no sub-node detail left to preserve. The one case
    /// where collapsing is free.
    pub fn fill_all(&mut self, material: MaterialId) {
        self.storage = Storage::Blocks(vec![material; BLOCKS_PER_CHUNK]);
    }

    /// The material of a block. For an expanded buffer, the material of its
    /// first sub-node — enough for a generator deciding what to place on top.
    #[must_use]
    pub fn get_block(&self, local: LocalBlock) -> MaterialId {
        match &self.storage {
            Storage::Blocks(cells) => cells[local.index()],
            Storage::SubNodes(cells) => cells[Self::cell_index(local, 0, 0, 0)],
        }
    }

    /// Sets a whole block.
    ///
    /// Stays on the cheap path if the buffer has not expanded; writes all 27
    /// cells if it has.
    pub fn set_block(&mut self, local: LocalBlock, material: MaterialId) {
        match &mut self.storage {
            Storage::Blocks(cells) => cells[local.index()] = material,
            Storage::SubNodes(cells) => {
                for z in 0..SUBNODES_PER_AXIS {
                    for y in 0..SUBNODES_PER_AXIS {
                        for x in 0..SUBNODES_PER_AXIS {
                            cells[Self::cell_index(local, x, y, z)] = material;
                        }
                    }
                }
            }
        }
    }

    /// Fills every block strictly below a per-column height, in world blocks.
    ///
    /// The bread-and-butter terrain operation, and deliberately a single call:
    /// a Lua generator producing a heightmap and handing it over once costs one
    /// FFI crossing, where a per-block loop would cost 4,096.
    ///
    /// `heights` is indexed `x + 16 * z`, in world block coordinates. Columns
    /// whose height falls below this chunk leave it untouched; columns above it
    /// fill it completely.
    ///
    /// # Errors
    ///
    /// [`BufferError::WrongHeightmapSize`] if `heights` is not 256 long.
    pub fn fill_below_heightmap(
        &mut self,
        heights: &[i32],
        material: MaterialId,
    ) -> Result<(), BufferError> {
        const COLUMNS: usize = (CHUNK_BLOCKS * CHUNK_BLOCKS) as usize;
        if heights.len() != COLUMNS {
            return Err(BufferError::WrongHeightmapSize {
                expected: COLUMNS,
                found: heights.len(),
            });
        }

        let base_y = self.pos.y * CHUNK_BLOCKS as i32;
        for z in 0..CHUNK_BLOCKS {
            for x in 0..CHUNK_BLOCKS {
                let height = heights[(x + CHUNK_BLOCKS * z) as usize];
                // How many of this chunk's 16 layers are below the surface.
                let filled = (height - base_y).clamp(0, CHUNK_BLOCKS as i32);
                for y in 0..filled {
                    self.set_block(LocalBlock::new(x, y as u32, z), material);
                }
            }
        }
        Ok(())
    }

    /// Fills every block where a density field is POSITIVE.
    ///
    /// The one rule, and it is deliberately the only one: **greater than zero
    /// is solid**. A threshold parameter would be a second way to say what
    /// subtracting a constant already says, and a mod that wants a different
    /// cut-off subtracts it inside the field where the rest of the arithmetic
    /// lives.
    ///
    /// Evaluated at BLOCK resolution — one sample per block, at the block's
    /// own corner. Sub-node resolution is 27 times the samples and, measured,
    /// 2.66 ms a chunk for a single octave against a 50 ms tick shared by all
    /// simulation for all players; see `docs/worldgen-density.md`. A mod
    /// wanting sub-node detail carves it with `set_subnode` afterwards, where
    /// the cost is proportional to what it actually changes.
    ///
    /// # Errors
    ///
    /// [`BufferError::Density`] if the program will not evaluate over a chunk.
    pub fn fill_density(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        material: MaterialId,
    ) -> Result<(), BufferError> {
        let side = CHUNK_BLOCKS as usize;
        let region = super::noise::Region3d {
            origin_x: (self.pos.x * CHUNK_BLOCKS as i32) as f32,
            origin_y: (self.pos.y * CHUNK_BLOCKS as i32) as f32,
            origin_z: (self.pos.z * CHUNK_BLOCKS as i32) as f32,
            step: 1.0,
            width: side,
            height: side,
            depth: side,
        };
        let mut field = vec![0.0f32; region.len()];
        density.evaluate(seed, &region, &mut field)?;

        // x-fastest, matching `fill_3d`'s layout and `LocalBlock::index`, so
        // no transpose and no chance of a transposed one.
        let mut index = 0;
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    if field[index] > 0.0 {
                        self.set_block(LocalBlock::new(x, y, z), material);
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Fills from a density field at SUB-NODE resolution, near the surface.
    ///
    /// # Why this is not twenty-seven times the cost
    ///
    /// A chunk is mostly not surface. Every block deep inside the ground is
    /// solid in all twenty-seven of its cells and every block in open air is
    /// empty in all of them, and sampling either at sub-node resolution asks a
    /// question whose answer was already known. Only the blocks the isosurface
    /// actually CROSSES need the finer look, and they are a shell through the
    /// chunk rather than a volume of it.
    ///
    /// So: one block-resolution pass over the chunk and one block of padding
    /// around it, then the fine pass on the blocks whose own sample disagrees
    /// in sign with one of their six neighbours. A generator pays for the shell
    /// rather than for the volume.
    ///
    /// # The two kinds of detail
    ///
    /// [`Detail::Sampled`] asks the field itself about all twenty-seven cells,
    /// so a mod that adds a high-frequency term to its density sees that term
    /// in the terrain. [`Detail::Smooth`] interpolates the block-resolution
    /// samples it already has, which costs almost nothing and cannot show
    /// anything finer than the block-scale field — it removes staircases rather
    /// than adding detail. Both produce a surface that follows the isosurface
    /// at sub-node resolution, which is the thing block resolution cannot do.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if the density program cannot be evaluated.
    pub fn fill_density_detail(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        material: MaterialId,
        detail: Detail,
    ) -> Result<(), BufferError> {
        // Padded by one block on every side, because whether a block is on the
        // surface is a question about its NEIGHBOURS, and the ones at the chunk
        // edge have neighbours in the next chunk. Cheaper than it looks: 18³ is
        // 1.4x the samples of 16³, and it is the only extra field work the
        // smooth path does at all.
        const PAD: usize = 1;
        let side = CHUNK_BLOCKS as usize;
        let padded = side + PAD * 2;
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let region = super::noise::Region3d {
            origin_x: (origin[0] - PAD as i32) as f32,
            origin_y: (origin[1] - PAD as i32) as f32,
            origin_z: (origin[2] - PAD as i32) as f32,
            step: 1.0,
            width: padded,
            height: padded,
            depth: padded,
        };
        let mut field = vec![0.0f32; region.len()];
        let mut scratch = super::density::Scratch::default();
        density.evaluate_with(seed, &region, &mut field, &mut scratch)?;

        // x-fastest, matching `Region3d`'s own layout.
        let at = |x: usize, y: usize, z: usize| field[x + padded * (y + padded * z)];

        let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        for z in 0..side {
            for y in 0..side {
                for x in 0..side {
                    let (px, py, pz) = (x + PAD, y + PAD, z + PAD);
                    let here = at(px, py, pz) > 0.0;
                    let surface = [
                        at(px - 1, py, pz),
                        at(px + 1, py, pz),
                        at(px, py - 1, pz),
                        at(px, py + 1, pz),
                        at(px, py, pz - 1),
                        at(px, py, pz + 1),
                    ]
                    .iter()
                    .any(|value| (*value > 0.0) != here);

                    let local = LocalBlock::new(x as u32, y as u32, z as u32);
                    if !surface {
                        // Wholly inside or wholly outside: the answer at block
                        // resolution is the answer, and writing it as a block
                        // keeps the buffer unexpanded where nothing is carved.
                        if here {
                            self.set_block(local, material);
                        }
                        continue;
                    }

                    match detail {
                        Detail::Sampled => {
                            let cell_region = super::noise::Region3d {
                                origin_x: (origin[0] + x as i32) as f32,
                                origin_y: (origin[1] + y as i32) as f32,
                                origin_z: (origin[2] + z as i32) as f32,
                                step: 1.0 / SUBNODES_PER_AXIS as f32,
                                width: SUBNODES_PER_AXIS as usize,
                                height: SUBNODES_PER_AXIS as usize,
                                depth: SUBNODES_PER_AXIS as usize,
                            };
                            density.evaluate_with(seed, &cell_region, &mut fine, &mut scratch)?;
                        }
                        Detail::Smooth => trilinear_cells(&field, padded, [px, py, pz], &mut fine),
                    }

                    // **A fill ADDS.** At block resolution a block where the
                    // field is not positive is left alone, and the cells are
                    // no different: start from what the block holds now and
                    // write the material only where the field is positive.
                    // Writing the other cells as air — which this did at
                    // first — meant a second sub-node fill wiped the first
                    // wherever its own surface crossed, so a generator could
                    // not put grass on dirt, or dirt on stone, without
                    // carving air between them.
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    for (index, value) in fine.iter().enumerate() {
                        if *value > 0.0 {
                            cells[index] = material;
                        }
                    }
                    self.set_block_cells(local, &cells);
                }
            }
        }
        Ok(())
    }

    // -- sub-node operations (the opt-in path) -----------------------------

    /// The material of one sub-node cell.
    ///
    /// Reading does **not** expand the buffer: an unexpanded block answers for
    /// all 27 of its cells.
    #[must_use]
    pub fn get_subnode(&self, local: LocalBlock, x: u32, y: u32, z: u32) -> MaterialId {
        match &self.storage {
            Storage::Blocks(cells) => cells[local.index()],
            Storage::SubNodes(cells) => cells[Self::cell_index(local, x, y, z)],
        }
    }

    /// Sets one sub-node cell.
    ///
    /// **This is the call that expands the buffer**, and the only kind that
    /// does. A generator that never calls it never pays for sub-nodes.
    pub fn set_subnode(&mut self, local: LocalBlock, x: u32, y: u32, z: u32, material: MaterialId) {
        self.expand();
        let Storage::SubNodes(cells) = &mut self.storage else {
            unreachable!("expand() guarantees sub-node storage");
        };
        cells[Self::cell_index(local, x, y, z)] = material;
    }

    /// Sets all 27 cells of a block at once.
    ///
    /// Expands, since a per-cell array is sub-node detail by definition.
    pub fn set_block_cells(&mut self, local: LocalBlock, block_cells: &Cells) {
        self.expand();
        let Storage::SubNodes(cells) = &mut self.storage else {
            unreachable!("expand() guarantees sub-node storage");
        };
        for z in 0..SUBNODES_PER_AXIS {
            for y in 0..SUBNODES_PER_AXIS {
                for x in 0..SUBNODES_PER_AXIS {
                    cells[Self::cell_index(local, x, y, z)] = block_cells[subnode_index(x, y, z)];
                }
            }
        }
    }

    /// Copies a block region from another buffer.
    ///
    /// Expands only if the source has: blitting block-resolution content into a
    /// block-resolution buffer should stay cheap, which is what makes structure
    /// placement affordable.
    ///
    /// The region is clipped to both buffers, so a structure overhanging a chunk
    /// boundary is handled by passing the same call to each chunk.
    pub fn blit(&mut self, source: &Self, from: LocalBlock, to: LocalBlock, size: [u32; 3]) {
        if source.is_expanded() {
            self.expand();
        }

        for dz in 0..size[2] {
            for dy in 0..size[1] {
                for dx in 0..size[0] {
                    let (Some(src), Some(dst)) =
                        (Self::offset(from, dx, dy, dz), Self::offset(to, dx, dy, dz))
                    else {
                        continue;
                    };

                    if source.is_expanded() || self.is_expanded() {
                        for z in 0..SUBNODES_PER_AXIS {
                            for y in 0..SUBNODES_PER_AXIS {
                                for x in 0..SUBNODES_PER_AXIS {
                                    let material = source.get_subnode(src, x, y, z);
                                    self.set_subnode(dst, x, y, z, material);
                                }
                            }
                        }
                    } else {
                        self.set_block(dst, source.get_block(src));
                    }
                }
            }
        }
    }

    /// Fills every block below `level` with `fluid`, around the terrain.
    ///
    /// **An ocean is a layer, not a material.** A block holds a material and,
    /// separately, a volume of one fluid, so filling a sea is not a matter of
    /// setting blocks to water — it is putting a volume into the space the
    /// ground leaves. Sub-Node Contract §4.
    ///
    /// How much goes in a block is what `Fluid::room_in` says the terrain
    /// leaves it: a solid block takes nothing, an empty one takes all 27 cells,
    /// and a half-carved one takes what is left. So a shoreline comes out of
    /// the terrain rather than having to be described.
    ///
    /// `level` is a WORLD height, like a heightmap's, so a sea level is one
    /// number for the whole world and does not restart at every chunk.
    ///
    /// **Placed, not simulated.** The conserved solver moves what exists and
    /// creates nothing (see `docs/subnode-contract.md` §4), so worldgen is the
    /// only place an ocean can come from. It settles from here like any other
    /// volume.
    pub fn fill_fluid_below(&mut self, level: i32, fluid: crate::fluid::FluidId) {
        let base_y = self.pos.y * CHUNK_BLOCKS as i32;
        for y in 0..CHUNK_BLOCKS {
            if base_y + y as i32 >= level {
                break;
            }
            for z in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    let room = crate::fluid::MAX_VOLUME.saturating_sub(self.filled_cells(local));
                    if room > 0 {
                        self.fluid.set(local, crate::fluid::Fluid::new(fluid, room));
                    }
                }
            }
        }
    }

    /// How many of a block's 27 cells hold terrain.
    ///
    /// **Whole-block first.** A buffer that has never been chiselled stores one
    /// material a block, so the answer is 0 or 27 without touching a cell —
    /// which is every block of a heightmap or density world. The per-cell count
    /// is for the expanded case and costs 27 reads only there.
    fn filled_cells(&self, local: LocalBlock) -> u32 {
        if !self.is_expanded() {
            return if self.get_block(local).is_air() {
                0
            } else {
                crate::fluid::MAX_VOLUME
            };
        }
        let mut filled = 0;
        for z in 0..SUBNODES_PER_AXIS {
            for y in 0..SUBNODES_PER_AXIS {
                for x in 0..SUBNODES_PER_AXIS {
                    if !self.get_subnode(local, x, y, z).is_air() {
                        filled += 1;
                    }
                }
            }
        }
        filled
    }

    /// The fluid this buffer holds, for the caller that stores it.
    #[must_use]
    pub const fn fluid(&self) -> &crate::fluid::FluidLayer {
        &self.fluid
    }

    // -- conversion --------------------------------------------------------

    /// Builds the palette-compressed [`Chunk`].
    ///
    /// Canonicalisation happens inside [`Chunk::set_block_local`], so a buffer
    /// full of blocks that happen to be uniform comes out as `Uniform` entries
    /// and a chiselled one comes out as `Partial` or `Mixed` — whichever is
    /// correct — without the generator having to know the difference.
    #[must_use]
    pub fn to_chunk(&self) -> Chunk {
        match &self.storage {
            Storage::Blocks(materials) => {
                // Start from the first block's material so a uniform buffer
                // produces a one-entry palette with no index storage at all,
                // rather than building 4,096 identical writes over air.
                let mut chunk = Chunk::new(self.pos, materials[0]);
                for (index, &material) in materials.iter().enumerate() {
                    if material != materials[0] {
                        chunk.set_block_local(
                            LocalBlock::from_index(index),
                            BlockValue::Uniform(material),
                        );
                    }
                }
                chunk
            }
            Storage::SubNodes(_) => {
                let mut chunk = Chunk::air(self.pos);
                for index in 0..BLOCKS_PER_CHUNK {
                    let local = LocalBlock::from_index(index);
                    let mut block_cells = EMPTY_CELLS;
                    for z in 0..SUBNODES_PER_AXIS {
                        for y in 0..SUBNODES_PER_AXIS {
                            for x in 0..SUBNODES_PER_AXIS {
                                block_cells[subnode_index(x, y, z)] =
                                    self.get_subnode(local, x, y, z);
                            }
                        }
                    }
                    chunk.set_block_local(local, BlockValue::Cells(block_cells));
                }
                chunk
            }
        }
    }

    // -- internals ---------------------------------------------------------

    /// Expands to sub-node storage, if not already expanded.
    fn expand(&mut self) {
        let Storage::Blocks(blocks) = &self.storage else {
            return;
        };

        let mut cells = vec![MaterialId::AIR; CELLS_PER_CHUNK];
        for (index, &material) in blocks.iter().enumerate() {
            let local = LocalBlock::from_index(index);
            for z in 0..SUBNODES_PER_AXIS {
                for y in 0..SUBNODES_PER_AXIS {
                    for x in 0..SUBNODES_PER_AXIS {
                        cells[Self::cell_index(local, x, y, z)] = material;
                    }
                }
            }
        }
        self.storage = Storage::SubNodes(cells);
    }

    /// Flat index of a sub-node cell. x-fastest, matching every other layout in
    /// the engine.
    const fn cell_index(local: LocalBlock, x: u32, y: u32, z: u32) -> usize {
        let cell_x = local.x * SUBNODES_PER_AXIS + x;
        let cell_y = local.y * SUBNODES_PER_AXIS + y;
        let cell_z = local.z * SUBNODES_PER_AXIS + z;
        (cell_x + CHUNK_SUBNODES * cell_y + CHUNK_SUBNODES * CHUNK_SUBNODES * cell_z) as usize
    }

    /// A block offset from `base`, or `None` if it leaves the chunk.
    const fn offset(base: LocalBlock, dx: u32, dy: u32, dz: u32) -> Option<LocalBlock> {
        let x = base.x + dx;
        let y = base.y + dy;
        let z = base.z + dz;
        if x >= CHUNK_BLOCKS || y >= CHUNK_BLOCKS || z >= CHUNK_BLOCKS {
            return None;
        }
        Some(LocalBlock { x, y, z })
    }
}

/// A buffer operation was given something it could not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BufferError {
    /// A heightmap was the wrong length.
    #[error("heightmap holds {found} columns but a chunk has {expected}")]
    WrongHeightmapSize {
        /// Columns a chunk has.
        expected: usize,
        /// Columns supplied.
        found: usize,
    },
    /// A density program could not be evaluated over this chunk.
    #[error(transparent)]
    Density(#[from] super::density::DensityError),
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);

    fn origin() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    /// A field that is positive below a sloping plane: y < x/4, so the surface
    /// crosses blocks at a shallow angle that block resolution has to stair-step
    /// and sub-node resolution does not.
    fn slope() -> super::super::density::Density {
        use super::super::density::{Axis, Op};
        super::super::density::Density::compile(vec![
            Op::Coordinate(Axis::X),
            Op::Constant(0.25),
            Op::Multiply,
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compile")
    }

    #[test]
    fn a_second_detail_fill_adds_to_the_first_instead_of_clearing_it() {
        // **Fills layer.** A generator paints stone, then dirt over it with a
        // band of the same field, then grass over that. Each of those is a
        // sub-node fill whose surface crosses blocks the earlier fill already
        // shaped, and the rule at cell resolution is the block rule: where the
        // field is not positive, leave the cell alone. Written as air instead,
        // the second fill's surface wipes the first's cells under it, and a
        // hillside comes out as ribbons of grass floating over stepped dirt.
        for detail in [Detail::Sampled, Detail::Smooth] {
            let mut buffer = ChunkBuffer::new(origin(), MaterialId(5));
            buffer
                .fill_density_detail(&slope(), 7, MaterialId(2), detail)
                .expect("fill");
            // A block the slope crosses: x = 8 puts the surface at y = 2.
            let local = LocalBlock::new(8, 2, 8);
            let mut below = 0;
            let mut above = 0;
            for cell in 0..SUBNODES_PER_BLOCK {
                let (sx, sy, sz) = crate::block::subnode_offset(cell);
                match buffer.get_subnode(local, sx, sy, sz) {
                    MaterialId(2) => below += 1,
                    MaterialId(5) => above += 1,
                    other => panic!("a cell the fill did not claim became {other:?} ({detail:?})"),
                }
            }
            assert!(
                below > 0 && above > 0,
                "the block was not crossed ({detail:?})"
            );
        }
    }

    #[test]
    fn detail_carves_the_surface_and_leaves_the_depths_alone() {
        // **The mechanism in one assertion.** A block deep inside the ground is
        // solid in all 27 of its cells and a block in open air is empty in all
        // of them; only the blocks the surface crosses are worth 27 samples.
        // What must NOT happen is the depths being carved — that would be the
        // shell test misfiring, and it would look like holes in the ground.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density_detail(&slope(), 7, MaterialId(2), Detail::Sampled)
            .expect("fill");

        // Deep below the slope: solid, every cell.
        for cell in 0..SUBNODES_PER_BLOCK {
            let (sx, sy, sz) = crate::block::subnode_offset(cell);
            assert_eq!(
                buffer.get_subnode(LocalBlock::new(8, 0, 8), sx, sy, sz),
                MaterialId(2),
                "a block under the surface was carved"
            );
        }
        // Well above it: empty, every cell.
        for cell in 0..SUBNODES_PER_BLOCK {
            let (sx, sy, sz) = crate::block::subnode_offset(cell);
            assert_eq!(
                buffer.get_subnode(LocalBlock::new(0, 12, 0), sx, sy, sz),
                MaterialId::AIR,
                "a block in open air was filled"
            );
        }
    }

    #[test]
    fn detail_gives_a_surface_block_some_cells_and_not_others() {
        // The point of sub-node worldgen: a block the surface crosses comes out
        // PART full. At block resolution every block is all or nothing, which
        // is the staircase a mod author is trying to get rid of.
        for detail in [Detail::Sampled, Detail::Smooth] {
            let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
            buffer
                .fill_density_detail(&slope(), 7, MaterialId(2), detail)
                .expect("fill");
            assert!(
                buffer.is_expanded(),
                "{detail:?} produced no sub-node detail at all"
            );

            let mut partial = 0;
            for x in 0..CHUNK_BLOCKS {
                for y in 0..CHUNK_BLOCKS {
                    let filled = (0..SUBNODES_PER_BLOCK)
                        .filter(|cell| {
                            let (sx, sy, sz) = crate::block::subnode_offset(*cell);
                            buffer.get_subnode(LocalBlock::new(x, y, 0), sx, sy, sz)
                                != MaterialId::AIR
                        })
                        .count();
                    if filled > 0 && filled < SUBNODES_PER_BLOCK {
                        partial += 1;
                    }
                }
            }
            assert!(
                partial > 0,
                "{detail:?} left every block all-or-nothing, which is block resolution"
            );
        }
    }

    #[test]
    fn detail_agrees_with_the_block_fill_about_what_is_solid() {
        // A sub-node fill must not MOVE the terrain. Every block the block-
        // resolution pass fills is at least partly filled here, and every block
        // it leaves empty is at most partly filled — so a mod switching detail
        // on gets the same landscape at a finer resolution rather than a
        // different one.
        let density = slope();
        let mut blocks = ChunkBuffer::new(origin(), MaterialId::AIR);
        blocks
            .fill_density(&density, 7, MaterialId(2))
            .expect("fill");
        let mut fine = ChunkBuffer::new(origin(), MaterialId::AIR);
        fine.fill_density_detail(&density, 7, MaterialId(2), Detail::Smooth)
            .expect("fill");

        for x in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                let local = LocalBlock::new(x, y, 4);
                let coarse = blocks.get_subnode(local, 0, 0, 0) != MaterialId::AIR;
                let filled = (0..SUBNODES_PER_BLOCK)
                    .filter(|cell| {
                        let (sx, sy, sz) = crate::block::subnode_offset(*cell);
                        fine.get_subnode(local, sx, sy, sz) != MaterialId::AIR
                    })
                    .count();
                if coarse {
                    assert!(filled > 0, "({x},{y}) was solid and came out empty");
                } else {
                    assert!(
                        filled < SUBNODES_PER_BLOCK,
                        "({x},{y}) was empty and came out solid"
                    );
                }
            }
        }
    }

    #[test]
    fn a_new_buffer_is_block_backed() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        assert!(!buffer.is_expanded());
        assert_eq!(buffer.memory_usage(), BLOCKS_PER_CHUNK * 2);
    }

    #[test]
    fn block_operations_never_expand() {
        // The property Sub-Node Contract §5 depends on. If this regresses,
        // every generator silently starts paying 27x.
        let mut buffer = ChunkBuffer::air(origin());
        buffer.fill_all(STONE);
        buffer.set_block(LocalBlock::new(1, 2, 3), DIRT);
        let heights = [8; 256];
        buffer
            .fill_below_heightmap(&heights, STONE)
            .expect("heightmap");
        assert_eq!(buffer.get_block(LocalBlock::new(1, 2, 3)), STONE);
        assert!(
            !buffer.is_expanded(),
            "block-level work must stay on the cheap path"
        );
    }

    #[test]
    fn reading_a_subnode_does_not_expand() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1), STONE);
        assert!(!buffer.is_expanded(), "a read must not cost an expansion");
    }

    #[test]
    fn writing_a_subnode_expands_once() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1, DIRT);
        assert!(buffer.is_expanded());
        assert_eq!(buffer.memory_usage(), CELLS_PER_CHUNK * 2);

        // The expansion must have preserved everything.
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1), DIRT);
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0), STONE);
        assert_eq!(buffer.get_subnode(LocalBlock::new(9, 9, 9), 2, 2, 2), STONE);
    }

    #[test]
    fn expanding_costs_exactly_twenty_seven_times_the_memory() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        let before = buffer.memory_usage();
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, DIRT);
        assert_eq!(buffer.memory_usage(), before * 27);
    }

    #[test]
    fn fill_all_collapses_an_expanded_buffer() {
        let mut buffer = ChunkBuffer::air(origin());
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, DIRT);
        assert!(buffer.is_expanded());
        buffer.fill_all(STONE);
        assert!(
            !buffer.is_expanded(),
            "after filling everything there is no sub-node detail left to keep"
        );
        assert_eq!(buffer.to_chunk().is_uniform(), Some(STONE));
    }

    #[test]
    fn setting_a_block_on_an_expanded_buffer_writes_all_cells() {
        let mut buffer = ChunkBuffer::air(origin());
        let local = LocalBlock::new(2, 2, 2);
        buffer.set_subnode(local, 0, 0, 0, DIRT);
        buffer.set_block(local, STONE);
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    assert_eq!(buffer.get_subnode(local, x, y, z), STONE);
                }
            }
        }
    }

    #[test]
    fn heightmap_fills_the_right_layers() {
        let mut buffer = ChunkBuffer::air(origin());
        let mut heights = [0i32; 256];
        heights[0] = 5;
        heights[1] = 0;
        heights[2] = 100; // above the chunk: fills it entirely
        heights[3] = -10; // below the chunk: leaves it alone
        buffer.fill_below_heightmap(&heights, STONE).expect("fill");

        for y in 0..16 {
            assert_eq!(
                buffer.get_block(LocalBlock::new(0, y, 0)),
                if y < 5 { STONE } else { MaterialId::AIR },
                "column 0 at y={y}"
            );
        }
        assert_eq!(buffer.get_block(LocalBlock::new(1, 0, 0)), MaterialId::AIR);
        assert_eq!(buffer.get_block(LocalBlock::new(2, 15, 0)), STONE);
        assert_eq!(buffer.get_block(LocalBlock::new(3, 0, 0)), MaterialId::AIR);
    }

    #[test]
    fn heightmap_respects_the_chunks_vertical_offset() {
        // A chunk at y=1 covers world blocks 16..32, so a height of 20 fills
        // only its bottom four layers.
        let mut buffer = ChunkBuffer::air(ChunkPos::new(0, 1, 0));
        buffer
            .fill_below_heightmap(&[20; 256], STONE)
            .expect("fill");
        assert_eq!(buffer.get_block(LocalBlock::new(0, 3, 0)), STONE);
        assert_eq!(buffer.get_block(LocalBlock::new(0, 4, 0)), MaterialId::AIR);
    }

    #[test]
    fn a_wrong_sized_heightmap_is_an_error_not_a_panic() {
        let mut buffer = ChunkBuffer::air(origin());
        assert!(matches!(
            buffer.fill_below_heightmap(&[0; 10], STONE),
            Err(BufferError::WrongHeightmapSize { .. })
        ));
    }

    #[test]
    fn to_chunk_canonicalises() {
        let mut buffer = ChunkBuffer::air(origin());
        let local = LocalBlock::new(0, 0, 0);
        // Every cell of a block set to the same material must come out Uniform,
        // not as a Mixed slot holding 27 copies.
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    buffer.set_subnode(local, x, y, z, STONE);
                }
            }
        }
        let chunk = buffer.to_chunk();
        assert_eq!(
            chunk.get_block_local(local),
            crate::BlockView::Uniform(STONE)
        );
        assert_eq!(chunk.mixed_len(), 0, "no mixed slot should be needed");
    }

    #[test]
    fn a_uniform_buffer_produces_a_uniform_chunk() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        let chunk = buffer.to_chunk();
        assert_eq!(chunk.is_uniform(), Some(STONE));
        assert_eq!(chunk.palette_len(), 1);
        assert_eq!(chunk.bits_per_index(), 0);
    }

    #[test]
    fn a_partially_chiselled_block_survives_conversion() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        let local = LocalBlock::new(5, 5, 5);
        buffer.set_subnode(local, 0, 0, 0, MaterialId::AIR);
        let chunk = buffer.to_chunk();
        let view = chunk.get_block_local(local);
        assert_eq!(view.occupied_units(), 26);
        assert_eq!(view.subnode(subnode_index(0, 0, 0)), MaterialId::AIR);
    }

    #[test]
    fn blit_copies_block_content_without_expanding() {
        let mut source = ChunkBuffer::air(origin());
        source.set_block(LocalBlock::new(0, 0, 0), STONE);
        source.set_block(LocalBlock::new(1, 0, 0), DIRT);

        let mut target = ChunkBuffer::air(origin());
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(4, 4, 4),
            [2, 1, 1],
        );

        assert_eq!(target.get_block(LocalBlock::new(4, 4, 4)), STONE);
        assert_eq!(target.get_block(LocalBlock::new(5, 4, 4)), DIRT);
        assert!(
            !target.is_expanded(),
            "a block-resolution blit must stay on the cheap path"
        );
    }

    #[test]
    fn blit_from_an_expanded_source_carries_subnode_detail() {
        let mut source = ChunkBuffer::new(origin(), STONE);
        source.set_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1, DIRT);

        let mut target = ChunkBuffer::air(origin());
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(8, 8, 8),
            [1, 1, 1],
        );

        assert!(target.is_expanded());
        assert_eq!(target.get_subnode(LocalBlock::new(8, 8, 8), 1, 1, 1), DIRT);
        assert_eq!(target.get_subnode(LocalBlock::new(8, 8, 8), 0, 0, 0), STONE);
    }

    #[test]
    fn blit_clips_at_the_chunk_edge() {
        let mut source = ChunkBuffer::new(origin(), STONE);
        source.set_block(LocalBlock::new(0, 0, 0), DIRT);

        let mut target = ChunkBuffer::air(origin());
        // Deliberately overhanging: a structure at the chunk edge.
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(15, 15, 15),
            [4, 4, 4],
        );
        assert_eq!(target.get_block(LocalBlock::new(15, 15, 15)), DIRT);
    }

    #[test]
    fn set_block_cells_expands_and_round_trips() {
        let mut buffer = ChunkBuffer::air(origin());
        let mut cells = EMPTY_CELLS;
        cells[0] = STONE;
        cells[26] = DIRT;
        let local = LocalBlock::new(3, 3, 3);
        buffer.set_block_cells(local, &cells);

        assert!(buffer.is_expanded());
        for (index, expected) in cells.iter().enumerate() {
            let (x, y, z) = crate::block::subnode_offset(index);
            assert_eq!(buffer.get_subnode(local, x, y, z), *expected);
        }
    }
}
