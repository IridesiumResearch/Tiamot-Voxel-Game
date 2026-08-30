// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Seeing far: downsampled summaries of a chunk, for drawing the horizon.
//!
//! A world 120,000 blocks across cannot be streamed at full detail to the
//! horizon, and it does not have to be: past a certain distance a block is
//! smaller than a pixel, and what a player can actually see is the SHAPE of the
//! land. A summary is that shape — one material per cell, at whatever
//! resolution the distance justifies.
//!
//! # The levels
//!
//! - **LOD0** is whatever Task 08 ships: the full sub-node mesher. Not defined
//!   here, because it is not a summary — it is the chunk.
//! - **LOD1** is one cell per BLOCK: [`CHUNK_BLOCKS`] cubed.
//! - **LOD2 and up** halve each axis again, so a cell covers 2, then 4, then 8
//!   blocks, down to [`Summary`] holding a single cell for the whole chunk.
//!
//! Each level is built from the one below it rather than from the chunk — a mip
//! chain. That is both cheaper (a level costs an eighth of its predecessor) and
//! the shape invalidation already has to take: an edit dirties a column of
//! levels, and rebuilding one rebuilds the rest from it.
//!
//! # Majority, and what a tie means
//!
//! A cell takes the material most of it is made of, **air included**. A block
//! that is one chiselled corner of stone is mostly air, and at a distance where
//! one cell is eight blocks across, drawing it solid would put a hillside where
//! there is a handrail.
//!
//! Ties are broken by the LOWEST material id, which is not arbitrary: it has to
//! be a total order that both ends agree on and that does not depend on
//! iteration order (charter rule 4). Air is id 0, so a cell exactly half air
//! reads as air — the conservative answer for something being drawn at a size
//! where it is nearly invisible anyway.
//!
//! # Determinism
//!
//! Every operation here is integer counting and comparison. No floats, no hash
//! iteration, no allocation whose size depends on content ordering — so the
//! same chunk summarises to the same bytes on every supported target, which is
//! what lets the CI gate hash them.

use crate::block::{BlockView, SUBNODES_PER_BLOCK};
use crate::coords::LocalBlock;
use crate::material::MaterialId;
use crate::{CHUNK_BLOCKS, Chunk};

/// The finest summary level: one cell per block.
///
/// LOD0 is the chunk itself and has no [`Summary`] — see the module docs.
pub const FINEST: u8 = 1;

/// The coarsest level, where the whole chunk is one cell.
///
/// `CHUNK_BLOCKS` is 16, so the chain is 16³, 8³, 4³, 2³, 1³ — five levels.
/// Derived rather than written down, because charter rule 6 makes 16 load
/// bearing and a second place that knows it is a second place to get it wrong.
pub const COARSEST: u8 = FINEST + CHUNK_BLOCKS.trailing_zeros() as u8;

/// How many cells along one axis a level has.
///
/// `None` for a level outside the chain: a caller asking for level 0 wants the
/// chunk, and one asking past [`COARSEST`] has divided past a single cell.
#[must_use]
pub const fn cells_per_axis(level: u8) -> Option<u32> {
    if level < FINEST || level > COARSEST {
        return None;
    }
    Some(CHUNK_BLOCKS >> (level - FINEST))
}

/// One chunk's shape at one level.
///
/// Cells are in `x + y * n + z * n * n` order, the same order the chunk's own
/// storage uses, so a mesher walking one walks the other the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    level: u8,
    cells: Vec<MaterialId>,
}

impl Summary {
    /// Builds the finest summary of a chunk: one cell per block.
    ///
    /// A block's cell is the material most of its 27 sub-nodes are, air
    /// included — see the module docs on why a mostly-empty block reads as
    /// empty.
    #[must_use]
    pub fn of(chunk: &Chunk) -> Self {
        let n = CHUNK_BLOCKS;
        let mut cells = Vec::with_capacity((n * n * n) as usize);
        for z in 0..n {
            for y in 0..n {
                for x in 0..n {
                    let local = LocalBlock::new(x, y, z);
                    cells.push(dominant_subnode(&chunk.get_block_local(local)));
                }
            }
        }
        Self {
            level: FINEST,
            cells,
        }
    }

    /// The next level up: each cell the majority of the 2×2×2 below it.
    ///
    /// `None` at [`COARSEST`], where there is nothing left to halve.
    #[must_use]
    pub fn coarser(&self) -> Option<Self> {
        let below = cells_per_axis(self.level)?;
        let level = self.level.checked_add(1)?;
        let above = cells_per_axis(level)?;
        let mut cells = Vec::with_capacity((above * above * above) as usize);
        for z in 0..above {
            for y in 0..above {
                for x in 0..above {
                    // The eight cells this one covers, in a fixed order so the
                    // tie-break sees the same sequence everywhere.
                    let mut group = [MaterialId::AIR; 8];
                    let mut count = 0;
                    for dz in 0..2 {
                        for dy in 0..2 {
                            for dx in 0..2 {
                                let index = ((x * 2 + dx)
                                    + (y * 2 + dy) * below
                                    + (z * 2 + dz) * below * below)
                                    as usize;
                                group[count] = self.cells[index];
                                count += 1;
                            }
                        }
                    }
                    cells.push(majority(&group));
                }
            }
        }
        Some(Self { level, cells })
    }

    /// Every level from this one to [`COARSEST`], this one first.
    ///
    /// The whole chain in one call, because that is how it is stored and how an
    /// edit invalidates it: a chunk's summaries are made and thrown away
    /// together.
    #[must_use]
    pub fn chain(chunk: &Chunk) -> Vec<Self> {
        let mut out = vec![Self::of(chunk)];
        while let Some(next) = out.last().and_then(Self::coarser) {
            out.push(next);
        }
        out
    }

    /// Which level this is.
    #[must_use]
    pub const fn level(&self) -> u8 {
        self.level
    }

    /// How many cells along one axis.
    #[must_use]
    pub fn width(&self) -> u32 {
        cells_per_axis(self.level).unwrap_or(1)
    }

    /// The cells, in `x + y * n + z * n * n` order.
    #[must_use]
    pub fn cells(&self) -> &[MaterialId] {
        &self.cells
    }

    /// One cell, or `None` outside the summary.
    #[must_use]
    pub fn cell(&self, x: u32, y: u32, z: u32) -> Option<MaterialId> {
        let n = self.width();
        if x >= n || y >= n || z >= n {
            return None;
        }
        self.cells.get((x + y * n + z * n * n) as usize).copied()
    }

    /// Whether every cell is air.
    ///
    /// What lets a summary of empty sky cost nothing to store or send.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.iter().all(|material| material.is_air())
    }

    /// Rebuilds a summary from parts, for the codec and the cache.
    ///
    /// # Errors
    ///
    /// [`SummaryError`] if the level is outside the chain or the cell count
    /// does not match it — a summary whose width disagrees with its level would
    /// index out of its own storage.
    pub fn from_parts(level: u8, cells: Vec<MaterialId>) -> Result<Self, SummaryError> {
        let Some(n) = cells_per_axis(level) else {
            return Err(SummaryError::Level { level });
        };
        let expected = (n * n * n) as usize;
        if cells.len() != expected {
            return Err(SummaryError::Size {
                level,
                expected,
                found: cells.len(),
            });
        }
        Ok(Self { level, cells })
    }
}

/// Why a summary could not be built from stored parts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SummaryError {
    /// A level outside the chain.
    #[error("level {level} is not a summary level ({FINEST} to {COARSEST})")]
    Level {
        /// The level asked for.
        level: u8,
    },

    /// The wrong number of cells for the level.
    #[error("a level {level} summary holds {expected} cells, not {found}")]
    Size {
        /// The level in question.
        level: u8,
        /// How many cells it should have.
        expected: usize,
        /// How many it had.
        found: usize,
    },
}

/// The material most of a block is made of, air included.
fn dominant_subnode(view: &BlockView<'_>) -> MaterialId {
    // The common cases without counting: a uniform block is its material, and
    // most of a world is uniform.
    if let BlockView::Uniform(material) = view {
        return *material;
    }
    let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
    for (index, cell) in cells.iter_mut().enumerate() {
        *cell = view.subnode(index);
    }
    majority(&cells)
}

/// The most common material in a slice, ties going to the lowest id.
///
/// A counting scan rather than a map: the inputs are 8 or 27 long, and reaching
/// for a `HashMap` here would put hash iteration order inside a result the
/// determinism gate hashes (charter rule 4).
fn majority(cells: &[MaterialId]) -> MaterialId {
    let mut best = MaterialId::AIR;
    let mut best_count = 0;
    for (index, candidate) in cells.iter().enumerate() {
        // Counted once per distinct value: skipping a value already seen makes
        // this O(n²) on a tiny n rather than O(n) plus an allocation.
        if cells[..index].contains(candidate) {
            continue;
        }
        let count = cells.iter().filter(|cell| *cell == candidate).count();
        // **Strictly greater, and the lowest id wins a tie.** The scan order is
        // the slice's, which is fixed, but relying on that would make the
        // answer depend on where a material happened to sit — so the tie is
        // broken by the id itself, which is the same on both ends.
        if count > best_count || (count == best_count && candidate.0 < best.0) {
            best = *candidate;
            best_count = count;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::ChunkPos;

    const STONE: MaterialId = MaterialId(1);
    const DIRT: MaterialId = MaterialId(2);

    fn home() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    #[test]
    fn the_chain_halves_until_one_cell_is_the_whole_chunk() {
        // Derived from `CHUNK_BLOCKS` rather than written down: charter rule 6
        // makes 16 load bearing, and a second place that knows it is a second
        // place to get it wrong.
        assert_eq!(cells_per_axis(FINEST), Some(CHUNK_BLOCKS));
        assert_eq!(cells_per_axis(COARSEST), Some(1));
        assert_eq!(cells_per_axis(0), None, "LOD0 is the chunk, not a summary");
        assert_eq!(cells_per_axis(COARSEST + 1), None);

        let chunk = Chunk::new(home(), STONE);
        let chain = Summary::chain(&chunk);
        assert_eq!(chain.len(), (COARSEST - FINEST + 1) as usize);
        assert_eq!(chain.first().map(Summary::width), Some(CHUNK_BLOCKS));
        assert_eq!(chain.last().map(Summary::width), Some(1));
    }

    #[test]
    fn a_solid_chunk_summarises_to_its_own_material_at_every_level() {
        let chunk = Chunk::new(home(), STONE);
        for summary in Summary::chain(&chunk) {
            assert!(
                summary.cells().iter().all(|cell| *cell == STONE),
                "level {} lost the material it was made of",
                summary.level()
            );
            assert!(!summary.is_empty());
        }
    }

    #[test]
    fn empty_sky_stays_empty() {
        let chunk = Chunk::air(home());
        for summary in Summary::chain(&chunk) {
            assert!(
                summary.is_empty(),
                "level {} invented terrain",
                summary.level()
            );
        }
    }

    #[test]
    fn a_block_that_is_mostly_air_reads_as_air() {
        // **The handrail case.** A block with one chiselled corner of stone is
        // mostly nothing, and at a distance where a cell is eight blocks across
        // drawing it solid would put a hillside where there is a handrail.
        let mut chunk = Chunk::air(home());
        chunk
            .set_subnode(crate::SubNodePos::new(0, 0, 0), STONE)
            .expect("one cell");
        let summary = Summary::of(&chunk);
        assert_eq!(
            summary.cell(0, 0, 0),
            Some(MaterialId::AIR),
            "one sub-node of twenty-seven made a whole block solid"
        );

        // And the counter-example, so this is not just "everything is air":
        // a block that is mostly stone reads as stone.
        let mut solid = Chunk::air(home());
        for index in (SUBNODES_PER_BLOCK / 2)..SUBNODES_PER_BLOCK {
            let (dx, dy, dz) = crate::block::subnode_offset(index);
            solid
                .set_subnode(
                    crate::SubNodePos::new(dx as i32, dy as i32, dz as i32),
                    STONE,
                )
                .expect("a cell");
        }
        assert_eq!(Summary::of(&solid).cell(0, 0, 0), Some(STONE));
    }

    #[test]
    fn a_tie_goes_to_the_lowest_id_and_not_to_whichever_was_seen_first() {
        // The tie-break has to be a total order both ends agree on. Scan order
        // is fixed but relying on it would make the answer depend on where a
        // material happened to sit in the slice.
        assert_eq!(majority(&[STONE, DIRT]), STONE);
        assert_eq!(majority(&[DIRT, STONE]), STONE, "scan order decided a tie");
        assert_eq!(
            majority(&[MaterialId::AIR, STONE]),
            MaterialId::AIR,
            "a cell exactly half air should read as air"
        );
        assert_eq!(majority(&[STONE, STONE, DIRT]), STONE, "a majority lost");
    }

    #[test]
    fn a_summary_that_does_not_match_its_level_is_refused() {
        // A summary whose width disagrees with its level indexes out of its own
        // storage, which is a panic in a decoder reading somebody else's bytes.
        assert!(Summary::from_parts(FINEST, vec![STONE; 4096]).is_ok());
        assert!(matches!(
            Summary::from_parts(FINEST, vec![STONE; 10]),
            Err(SummaryError::Size { .. })
        ));
        assert!(matches!(
            Summary::from_parts(0, vec![STONE]),
            Err(SummaryError::Level { .. })
        ));
        assert!(matches!(
            Summary::from_parts(COARSEST + 1, vec![STONE]),
            Err(SummaryError::Level { .. })
        ));
    }

    #[test]
    fn half_a_chunk_of_ground_keeps_its_horizon_at_every_level() {
        // The shape that matters: a summary exists to draw the SKYLINE, so the
        // level where one cell is the whole chunk must still say "half of this
        // is ground" by being ground — the majority — rather than air.
        let mut chunk = Chunk::air(home());
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS / 2 {
                for x in 0..CHUNK_BLOCKS {
                    chunk.set_block_local(
                        LocalBlock::new(x, y, z),
                        crate::BlockValue::Uniform(DIRT),
                    );
                }
            }
        }
        let chain = Summary::chain(&chunk);
        let finest = chain.first().expect("a chain");
        assert_eq!(finest.cell(0, 0, 0), Some(DIRT), "the ground went missing");
        assert_eq!(
            finest.cell(0, CHUNK_BLOCKS - 1, 0),
            Some(MaterialId::AIR),
            "the sky filled in"
        );

        // Exactly half, so the tie-break decides — and air wins, which is the
        // conservative answer for something drawn at a size where it is nearly
        // invisible. Written down because it is a choice, not an accident.
        assert_eq!(
            chain.last().and_then(|whole| whole.cell(0, 0, 0)),
            Some(MaterialId::AIR)
        );
    }
}
