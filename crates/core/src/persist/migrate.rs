// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Forward migration of chunk blobs, one version step at a time.
//!
//! # The shape
//!
//! Migration is a **chain of v→v+1 steps**, not a set of v→current jumps. Each
//! step only has to know about two adjacent formats, which is the difference
//! between adding a format version being a small local change and being a
//! rewrite of every previous migration.
//!
//! Chunks migrate **lazily on load** and are rewritten at the next save. A
//! world with a million chunks does not pay a migration cost at startup, and
//! chunks nobody visits are never touched — they still decode correctly years
//! later because their step is still in the chain.
//!
//! **Steps are append-only and permanent.** Deleting the v0→v1 step orphans
//! every world that still has a v0 chunk in it.
//!
//! # The v0 step is deliberately synthetic
//!
//! `v0` is not a format that ever shipped — Task 03 is the first persistence
//! code and its output is v1. It exists so the chain is exercised by a real
//! migration from the very first commit, with a real decode of a real older
//! shape, rather than being an untested framework that gets its first workout
//! during an actual format change under pressure.
//!
//! The difference is genuine and representative: v0 has no `mixed_free` field,
//! because a first cut would plausibly not have modelled reclaimed mixed slots.

use serde::{Deserialize, Serialize};

use crate::block::{BlockContent, Cells};
use crate::persist::codec::ChunkPayload;

/// A migration step could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// No step exists for this version.
    #[error("no migration step from chunk format version {from}")]
    NoStep {
        /// Version with no step.
        from: u8,
    },

    /// A blob failed to decode at the version it claimed.
    #[error("chunk blob does not decode as format version {version}")]
    Decode {
        /// Version attempted.
        version: u8,
        /// Underlying error.
        #[source]
        source: postcard::Error,
    },
}

/// The v0 chunk payload: v1 without reclaimed mixed slots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChunkPayloadV0 {
    pub palette: Vec<(BlockContent, u32)>,
    pub bits_per_index: u8,
    pub index_words: Vec<u64>,
    pub mixed_cells: Vec<Cells>,
}

/// Migrates a decompressed payload from `version` up to the current format.
///
/// Applies one step at a time until it reaches [`CHUNK_FORMAT_VERSION`].
///
/// [`CHUNK_FORMAT_VERSION`]: crate::persist::codec::CHUNK_FORMAT_VERSION
///
/// # Errors
///
/// [`MigrationError`] if a step is missing or a blob does not decode at the
/// version it claims.
pub fn migrate_chunk(version: u8, serialised: &[u8]) -> Result<ChunkPayload, MigrationError> {
    match version {
        0 => {
            let v0: ChunkPayloadV0 = postcard::from_bytes(serialised)
                .map_err(|source| MigrationError::Decode { version: 0, source })?;
            Ok(v0_to_v1(v0))
        }
        // When v2 arrives: decode as v1, apply v1_to_v2, and make the v0 arm
        // chain through it rather than returning directly.
        other => Err(MigrationError::NoStep { from: other }),
    }
}

/// v0 → v1: reclaimed mixed slots become explicit.
///
/// A v0 blob has no free list because v0 never reclaimed a slot, so every slot
/// it holds is live. The empty free list is therefore not a default — it is the
/// correct value, and this is the whole content of the step.
fn v0_to_v1(v0: ChunkPayloadV0) -> ChunkPayload {
    ChunkPayload {
        palette: v0.palette,
        bits_per_index: v0.bits_per_index,
        index_words: v0.index_words,
        mixed_cells: v0.mixed_cells,
        mixed_free: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::ChunkPos;
    use crate::material::MaterialRegistry;
    use crate::persist::codec::{CHUNK_FORMAT_VERSION, NO_DICTIONARY, ZSTD_LEVEL, decode_chunk};
    use crate::persist::idmap::IdTable;
    use crate::persist::schema;
    use crate::{Chunk, MaterialId, Registry};
    use rusqlite::Connection;

    /// Writes a blob in the retired v0 format, as an older engine would have.
    fn write_v0_blob(payload: &ChunkPayloadV0) -> Vec<u8> {
        let serialised = postcard::to_allocvec(payload).expect("serialise");
        let compressed = zstd::bulk::compress(&serialised, ZSTD_LEVEL).expect("compress");
        let mut blob = vec![0u8, NO_DICTIONARY];
        blob.extend_from_slice(&compressed);
        blob
    }

    #[test]
    fn the_v0_step_produces_a_loadable_chunk() {
        // The migration gate. A blob in a format this build no longer writes
        // must still load, all the way through to a valid Chunk.
        let conn = Connection::open_in_memory().expect("open");
        schema::create(&conn).expect("schema");
        let mut registry = Registry::new();
        registry.register("core:stone").expect("register");
        let mut table = IdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut registry).expect("reconcile");
        let stone_world = table.id_of("core:stone").expect("mapped");

        // A uniform chunk of stone, expressed in v0's shape with world ids.
        let v0 = ChunkPayloadV0 {
            palette: vec![(
                BlockContent::Uniform(MaterialId(stone_world)),
                crate::BLOCKS_PER_CHUNK as u32,
            )],
            bits_per_index: 0,
            index_words: Vec::new(),
            mixed_cells: Vec::new(),
        };

        let blob = write_v0_blob(&v0);
        assert_eq!(blob[0], 0, "the blob must claim version 0");

        let pos = ChunkPos::new(0, 0, 0);
        let chunk = decode_chunk(pos, &blob, &map, &[]).expect("v0 blob should migrate and load");

        let stone_runtime = registry.id_of("core:stone").expect("registered");
        assert_eq!(chunk.is_uniform(), Some(stone_runtime));
        assert_eq!(chunk, Chunk::new(pos, stone_runtime));
    }

    #[test]
    fn a_migrated_chunk_re_encodes_at_the_current_version() {
        // Lazy migration means the rewrite happens on the next save. Proving
        // the re-encode lands at the current version is what makes the old
        // format eventually disappear from a live world.
        let conn = Connection::open_in_memory().expect("open");
        schema::create(&conn).expect("schema");
        let mut registry = Registry::new();
        registry.register("core:stone").expect("register");
        let mut table = IdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut registry).expect("reconcile");
        let stone_world = table.id_of("core:stone").expect("mapped");

        let v0 = ChunkPayloadV0 {
            palette: vec![(
                BlockContent::Uniform(MaterialId(stone_world)),
                crate::BLOCKS_PER_CHUNK as u32,
            )],
            bits_per_index: 0,
            index_words: Vec::new(),
            mixed_cells: Vec::new(),
        };

        let pos = ChunkPos::new(0, 0, 0);
        let chunk = decode_chunk(pos, &write_v0_blob(&v0), &map, &[]).expect("migrate");
        let re_encoded =
            crate::persist::codec::encode_chunk(&chunk, &map, None).expect("re-encode");
        assert_eq!(re_encoded[0], CHUNK_FORMAT_VERSION);
    }

    #[test]
    fn the_step_only_adds_the_missing_field() {
        let v0 = ChunkPayloadV0 {
            palette: vec![(BlockContent::Uniform(MaterialId(3)), 4096)],
            bits_per_index: 2,
            index_words: vec![7, 9],
            mixed_cells: vec![[MaterialId(3); 27]],
        };
        let v1 = v0_to_v1(v0.clone());
        assert_eq!(v1.palette, v0.palette);
        assert_eq!(v1.bits_per_index, v0.bits_per_index);
        assert_eq!(v1.index_words, v0.index_words);
        assert_eq!(v1.mixed_cells, v0.mixed_cells);
        assert!(v1.mixed_free.is_empty(), "v0 never reclaimed a slot");
    }

    #[test]
    fn an_unknown_version_is_refused() {
        assert!(matches!(
            migrate_chunk(200, &[]),
            Err(MigrationError::NoStep { from: 200 })
        ));
    }

    #[test]
    fn a_v0_blob_that_is_not_v0_shaped_is_an_error_not_a_panic() {
        let garbage = zstd::bulk::compress(&[0xFFu8; 64], ZSTD_LEVEL).expect("compress");
        assert!(migrate_chunk(0, &garbage).is_err());
    }
}
