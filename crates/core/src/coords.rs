// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! World coordinate types at the three resolutions the engine works in.
//!
//! [`ChunkPos`] addresses a 16³-block chunk, [`BlockPos`] a single block, and
//! [`SubNodePos`] a single 1/3-yard sub-node cell. Every conversion between
//! them is here, and every one uses Euclidean division so that behaviour on the
//! negative side of the origin is identical to the positive side.
//!
//! # Why Euclidean division, specifically
//!
//! Rust's `/` truncates toward zero, so `-1 / 16 == 0` and blocks at −1 and 0
//! would land in the same chunk while blocks at −16 and −17 would not. That is
//! a coordinate system with a seam through the origin, and seams in worldgen
//! break the determinism gate. [`i32::div_euclid`] floors instead, giving
//! uniformly sized chunks everywhere. This module never uses `/` or `%` on a
//! coordinate.

use crate::{CHUNK_BLOCKS, CHUNK_SUBNODES, SUBNODES_PER_AXIS};

/// Half the world's extent along each axis, in blocks.
///
/// The valid block coordinate range is half-open — `-60_000 ..= 59_999` — so
/// the world is exactly 120,000 blocks per axis as charter rule 6 states. An
/// inclusive bound at both ends would give 120,001, and the odd axis length
/// would put the origin off-centre by half a block in every chunk-alignment
/// calculation downstream.
pub const WORLD_HALF_EXTENT_BLOCKS: i32 = 60_000;

/// Half the world's extent along each axis, in sub-node cells.
pub const WORLD_HALF_EXTENT_SUBNODES: i32 = WORLD_HALF_EXTENT_BLOCKS * SUBNODES_PER_AXIS as i32;

/// Half the world's extent along each axis, in chunks.
pub const WORLD_HALF_EXTENT_CHUNKS: i32 = WORLD_HALF_EXTENT_BLOCKS / CHUNK_BLOCKS as i32;

/// A coordinate fell outside the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{kind} coordinate ({x}, {y}, {z}) is outside the world bound of ±{bound}")]
pub struct CoordError {
    /// Which resolution the coordinate was at.
    pub kind: CoordKind,
    /// The offending x.
    pub x: i32,
    /// The offending y.
    pub y: i32,
    /// The offending z.
    pub z: i32,
    /// The half-extent that was exceeded, at that resolution.
    pub bound: i32,
}

/// Which of the three coordinate resolutions a [`CoordError`] refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordKind {
    /// A [`ChunkPos`].
    Chunk,
    /// A [`BlockPos`].
    Block,
    /// A [`SubNodePos`].
    SubNode,
}

impl std::fmt::Display for CoordKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Chunk => "chunk",
            Self::Block => "block",
            Self::SubNode => "sub-node",
        };
        f.write_str(name)
    }
}

/// Generates one coordinate newtype. The three differ only in their bound and
/// their error label, and writing them out three times invites the kind of
/// copy-paste divergence this module exists to prevent.
macro_rules! coord_type {
    ($name:ident, $kind:expr, $bound:expr, $doc:expr) => {
        #[doc = $doc]
        // Serialisable because coordinates cross the wire (Task 06) and go into
        // chunk blobs. Three i32 fields, so postcard encodes them positionally
        // and there are no variants to reorder.
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Default,
            serde::Serialize,
            serde::Deserialize,
        )]
        pub struct $name {
            /// East-west axis.
            pub x: i32,
            /// Vertical axis.
            pub y: i32,
            /// North-south axis.
            pub z: i32,
        }

        impl $name {
            /// Half the world's extent at this resolution.
            pub const HALF_EXTENT: i32 = $bound;

            /// Constructs without checking the world bound.
            ///
            /// Use for arithmetic that will be bounds-checked later. Anything
            /// derived from untrusted input should use [`Self::checked`].
            #[must_use]
            pub const fn new(x: i32, y: i32, z: i32) -> Self {
                Self { x, y, z }
            }

            /// Constructs, rejecting coordinates outside the world.
            ///
            /// # Errors
            ///
            /// [`CoordError`] if any axis is outside `-HALF_EXTENT ..=
            /// HALF_EXTENT - 1`.
            pub const fn checked(x: i32, y: i32, z: i32) -> Result<Self, CoordError> {
                let value = Self { x, y, z };
                if value.in_world() {
                    Ok(value)
                } else {
                    Err(CoordError {
                        kind: $kind,
                        x,
                        y,
                        z,
                        bound: Self::HALF_EXTENT,
                    })
                }
            }

            /// Whether every axis lies inside the world bound.
            #[must_use]
            pub const fn in_world(self) -> bool {
                let lo = -Self::HALF_EXTENT;
                let hi = Self::HALF_EXTENT;
                self.x >= lo
                    && self.x < hi
                    && self.y >= lo
                    && self.y < hi
                    && self.z >= lo
                    && self.z < hi
            }
        }

        impl From<(i32, i32, i32)> for $name {
            fn from((x, y, z): (i32, i32, i32)) -> Self {
                Self::new(x, y, z)
            }
        }
    };
}

coord_type!(
    ChunkPos,
    CoordKind::Chunk,
    WORLD_HALF_EXTENT_CHUNKS,
    "Position of a 16³-block chunk, in chunk units.\n\nCharter rule 7 makes this the stable half of an authoritative position: the pair `(ChunkPos, f32 local)` is a floating origin, so world-space `f32` is never accumulated and precision does not decay with distance from the origin."
);

coord_type!(
    BlockPos,
    CoordKind::Block,
    WORLD_HALF_EXTENT_BLOCKS,
    "Position of a single block, in blocks, relative to the world origin."
);

coord_type!(
    SubNodePos,
    CoordKind::SubNode,
    WORLD_HALF_EXTENT_SUBNODES,
    "Position of a single sub-node cell, in sub-nodes, relative to the world origin.\n\nThere are 3 sub-nodes per block along each axis and 27 per block in total (charter rule 5)."
);

impl BlockPos {
    /// The chunk containing this block.
    #[must_use]
    pub const fn chunk(self) -> ChunkPos {
        ChunkPos::new(
            self.x.div_euclid(CHUNK_BLOCKS as i32),
            self.y.div_euclid(CHUNK_BLOCKS as i32),
            self.z.div_euclid(CHUNK_BLOCKS as i32),
        )
    }

    /// This block's offset within its chunk, each axis in `0..16`.
    #[must_use]
    pub const fn local(self) -> LocalBlock {
        LocalBlock {
            x: self.x.rem_euclid(CHUNK_BLOCKS as i32) as u32,
            y: self.y.rem_euclid(CHUNK_BLOCKS as i32) as u32,
            z: self.z.rem_euclid(CHUNK_BLOCKS as i32) as u32,
        }
    }

    /// The sub-node at offset `(dx, dy, dz)` within this block.
    ///
    /// Each offset is expected in `0..3`; larger values simply address a
    /// neighbouring block's sub-nodes, which is occasionally what a caller
    /// wants.
    #[must_use]
    pub const fn subnode(self, dx: i32, dy: i32, dz: i32) -> SubNodePos {
        SubNodePos::new(
            self.x * SUBNODES_PER_AXIS as i32 + dx,
            self.y * SUBNODES_PER_AXIS as i32 + dy,
            self.z * SUBNODES_PER_AXIS as i32 + dz,
        )
    }

    /// The block at the origin corner of a chunk.
    #[must_use]
    pub const fn from_chunk_corner(chunk: ChunkPos) -> Self {
        Self::new(
            chunk.x * CHUNK_BLOCKS as i32,
            chunk.y * CHUNK_BLOCKS as i32,
            chunk.z * CHUNK_BLOCKS as i32,
        )
    }
}

impl SubNodePos {
    /// The block containing this sub-node.
    #[must_use]
    pub const fn block(self) -> BlockPos {
        BlockPos::new(
            self.x.div_euclid(SUBNODES_PER_AXIS as i32),
            self.y.div_euclid(SUBNODES_PER_AXIS as i32),
            self.z.div_euclid(SUBNODES_PER_AXIS as i32),
        )
    }

    /// The chunk containing this sub-node.
    #[must_use]
    pub const fn chunk(self) -> ChunkPos {
        ChunkPos::new(
            self.x.div_euclid(CHUNK_SUBNODES as i32),
            self.y.div_euclid(CHUNK_SUBNODES as i32),
            self.z.div_euclid(CHUNK_SUBNODES as i32),
        )
    }

    /// This sub-node's offset within its block, each axis in `0..3`.
    ///
    /// Feed the result to [`crate::block::subnode_index`] to get the canonical
    /// index into a block's 27 cells.
    #[must_use]
    pub const fn local(self) -> (u32, u32, u32) {
        (
            self.x.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            self.y.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            self.z.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
        )
    }
}

/// A block's offset within its chunk, each axis in `0..16`.
///
/// Distinct from [`BlockPos`] on purpose: mixing world and chunk-local
/// coordinates is the classic voxel bug, and it produces plausible-looking
/// results rather than obvious ones. The type system separates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LocalBlock {
    /// East-west offset, `0..16`.
    pub x: u32,
    /// Vertical offset, `0..16`.
    pub y: u32,
    /// North-south offset, `0..16`.
    pub z: u32,
}

impl LocalBlock {
    /// Constructs from raw offsets.
    ///
    /// # Panics
    ///
    /// In debug builds, if any axis is 16 or greater.
    #[must_use]
    pub const fn new(x: u32, y: u32, z: u32) -> Self {
        debug_assert!(x < CHUNK_BLOCKS && y < CHUNK_BLOCKS && z < CHUNK_BLOCKS);
        Self { x, y, z }
    }

    /// Flat index into a chunk's 4096 block slots.
    ///
    /// The layout is x-major: `x + 16 * y + 256 * z`, matching the sub-node
    /// convention in [`crate::block::subnode_index`] so the two never need to
    /// be reasoned about separately.
    #[must_use]
    pub const fn index(self) -> usize {
        (self.x + CHUNK_BLOCKS * self.y + CHUNK_BLOCKS * CHUNK_BLOCKS * self.z) as usize
    }

    /// Inverse of [`Self::index`].
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        let index = index as u32;
        Self {
            x: index % CHUNK_BLOCKS,
            y: (index / CHUNK_BLOCKS) % CHUNK_BLOCKS,
            z: index / (CHUNK_BLOCKS * CHUNK_BLOCKS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_to_chunk_is_uniform_across_the_origin() {
        // The bug this guards: truncating division puts blocks -15..0 in one
        // 16-wide chunk and -31..-16 in the next, leaving chunk 0 spanning 31
        // blocks. Every one of these must land in a 16-block chunk.
        assert_eq!(BlockPos::new(0, 0, 0).chunk(), ChunkPos::new(0, 0, 0));
        assert_eq!(BlockPos::new(15, 0, 0).chunk(), ChunkPos::new(0, 0, 0));
        assert_eq!(BlockPos::new(16, 0, 0).chunk(), ChunkPos::new(1, 0, 0));
        assert_eq!(BlockPos::new(-1, 0, 0).chunk(), ChunkPos::new(-1, 0, 0));
        assert_eq!(BlockPos::new(-16, 0, 0).chunk(), ChunkPos::new(-1, 0, 0));
        assert_eq!(BlockPos::new(-17, 0, 0).chunk(), ChunkPos::new(-2, 0, 0));
    }

    #[test]
    fn local_offsets_are_never_negative() {
        assert_eq!(
            BlockPos::new(-1, -1, -1).local(),
            LocalBlock::new(15, 15, 15)
        );
        assert_eq!(
            BlockPos::new(-16, -16, -16).local(),
            LocalBlock::new(0, 0, 0)
        );
        assert_eq!(SubNodePos::new(-1, -2, -3).local(), (2, 1, 0));
    }

    #[test]
    fn every_chunk_holds_exactly_sixteen_blocks_per_axis() {
        for x in -40i32..40 {
            let chunk = BlockPos::new(x, 0, 0).chunk();
            let local = BlockPos::new(x, 0, 0).local();
            // Reconstruct and check we get back where we started.
            let rebuilt = chunk.x * CHUNK_BLOCKS as i32 + local.x as i32;
            assert_eq!(rebuilt, x, "round trip failed at x={x}");
        }
    }

    #[test]
    fn subnode_to_block_and_chunk_agree() {
        for x in -100i32..100 {
            let subnode = SubNodePos::new(x, 0, 0);
            assert_eq!(
                subnode.block().chunk(),
                subnode.chunk(),
                "block-then-chunk disagreed with direct chunk at x={x}"
            );
        }
    }

    #[test]
    fn block_subnode_round_trips() {
        let block = BlockPos::new(-5, 7, -13);
        for dz in 0..3 {
            for dy in 0..3 {
                for dx in 0..3 {
                    let subnode = block.subnode(dx, dy, dz);
                    assert_eq!(subnode.block(), block);
                    assert_eq!(subnode.local(), (dx as u32, dy as u32, dz as u32));
                }
            }
        }
    }

    #[test]
    fn world_bounds_are_half_open() {
        assert!(BlockPos::checked(-WORLD_HALF_EXTENT_BLOCKS, 0, 0).is_ok());
        assert!(BlockPos::checked(WORLD_HALF_EXTENT_BLOCKS - 1, 0, 0).is_ok());
        assert!(BlockPos::checked(WORLD_HALF_EXTENT_BLOCKS, 0, 0).is_err());
        assert!(BlockPos::checked(-WORLD_HALF_EXTENT_BLOCKS - 1, 0, 0).is_err());

        // Exactly 120,000 blocks per axis, per charter rule 6.
        let span = i64::from(WORLD_HALF_EXTENT_BLOCKS) * 2;
        assert_eq!(span, 120_000);
    }

    #[test]
    fn bounds_scale_consistently_across_resolutions() {
        assert_eq!(WORLD_HALF_EXTENT_SUBNODES, 180_000);
        assert_eq!(WORLD_HALF_EXTENT_CHUNKS, 3_750);

        // The corner block of the last valid chunk must itself be valid.
        let last_chunk = ChunkPos::new(
            WORLD_HALF_EXTENT_CHUNKS - 1,
            WORLD_HALF_EXTENT_CHUNKS - 1,
            WORLD_HALF_EXTENT_CHUNKS - 1,
        );
        assert!(last_chunk.in_world());
        assert!(BlockPos::from_chunk_corner(last_chunk).in_world());
    }

    #[test]
    fn out_of_world_errors_name_the_resolution() {
        let err = SubNodePos::checked(WORLD_HALF_EXTENT_SUBNODES, 0, 0).expect_err("out of world");
        assert_eq!(err.kind, CoordKind::SubNode);
        assert!(err.to_string().contains("sub-node"));
    }

    #[test]
    fn local_block_index_round_trips_over_the_whole_chunk() {
        for index in 0..crate::BLOCKS_PER_CHUNK {
            assert_eq!(LocalBlock::from_index(index).index(), index);
        }
    }
}
