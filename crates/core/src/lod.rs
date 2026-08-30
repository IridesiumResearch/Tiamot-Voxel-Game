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
use crate::interest::ViewDistance;
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
/// How far the horizon reaches, for a given detail radius.
///
/// **Four times out, and twice up, capped at the maximum view.** Not a taste
/// decision: criterion A3 wants 32 chunks of overworld view, the default detail
/// radius is 8, and four times eight is exactly that. Vertically it stops at
/// twice, because the vertical extent of what a player can actually see is set
/// by the terrain and not by the view distance — a horizon 32 chunks tall is
/// half a kilometre of empty sky above and stone below, summarised for nobody.
///
/// The cost is bounded by the summaries, not by this: a box this size is tens
/// of thousands of chunks, and what makes that affordable is that the far ones
/// are a few dozen bytes each and are paced out at the same rate as everything
/// else.
#[must_use]
pub fn horizon_for(view: ViewDistance) -> ViewDistance {
    ViewDistance::clamped(
        view.horizontal.saturating_mul(4),
        view.vertical.saturating_mul(2),
    )
}

/// Which level to draw a chunk at, given how far away it is.
///
/// # A ring is a distance band, and the level doubles across each one
///
/// A cell at level `n` is `2^(n-1)` blocks across. What a player should see is
/// roughly constant *apparent* cell size, so the level wants to grow with the
/// logarithm of distance — twice as far away, cells twice as big. That is one
/// integer `ilog2`, which is exact, has no libm in it, and gives the same
/// answer on every target (charter rule 4).
///
/// Inside the detail radius there is no summary at all: the client has the real
/// chunk and meshes it at sub-node resolution. That is what [`Level::Chunk`]
/// means, and it is deliberately a different thing from "level 0" — level 0
/// does not exist, because LOD0 is not a summary.
///
/// # Hysteresis is not optional
///
/// The bands have hard edges, and a player standing on one is a player whose
/// chunks change level every time they step. Each change is a remesh, so a
/// pacing camera would spend the whole frame budget rebuilding geometry that
/// looks identical. [`Rings::stable_level`] fixes that by refusing to change a
/// chunk's level until the *whole* margin band has crossed the boundary: the
/// level only moves when it would still move if the player were a chunk
/// further on. Walking through costs one rebuild; pacing across costs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rings {
    /// Chunks nearer than this are sent and drawn in full.
    detail: u32,
    /// The margin, in chunks, a boundary crossing has to clear.
    margin: u32,
}

/// What resolution one chunk should be drawn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The real chunk, meshed at sub-node resolution. No summary involved.
    Chunk,
    /// A summary at this level.
    Summary(u8),
}

impl Rings {
    /// The default margin: one chunk.
    ///
    /// Enough that a player walking about inside a chunk never crosses a
    /// boundary twice, which is the case that produced the churn. Bigger would
    /// mean a visible band where a chunk is drawn coarser than its neighbours
    /// at the same distance, and a player can see that when they turn round.
    pub const MARGIN: u32 = 1;

    /// Rings around a detail radius, in chunks.
    ///
    /// A `detail` of zero is raised to one: a world where the chunk you are
    /// standing in is a summary is not a view distance, it is a bug, and
    /// clamping is kinder than dividing by zero.
    #[must_use]
    pub const fn new(detail: u32, margin: u32) -> Self {
        Self {
            detail: if detail == 0 { 1 } else { detail },
            margin,
        }
    }

    /// The detail radius these rings are built around.
    #[must_use]
    pub const fn detail(&self) -> u32 {
        self.detail
    }

    /// The level for a chunk `distance` chunks from the centre.
    ///
    /// Distance is the Chebyshev distance — the box, not the sphere, matching
    /// [`crate::interest::chunks_around`], which streams a box.
    #[must_use]
    pub fn level_at(&self, distance: u32) -> Level {
        if distance <= self.detail {
            return Level::Chunk;
        }
        // How many times further out than the detail radius, floored. One at
        // the first ring, two at twice the radius, four at four times it.
        let ratio = distance / self.detail;
        let step = u8::try_from(ratio.ilog2()).unwrap_or(u8::MAX);
        Level::Summary(FINEST.saturating_add(step).min(COARSEST))
    }

    /// The level to actually build, given what is already built.
    ///
    /// **The hysteresis.** A chunk keeps the level it has until the distance is
    /// past the boundary by [`Rings::MARGIN`] in the direction of the change,
    /// so a camera oscillating across a ring edge rebuilds nothing. `current`
    /// is `None` for a chunk that has never been built, which always takes the
    /// level it is asked for — there is nothing to churn.
    #[must_use]
    pub fn stable_level(&self, current: Option<Level>, distance: u32) -> Level {
        let want = self.level_at(distance);
        let Some(current) = current else {
            return want;
        };
        if want == current {
            return current;
        }
        // Both edges of the margin band have to agree, or the player is still
        // standing on the boundary and this is the churn.
        let nearer = self.level_at(distance.saturating_sub(self.margin));
        let further = self.level_at(distance.saturating_add(self.margin));
        if nearer == want && further == want {
            want
        } else {
            current
        }
    }
}

impl Default for Rings {
    /// The default view distance's rings.
    ///
    /// Written out rather than derived: a derived default has a detail radius
    /// of zero, which `new` would clamp to one, and a horizon that started a
    /// chunk away would be a silent disaster rather than a compile error.
    fn default() -> Self {
        Self::new(u32::from(ViewDistance::DEFAULT.horizontal), Self::MARGIN)
    }
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

    /// An encoding this build does not know.
    #[error("summary format version {found} is not one this build can read")]
    Version {
        /// The version byte found.
        found: u8,
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

/// Turning a summary into bytes and back.
///
/// **A summary goes to disk AND onto the wire**, and one encoding serves both:
/// the cache stores exactly what a client is sent, so serving a cached summary
/// is a read rather than a re-encode.
///
/// # This is hostile input (charter rule 14)
///
/// A client decodes summaries from servers it does not trust, so the decoder
/// refuses rather than trusts: a version it does not know, a level outside the
/// chain, a length that disagrees with the level, and anything longer than the
/// largest summary there can be. No allocation is sized by a number the sender
/// chose — the cell count comes from the LEVEL, which is checked first.
pub mod codec {
    use super::{Summary, SummaryError, cells_per_axis};
    use crate::material::MaterialId;

    /// What every encoded summary starts with.
    const VERSION: u8 = 1;

    /// `zstd` level for summary blobs.
    ///
    /// The same 3 the chunk codec uses, and for the same reason: a summary is
    /// encoded once and sent to every player who comes near it, so the trade
    /// is already on the right side without paying for a slower level.
    const ZSTD_LEVEL: i32 = 3;

    /// The cells of the finest level, in bytes.
    const FINEST_BYTES: usize = (16 * 16 * 16) * 2;

    /// The largest an encoded summary can be.
    ///
    /// The finest level is `CHUNK_BLOCKS`³ cells of two bytes; the slack past
    /// it is because `zstd` on incompressible data is slightly LARGER than its
    /// input — a horizon of noise is not a thing this refuses.
    pub const MAX_ENCODED: usize = 2 + FINEST_BYTES + FINEST_BYTES / 64 + 128;

    /// Encodes a summary: a version, its level, then `zstd` over a `u16` per
    /// cell.
    ///
    /// Little-endian and fixed-width under the compression, so the bytes are
    /// the same on every supported target — which is what lets the determinism
    /// gate hash them and the cache compare them.
    ///
    /// **Compressed, unlike an early draft of this, because an uncompressed
    /// level-1 summary is 8 KiB and a palette-compressed chunk is a fraction of
    /// that.** A horizon that costs more bandwidth than the terrain in front of
    /// it is not a horizon. Most of a horizon is uniform stone or uniform air,
    /// which is where the whole saving is.
    ///
    /// # Panics
    ///
    /// Never in practice: `zstd` over an in-memory buffer of known size fails
    /// only on allocation failure, and the fallback stores the cells raw rather
    /// than producing a blob nothing can read.
    #[must_use]
    pub fn encode(summary: &Summary) -> Vec<u8> {
        let mut cells = Vec::with_capacity(summary.cells().len() * 2);
        for cell in summary.cells() {
            cells.extend_from_slice(&cell.0.to_le_bytes());
        }
        let body = zstd::bulk::compress(&cells, ZSTD_LEVEL).unwrap_or(cells);
        let mut out = Vec::with_capacity(2 + body.len());
        out.push(VERSION);
        out.push(summary.level());
        out.extend_from_slice(&body);
        out
    }

    /// Reads a summary, or says why it is not one.
    ///
    /// # Errors
    ///
    /// [`SummaryError`] for an unknown version, a level outside the chain, or a
    /// body whose length does not match the level it claims.
    pub fn decode(bytes: &[u8]) -> Result<Summary, SummaryError> {
        if bytes.len() > MAX_ENCODED {
            return Err(SummaryError::Size {
                level: 0,
                expected: MAX_ENCODED,
                found: bytes.len(),
            });
        }
        let [version, level, body @ ..] = bytes else {
            return Err(SummaryError::Level { level: 0 });
        };
        if *version != VERSION {
            return Err(SummaryError::Version { found: *version });
        }
        // **The level decides the size, and it is checked first.** The
        // decompression bound comes from the level, not from the blob — so a
        // sender cannot choose the allocation, and a decompression bomb is a
        // refusal rather than a gigabyte.
        let Some(n) = cells_per_axis(*level) else {
            return Err(SummaryError::Level { level: *level });
        };
        let expected = (n * n * n) as usize;
        let cells_bytes =
            zstd::bulk::decompress(body, expected * 2).map_err(|_| SummaryError::Size {
                level: *level,
                expected: expected * 2,
                found: body.len(),
            })?;
        if cells_bytes.len() != expected * 2 {
            return Err(SummaryError::Size {
                level: *level,
                expected: expected * 2,
                found: cells_bytes.len(),
            });
        }
        let cells = cells_bytes
            .chunks_exact(2)
            .map(|pair| MaterialId(u16::from_le_bytes([pair[0], pair[1]])))
            .collect();
        Summary::from_parts(*level, cells)
    }

    #[cfg(test)]
    mod tests {
        use super::super::{COARSEST, FINEST};
        use super::*;

        fn summary(level: u8) -> Summary {
            let n = cells_per_axis(level).expect("a level");
            Summary::from_parts(level, vec![MaterialId(3); (n * n * n) as usize]).expect("build")
        }

        #[test]
        fn every_level_survives_the_wire() {
            for level in FINEST..=COARSEST {
                let sent = summary(level);
                let bytes = encode(&sent);
                assert_eq!(decode(&bytes), Ok(sent), "level {level} did not round-trip");
            }
        }

        #[test]
        fn anything_that_is_not_a_summary_is_refused() {
            // A client decodes these from servers it does not trust, so every
            // failure is a refusal rather than a guess (charter rule 14).
            assert!(decode(&[]).is_err(), "an empty message");
            assert!(decode(&[VERSION]).is_err(), "a header with no level");
            assert!(
                matches!(decode(&[9, FINEST]), Err(SummaryError::Version { .. })),
                "a version this build does not know"
            );
            assert!(
                matches!(decode(&[VERSION, 0]), Err(SummaryError::Level { .. })),
                "level 0 is the chunk, not a summary"
            );
            assert!(
                matches!(
                    decode(&[VERSION, COARSEST + 1]),
                    Err(SummaryError::Level { .. })
                ),
                "a level past the end of the chain"
            );

            // A body that disagrees with its own level, both ways round.
            let good = encode(&summary(COARSEST));
            let mut short = good.clone();
            short.pop();
            assert!(matches!(decode(&short), Err(SummaryError::Size { .. })));
            let mut long = good;
            long.push(0);
            assert!(
                matches!(decode(&long), Err(SummaryError::Size { .. })),
                "trailing bytes were ignored, so one summary has two spellings"
            );

            // And nothing enormous, whatever it claims to be.
            assert!(decode(&vec![VERSION; MAX_ENCODED + 1]).is_err());
        }

        #[test]
        fn a_cut_summary_never_decodes_and_never_panics() {
            let good = encode(&summary(FINEST));
            for cut in 0..good.len() {
                assert!(
                    decode(&good[..cut]).is_err(),
                    "a summary cut at {cut} bytes decoded anyway"
                );
            }
        }
    }
}

#[cfg(test)]
mod ring_tests {
    use super::{COARSEST, FINEST, Level, Rings};

    #[test]
    fn what_you_are_standing_in_is_never_a_summary() {
        let rings = Rings::new(8, Rings::MARGIN);
        for distance in 0..=8 {
            assert_eq!(rings.level_at(distance), Level::Chunk, "at {distance}");
        }
        assert_eq!(rings.level_at(9), Level::Summary(FINEST));
    }

    #[test]
    fn the_level_climbs_with_the_logarithm_of_the_distance() {
        // Twice as far away, cells twice as big — which is one level up, since
        // a level doubles the cell. Anything steeper wastes bandwidth on
        // terrain smaller than a pixel; anything shallower makes the horizon
        // cost more than the world in front of it.
        let rings = Rings::new(8, Rings::MARGIN);
        assert_eq!(rings.level_at(9), Level::Summary(1));
        assert_eq!(rings.level_at(15), Level::Summary(1));
        assert_eq!(rings.level_at(16), Level::Summary(2));
        assert_eq!(rings.level_at(31), Level::Summary(2));
        assert_eq!(rings.level_at(32), Level::Summary(3));
        assert_eq!(rings.level_at(64), Level::Summary(4));
        assert_eq!(rings.level_at(128), Level::Summary(COARSEST));
    }

    #[test]
    fn it_never_asks_for_a_level_that_does_not_exist() {
        // The far end saturates rather than running off the end of the chain:
        // a distance of four billion chunks is not reachable, but a `u32`
        // holds it and an overflow here would be an allocation somewhere else.
        let rings = Rings::new(1, Rings::MARGIN);
        for distance in [2, 1_000, u32::MAX / 2, u32::MAX] {
            let Level::Summary(level) = rings.level_at(distance) else {
                panic!("distance {distance} came back as a full chunk");
            };
            assert!(
                (FINEST..=COARSEST).contains(&level),
                "distance {distance} asked for level {level}"
            );
        }
    }

    #[test]
    fn a_camera_pacing_across_a_ring_edge_rebuilds_nothing() {
        // **Criterion T3.** The boundary between two rings is a hard edge, and
        // a player standing on one would flip every chunk behind it between
        // two levels — a remesh each way, every step, for geometry that looks
        // identical. The margin band has to clear the boundary entirely before
        // the level moves.
        let rings = Rings::new(8, Rings::MARGIN);
        let boundary = 16; // where level 1 becomes level 2

        let mut level = rings.level_at(boundary - 2);
        assert_eq!(level, Level::Summary(1));
        let mut rebuilds = 0;
        // Pace back and forth across the edge, a chunk either side, forty times.
        for step in 0..40 {
            let distance = if step % 2 == 0 {
                boundary
            } else {
                boundary - 1
            };
            let next = rings.stable_level(Some(level), distance);
            if next != level {
                rebuilds += 1;
                level = next;
            }
        }
        assert_eq!(
            rebuilds, 0,
            "pacing across a ring edge rebuilt {rebuilds} times"
        );
    }

    #[test]
    fn walking_through_a_ring_edge_still_changes_level_exactly_once() {
        // The other half of the same criterion, and the one that catches a
        // hysteresis that simply never changes anything.
        let rings = Rings::new(8, Rings::MARGIN);
        let mut level = rings.level_at(9);
        let mut changes = 0;
        // Out to 33 rather than 32: the margin means a change lands one chunk
        // PAST the boundary, which is the whole point of it.
        for distance in 9..=33 {
            let next = rings.stable_level(Some(level), distance);
            if next != level {
                changes += 1;
                level = next;
            }
        }
        assert_eq!(
            changes, 2,
            "walking out past two ring edges changed level {changes} times"
        );
        assert_eq!(level, Level::Summary(3));

        // And walking back in comes back to where it started.
        for distance in (0..=33).rev() {
            level = rings.stable_level(Some(level), distance);
        }
        assert_eq!(level, Level::Chunk);
    }

    #[test]
    fn a_chunk_that_has_never_been_built_takes_the_level_it_is_asked_for() {
        // Hysteresis is about not rebuilding, and there is nothing to rebuild.
        let rings = Rings::new(8, Rings::MARGIN);
        for distance in [0, 9, 16, 17, 64] {
            assert_eq!(
                rings.stable_level(None, distance),
                rings.level_at(distance),
                "at {distance}"
            );
        }
    }

    #[test]
    fn a_detail_radius_of_zero_is_clamped_rather_than_dividing_by_zero() {
        let rings = Rings::new(0, Rings::MARGIN);
        assert_eq!(rings.detail(), 1);
        assert_eq!(rings.level_at(0), Level::Chunk);
        assert_eq!(rings.level_at(1), Level::Chunk);
        assert_eq!(rings.level_at(2), Level::Summary(2));
    }
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
