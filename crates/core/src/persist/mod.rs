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
pub mod idmap;
pub mod migrate;
pub mod schema;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::chunk::Chunk;
use crate::coords::ChunkPos;
use crate::material::Registry;
use crate::persist::codec::{CodecError, Dictionary};
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
}

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

    /// Material id mapping failed.
    #[error("material id mapping failed")]
    Materials(#[from] IdMapError),

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

/// An open world database.
///
/// Holds the connection, the reconciled material mapping for this session, and
/// any `zstd` dictionaries the world carries.
pub struct WorldDb {
    conn: Connection,
    path: PathBuf,
    ids: IdTable,
    materials: MaterialMap,
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

        Ok(Self {
            conn,
            path,
            ids,
            materials,
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
