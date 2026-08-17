// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The world database schema and the connection pragmas that make it safe.

use rusqlite::Connection;

/// Schema version, stored in `meta`. Bumped when the *table layout* changes,
/// which is separate from the per-blob format versions in
/// [`crate::persist::codec`].
///
/// # Why adding `chunk_fluid` did not bump this
///
/// Because a bump is not a migration, it is a **refusal**: [`WorldDb::open`]
/// rejects any world whose stored version is not this one, so bumping without a
/// table-migration path would make every existing save unopenable. A purely
/// additive table needs no migration — [`create`] runs on every open and
/// `CREATE TABLE IF NOT EXISTS` gives an older world the new table, empty,
/// which is the true answer that it has no fluid saved. Nothing existing reads
/// or writes it, so an older build opening a newer world simply ignores it.
///
/// Bump this only for a change that would make old rows misread: a column
/// added to an existing table, a type changed, a key redefined.
///
/// [`WorldDb::open`]: crate::persist::WorldDb::open
pub const SCHEMA_VERSION: i64 = 1;

/// The domain every chunk and entity is written under until Task 15a.
///
/// Charter-adjacent, and reserved deliberately early — see [`SCHEMA`].
pub const DEFAULT_DOMAIN: &str = "overworld";

/// The world schema.
///
/// # Why `domain` exists before anything uses it
///
/// Task 15a generalises the world into multiple simulation domains. Adding the
/// column then would mean migrating a live chunk table and rewriting every
/// query that touches it — on a player's save file, in the field. Reserving it
/// now costs one indexed `TEXT NOT NULL DEFAULT 'overworld'` and nothing else,
/// and turns 15a from a schema retrofit into a feature change.
///
/// **No domain logic exists yet, and none should be added here.** The column is
/// storage readiness, not a feature.
pub const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB
);

CREATE TABLE IF NOT EXISTS id_map (
    string_id  TEXT PRIMARY KEY,
    numeric_id INTEGER UNIQUE NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    domain  TEXT NOT NULL DEFAULT 'overworld',
    x       INT NOT NULL,
    y       INT NOT NULL,
    z       INT NOT NULL,
    version INT NOT NULL,
    data    BLOB NOT NULL,
    PRIMARY KEY (domain, x, y, z)
);

-- A pond is not derived state. Light is thrown away on shutdown and recomputed,
-- because recomputing gives the same answer; there is no function from terrain
-- back to 'somebody poured milk here', so fluid has to be written down.
--
-- Its own table rather than a column on `chunks`, and rather than a field in
-- the chunk blob, because `FluidLayer`'s whole design is that a dry chunk costs
-- nothing. A column would be one NULL per dry chunk and a blob field would put
-- a fluid byte in every chunk in the world; a separate table gives a row only
-- to the chunks that actually hold something. Rows are DELETEd when a layer
-- drains, so the table stays proportional to the milk rather than to history.
CREATE TABLE IF NOT EXISTS chunk_fluid (
    domain  TEXT NOT NULL DEFAULT 'overworld',
    x       INT NOT NULL,
    y       INT NOT NULL,
    z       INT NOT NULL,
    data    BLOB NOT NULL,
    PRIMARY KEY (domain, x, y, z)
);

-- Stable world ids for fluids. Charter rule 8: a fluid byte carries a numeric
-- id, `Fluids::register` numbers positionally in registration order, and
-- without this table that per-session number went straight to disk — so adding
-- a mod that registered a fluid ahead of an existing one silently turned every
-- stored pond into a different fluid.
--
-- Its own table rather than a row in `id_map` because the two id spaces are not
-- the same shape: a material id is a u16 and a fluid id is FOUR BITS, so id 500
-- would not fit in the byte. Fifteen ids, and a world's history shares them
-- with whatever is registered now — see `persist::fluidmap`.
CREATE TABLE IF NOT EXISTS fluid_ids (
    string_id  TEXT PRIMARY KEY,
    numeric_id INTEGER UNIQUE NOT NULL
);

CREATE TABLE IF NOT EXISTS players (
    uuid    TEXT PRIMARY KEY,
    version INT NOT NULL,
    data    BLOB NOT NULL
);

-- Charter rule 13: an identity is a SET of authorised keys, not one key.
-- The root key's row has added_by_pubkey IS NULL; every other key was added by
-- a signature from an existing one. next_key_hash is the pre-rotation
-- commitment, so a stolen current key cannot rotate the identity away from its
-- owner.
CREATE TABLE IF NOT EXISTS player_keys (
    uuid            TEXT NOT NULL,
    pubkey          BLOB NOT NULL,
    next_key_hash   BLOB,
    added_at        INT NOT NULL,
    added_by_pubkey BLOB,
    revoked_at      INT,
    PRIMARY KEY (uuid, pubkey)
);

-- Display names are a per-server claim bound to a UUID. The PRIMARY KEY on
-- server_name is what enforces one holder per name; identity itself is always
-- the UUID (charter rule 13).
CREATE TABLE IF NOT EXISTS player_names (
    server_name TEXT PRIMARY KEY,
    uuid        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entities (
    id      INTEGER PRIMARY KEY,
    domain  TEXT NOT NULL DEFAULT 'overworld',
    chunk_x INT NOT NULL,
    chunk_y INT NOT NULL,
    chunk_z INT NOT NULL,
    version INT NOT NULL,
    data    BLOB NOT NULL
);

-- A mod's own facts about the world: which world this is, whether something has
-- already happened, who a thing belongs to. Without it a mod has to smuggle
-- such a fact into a block somewhere, where a player can dig it up.
--
-- Keyed by mod id AND key, so one mod cannot read another's — the isolation is
-- a property of the primary key and of the API that never takes a mod id as a
-- parameter, rather than of everyone's good behaviour.
--
-- Additive, like `chunk_fluid`: `CREATE TABLE IF NOT EXISTS` gives an older
-- world the new table empty on the next open, so this does not bump
-- SCHEMA_VERSION. A bump is a refusal, not a migration.
CREATE TABLE IF NOT EXISTS mod_storage (
    mod_id TEXT NOT NULL,
    key    TEXT NOT NULL,
    value  BLOB NOT NULL,
    PRIMARY KEY (mod_id, key)
);

CREATE INDEX IF NOT EXISTS entities_by_chunk
    ON entities (domain, chunk_x, chunk_y, chunk_z);

CREATE INDEX IF NOT EXISTS player_keys_by_uuid
    ON player_keys (uuid);
";

/// Applies the connection pragmas a world database depends on.
///
/// Each one is load-bearing:
///
/// - **WAL** — readers do not block the writer. A save must never stall the
///   tick, and the tick is the only writer.
/// - **`synchronous = NORMAL`** — under WAL this is crash-safe against process
///   death, which is the failure that actually happens. `FULL` additionally
///   survives OS/power loss at the cost of an fsync per commit, which a server
///   saving chunks every few seconds cannot afford. The trade is deliberate.
/// - **`foreign_keys = ON`** — `SQLite` defaults this OFF per connection.
/// - **`busy_timeout`** — a save colliding with a checkpoint should wait, not
///   fail. Five seconds is far longer than any legitimate contention here, so
///   hitting it means something is genuinely wrong and an error is right.
///
/// # WAL checkpoint policy
///
/// A **passive** checkpoint runs on every explicit flush. Passive never blocks
/// readers and gives up rather than waiting, which is correct for a periodic
/// flush — if it cannot checkpoint now it will succeed on the next one.
///
/// Task 16's `save-freeze` depends on this: it needs a moment where the WAL is
/// known to be folded into the main database so the file can be copied. It gets
/// that by halting writers and then requesting a **truncate** checkpoint, which
/// does wait. Do not change the flush path to truncate — that would put an
/// unbounded wait in the tick loop.
///
/// # Errors
///
/// Any pragma failure.
pub fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    // journal_mode returns a row, so it needs query_row rather than execute.
    conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Creates the schema if absent.
///
/// # Errors
///
/// Any SQL failure.
pub fn create(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

/// Runs a passive WAL checkpoint. See [`apply_pragmas`] for the policy.
///
/// # Errors
///
/// Any SQL failure. A checkpoint that simply could not proceed is not an
/// error — passive mode reports that by doing nothing.
pub fn checkpoint_passive(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))
}
