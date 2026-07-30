// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Palette-compressed storage for a 16³-block chunk.
//!
//! # Why a palette
//!
//! A chunk holds 4096 blocks, and a real one holds very few *distinct* blocks:
//! solid stone, air, and a handful of surfaces. Storing 4096 copies of a 12-byte
//! [`BlockContent`] would be 48 KiB for something describable in a few dozen
//! bytes. Instead the chunk keeps a palette of the distinct contents present and
//! 4096 indices into it, packed to exactly the width the palette needs (see
//! [`crate::bitpack`]). A chunk of solid stone needs one palette entry and *no*
//! index storage whatsoever.
//!
//! # Three layers of storage
//!
//! 1. **Palette** — the distinct [`BlockContent`]s in this chunk, reference
//!    counted so an entry can be reclaimed when its last block changes.
//! 2. **Indices** — one bit-packed palette index per block.
//! 3. **Mixed table** — the `[MaterialId; 27]` arrays that
//!    [`BlockContent::Mixed`] entries point at, interned so equal contents share
//!    one slot.
//!
//! # Canonical form is an invariant, not a convention
//!
//! Everything stored here has been through [`BlockValue::canonical`]. That means
//! a given world state has exactly one representation, which is what makes
//! chunk equality, the persistence round-trip, and the cross-platform
//! determinism hash well defined. A `Partial` with a full mask must never reach
//! the palette, because then two chunks with identical contents could hash
//! differently depending on the order their blocks were written.

use std::collections::BTreeMap;

use crate::bitpack::BitArray;
use crate::block::{BlockContent, BlockValue, BlockView, Cells, SlotIndex};
use crate::coords::{BlockPos, ChunkPos, LocalBlock, SubNodePos};
use crate::material::MaterialId;
use crate::{BLOCKS_PER_CHUNK, CHUNK_SUBNODES, block};

#[cfg(test)]
use crate::block::SUBNODES_PER_BLOCK;

/// A position was addressed in a chunk that does not contain it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("position is not inside chunk ({}, {}, {})", chunk.x, chunk.y, chunk.z)]
pub struct NotInChunk {
    /// The chunk that was addressed.
    pub chunk: ChunkPos,
}

/// One distinct block content, with the number of blocks using it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaletteEntry {
    content: BlockContent,
    /// Blocks referring to this entry. Zero means the slot is free for reuse.
    refs: u32,
}

/// Interned `[MaterialId; 27]` arrays for [`BlockContent::Mixed`] blocks.
///
/// Interning means two blocks with identical mixed contents share a slot, so a
/// palette comparison by slot index is also a comparison by content and the
/// palette never holds two entries describing the same thing.
///
/// The lookup index is keyed by a fixed FNV-1a hash rather than by the 54-byte
/// cell array, which would otherwise be stored twice. A hash collision between
/// two genuinely different cell arrays costs a missed deduplication and nothing
/// else — the arrays are still stored and read correctly, there is simply one
/// more slot than strictly necessary. At 64 bits that is not a case that will
/// occur, but it is handled rather than assumed away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MixedTable {
    cells: Vec<Cells>,
    free: Vec<u16>,
    /// hash → the one slot that hash currently resolves to.
    lookup: BTreeMap<u64, u16>,
    live: u32,
}

impl MixedTable {
    /// FNV-1a over the cell array. Fixed constants, no seed: iteration and
    /// results must not vary between processes (charter rule 4).
    fn hash(cells: &Cells) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for cell in cells {
            for byte in cell.get().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }

    fn get(&self, slot: SlotIndex) -> &Cells {
        &self.cells[slot.get()]
    }

    /// Slot holding exactly these cells, if one is already interned.
    fn find(&self, cells: &Cells) -> Option<SlotIndex> {
        let slot = *self.lookup.get(&Self::hash(cells))?;
        // Confirm rather than trust: a collision must not alias two blocks onto
        // one another's contents.
        if self.cells[slot as usize] == *cells {
            Some(SlotIndex(slot))
        } else {
            None
        }
    }

    /// Interns `cells`, reusing an existing slot when possible.
    fn insert(&mut self, cells: Cells) -> SlotIndex {
        if let Some(existing) = self.find(&cells) {
            return existing;
        }

        let slot = if let Some(reused) = self.free.pop() {
            self.cells[reused as usize] = cells;
            reused
        } else {
            let slot = u16::try_from(self.cells.len())
                .expect("a chunk has 4096 blocks, so it cannot need more mixed slots than that");
            self.cells.push(cells);
            slot
        };

        self.lookup.insert(Self::hash(&cells), slot);
        self.live += 1;
        SlotIndex(slot)
    }

    fn remove(&mut self, slot: SlotIndex) {
        let hash = Self::hash(&self.cells[slot.get()]);
        // Only clear the lookup if it still points here. Under a collision it
        // may point at a different slot, which must survive.
        if self.lookup.get(&hash) == Some(&slot.0) {
            self.lookup.remove(&hash);
        }
        self.free.push(slot.0);
        self.live -= 1;
    }

    fn memory_usage(&self) -> usize {
        // BTreeMap gives no capacity accounting, so its contribution is
        // estimated: a node holds up to 11 pairs plus edge pointers, and
        // occupancy averages well under full. Two words per live entry is a
        // deliberately generous approximation rather than a measurement.
        const ESTIMATED_BTREE_BYTES_PER_ENTRY: usize = 2 * size_of::<u64>() + size_of::<u16>();

        self.cells.capacity() * size_of::<Cells>()
            + self.free.capacity() * size_of::<u16>()
            + self.live as usize * ESTIMATED_BTREE_BYTES_PER_ENTRY
    }
}

/// A chunk's serialised parts: exactly its internal state, nothing derived.
///
/// The mixed table's lookup index is deliberately absent — it is derived from
/// the cells and is rebuilt on load. Serialising derived data invites it to
/// disagree with what it was derived from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChunkParts {
    pub palette: Vec<(BlockContent, u32)>,
    pub bits_per_index: u8,
    pub index_words: Vec<u64>,
    pub mixed_cells: Vec<Cells>,
    pub mixed_free: Vec<u16>,
}

/// A serialised chunk failed validation.
///
/// Every variant is reachable from a corrupt, truncated, or hand-edited world
/// file. None of them is a bug in the engine, and none may panic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CorruptChunk {
    /// The palette holds no entries at all.
    #[error("chunk palette is empty")]
    EmptyPalette,

    /// The palette claims more entries than a chunk has blocks.
    #[error("chunk palette has {len} entries, more than the 4096 blocks that could use them")]
    PaletteTooLarge {
        /// Entries claimed.
        len: usize,
    },

    /// The mixed table claims more slots than `u16` can address.
    #[error("mixed table has {len} slots, more than a u16 slot index can address")]
    MixedTableTooLarge {
        /// Slots claimed.
        len: usize,
    },

    /// The index width is too narrow for the palette.
    #[error("index width {bits} cannot address a palette of {palette} entries")]
    IndexTooNarrow {
        /// Stored width.
        bits: u8,
        /// Palette length.
        palette: usize,
    },

    /// The packed index array is malformed.
    #[error("packed block indices are malformed")]
    Indices(#[source] crate::bitpack::BitArrayError),

    /// A block index points past the end of the palette.
    #[error("block {block} indexes palette slot {slot}, but the palette has {palette} entries")]
    IndexOutOfRange {
        /// Offending block.
        block: usize,
        /// Slot it referenced.
        slot: usize,
        /// Palette length.
        palette: usize,
    },

    /// A block index points at a reclaimed palette slot.
    #[error("block {block} indexes palette slot {slot}, which has no references")]
    IndexToFreeSlot {
        /// Offending block.
        block: usize,
        /// Slot it referenced.
        slot: usize,
    },

    /// A palette entry's refcount disagrees with the indices.
    #[error("palette slot {slot} claims {stored} references but {counted} blocks use it")]
    RefcountMismatch {
        /// Offending slot.
        slot: usize,
        /// Refcount as stored.
        stored: u32,
        /// Refcount as counted from the indices.
        counted: u32,
    },

    /// A `Mixed` entry points at a missing or freed side-table slot.
    #[error("palette slot {slot} references mixed slot {mixed}, which is absent or freed")]
    DanglingMixedSlot {
        /// Offending palette slot.
        slot: usize,
        /// Mixed slot it referenced.
        mixed: u16,
    },

    /// A stored `Partial` is not in canonical form.
    #[error("palette slot {slot} holds a non-canonical Partial")]
    NonCanonicalPartial {
        /// Offending slot.
        slot: usize,
    },
}

/// A palette-compressed 16³-block chunk.
///
/// See the [module documentation](self) for the storage design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pos: ChunkPos,
    palette: Vec<PaletteEntry>,
    /// Palette slots with a zero refcount, available for reuse.
    free: Vec<u16>,
    live_entries: u32,
    indices: BitArray,
    mixed: MixedTable,
}

impl Chunk {
    /// A chunk entirely filled with one material.
    ///
    /// Allocates a single palette entry and no index storage at all.
    #[must_use]
    pub fn new(pos: ChunkPos, fill: MaterialId) -> Self {
        // `vec![]` rather than pushing onto an empty Vec: `Vec::push` rounds a
        // first allocation up to 4 elements for small types, which would
        // quadruple a uniform chunk's footprint for no benefit. The macro
        // allocates exactly one.
        let palette = vec![PaletteEntry {
            content: BlockContent::Uniform(fill),
            refs: BLOCKS_PER_CHUNK as u32,
        }];

        Self {
            pos,
            palette,
            free: Vec::new(),
            live_entries: 1,
            indices: BitArray::new(BLOCKS_PER_CHUNK, 0),
            mixed: MixedTable::default(),
        }
    }

    /// An empty chunk.
    #[must_use]
    pub fn air(pos: ChunkPos) -> Self {
        Self::new(pos, MaterialId::AIR)
    }

    /// This chunk's position.
    #[must_use]
    pub const fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Whether a block lies in this chunk.
    #[must_use]
    pub fn contains_block(&self, pos: BlockPos) -> bool {
        pos.chunk() == self.pos
    }

    /// Whether a sub-node lies in this chunk.
    #[must_use]
    pub fn contains_subnode(&self, pos: SubNodePos) -> bool {
        pos.chunk() == self.pos
    }

    // -- reads ------------------------------------------------------------

    /// The block at a world position, or `None` if it is in another chunk.
    #[must_use]
    pub fn get_block(&self, pos: BlockPos) -> Option<BlockView<'_>> {
        self.contains_block(pos)
            .then(|| self.get_block_local(pos.local()))
    }

    /// The block at a chunk-local position.
    ///
    /// The hot path — meshing, lighting, and physics all walk chunks locally
    /// and have already established that they are in bounds.
    #[must_use]
    pub fn get_block_local(&self, local: LocalBlock) -> BlockView<'_> {
        self.view(self.content_at(local.index()))
    }

    /// The material of a sub-node at a world position, or `None` if it is in
    /// another chunk.
    #[must_use]
    pub fn get_subnode(&self, pos: SubNodePos) -> Option<MaterialId> {
        if !self.contains_subnode(pos) {
            return None;
        }
        let (x, y, z) = pos.local();
        Some(
            self.get_block_local(pos.block().local())
                .subnode_at(x, y, z),
        )
    }

    /// Whether every block in this chunk is the same uniform material.
    ///
    /// `None` if the chunk holds anything else, including a single partially
    /// chiselled block.
    #[must_use]
    pub fn is_uniform(&self) -> Option<MaterialId> {
        if self.live_entries != 1 {
            return None;
        }
        match self.palette.iter().find(|entry| entry.refs > 0)?.content {
            BlockContent::Uniform(material) => Some(material),
            BlockContent::Partial { .. } | BlockContent::Mixed(_) => None,
        }
    }

    /// Iterates every block in the chunk in ascending
    /// [`LocalBlock::index`] order.
    pub fn blocks(&self) -> impl Iterator<Item = (LocalBlock, BlockView<'_>)> + '_ {
        (0..BLOCKS_PER_CHUNK).map(move |index| {
            (
                LocalBlock::from_index(index),
                self.view(self.content_at(index)),
            )
        })
    }

    /// The 27 cells of a block, expanded.
    #[must_use]
    pub fn block_cells(&self, local: LocalBlock) -> Cells {
        self.get_block_local(local).cells()
    }

    // -- writes -----------------------------------------------------------

    /// Sets the block at a world position.
    ///
    /// The value is canonicalised before storage, so callers need not
    /// pre-reduce a full `Partial` or a single-material `Cells`.
    ///
    /// # Errors
    ///
    /// [`NotInChunk`] if the position is in another chunk.
    pub fn set_block(&mut self, pos: BlockPos, value: BlockValue) -> Result<(), NotInChunk> {
        if !self.contains_block(pos) {
            return Err(NotInChunk { chunk: self.pos });
        }
        self.set_block_local(pos.local(), value);
        Ok(())
    }

    /// Sets the block at a chunk-local position.
    pub fn set_block_local(&mut self, local: LocalBlock, value: BlockValue) {
        let value = value.canonical();
        let index = local.index();

        // Intern first: this may widen the index array, and the old index must
        // be read at the final width.
        let new_slot = self.intern(value);
        let old_slot = self.indices.get(index) as u16;

        if old_slot == new_slot {
            // No change. Undo the reference `intern` optimistically took.
            self.palette[new_slot as usize].refs -= 1;
            return;
        }

        self.indices.set(index, u32::from(new_slot));
        self.release(old_slot);
    }

    /// Sets a single sub-node at a world position.
    ///
    /// # Errors
    ///
    /// [`NotInChunk`] if the position is in another chunk.
    pub fn set_subnode(&mut self, pos: SubNodePos, material: MaterialId) -> Result<(), NotInChunk> {
        if !self.contains_subnode(pos) {
            return Err(NotInChunk { chunk: self.pos });
        }
        let local = pos.block().local();
        let (x, y, z) = pos.local();
        let cell = block::subnode_index(x, y, z);

        let mut cells = self.block_cells(local);
        if cells[cell] == material {
            return Ok(());
        }
        cells[cell] = material;
        self.set_block_local(local, BlockValue::Cells(cells));
        Ok(())
    }

    /// Fills an inclusive sub-node region with one material.
    ///
    /// The region is given in world sub-node coordinates and clipped to this
    /// chunk, so a caller can pass a region spanning several chunks and hand
    /// the same rectangle to each without clipping it themselves.
    ///
    /// Blocks entirely inside the region take the uniform fast path rather than
    /// being written cell by cell.
    pub fn fill_region(&mut self, from: SubNodePos, to: SubNodePos, material: MaterialId) {
        let origin = self.subnode_origin();
        let min_x = from.x.min(to.x).max(origin.x);
        let min_y = from.y.min(to.y).max(origin.y);
        let min_z = from.z.min(to.z).max(origin.z);
        let extent = CHUNK_SUBNODES as i32 - 1;
        let max_x = from.x.max(to.x).min(origin.x + extent);
        let max_y = from.y.max(to.y).min(origin.y + extent);
        let max_z = from.z.max(to.z).min(origin.z + extent);

        if min_x > max_x || min_y > max_y || min_z > max_z {
            return;
        }

        // Walk blocks, not sub-nodes: a region covering whole blocks should
        // cost one palette write per block rather than 27.
        let block_min = SubNodePos::new(min_x, min_y, min_z).block().local();
        let block_max = SubNodePos::new(max_x, max_y, max_z).block().local();

        for bz in block_max.z.min(block_min.z)..=block_max.z.max(block_min.z) {
            for by in block_max.y.min(block_min.y)..=block_max.y.max(block_min.y) {
                for bx in block_max.x.min(block_min.x)..=block_max.x.max(block_min.x) {
                    let local = LocalBlock::new(bx, by, bz);
                    let block_origin = self.block_subnode_origin(local);

                    // The region's overlap with this block, in block-local
                    // sub-node offsets.
                    let lo_x = (min_x - block_origin.x).max(0) as u32;
                    let hi_x = (max_x - block_origin.x).min(2) as u32;
                    let lo_y = (min_y - block_origin.y).max(0) as u32;
                    let hi_y = (max_y - block_origin.y).min(2) as u32;
                    let lo_z = (min_z - block_origin.z).max(0) as u32;
                    let hi_z = (max_z - block_origin.z).min(2) as u32;

                    let covers_whole_block =
                        lo_x == 0 && hi_x == 2 && lo_y == 0 && hi_y == 2 && lo_z == 0 && hi_z == 2;

                    if covers_whole_block {
                        self.set_block_local(local, BlockValue::Uniform(material));
                        continue;
                    }

                    let mut cells = self.block_cells(local);
                    for z in lo_z..=hi_z {
                        for y in lo_y..=hi_y {
                            for x in lo_x..=hi_x {
                                cells[block::subnode_index(x, y, z)] = material;
                            }
                        }
                    }
                    self.set_block_local(local, BlockValue::Cells(cells));
                }
            }
        }
    }

    // -- palette management -----------------------------------------------

    /// Number of live palette entries.
    #[must_use]
    pub fn palette_len(&self) -> usize {
        self.live_entries as usize
    }

    /// Number of interned mixed-cell slots.
    #[must_use]
    pub fn mixed_len(&self) -> usize {
        self.mixed.live as usize
    }

    /// Bits currently used per block index.
    #[must_use]
    pub fn bits_per_index(&self) -> u8 {
        self.indices.bits_per_entry()
    }

    /// Compacts the palette and mixed table, dropping unreferenced entries and
    /// narrowing the index array to the smallest width that still fits.
    ///
    /// Content-preserving: every block reads back exactly as it did before.
    /// Called automatically whenever a removal makes a narrower index width
    /// possible, so explicit calls are only needed after bulk edits where the
    /// caller wants the memory back immediately.
    pub fn repack(&mut self) {
        let mut remap = vec![u16::MAX; self.palette.len()];
        let mut packed = Vec::with_capacity(self.live_entries as usize);
        let mut mixed = MixedTable::default();

        for (old_slot, entry) in self.palette.iter().enumerate() {
            if entry.refs == 0 {
                continue;
            }
            // Re-intern mixed cells into a fresh table so its backing Vec
            // shrinks too. Without this, a chunk that was heavily chiselled and
            // then filled in would keep the whole mixed table forever.
            let content = match entry.content {
                BlockContent::Mixed(slot) => {
                    BlockContent::Mixed(mixed.insert(*self.mixed.get(slot)))
                }
                other => other,
            };
            // `packed` only ever gains live entries, of which there are at most
            // one per block, so this cannot exceed 4096 and cannot truncate.
            remap[old_slot] = packed.len() as u16;
            packed.push(PaletteEntry {
                content,
                refs: entry.refs,
            });
        }

        let bits = BitArray::bits_for(packed.len());
        if bits == self.indices.bits_per_entry() {
            self.indices.remap(|slot| u32::from(remap[slot as usize]));
        } else {
            let mut narrowed = BitArray::new(BLOCKS_PER_CHUNK, bits);
            if bits > 0 {
                for index in 0..BLOCKS_PER_CHUNK {
                    let slot = self.indices.get(index) as usize;
                    narrowed.set(index, u32::from(remap[slot]));
                }
            }
            self.indices = narrowed;
        }

        packed.shrink_to_fit();
        self.palette = packed;
        self.free = Vec::new();
        self.mixed = mixed;
    }

    /// Heap bytes used by this chunk's content storage.
    ///
    /// Covers the palette, the packed index array, and the mixed-cell table —
    /// the three things that scale with contents. The chunk's own fields are
    /// stack-resident and excluded.
    ///
    /// The mixed table's lookup index is a [`BTreeMap`], which exposes no
    /// capacity accounting, so its contribution is a documented estimate rather
    /// than a measurement; see [`MixedTable::memory_usage`]. Everything else is
    /// exact.
    ///
    /// Measured sizes for representative chunks, from
    /// `chunk::tests::documented_memory_sizes`:
    ///
    /// | Chunk | Palette | Bits/index | Bytes |
    /// |---|---|---|---|
    /// | Uniform (solid or air) | 1 | 0 | 12 |
    /// | Two materials, split in half | 2 | 1 | 560 |
    /// | Flat terrain, 4 layers | 4 | 2 | 1,072 |
    /// | One chiselled block in solid stone | 2 | 1 | 560 |
    /// | Every block a distinct partial mask | 4096 | 12 | 55,296 |
    /// | Every block a distinct 2-material mix | 4096 | 12 | 350,208 |
    ///
    /// For scale, an uncompressed chunk storing 27 `MaterialId`s per block
    /// unconditionally would be 221,184 bytes regardless of contents. Ordinary
    /// terrain therefore costs well under 1% of that, and even the all-partial
    /// pathological case is a quarter of it.
    ///
    /// The last two rows are the pathological cases Task 02b exists to measure;
    /// they are not shapes real terrain produces. Note that the all-mixed case
    /// is the only one that exceeds the uncompressed size — a block needing all
    /// 27 cells stored individually costs the 54 bytes plus palette and
    /// interning overhead. That number is an input to 02b's keep /
    /// keep-with-limits / fallback decision, not a bug.
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        self.palette.capacity() * size_of::<PaletteEntry>()
            + self.free.capacity() * size_of::<u16>()
            + self.indices.memory_usage()
            + self.mixed.memory_usage()
    }

    // -- serialisation ----------------------------------------------------

    /// Exposes the exact internal state for serialisation.
    ///
    /// Crate-private: this is the persistence layer's business and nobody
    /// else's. Callers outside the crate work through the public block API.
    pub(crate) fn to_parts(&self) -> ChunkParts {
        ChunkParts {
            palette: self
                .palette
                .iter()
                .map(|entry| (entry.content, entry.refs))
                .collect(),
            bits_per_index: self.indices.bits_per_entry(),
            index_words: self.indices.words().to_vec(),
            mixed_cells: self.mixed.cells.clone(),
            mixed_free: self.mixed.free.clone(),
        }
    }

    /// Rebuilds a chunk from serialised parts, validating every invariant.
    ///
    /// **Everything here is reachable from a corrupt or hand-edited world file,
    /// so nothing may panic and nothing may be trusted.** A palette index
    /// pointing past the palette, a mixed slot pointing past the table, or
    /// refcounts that do not match the indices would each produce a chunk that
    /// panics on the next read — long after the bad data was loaded, and
    /// nowhere near it.
    ///
    /// # Errors
    ///
    /// [`CorruptChunk`] describing the first inconsistency found.
    pub(crate) fn from_parts(pos: ChunkPos, parts: ChunkParts) -> Result<Self, CorruptChunk> {
        let ChunkParts {
            palette,
            bits_per_index,
            index_words,
            mixed_cells,
            mixed_free,
        } = parts;

        if palette.is_empty() {
            return Err(CorruptChunk::EmptyPalette);
        }
        if palette.len() > BLOCKS_PER_CHUNK {
            return Err(CorruptChunk::PaletteTooLarge { len: palette.len() });
        }

        let needed = BitArray::bits_for(palette.len());
        if bits_per_index < needed {
            return Err(CorruptChunk::IndexTooNarrow {
                bits: bits_per_index,
                palette: palette.len(),
            });
        }

        let indices = BitArray::from_words(BLOCKS_PER_CHUNK, bits_per_index, index_words)
            .map_err(CorruptChunk::Indices)?;

        let (mixed, free) = Self::rebuild_mixed_table(mixed_cells, mixed_free)?;
        let live_entries = Self::validate_palette(&palette, &mixed, &free)?;
        Self::validate_indices(&palette, &indices)?;

        let free_palette = palette
            .iter()
            .enumerate()
            .filter(|(_, (_, refs))| *refs == 0)
            .map(|(slot, _)| slot as u16)
            .collect();

        Ok(Self {
            pos,
            palette: palette
                .into_iter()
                .map(|(content, refs)| PaletteEntry { content, refs })
                .collect(),
            free: free_palette,
            live_entries,
            indices,
            mixed,
        })
    }

    /// Rebuilds the mixed table's derived lookup, returning it and its free set.
    fn rebuild_mixed_table(
        cells: Vec<Cells>,
        free_list: Vec<u16>,
    ) -> Result<(MixedTable, std::collections::BTreeSet<u16>), CorruptChunk> {
        // Derived from the cells rather than trusted from the file: a
        // serialised lookup could disagree with what it was derived from, and
        // interning would silently stop working.
        let mut mixed = MixedTable {
            cells,
            free: free_list,
            lookup: BTreeMap::new(),
            live: 0,
        };
        let free: std::collections::BTreeSet<u16> = mixed.free.iter().copied().collect();

        for slot in 0..mixed.cells.len() {
            let slot = u16::try_from(slot).map_err(|_| CorruptChunk::MixedTableTooLarge {
                len: mixed.cells.len(),
            })?;
            if free.contains(&slot) {
                continue;
            }
            mixed
                .lookup
                .insert(MixedTable::hash(&mixed.cells[slot as usize]), slot);
            mixed.live += 1;
        }

        Ok((mixed, free))
    }

    /// Checks every live palette entry, returning how many there are.
    fn validate_palette(
        palette: &[(BlockContent, u32)],
        mixed: &MixedTable,
        free: &std::collections::BTreeSet<u16>,
    ) -> Result<u32, CorruptChunk> {
        let mut live_entries = 0;
        for (slot, (content, refs)) in palette.iter().enumerate() {
            if *refs == 0 {
                continue;
            }
            live_entries += 1;
            match content {
                BlockContent::Uniform(_) => {}
                BlockContent::Partial {
                    material,
                    occupancy,
                } => {
                    // Non-canonical content on disk would give one world state
                    // two representations, which is exactly what canonical form
                    // exists to prevent (see the module docs).
                    if material.is_air()
                        || *occupancy == 0
                        || *occupancy == block::OCCUPANCY_FULL
                        || *occupancy & !block::OCCUPANCY_FULL != 0
                    {
                        return Err(CorruptChunk::NonCanonicalPartial { slot });
                    }
                }
                BlockContent::Mixed(mixed_slot) => {
                    if mixed_slot.get() >= mixed.cells.len() || free.contains(&mixed_slot.0) {
                        return Err(CorruptChunk::DanglingMixedSlot {
                            slot,
                            mixed: mixed_slot.0,
                        });
                    }
                }
            }
        }
        Ok(live_entries)
    }

    /// Checks that every index addresses a live entry and that the stored
    /// refcounts are exactly what the indices imply.
    fn validate_indices(
        palette: &[(BlockContent, u32)],
        indices: &BitArray,
    ) -> Result<(), CorruptChunk> {
        let mut counted_refs = vec![0u32; palette.len()];

        for index in 0..BLOCKS_PER_CHUNK {
            let slot = indices.get(index) as usize;
            if slot >= palette.len() {
                return Err(CorruptChunk::IndexOutOfRange {
                    block: index,
                    slot,
                    palette: palette.len(),
                });
            }
            if palette[slot].1 == 0 {
                return Err(CorruptChunk::IndexToFreeSlot { block: index, slot });
            }
            counted_refs[slot] += 1;
        }

        for (slot, ((_, refs), counted)) in palette.iter().zip(&counted_refs).enumerate() {
            if *refs != *counted {
                return Err(CorruptChunk::RefcountMismatch {
                    slot,
                    stored: *refs,
                    counted: *counted,
                });
            }
        }
        Ok(())
    }

    // -- internals --------------------------------------------------------

    fn content_at(&self, index: usize) -> BlockContent {
        self.palette[self.indices.get(index) as usize].content
    }

    fn view(&self, content: BlockContent) -> BlockView<'_> {
        match content {
            BlockContent::Uniform(material) => BlockView::Uniform(material),
            BlockContent::Partial {
                material,
                occupancy,
            } => BlockView::Partial {
                material,
                occupancy,
            },
            BlockContent::Mixed(slot) => BlockView::Mixed(self.mixed.get(slot)),
        }
    }

    fn subnode_origin(&self) -> SubNodePos {
        SubNodePos::new(
            self.pos.x * CHUNK_SUBNODES as i32,
            self.pos.y * CHUNK_SUBNODES as i32,
            self.pos.z * CHUNK_SUBNODES as i32,
        )
    }

    fn block_subnode_origin(&self, local: LocalBlock) -> SubNodePos {
        let origin = self.subnode_origin();
        SubNodePos::new(
            origin.x + (local.x * crate::SUBNODES_PER_AXIS) as i32,
            origin.y + (local.y * crate::SUBNODES_PER_AXIS) as i32,
            origin.z + (local.z * crate::SUBNODES_PER_AXIS) as i32,
        )
    }

    /// Finds or creates the palette slot for `value`, taking a reference to it.
    ///
    /// `value` must already be canonical.
    fn intern(&mut self, value: BlockValue) -> u16 {
        let content = match value {
            BlockValue::Uniform(material) => BlockContent::Uniform(material),
            BlockValue::Partial {
                material,
                occupancy,
            } => BlockContent::Partial {
                material,
                occupancy,
            },
            BlockValue::Cells(cells) => {
                // Reuse an interned slot if these exact cells are already
                // present; the matching palette entry then also exists, because
                // slots and Mixed palette entries are created and destroyed
                // together.
                match self.mixed.find(&cells) {
                    Some(slot) => BlockContent::Mixed(slot),
                    None => BlockContent::Mixed(self.mixed.insert(cells)),
                }
            }
        };

        if let Some(slot) = self
            .palette
            .iter()
            .position(|entry| entry.refs > 0 && entry.content == content)
        {
            self.palette[slot].refs += 1;
            return slot as u16;
        }

        let slot = if let Some(reused) = self.free.pop() {
            self.palette[reused as usize] = PaletteEntry { content, refs: 1 };
            reused
        } else {
            let slot = u16::try_from(self.palette.len())
                .expect("a chunk has 4096 blocks, so its palette cannot exceed 4096 entries");
            self.palette.push(PaletteEntry { content, refs: 1 });
            self.grow_indices_if_needed();
            slot
        };

        self.live_entries += 1;
        slot
    }

    /// Drops a reference, reclaiming the entry when it reaches zero.
    fn release(&mut self, slot: u16) {
        let entry = &mut self.palette[slot as usize];
        entry.refs -= 1;
        if entry.refs > 0 {
            return;
        }

        // A mixed slot's lifetime is its palette entry's: interning guarantees
        // exactly one entry per distinct cell array, so this is the last user.
        if let BlockContent::Mixed(mixed_slot) = entry.content {
            self.mixed.remove(mixed_slot);
        }

        self.free.push(slot);
        self.live_entries -= 1;

        // Narrowing is worth a repack; it halves or better the index array,
        // which is the largest allocation in most chunks. Crossing a power-of-
        // two boundary downward is rare, so this is not a per-write cost.
        if BitArray::bits_for(self.live_entries as usize) < self.indices.bits_per_entry() {
            self.repack();
        }
    }

    fn grow_indices_if_needed(&mut self) {
        let needed = BitArray::bits_for(self.palette.len());
        if needed > self.indices.bits_per_entry() {
            self.indices = self.indices.resized(needed);
        }
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::air(ChunkPos::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);
    const GRASS: MaterialId = MaterialId(4);

    fn origin() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    #[test]
    fn a_new_chunk_is_uniform_and_costs_almost_nothing() {
        let chunk = Chunk::new(origin(), STONE);
        assert_eq!(chunk.is_uniform(), Some(STONE));
        assert_eq!(chunk.palette_len(), 1);
        assert_eq!(
            chunk.bits_per_index(),
            0,
            "one entry needs no index storage"
        );
        assert_eq!(chunk.memory_usage(), 12);
    }

    #[test]
    fn uniform_chunk_meets_the_memory_bound() {
        // Acceptance criterion: a uniform chunk uses at most 64 bytes of
        // content storage.
        for material in [MaterialId::AIR, STONE] {
            let chunk = Chunk::new(origin(), material);
            assert!(
                chunk.memory_usage() <= 64,
                "uniform chunk used {} bytes",
                chunk.memory_usage()
            );
        }
    }

    #[test]
    fn every_block_reads_back_the_fill() {
        let chunk = Chunk::new(origin(), STONE);
        for (_, view) in chunk.blocks() {
            assert_eq!(view, BlockView::Uniform(STONE));
        }
        assert_eq!(chunk.blocks().count(), BLOCKS_PER_CHUNK);
    }

    #[test]
    fn palette_grows_and_index_width_follows() {
        let mut chunk = Chunk::air(origin());
        assert_eq!(chunk.bits_per_index(), 0);

        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Uniform(STONE));
        assert_eq!(chunk.palette_len(), 2);
        assert_eq!(chunk.bits_per_index(), 1);

        chunk.set_block_local(LocalBlock::new(1, 0, 0), BlockValue::Uniform(DIRT));
        assert_eq!(chunk.palette_len(), 3);
        assert_eq!(chunk.bits_per_index(), 2);

        chunk.set_block_local(LocalBlock::new(2, 0, 0), BlockValue::Uniform(GRASS));
        assert_eq!(chunk.palette_len(), 4);
        assert_eq!(chunk.bits_per_index(), 2);
    }

    #[test]
    fn palette_shrinks_when_the_last_user_of_an_entry_goes_away() {
        let mut chunk = Chunk::air(origin());
        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Uniform(STONE));
        chunk.set_block_local(LocalBlock::new(1, 0, 0), BlockValue::Uniform(DIRT));
        chunk.set_block_local(LocalBlock::new(2, 0, 0), BlockValue::Uniform(GRASS));
        assert_eq!(chunk.palette_len(), 4);
        assert_eq!(chunk.bits_per_index(), 2);

        // Remove grass and dirt: two live entries left, so one bit suffices and
        // the index array should have narrowed automatically.
        chunk.set_block_local(LocalBlock::new(2, 0, 0), BlockValue::AIR);
        chunk.set_block_local(LocalBlock::new(1, 0, 0), BlockValue::AIR);
        assert_eq!(chunk.palette_len(), 2);
        assert_eq!(chunk.bits_per_index(), 1);

        // And back to uniform.
        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::AIR);
        assert_eq!(chunk.palette_len(), 1);
        assert_eq!(chunk.bits_per_index(), 0);
        assert_eq!(chunk.is_uniform(), Some(MaterialId::AIR));
    }

    #[test]
    fn subnode_round_trips_across_all_twenty_seven_positions() {
        let mut chunk = Chunk::air(origin());
        let block = BlockPos::new(4, 5, 6);

        for index in 0..SUBNODES_PER_BLOCK {
            let (x, y, z) = block::subnode_offset(index);
            let pos = block.subnode(x as i32, y as i32, z as i32);
            chunk.set_subnode(pos, STONE).expect("in chunk");
            assert_eq!(chunk.get_subnode(pos), Some(STONE), "set at index {index}");
        }

        // All 27 set, so the block must have collapsed back to Uniform.
        assert_eq!(
            chunk.get_block(block),
            Some(BlockView::Uniform(STONE)),
            "a fully occupied block must canonicalise to Uniform"
        );

        for index in 0..SUBNODES_PER_BLOCK {
            let (x, y, z) = block::subnode_offset(index);
            let pos = block.subnode(x as i32, y as i32, z as i32);
            chunk.set_subnode(pos, MaterialId::AIR).expect("in chunk");
            assert_eq!(chunk.get_subnode(pos), Some(MaterialId::AIR));
        }
        assert_eq!(chunk.is_uniform(), Some(MaterialId::AIR));
    }

    #[test]
    fn a_single_subnode_write_produces_a_partial_not_a_mixed() {
        let mut chunk = Chunk::air(origin());
        chunk
            .set_subnode(SubNodePos::new(0, 0, 0), STONE)
            .expect("in chunk");

        assert_eq!(
            chunk.get_block(BlockPos::new(0, 0, 0)),
            Some(BlockView::Partial {
                material: STONE,
                occupancy: 1
            })
        );
        assert_eq!(chunk.mixed_len(), 0, "no mixed slot should be needed");
    }

    #[test]
    fn two_materials_in_one_block_produce_a_mixed_slot() {
        let mut chunk = Chunk::air(origin());
        chunk
            .set_subnode(SubNodePos::new(0, 0, 0), STONE)
            .expect("in chunk");
        chunk
            .set_subnode(SubNodePos::new(1, 0, 0), DIRT)
            .expect("in chunk");

        assert_eq!(chunk.mixed_len(), 1);
        let view = chunk.get_block(BlockPos::new(0, 0, 0)).expect("in chunk");
        assert_eq!(view.subnode(0), STONE);
        assert_eq!(view.subnode(1), DIRT);
        assert_eq!(view.subnode(2), MaterialId::AIR);
    }

    #[test]
    fn identical_mixed_blocks_share_one_slot() {
        let mut chunk = Chunk::air(origin());
        let mut cells = block::EMPTY_CELLS;
        cells[0] = STONE;
        cells[1] = DIRT;

        for x in 0..8 {
            chunk.set_block_local(LocalBlock::new(x, 0, 0), BlockValue::Cells(cells));
        }

        assert_eq!(chunk.mixed_len(), 1, "interning should collapse these");
        assert_eq!(chunk.palette_len(), 2, "air plus the one mixed content");
    }

    #[test]
    fn a_mixed_slot_is_reclaimed_with_its_palette_entry() {
        let mut chunk = Chunk::air(origin());
        let mut cells = block::EMPTY_CELLS;
        cells[0] = STONE;
        cells[1] = DIRT;

        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Cells(cells));
        assert_eq!(chunk.mixed_len(), 1);

        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::AIR);
        assert_eq!(chunk.mixed_len(), 0, "the slot should have been reclaimed");
        assert_eq!(chunk.palette_len(), 1);
    }

    #[test]
    fn writing_the_same_value_twice_is_a_no_op() {
        let mut chunk = Chunk::air(origin());
        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Uniform(STONE));
        let after_first = chunk.clone();
        chunk.set_block_local(LocalBlock::new(0, 0, 0), BlockValue::Uniform(STONE));
        assert_eq!(chunk, after_first, "refcounts must not drift");
    }

    #[test]
    fn set_block_canonicalises_a_full_partial() {
        let mut chunk = Chunk::air(origin());
        chunk.set_block_local(
            LocalBlock::new(0, 0, 0),
            BlockValue::Partial {
                material: STONE,
                occupancy: block::OCCUPANCY_FULL,
            },
        );
        assert_eq!(
            chunk.get_block_local(LocalBlock::new(0, 0, 0)),
            BlockView::Uniform(STONE),
            "a full mask must be stored as Uniform, not as a second representation"
        );
    }

    #[test]
    fn world_coordinates_outside_the_chunk_are_rejected() {
        let mut chunk = Chunk::air(ChunkPos::new(1, 0, 0));
        // Chunk (1,0,0) covers blocks 16..32.
        assert!(chunk.get_block(BlockPos::new(0, 0, 0)).is_none());
        assert!(chunk.get_block(BlockPos::new(16, 0, 0)).is_some());
        assert!(chunk.get_block(BlockPos::new(31, 0, 0)).is_some());
        assert!(chunk.get_block(BlockPos::new(32, 0, 0)).is_none());

        assert!(
            chunk
                .set_block(BlockPos::new(0, 0, 0), BlockValue::Uniform(STONE))
                .is_err()
        );
        assert!(
            chunk
                .set_block(BlockPos::new(16, 0, 0), BlockValue::Uniform(STONE))
                .is_ok()
        );
    }

    #[test]
    fn negative_chunks_address_correctly() {
        let pos = ChunkPos::new(-1, -1, -1);
        let mut chunk = Chunk::air(pos);
        let block = BlockPos::new(-1, -1, -1);
        assert!(chunk.contains_block(block));
        chunk
            .set_block(block, BlockValue::Uniform(STONE))
            .expect("in chunk");
        assert_eq!(chunk.get_block(block), Some(BlockView::Uniform(STONE)));

        // The far corner of the same chunk.
        let corner = BlockPos::new(-16, -16, -16);
        assert!(chunk.contains_block(corner));
        assert_eq!(
            chunk.get_block(corner),
            Some(BlockView::Uniform(MaterialId::AIR))
        );
    }

    #[test]
    fn fill_region_takes_the_whole_block_fast_path() {
        let mut chunk = Chunk::air(origin());
        // Blocks 0..2 on each axis, entirely covered: sub-nodes 0..5.
        chunk.fill_region(SubNodePos::new(0, 0, 0), SubNodePos::new(5, 5, 5), STONE);

        for z in 0..2 {
            for y in 0..2 {
                for x in 0..2 {
                    assert_eq!(
                        chunk.get_block_local(LocalBlock::new(x, y, z)),
                        BlockView::Uniform(STONE),
                        "block ({x},{y},{z}) should be uniform"
                    );
                }
            }
        }
        assert_eq!(chunk.mixed_len(), 0);
        assert_eq!(chunk.palette_len(), 2);
    }

    #[test]
    fn fill_region_handles_partial_block_coverage() {
        let mut chunk = Chunk::air(origin());
        // A single sub-node.
        chunk.fill_region(SubNodePos::new(1, 1, 1), SubNodePos::new(1, 1, 1), STONE);

        let view = chunk.get_block_local(LocalBlock::new(0, 0, 0));
        assert_eq!(
            view,
            BlockView::Partial {
                material: STONE,
                occupancy: 1 << block::subnode_index(1, 1, 1),
            }
        );
    }

    #[test]
    fn fill_region_clips_to_the_chunk() {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        // A region far larger than the chunk, mostly outside it.
        chunk.fill_region(
            SubNodePos::new(-1000, -1000, -1000),
            SubNodePos::new(1000, 1000, 1000),
            STONE,
        );
        assert_eq!(
            chunk.is_uniform(),
            Some(STONE),
            "the whole chunk should fill"
        );
    }

    #[test]
    fn fill_region_entirely_outside_the_chunk_does_nothing() {
        let mut chunk = Chunk::air(origin());
        chunk.fill_region(
            SubNodePos::new(1000, 1000, 1000),
            SubNodePos::new(1010, 1010, 1010),
            STONE,
        );
        assert_eq!(chunk.is_uniform(), Some(MaterialId::AIR));
    }

    #[test]
    fn repack_is_content_identity() {
        let mut chunk = Chunk::air(origin());
        let mut cells = block::EMPTY_CELLS;
        cells[3] = STONE;
        cells[4] = DIRT;

        for x in 0..16 {
            chunk.set_block_local(LocalBlock::new(x, 0, 0), BlockValue::Uniform(STONE));
            chunk.set_block_local(LocalBlock::new(x, 1, 0), BlockValue::Cells(cells));
            chunk.set_block_local(
                LocalBlock::new(x, 2, 0),
                BlockValue::Partial {
                    material: DIRT,
                    occupancy: 0b101,
                },
            );
        }

        let before: Vec<Cells> = chunk.blocks().map(|(_, view)| view.cells()).collect();
        chunk.repack();
        let after: Vec<Cells> = chunk.blocks().map(|(_, view)| view.cells()).collect();
        assert_eq!(before, after, "repack changed chunk contents");
    }

    #[test]
    fn repack_reclaims_the_mixed_table() {
        let mut chunk = Chunk::air(origin());
        // Fill with many distinct mixed blocks, then wipe them.
        for x in 0..16 {
            let mut cells = block::EMPTY_CELLS;
            cells[0] = STONE;
            cells[1] = MaterialId(10 + x as u16);
            chunk.set_block_local(LocalBlock::new(x, 0, 0), BlockValue::Cells(cells));
        }
        assert_eq!(chunk.mixed_len(), 16);
        let peak = chunk.memory_usage();

        for x in 0..16 {
            chunk.set_block_local(LocalBlock::new(x, 0, 0), BlockValue::AIR);
        }
        assert_eq!(chunk.mixed_len(), 0);

        chunk.repack();
        assert!(
            chunk.memory_usage() < peak,
            "repack should release the mixed table: {} vs peak {peak}",
            chunk.memory_usage()
        );
        assert_eq!(chunk.memory_usage(), 12, "back to a uniform chunk's cost");
    }

    #[test]
    fn documented_memory_sizes() {
        // What an uncompressed chunk would cost, unconditionally, for scale.
        const UNCOMPRESSED: usize = BLOCKS_PER_CHUNK * size_of::<Cells>();

        // These numbers are quoted in `Chunk::memory_usage`'s documentation.
        // If one changes, update the table there rather than just the number
        // here — the table is what a reader consults.
        assert_eq!(UNCOMPRESSED, 221_184);

        let uniform = Chunk::new(origin(), STONE);
        assert_eq!(uniform.memory_usage(), 12);

        let mut halves = Chunk::new(origin(), STONE);
        for z in 0..16 {
            for y in 0..8 {
                for x in 0..16 {
                    halves.set_block_local(LocalBlock::new(x, y, z), BlockValue::Uniform(DIRT));
                }
            }
        }
        assert_eq!(halves.palette_len(), 2);
        assert_eq!(halves.memory_usage(), 560);

        let mut layered = Chunk::new(origin(), STONE);
        for z in 0..16 {
            for x in 0..16 {
                layered.set_block_local(LocalBlock::new(x, 13, z), BlockValue::Uniform(DIRT));
                layered.set_block_local(LocalBlock::new(x, 14, z), BlockValue::Uniform(GRASS));
                layered.set_block_local(LocalBlock::new(x, 15, z), BlockValue::AIR);
            }
        }
        assert_eq!(layered.palette_len(), 4);
        assert_eq!(layered.memory_usage(), 1_072);

        let mut chiselled = Chunk::new(origin(), STONE);
        chiselled.set_block_local(
            LocalBlock::new(8, 8, 8),
            BlockValue::Partial {
                material: STONE,
                occupancy: 0b1011,
            },
        );
        assert_eq!(chiselled.memory_usage(), 560);

        // Pathological: every block a distinct partial mask. Task 02b measures
        // whether shapes like this need capping.
        let mut all_partial = Chunk::air(origin());
        for index in 0..BLOCKS_PER_CHUNK {
            all_partial.set_block_local(
                LocalBlock::from_index(index),
                BlockValue::Partial {
                    material: STONE,
                    // Distinct non-empty, non-full 27-bit mask per block.
                    occupancy: (index as u32 + 1) & block::OCCUPANCY_FULL,
                },
            );
        }
        assert_eq!(all_partial.palette_len(), 4096);
        assert_eq!(all_partial.bits_per_index(), 12);
        assert_eq!(all_partial.memory_usage(), 55_296);
        assert!(all_partial.memory_usage() < UNCOMPRESSED);

        // Pathological: every block a distinct multi-material mix. The two
        // varying cells are chosen to give 64 × 64 = 4096 genuinely distinct
        // arrays, so interning cannot collapse any of them.
        let mut all_mixed = Chunk::air(origin());
        for index in 0..BLOCKS_PER_CHUNK {
            let mut cells = [STONE; SUBNODES_PER_BLOCK];
            cells[0] = MaterialId(100 + (index % 64) as u16);
            cells[1] = MaterialId(200 + (index / 64) as u16);
            all_mixed.set_block_local(LocalBlock::from_index(index), BlockValue::Cells(cells));
        }
        assert_eq!(all_mixed.palette_len(), 4096);
        assert_eq!(
            all_mixed.mixed_len(),
            4096,
            "no two blocks should share a slot"
        );
        assert_eq!(all_mixed.bits_per_index(), 12);
        assert_eq!(all_mixed.memory_usage(), 350_208);
    }

    #[test]
    fn a_chunk_of_every_distinct_material_still_addresses_correctly() {
        // 4096 distinct materials needs a 12-bit index, which straddles 64-bit
        // words at most positions — the case the bit packer must get right.
        let mut chunk = Chunk::air(origin());
        for index in 0..BLOCKS_PER_CHUNK {
            chunk.set_block_local(
                LocalBlock::from_index(index),
                BlockValue::Uniform(MaterialId(index as u16 + 2)),
            );
        }
        assert_eq!(chunk.palette_len(), 4096);
        assert_eq!(chunk.bits_per_index(), 12);

        for index in 0..BLOCKS_PER_CHUNK {
            assert_eq!(
                chunk.get_block_local(LocalBlock::from_index(index)),
                BlockView::Uniform(MaterialId(index as u16 + 2)),
                "at block {index}"
            );
        }
    }

    #[test]
    fn is_uniform_rejects_a_chunk_with_one_chiselled_block() {
        let mut chunk = Chunk::new(origin(), STONE);
        assert_eq!(chunk.is_uniform(), Some(STONE));
        chunk
            .set_subnode(SubNodePos::new(0, 0, 0), MaterialId::AIR)
            .expect("in chunk");
        assert_eq!(chunk.is_uniform(), None);
    }
}
