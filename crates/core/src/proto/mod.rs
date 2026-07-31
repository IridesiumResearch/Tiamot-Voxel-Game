// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The wire protocol.
//!
//! # Every inbound byte is hostile
//!
//! Charter rule 14's posture starts here. Not because most peers are hostile,
//! but because the decoder cannot tell which are, and "we only talk to our own
//! client" stops being true the moment someone writes a bot.
//!
//! Concretely, and enforced by [`decode`]:
//!
//! - **length caps before allocation** — a declared size is a claim, not a fact,
//!   and allocating on it is how a four-byte message becomes four gigabytes;
//! - **decode failures are per-connection disconnects, never panics** — a
//!   malformed message ends one session and nothing else;
//! - **no trailing data** — a message that decodes but leaves bytes over is
//!   malformed, and accepting it invites parser-differential attacks.
//!
//! # postcard variants are position-encoded
//!
//! The same sharp edge as the chunk format, and worse here because both ends
//! must agree. postcard writes an enum variant as its ordinal. **Insert a
//! variant anywhere but the end and every existing peer reinterprets every later
//! message as a different one** — no error, no checksum, just wrong messages.
//!
//! The rules, for every type reachable from [`ClientMessage`] or
//! [`ServerMessage`]:
//!
//! 1. **New variants go at the end. Always.**
//! 2. **Never remove or reorder a variant.** Deprecate in place.
//! 3. Any change bumps [`PROTOCOL_VERSION`].
//!
//! The version is exchanged in the first message of every connection precisely
//! so a mismatch is a clean rejection rather than a mysterious decode error.

use serde::{Deserialize, Serialize};

use crate::coords::{BlockPos, ChunkPos, SubNodePos};

/// The wire protocol version.
///
/// **Bump on any change to a message type.** Peers exchange this before
/// anything else and refuse each other cleanly on mismatch — see
/// [`ServerMessage::Disconnect`].
pub const PROTOCOL_VERSION: u32 = 2;
// v2 (Task 07): appended `ServerMessage::InventoryUpdate`. Appended, never
// inserted — see the module docs and CONTRIBUTING's protocol checklist.

/// Largest inbound message the decoder will consider, in bytes.
///
/// Checked **before** allocating. Chunk data is the largest legitimate message
/// and Task 03 measured a heavily chiselled chunk at 937 bytes compressed, so
/// 1 MiB is generous by three orders of magnitude while still bounding what a
/// peer can make the server hold.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Largest content-transfer chunk a peer may declare.
pub const MAX_CONTENT_CHUNK_BYTES: usize = 256 * 1024;

/// Longest display name.
pub const MAX_NAME_BYTES: usize = 32;

/// Longest chat message.
pub const MAX_CHAT_BYTES: usize = 512;

/// A `BLAKE3` content hash.
pub type ContentHash = [u8; 32];

/// An Ed25519 signature on the wire.
///
/// A newtype because serde has no built-in impl for `[u8; 64]`, and because the
/// fixed size is a security property worth keeping in the type: a
/// variable-length signature field would let a peer declare a length, and a
/// declared length is a claim. The deserialiser enforces exactly 64 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WireSignature(pub [u8; 64]);

impl std::fmt::Debug for WireSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WireSignature(..)")
    }
}

impl Serialize for WireSignature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for WireSignature {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = WireSignature;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a 64-byte Ed25519 signature")
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<Self::Value, E> {
                let array: [u8; 64] = bytes
                    .try_into()
                    .map_err(|_| E::invalid_length(bytes.len(), &self))?;
                Ok(WireSignature(array))
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

/// Why a connection was closed.
///
/// Carried in [`ServerMessage::Disconnect`] so a client can say something
/// useful rather than "connection lost".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisconnectReason {
    /// Protocol versions do not match.
    VersionMismatch {
        /// What the server speaks.
        server: u32,
        /// What the client claimed.
        client: u32,
    },
    /// Identity verification failed.
    AuthFailed {
        /// Human-readable detail. Deliberately vague about *why*, so it is not
        /// an oracle for probing which keys or names a server knows.
        detail: String,
    },
    /// The claimed display name belongs to another identity.
    NameTaken {
        /// The contested name.
        name: String,
    },
    /// The server is enforcing an allowlist and this identity is not on it.
    NotAllowlisted,
    /// The server is full.
    ServerFull {
        /// Configured maximum.
        max_players: u32,
    },
    /// A message could not be decoded, or broke a limit.
    ProtocolError {
        /// What went wrong.
        detail: String,
    },
    /// An operator kicked this player.
    Kicked {
        /// Reason given.
        reason: String,
    },
    /// The server is shutting down.
    ServerStopping,
}

/// A block or sub-node edit.
///
/// Sub-node edits ride the same message as block edits rather than getting a
/// separate opcode. Task 02b measured a minute of continuous chiselling at
/// 2.79 KiB compressed against a 32 KiB/min budget, so a dedicated compact
/// encoding buys nothing that compression does not already give — recorded in
/// the Sub-Node Contract §10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edit {
    /// Replace a whole block.
    Block {
        /// Where.
        pos: BlockPos,
        /// The new material's numeric id.
        material: u16,
    },
    /// Replace one sub-node cell.
    SubNode {
        /// Where.
        pos: SubNodePos,
        /// The new material's numeric id.
        material: u16,
    },
}

/// One mod in the server's resolved set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModEntry {
    /// The mod id.
    pub id: String,
    /// Its version.
    pub version: String,
    /// `BLAKE3` of its client-relevant files, for content addressing.
    pub content_hash: ContentHash,
}

/// Messages a client sends.
///
/// **APPEND ONLY.** See the module docs.
///
/// `PartialEq` but not `Eq`: [`ClientMessage::PlayerInput`] carries `f32`
/// fields, and float equality is not an equivalence relation. That is also why
/// `validate_client_message` rejects non-finite inputs before they can reach
/// anything that compares them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message on a connection.
    Hello {
        /// The client's protocol version.
        protocol_version: u32,
        /// The public key it claims to hold.
        public_key: [u8; 32],
        /// The display name it wants.
        display_name: String,
    },
    /// Response to [`ServerMessage::AuthChallenge`].
    ///
    /// The signature covers `nonce ‖ cert fingerprint ‖ protocol version` — see
    /// [`crate::identity::challenge_payload`] for why all three.
    AuthResponse {
        /// Signature over the challenge payload.
        signature: WireSignature,
    },
    /// Asks for content the client does not have cached.
    ContentRequest {
        /// Hashes it is missing.
        hashes: Vec<ContentHash>,
    },
    /// The client is ready to enter the world.
    JoinWorld,
    /// Movement and action input for one tick.
    PlayerInput {
        /// The tick this input is for.
        tick: u64,
        /// Movement intent, in sub-node units per tick.
        movement: [f32; 3],
        /// Yaw and pitch, in turns rather than radians so no trigonometry is
        /// needed to transmit them.
        look: [f32; 2],
        /// Bitfield of named actions currently held.
        actions: u32,
    },
    /// A block or sub-node edit.
    BlockDelta {
        /// The edit.
        edit: Edit,
    },
    /// A chat message.
    Chat {
        /// The text.
        text: String,
    },
    /// Adds a key to this identity's set, signed by an existing key.
    AddKey {
        /// The key to authorise.
        new_public_key: [u8; 32],
        /// Its successor commitment.
        next_key_hash: Option<[u8; 32]>,
        /// Signature by an already-authorised key.
        signature: WireSignature,
        /// Which authorised key signed.
        signer_public_key: [u8; 32],
    },
    /// Rotates a key to its pre-committed successor.
    RotateKey {
        /// The successor.
        new_public_key: [u8; 32],
        /// The successor's own commitment.
        new_next_key_hash: Option<[u8; 32]>,
        /// Signature by the key being retired.
        signature: WireSignature,
    },
    /// Client is leaving.
    Disconnect,
}

/// Messages a server sends.
///
/// **APPEND ONLY.** See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Accepts a [`ClientMessage::Hello`].
    HelloAck {
        /// The server's protocol version.
        protocol_version: u32,
        /// `BLAKE3` of the server's self-signed certificate.
        ///
        /// Surfaced to the user for trust-on-first-use, and bound into the
        /// challenge signature so a relay cannot forward a handshake.
        cert_fingerprint: [u8; 32],
    },
    /// Challenges the client to prove it holds the claimed key.
    AuthChallenge {
        /// A fresh 32-byte nonce.
        nonce: [u8; 32],
    },
    /// The server's resolved mod set.
    ModManifest {
        /// Mods, in load order.
        mods: Vec<ModEntry>,
        /// The resolved set's fingerprint, from Task 05.
        set_fingerprint: u64,
    },
    /// A slice of requested content.
    ContentChunk {
        /// Which content.
        hash: ContentHash,
        /// Byte offset of this slice.
        offset: u64,
        /// Total size of the whole item, so a client can size its buffer once.
        total_len: u64,
        /// zstd-compressed slice.
        data: Vec<u8>,
    },
    /// The client is now in the world.
    JoinWorld {
        /// The identity the server resolved.
        player_uuid: [u8; 32],
        /// Where the player is.
        spawn: BlockPos,
        /// The server's tick when this was sent.
        tick: u64,
    },
    /// A chunk's contents.
    ChunkData {
        /// Which chunk.
        pos: ChunkPos,
        /// The chunk blob, in the Task 03 format.
        blob: Vec<u8>,
    },
    /// A chunk left the client's interest set.
    ChunkUnload {
        /// Which chunk.
        pos: ChunkPos,
    },
    /// A block or sub-node changed.
    BlockDelta {
        /// The edit.
        edit: Edit,
        /// Who made it, or `None` for the engine.
        actor: Option<[u8; 32]>,
    },
    /// Entity state for one tick.
    EntityStateDelta {
        /// The tick this describes.
        tick: u64,
        /// Opaque per-entity payload; Task 12 defines the contents.
        payload: Vec<u8>,
    },
    /// A chat message.
    Chat {
        /// Who sent it, or `None` for the server.
        from: Option<[u8; 32]>,
        /// The text.
        text: String,
    },
    /// The connection is closing.
    Disconnect {
        /// Why.
        reason: DisconnectReason,
    },
    /// The player's inventory changed.
    ///
    /// **Appended at the end** (protocol v2). Inserting it above `Disconnect`
    /// would have shifted that variant's ordinal and silently reinterpreted
    /// every disconnect on every existing peer — which is exactly what
    /// `server_variant_ordinals_are_pinned` caught when I tried it.
    ///
    /// Sent whole rather than as a delta. An inventory is small — tens of
    /// stacks — and a delta stream that ever dropped a message would leave the
    /// client permanently wrong with no way to notice. Charter rule 5: amounts
    /// are in **units**, and the client displays `units / 27` blocks plus
    /// `units % 27` nodes.
    InventoryUpdate {
        /// Material id and unit count, in ascending material order.
        stacks: Vec<(u16, u32)>,
    },
}

/// A message could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The message exceeds [`MAX_MESSAGE_BYTES`].
    #[error("message is {len} bytes, over the {MAX_MESSAGE_BYTES}-byte limit")]
    TooLarge {
        /// The declared or actual length.
        len: usize,
    },

    /// The message is empty.
    #[error("message is empty")]
    Empty,

    /// The bytes are not a valid message.
    #[error("malformed message")]
    Malformed(#[source] postcard::Error),

    /// The message decoded but left bytes over.
    #[error("message decoded with {trailing} trailing bytes")]
    TrailingData {
        /// How many bytes were left.
        trailing: usize,
    },

    /// A field broke a documented limit.
    #[error("{field} is {len} bytes, over the {limit}-byte limit")]
    FieldTooLarge {
        /// Which field.
        field: &'static str,
        /// Its length.
        len: usize,
        /// The limit.
        limit: usize,
    },

    /// Encoding failed.
    #[error("could not encode message")]
    Encode(#[source] postcard::Error),
}

impl ProtocolError {
    /// The disconnect reason to send before closing.
    #[must_use]
    pub fn to_disconnect(&self) -> DisconnectReason {
        DisconnectReason::ProtocolError {
            detail: self.to_string(),
        }
    }
}

/// Encodes a message.
///
/// # Errors
///
/// [`ProtocolError::Encode`] if serialisation fails, or
/// [`ProtocolError::TooLarge`] if the result exceeds the cap — which would mean
/// the *server* is trying to send something oversized, and is a bug worth
/// catching here rather than at the peer.
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let bytes = postcard::to_allocvec(message).map_err(ProtocolError::Encode)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge { len: bytes.len() });
    }
    Ok(bytes)
}

/// Decodes a message from untrusted bytes.
///
/// **Every failure here is a per-connection disconnect, never a panic.** This
/// function is the fuzz target in `fuzz/`, and it is the boundary charter rule
/// 14 is about.
///
/// # Errors
///
/// [`ProtocolError`] for anything that is not exactly one well-formed message
/// within the limits.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtocolError> {
    // Length first, before postcard sees anything. A declared size inside the
    // payload is a claim; this is a fact about what actually arrived.
    if bytes.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge { len: bytes.len() });
    }

    let (message, rest) = postcard::take_from_bytes(bytes).map_err(ProtocolError::Malformed)?;

    // Trailing data means the sender and this decoder disagree about where the
    // message ends. Accepting it is how parser-differential bugs start.
    if !rest.is_empty() {
        return Err(ProtocolError::TrailingData {
            trailing: rest.len(),
        });
    }

    Ok(message)
}

/// Checks a decoded client message against the documented field limits.
///
/// Separate from [`decode`] because postcard cannot express these, and because
/// a length that is legal to *decode* may still be unreasonable to *act on* — a
/// 900 KiB display name decodes fine and is obviously an attack.
///
/// # Errors
///
/// [`ProtocolError::FieldTooLarge`] naming the offending field.
pub fn validate_client_message(message: &ClientMessage) -> Result<(), ProtocolError> {
    match message {
        ClientMessage::Hello { display_name, .. } => {
            check_len("display_name", display_name.len(), MAX_NAME_BYTES)?;
            if display_name.is_empty() {
                return Err(ProtocolError::FieldTooLarge {
                    field: "display_name",
                    len: 0,
                    limit: MAX_NAME_BYTES,
                });
            }
        }
        ClientMessage::Chat { text } => check_len("chat", text.len(), MAX_CHAT_BYTES)?,
        ClientMessage::ContentRequest { hashes } => {
            // A request list is cheap to send and expensive to serve. Bound it.
            check_len("content_request", hashes.len(), 1024)?;
        }
        ClientMessage::PlayerInput { movement, look, .. } => {
            // NaN in an input would propagate into simulation state, and NaN
            // payloads are not specified across platforms (charter rule 4).
            // A client sending one is broken or hostile; either way it must not
            // reach the tick.
            for value in movement.iter().chain(look.iter()) {
                if !value.is_finite() {
                    return Err(ProtocolError::FieldTooLarge {
                        field: "player_input",
                        len: 0,
                        limit: 0,
                    });
                }
            }
        }
        ClientMessage::AuthResponse { .. }
        | ClientMessage::JoinWorld
        | ClientMessage::BlockDelta { .. }
        | ClientMessage::AddKey { .. }
        | ClientMessage::RotateKey { .. }
        | ClientMessage::Disconnect => {}
    }
    Ok(())
}

fn check_len(field: &'static str, len: usize, limit: usize) -> Result<(), ProtocolError> {
    if len > limit {
        return Err(ProtocolError::FieldTooLarge { field, len, limit });
    }
    Ok(())
}

/// Whether a peer's protocol version is compatible with this build.
#[must_use]
pub const fn version_compatible(peer: u32) -> bool {
    // Exact match for now. When the protocol gains a compatibility window, this
    // is the one place that changes.
    peer == PROTOCOL_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: [1u8; 32],
            display_name: "Bob".to_owned(),
        }
    }

    #[test]
    fn a_message_round_trips() {
        let bytes = encode(&hello()).expect("encode");
        let decoded: ClientMessage = decode(&bytes).expect("decode");
        assert_eq!(decoded, hello());
    }

    #[test]
    fn every_server_message_round_trips() {
        let messages = [
            ServerMessage::HelloAck {
                protocol_version: PROTOCOL_VERSION,
                cert_fingerprint: [2u8; 32],
            },
            ServerMessage::AuthChallenge { nonce: [3u8; 32] },
            ServerMessage::ModManifest {
                mods: vec![ModEntry {
                    id: "core".to_owned(),
                    version: "0.1.0".to_owned(),
                    content_hash: [4u8; 32],
                }],
                set_fingerprint: 99,
            },
            ServerMessage::ChunkData {
                pos: ChunkPos::new(1, -2, 3),
                blob: vec![5, 6, 7],
            },
            ServerMessage::Disconnect {
                reason: DisconnectReason::VersionMismatch {
                    server: 1,
                    client: 2,
                },
            },
        ];
        for message in messages {
            let bytes = encode(&message).expect("encode");
            let decoded: ServerMessage = decode(&bytes).expect("decode");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn an_empty_message_is_an_error() {
        assert!(matches!(
            decode::<ClientMessage>(&[]),
            Err(ProtocolError::Empty)
        ));
    }

    #[test]
    fn an_oversized_message_is_refused_before_decoding() {
        // The cap is checked on what ARRIVED, not on what the payload claims,
        // so this cannot be talked around.
        let huge = vec![0u8; MAX_MESSAGE_BYTES + 1];
        assert!(matches!(
            decode::<ClientMessage>(&huge),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn trailing_data_is_refused() {
        // A message that decodes but leaves bytes over means the sender and the
        // decoder disagree about framing — the start of a parser-differential
        // bug.
        let mut bytes = encode(&hello()).expect("encode");
        bytes.push(0xFF);
        assert!(matches!(
            decode::<ClientMessage>(&bytes),
            Err(ProtocolError::TrailingData { trailing: 1 })
        ));
    }

    #[test]
    fn garbage_never_panics() {
        // The core of the hostile-input posture. Exhaustively over short
        // inputs, since those hit the framing edges hardest.
        for length in 0..48 {
            for seed in 0..64u8 {
                let bytes: Vec<u8> = (0..length).map(|i| seed.wrapping_add(i as u8)).collect();
                let _ = decode::<ClientMessage>(&bytes);
                let _ = decode::<ServerMessage>(&bytes);
            }
        }
    }

    #[test]
    fn truncating_a_valid_message_never_panics() {
        let bytes = encode(&hello()).expect("encode");
        for cut in 0..bytes.len() {
            let _ = decode::<ClientMessage>(&bytes[..cut]);
        }
    }

    #[test]
    fn corrupting_a_valid_message_never_panics() {
        let bytes = encode(&hello()).expect("encode");
        for index in 0..bytes.len() {
            for flip in [0x01u8, 0x80, 0xFF] {
                let mut corrupted = bytes.clone();
                corrupted[index] ^= flip;
                let _ = decode::<ClientMessage>(&corrupted);
            }
        }
    }

    #[test]
    fn an_oversized_display_name_is_rejected() {
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: [0u8; 32],
            display_name: "x".repeat(MAX_NAME_BYTES + 1),
        };
        assert!(matches!(
            validate_client_message(&message),
            Err(ProtocolError::FieldTooLarge {
                field: "display_name",
                ..
            })
        ));
    }

    #[test]
    fn an_empty_display_name_is_rejected() {
        let message = ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: [0u8; 32],
            display_name: String::new(),
        };
        assert!(validate_client_message(&message).is_err());
    }

    #[test]
    fn a_non_finite_player_input_is_rejected() {
        // NaN payloads are not specified across platforms (charter rule 4), so
        // one reaching simulation state would break the determinism gate for
        // everyone on the server.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let message = ClientMessage::PlayerInput {
                tick: 1,
                movement: [bad, 0.0, 0.0],
                look: [0.0, 0.0],
                actions: 0,
            };
            assert!(
                validate_client_message(&message).is_err(),
                "{bad} must not reach the tick"
            );
        }

        let good = ClientMessage::PlayerInput {
            tick: 1,
            movement: [1.0, -2.0, 0.5],
            look: [0.25, 0.5],
            actions: 3,
        };
        assert!(validate_client_message(&good).is_ok());
    }

    #[test]
    fn an_oversized_chat_message_is_rejected() {
        let message = ClientMessage::Chat {
            text: "x".repeat(MAX_CHAT_BYTES + 1),
        };
        assert!(validate_client_message(&message).is_err());
    }

    #[test]
    fn version_compatibility_is_exact_for_now() {
        assert!(version_compatible(PROTOCOL_VERSION));
        assert!(!version_compatible(PROTOCOL_VERSION + 1));
        assert!(!version_compatible(0));
    }

    #[test]
    fn a_protocol_error_becomes_a_disconnect_reason() {
        let err = ProtocolError::TooLarge { len: 99 };
        assert!(matches!(
            err.to_disconnect(),
            DisconnectReason::ProtocolError { .. }
        ));
    }

    #[test]
    fn client_variant_ordinals_are_pinned() {
        // postcard encodes a variant as its ordinal, so INSERTING a variant
        // anywhere but the end silently reinterprets every later message on
        // every existing peer — no error, no checksum, just wrong messages.
        //
        // Every ordinal is pinned, not just the ends: pinning only the first
        // and last would still let two variants swap places in between. If this
        // test fails after an edit, something was inserted or reordered rather
        // than appended, and PROTOCOL_VERSION must be bumped.
        let signature = WireSignature([0u8; 64]);
        let client: [(ClientMessage, u8); 10] = [
            (
                ClientMessage::Hello {
                    protocol_version: 0,
                    public_key: [0u8; 32],
                    display_name: String::new(),
                },
                0,
            ),
            (ClientMessage::AuthResponse { signature }, 1),
            (ClientMessage::ContentRequest { hashes: Vec::new() }, 2),
            (ClientMessage::JoinWorld, 3),
            (
                ClientMessage::PlayerInput {
                    tick: 0,
                    movement: [0.0; 3],
                    look: [0.0; 2],
                    actions: 0,
                },
                4,
            ),
            (
                ClientMessage::BlockDelta {
                    edit: Edit::Block {
                        pos: BlockPos::new(0, 0, 0),
                        material: 0,
                    },
                },
                5,
            ),
            (
                ClientMessage::Chat {
                    text: String::new(),
                },
                6,
            ),
            (
                ClientMessage::AddKey {
                    new_public_key: [0u8; 32],
                    next_key_hash: None,
                    signature,
                    signer_public_key: [0u8; 32],
                },
                7,
            ),
            (
                ClientMessage::RotateKey {
                    new_public_key: [0u8; 32],
                    new_next_key_hash: None,
                    signature,
                },
                8,
            ),
            (ClientMessage::Disconnect, 9),
        ];

        for (message, expected) in client {
            let bytes = encode(&message).expect("encode");
            assert_eq!(
                bytes[0], expected,
                "{message:?} should be ordinal {expected}; a variant was inserted or reordered"
            );
        }
    }

    #[test]
    fn server_variant_ordinals_are_pinned() {
        let server: [(ServerMessage, u8); 10] = [
            (
                ServerMessage::HelloAck {
                    protocol_version: 0,
                    cert_fingerprint: [0u8; 32],
                },
                0,
            ),
            (ServerMessage::AuthChallenge { nonce: [0u8; 32] }, 1),
            (
                ServerMessage::ModManifest {
                    mods: Vec::new(),
                    set_fingerprint: 0,
                },
                2,
            ),
            (
                ServerMessage::ContentChunk {
                    hash: [0u8; 32],
                    offset: 0,
                    total_len: 0,
                    data: Vec::new(),
                },
                3,
            ),
            (
                ServerMessage::JoinWorld {
                    player_uuid: [0u8; 32],
                    spawn: BlockPos::new(0, 0, 0),
                    tick: 0,
                },
                4,
            ),
            (
                ServerMessage::ChunkData {
                    pos: ChunkPos::new(0, 0, 0),
                    blob: Vec::new(),
                },
                5,
            ),
            (
                ServerMessage::ChunkUnload {
                    pos: ChunkPos::new(0, 0, 0),
                },
                6,
            ),
            (
                ServerMessage::BlockDelta {
                    edit: Edit::Block {
                        pos: BlockPos::new(0, 0, 0),
                        material: 0,
                    },
                    actor: None,
                },
                7,
            ),
            (
                ServerMessage::EntityStateDelta {
                    tick: 0,
                    payload: Vec::new(),
                },
                8,
            ),
            (
                ServerMessage::Chat {
                    from: None,
                    text: String::new(),
                },
                9,
            ),
        ];

        for (message, expected) in server {
            let bytes = encode(&message).expect("encode");
            assert_eq!(
                bytes[0], expected,
                "{message:?} should be ordinal {expected}; a variant was inserted or reordered"
            );
        }

        // Disconnect's ordinal is pinned separately because it is the one an
        // appended variant is most likely to displace: it reads like the
        // natural end of the enum, so a new variant gets written above it.
        // Doing exactly that is what this caught during the protocol v2 change.
        let disconnect = encode(&ServerMessage::Disconnect {
            reason: DisconnectReason::ServerStopping,
        })
        .expect("encode");
        assert_eq!(
            disconnect[0], 10,
            "Disconnect must stay at ordinal 10; a variant was inserted above it"
        );

        // Protocol v2, appended after Disconnect.
        let inventory =
            encode(&ServerMessage::InventoryUpdate { stacks: Vec::new() }).expect("encode");
        assert_eq!(inventory[0], 11);
    }

    #[test]
    fn a_signature_field_is_exactly_sixty_four_bytes() {
        // A variable-length signature would let a peer declare a length, and a
        // declared length is a claim. The newtype makes the size structural.
        let message = ClientMessage::AuthResponse {
            signature: WireSignature([7u8; 64]),
        };
        let bytes = encode(&message).expect("encode");
        let decoded: ClientMessage = decode(&bytes).expect("decode");
        assert_eq!(decoded, message);

        // A short signature must not decode.
        let mut truncated = bytes.clone();
        truncated.truncate(bytes.len() - 1);
        assert!(decode::<ClientMessage>(&truncated).is_err());
    }
}
