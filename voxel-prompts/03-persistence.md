# TASK 03 — Persistence: SQLite world format, versioning, ID mapping

Depends on: 02. All code in `crates/core` (module `persist`).

## Objective
Crash-safe world storage with lazy migrations and mod-churn-proof ID mapping.

## Design (implement exactly)
- One SQLite database per world (`rusqlite`, bundled feature; WAL mode,
  `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`). Document the WAL
  checkpoint policy (passive checkpoint on flush; `save-freeze` in Task 16 relies on it).
- Tables:
  - `meta(key TEXT PRIMARY KEY, value BLOB)` — world_seed, format_version, engine_version,
    created_at, world name.
  - `id_map(string_id TEXT PRIMARY KEY, numeric_id INTEGER UNIQUE NOT NULL)`.
  - `chunks(domain TEXT NOT NULL DEFAULT 'overworld', x INT, y INT, z INT,
    version INT NOT NULL, data BLOB NOT NULL, PRIMARY KEY(domain, x, y, z))`.
  - `players(uuid TEXT PRIMARY KEY, version INT, data BLOB)`.
  - `player_keys(uuid TEXT NOT NULL, pubkey BLOB NOT NULL, next_key_hash BLOB,
    added_at INT NOT NULL, added_by_pubkey BLOB, revoked_at INT,
    PRIMARY KEY(uuid, pubkey))` — the key SET for an identity (charter rule 13). The root
    key's row has `added_by_pubkey IS NULL`. `next_key_hash` is the pre-rotation commitment.
  - `player_names(server_name TEXT PRIMARY KEY, uuid TEXT NOT NULL)` — display-name binding,
    name → UUID, enforcing one holder per name.
  - `entities(id INTEGER PRIMARY KEY, domain TEXT NOT NULL DEFAULT 'overworld',
    chunk_x INT, chunk_y INT, chunk_z INT, version INT, data BLOB)` with an index on
    (domain, chunk coords).

  **Why `domain` exists now, before anything uses it:** Task 15a generalises the world into
  multiple simulation domains. Adding that column later would mean migrating a live chunk
  table and every query that touches it, for no benefit — the migration chain is already
  proven below by a synthetic test. Reserve the column, default every write to
  `'overworld'`, and let 15a be a feature change rather than a schema retrofit. Do not build
  any domain *logic* here.

- Chunk encoding: `postcard`-serialized palette+indices+mixed-table, zstd-compressed.
  Support an optional shared zstd dictionary stored in `meta` (train later; code path now,
  dictionary id in the chunk header byte).
- Every blob starts with a 1-byte format version. `FORMAT_VERSION` const per record type.
  Migration framework: `fn migrate(version, bytes) -> Result<CurrentForm>` chains v→v+1
  steps; chunks migrate lazily on load and are rewritten on next save. Include a synthetic
  `v0→v1` migration in tests to prove the chain works — this is the migration gate; no later
  task needs to invent a schema change to exercise it.
- **ID mapping protocol** (the critical part):
  - `IdTable::load(db)` reads id_map. `reconcile(registered: &[String])`:
    existing names keep their numeric ids; new names get the next free id; names in the DB
    but not registered are NOT removed — they stay mapped and the engine aliases them to the
    `engine:unknown` placeholder material for this session.
  - Runtime palettes store numeric ids; encode/decode translates through the table so blobs
    on disk always reference stable numeric ids owned by this world.
  - Unknown-material blocks must round-trip: load world with mod absent, touch nothing,
    save, re-add mod, content restored byte-identical.
- `WorldDb` API: open/create, load_chunk, save_chunk (upsert, single transaction per batch),
  save_chunks_batch, load/save player, player-key CRUD, meta get/set, flush/close. Every
  chunk/entity accessor takes a domain argument (defaulted to overworld by a helper) so
  15a is additive at the call sites too.
- Wire into `server`: on startup open/create world from config; on SIGTERM flush and close.

## Tests
- [A] Round-trip: random chunks (reuse Task 02 proptest generators) → save → load → equal.
- [A] Crash safety: kill a write mid-transaction (panic inside a transaction in a subprocess,
  or copy db+wal mid-write) → reopen → db valid, prior data intact.
- [A] Migration chain v0→current on synthetic old blobs.
- [A] The unknown-material round-trip scenario above, end to end.
- [A] Domain column: writes default to `'overworld'`; a row written under a second domain
  string is retrievable independently and does not collide on the primary key. (Schema
  readiness only — no domain semantics yet.)
- [A] Bench: encode+compress and decode+decompress per chunk; batch save of 1000 chunks.

## Acceptance criteria
- [A] All tests pass, including crash-safety and unknown-material round-trip.
- [A] Uniform-white chunk stored size ≤ 100 bytes (measure, assert loosely, document).
      Record the chiselled-chunk size measured in Task 02b alongside it for comparison.
- [A] Server creates/opens a world file and shuts down cleanly with a final flush.
