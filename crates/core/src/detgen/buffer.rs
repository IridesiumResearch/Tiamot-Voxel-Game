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

use crate::block::{BlockValue, Cells, EMPTY_CELLS, subnode_index};
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
}

impl ChunkBuffer {
    /// A buffer full of one material, at block resolution.
    #[must_use]
    pub fn new(pos: ChunkPos, fill: MaterialId) -> Self {
        Self {
            pos,
            storage: Storage::Blocks(vec![fill; BLOCKS_PER_CHUNK]),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);

    fn origin() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
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
