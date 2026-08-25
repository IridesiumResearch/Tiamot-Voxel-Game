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
//! Entity blobs migrate through the same shape, in [`migrate_entity`], and
//! keep their own version — the two formats change for entirely different
//! reasons and a shared number would migrate every world for a change that
//! touched nothing it holds.
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
///
/// Shared by chunks and entities: the two chains are separate and their
/// failures are the same two.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// No step exists for this version.
    #[error("no migration step from format version {from}")]
    NoStep {
        /// Version with no step.
        from: u8,
    },

    /// A blob failed to decode at the version it claimed.
    #[error("a blob does not decode as format version {version}")]
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

/// The v1 entity: v2 without the stack an item on the ground is made of.
///
/// **A copy of the struct as it was**, not the live one with a field removed.
/// The live struct will keep growing, and a step that referred to it would
/// silently start decoding a shape that never existed on disk.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct EntityV1 {
    transform: crate::ent::Transform,
    velocity: crate::ent::Velocity,
    collider: Option<crate::ent::Collider>,
    model: Option<String>,
    health: Option<crate::ent::Health>,
    nametag: Option<crate::ent::Nametag>,
    owner: Option<crate::ent::Owner>,
    source: String,
    script: Option<Vec<u8>>,
}

/// Brings one entity blob forward to [`crate::persist::ENTITY_FORMAT_VERSION`].
///
/// # Errors
///
/// [`MigrationError::NoStep`] for a version with no step, and
/// [`MigrationError::Decode`] for a blob that does not decode at the version it
/// claims to be.
pub fn migrate_entity(
    version: u8,
    serialised: &[u8],
) -> Result<crate::ent::Entity, MigrationError> {
    match version {
        1 => {
            let v1: EntityV1 = postcard::from_bytes(serialised)
                .map_err(|source| MigrationError::Decode { version: 1, source })?;
            Ok(v1_to_v2(v1))
        }
        other => Err(MigrationError::NoStep { from: other }),
    }
}

/// v1 → v2: an entity can be a stack lying on the ground.
///
/// Nothing written before v2 was an item, because there was no way to be one.
/// `None` is therefore the correct value rather than a default that happens to
/// be convenient.
fn v1_to_v2(v1: EntityV1) -> crate::ent::Entity {
    crate::ent::Entity {
        transform: v1.transform,
        velocity: v1.velocity,
        on_ground: false,
        drive: crate::phys::Intent::default(),
        collider: v1.collider,
        model: v1.model,
        item: None,
        anim: crate::ent::AnimTag::default(),
        health: v1.health,
        nametag: v1.nametag,
        owner: v1.owner,
        source: v1.source,
        script: v1.script,
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
    fn a_v1_entity_reads_back_as_a_v2_one_carrying_no_item() {
        // **The step that a wrong comment nearly skipped.** `postcard` is not
        // self-describing, so a blob written before `item` existed simply runs
        // out of bytes — the counter-example below is the whole reason this
        // needed a migration rather than an appended field.
        #[derive(serde::Serialize)]
        struct WriteV1 {
            transform: crate::ent::Transform,
            velocity: crate::ent::Velocity,
            collider: Option<crate::ent::Collider>,
            model: Option<String>,
            health: Option<crate::ent::Health>,
            nametag: Option<crate::ent::Nametag>,
            owner: Option<crate::ent::Owner>,
            source: String,
            script: Option<Vec<u8>>,
        }
        let written = WriteV1 {
            transform: crate::ent::Transform::from_world(4.0, 5.0, 6.0),
            velocity: crate::ent::Velocity([1.0, 0.0, -1.0]),
            collider: Some(crate::ent::Collider {
                width: 1.8,
                height: 5.4,
            }),
            model: Some("engine:humanoid".to_owned()),
            health: Some(crate::ent::Health::full(20)),
            nametag: None,
            owner: None,
            source: "core_mimic".to_owned(),
            script: Some(vec![1, 2, 3]),
        };
        let blob = postcard::to_allocvec(&written).expect("encode");

        // The counter-example first: the CURRENT struct cannot read it.
        assert!(
            postcard::from_bytes::<crate::ent::Entity>(&blob).is_err(),
            "a v1 blob decoded as v2, so this migration is not needed and the \
             comment that said an append was safe was right after all"
        );

        let entity = migrate_entity(1, &blob).expect("the step exists");
        assert_eq!(entity.model.as_deref(), Some("engine:humanoid"));
        assert_eq!(entity.source, "core_mimic");
        assert_eq!(entity.script, Some(vec![1, 2, 3]));
        assert_eq!(entity.health, Some(crate::ent::Health::full(20)));
        assert!(
            entity.item.is_none(),
            "nothing written before v2 could be an item"
        );
    }

    #[test]
    fn an_entity_version_with_no_step_is_refused() {
        assert!(matches!(
            migrate_entity(200, &[]),
            Err(MigrationError::NoStep { from: 200 })
        ));
        assert!(
            migrate_entity(1, &[0xFF; 4]).is_err(),
            "a blob that is not v1-shaped is an error, not a panic"
        );
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
