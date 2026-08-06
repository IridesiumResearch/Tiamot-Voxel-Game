// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Block content: what occupies a single 1-yard cube.
//!
//! # The canonical sub-node index convention
//!
//! **This is the one place the convention is defined. Everything else in the
//! engine references [`subnode_index`] rather than restating it.**
//!
//! A block is subdivided 3×3×3 into 27 sub-node cells (charter rule 5). A cell
//! at block-local offset `(x, y, z)`, each in `0..3`, has index:
//!
//! ```text
//! index = x + 3 * y + 9 * z
//! ```
//!
//! so the index runs x fastest, then y, then z. Index 0 is the `(0, 0, 0)`
//! corner and index 26 is `(2, 2, 2)`. This is the layout of [`Cells`], and bit
//! `index` of a [`BlockContent::Partial`] occupancy mask refers to the same
//! cell.
//!
//! It matches [`crate::coords::LocalBlock::index`]'s x-major block layout, so
//! blocks and sub-nodes are never laid out differently from one another.
//!
//! # Storage forms and canonicalisation
//!
//! [`BlockContent`] has three forms, chosen so the overwhelmingly common cases
//! cost almost nothing: a whole block of one material is [`BlockContent::Uniform`],
//! a partially chiselled block of one material is [`BlockContent::Partial`]
//! with a 27-bit mask, and only a genuinely multi-material block needs a
//! [`BlockContent::Mixed`] side-table slot.
//!
//! Those three forms overlap — a full `Partial` mask means the same thing as a
//! `Uniform`, and a `Mixed` slot holding 27 copies of one material means the
//! same thing again. Overlap is a correctness hazard: two representations of
//! one world state make equality, hashing, and the persistence round-trip all
//! ambiguous, and a determinism hash over chunk contents would depend on which
//! path last wrote the block. So every write canonicalises, and the invariants
//! in [`BlockValue::canonical`] hold for everything stored in a
//! [`crate::Chunk`].

use crate::UNITS_PER_BLOCK;
use crate::material::MaterialId;

/// Sub-node cells in one block.
pub const SUBNODES_PER_BLOCK: usize = UNITS_PER_BLOCK as usize;

/// An occupancy mask with every sub-node set.
pub const OCCUPANCY_FULL: u32 = (1 << SUBNODES_PER_BLOCK) - 1;

/// An occupancy mask with no sub-node set.
pub const OCCUPANCY_EMPTY: u32 = 0;

/// The 27 sub-node cells of one block, in [`subnode_index`] order.
pub type Cells = [MaterialId; SUBNODES_PER_BLOCK];

/// A block with every cell air.
pub const EMPTY_CELLS: Cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];

/// Index of the sub-node at block-local offset `(x, y, z)`.
///
/// This is the canonical convention — see the [module documentation](self).
/// `index = x + 3 * y + 9 * z`, with each offset in `0..3`.
///
/// # Panics
///
/// In debug builds, if any offset is 3 or greater.
#[must_use]
pub const fn subnode_index(x: u32, y: u32, z: u32) -> usize {
    debug_assert!(x < 3 && y < 3 && z < 3);
    (x + 3 * y + 9 * z) as usize
}

/// Block-local offset of a sub-node index. Inverse of [`subnode_index`].
///
/// # Panics
///
/// In debug builds, if `index` is 27 or greater.
#[must_use]
pub const fn subnode_offset(index: usize) -> (u32, u32, u32) {
    debug_assert!(index < SUBNODES_PER_BLOCK);
    let index = index as u32;
    (index % 3, (index / 3) % 3, index / 9)
}

/// Index into a chunk-local table of [`Cells`].
///
/// Only meaningful alongside the chunk that issued it. Chunks intern their
/// mixed cell arrays, so equal contents share one slot and a
/// [`BlockContent::Mixed`] comparison by slot is also a comparison by content.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct SlotIndex(pub u16);

impl SlotIndex {
    /// The raw index.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

/// How a block's contents are stored inside a chunk.
///
/// This is the *storage* form and it is 12 bytes; [`BlockValue`] is the owned
/// form callers pass in, and [`BlockView`] is the borrowed form they read back.
/// Only canonical values are ever stored — see [`BlockValue::canonical`].
// SERIALISED ON DISK. postcard encodes enum variants by POSITION, so adding a
// variant anywhere but the end silently reinterprets every existing world file.
// New variants go at the bottom, and the change bumps
// `persist::codec::CHUNK_FORMAT_VERSION`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum BlockContent {
    /// Every sub-node is the same material. Includes a block of pure air.
    Uniform(MaterialId),

    /// One material occupying some sub-nodes, air in the rest.
    Partial {
        /// The occupying material. Never [`MaterialId::AIR`] in canonical form.
        material: MaterialId,
        /// 27-bit mask over sub-node positions, indexed by [`subnode_index`].
        /// Never empty and never full in canonical form.
        occupancy: u32,
    },

    /// Two or more distinct materials; the cells live in a chunk-local table.
    Mixed(SlotIndex),
}

impl BlockContent {
    /// A block of air.
    pub const AIR: Self = Self::Uniform(MaterialId::AIR);

    /// Whether this block is entirely air.
    #[must_use]
    pub const fn is_air(self) -> bool {
        matches!(self, Self::Uniform(material) if material.is_air())
    }
}

impl Default for BlockContent {
    fn default() -> Self {
        Self::AIR
    }
}

/// An owned block value, as callers supply it to [`crate::Chunk::set_block`].
///
/// Unlike [`BlockContent`] this carries mixed cells inline rather than
/// referring to a chunk-local slot, so it is meaningful on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockValue {
    /// Every sub-node is the same material.
    Uniform(MaterialId),

    /// One material occupying the masked sub-nodes, air elsewhere.
    Partial {
        /// The occupying material.
        material: MaterialId,
        /// 27-bit mask indexed by [`subnode_index`].
        occupancy: u32,
    },

    /// Explicit per-cell materials.
    Cells(Cells),
}

impl BlockValue {
    /// A block of air.
    pub const AIR: Self = Self::Uniform(MaterialId::AIR);

    /// Reduces to the unique canonical representation of the same contents.
    ///
    /// The invariants, all of which hold for everything a chunk stores:
    ///
    /// - `Partial` with a full mask becomes `Uniform(material)`.
    /// - `Partial` with an empty mask, or with air as its material, becomes
    ///   `Uniform(AIR)` — a mask of air over air is just air.
    /// - `Cells` holding one distinct material collapses to `Uniform`, and
    ///   holding one material plus air collapses to `Partial`.
    /// - Occupancy bits above 27 are discarded rather than trusted.
    ///
    /// Idempotent: canonicalising a canonical value returns it unchanged.
    #[must_use]
    pub fn canonical(self) -> Self {
        match self {
            Self::Uniform(material) => Self::Uniform(material),
            Self::Partial {
                material,
                occupancy,
            } => Self::canonical_partial(material, occupancy),
            Self::Cells(cells) => Self::canonical_cells(&cells),
        }
    }

    fn canonical_partial(material: MaterialId, occupancy: u32) -> Self {
        let occupancy = occupancy & OCCUPANCY_FULL;
        if material.is_air() || occupancy == OCCUPANCY_EMPTY {
            Self::AIR
        } else if occupancy == OCCUPANCY_FULL {
            Self::Uniform(material)
        } else {
            Self::Partial {
                material,
                occupancy,
            }
        }
    }

    fn canonical_cells(cells: &Cells) -> Self {
        // One pass: find the first non-air material and whether any cell
        // disagrees with it. A block is Mixed only if two distinct non-air
        // materials appear.
        let mut solid: Option<MaterialId> = None;
        let mut occupancy = OCCUPANCY_EMPTY;

        for (index, &material) in cells.iter().enumerate() {
            if material.is_air() {
                continue;
            }
            occupancy |= 1 << index;
            match solid {
                None => solid = Some(material),
                Some(existing) if existing == material => {}
                Some(_) => return Self::Cells(*cells),
            }
        }

        match solid {
            None => Self::AIR,
            Some(material) => Self::canonical_partial(material, occupancy),
        }
    }

    /// Whether this value is already in canonical form.
    #[must_use]
    pub fn is_canonical(self) -> bool {
        self.canonical() == self
    }

    /// Expands to explicit per-cell materials.
    #[must_use]
    pub fn cells(&self) -> Cells {
        match self {
            Self::Uniform(material) => [*material; SUBNODES_PER_BLOCK],
            Self::Partial {
                material,
                occupancy,
            } => {
                let mut cells = EMPTY_CELLS;
                for (index, cell) in cells.iter_mut().enumerate() {
                    if occupancy & (1 << index) != 0 {
                        *cell = *material;
                    }
                }
                cells
            }
            Self::Cells(cells) => *cells,
        }
    }

    /// Count of non-air sub-nodes, in units.
    #[must_use]
    pub fn occupied_units(&self) -> u32 {
        match self {
            Self::Uniform(material) => {
                if material.is_air() {
                    0
                } else {
                    UNITS_PER_BLOCK
                }
            }
            Self::Partial { occupancy, .. } => (occupancy & OCCUPANCY_FULL).count_ones(),
            Self::Cells(cells) => cells.iter().filter(|cell| !cell.is_air()).count() as u32,
        }
    }
}

impl Default for BlockValue {
    fn default() -> Self {
        Self::AIR
    }
}

/// A borrowed view of a block's contents, as [`crate::Chunk::get_block`]
/// returns it.
///
/// Mixed blocks borrow their cells from the chunk's side table rather than
/// copying 54 bytes on every read, which matters because reads vastly outnumber
/// writes in meshing, lighting, and physics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockView<'a> {
    /// Every sub-node is the same material.
    Uniform(MaterialId),

    /// One material occupying the masked sub-nodes, air elsewhere.
    Partial {
        /// The occupying material.
        material: MaterialId,
        /// 27-bit mask indexed by [`subnode_index`].
        occupancy: u32,
    },

    /// Two or more materials, borrowed from the chunk's side table.
    Mixed(&'a Cells),
}

impl BlockValue {
    /// Borrows this value as a [`BlockView`].
    ///
    /// The two describe the same thing — one owns its cells and the other
    /// borrows them — and code that reads block geometry is written against the
    /// view. Without this, every caller holding a `BlockValue` has to
    /// re-match the three variants to ask a question about its shape.
    #[must_use]
    pub const fn view(&self) -> BlockView<'_> {
        match self {
            Self::Uniform(material) => BlockView::Uniform(*material),
            Self::Partial {
                material,
                occupancy,
            } => BlockView::Partial {
                material: *material,
                occupancy: *occupancy,
            },
            Self::Cells(cells) => BlockView::Mixed(cells),
        }
    }
}

impl BlockView<'_> {
    /// The material at a sub-node index.
    ///
    /// # Panics
    ///
    /// In debug builds, if `index` is 27 or greater.
    #[must_use]
    pub fn subnode(&self, index: usize) -> MaterialId {
        debug_assert!(index < SUBNODES_PER_BLOCK);
        match self {
            Self::Uniform(material) => *material,
            Self::Partial {
                material,
                occupancy,
            } => {
                if occupancy & (1 << index) != 0 {
                    *material
                } else {
                    MaterialId::AIR
                }
            }
            Self::Mixed(cells) => cells[index],
        }
    }

    /// The material at a block-local sub-node offset.
    #[must_use]
    pub fn subnode_at(&self, x: u32, y: u32, z: u32) -> MaterialId {
        self.subnode(subnode_index(x, y, z))
    }

    /// Expands to explicit per-cell materials.
    #[must_use]
    pub fn cells(&self) -> Cells {
        match self {
            Self::Uniform(material) => [*material; SUBNODES_PER_BLOCK],
            Self::Partial { .. } => self.to_value().cells(),
            Self::Mixed(cells) => **cells,
        }
    }

    /// Count of non-air sub-nodes, in units.
    #[must_use]
    pub fn occupied_units(&self) -> u32 {
        match self {
            Self::Uniform(material) => {
                if material.is_air() {
                    0
                } else {
                    UNITS_PER_BLOCK
                }
            }
            Self::Partial { occupancy, .. } => occupancy.count_ones(),
            Self::Mixed(cells) => cells.iter().filter(|cell| !cell.is_air()).count() as u32,
        }
    }

    /// Whether this block is entirely air.
    #[must_use]
    pub fn is_air(&self) -> bool {
        matches!(self, Self::Uniform(material) if material.is_air())
    }

    /// Copies into an owned [`BlockValue`].
    #[must_use]
    pub fn to_value(&self) -> BlockValue {
        match self {
            Self::Uniform(material) => BlockValue::Uniform(*material),
            Self::Partial {
                material,
                occupancy,
            } => BlockValue::Partial {
                material: *material,
                occupancy: *occupancy,
            },
            Self::Mixed(cells) => BlockValue::Cells(**cells),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);

    #[test]
    fn subnode_index_matches_the_documented_formula() {
        assert_eq!(subnode_index(0, 0, 0), 0);
        assert_eq!(subnode_index(1, 0, 0), 1);
        assert_eq!(subnode_index(0, 1, 0), 3);
        assert_eq!(subnode_index(0, 0, 1), 9);
        assert_eq!(subnode_index(2, 2, 2), 26);
    }

    #[test]
    fn subnode_index_round_trips_over_all_twenty_seven_positions() {
        let mut seen = [false; SUBNODES_PER_BLOCK];
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    let index = subnode_index(x, y, z);
                    assert!(!seen[index], "index {index} produced twice");
                    seen[index] = true;
                    assert_eq!(subnode_offset(index), (x, y, z));
                }
            }
        }
        assert!(seen.iter().all(|&hit| hit), "not every index was produced");
    }

    #[test]
    fn occupancy_full_covers_exactly_twenty_seven_bits() {
        assert_eq!(OCCUPANCY_FULL.count_ones(), 27);
        assert_eq!(OCCUPANCY_FULL, 0x07FF_FFFF);
    }

    #[test]
    fn partial_with_a_full_mask_becomes_uniform() {
        let value = BlockValue::Partial {
            material: STONE,
            occupancy: OCCUPANCY_FULL,
        };
        assert_eq!(value.canonical(), BlockValue::Uniform(STONE));
    }

    #[test]
    fn partial_with_an_empty_mask_becomes_air() {
        let value = BlockValue::Partial {
            material: STONE,
            occupancy: OCCUPANCY_EMPTY,
        };
        assert_eq!(value.canonical(), BlockValue::AIR);
    }

    #[test]
    fn partial_of_air_becomes_air_whatever_the_mask() {
        // A mask of air over air is just air; storing it as Partial would be a
        // second representation of a state Uniform already covers.
        let value = BlockValue::Partial {
            material: MaterialId::AIR,
            occupancy: 0b101,
        };
        assert_eq!(value.canonical(), BlockValue::AIR);
    }

    #[test]
    fn occupancy_bits_above_twenty_seven_are_discarded() {
        let value = BlockValue::Partial {
            material: STONE,
            occupancy: 0xFFFF_FFFF,
        };
        // All 32 bits set: the top 5 are meaningless and must not survive to
        // make the mask look "not full".
        assert_eq!(value.canonical(), BlockValue::Uniform(STONE));
    }

    #[test]
    fn cells_of_one_material_collapse_to_uniform() {
        let value = BlockValue::Cells([STONE; SUBNODES_PER_BLOCK]);
        assert_eq!(value.canonical(), BlockValue::Uniform(STONE));
    }

    #[test]
    fn cells_of_one_material_plus_air_collapse_to_partial() {
        let mut cells = EMPTY_CELLS;
        cells[0] = STONE;
        cells[5] = STONE;
        assert_eq!(
            BlockValue::Cells(cells).canonical(),
            BlockValue::Partial {
                material: STONE,
                occupancy: 0b10_0001,
            }
        );
    }

    #[test]
    fn all_air_cells_collapse_to_air() {
        assert_eq!(BlockValue::Cells(EMPTY_CELLS).canonical(), BlockValue::AIR);
    }

    #[test]
    fn two_distinct_materials_stay_mixed() {
        let mut cells = [STONE; SUBNODES_PER_BLOCK];
        cells[13] = DIRT;
        let value = BlockValue::Cells(cells);
        assert_eq!(value.canonical(), value);
    }

    #[test]
    fn canonicalisation_is_idempotent() {
        let mut cells = [STONE; SUBNODES_PER_BLOCK];
        cells[13] = DIRT;
        let candidates = [
            BlockValue::Uniform(MaterialId::AIR),
            BlockValue::Uniform(STONE),
            BlockValue::Partial {
                material: STONE,
                occupancy: 0b1011,
            },
            BlockValue::Partial {
                material: STONE,
                occupancy: OCCUPANCY_FULL,
            },
            BlockValue::Cells(cells),
            BlockValue::Cells(EMPTY_CELLS),
        ];
        for candidate in candidates {
            let once = candidate.canonical();
            assert_eq!(once.canonical(), once, "not idempotent for {candidate:?}");
            assert!(once.is_canonical());
        }
    }

    #[test]
    fn cells_expansion_agrees_with_the_occupancy_mask() {
        let value = BlockValue::Partial {
            material: STONE,
            occupancy: 0b1010,
        };
        let cells = value.cells();
        assert_eq!(cells[0], MaterialId::AIR);
        assert_eq!(cells[1], STONE);
        assert_eq!(cells[2], MaterialId::AIR);
        assert_eq!(cells[3], STONE);
        assert_eq!(value.occupied_units(), 2);
    }

    #[test]
    fn occupied_units_counts_only_non_air() {
        assert_eq!(BlockValue::Uniform(STONE).occupied_units(), 27);
        assert_eq!(BlockValue::AIR.occupied_units(), 0);
        let mut cells = [STONE; SUBNODES_PER_BLOCK];
        cells[0] = MaterialId::AIR;
        cells[1] = DIRT;
        assert_eq!(BlockValue::Cells(cells).occupied_units(), 26);
    }

    #[test]
    fn block_content_is_eight_bytes() {
        // Pinned because the chunk memory bound depends on it: a palette entry
        // is this plus a u32 refcount, so 12 bytes. Eight rather than twelve
        // because the layout packs the discriminant into padding beside the
        // u16 material — `Partial`'s u16 + u32 leaves two bytes spare and the
        // tag lands there. If a future variant breaks that, this test fires and
        // `Chunk::memory_usage`'s documented table needs regenerating.
        assert_eq!(size_of::<BlockContent>(), 8);
    }
}
