// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Loading and flushing the identity registry.
//!
//! The bridge between [`IdentityRegistry`], which is pure and in-memory, and
//! the `player_keys` / `player_names` tables. Kept in its own module so the
//! registry itself never mentions `SQLite` and the join flow stays testable
//! without a filesystem.
//!
//! # Why the flush is incremental
//!
//! A long-lived server's registry holds every player who has ever joined, and a
//! typical tick changes none of them. The registry tracks which identities and
//! names moved; this module writes only those. The alternative — rewriting both
//! tables on every save — turns a routine autosave into work proportional to
//! the server's entire history.
//!
//! # Why dirty flags clear only after the write
//!
//! [`flush`] clears the registry's dirty set as its last act, after the
//! transaction has committed. Clearing first would mean a failed write leaves
//! the registry believing the database already has the change, and the binding
//! would be lost at the next restart with nothing logged.

use ed25519_dalek::VerifyingKey;

use super::registry::IdentityRegistry;
use crate::identity::keyset::COMMITMENT_BYTES;
use crate::identity::{AuthorisedKey, PlayerUuid, public_key_from_bytes};
use crate::persist::{PlayerKey, StoredPlayerKey, WorldDb, WorldError};

/// A stored identity could not be turned back into a key set.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database itself failed.
    #[error(transparent)]
    World(#[from] WorldError),

    /// A row's UUID text is not a valid player UUID.
    #[error("stored identity `{uuid}` has an unreadable UUID: {reason}")]
    BadUuid {
        /// The offending text.
        uuid: String,
        /// Why it could not be read.
        reason: String,
    },

    /// A row's public key is not a valid Ed25519 key.
    #[error(
        "stored identity `{uuid}` has an unusable public key: {reason}. The row is kept; the \
         identity is skipped so the rest of the server can start."
    )]
    BadKey {
        /// Which identity the row belongs to.
        uuid: String,
        /// Why the key was rejected.
        reason: String,
    },

    /// The rows do not describe a valid key set.
    #[error("stored identity `{uuid}` does not form a valid key set: {reason}")]
    BadKeySet {
        /// Which identity.
        uuid: String,
        /// Why the set was refused.
        reason: String,
    },
}

/// What a [`load`] found, including what it had to skip.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Identities successfully loaded.
    pub identities: usize,
    /// Name bindings loaded.
    pub names: usize,
    /// Identities that could not be read, and why.
    ///
    /// **Skipped, not fatal.** One corrupt row must not stop a server booting —
    /// the other players are still there, and refusing to start turns a problem
    /// affecting one account into an outage affecting everyone. The rows are
    /// left in place so the damage can be inspected rather than overwritten.
    pub skipped: Vec<StoreError>,
}

/// Rebuilds a registry from the world database.
///
/// # Errors
///
/// [`StoreError::World`] if the database cannot be read at all. Individual
/// unreadable identities are collected into [`LoadReport::skipped`] rather than
/// failing the load.
pub fn load(db: &WorldDb) -> Result<(IdentityRegistry, LoadReport), StoreError> {
    let mut registry = IdentityRegistry::default();
    let mut report = LoadReport::default();

    for uuid_text in db.all_player_uuids()? {
        // Revoked keys included: the tombstones are what keep a rotation chain
        // replayable, and a set rebuilt without them has lost its history.
        let rows = db.all_player_keys(&uuid_text)?;
        match to_authorised_keys(&uuid_text, &rows) {
            Ok(keys) => match registry.insert_stored(keys) {
                Ok(()) => report.identities += 1,
                Err(err) => report.skipped.push(StoreError::BadKeySet {
                    uuid: uuid_text.clone(),
                    reason: err.to_string(),
                }),
            },
            Err(err) => report.skipped.push(err),
        }
    }

    for (name, uuid_text) in db.all_name_bindings()? {
        match PlayerUuid::from_hex(&uuid_text) {
            Ok(uuid) => {
                // A binding whose identity did not load would be a name held by
                // nobody: unusable by its owner and unclaimable by anyone else.
                // Drop it from memory and leave the row alone; if the identity
                // is later repaired the binding comes back with it.
                if registry.contains(&uuid) {
                    registry.bind_name_clean(&name, uuid);
                    report.names += 1;
                } else {
                    report.skipped.push(StoreError::BadKeySet {
                        uuid: uuid_text,
                        reason: format!("holds the name `{name}` but has no loadable key set"),
                    });
                }
            }
            Err(err) => report.skipped.push(StoreError::BadUuid {
                uuid: uuid_text,
                reason: err.to_string(),
            }),
        }
    }

    Ok((registry, report))
}

/// Writes everything the registry has changed since the last flush.
///
/// A no-op when nothing is dirty.
///
/// # Errors
///
/// [`StoreError::World`] if the write fails. The registry's dirty set is left
/// intact in that case, so the next flush retries rather than losing the
/// change.
pub fn flush(db: &WorldDb, registry: &mut IdentityRegistry) -> Result<(), StoreError> {
    if !registry.is_dirty() {
        return Ok(());
    }

    for uuid in registry.dirty_identities() {
        let uuid_text = uuid.to_string();
        let Some(keys) = registry.key_set(&uuid) else {
            continue;
        };
        for entry in keys.all_keys() {
            let pubkey = entry.key.to_bytes();
            let added_by = entry.added_by.map(|key| key.to_bytes());
            db.add_player_key(&PlayerKey {
                uuid: &uuid_text,
                pubkey: &pubkey,
                next_key_hash: entry.next_key_hash.as_ref().map(<[u8; 32]>::as_slice),
                added_at: entry.added_at,
                added_by: added_by.as_ref().map(<[u8; 32]>::as_slice),
            })?;
            if let Some(at) = entry.revoked_at {
                db.revoke_player_key(&uuid_text, &pubkey, at)?;
            }
        }
    }

    for name in registry.dirty_names() {
        match registry.name_holder(&name) {
            Some(uuid) => db.set_name(&name, &uuid.to_string())?,
            // Absent from the registry means released, so the row goes too.
            None => db.release_name(&name)?,
        }
    }

    // Last, and only on success — see the module docs.
    registry.clear_dirty();
    Ok(())
}

fn to_authorised_keys(
    uuid: &str,
    rows: &[StoredPlayerKey],
) -> Result<Vec<AuthorisedKey>, StoreError> {
    rows.iter()
        .map(|row| {
            Ok(AuthorisedKey {
                key: to_key(uuid, &row.pubkey)?,
                next_key_hash: to_commitment(uuid, row.next_key_hash.as_deref())?,
                added_at: row.added_at,
                added_by: row
                    .added_by
                    .as_deref()
                    .map(|bytes| to_key(uuid, bytes))
                    .transpose()?,
                revoked_at: row.revoked_at,
            })
        })
        .collect()
}

fn to_key(uuid: &str, bytes: &[u8]) -> Result<VerifyingKey, StoreError> {
    let sized: [u8; 32] = bytes.try_into().map_err(|_| StoreError::BadKey {
        uuid: uuid.to_owned(),
        reason: format!("expected 32 bytes, found {}", bytes.len()),
    })?;
    // Goes through the checked constructor, which rejects small-order points.
    // A raw `from_bytes` would accept 32 zero bytes as a public key, and that
    // key verifies signatures nobody holds a secret for.
    public_key_from_bytes(&sized).map_err(|err| StoreError::BadKey {
        uuid: uuid.to_owned(),
        reason: err.to_string(),
    })
}

fn to_commitment(
    uuid: &str,
    bytes: Option<&[u8]>,
) -> Result<Option<[u8; COMMITMENT_BYTES]>, StoreError> {
    bytes
        .map(|bytes| {
            bytes.try_into().map_err(|_| StoreError::BadKey {
                uuid: uuid.to_owned(),
                reason: format!(
                    "pre-rotation commitment is {} bytes, expected {COMMITMENT_BYTES}",
                    bytes.len()
                ),
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, KeySet, keyset::add_key_payload};
    use crate::material::Registry;

    /// A fresh world file, matching the convention in `tests/persistence.rs`.
    ///
    /// WAL leaves `-wal` and `-shm` sidecars; a stale one from a previous run
    /// would let a test read data it never wrote.
    fn world_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("tiamot-store-tests");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(format!("{name}.sqlite"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    fn open(path: &std::path::Path) -> WorldDb {
        let mut registry = Registry::new();
        WorldDb::open(path, &mut registry).expect("open")
    }

    fn alice_registry(alice: &Identity) -> IdentityRegistry {
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), None, 0));
        registry
    }

    #[test]
    fn an_empty_world_loads_an_empty_registry() {
        let path = world_path("empty");
        let db = open(&path);
        let (registry, report) = load(&db).expect("load");
        assert!(registry.is_empty());
        assert_eq!(report.identities, 0);
        assert!(report.skipped.is_empty());
        db.close().expect("close");
    }

    #[test]
    fn a_name_binding_survives_a_restart() {
        // The acceptance criterion: reconnect after a restart and your name is
        // still yours.
        let path = world_path("name-survives");
        let alice = Identity::generate().expect("generate");
        let uuid = alice.uuid_as_root();

        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry.bind_name("Alice", uuid).expect("bind");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (restored, report) = load(&db).expect("load");

        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.identities, 1);
        assert_eq!(restored.name_holder("Alice"), Some(uuid));
        assert_eq!(
            restored.identity_of_key(&alice.public_key()),
            Some(uuid),
            "the key must still resolve after a reload"
        );
        db.close().expect("close");
    }

    #[test]
    fn a_second_device_key_survives_a_restart() {
        // Charter rule 13: a key set is the identity, not one key. If only the
        // root key were persisted, adding a laptop would work until the server
        // restarted and then silently stop.
        let path = world_path("second-device");
        let alice = Identity::generate().expect("generate");
        let laptop = Identity::generate().expect("generate");
        let uuid = alice.uuid_as_root();

        let db = open(&path);
        let mut registry = alice_registry(&alice);
        let payload = add_key_payload(&uuid, &laptop.public_key(), None);
        registry
            .add_key(
                &uuid,
                &alice.public_key(),
                laptop.public_key(),
                None,
                &alice.sign(&payload),
                1,
            )
            .expect("add key");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (restored, report) = load(&db).expect("load");

        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            restored.identity_of_key(&laptop.public_key()),
            Some(uuid),
            "the added key must resolve to the same identity after a reload"
        );
        assert_eq!(restored.identity_of_key(&alice.public_key()), Some(uuid));
        db.close().expect("close");
    }

    #[test]
    fn a_released_name_does_not_come_back_after_a_restart() {
        // The half that a naive "write what is bound" flush gets wrong: a
        // released name has nothing to write, so the stale row survives and the
        // name stays locked to someone who no longer holds it.
        let path = world_path("released-name");
        let alice = Identity::generate().expect("generate");
        let uuid = alice.uuid_as_root();

        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry.bind_name("Alice", uuid).expect("bind");
        flush(&db, &mut registry).expect("flush");

        registry.release_name("Alice");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (restored, _) = load(&db).expect("load");
        assert_eq!(
            restored.name_holder("Alice"),
            None,
            "a released name must not be resurrected by a restart"
        );
        db.close().expect("close");
    }

    #[test]
    fn a_rename_leaves_the_old_name_free_after_a_restart() {
        let path = world_path("rename");
        let alice = Identity::generate().expect("generate");
        let bob = Identity::generate().expect("generate");

        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry.insert(KeySet::new(bob.public_key(), None, 0));
        registry
            .bind_name("Alice", alice.uuid_as_root())
            .expect("bind");
        flush(&db, &mut registry).expect("flush");

        // Alice renames. Her old name must become available to Bob.
        registry
            .bind_name("Alicia", alice.uuid_as_root())
            .expect("rename");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (mut restored, _) = load(&db).expect("load");
        assert_eq!(restored.name_holder("Alicia"), Some(alice.uuid_as_root()));
        assert_eq!(
            restored.name_holder("Alice"),
            None,
            "the released name must be free after a restart"
        );
        restored
            .bind_name("Alice", bob.uuid_as_root())
            .expect("Bob should be able to take the freed name");
        db.close().expect("close");
    }

    #[test]
    fn a_revoked_key_stays_revoked_after_a_restart() {
        // Rotation revokes the old key. If the tombstone were not persisted, a
        // restart would re-authorise a key the player deliberately retired —
        // the exact failure key rotation exists to prevent.
        let path = world_path("revoked");
        let alice = Identity::generate().expect("generate");
        let successor = Identity::generate().expect("generate");
        let uuid = alice.uuid_as_root();

        let db = open(&path);
        let commitment = crate::identity::keyset::commit_to(&successor.public_key());
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), Some(commitment), 0));

        let payload =
            crate::identity::keyset::rotate_key_payload(&uuid, &successor.public_key(), None);
        registry
            .rotate_key(
                &uuid,
                &alice.public_key(),
                successor.public_key(),
                None,
                &alice.sign(&payload),
                2,
            )
            .expect("rotate");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (restored, report) = load(&db).expect("load");

        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            restored.identity_of_key(&successor.public_key()),
            Some(uuid),
            "the new key must be authorised"
        );
        assert_eq!(
            restored.identity_of_key(&alice.public_key()),
            None,
            "the rotated-away key must NOT come back after a restart"
        );
        db.close().expect("close");
    }

    #[test]
    fn flushing_twice_writes_nothing_the_second_time() {
        let path = world_path("twice");
        let alice = Identity::generate().expect("generate");
        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry
            .bind_name("Alice", alice.uuid_as_root())
            .expect("bind");

        assert!(registry.is_dirty());
        flush(&db, &mut registry).expect("flush");
        assert!(!registry.is_dirty(), "a flush must clear the dirty set");

        flush(&db, &mut registry).expect("second flush");
        assert!(!registry.is_dirty());
        db.close().expect("close");
    }

    #[test]
    fn a_loaded_registry_is_not_dirty() {
        // Otherwise every startup would rewrite every identity it just read.
        let path = world_path("not-dirty");
        let alice = Identity::generate().expect("generate");
        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry
            .bind_name("Alice", alice.uuid_as_root())
            .expect("bind");
        flush(&db, &mut registry).expect("flush");
        db.close().expect("close");

        let db = open(&path);
        let (restored, _) = load(&db).expect("load");
        assert!(
            !restored.is_dirty(),
            "a freshly loaded registry has nothing to write back"
        );
        db.close().expect("close");
    }

    #[test]
    fn a_corrupt_key_row_is_skipped_rather_than_stopping_the_server() {
        let path = world_path("corrupt");
        let alice = Identity::generate().expect("generate");
        let uuid = alice.uuid_as_root();

        let db = open(&path);
        let mut registry = alice_registry(&alice);
        registry.bind_name("Alice", uuid).expect("bind");
        flush(&db, &mut registry).expect("flush");

        // A second identity whose key is 32 zero bytes — a small-order point
        // that decodes but must never be trusted.
        db.add_player_key(&PlayerKey {
            uuid: &"0".repeat(64),
            pubkey: &[0u8; 32],
            next_key_hash: None,
            added_at: 0,
            added_by: None,
        })
        .expect("insert corrupt row");
        db.close().expect("close");

        let db = open(&path);
        let (restored, report) = load(&db).expect("load must not fail on one bad row");

        assert_eq!(report.identities, 1, "the good identity must still load");
        assert_eq!(
            report.skipped.len(),
            1,
            "and the bad one must be reported: {:?}",
            report.skipped
        );
        assert_eq!(restored.name_holder("Alice"), Some(uuid));
        db.close().expect("close");
    }

    #[test]
    fn a_name_held_by_an_unloadable_identity_is_dropped() {
        let path = world_path("ghost-name");
        let db = open(&path);
        db.set_name("Ghost", &"0".repeat(64))
            .expect("bind a name with no identity behind it");
        db.close().expect("close");

        let db = open(&path);
        let (restored, report) = load(&db).expect("load");

        assert_eq!(restored.name_holder("Ghost"), None);
        assert_eq!(report.names, 0);
        assert_eq!(report.skipped.len(), 1);
        db.close().expect("close");
    }
}
