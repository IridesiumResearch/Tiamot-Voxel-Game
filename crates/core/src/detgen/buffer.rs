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
        // **What the field cannot possibly do here, before doing it.** Most of
        // a world is not near its surface: a chunk a hundred blocks up is all
        // sky and one a hundred down is all rock, and both used to cost a full
        // evaluation to discover. `Density::bounds` answers from the program's
        // shape rather than from samples, so it cannot miss surface the way a
        // handful of probe points can — see its own docs for why that
        // distinction is the whole design.
        let bounds = density.bounds(seed, &region);
        if bounds.is_all_empty() {
            return Ok(());
        }
        if bounds.is_all_solid() {
            self.fill_all(material);
            return Ok(());
        }

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
        // The same skip as the block-resolution fill, and it is worth more
        // here: this path pays 18³ samples and then a 27-sample pass on every
        // block of the surface shell. **Over the PADDED region**, so a chunk
        // called solid is solid including the ring its neighbours look at, and
        // no block can be found on the surface after the fact.
        let bounds = density.bounds(seed, &region);
        if bounds.is_all_empty() {
            return Ok(());
        }
        if bounds.is_all_solid() {
            // Every cell of every block, with no surface anywhere in it: the
            // sub-node pass would write exactly this and take 4,096 samples to
            // decide it.
            self.fill_all(material);
            return Ok(());
        }

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

    /// Stands a run of cells of `material` on every surface the buffer holds.
    ///
    /// # Why this is a fill of its own and not a field
    ///
    /// Ground cover — grass, ferns — wants three things at once: to stand ON
    /// the surface the ground fill made, neither floating over a smooth slope
    /// nor sunk into it; to be a cell or two tall; and to stay inside ONE
    /// block, so a tuft is never two stacked blocks that highlight and dig
    /// apart. A density field can say none of that. It has no idea which block
    /// a sample is in, and a run two cells tall in a block whose surface sits
    /// at an arbitrary cell contains the block's one sample point in a third
    /// of columns — so [`fill_density_detail`](Self::fill_density_detail)'s
    /// surface-shell test, built from those samples, misses the other two
    /// thirds in stripes that follow the contours. The buffer, on the other
    /// hand, knows exactly where every surface is: it is wherever an occupied
    /// cell has an empty one above it.
    ///
    /// # The rule
    ///
    /// For every cell column, at the LOWEST cell in each block that is empty
    /// with an occupied cell below it, and where `take` is positive at that
    /// cell (everywhere, with no `take`), that cell and up to `cells - 1`
    /// empty cells above it become `material` — never crossing into the block
    /// above, and never overwriting a cell that holds something. Where the
    /// surface is a block's top cell the run is that one cell. Cover written
    /// by this call is not a surface for it: a run never stands on another
    /// run.
    ///
    /// **The lowest, not every one**, which matters only for a block holding
    /// two surfaces at once — a one-cell shelf, air over stone over air over
    /// stone inside three cells. The shelf's upper face gets nothing. It is a
    /// rare shape and the lower face is the one a player stands on, so this is
    /// a deliberate limit rather than an oversight; the alternative is runs
    /// interleaved with the ground they grow from, inside one block, which is
    /// the thing this call exists to avoid.
    ///
    /// `cells` is clamped to 1..=3. `take` is evaluated at cell resolution and
    /// only in the blocks that hold a surface, the economy the detail fill
    /// makes.
    ///
    /// The chunk's bottom cell row has nothing below it to look at, so a
    /// surface exactly on the chunk floor gets no cover from this chunk: the
    /// neighbouring chunk is not available at generation. One row in
    /// forty-eight.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if `take` cannot be evaluated.
    pub fn fill_cover(
        &mut self,
        material: MaterialId,
        cells: u32,
        take: Option<&super::density::Density>,
        seed: u64,
    ) -> Result<(), BufferError> {
        let cells = cells.clamp(1, SUBNODES_PER_AXIS);
        // **Nothing to stand on, and nothing to look at.** Cover grows where an
        // empty cell sits on an occupied one, and a buffer holding one material
        // everywhere has no such cell anywhere in it — the chunk floor is the
        // only boundary left, and this call already cannot reach below itself.
        //
        // Worth its own check because the scan below reads all 27 cells of all
        // 4,096 blocks whatever they hold: measured at 0.047 ms on a chunk of
        // plain air, against 0.011 ms to generate one. A generator calling this
        // over a streamed column pays that on every chunk of empty sky above
        // its terrain and every chunk of solid rock below it, which is most of
        // them.
        if let Storage::Blocks(blocks) = &self.storage
            && let Some(first) = blocks.first()
            && blocks.iter().all(|block| block == first)
        {
            return Ok(());
        }
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        let mut scratch = super::density::Scratch::default();
        let mut block_cells: Cells = EMPTY_CELLS;

        for z in 0..CHUNK_BLOCKS {
            for x in 0..CHUNK_BLOCKS {
                // Whether the cell under each of this column's nine cell
                // columns is occupied, carried up block by block. Starts
                // false: the chunk floor has no cell below it.
                let mut below = [false; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize];
                for y in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                block_cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    // The bases: empty cells standing on an occupied one.
                    // Read BEFORE anything is written, so a run this call
                    // writes is never the ground for another.
                    let mut bases: [Option<u32>; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize] =
                        [None; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize];
                    let mut any = false;
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cx in 0..SUBNODES_PER_AXIS {
                            let column = (cz * SUBNODES_PER_AXIS + cx) as usize;
                            let mut under = below[column];
                            for cy in 0..SUBNODES_PER_AXIS {
                                let here =
                                    block_cells[subnode_index(cx, cy, cz)] != MaterialId::AIR;
                                if !here && under && bases[column].is_none() {
                                    bases[column] = Some(cy);
                                    any = true;
                                }
                                under = here;
                            }
                            below[column] = under;
                        }
                    }
                    if !any {
                        continue;
                    }
                    if let Some(take) = take {
                        let cell_region = super::noise::Region3d {
                            origin_x: (origin[0] + x as i32) as f32,
                            origin_y: (origin[1] + y as i32) as f32,
                            origin_z: (origin[2] + z as i32) as f32,
                            step: 1.0 / SUBNODES_PER_AXIS as f32,
                            width: SUBNODES_PER_AXIS as usize,
                            height: SUBNODES_PER_AXIS as usize,
                            depth: SUBNODES_PER_AXIS as usize,
                        };
                        take.evaluate_with(seed, &cell_region, &mut fine, &mut scratch)?;
                    }
                    let mut written = false;
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cx in 0..SUBNODES_PER_AXIS {
                            let column = (cz * SUBNODES_PER_AXIS + cx) as usize;
                            let Some(base) = bases[column] else {
                                continue;
                            };
                            if take.is_some() && fine[subnode_index(cx, base, cz)] <= 0.0 {
                                continue;
                            }
                            // Up from the base, inside this block, through
                            // empty cells only.
                            for cy in base..(base + cells).min(SUBNODES_PER_AXIS) {
                                let index = subnode_index(cx, cy, cz);
                                if block_cells[index] != MaterialId::AIR {
                                    break;
                                }
                                block_cells[index] = material;
                                written = true;
                            }
                        }
                    }
                    if written {
                        self.set_block_cells(local, &block_cells);
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes one block by WORLD position, ignoring anything outside this chunk.
    ///
    /// # Why a generator needs to aim outside its own chunk
    ///
    /// A structure — a tree, a hut, a ruin — is rooted at one place and reaches
    /// out from it, and nothing makes that reach stop at a chunk boundary. The
    /// obvious fix is to let a generator write into its neighbours and have the
    /// engine hold those writes until those chunks are made, and that fix is
    /// **wrong**: chunks are generated in whatever order players walk towards
    /// them, so a chunk made before its neighbour would be missing the half of
    /// a tree that neighbour was going to contribute, and made after it would
    /// have it. Same seed, different world. Charter rule 4 is not only about
    /// floats.
    ///
    /// The order-independent shape is the other way round: when generating a
    /// chunk, run the structure pass for **every chunk within reach** — each
    /// one's structures are a pure function of its own position and the seed,
    /// via `rng_stream` — and write all of them in world coordinates. The ones
    /// that land here are kept and the rest are dropped, and the same chunk
    /// comes out whatever was generated before it. Every neighbour does the
    /// same work and keeps a different slice of it.
    ///
    /// That is what this call is for: **dropping is the feature**. A generator
    /// placing a structure whose root is two chunks away should not have to
    /// know which of its blocks fall inside, and asking it to check would be
    /// asking every mod to reimplement this and get the edges right.
    ///
    /// Returns whether the write landed, which a caller may ignore.
    pub fn set_block_world(&mut self, at: crate::BlockPos, material: MaterialId) -> bool {
        if at.chunk() != self.pos {
            return false;
        }
        self.set_block(at.local(), material);
        true
    }

    /// Writes one sub-node cell by WORLD cell position, ignoring anything
    /// outside this chunk.
    ///
    /// The sub-node half of [`Self::set_block_world`], for the same reason and
    /// with the same rule. Expands the buffer only when the write lands, so a
    /// generator running the structure pass for a neighbourhood does not pay
    /// 27x for the chunks it contributes nothing to.
    pub fn set_subnode_world(&mut self, at: crate::SubNodePos, material: MaterialId) -> bool {
        let block = crate::BlockPos::new(
            at.x.div_euclid(SUBNODES_PER_AXIS as i32),
            at.y.div_euclid(SUBNODES_PER_AXIS as i32),
            at.z.div_euclid(SUBNODES_PER_AXIS as i32),
        );
        if block.chunk() != self.pos {
            return false;
        }
        self.set_subnode(
            block.local(),
            at.x.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            at.y.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            at.z.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            material,
        );
        true
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
    fn skipping_a_chunk_by_its_bounds_writes_what_evaluating_it_would_have() {
        // **The differential test the pruning has to pass.** `fill_density`
        // now asks `Density::bounds` whether a chunk can hold any surface and
        // returns early when it cannot, which is only sound if the answer is
        // the same as doing the work. So: the same fills, checked against a
        // reference that evaluates every block and knows nothing about bounds.
        //
        // Chunks are chosen to cover all three outcomes — wholly empty, wholly
        // solid, and crossed — and the assertion at the end is that all three
        // actually occurred, because a run where every chunk was crossed would
        // pass this without testing anything.
        use super::super::density::{Axis, Op};
        use super::super::noise::{Fractal, FractalParams};

        let density = super::super::density::Density::compile(vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 3,
                    frequency: 0.02,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 10.0,
                stream: 5,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compile");

        let (mut empty, mut solid, mut crossed) = (0, 0, 0);
        for cy in -6..6 {
            for cx in -2..3 {
                let pos = ChunkPos::new(cx, cy, 1);
                let mut buffer = ChunkBuffer::new(pos, MaterialId::AIR);
                buffer.fill_density(&density, 3, STONE).expect("fill");

                // The reference: evaluate the whole chunk, block by block.
                let side = CHUNK_BLOCKS as usize;
                let region = super::super::noise::Region3d {
                    origin_x: (pos.x * CHUNK_BLOCKS as i32) as f32,
                    origin_y: (pos.y * CHUNK_BLOCKS as i32) as f32,
                    origin_z: (pos.z * CHUNK_BLOCKS as i32) as f32,
                    step: 1.0,
                    width: side,
                    height: side,
                    depth: side,
                };
                let mut field = vec![0.0f32; region.len()];
                density.evaluate(3, &region, &mut field).expect("evaluate");

                let mut positive = 0;
                let mut index = 0;
                for z in 0..CHUNK_BLOCKS {
                    for y in 0..CHUNK_BLOCKS {
                        for x in 0..CHUNK_BLOCKS {
                            let local = LocalBlock::new(x, y, z);
                            let wanted = if field[index] > 0.0 {
                                positive += 1;
                                STONE
                            } else {
                                MaterialId::AIR
                            };
                            assert_eq!(
                                buffer.get_block(local),
                                wanted,
                                "chunk {pos:?} block ({x}, {y}, {z}): the pruned fill and the \
                                 reference disagree"
                            );
                            index += 1;
                        }
                    }
                }
                if positive == 0 {
                    empty += 1;
                } else if positive == region.len() {
                    solid += 1;
                } else {
                    crossed += 1;
                }
            }
        }

        assert!(
            empty > 0 && solid > 0 && crossed > 0,
            "the sweep has to reach all three outcomes to mean anything: \
             {empty} empty, {solid} solid, {crossed} crossed"
        );
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

    /// Ground under the cover tests: the slope, at sub-node resolution, so
    /// its surface sits at a different cell in different columns.
    fn sloped_ground() -> ChunkBuffer {
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density_detail(&slope(), 7, STONE, Detail::Sampled)
            .expect("fill");
        buffer
    }

    /// The material of the cell directly under `(local, cx, cy, cz)`, or
    /// `None` at the chunk floor.
    fn under(
        buffer: &ChunkBuffer,
        local: LocalBlock,
        cx: u32,
        cy: u32,
        cz: u32,
    ) -> Option<MaterialId> {
        if cy > 0 {
            Some(buffer.get_subnode(local, cx, cy - 1, cz))
        } else if local.y > 0 {
            Some(buffer.get_subnode(LocalBlock::new(local.x, local.y - 1, local.z), cx, 2, cz))
        } else {
            None
        }
    }

    #[test]
    fn cover_stands_on_the_ground_inside_one_block_and_is_never_stacked() {
        // Every cover cell either stands on ground, or stands on a cover cell
        // that stands on ground — and a cover cell at the bottom of a block
        // ALWAYS stands on ground, which is what "never two stacked blocks"
        // means in cells. The slope puts the surface at every cell height, so
        // the one-cell case (surface in a block's top cell) and the two-cell
        // case both occur.
        const GRASS: MaterialId = MaterialId(9);
        let mut buffer = sloped_ground();
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");

        let (mut singles, mut doubles, mut total) = (0, 0, 0);
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cell in 0..SUBNODES_PER_BLOCK {
                        let (cx, cy, cz) = crate::block::subnode_offset(cell);
                        if buffer.get_subnode(local, cx, cy, cz) != GRASS {
                            continue;
                        }
                        total += 1;
                        let below = under(&buffer, local, cx, cy, cz)
                            .expect("cover never sits on the chunk floor");
                        if below == GRASS {
                            assert!(
                                cy > 0,
                                "a run crossed a block boundary at {local:?} ({cx}, {cy}, {cz})"
                            );
                            let ground = under(&buffer, local, cx, cy - 1, cz).expect("in chunk");
                            assert_eq!(
                                ground, STONE,
                                "a run is more than two cells tall at {local:?}"
                            );
                            doubles += 1;
                        } else {
                            assert_eq!(
                                below, STONE,
                                "cover floating at {local:?} ({cx}, {cy}, {cz})"
                            );
                            let above_is_grass =
                                cy < 2 && buffer.get_subnode(local, cx, cy + 1, cz) == GRASS;
                            if !above_is_grass {
                                singles += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(total > 0, "the slope should have grown cover");
        assert!(
            doubles > 0,
            "no run reached two cells, so `cells` did nothing"
        );
        assert!(
            singles > 0,
            "no run was clipped to one cell at a block's top, which the slope must produce"
        );
    }

    #[test]
    fn cover_on_a_chunk_of_one_material_writes_nothing_and_looks_at_nothing() {
        // The skip that makes this call affordable over a streamed column: a
        // chunk of plain sky or plain rock has no cell standing on another, so
        // there is nothing to find and the 27-cells-of-4,096-blocks scan is
        // pure cost. The assertion is that skipping it is not a behaviour
        // change — the same chunk, cell for cell.
        const GRASS: MaterialId = MaterialId(9);
        for held in [MaterialId::AIR, STONE] {
            let mut buffer = ChunkBuffer::new(origin(), held);
            let before = buffer.clone();
            buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
            assert_eq!(
                buffer, before,
                "cover changed a chunk made entirely of one material"
            );
        }

        // And the skip is not reached once anything breaks the uniformity, or
        // it would be skipping real work. One block of stone in the sky is a
        // surface, and its top cell grows a run.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer.set_block(LocalBlock::new(5, 5, 5), STONE);
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 6, 5), 1, 0, 1),
            GRASS,
            "the block over a lone stone block should have grown cover"
        );
    }

    #[test]
    fn cover_takes_only_where_the_field_is_positive_and_never_overwrites() {
        // `take` positive for x < 8 only; and a slab of stone laid one cell
        // over part of the surface, which a run must not write into.
        use super::super::density::{Axis, Op};
        const GRASS: MaterialId = MaterialId(9);
        let take = super::super::density::Density::compile(vec![
            Op::Constant(8.0),
            Op::Coordinate(Axis::X),
            Op::Subtract,
        ])
        .expect("compile");
        let mut buffer = sloped_ground();
        buffer.fill_cover(GRASS, 2, Some(&take), 7).expect("cover");

        let mut west = 0;
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cell in 0..SUBNODES_PER_BLOCK {
                        let (cx, cy, cz) = crate::block::subnode_offset(cell);
                        if buffer.get_subnode(local, cx, cy, cz) == GRASS {
                            assert!(x < 8, "cover where `take` is not positive, at {local:?}");
                            west += 1;
                        }
                    }
                }
            }
        }
        assert!(west > 0, "no cover where `take` is positive");

        // A block of stone set whole is untouched, and the cell under it in
        // the column gets a run of one.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        for x in 0..CHUNK_BLOCKS {
            for z in 0..CHUNK_BLOCKS {
                buffer.set_block(LocalBlock::new(x, 3, z), STONE);
                buffer.set_block(LocalBlock::new(x, 4, z), STONE);
            }
        }
        // Carve the bottom cell layer of the upper block: a one-cell gap.
        buffer.set_subnode(LocalBlock::new(5, 4, 5), 1, 0, 1, MaterialId::AIR);
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 4, 5), 1, 0, 1),
            GRASS,
            "the gap gets a run of one"
        );
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 4, 5), 1, 1, 1),
            STONE,
            "the stone over the gap is kept"
        );
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
