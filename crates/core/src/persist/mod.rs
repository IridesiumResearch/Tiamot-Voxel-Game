// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! World persistence: one `SQLite` database per world.
//!
//! # Why `SQLite` and not a custom region format
//!
//! A save file is the single artefact a player cannot afford to lose, and
//! crash safety is genuinely hard. `SQLite` has spent twenty years on exactly
//! that problem, ships a WAL that survives process death, and gives us atomic
//! multi-table transactions for free — a chunk save and its entity rows commit
//! together or not at all.
//!
//! The cost is a C dependency and some per-row overhead. Both are acceptable
//! for something that runs a few times a second and holds everything the player
//! has built.
//!
//! # Layout
//!
//! - [`schema`] — tables, pragmas, and the WAL checkpoint policy
//! - [`idmap`] — string ⇄ world numeric material ids, and mod-churn survival
//! - [`codec`] — chunk blob encoding, `postcard` + `zstd` behind a version byte
//! - [`migrate`] — the v→v+1 blob migration chain
//!
//! # Everything on disk is untrusted
//!
//! Not because a world file arrives from a stranger, but because it can be
//! truncated by a full disk, corrupted by failing hardware, or hand-edited by
//! someone curious. Every decode path returns an error rather than panicking,
//! and every structural invariant is re-checked on load rather than assumed —
//! see [`crate::chunk::CorruptChunk`].

pub mod codec;
pub mod containers;
pub mod fluidmap;
pub mod idmap;
pub mod migrate;
pub mod playerdata;
pub mod schema;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::chunk::Chunk;
use crate::coords::ChunkPos;
use crate::fluid::FluidLayer;
use crate::material::Registry;
use crate::persist::codec::{CodecError, Dictionary};
use crate::persist::fluidmap::{FluidIdTable, FluidMap, FluidMapError};
use crate::persist::idmap::{IdMapError, IdTable, MaterialMap};

pub use crate::persist::schema::DEFAULT_DOMAIN;

/// Meta keys the engine owns.
pub mod meta_keys {
    /// The world's generation seed.
    pub const WORLD_SEED: &str = "world_seed";
    /// Schema version, as [`super::schema::SCHEMA_VERSION`].
    pub const SCHEMA_VERSION: &str = "schema_version";
    /// Engine version that last wrote this world.
    pub const ENGINE_VERSION: &str = "engine_version";
    /// Unix timestamp of world creation.
    pub const CREATED_AT: &str = "created_at";
    /// Human-readable world name.
    pub const WORLD_NAME: &str = "world_name";
    /// Domain instances created at runtime, as `instance\ttemplate` lines.
    ///
    /// Which instances exist has to survive a restart or a ship somebody built
    /// would be a domain nothing could name the next morning. See
    /// [`crate::domain`].
    pub const DOMAIN_INSTANCES: &str = "domain_instances";
}

/// The format an entity blob is written in.
///
/// The `version` column of the `entities` table, and its own number rather than
/// [`codec::CHUNK_FORMAT_VERSION`] because the two blobs change for entirely
/// different reasons — adding a field to a block does not move an entity's
/// bytes, and a shared number would force a migration on every world for a
/// change that touched nothing it holds.
///
/// **Appending a struct FIELD is not safe, whatever this used to say.**
/// `postcard` is not self-describing: a decoder reads fields in order and stops
/// where the struct ends, so a blob written before a field existed runs out of
/// bytes and fails with "hit the end of buffer". Measured, not assumed — the
/// claim that an append was safe stood here from Task 03 until an entity grew
/// its first new field, which is exactly when a wrong rule about migrations
/// costs somebody their world.
///
/// Appending an enum VARIANT is safe, because a variant nothing has written
/// yet appears in no existing blob.
///
/// So: any change to [`crate::ent::Entity`]'s persisted fields bumps this and
/// adds a step to [`migrate::migrate_entity`].
pub const ENTITY_FORMAT_VERSION: u8 = 2;

/// Anything that can go wrong talking to a world database.
#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    /// A SQL or connection failure.
    #[error("world database error")]
    Sql(#[from] rusqlite::Error),

    /// A chunk blob could not be encoded or decoded.
    #[error("chunk at ({}, {}, {}) in domain `{domain}`", pos.x, pos.y, pos.z)]
    Chunk {
        /// Which chunk.
        pos: ChunkPos,
        /// Which domain.
        domain: String,
        /// Why.
        #[source]
        source: CodecError,
    },

    /// A stored fluid layer could not be decoded.
    ///
    /// Separate from [`WorldError::Chunk`] because the two blobs are separate
    /// rows with separate formats: a world whose terrain reads perfectly can
    /// still have one unreadable pond, and saying so is more useful than
    /// reporting the chunk as broken.
    #[error("fluid for chunk at ({}, {}, {}) in domain `{domain}`", pos.x, pos.y, pos.z)]
    Fluid {
        /// Which chunk.
        pos: ChunkPos,
        /// Which domain.
        domain: String,
        /// Why.
        #[source]
        source: crate::fluid::codec::FluidDecodeError,
    },

    /// A stored entity could not be encoded or decoded.
    ///
    /// Its own variant rather than folding into [`WorldError::Chunk`] for the
    /// same reason fluid has one: a chunk whose terrain reads perfectly can
    /// still have one unreadable mob in it, and saying which is more useful
    /// than condemning the chunk.
    #[error("entity in chunk ({}, {}, {}) in domain `{domain}`: {reason}", pos.x, pos.y, pos.z)]
    Entity {
        /// Which chunk it was anchored to.
        pos: ChunkPos,
        /// Which domain.
        domain: String,
        /// Why.
        reason: String,
    },

    /// A stored mod value could not be encoded or decoded.
    #[error("mod `{mod_id}` storage key `{key}`: {reason}")]
    ModStorage {
        /// Which mod.
        mod_id: String,
        /// Which key.
        key: String,
        /// Why.
        reason: String,
    },

    /// A stored player row could not be read.
    #[error("player `{player}`: {reason}")]
    Player {
        /// Whose row, as hex.
        player: String,
        /// Why.
        reason: String,
    },

    /// Material id mapping failed.
    #[error("material id mapping failed")]
    Materials(#[from] IdMapError),

    /// Fluid id mapping failed.
    #[error("fluid id mapping failed")]
    Fluids(#[from] FluidMapError),

    /// A fluid id could not be translated between the session and the world.
    #[error("fluid for chunk at ({}, {}, {})", pos.x, pos.y, pos.z)]
    FluidId {
        /// Which chunk.
        pos: ChunkPos,
        /// Why.
        #[source]
        source: crate::persist::fluidmap::UnmappedFluid,
    },

    /// The world was written by an incompatible schema version.
    #[error(
        "world uses schema version {found}, but this build understands {expected} — \
         the world was created by a different engine version"
    )]
    SchemaVersion {
        /// Version found in the file.
        found: i64,
        /// Version this build writes.
        expected: i64,
    },
}

/// The same block, under a different fluid id.
///
/// Preserves the volume exactly — only the id bits move between the session's
/// numbering and the world's. A pond that came back holding a different amount
/// than it was saved with would be a conservation failure that survived a
/// restart, which is the hardest kind to find.
fn retag(value: crate::fluid::Fluid, fluid: crate::fluid::FluidId) -> crate::fluid::Fluid {
    crate::fluid::Fluid::new(fluid, value.volume())
}

/// An open world database.
///
/// Holds the connection, the reconciled material mapping for this session, and
/// any `zstd` dictionaries the world carries.
pub struct WorldDb {
    conn: Connection,
    path: PathBuf,
    ids: IdTable,
    materials: MaterialMap,
    /// The world's fluid name ⇄ id table, loaded at open.
    fluid_ids: FluidIdTable,
    /// Session ⇄ world fluid translation.
    ///
    /// **The identity until [`WorldDb::reconcile_fluids`] is called**, because
    /// fluids are registered during mod load and a world is opened before that
    /// — there is nothing to reconcile against yet. A world that holds fluid and
    /// was never reconciled is a caller bug, and the save path says so rather
    /// than writing session ids to disk, which is the defect this whole
    /// mechanism exists to remove.
    fluids: Option<FluidMap>,
    dictionaries: Vec<Dictionary>,
}

impl WorldDb {
    /// Opens a world, creating it if absent, and reconciles material ids.
    ///
    /// `registry` is mutated: materials the world knows but no mod registered
    /// this session are added as behaviourless aliases so their blocks
    /// round-trip (charter rule 8). Pass the registry **after** the
    /// registration window has closed and frozen (charter rule 9).
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the file cannot be opened, the schema cannot be
    /// created, or the world's schema version is not the one this build writes.
    pub fn open(path: impl AsRef<Path>, registry: &mut Registry) -> Result<Self, WorldError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|err| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(format!("could not create world directory: {err}")),
                )
            })?;
        }

        let conn = Connection::open(&path)?;
        Self::from_connection(conn, path, registry)
    }

    /// Opens an in-memory world. Test and singleplayer-preview use only.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_in_memory(registry: &mut Registry) -> Result<Self, WorldError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, PathBuf::from(":memory:"), registry)
    }

    fn from_connection(
        conn: Connection,
        path: PathBuf,
        registry: &mut Registry,
    ) -> Result<Self, WorldError> {
        schema::apply_pragmas(&conn)?;
        schema::create(&conn)?;

        // Reject a world from a different schema generation rather than
        // half-reading it. Blob formats migrate; table layouts do not, yet.
        match Self::read_meta_i64(&conn, meta_keys::SCHEMA_VERSION)? {
            None => Self::write_meta_i64(&conn, meta_keys::SCHEMA_VERSION, schema::SCHEMA_VERSION)?,
            Some(found) if found != schema::SCHEMA_VERSION => {
                return Err(WorldError::SchemaVersion {
                    found,
                    expected: schema::SCHEMA_VERSION,
                });
            }
            Some(_) => {}
        }

        Self::write_meta_str(&conn, meta_keys::ENGINE_VERSION, env!("CARGO_PKG_VERSION"))?;

        let mut ids = IdTable::load(&conn)?;
        let materials = ids.reconcile(&conn, registry)?;
        let fluid_ids = FluidIdTable::load(&conn)?;

        Ok(Self {
            conn,
            path,
            ids,
            materials,
            fluid_ids,
            fluids: None,
            dictionaries: Vec::new(),
        })
    }

    /// The world file's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This session's material translation.
    #[must_use]
    pub fn materials(&self) -> &MaterialMap {
        &self.materials
    }

    /// The world's string ⇄ numeric id table.
    #[must_use]
    pub fn ids(&self) -> &IdTable {
        &self.ids
    }

    // -- chunks -----------------------------------------------------------

    /// Loads a chunk from the default domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable blob.
    pub fn load_chunk(&self, pos: ChunkPos) -> Result<Option<Chunk>, WorldError> {
        self.load_chunk_in(DEFAULT_DOMAIN, pos)
    }

    /// Loads a chunk from a named domain.
    ///
    /// Task 15a will use this; until then every caller goes through
    /// [`Self::load_chunk`]. The parameter exists now so 15a is additive at the
    /// call sites as well as in the schema.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable blob.
    pub fn load_chunk_in(&self, domain: &str, pos: ChunkPos) -> Result<Option<Chunk>, WorldError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM chunks WHERE domain = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
                params![domain, pos.x, pos.y, pos.z],
                |row| row.get(0),
            )
            .optional()?;

        let Some(blob) = blob else {
            return Ok(None);
        };

        codec::decode_chunk(pos, &blob, &self.materials, &self.dictionaries)
            .map(Some)
            .map_err(|source| WorldError::Chunk {
                pos,
                domain: domain.to_owned(),
                source,
            })
    }

    /// Saves a chunk to the default domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable chunk.
    pub fn save_chunk(&self, pos: ChunkPos, chunk: &Chunk) -> Result<(), WorldError> {
        self.save_chunk_in(DEFAULT_DOMAIN, pos, chunk)
    }

    /// Saves a chunk to a named domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable chunk.
    pub fn save_chunk_in(
        &self,
        domain: &str,
        pos: ChunkPos,
        chunk: &Chunk,
    ) -> Result<(), WorldError> {
        let blob = self.encode(domain, pos, chunk)?;
        self.conn.execute(
            "INSERT INTO chunks (domain, x, y, z, version, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(domain, x, y, z) DO UPDATE SET version = excluded.version,
                                                        data = excluded.data",
            params![
                domain,
                pos.x,
                pos.y,
                pos.z,
                i64::from(codec::CHUNK_FORMAT_VERSION),
                blob
            ],
        )?;
        Ok(())
    }

    /// Saves many chunks in a single transaction.
    ///
    /// One transaction rather than one per chunk, because a per-chunk
    /// transaction means a WAL commit per chunk and a save of a thousand chunks
    /// would spend all its time in fsync bookkeeping. It is also the atomicity
    /// the caller usually wants: a batch either lands or it does not.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable chunk. The transaction
    /// rolls back, so a failed batch changes nothing.
    pub fn save_chunks_batch<'a>(
        &mut self,
        chunks: impl IntoIterator<Item = (ChunkPos, &'a Chunk)>,
    ) -> Result<usize, WorldError> {
        self.save_chunks_batch_in(DEFAULT_DOMAIN, chunks)
    }

    /// Saves many chunks to a named domain in a single transaction.
    ///
    /// # Errors
    ///
    /// As [`Self::save_chunks_batch`].
    pub fn save_chunks_batch_in<'a>(
        &mut self,
        domain: &str,
        chunks: impl IntoIterator<Item = (ChunkPos, &'a Chunk)>,
    ) -> Result<usize, WorldError> {
        // Encode before opening the transaction: compression is the slow part
        // and holding a write transaction across it would block checkpoints for
        // no reason.
        let encoded = chunks
            .into_iter()
            .map(|(pos, chunk)| self.encode(domain, pos, chunk).map(|blob| (pos, blob)))
            .collect::<Result<Vec<_>, _>>()?;

        let transaction = self.conn.transaction()?;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO chunks (domain, x, y, z, version, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(domain, x, y, z) DO UPDATE SET version = excluded.version,
                                                            data = excluded.data",
            )?;
            for (pos, blob) in &encoded {
                statement.execute(params![
                    domain,
                    pos.x,
                    pos.y,
                    pos.z,
                    i64::from(codec::CHUNK_FORMAT_VERSION),
                    blob
                ])?;
            }
        }
        transaction.commit()?;
        Ok(encoded.len())
    }

    // -- fluid ------------------------------------------------------------

    /// Gives this world's fluids stable ids, and adopts what it already knew.
    ///
    /// **Call this once, after the mods have registered their fluids and before
    /// anything reads or writes a fluid layer.** It cannot happen inside
    /// [`WorldDb::open`] the way the material reconcile does, because fluids are
    /// registered during mod load and the world is opened before that.
    ///
    /// `fluids` is mutated exactly as `registry` is by the material reconcile:
    /// a fluid the world knows but no mod registered this session is added as an
    /// inert placeholder so its stored bytes round-trip (charter rule 8). See
    /// [`crate::fluid::Fluids::register_placeholder`].
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure, or if the world has already used all
    /// fifteen fluid ids and a newly registered fluid cannot be given one.
    pub fn reconcile_fluids(
        &mut self,
        fluids: &mut crate::fluid::Fluids,
    ) -> Result<(), WorldError> {
        self.fluids = Some(self.fluid_ids.reconcile(&self.conn, fluids)?);
        Ok(())
    }

    /// Whether [`WorldDb::reconcile_fluids`] has run.
    #[must_use]
    pub const fn fluids_reconciled(&self) -> bool {
        self.fluids.is_some()
    }

    /// The translation, or the identity for a world with no fluid at all.
    ///
    /// A world whose mods registered nothing and which has never stored a fluid
    /// has an empty table and an identity map, so the common case needs no
    /// reconcile call at all — which is what keeps every existing test and every
    /// fluid-free world working untouched.
    fn fluid_map(&self) -> FluidMap {
        self.fluids.clone().unwrap_or_default()
    }

    /// Rewrites a layer's fluid ids from one id space into the other.
    ///
    /// A fresh layer rather than an in-place edit: the caller's layer is the
    /// live one the simulation is holding, and rewriting it into world ids would
    /// leave the running world speaking the wrong numbers.
    fn translate(
        layer: &FluidLayer,
        mut convert: impl FnMut(crate::fluid::Fluid) -> Result<crate::fluid::Fluid, WorldError>,
    ) -> Result<FluidLayer, WorldError> {
        let mut translated = Vec::with_capacity(crate::BLOCKS_PER_CHUNK);
        for value in layer.blocks() {
            translated.push(convert(value)?);
        }
        Ok(FluidLayer::from_blocks(translated))
    }

    /// A layer in this session's ids, as the ids this world stores.
    fn to_world_ids(&self, pos: ChunkPos, layer: &FluidLayer) -> Result<FluidLayer, WorldError> {
        let map = self.fluid_map();
        Self::translate(layer, |value| {
            if value.is_empty() {
                return Ok(crate::fluid::Fluid::EMPTY);
            }
            let world = map
                .to_world(value.fluid())
                .map_err(|source| WorldError::FluidId { pos, source })?;
            Ok(retag(value, crate::fluid::FluidId(world)))
        })
    }

    /// Loads a chunk's fluid from the default domain.
    ///
    /// `None` for a chunk with no row, which is the overwhelming majority of
    /// them and is why this is a separate table — see the schema.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable layer.
    pub fn load_chunk_fluid(&self, pos: ChunkPos) -> Result<Option<FluidLayer>, WorldError> {
        self.load_chunk_fluid_in(DEFAULT_DOMAIN, pos)
    }

    /// Loads a chunk's fluid from a named domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable layer.
    pub fn load_chunk_fluid_in(
        &self,
        domain: &str,
        pos: ChunkPos,
    ) -> Result<Option<FluidLayer>, WorldError> {
        let blob: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT data FROM chunk_fluid WHERE domain = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
                params![domain, pos.x, pos.y, pos.z],
                |row| row.get(0),
            )
            .optional()?;

        let Some(blob) = blob else {
            return Ok(None);
        };

        let stored = crate::fluid::codec::decode(&blob).map_err(|source| WorldError::Fluid {
            pos,
            domain: domain.to_owned(),
            source,
        })?;

        // World ids to this session's. Every name the table lists has a session
        // id after a reconcile — a placeholder if its mod is gone — so the only
        // way this fails is a row naming an id the world's own table does not,
        // which is a corrupt or hand-edited file.
        let map = self.fluid_map();
        Self::translate(&stored, |value| {
            if value.is_empty() {
                return Ok(crate::fluid::Fluid::EMPTY);
            }
            let session = map
                .to_session(value.fluid().0)
                .map_err(|source| WorldError::FluidId { pos, source })?;
            Ok(retag(value, session))
        })
        .map(Some)
    }

    /// Saves a chunk's fluid to the default domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure.
    pub fn save_chunk_fluid(&self, pos: ChunkPos, layer: &FluidLayer) -> Result<(), WorldError> {
        self.save_chunk_fluid_in(DEFAULT_DOMAIN, pos, layer)
    }

    /// Saves a chunk's fluid to a named domain, or removes it if it drained.
    ///
    /// **Charter rule 8 says numeric runtime ids are per-session and never
    /// stable across runs, and this path does not honour it yet.** A chunk's
    /// blocks are translated to WORLD material ids on the way to disk — see
    /// [`Self::encode`] and `idmap::MaterialMap` — but a [`crate::fluid::Fluid`]
    /// byte carries a four-bit fluid id that goes to disk exactly as the running
    /// session numbered it.
    ///
    /// [`crate::fluid::Fluids::register`] hands out ids positionally in
    /// registration order, so the stored id is stable for as long as the mod set
    /// and its load order are. Add a mod that registers a fluid ahead of an
    /// existing one and every saved pond in the world decodes as the wrong
    /// fluid — silently, because the byte is still perfectly valid.
    ///
    /// The fix is the one materials already have: a persistent name→id table in
    /// the world database and a translation on the way in and out. It needs the
    /// session's fluid registry to reach this layer, which it does not today —
    /// fluids are registered during mod load, after the database is opened.
    /// Until then, **a world's mod set must not gain or reorder fluids.**
    ///
    /// # An empty layer DELETEs rather than writing one byte
    ///
    /// `fluid::codec::encode` turns an empty layer into a single byte, so
    /// storing one would be nearly free — and would still be wrong. A pond that
    /// is drained and then unloaded must not come back when the chunk next
    /// loads, and a row that says "empty" is indistinguishable at the call site
    /// from a row that says "a pond, still here" until it has been decoded.
    /// Deleting keeps the table proportional to the milk in the world rather
    /// than to every chunk milk has ever touched.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure.
    pub fn save_chunk_fluid_in(
        &self,
        domain: &str,
        pos: ChunkPos,
        layer: &FluidLayer,
    ) -> Result<(), WorldError> {
        if layer.is_empty() {
            self.conn.execute(
                "DELETE FROM chunk_fluid WHERE domain = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
                params![domain, pos.x, pos.y, pos.z],
            )?;
            return Ok(());
        }

        let stored = self.to_world_ids(pos, layer)?;
        self.conn.execute(
            "INSERT INTO chunk_fluid (domain, x, y, z, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(domain, x, y, z) DO UPDATE SET data = excluded.data",
            params![
                domain,
                pos.x,
                pos.y,
                pos.z,
                crate::fluid::codec::encode(&stored)
            ],
        )?;
        Ok(())
    }

    /// Saves many fluid layers in a single transaction.
    ///
    /// The same reasoning as [`Self::save_chunks_batch`]: one WAL commit for the
    /// batch rather than one per chunk. Layers that drained are deleted in the
    /// same transaction, so a save either lands whole or not at all — a batch
    /// that wrote the new ponds but not the removals would resurrect milk.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure. The transaction rolls back.
    pub fn save_chunk_fluid_batch<'a>(
        &mut self,
        layers: impl IntoIterator<Item = (ChunkPos, &'a FluidLayer)>,
    ) -> Result<usize, WorldError> {
        self.save_chunk_fluid_batch_in(DEFAULT_DOMAIN, layers)
    }

    /// Saves many fluid layers to a named domain in one transaction.
    ///
    /// # Errors
    ///
    /// As [`Self::save_chunk_fluid_batch`].
    pub fn save_chunk_fluid_batch_in<'a>(
        &mut self,
        domain: &str,
        layers: impl IntoIterator<Item = (ChunkPos, &'a FluidLayer)>,
    ) -> Result<usize, WorldError> {
        let encoded = layers
            .into_iter()
            .map(|(pos, layer)| {
                let blob = if layer.is_empty() {
                    None
                } else {
                    Some(crate::fluid::codec::encode(&self.to_world_ids(pos, layer)?))
                };
                Ok((pos, blob))
            })
            .collect::<Result<Vec<_>, WorldError>>()?;

        let transaction = self.conn.transaction()?;
        {
            let mut write = transaction.prepare_cached(
                "INSERT INTO chunk_fluid (domain, x, y, z, data) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(domain, x, y, z) DO UPDATE SET data = excluded.data",
            )?;
            let mut remove = transaction.prepare_cached(
                "DELETE FROM chunk_fluid WHERE domain = ?1 AND x = ?2 AND y = ?3 AND z = ?4",
            )?;
            for (pos, blob) in &encoded {
                match blob {
                    Some(blob) => {
                        write.execute(params![domain, pos.x, pos.y, pos.z, blob])?;
                    }
                    None => {
                        remove.execute(params![domain, pos.x, pos.y, pos.z])?;
                    }
                }
            }
        }
        transaction.commit()?;
        Ok(encoded.len())
    }

    /// Encodes a chunk into the wire/disk blob format.
    ///
    /// The **same** bytes that go to disk are what a client is sent, so a
    /// blob a client can decode is a blob the world can store. Two encodings
    /// would be two formats to keep in step, and the one exercised less would
    /// be the one that broke.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk references a material the world has no id
    /// for.
    pub fn chunk_blob(&self, pos: ChunkPos, chunk: &Chunk) -> Result<Vec<u8>, WorldError> {
        self.encode(DEFAULT_DOMAIN, pos, chunk)
    }

    fn encode(&self, domain: &str, pos: ChunkPos, chunk: &Chunk) -> Result<Vec<u8>, WorldError> {
        codec::encode_chunk(chunk, &self.materials, self.dictionaries.first()).map_err(|source| {
            WorldError::Chunk {
                pos,
                domain: domain.to_owned(),
                source,
            }
        })
    }

    // -- entities ---------------------------------------------------------

    /// Loads the entities anchored to a chunk, in the order they were saved.
    ///
    /// # Order is part of the contract
    ///
    /// `ORDER BY id` is not decoration. Entities come back into
    /// [`crate::ent::Entities`] in the order this returns them, and that order
    /// becomes the iteration order every later tick sees — which charter rule 4
    /// requires be a property of the data rather than of the database's mood.
    /// `SQLite` is free to return rows in any order without an `ORDER BY`, and
    /// usually returns them in rowid order, which is exactly the kind of "works
    /// until it doesn't" this rule exists to forbid.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable entity.
    pub fn load_chunk_entities(
        &self,
        pos: ChunkPos,
    ) -> Result<Vec<crate::ent::Entity>, WorldError> {
        self.load_chunk_entities_in(DEFAULT_DOMAIN, pos)
    }

    /// Loads a chunk's entities from a named domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable entity.
    pub fn load_chunk_entities_in(
        &self,
        domain: &str,
        pos: ChunkPos,
    ) -> Result<Vec<crate::ent::Entity>, WorldError> {
        let mut statement = self.conn.prepare(
            "SELECT version, data FROM entities
             WHERE domain = ?1 AND chunk_x = ?2 AND chunk_y = ?3 AND chunk_z = ?4
             ORDER BY id",
        )?;
        let rows = statement.query_map(params![domain, pos.x, pos.y, pos.z], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut entities = Vec::new();
        for row in rows {
            let (version, blob) = row?;
            // **Migrated on load and rewritten at the next save**, exactly as a
            // chunk is: a world nobody visits pays nothing, and an entity in a
            // corner of the map still decodes years later because its step is
            // still in the chain.
            let entity = match u8::try_from(version) {
                Ok(ENTITY_FORMAT_VERSION) => {
                    postcard::from_bytes(&blob).map_err(|source| WorldError::Entity {
                        pos,
                        domain: domain.to_owned(),
                        reason: source.to_string(),
                    })?
                }
                Ok(older) => {
                    migrate::migrate_entity(older, &blob).map_err(|source| WorldError::Entity {
                        pos,
                        domain: domain.to_owned(),
                        reason: source.to_string(),
                    })?
                }
                Err(_) => {
                    return Err(WorldError::Entity {
                        pos,
                        domain: domain.to_owned(),
                        reason: format!(
                            "stored in format version {version}, this build writes \
                             {ENTITY_FORMAT_VERSION}"
                        ),
                    });
                }
            };
            entities.push(entity);
        }
        Ok(entities)
    }

    /// Replaces the entities anchored to a chunk.
    ///
    /// # Replace, not merge
    ///
    /// A chunk's entities are saved as a set, because that is how they are
    /// frozen: [`crate::ent::Entities::take_chunk`] removes all of them at once
    /// and this writes all of them at once. Merging would leave a mob behind
    /// every time one wandered into the next chunk — it would be written there
    /// and never removed here, and the world would slowly fill with copies.
    ///
    /// Passing an empty slice deletes the chunk's rows, so a chunk nothing
    /// lives in costs nothing, exactly as a dry chunk costs no fluid row.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable entity.
    pub fn save_chunk_entities(
        &self,
        pos: ChunkPos,
        entities: &[crate::ent::Entity],
    ) -> Result<(), WorldError> {
        self.save_chunk_entities_in(DEFAULT_DOMAIN, pos, entities)
    }

    /// Replaces a chunk's entities in a named domain.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable entity.
    pub fn save_chunk_entities_in(
        &self,
        domain: &str,
        pos: ChunkPos,
        entities: &[crate::ent::Entity],
    ) -> Result<(), WorldError> {
        // One transaction, because a half-written chunk is worse than an
        // unwritten one: the delete has already happened and the world has lost
        // entities it still believes it saved.
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM entities
             WHERE domain = ?1 AND chunk_x = ?2 AND chunk_y = ?3 AND chunk_z = ?4",
            params![domain, pos.x, pos.y, pos.z],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO entities (domain, chunk_x, chunk_y, chunk_z, version, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for entity in entities {
                let blob = postcard::to_allocvec(entity).map_err(|source| WorldError::Entity {
                    pos,
                    domain: domain.to_owned(),
                    reason: source.to_string(),
                })?;
                insert.execute(params![
                    domain,
                    pos.x,
                    pos.y,
                    pos.z,
                    i64::from(ENTITY_FORMAT_VERSION),
                    blob
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every chunk that has entities stored in it, in a stable order.
    ///
    /// For the shutdown flush and for tests. Ordered so two runs over one file
    /// see the same world.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure.
    pub fn chunks_with_entities(&self) -> Result<Vec<ChunkPos>, WorldError> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT chunk_x, chunk_y, chunk_z FROM entities
             WHERE domain = ?1 ORDER BY chunk_x, chunk_y, chunk_z",
        )?;
        let rows = statement.query_map(params![DEFAULT_DOMAIN], |row| {
            Ok(ChunkPos::new(row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    // -- mod storage ------------------------------------------------------

    /// Everything one mod has stored, in key order.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable value.
    pub fn load_mod_storage(&self, mod_id: &str) -> Result<crate::storage::Bag, WorldError> {
        let mut statement = self
            .conn
            .prepare("SELECT key, value FROM mod_storage WHERE mod_id = ?1 ORDER BY key")?;
        let rows = statement.query_map(params![mod_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut bag = crate::storage::Bag::new();
        for row in rows {
            let (key, blob) = row?;
            let value = postcard::from_bytes(&blob).map_err(|source| WorldError::ModStorage {
                mod_id: mod_id.to_owned(),
                key: key.clone(),
                reason: source.to_string(),
            })?;
            bag.insert(key, value);
        }
        Ok(bag)
    }

    /// Every mod that has anything stored, in order.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure.
    pub fn mods_with_storage(&self) -> Result<Vec<String>, WorldError> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT mod_id FROM mod_storage ORDER BY mod_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Replaces everything one mod has stored.
    ///
    /// Replace rather than merge, for the reason a chunk's entities are
    /// replaced: the caller holds the whole bag in memory and a merge would
    /// leave a deleted key behind for ever.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable value.
    pub fn save_mod_storage(
        &self,
        mod_id: &str,
        bag: &crate::storage::Bag,
    ) -> Result<(), WorldError> {
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute("DELETE FROM mod_storage WHERE mod_id = ?1", params![mod_id])?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO mod_storage (mod_id, key, value) VALUES (?1, ?2, ?3)")?;
            for (key, value) in bag {
                let blob =
                    postcard::to_allocvec(value).map_err(|source| WorldError::ModStorage {
                        mod_id: mod_id.to_owned(),
                        key: key.clone(),
                        reason: source.to_string(),
                    })?;
                insert.execute(params![mod_id, key, blob])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    // -- players ----------------------------------------------------------

    /// Loads a player's opaque state blob.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn load_player(&self, uuid: &str) -> Result<Option<Vec<u8>>, WorldError> {
        Ok(self
            .conn
            .query_row(
                "SELECT data FROM players WHERE uuid = ?1",
                params![uuid],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Saves a player's opaque state blob.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn save_player(&self, uuid: &str, version: u8, data: &[u8]) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO players (uuid, version, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(uuid) DO UPDATE SET version = excluded.version, data = excluded.data",
            params![uuid, i64::from(version), data],
        )?;
        Ok(())
    }

    /// Every container the world holds, by name, with its version byte first.
    ///
    /// Loaded whole at startup: a container is a handful of stacks and a world
    /// has as many as somebody has placed chests. Fetching one at a time as
    /// blocks are opened would put a database read inside the tick, on a path
    /// a player is standing there waiting on.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn load_containers(&self) -> Result<Vec<(String, Vec<u8>)>, WorldError> {
        let mut statement = self.conn.prepare("SELECT name, data FROM containers")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Writes one container's contents.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn save_container(&self, name: &str, version: u8, data: &[u8]) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO containers (name, version, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET version = excluded.version, data = excluded.data",
            params![name, i64::from(version), data],
        )?;
        Ok(())
    }

    /// Forgets a container, for one whose block has been broken.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn delete_container(&self, name: &str) -> Result<(), WorldError> {
        self.conn
            .execute("DELETE FROM containers WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// Adds an authorised key to an identity's key set (charter rule 13).
    ///
    /// `added_by` is `None` only for the root key.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn add_player_key(&self, key: &PlayerKey<'_>) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO player_keys (uuid, pubkey, next_key_hash, added_at, added_by_pubkey)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(uuid, pubkey) DO UPDATE SET next_key_hash = excluded.next_key_hash",
            params![
                key.uuid,
                key.pubkey,
                key.next_key_hash,
                key.added_at,
                key.added_by
            ],
        )?;
        Ok(())
    }

    /// Every non-revoked key authorised for an identity.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn player_keys(&self, uuid: &str) -> Result<Vec<StoredPlayerKey>, WorldError> {
        let mut statement = self.conn.prepare(
            "SELECT pubkey, next_key_hash, added_at, added_by_pubkey, revoked_at
             FROM player_keys WHERE uuid = ?1 AND revoked_at IS NULL
             ORDER BY added_at, pubkey",
        )?;
        let rows = statement.query_map(params![uuid], |row| {
            Ok(StoredPlayerKey {
                pubkey: row.get(0)?,
                next_key_hash: row.get(1)?,
                added_at: row.get(2)?,
                added_by: row.get(3)?,
                revoked_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Marks a key revoked. Revocation is a tombstone, never a delete — the
    /// history of an identity's key set has to stay replayable.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn revoke_player_key(&self, uuid: &str, pubkey: &[u8], at: i64) -> Result<(), WorldError> {
        self.conn.execute(
            "UPDATE player_keys SET revoked_at = ?3 WHERE uuid = ?1 AND pubkey = ?2",
            params![uuid, pubkey, at],
        )?;
        Ok(())
    }

    /// How many of an identity's keys have been revoked.
    ///
    /// Revoked keys are tombstoned rather than deleted, so the history of a key
    /// set stays replayable — a rotation chain that is missing its middle
    /// cannot be verified.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn revoked_key_count(&self, uuid: &str) -> Result<usize, WorldError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM player_keys WHERE uuid = ?1 AND revoked_at IS NOT NULL",
            params![uuid],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Runs `SQLite`'s structural integrity check.
    ///
    /// Returns `"ok"` for a sound database. Anything else is a description of
    /// the damage found, and is what a `--verify` command or a post-crash check
    /// should surface to an operator rather than silently continuing.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn integrity_check(&self) -> Result<String, WorldError> {
        Ok(self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?)
    }

    /// Binds a display name to a UUID, failing if another identity holds it.
    ///
    /// # Errors
    ///
    /// Any SQL failure, including the primary-key conflict that enforces one
    /// holder per name.
    pub fn claim_name(&self, server_name: &str, uuid: &str) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO player_names (server_name, uuid) VALUES (?1, ?2)",
            params![server_name, uuid],
        )?;
        Ok(())
    }

    /// The UUID holding a display name.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn name_holder(&self, server_name: &str) -> Result<Option<String>, WorldError> {
        Ok(self
            .conn
            .query_row(
                "SELECT uuid FROM player_names WHERE server_name = ?1",
                params![server_name],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Releases a display name so another identity may claim it.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn release_name(&self, server_name: &str) -> Result<(), WorldError> {
        self.conn.execute(
            "DELETE FROM player_names WHERE server_name = ?1",
            params![server_name],
        )?;
        Ok(())
    }

    /// Binds a name, taking it from whoever holds it.
    ///
    /// Unlike [`claim_name`](Self::claim_name) this does not fail on a
    /// conflict. First-come is enforced in the registry, which knows whether
    /// the claimant is the existing holder reconnecting; by the time a binding
    /// reaches the database that decision has already been made, and failing
    /// here would only turn an accepted join into a save error.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn set_name(&self, server_name: &str, uuid: &str) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO player_names (server_name, uuid) VALUES (?1, ?2)
             ON CONFLICT(server_name) DO UPDATE SET uuid = excluded.uuid",
            params![server_name, uuid],
        )?;
        Ok(())
    }

    /// Every display-name binding, in a stable order.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn all_name_bindings(&self) -> Result<Vec<(String, String)>, WorldError> {
        let mut statement = self
            .conn
            .prepare("SELECT server_name, uuid FROM player_names ORDER BY server_name")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every identity with at least one key on record, in a stable order.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn all_player_uuids(&self) -> Result<Vec<String>, WorldError> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT uuid FROM player_keys ORDER BY uuid")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every key on record for an identity, **including revoked ones**.
    ///
    /// [`player_keys`](Self::player_keys) filters revocations out, which is
    /// right for "may this key be used" but wrong for reloading a key set: the
    /// tombstones are what make the rotation chain replayable, and a set
    /// rebuilt without them has quietly lost its history.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn all_player_keys(&self, uuid: &str) -> Result<Vec<StoredPlayerKey>, WorldError> {
        let mut statement = self.conn.prepare(
            "SELECT pubkey, next_key_hash, added_at, added_by_pubkey, revoked_at
             FROM player_keys WHERE uuid = ?1
             ORDER BY added_at, pubkey",
        )?;
        let rows = statement.query_map(params![uuid], |row| {
            Ok(StoredPlayerKey {
                pubkey: row.get(0)?,
                next_key_hash: row.get(1)?,
                added_at: row.get(2)?,
                added_by: row.get(3)?,
                revoked_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // -- meta -------------------------------------------------------------

    /// Reads a meta value.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>, WorldError> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Writes a meta value.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), WorldError> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Reads the world seed, if set.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn world_seed(&self) -> Result<Option<u64>, WorldError> {
        Ok(self
            .meta(meta_keys::WORLD_SEED)?
            .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
            .map(u64::from_le_bytes))
    }

    /// Sets the world seed.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn set_world_seed(&self, seed: u64) -> Result<(), WorldError> {
        self.set_meta(meta_keys::WORLD_SEED, &seed.to_le_bytes())
    }

    /// Reads the domain instances this world has, as `(instance, template)`.
    ///
    /// A line per instance, tab-separated. A text format rather than a
    /// serialised structure because this is a list of two strings that a person
    /// debugging a world file should be able to read, and because a format with
    /// no versioning to get wrong cannot be got wrong.
    ///
    /// Anything malformed is skipped rather than failing the load. A world that
    /// will not open because one line of a side table is unreadable is worse
    /// than a world that opens with one ship missing — and the ship's chunks
    /// are still there either way, as an unknown domain.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn domain_instances(&self) -> Result<Vec<(String, String)>, WorldError> {
        let Some(bytes) = self.meta(meta_keys::DOMAIN_INSTANCES)? else {
            return Ok(Vec::new());
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(Vec::new());
        };
        Ok(text
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(instance, template)| !instance.is_empty() && !template.is_empty())
            .map(|(instance, template)| (instance.to_owned(), template.to_owned()))
            .collect())
    }

    /// Writes the domain instances this world has.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn set_domain_instances(&self, instances: &[(String, String)]) -> Result<(), WorldError> {
        let text = instances
            .iter()
            .map(|(instance, template)| format!("{instance}\t{template}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.set_meta(meta_keys::DOMAIN_INSTANCES, text.as_bytes())
    }

    /// Every domain this world has anything stored under.
    ///
    /// Read from the tables themselves rather than from a list, because the
    /// list is what can be wrong: a domain with chunks in it exists whatever
    /// any registry says, and this is how one whose mod was removed is found
    /// and preserved rather than quietly orphaned (charter rule 8).
    ///
    /// Sorted, so nothing downstream of this depends on SQLite's row order.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn stored_domains(&self) -> Result<Vec<String>, WorldError> {
        let mut found = std::collections::BTreeSet::new();
        for table in ["chunks", "chunk_fluid", "entities"] {
            let mut statement = self
                .conn
                .prepare(&format!("SELECT DISTINCT domain FROM {table}"))?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for domain in rows {
                found.insert(domain?);
            }
        }
        Ok(found.into_iter().collect())
    }

    fn read_meta_i64(conn: &Connection, key: &str) -> rusqlite::Result<Option<i64>> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map(|value| {
            value
                .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
                .map(i64::from_le_bytes)
        })
    }

    fn write_meta_i64(conn: &Connection, key: &str, value: i64) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.to_le_bytes().as_slice()],
        )?;
        Ok(())
    }

    fn write_meta_str(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value.as_bytes()],
        )?;
        Ok(())
    }

    // -- lifecycle --------------------------------------------------------

    /// Flushes pending work and folds the WAL back into the database.
    ///
    /// Passive checkpoint only — see [`schema::apply_pragmas`] for why this
    /// must never be the blocking kind.
    ///
    /// # Errors
    ///
    /// Any SQL failure.
    pub fn flush(&self) -> Result<(), WorldError> {
        schema::checkpoint_passive(&self.conn)?;
        Ok(())
    }

    /// Flushes and closes.
    ///
    /// Dropping a `WorldDb` also closes it safely — WAL means an unclean exit
    /// loses at most the uncommitted transaction. This exists so a clean
    /// shutdown can report a failure to flush instead of discarding it.
    ///
    /// # Errors
    ///
    /// Any SQL failure during the final flush.
    pub fn close(self) -> Result<(), WorldError> {
        self.flush()?;
        drop(self);
        Ok(())
    }
}

/// A key being added to an identity's key set.
#[derive(Debug, Clone, Copy)]
pub struct PlayerKey<'a> {
    /// Canonical player UUID, `BLAKE3` of the root public key.
    pub uuid: &'a str,
    /// The Ed25519 public key being authorised.
    pub pubkey: &'a [u8],
    /// Hash of this key's designated successor — the pre-rotation commitment.
    pub next_key_hash: Option<&'a [u8]>,
    /// Unix timestamp.
    pub added_at: i64,
    /// The existing key that authorised this one. `None` only for the root.
    pub added_by: Option<&'a [u8]>,
}

/// A key read back from an identity's key set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPlayerKey {
    /// The authorised public key.
    pub pubkey: Vec<u8>,
    /// Pre-rotation commitment.
    pub next_key_hash: Option<Vec<u8>>,
    /// Unix timestamp.
    pub added_at: i64,
    /// The key that authorised this one. `None` for the root.
    pub added_by: Option<Vec<u8>>,
    /// When revoked, if it has been.
    pub revoked_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_world_written_before_fluid_existed_still_opens() {
        // **Adding `chunk_fluid` did not bump `SCHEMA_VERSION`, and this is the
        // test that says why that is safe.** A bump is a refusal rather than a
        // migration — [`WorldDb::open`] rejects any world whose stored version
        // is not the current one — so bumping would have made every existing
        // save unopenable. `CREATE TABLE IF NOT EXISTS` runs on every open
        // instead, and a world from before the feature gets the table empty,
        // which is the true answer that it has no fluid saved.
        //
        // A real file rather than `open_in_memory`, because the whole point is
        // to close a world and open it again with different code.
        let dir = std::env::temp_dir().join("tiamot-pre-fluid-world");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(format!("{}.tiamot", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let pos = ChunkPos::new(2, 0, -1);
        {
            let mut registry = Registry::default();
            let db = WorldDb::open(&path, &mut registry).expect("create");
            db.save_chunk(pos, &Chunk::air(pos)).expect("save");
            // Leaves the file shaped exactly like one an older build wrote.
            db.conn
                .execute("DROP TABLE chunk_fluid", [])
                .expect("drop the table");
        }

        let mut registry = Registry::default();
        let db = WorldDb::open(&path, &mut registry).expect("a pre-fluid world must still open");
        assert!(
            db.load_chunk(pos).expect("load").is_some(),
            "the terrain did not survive the reopen"
        );
        assert!(
            db.load_chunk_fluid(pos).expect("load").is_none(),
            "a world with no fluid rows reported fluid"
        );

        let _ = std::fs::remove_file(&path);
    }
}
