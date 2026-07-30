// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Key sets and pre-committed rotation.
//!
//! # An identity is a set of keys, not a key
//!
//! The passkey model: many credentials, one account. Adding a device means
//! adding a key, authorised by a signature from a key you already hold — not
//! copying a secret between machines. Losing one device loses one key, not the
//! identity.
//!
//! The canonical UUID stays `BLAKE3(root pubkey)` throughout. The root key can
//! be revoked, rotated, or lost and the UUID does not move, because everything
//! else in the engine keys on it.
//!
//! # Pre-committed rotation (KERI pre-rotation)
//!
//! The problem with plain key rotation: if a thief has your current key, they
//! can rotate the identity to a key of *their* choosing and lock you out. Being
//! able to prove you were the original owner does not help — the server has no
//! way to tell which of you is lying.
//!
//! The fix is to commit to the successor **in advance**. Each key registers
//! `next_key_hash` = `BLAKE3` of the public key that will replace it. A rotation
//! is accepted only if the new key hashes to that stored commitment *and* the
//! request is signed by the current key.
//!
//! A thief with the current key therefore cannot rotate: they can sign, but
//! they do not have the pre-committed successor's private key, and any key they
//! propose will not match the commitment. The owner, who generated the
//! successor and kept it offline, can. **The commitment is made before the theft
//! and cannot be changed by the thief**, which is what makes it work.
//!
//! One consequence worth understanding: a key with no commitment
//! (`next_key_hash` = `None`) **cannot be rotated at all**. That is deliberate.
//! Allowing a thief to rotate an uncommitted key to one of their choosing is
//! exactly the attack this prevents, so an uncommitted key can only be replaced
//! by adding a new key with an existing authorised one, or by an admin rebind.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, VerifyingKey};

use super::PlayerUuid;

/// Bytes in a successor commitment.
pub const COMMITMENT_BYTES: usize = 32;

/// One authorised key in an identity's set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorisedKey {
    /// The public key.
    pub key: VerifyingKey,
    /// `BLAKE3` of this key's designated successor, if one was committed.
    ///
    /// `None` means this key cannot be rotated — see the module docs.
    pub next_key_hash: Option<[u8; COMMITMENT_BYTES]>,
    /// Unix timestamp when this key was authorised.
    pub added_at: i64,
    /// The key that authorised this one. `None` only for the root.
    pub added_by: Option<VerifyingKey>,
    /// When revoked, if it has been. Revocation is a tombstone, never a delete.
    pub revoked_at: Option<i64>,
}

impl AuthorisedKey {
    /// Whether this key may currently be used.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// Something went wrong changing a key set.
#[derive(Debug, thiserror::Error)]
pub enum KeySetError {
    /// The signing key is not in the identity's authorised set.
    #[error("the request is not signed by an authorised key for this identity")]
    NotAuthorised,

    /// The signing key is in the set but has been revoked.
    #[error("the signing key has been revoked")]
    Revoked,

    /// The signature did not verify.
    #[error("signature verification failed")]
    BadSignature,

    /// The key being added is already present.
    #[error("that key is already authorised for this identity")]
    AlreadyPresent,

    /// The key being rotated to does not match the stored commitment.
    #[error(
        "the proposed key does not match this key's pre-committed successor. A stolen key cannot \
         rotate an identity away from its owner — this is that protection working."
    )]
    CommitmentMismatch,

    /// The current key never committed to a successor.
    #[error(
        "this key has no pre-committed successor and therefore cannot be rotated. Add a new key \
         signed by an existing one, or ask an admin for a rebind."
    )]
    NoCommitment,

    /// The key to operate on is not in the set.
    #[error("that key is not part of this identity")]
    UnknownKey,

    /// Removing the last active key would leave an identity nobody can use.
    #[error("cannot revoke the last active key of an identity")]
    LastActiveKey,
}

/// What a client signs to add or rotate a key.
///
/// Domain-separated from the join challenge and from each other, so a signature
/// gathered for one purpose can never be replayed as another. Without this, a
/// signature captured during a normal join could be presented as authorisation
/// to add an attacker's key.
#[must_use]
pub fn add_key_payload(
    uuid: &PlayerUuid,
    new_key: &VerifyingKey,
    next_key_hash: Option<&[u8; COMMITMENT_BYTES]>,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(128);
    payload.extend_from_slice(b"tiamot:add-key:v1");
    payload.extend_from_slice(uuid.as_bytes());
    payload.extend_from_slice(new_key.as_bytes());
    if let Some(hash) = next_key_hash {
        payload.extend_from_slice(hash);
    }
    payload
}

/// What a client signs to rotate a key.
#[must_use]
pub fn rotate_key_payload(
    uuid: &PlayerUuid,
    new_key: &VerifyingKey,
    new_next_key_hash: Option<&[u8; COMMITMENT_BYTES]>,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(128);
    payload.extend_from_slice(b"tiamot:rotate-key:v1");
    payload.extend_from_slice(uuid.as_bytes());
    payload.extend_from_slice(new_key.as_bytes());
    if let Some(hash) = new_next_key_hash {
        payload.extend_from_slice(hash);
    }
    payload
}

/// The commitment a key makes to its successor.
#[must_use]
pub fn commit_to(successor: &VerifyingKey) -> [u8; COMMITMENT_BYTES] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:key-commitment:v1");
    hasher.update(successor.as_bytes());
    *hasher.finalize().as_bytes()
}

/// A completed rotation, for persisting and replaying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationProof {
    /// The key that was replaced.
    pub retired: VerifyingKey,
    /// The key that replaced it.
    pub successor: VerifyingKey,
    /// The successor's own commitment, carried forward.
    pub next_key_hash: Option<[u8; COMMITMENT_BYTES]>,
    /// When it happened.
    pub at: i64,
}

/// One identity's authorised keys.
///
/// Ordered by public key bytes so iteration and persistence are deterministic —
/// the key set is replayed from `player_keys` on load, and a set that came back
/// in a different order would produce different audit output on different runs.
#[derive(Debug, Clone)]
pub struct KeySet {
    uuid: PlayerUuid,
    root: VerifyingKey,
    keys: BTreeMap<[u8; 32], AuthorisedKey>,
}

impl KeySet {
    /// Creates a new identity from its root key.
    ///
    /// The root's commitment is set now or never: a key with no commitment can
    /// never be rotated (module docs).
    #[must_use]
    pub fn new(root: VerifyingKey, next_key_hash: Option<[u8; COMMITMENT_BYTES]>, at: i64) -> Self {
        let uuid = PlayerUuid::from_root_key(&root);
        let mut keys = BTreeMap::new();
        keys.insert(
            *root.as_bytes(),
            AuthorisedKey {
                key: root,
                next_key_hash,
                added_at: at,
                added_by: None,
                revoked_at: None,
            },
        );
        Self { uuid, root, keys }
    }

    /// Rebuilds a key set from persisted rows.
    ///
    /// The root is identified by having no `added_by`, which is exactly the
    /// invariant the `player_keys` schema records.
    ///
    /// # Errors
    ///
    /// [`KeySetError::UnknownKey`] if no root row is present.
    pub fn from_stored(keys: Vec<AuthorisedKey>) -> Result<Self, KeySetError> {
        let root = keys
            .iter()
            .find(|entry| entry.added_by.is_none())
            .ok_or(KeySetError::UnknownKey)?
            .key;
        Ok(Self {
            uuid: PlayerUuid::from_root_key(&root),
            root,
            keys: keys
                .into_iter()
                .map(|entry| (*entry.key.as_bytes(), entry))
                .collect(),
        })
    }

    /// The identity this set belongs to.
    #[must_use]
    pub const fn uuid(&self) -> PlayerUuid {
        self.uuid
    }

    /// The root key. The UUID is derived from this and only this.
    #[must_use]
    pub const fn root(&self) -> &VerifyingKey {
        &self.root
    }

    /// Whether a key may currently authenticate as this identity.
    #[must_use]
    pub fn is_authorised(&self, key: &VerifyingKey) -> bool {
        self.keys
            .get(key.as_bytes())
            .is_some_and(AuthorisedKey::is_active)
    }

    /// Every key, active and revoked, in a deterministic order.
    #[must_use]
    pub fn all_keys(&self) -> Vec<&AuthorisedKey> {
        self.keys.values().collect()
    }

    /// Active keys only.
    #[must_use]
    pub fn active_keys(&self) -> Vec<&AuthorisedKey> {
        self.keys
            .values()
            .filter(|entry| entry.is_active())
            .collect()
    }

    /// Adds a device key, authorised by a key already in the set.
    ///
    /// # Errors
    ///
    /// [`KeySetError`] if the signer is not authorised, the signature does not
    /// verify, or the key is already present.
    pub fn add_key(
        &mut self,
        signer: &VerifyingKey,
        new_key: VerifyingKey,
        next_key_hash: Option<[u8; COMMITMENT_BYTES]>,
        signature: &Signature,
        at: i64,
    ) -> Result<(), KeySetError> {
        self.check_signer(signer)?;

        if self.keys.contains_key(new_key.as_bytes()) {
            return Err(KeySetError::AlreadyPresent);
        }

        let payload = add_key_payload(&self.uuid, &new_key, next_key_hash.as_ref());
        signer
            .verify_strict(&payload, signature)
            .map_err(|_| KeySetError::BadSignature)?;

        self.keys.insert(
            *new_key.as_bytes(),
            AuthorisedKey {
                key: new_key,
                next_key_hash,
                added_at: at,
                added_by: Some(*signer),
                revoked_at: None,
            },
        );
        Ok(())
    }

    /// Rotates a key to its **pre-committed** successor.
    ///
    /// Accepted only if `BLAKE3(new_key)` equals the commitment the current key
    /// registered, and the request is signed by the current key. See the module
    /// docs for why both conditions are needed.
    ///
    /// The retired key is revoked rather than deleted, so the chain stays
    /// replayable.
    ///
    /// # Errors
    ///
    /// [`KeySetError`] naming which condition failed.
    pub fn rotate_key(
        &mut self,
        current: &VerifyingKey,
        new_key: VerifyingKey,
        new_next_key_hash: Option<[u8; COMMITMENT_BYTES]>,
        signature: &Signature,
        at: i64,
    ) -> Result<RotationProof, KeySetError> {
        self.check_signer(current)?;

        let commitment = self
            .keys
            .get(current.as_bytes())
            .and_then(|entry| entry.next_key_hash)
            .ok_or(KeySetError::NoCommitment)?;

        // The commitment check comes BEFORE the signature check on purpose. A
        // thief holding the current key can always produce a valid signature;
        // what they cannot do is produce a key matching a commitment made
        // before the theft. Checking that first means the error they see names
        // the actual reason they are stuck.
        if commit_to(&new_key) != commitment {
            return Err(KeySetError::CommitmentMismatch);
        }

        let payload = rotate_key_payload(&self.uuid, &new_key, new_next_key_hash.as_ref());
        current
            .verify_strict(&payload, signature)
            .map_err(|_| KeySetError::BadSignature)?;

        if let Some(entry) = self.keys.get_mut(current.as_bytes()) {
            entry.revoked_at = Some(at);
        }

        self.keys.insert(
            *new_key.as_bytes(),
            AuthorisedKey {
                key: new_key,
                next_key_hash: new_next_key_hash,
                added_at: at,
                // The successor inherits the retired key's position: if the
                // root rotates, the successor is still not the root, because
                // the UUID must not move.
                added_by: Some(*current),
                revoked_at: None,
            },
        );

        Ok(RotationProof {
            retired: *current,
            successor: new_key,
            next_key_hash: new_next_key_hash,
            at,
        })
    }

    /// Revokes a key.
    ///
    /// # Errors
    ///
    /// [`KeySetError::UnknownKey`] if it is not in the set, or
    /// [`KeySetError::LastActiveKey`] if it is the only one left — an identity
    /// with no usable key is unreachable except by admin rebind.
    pub fn revoke_key(&mut self, key: &VerifyingKey, at: i64) -> Result<(), KeySetError> {
        if !self.keys.contains_key(key.as_bytes()) {
            return Err(KeySetError::UnknownKey);
        }
        if self.active_keys().len() <= 1 {
            return Err(KeySetError::LastActiveKey);
        }
        if let Some(entry) = self.keys.get_mut(key.as_bytes()) {
            entry.revoked_at = Some(at);
        }
        Ok(())
    }

    /// Replaces the identity's root key entirely. **Admin operation.**
    ///
    /// The escape hatch for a player with no recovery phrase and no second
    /// device. Deliberately not reachable from the protocol: it is an RCON
    /// command, it is audit-logged, and it is the one operation that can move an
    /// identity to a key its owner never signed for.
    ///
    /// **The UUID does not change**, which is the entire point — inventory,
    /// ownership, and mod state all key on it and must survive.
    pub fn admin_rebind(&mut self, new_root: VerifyingKey, at: i64) {
        for entry in self.keys.values_mut() {
            if entry.revoked_at.is_none() {
                entry.revoked_at = Some(at);
            }
        }
        self.keys.insert(
            *new_root.as_bytes(),
            AuthorisedKey {
                key: new_root,
                next_key_hash: None,
                added_at: at,
                // Marked as authorised by the OLD root, not as a new root: the
                // UUID is derived from the original root and must keep deriving
                // from it. Recording this as a root row would change the
                // identity's UUID on the next reload.
                added_by: Some(self.root),
                revoked_at: None,
            },
        );
    }

    fn check_signer(&self, signer: &VerifyingKey) -> Result<(), KeySetError> {
        match self.keys.get(signer.as_bytes()) {
            None => Err(KeySetError::NotAuthorised),
            Some(entry) if !entry.is_active() => Err(KeySetError::Revoked),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    fn identity() -> Identity {
        Identity::generate().expect("generate")
    }

    /// A key set whose root has committed to `successor`.
    fn set_with_commitment(root: &Identity, successor: &Identity) -> KeySet {
        KeySet::new(
            root.public_key(),
            Some(commit_to(&successor.public_key())),
            100,
        )
    }

    #[test]
    fn the_root_key_is_authorised_and_defines_the_uuid() {
        let root = identity();
        let set = KeySet::new(root.public_key(), None, 1);
        assert!(set.is_authorised(&root.public_key()));
        assert_eq!(set.uuid(), root.uuid_as_root());
    }

    #[test]
    fn a_stranger_is_not_authorised() {
        let set = KeySet::new(identity().public_key(), None, 1);
        assert!(!set.is_authorised(&identity().public_key()));
    }

    #[test]
    fn a_second_device_can_be_added_by_the_first() {
        // The passkey model: many credentials, one account.
        let root = identity();
        let laptop = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = add_key_payload(&set.uuid(), &laptop.public_key(), None);
        set.add_key(
            &root.public_key(),
            laptop.public_key(),
            None,
            &root.sign(&payload),
            2,
        )
        .expect("the root should be able to add a device");

        assert!(set.is_authorised(&laptop.public_key()));
        assert_eq!(
            set.uuid(),
            root.uuid_as_root(),
            "adding a device must not move the UUID"
        );
    }

    #[test]
    fn an_add_key_signed_by_an_unauthorised_key_is_rejected() {
        let root = identity();
        let mallory = identity();
        let target = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = add_key_payload(&set.uuid(), &target.public_key(), None);
        let err = set
            .add_key(
                &mallory.public_key(),
                target.public_key(),
                None,
                &mallory.sign(&payload),
                2,
            )
            .expect_err("a stranger must not be able to add keys");
        assert!(matches!(err, KeySetError::NotAuthorised), "{err:?}");
        assert!(!set.is_authorised(&target.public_key()));
    }

    #[test]
    fn an_add_key_with_a_forged_signature_is_rejected() {
        // The signer IS authorised, but the signature is not theirs.
        let root = identity();
        let laptop = identity();
        let mallory = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = add_key_payload(&set.uuid(), &laptop.public_key(), None);
        let err = set
            .add_key(
                &root.public_key(),
                laptop.public_key(),
                None,
                &mallory.sign(&payload),
                2,
            )
            .expect_err("a forged signature must be rejected");
        assert!(matches!(err, KeySetError::BadSignature), "{err:?}");
    }

    #[test]
    fn a_precommitted_rotation_succeeds() {
        let root = identity();
        let successor = identity();
        let mut set = set_with_commitment(&root, &successor);

        let payload = rotate_key_payload(&set.uuid(), &successor.public_key(), None);
        let proof = set
            .rotate_key(
                &root.public_key(),
                successor.public_key(),
                None,
                &root.sign(&payload),
                200,
            )
            .expect("a rotation matching the commitment should succeed");

        assert_eq!(proof.retired, root.public_key());
        assert_eq!(proof.successor, successor.public_key());
        assert!(set.is_authorised(&successor.public_key()));
        assert!(
            !set.is_authorised(&root.public_key()),
            "the retired key must stop working"
        );
        assert_eq!(
            set.uuid(),
            root.uuid_as_root(),
            "rotation must not move the UUID"
        );
    }

    #[test]
    fn a_thief_with_the_current_key_cannot_rotate_the_identity_away() {
        // THE attack pre-rotation exists to stop. Mallory has stolen the
        // current key outright — she can sign anything. What she cannot do is
        // produce a key matching a commitment the owner made before the theft.
        let root = identity();
        let owner_successor = identity(); // kept offline by the owner
        let mallory_key = identity(); // the thief's chosen replacement
        let mut set = set_with_commitment(&root, &owner_successor);

        let payload = rotate_key_payload(&set.uuid(), &mallory_key.public_key(), None);
        let err = set
            .rotate_key(
                &root.public_key(),
                mallory_key.public_key(),
                None,
                // A perfectly valid signature from the stolen key.
                &root.sign(&payload),
                200,
            )
            .expect_err("a thief must not be able to rotate to a key of their choosing");

        assert!(matches!(err, KeySetError::CommitmentMismatch), "{err:?}");
        assert!(
            set.is_authorised(&root.public_key()),
            "the failed rotation must change nothing"
        );
        assert!(!set.is_authorised(&mallory_key.public_key()));

        // And the real owner can still rotate to the key they committed to.
        let payload = rotate_key_payload(&set.uuid(), &owner_successor.public_key(), None);
        set.rotate_key(
            &root.public_key(),
            owner_successor.public_key(),
            None,
            &root.sign(&payload),
            201,
        )
        .expect("the owner's committed successor should still work");
    }

    #[test]
    fn a_key_with_no_commitment_cannot_be_rotated() {
        // Deliberate: allowing an uncommitted key to rotate anywhere would
        // hand a thief exactly the capability pre-rotation removes.
        let root = identity();
        let successor = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = rotate_key_payload(&set.uuid(), &successor.public_key(), None);
        let err = set
            .rotate_key(
                &root.public_key(),
                successor.public_key(),
                None,
                &root.sign(&payload),
                2,
            )
            .expect_err("an uncommitted key must not be rotatable");
        assert!(matches!(err, KeySetError::NoCommitment), "{err:?}");
        assert!(
            err.to_string().contains("rebind"),
            "the error should say what to do instead"
        );
    }

    #[test]
    fn a_rotation_with_a_forged_signature_is_rejected_even_when_committed() {
        let root = identity();
        let successor = identity();
        let mallory = identity();
        let mut set = set_with_commitment(&root, &successor);

        let payload = rotate_key_payload(&set.uuid(), &successor.public_key(), None);
        let err = set
            .rotate_key(
                &root.public_key(),
                successor.public_key(),
                None,
                &mallory.sign(&payload),
                200,
            )
            .expect_err("both conditions are required, not either");
        assert!(matches!(err, KeySetError::BadSignature), "{err:?}");
    }

    #[test]
    fn rotation_chains_when_each_key_commits_to_the_next() {
        let root = identity();
        let second = identity();
        let third = identity();

        let mut set = set_with_commitment(&root, &second);

        let payload = rotate_key_payload(
            &set.uuid(),
            &second.public_key(),
            Some(&commit_to(&third.public_key())),
        );
        set.rotate_key(
            &root.public_key(),
            second.public_key(),
            Some(commit_to(&third.public_key())),
            &root.sign(&payload),
            2,
        )
        .expect("first rotation");

        let payload = rotate_key_payload(&set.uuid(), &third.public_key(), None);
        set.rotate_key(
            &second.public_key(),
            third.public_key(),
            None,
            &second.sign(&payload),
            3,
        )
        .expect("the chain should continue");

        assert!(set.is_authorised(&third.public_key()));
        assert_eq!(set.uuid(), root.uuid_as_root(), "the UUID never moves");
    }

    #[test]
    fn a_revoked_key_cannot_authorise_anything() {
        let root = identity();
        let laptop = identity();
        let target = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = add_key_payload(&set.uuid(), &laptop.public_key(), None);
        set.add_key(
            &root.public_key(),
            laptop.public_key(),
            None,
            &root.sign(&payload),
            2,
        )
        .expect("add");
        set.revoke_key(&laptop.public_key(), 3).expect("revoke");

        assert!(!set.is_authorised(&laptop.public_key()));
        let payload = add_key_payload(&set.uuid(), &target.public_key(), None);
        assert!(matches!(
            set.add_key(
                &laptop.public_key(),
                target.public_key(),
                None,
                &laptop.sign(&payload),
                4
            ),
            Err(KeySetError::Revoked)
        ));
    }

    #[test]
    fn the_last_active_key_cannot_be_revoked() {
        let root = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);
        assert!(matches!(
            set.revoke_key(&root.public_key(), 2),
            Err(KeySetError::LastActiveKey)
        ));
    }

    #[test]
    fn revocation_is_a_tombstone_so_the_chain_stays_replayable() {
        let root = identity();
        let successor = identity();
        let mut set = set_with_commitment(&root, &successor);

        let payload = rotate_key_payload(&set.uuid(), &successor.public_key(), None);
        set.rotate_key(
            &root.public_key(),
            successor.public_key(),
            None,
            &root.sign(&payload),
            200,
        )
        .expect("rotate");

        assert_eq!(
            set.all_keys().len(),
            2,
            "the retired key must still be recorded"
        );
        assert_eq!(set.active_keys().len(), 1);
    }

    #[test]
    fn an_admin_rebind_replaces_every_key_but_keeps_the_uuid() {
        // The escape hatch: no phrase, no second device. It has to work, and it
        // has to preserve everything keyed on the UUID.
        let root = identity();
        let lost_laptop = identity();
        let new_root = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);

        let payload = add_key_payload(&set.uuid(), &lost_laptop.public_key(), None);
        set.add_key(
            &root.public_key(),
            lost_laptop.public_key(),
            None,
            &root.sign(&payload),
            2,
        )
        .expect("add");

        let original_uuid = set.uuid();
        set.admin_rebind(new_root.public_key(), 500);

        assert!(set.is_authorised(&new_root.public_key()));
        assert!(
            !set.is_authorised(&root.public_key()),
            "old keys must be revoked"
        );
        assert!(!set.is_authorised(&lost_laptop.public_key()));
        assert_eq!(
            set.uuid(),
            original_uuid,
            "the UUID must survive a rebind, or the player loses everything keyed on it"
        );
    }

    #[test]
    fn a_rebound_set_keeps_its_uuid_after_a_reload() {
        // The subtle one: if the rebind recorded the new key as a ROOT row, the
        // UUID would be recomputed from it on the next load and the player
        // would lose everything anyway.
        let root = identity();
        let new_root = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);
        let original_uuid = set.uuid();

        set.admin_rebind(new_root.public_key(), 500);

        let reloaded =
            KeySet::from_stored(set.all_keys().into_iter().cloned().collect()).expect("reload");
        assert_eq!(
            reloaded.uuid(),
            original_uuid,
            "a reload must not recompute the UUID from the rebound key"
        );
        assert!(reloaded.is_authorised(&new_root.public_key()));
    }

    #[test]
    fn a_key_set_round_trips_through_storage() {
        let root = identity();
        let laptop = identity();
        let mut set = KeySet::new(root.public_key(), Some(commit_to(&laptop.public_key())), 1);
        let payload = add_key_payload(&set.uuid(), &laptop.public_key(), None);
        set.add_key(
            &root.public_key(),
            laptop.public_key(),
            None,
            &root.sign(&payload),
            2,
        )
        .expect("add");

        let reloaded =
            KeySet::from_stored(set.all_keys().into_iter().cloned().collect()).expect("reload");
        assert_eq!(reloaded.uuid(), set.uuid());
        assert!(reloaded.is_authorised(&root.public_key()));
        assert!(reloaded.is_authorised(&laptop.public_key()));
    }

    #[test]
    fn add_and_rotate_payloads_are_domain_separated() {
        // A signature gathered for one purpose must never be replayable as
        // another — including as a join challenge.
        let uuid = identity().uuid_as_root();
        let key = identity().public_key();
        let add = add_key_payload(&uuid, &key, None);
        let rotate = rotate_key_payload(&uuid, &key, None);
        assert_ne!(add, rotate);

        let nonce = crate::identity::generate_nonce().expect("nonce");
        let challenge = crate::identity::challenge_payload(&nonce, b"fp", 1);
        assert_ne!(add, challenge);
        assert_ne!(rotate, challenge);
    }

    #[test]
    fn adding_a_key_twice_is_refused() {
        let root = identity();
        let laptop = identity();
        let mut set = KeySet::new(root.public_key(), None, 1);
        let payload = add_key_payload(&set.uuid(), &laptop.public_key(), None);
        set.add_key(
            &root.public_key(),
            laptop.public_key(),
            None,
            &root.sign(&payload),
            2,
        )
        .expect("add");
        assert!(matches!(
            set.add_key(
                &root.public_key(),
                laptop.public_key(),
                None,
                &root.sign(&payload),
                3
            ),
            Err(KeySetError::AlreadyPresent)
        ));
    }
}
