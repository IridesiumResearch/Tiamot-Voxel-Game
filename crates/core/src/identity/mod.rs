// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Player identity: cryptographic, and **recoverable**.
//!
//! Charter rule 13, implemented whole. The whole model lands together because
//! retrofitting recovery onto a shipped protocol and a shipped `players` table
//! is a breaking change, and because a half-implemented identity system is
//! worse than none — it looks like it works right up until someone loses a
//! laptop.
//!
//! # The design in one paragraph
//!
//! An identity is an Ed25519 key. **A key you can only lose once is a design
//! defect, not security**, so: the 32-byte secret key *is* the BIP-39 seed, so a
//! 24-word phrase reconstructs it exactly; an identity is a *set* of authorised
//! keys rather than one key, so adding a device does not mean moving one; each
//! key pre-commits to the hash of its designated successor, so a stolen key
//! cannot rotate the identity away from its owner; and an admin can rebind a
//! UUID to a new root key for the player who has neither phrase nor second
//! device.
//!
//! # Why the seed *is* the key
//!
//! BIP-39 normally produces a seed that is then run through a derivation path
//! to get keys. That indirection buys nothing here and costs clarity: an
//! Ed25519 secret key is exactly 32 bytes of uniform entropy, which is exactly
//! what BIP-39's 24-word form encodes. Using the entropy directly means there is
//! precisely one key a phrase can produce, and no "which derivation path did we
//! use in 2026?" question in five years.
//!
//! # The honest limit
//!
//! **Keypairs are free.** Anyone can mint unlimited identities in a loop, so
//! UUID bans are trivially evaded and this system does not solve moderation.
//! It solves *impersonation*: nobody can take your name or your build. Bans need
//! IP/subnet blocking, [`Allowlist`], or a community identity service behind
//! [`AuthProvider`]. This is stated here, in the code, because a reader who
//! believes otherwise will build something that depends on it being false.

pub mod keyset;
mod phrase;

pub use keyset::{AuthorisedKey, KeySet, KeySetError, RotationProof};
pub use phrase::{PhraseError, RecoveryPhrase};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// Bytes in an Ed25519 secret key, a BIP-39 24-word entropy payload, and the
/// engine's identity seed. All the same number, which is the point.
pub const SEED_BYTES: usize = 32;

/// Bytes in a canonical player UUID.
pub const UUID_BYTES: usize = 32;

/// Bytes in a handshake nonce.
pub const NONCE_BYTES: usize = 32;

/// A player's canonical identifier: `BLAKE3` of their **root** public key.
///
/// **Never changes**, whatever happens to the key set — devices added, keys
/// rotated, keys revoked. That stability is what lets every other system key on
/// it: inventory, ownership, bans, mod storage, and later the mimic imprint all
/// use the UUID and never the display name.
/// Serialised as its 32 raw bytes, which is what makes it usable as a key in
/// anything that persists — an entity's owner, a nametag bound to a player, a
/// mod's storage. Not as hex: that is a display form, and a round trip through
/// one is a place for a world file to disagree with itself.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct PlayerUuid([u8; UUID_BYTES]);

impl PlayerUuid {
    /// Derives the UUID from a root public key.
    #[must_use]
    pub fn from_root_key(root: &VerifyingKey) -> Self {
        // Domain-separated so a hash of a pubkey in one context can never be
        // mistaken for a hash of the same bytes in another.
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tiamot:player-uuid:v1");
        hasher.update(root.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    /// The raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; UUID_BYTES] {
        &self.0
    }

    /// Wraps raw bytes, for loading from storage.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; UUID_BYTES]) -> Self {
        Self(bytes)
    }

    /// Lowercase hex, as stored in the world database and shown in admin tools.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(UUID_BYTES * 2);
        for byte in &self.0 {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parses the hex form.
    ///
    /// # Errors
    ///
    /// [`IdentityError::BadUuid`] if the string is not 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self, IdentityError> {
        if text.len() != UUID_BYTES * 2 {
            return Err(IdentityError::BadUuid {
                text: text.to_owned(),
            });
        }
        let mut bytes = [0u8; UUID_BYTES];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let pair =
                text.get(index * 2..index * 2 + 2)
                    .ok_or_else(|| IdentityError::BadUuid {
                        text: text.to_owned(),
                    })?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| IdentityError::BadUuid {
                text: text.to_owned(),
            })?;
        }
        Ok(Self(bytes))
    }

    /// A short prefix, for logs. **Never** for identification.
    #[must_use]
    pub fn short(&self) -> String {
        self.to_hex()[..12].to_owned()
    }
}

impl std::fmt::Debug for PlayerUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlayerUuid({})", self.short())
    }
}

impl std::fmt::Display for PlayerUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Something went wrong with an identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A UUID string is malformed.
    #[error("`{text}` is not a valid player UUID (expected 64 hex characters)")]
    BadUuid {
        /// What was supplied.
        text: String,
    },

    /// A public key is malformed.
    #[error("malformed public key")]
    BadPublicKey,

    /// A signature is malformed.
    #[error("malformed signature")]
    BadSignature,

    /// A signature did not verify.
    #[error("signature verification failed")]
    VerificationFailed,

    /// The system entropy source failed.
    #[error("could not obtain secure randomness")]
    Entropy,

    /// A key file could not be read or written.
    #[error("could not access the identity key file at `{path}`")]
    KeyFile {
        /// The path.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// A key file is not the right size.
    #[error("identity key file at `{path}` is {found} bytes, expected {SEED_BYTES}")]
    KeyFileSize {
        /// The path.
        path: String,
        /// Size found.
        found: usize,
    },
}

/// A player's secret identity. **Never leaves the client.**
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Generates a fresh identity from system entropy.
    ///
    /// # Errors
    ///
    /// [`IdentityError::Entropy`] if the system randomness source fails, which
    /// is not something to paper over with a weaker fallback.
    pub fn generate() -> Result<Self, IdentityError> {
        let mut seed = [0u8; SEED_BYTES];
        getrandom::fill(&mut seed).map_err(|_| IdentityError::Entropy)?;
        Ok(Self::from_seed(&seed))
    }

    /// Reconstructs an identity from its 32-byte seed.
    ///
    /// The seed *is* the secret key — see the module docs.
    #[must_use]
    pub fn from_seed(seed: &[u8; SEED_BYTES]) -> Self {
        Self {
            signing: SigningKey::from_bytes(seed),
        }
    }

    /// The seed, for rendering a recovery phrase or writing the key file.
    ///
    /// Handle as the secret it is.
    #[must_use]
    pub fn seed(&self) -> [u8; SEED_BYTES] {
        self.signing.to_bytes()
    }

    /// The public key.
    #[must_use]
    pub fn public_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// The canonical UUID, treating this key as the root.
    ///
    /// For a key added later to an existing identity, the UUID is the *root's*,
    /// not this one's — see [`KeySet`].
    #[must_use]
    pub fn uuid_as_root(&self) -> PlayerUuid {
        PlayerUuid::from_root_key(&self.public_key())
    }

    /// The 24-word recovery phrase.
    ///
    /// # Errors
    ///
    /// [`PhraseError`] if encoding fails, which for a 32-byte seed cannot
    /// happen.
    pub fn recovery_phrase(&self) -> Result<RecoveryPhrase, PhraseError> {
        RecoveryPhrase::from_seed(&self.seed())
    }

    /// Signs a payload.
    #[must_use]
    pub fn sign(&self, payload: &[u8]) -> Signature {
        self.signing.sign(payload)
    }

    /// Loads from a key file, or creates one if absent.
    ///
    /// The file is the seed, raw, 32 bytes. On Unix it is written `0600`.
    ///
    /// # Deliberately not passphrase-encrypted
    ///
    /// A prompt on every launch is the kind of friction that makes people stop
    /// playing, and it would protect against an attacker who already has read
    /// access to the user's home directory — at which point they have the
    /// session too. The recovery phrase is the real backup, and the threat this
    /// system actually defends against is impersonation over the network, not
    /// local disk access. Stated rather than left implicit, because "why is
    /// this not encrypted" deserves an answer in the code.
    ///
    /// # Errors
    ///
    /// [`IdentityError::KeyFile`] on I/O failure, or
    /// [`IdentityError::KeyFileSize`] if an existing file is the wrong size.
    pub fn load_or_create(path: &std::path::Path) -> Result<Self, IdentityError> {
        if path.exists() {
            let bytes = std::fs::read(path).map_err(|source| IdentityError::KeyFile {
                path: path.display().to_string(),
                source,
            })?;
            let seed: [u8; SEED_BYTES] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| IdentityError::KeyFileSize {
                        path: path.display().to_string(),
                        found: bytes.len(),
                    })?;
            return Ok(Self::from_seed(&seed));
        }

        let identity = Self::generate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IdentityError::KeyFile {
                path: path.display().to_string(),
                source,
            })?;
        }
        std::fs::write(path, identity.seed()).map_err(|source| IdentityError::KeyFile {
            path: path.display().to_string(),
            source,
        })?;
        restrict_permissions(path)?;
        Ok(identity)
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret, not even by accident in a log line.
        write!(f, "Identity({})", self.uuid_as_root().short())
    }
}

/// Makes a key file readable only by its owner.
///
/// The `Result` is load-bearing on Unix and vestigial on Windows, where the
/// file inherits the user profile's ACL and there is nothing to do. The
/// signature stays uniform so the caller does not branch on platform — which is
/// exactly the shape clippy objects to on the Windows build only, and the
/// reason this allow is here rather than a `cfg`-split function.
#[allow(
    clippy::unnecessary_wraps,
    reason = "infallible on Windows, fallible on Unix; one signature for both callers"
)]
fn restrict_permissions(path: &std::path::Path) -> Result<(), IdentityError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| IdentityError::KeyFile {
                path: path.display().to_string(),
                source,
            },
        )?;
    }
    #[cfg(not(unix))]
    {
        // Windows inherits the user profile's ACL, which is already
        // user-only for a file under the platform data directory. Recorded
        // rather than silently skipped.
        let _ = path;
    }
    Ok(())
}

/// The payload a client signs to prove it holds a key.
///
/// **The binding is the security property**, not the signature itself. The
/// payload is `nonce ‖ server cert fingerprint ‖ protocol version`, which makes
/// a signature useless anywhere but the exact server and session it was made
/// for:
///
/// - the **nonce** is fresh per connection, so a captured signature cannot be
///   replayed to the same server;
/// - the **cert fingerprint** ties it to one server, so a man-in-the-middle
///   cannot relay a handshake captured on a server it controls;
/// - the **protocol version** stops a signature made under one wire format
///   being reinterpreted under another.
///
/// Drop any of the three and the handshake becomes forwardable.
#[must_use]
pub fn challenge_payload(
    nonce: &[u8; NONCE_BYTES],
    server_cert_fingerprint: &[u8],
    protocol_version: u32,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(NONCE_BYTES + server_cert_fingerprint.len() + 4 + 24);
    // Domain separation, so this signature can never be confused with one made
    // over an AddKey or a RotateKey record.
    payload.extend_from_slice(b"tiamot:auth-challenge:v1");
    payload.extend_from_slice(nonce);
    payload.extend_from_slice(server_cert_fingerprint);
    payload.extend_from_slice(&protocol_version.to_le_bytes());
    payload
}

/// Generates a fresh handshake nonce.
///
/// # Errors
///
/// [`IdentityError::Entropy`] if system randomness fails. A predictable nonce
/// would make every signature replayable, so a fallback would be worse than a
/// failure.
pub fn generate_nonce() -> Result<[u8; NONCE_BYTES], IdentityError> {
    let mut nonce = [0u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| IdentityError::Entropy)?;
    Ok(nonce)
}

/// Verifies a client's proof that it holds a key.
///
/// # Errors
///
/// [`IdentityError::VerificationFailed`] if the signature does not match.
pub fn verify_challenge(
    key: &VerifyingKey,
    nonce: &[u8; NONCE_BYTES],
    server_cert_fingerprint: &[u8],
    protocol_version: u32,
    signature: &Signature,
) -> Result<(), IdentityError> {
    let payload = challenge_payload(nonce, server_cert_fingerprint, protocol_version);
    // `verify_strict` rather than `verify`: it additionally rejects signatures
    // whose R component is a small-order point, closing the malleability that
    // makes one signature verify under several keys. The cost is a few
    // microseconds on a once-per-connection operation.
    key.verify_strict(&payload, signature)
        .map_err(|_| IdentityError::VerificationFailed)
}

/// Parses a public key from wire bytes, **rejecting weak keys**.
///
/// # Small-order keys are refused
///
/// `VerifyingKey::from_bytes` accepts small-order points — 32 zero bytes decodes
/// successfully, which is not the intuition and is worth knowing. Such keys sit
/// in a tiny subgroup and admit signature forgeries under non-strict
/// verification, so a peer offering one is either broken or probing.
///
/// This is the charter rule 14 posture applied to cryptography: the value
/// arrives from the network, so it is checked rather than assumed well-formed.
/// Found because a test asserted all-zero bytes would fail to decode and it did
/// not.
///
/// # Errors
///
/// [`IdentityError::BadPublicKey`] if the bytes are the wrong length, not a
/// valid point, or a small-order point. **Reachable from the network**, so it
/// must not panic.
pub fn public_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey, IdentityError> {
    let array: [u8; 32] = bytes.try_into().map_err(|_| IdentityError::BadPublicKey)?;
    let key = VerifyingKey::from_bytes(&array).map_err(|_| IdentityError::BadPublicKey)?;
    if key.is_weak() {
        return Err(IdentityError::BadPublicKey);
    }
    Ok(key)
}

/// Parses a signature from wire bytes.
///
/// # Errors
///
/// [`IdentityError::BadSignature`] if the length is wrong.
pub fn signature_from_bytes(bytes: &[u8]) -> Result<Signature, IdentityError> {
    let array: [u8; 64] = bytes.try_into().map_err(|_| IdentityError::BadSignature)?;
    Ok(Signature::from_bytes(&array))
}

/// Where identity verification happens.
///
/// The built-in implementation is self-sovereign: a key is valid if it is in the
/// identity's authorised set. The trait exists so a community identity service
/// can be added later **without a protocol break** — the wire messages already
/// carry everything such a provider would need.
///
/// Do not add a second implementation speculatively.
pub trait AuthProvider {
    /// Verifies a completed challenge and returns the identity it proves.
    ///
    /// The key lookup is a **parameter rather than a field** so a provider is
    /// stateless: one instance serves every session, and a caller holding
    /// `&mut` on its identity store can still call this by reborrowing. An
    /// earlier version captured the store by reference and made those two
    /// things mutually exclusive.
    ///
    /// # Errors
    ///
    /// Any verification failure, with a reason suitable for a disconnect
    /// message.
    fn verify(
        &self,
        keys: &dyn KeyLookup,
        claimed_key: &VerifyingKey,
        nonce: &[u8; NONCE_BYTES],
        server_cert_fingerprint: &[u8],
        protocol_version: u32,
        signature: &Signature,
    ) -> Result<Verified, IdentityError>;
}

/// Who a proven key turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    /// The key belongs to an identity the server already knows.
    Existing(PlayerUuid),

    /// The key is unknown, so it is the root of a new identity.
    ///
    /// The caller registers it. Kept distinct from
    /// [`Existing`](Self::Existing) because "first join" is a decision with
    /// consequences — it writes to the database and it is the one moment an
    /// allowlist can turn someone away for good — and a caller that could not
    /// tell the two apart would silently create identities.
    New(PlayerUuid),
}

impl Verified {
    /// The identity, however it was resolved.
    #[must_use]
    pub const fn uuid(&self) -> PlayerUuid {
        match self {
            Self::Existing(uuid) | Self::New(uuid) => *uuid,
        }
    }

    /// Whether this is a first join.
    #[must_use]
    pub const fn is_new(&self) -> bool {
        matches!(self, Self::New(_))
    }
}

/// The built-in self-sovereign provider.
///
/// Looks the claimed key up in a [`KeySet`], so **any** authorised key of an
/// identity can join as that identity — which is the whole point of key sets.
///
/// Stateless: the key store is passed to [`AuthProvider::verify`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SelfSovereign;

/// Finds which identity, if any, a public key belongs to.
pub trait KeyLookup {
    /// The identity this key is authorised for.
    fn identity_of(&self, key: &VerifyingKey) -> Option<PlayerUuid>;
}

impl AuthProvider for SelfSovereign {
    fn verify(
        &self,
        keys: &dyn KeyLookup,
        claimed_key: &VerifyingKey,
        nonce: &[u8; NONCE_BYTES],
        server_cert_fingerprint: &[u8],
        protocol_version: u32,
        signature: &Signature,
    ) -> Result<Verified, IdentityError> {
        // Signature FIRST, then lookup. Checking membership before proof would
        // let an attacker probe which keys a server knows about.
        verify_challenge(
            claimed_key,
            nonce,
            server_cert_fingerprint,
            protocol_version,
            signature,
        )?;
        Ok(match keys.identity_of(claimed_key) {
            Some(uuid) => Verified::Existing(uuid),
            // An unknown key is a NEW identity, not a rejection. That is what
            // "self-sovereign" means: nobody issues you an account, you bring
            // your own key and the UUID falls out of it. A server that refused
            // unknown keys would be one nobody could ever join for the first
            // time.
            //
            // Restricting who may join is the allowlist's job, checked by the
            // caller against the resolved UUID. Keeping the two separate means
            // an allowlisted server still derives the same UUID for a player as
            // an open one, so moving a world between the two does not change
            // anybody's identity.
            None => Verified::New(PlayerUuid::from_root_key(claimed_key)),
        })
    }
}

/// An explicit list of identities permitted to join.
///
/// Ten lines, and it is what small private servers actually use. Included
/// because the honest sybil note above means UUID bans do not work: an
/// allowlist inverts the problem, and inverting it is the only thing that
/// actually holds.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    enabled: bool,
    permitted: std::collections::BTreeSet<PlayerUuid>,
}

impl Allowlist {
    /// A disabled allowlist. Everyone may join.
    #[must_use]
    pub fn open() -> Self {
        Self::default()
    }

    /// An enabled allowlist containing exactly these identities.
    #[must_use]
    pub fn restricted(permitted: impl IntoIterator<Item = PlayerUuid>) -> Self {
        Self {
            enabled: true,
            permitted: permitted.into_iter().collect(),
        }
    }

    /// Whether the allowlist is being enforced.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Turns enforcement on or off.
    pub const fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Adds an identity.
    pub fn allow(&mut self, uuid: PlayerUuid) {
        self.permitted.insert(uuid);
    }

    /// Removes an identity.
    pub fn revoke(&mut self, uuid: &PlayerUuid) {
        self.permitted.remove(uuid);
    }

    /// Whether an identity may join.
    #[must_use]
    pub fn permits(&self, uuid: &PlayerUuid) -> bool {
        !self.enabled || self.permitted.contains(uuid)
    }

    /// Every permitted identity, in a stable order.
    #[must_use]
    pub fn entries(&self) -> Vec<PlayerUuid> {
        self.permitted.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_stable_for_a_key_and_distinct_between_keys() {
        let a = Identity::generate().expect("generate");
        let b = Identity::generate().expect("generate");
        assert_eq!(a.uuid_as_root(), a.uuid_as_root());
        assert_ne!(a.uuid_as_root(), b.uuid_as_root());
    }

    #[test]
    fn a_uuid_round_trips_through_hex() {
        let identity = Identity::generate().expect("generate");
        let uuid = identity.uuid_as_root();
        assert_eq!(PlayerUuid::from_hex(&uuid.to_hex()).expect("parse"), uuid);
    }

    #[test]
    fn malformed_uuid_text_is_an_error_not_a_panic() {
        for bad in ["", "zz", &"g".repeat(64), &"ab".repeat(31)] {
            assert!(PlayerUuid::from_hex(bad).is_err(), "`{bad}` should fail");
        }
    }

    #[test]
    fn the_seed_is_the_key() {
        // The property the whole recovery story rests on: a phrase encodes the
        // seed, the seed IS the secret key, so a phrase reproduces exactly one
        // identity with no derivation path to remember.
        let seed = [7u8; SEED_BYTES];
        let first = Identity::from_seed(&seed);
        let second = Identity::from_seed(&seed);
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(first.seed(), seed);
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let identity = Identity::from_seed(&[0xAB; SEED_BYTES]);
        let printed = format!("{identity:?}");
        assert!(!printed.contains("ab".repeat(8).as_str()), "{printed}");
        assert!(printed.contains(&identity.uuid_as_root().short()));
    }

    #[test]
    fn a_valid_challenge_verifies() {
        let identity = Identity::generate().expect("generate");
        let nonce = generate_nonce().expect("nonce");
        let fingerprint = b"server-cert-fingerprint";
        let signature = identity.sign(&challenge_payload(&nonce, fingerprint, 1));

        verify_challenge(&identity.public_key(), &nonce, fingerprint, 1, &signature)
            .expect("a valid challenge should verify");
    }

    #[test]
    fn a_replayed_signature_with_a_stale_nonce_is_rejected() {
        let identity = Identity::generate().expect("generate");
        let old_nonce = generate_nonce().expect("nonce");
        let new_nonce = generate_nonce().expect("nonce");
        let fingerprint = b"fp";
        let captured = identity.sign(&challenge_payload(&old_nonce, fingerprint, 1));

        assert!(
            verify_challenge(
                &identity.public_key(),
                &new_nonce,
                fingerprint,
                1,
                &captured
            )
            .is_err(),
            "a signature over an old nonce must not verify against a fresh one"
        );
    }

    #[test]
    fn a_signature_for_another_server_is_rejected() {
        // The MITM relay case: an attacker runs a server, captures a real
        // handshake, and tries to present it to the server the player meant to
        // join. Binding the cert fingerprint is what stops it.
        let identity = Identity::generate().expect("generate");
        let nonce = generate_nonce().expect("nonce");
        let signature = identity.sign(&challenge_payload(&nonce, b"attacker-server", 1));

        assert!(
            verify_challenge(
                &identity.public_key(),
                &nonce,
                b"honest-server",
                1,
                &signature
            )
            .is_err(),
            "a signature bound to one server must not verify on another"
        );
    }

    #[test]
    fn a_signature_from_another_protocol_version_is_rejected() {
        let identity = Identity::generate().expect("generate");
        let nonce = generate_nonce().expect("nonce");
        let signature = identity.sign(&challenge_payload(&nonce, b"fp", 1));
        assert!(verify_challenge(&identity.public_key(), &nonce, b"fp", 2, &signature).is_err());
    }

    #[test]
    fn another_players_key_cannot_verify_your_challenge() {
        let alice = Identity::generate().expect("generate");
        let mallory = Identity::generate().expect("generate");
        let nonce = generate_nonce().expect("nonce");
        let signature = mallory.sign(&challenge_payload(&nonce, b"fp", 1));

        assert!(verify_challenge(&alice.public_key(), &nonce, b"fp", 1, &signature).is_err());
    }

    #[test]
    fn malformed_wire_keys_and_signatures_are_errors_not_panics() {
        // Both are reachable straight from the network.
        for length in [0usize, 1, 31, 33, 64] {
            assert!(public_key_from_bytes(&vec![0u8; length]).is_err());
        }
        // All-zero 32 bytes DOES decode as a point — it is small-order, not
        // malformed. It is refused for that reason instead.
        assert!(
            public_key_from_bytes(&[0u8; 32]).is_err(),
            "a small-order key must be refused"
        );
        for length in [0usize, 1, 63, 65] {
            assert!(signature_from_bytes(&vec![0u8; length]).is_err());
        }
    }

    #[test]
    fn a_key_file_round_trips_and_is_owner_only() {
        let dir = std::env::temp_dir().join("tiamot-identity-test");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("identity.key");

        let created = Identity::load_or_create(&path).expect("create");
        let loaded = Identity::load_or_create(&path).expect("load");
        assert_eq!(
            created.public_key(),
            loaded.public_key(),
            "loading must give back the same identity"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the key file must be owner-only");
        }
    }

    #[test]
    fn a_truncated_key_file_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join("tiamot-identity-truncated");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("identity.key");
        std::fs::write(&path, [1u8; 16]).expect("write");

        assert!(matches!(
            Identity::load_or_create(&path),
            Err(IdentityError::KeyFileSize { found: 16, .. })
        ));
    }

    #[test]
    fn an_open_allowlist_permits_everyone() {
        let allowlist = Allowlist::open();
        assert!(!allowlist.is_enabled());
        assert!(allowlist.permits(&Identity::generate().expect("generate").uuid_as_root()));
    }

    #[test]
    fn a_restricted_allowlist_permits_only_its_entries() {
        let allowed = Identity::generate().expect("generate").uuid_as_root();
        let stranger = Identity::generate().expect("generate").uuid_as_root();
        let mut allowlist = Allowlist::restricted([allowed]);

        assert!(allowlist.permits(&allowed));
        assert!(!allowlist.permits(&stranger), "a stranger must be refused");

        allowlist.allow(stranger);
        assert!(allowlist.permits(&stranger));
        allowlist.revoke(&stranger);
        assert!(!allowlist.permits(&stranger));

        // Disabling enforcement opens it without losing the list.
        allowlist.set_enabled(false);
        assert!(allowlist.permits(&stranger));
        assert_eq!(allowlist.entries(), vec![allowed]);
    }
}
