// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's view of who exists: key sets and name bindings.
//!
//! Backed by `player_keys` and `player_names` from Task 03, which is why those
//! tables were reserved then — this is the shape they were reserved for.
//!
//! # Names are display strings; UUIDs are identity
//!
//! Charter rule 13, and it is worth being explicit about what that costs. The
//! binding is first-come and one holder per name, so the *name* is a scarce
//! resource, but nothing keys on it. Inventory, ownership, bans, mod storage —
//! all UUID. A name can be released, reassigned by an admin, or changed, and
//! nothing else in the engine notices.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, VerifyingKey};

use crate::identity::{AuthorisedKey, KeyLookup, KeySet, KeySetError, PlayerUuid, RotationProof};

/// A display name bound to an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameBinding {
    /// The name, as claimed.
    pub name: String,
    /// Who holds it.
    pub uuid: PlayerUuid,
}

/// Something went wrong updating the registry.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// The name belongs to a different identity.
    #[error("the name `{name}` is held by another player")]
    NameTaken {
        /// The contested name.
        name: String,
    },

    /// No such identity.
    #[error("unknown identity")]
    UnknownIdentity,

    /// A key-set operation failed.
    #[error(transparent)]
    KeySet(#[from] KeySetError),
}

/// Every identity the server knows, and every bound name.
///
/// In-memory; the transport layer is responsible for loading it from and
/// flushing it to the world database. Kept separate from `WorldDb` so the join
/// flow can be tested without a filesystem, which is most of why the identity
/// suite runs in microseconds.
#[derive(Debug, Default)]
pub struct IdentityRegistry {
    identities: BTreeMap<PlayerUuid, KeySet>,
    /// Every authorised key, mapped to its identity.
    ///
    /// Denormalised from the key sets because it is looked up on every join and
    /// a scan over all identities would be O(players) per handshake. Rebuilt,
    /// never edited independently — the key sets are the source of truth.
    by_key: BTreeMap<[u8; 32], PlayerUuid>,
    names: BTreeMap<String, PlayerUuid>,
}

impl IdentityRegistry {
    /// Adds or replaces an identity.
    pub fn insert(&mut self, keys: KeySet) {
        let uuid = keys.uuid();
        // Drop any stale key mappings for this identity before reindexing, so a
        // revoked key cannot linger in the lookup after a reload.
        self.by_key.retain(|_, owner| *owner != uuid);
        for entry in keys.all_keys() {
            if entry.is_active() {
                self.by_key.insert(*entry.key.as_bytes(), uuid);
            }
        }
        self.identities.insert(uuid, keys);
    }

    /// Rebuilds an identity from persisted key rows.
    ///
    /// # Errors
    ///
    /// [`RegistryError::KeySet`] if the rows do not describe a valid set.
    pub fn insert_stored(&mut self, keys: Vec<AuthorisedKey>) -> Result<(), RegistryError> {
        self.insert(KeySet::from_stored(keys)?);
        Ok(())
    }

    /// The identity a key belongs to, if it is currently authorised.
    #[must_use]
    pub fn identity_of_key(&self, key: &VerifyingKey) -> Option<PlayerUuid> {
        self.by_key.get(key.as_bytes()).copied()
    }

    /// An identity's key set.
    #[must_use]
    pub fn key_set(&self, uuid: &PlayerUuid) -> Option<&KeySet> {
        self.identities.get(uuid)
    }

    /// Whether an identity is known.
    #[must_use]
    pub fn contains(&self, uuid: &PlayerUuid) -> bool {
        self.identities.contains_key(uuid)
    }

    /// How many identities are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.identities.len()
    }

    /// Whether no identities are known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }

    /// Who holds a display name.
    #[must_use]
    pub fn name_holder(&self, name: &str) -> Option<PlayerUuid> {
        self.names.get(name).copied()
    }

    /// The name an identity holds.
    #[must_use]
    pub fn name_of(&self, uuid: &PlayerUuid) -> Option<&str> {
        self.names
            .iter()
            .find(|(_, holder)| *holder == uuid)
            .map(|(name, _)| name.as_str())
    }

    /// Binds a name to an identity, first-come.
    ///
    /// Re-binding the same name to the same identity is a no-op, so a
    /// reconnecting player is not refused their own name.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NameTaken`] if another identity holds it.
    pub fn bind_name(&mut self, name: &str, uuid: PlayerUuid) -> Result<(), RegistryError> {
        match self.names.get(name) {
            Some(holder) if *holder == uuid => Ok(()),
            Some(_) => Err(RegistryError::NameTaken {
                name: name.to_owned(),
            }),
            None => {
                // One name per identity: taking a new one releases the old,
                // otherwise a player could hoard names by reconnecting.
                if let Some(previous) = self.name_of(&uuid).map(ToOwned::to_owned) {
                    self.names.remove(&previous);
                }
                self.names.insert(name.to_owned(), uuid);
                Ok(())
            }
        }
    }

    /// Releases a name. **Admin operation** (RCON `rename`).
    pub fn release_name(&mut self, name: &str) {
        self.names.remove(name);
    }

    /// Every binding, in a stable order.
    #[must_use]
    pub fn bindings(&self) -> Vec<NameBinding> {
        self.names
            .iter()
            .map(|(name, uuid)| NameBinding {
                name: name.clone(),
                uuid: *uuid,
            })
            .collect()
    }

    /// Adds a key to an identity's set, authorised by an existing key.
    ///
    /// # Errors
    ///
    /// [`RegistryError`] if the identity is unknown or the key set refuses.
    pub fn add_key(
        &mut self,
        uuid: &PlayerUuid,
        signer: &VerifyingKey,
        new_key: VerifyingKey,
        next_key_hash: Option<[u8; 32]>,
        signature: &Signature,
        at: i64,
    ) -> Result<(), RegistryError> {
        let keys = self
            .identities
            .get_mut(uuid)
            .ok_or(RegistryError::UnknownIdentity)?;
        keys.add_key(signer, new_key, next_key_hash, signature, at)?;
        // Reindex from the key set rather than inserting directly — the set is
        // the source of truth and the index is derived.
        let updated = keys.clone();
        self.insert(updated);
        Ok(())
    }

    /// Rotates a key to its pre-committed successor.
    ///
    /// # Errors
    ///
    /// [`RegistryError`] if the identity is unknown or the rotation is refused.
    pub fn rotate_key(
        &mut self,
        uuid: &PlayerUuid,
        current: &VerifyingKey,
        new_key: VerifyingKey,
        new_next_key_hash: Option<[u8; 32]>,
        signature: &Signature,
        at: i64,
    ) -> Result<RotationProof, RegistryError> {
        let keys = self
            .identities
            .get_mut(uuid)
            .ok_or(RegistryError::UnknownIdentity)?;
        let proof = keys.rotate_key(current, new_key, new_next_key_hash, signature, at)?;
        let updated = keys.clone();
        self.insert(updated);
        Ok(proof)
    }

    /// Replaces an identity's root key. **Admin operation** (RCON `rebind`).
    ///
    /// # Errors
    ///
    /// [`RegistryError::UnknownIdentity`] if there is no such identity.
    pub fn admin_rebind(
        &mut self,
        uuid: &PlayerUuid,
        new_root: VerifyingKey,
        at: i64,
    ) -> Result<(), RegistryError> {
        let keys = self
            .identities
            .get_mut(uuid)
            .ok_or(RegistryError::UnknownIdentity)?;
        keys.admin_rebind(new_root, at);
        let updated = keys.clone();
        self.insert(updated);
        Ok(())
    }
}

impl KeyLookup for IdentityRegistry {
    fn identity_of(&self, key: &VerifyingKey) -> Option<PlayerUuid> {
        self.identity_of_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{Identity, keyset::add_key_payload};

    fn identity() -> Identity {
        Identity::generate().expect("generate")
    }

    #[test]
    fn a_key_resolves_to_its_identity() {
        let alice = identity();
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), None, 0));

        assert_eq!(
            registry.identity_of_key(&alice.public_key()),
            Some(alice.uuid_as_root())
        );
        assert_eq!(registry.identity_of_key(&identity().public_key()), None);
    }

    #[test]
    fn an_added_key_resolves_to_the_same_identity() {
        let alice = identity();
        let laptop = identity();
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), None, 0));

        let uuid = alice.uuid_as_root();
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

        assert_eq!(registry.identity_of_key(&laptop.public_key()), Some(uuid));
        assert_eq!(registry.identity_of_key(&alice.public_key()), Some(uuid));
        assert_eq!(registry.len(), 1, "a device is not a new player");
    }

    #[test]
    fn a_rotated_away_key_stops_resolving() {
        // The index is derived from the key set, so a revocation must remove it
        // rather than leaving a stale mapping that would let the old key join.
        let alice = identity();
        let successor = identity();
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(
            alice.public_key(),
            Some(crate::identity::keyset::commit_to(&successor.public_key())),
            0,
        ));

        let uuid = alice.uuid_as_root();
        let payload =
            crate::identity::keyset::rotate_key_payload(&uuid, &successor.public_key(), None);
        registry
            .rotate_key(
                &uuid,
                &alice.public_key(),
                successor.public_key(),
                None,
                &alice.sign(&payload),
                1,
            )
            .expect("rotate");

        assert_eq!(
            registry.identity_of_key(&alice.public_key()),
            None,
            "the retired key must stop resolving"
        );
        assert_eq!(
            registry.identity_of_key(&successor.public_key()),
            Some(uuid)
        );
    }

    #[test]
    fn a_rebind_swaps_every_key_but_keeps_the_identity() {
        let alice = identity();
        let new_root = identity();
        let mut registry = IdentityRegistry::default();
        registry.insert(KeySet::new(alice.public_key(), None, 0));
        let uuid = alice.uuid_as_root();
        registry.bind_name("Alice", uuid).expect("bind");

        registry
            .admin_rebind(&uuid, new_root.public_key(), 5)
            .expect("rebind");

        assert_eq!(registry.identity_of_key(&new_root.public_key()), Some(uuid));
        assert_eq!(registry.identity_of_key(&alice.public_key()), None);
        assert_eq!(
            registry.name_holder("Alice"),
            Some(uuid),
            "a rebind must not cost the player their name"
        );
    }

    #[test]
    fn a_name_is_first_come() {
        let alice = identity().uuid_as_root();
        let bob = identity().uuid_as_root();
        let mut registry = IdentityRegistry::default();

        registry.bind_name("Shared", alice).expect("first come");
        assert!(matches!(
            registry.bind_name("Shared", bob),
            Err(RegistryError::NameTaken { .. })
        ));
        assert_eq!(registry.name_holder("Shared"), Some(alice));
    }

    #[test]
    fn rebinding_your_own_name_is_a_no_op() {
        // A reconnecting player must not be refused their own name.
        let alice = identity().uuid_as_root();
        let mut registry = IdentityRegistry::default();
        registry.bind_name("Alice", alice).expect("bind");
        registry.bind_name("Alice", alice).expect("rebind own name");
        assert_eq!(registry.name_holder("Alice"), Some(alice));
    }

    #[test]
    fn taking_a_new_name_releases_the_old_one() {
        // Otherwise a player could hoard names by reconnecting under new ones.
        let alice = identity().uuid_as_root();
        let mut registry = IdentityRegistry::default();
        registry.bind_name("First", alice).expect("bind");
        registry.bind_name("Second", alice).expect("rename");

        assert_eq!(registry.name_holder("Second"), Some(alice));
        assert_eq!(
            registry.name_holder("First"),
            None,
            "the old name is released"
        );
        assert_eq!(registry.bindings().len(), 1);
    }

    #[test]
    fn releasing_a_name_frees_it_for_someone_else() {
        let alice = identity().uuid_as_root();
        let bob = identity().uuid_as_root();
        let mut registry = IdentityRegistry::default();
        registry.bind_name("Contested", alice).expect("bind");
        registry.release_name("Contested");
        registry.bind_name("Contested", bob).expect("now free");
        assert_eq!(registry.name_holder("Contested"), Some(bob));
    }

    #[test]
    fn an_unknown_identity_cannot_be_operated_on() {
        let mut registry = IdentityRegistry::default();
        let stranger = identity();
        assert!(matches!(
            registry.admin_rebind(&stranger.uuid_as_root(), stranger.public_key(), 1),
            Err(RegistryError::UnknownIdentity)
        ));
    }

    #[test]
    fn reinserting_an_identity_does_not_leave_stale_key_mappings() {
        // The index is denormalised, so the one bug it can have is going stale.
        let alice = identity();
        let successor = identity();
        let mut registry = IdentityRegistry::default();

        let mut keys = KeySet::new(
            alice.public_key(),
            Some(crate::identity::keyset::commit_to(&successor.public_key())),
            0,
        );
        registry.insert(keys.clone());
        assert!(registry.identity_of_key(&alice.public_key()).is_some());

        let payload = crate::identity::keyset::rotate_key_payload(
            &alice.uuid_as_root(),
            &successor.public_key(),
            None,
        );
        keys.rotate_key(
            &alice.public_key(),
            successor.public_key(),
            None,
            &alice.sign(&payload),
            1,
        )
        .expect("rotate");

        registry.insert(keys);
        assert_eq!(
            registry.identity_of_key(&alice.public_key()),
            None,
            "reinsert must drop the retired key from the index"
        );
    }
}
