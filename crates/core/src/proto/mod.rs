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
pub const PROTOCOL_VERSION: u32 = 34;
// v2 (Task 07): appended `ServerMessage::InventoryUpdate`. Appended, never
// inserted — see the module docs and CONTRIBUTING's protocol checklist.
// v3 (Task 08): appended `ServerMessage::MaterialTable`.
// v4 (Task 09): appended `ServerMessage::PlayerState`, and `ServerMessage`
// stopped deriving `Eq` because that variant carries `f32` fields.
// v5 (Task 09): appended `ClientMessage::{StartDig, CancelDig, SelectTool}` and
// `ServerMessage::DigProgress`. `ClientMessage` was already `PartialEq`-only.
// v6 (Task 09): appended `ClientMessage::Place` and `Edit::Partial`. `Edit` is
// nested inside `BlockDelta` on both sides, so appending a variant to it
// changes what both messages can carry without moving either of them.
// v7 (Task 09): appended `ServerMessage::ToolTable`.
// v8 (Task 10): appended `ServerMessage::ChunkLight`. Light is a separate
// message rather than a field on `ChunkData` for two reasons: a chunk's light
// changes without its blocks changing — a lamp placed next door — so the two
// have different lifetimes on the wire, and appending a variant is the only
// shape of protocol change this format makes safe.
// v9 (Task 10): appended `ServerMessage::{SkyTable, TimeOfDay}`. The sky is sent
// once on join because it is registration data; the time is sent repeatedly
// because it is state. Two messages rather than one for exactly that reason —
// re-sending a mod's whole keyframe list twenty times a second to carry one
// float would be absurd.
// v11 (Task 11): appended `ServerMessage::{ChunkFluid, FluidTable}`.
// v12 (Task 11): appended `ClientMessage::ViewDistance` and
// `ServerMessage::ViewDistance`. How far a player sees is a bargain between two
// machines — the client knows what its GPU can take, the server knows what it
// can afford to send fifty of — so the client asks and the server answers with
// what it is willing to send. Two messages rather than one because the granted
// value is not the requested one, and a client drawing its fog for a radius the
// server refused would end the world in clear air.
// v14 (Task 12): appended `ServerMessage::{EntitySpawn, EntityDespawn,
// EntityState}`. Three messages rather than one because they have three
// lifetimes: a spawn and a despawn must arrive or the client holds an id it
// cannot draw or a mob that never leaves, while a state update is superseded
// 50 ms later and is better dropped than retransmitted behind a stalled
// stream. They are also PER PLAYER rather than broadcast — which entities
// somebody can see is their own interest set, and sending everyone every mob
// would make a populated world cost the square of the people watching it.
// v30 (post-14): appended `ClientMessage::SelectSlot`. Which slot a player is
// holding was known only to the client — the hotbar keys are handled locally
// and nothing but a `Place` ever told the server what was in hand. That is
// enough for building and not enough for anything a mod wants to do with an
// ITEM: a sword that swings, a torch that lights, a tool that digs faster. The
// server has to know what is held before a mod can ask.
// v29 (post-14): `MaterialDef` carries `placeable`. Everything a player can
// carry is one id in one table — a stack is a material, a quantity and a cut —
// and an ITEM is a material that may not be put in the world. One flag rather
// than a second id space, because the distinction matters in exactly one place
// and a `kind` byte would put a `match` in all the others. The client needs it
// to stop offering a placement that the server would only refuse.
// v28 (post-14): `EntityDef` carries `item`. An entity can BE a stack — one
// lying on the ground — and the client draws it as the same cells a hand holds
// and a slot shows. On the SPAWN and not in the delta: what an item is never
// changes while it lies there, so putting it in the twenty-times-a-second
// message would be paying for it on every one of them.
// v27 (post-14): `ShowDialog` and `UpdateDialog` carry `compact`. A FIELD
// change on two messages, the same shape of change v24 made, and it moves the
// version for the same reason: a v26 client decoding a v27 dialog would read
// the bool as part of the next field and get a tree that is not one.
// **Why the flag exists at all.** A dialog is drawn in the same centred four by
// three sheet as every other screen, because a player reads them as one system
// and a screen that resized as they switched tabs read as the window moving
// under them. A yes/no prompt does not want a whole sheet, and the engine
// cannot tell a prompt from an inventory by looking at it — so the mod says.
// v26 (post-14): appended `ClientMessage::SwapOffhand`, and `player:main` grew
// a twenty-eighth slot for the off-hand to live in. The slot count is not on
// the wire — a view's contents are sent as a list and the client reads its
// length — so the growth needs no variant of its own; the message does.
// v25 (post-14): appended `Widget::ShapeEditor` and `DialogEvent::Chiselled`.
// Both APPENDED at the end of their enums, which this format makes safe — but
// the version still moves, because a v24 client handed a tree holding a variant
// it has never heard of cannot draw it, and "cannot draw it" for a widget in
// the middle of a crafting screen is a screen with a hole in it.
// v24 (post-14): `InventoryUpdate`, `ViewUpdate` and `ClientMessage::Place`
// carry a SHAPE. `(u16, u32)` became `StackDef`, which is a FIELD CHANGE on
// existing messages rather than an appended variant — the one shape of change
// this format does not make safe, exactly as v10 and v13 were. The version
// check is what keeps a v23 peer from reading these bytes as the old pair.
// v13 (Task 11): `FluidDef` grew a `color`, for the tint over a submerged
// camera. **A field on an existing struct, not an appended variant** — the one
// shape of change this format does not make safe, exactly as v10 was, so the
// version check is what keeps a v12 client from reading the next fluid's bytes
// as this one's colour.
// v10 (Task 10): `SkyFrame` grew a `grade`. **A field on an existing struct, not
// an appended variant** — the one shape of change this format does not make
// safe, because postcard is not self-describing and an old client would read the
// next keyframe's bytes as this one's grade. That is what the version check is
// for: v9 and v10 refuse each other before either reads a keyframe.

// v34 (post-14): `ClientMessage::Place` carries the FACE the placement was
// made against, so the server can orient a cut stack — front toward the
// player, or toward their feet against a wall. **A field on an existing
// message**, which is the one shape of change this format does not make safe,
// exactly as v24 and v13 were: postcard is not self-describing, so a v33 peer
// would read the byte after the shape as the start of the next message. The
// version check is what keeps the two apart.

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

/// Longest canonical string id a message may carry.
///
/// A mod id and a name, plus punctuation. Ids come from manifests, so this is
/// generous rather than tight — the point is that a hostile server cannot make
/// a client allocate an unbounded string, not that any real id approaches it.
pub const MAX_ID_BYTES: usize = 128;

/// Most entities one message may describe.
///
/// A player's interest cylinder at the largest view distance holds about 1,800
/// chunks, and a world with an entity in every one of them is already past
/// anything the tick budget allows. The cap is what stops a hostile server
/// making a client allocate for a herd that does not exist — charter rule 14 —
/// and the server splits legitimately larger sets across messages.
pub const MAX_ENTITIES_PER_MESSAGE: usize = 4096;

/// Most actions a server may declare in one [`ServerMessage::ActionTable`].
///
/// Charter rule 14: a server is not trusted. Each entry costs the client a row
/// in the settings screen and a string it holds for the session, so the cap is
/// generous for any real mod set and finite against one that is not. A hundred
/// mods with ten controls each fits.
pub const MAX_ACTIONS: usize = 1024;

/// Most sounds a server may declare in one [`ServerMessage::SoundTable`].
///
/// Each entry costs the client a decode job and a buffer it holds for the
/// session, so this is generous for any real mod set and finite against one
/// that is not.
pub const MAX_SOUNDS: usize = 1024;

/// Most HUD scripts a server may push in one [`ServerMessage::HudScripts`].
///
/// Far tighter than the other tables, and deliberately so: every entry here is
/// CODE that will run on the player's machine sixty times a second. A mod set
/// needing more than a handful of HUD scripts is describing something other
/// than a HUD, and the client's own [`crate::script::HudLimits::scripts`]
/// refuses past its own smaller number anyway — this is the bound that stops a
/// hostile server making a client allocate the list at all.
pub const MAX_HUD_SCRIPTS: usize = 32;

/// Most cue bindings a server may declare in one
/// [`ServerMessage::SoundBindings`].
///
/// One per event a mod wants a noise on. Generous against any real mod set and
/// finite against a server that would otherwise make a client allocate an
/// unbounded table before it has drawn a frame.
pub const MAX_SOUND_BINDINGS: usize = 4096;

/// A `BLAKE3` content hash.
pub type ContentHash = [u8; 32];

/// Bits in [`ClientMessage::PlayerInput`]'s `actions` field.
///
/// A bitfield rather than separate booleans because it is sent 20 times a
/// second per player and will grow as mods register actions (charter rule 11 —
/// mods name actions, the engine owns the keys).
///
/// **Append only, like the message variants.** These numbers are on the wire,
/// so renumbering one silently turns every peer's sneak into a jump.
pub mod actions {
    /// Leave the ground, honoured only when standing on something.
    pub const JUMP: u32 = 1 << 0;
    /// Move at the sprint speed.
    pub const SPRINT: u32 = 1 << 1;
    /// Move slowly, and refuse to walk off an edge.
    ///
    /// Takes precedence over [`SPRINT`] when a client sends both: the guard
    /// against falling is the safer answer to a contradiction, and a client
    /// sending both is buggy rather than expressing a preference.
    pub const SNEAK: u32 = 1 << 2;
    /// Leave gravity behind, honoured only for a player the server allows it.
    ///
    /// **Asking is not being allowed.** Every client can set this bit; the
    /// server ignores it for anybody who is not an operator, exactly as it
    /// ignores a placement into occupied space. Charter rule 2: the client
    /// says what it wants and the server decides what happens.
    pub const FLY: u32 = 1 << 3;
}

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

    /// Replace a whole block with a partially-filled one.
    ///
    /// **Appended at the end** (protocol v6). [`Edit`] is a postcard enum and
    /// postcard encodes variants positionally, so a variant inserted anywhere
    /// else silently reinterprets every message a differently-built peer sends.
    ///
    /// One edit rather than 27 [`Edit::SubNode`]s. Placing spare nodes is a
    /// single player action and it produces a single block, so sending it as
    /// two dozen independent cell writes would put a partially-built block on
    /// screen for as long as the burst took to arrive, and would cost 27 times
    /// the bytes to say the same thing.
    Partial {
        /// Which block.
        pos: BlockPos,
        /// The material filling the occupied cells.
        material: u16,
        /// Which of the 27 cells are filled, indexed by
        /// [`crate::block::subnode_index`].
        ///
        /// Bits at or above [`crate::UNITS_PER_BLOCK`] are invalid
        /// and rejected by [`validate_client_message`]; they cannot address a
        /// cell, so a peer setting one is broken or probing.
        occupancy: u32,
    },
}

/// One tool a mod registered, as the client needs to see it.
///
/// # Why the client is told this at all
///
/// Charter rule 1: the engine has no tools of its own, not even a bare hand —
/// a mod says what a player digs with. So a client cannot offer a way to
/// *choose* one without being told what exists, and hard-coding `core_tools:
/// chisel` into the client would put mod content in the engine, which is the
/// one thing the charter's first rule forbids.
///
/// This is the tool equivalent of [`MaterialDef`], and it arrives the same way
/// and for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDef {
    /// The qualified id, e.g. `"core_tools:chisel"`.
    pub id: String,
    /// What to show a player. Empty when the mod did not say, and the client
    /// falls back to the id.
    pub name: String,
    /// What shape it removes: `"block"` or `"subnode"`.
    ///
    /// A string rather than an enum because [`crate::dig::Brush`] is a
    /// simulation type and the task's own wording calls the brush format
    /// "extensible", so mods will grow shapes this enum does not have. A client
    /// showing an unfamiliar brush verbatim is right; one that could not decode
    /// the message at all is not.
    pub brush: String,
    /// Whether this is what a player digs with holding nothing.
    pub default: bool,
}

/// A named action a mod registered, as the client needs to see it.
///
/// # Why the client is told this at all
///
/// Charter rule 11: a mod registers a NAME and the engine owns the key. The
/// client is the thing that owns keys, so the names have to reach it — and with
/// them the mod that asked, because a settings screen that cannot say who wants
/// a binding is a list of ids a player has no way to judge.
///
/// The default is a STRING and not a key type. `crates/core` must never depend
/// on winit (charter rule 3), so the engine carries the name winit would use —
/// `"KeyW"`, `"Space"` — and the client turns it into a `KeyCode`. A default it
/// cannot parse is dropped rather than refused: an action nobody can trigger
/// until it is bound is a worse outcome than a join that fails, but only just,
/// and it is the one that leaves the player able to fix it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActionDef {
    /// The qualified id, e.g. `"core_tools:chisel_mode"`.
    pub id: String,
    /// One line for the settings screen. Empty when the mod did not say.
    pub description: String,
    /// The mod that registered it, for attribution in the UI.
    pub mod_id: String,
    /// The binding the mod suggests, as a winit `KeyCode` name.
    ///
    /// Empty means the mod shipped it unbound, which is legitimate: the player
    /// binds it or it does nothing.
    pub default_key: String,
}

/// A sound a mod registered, as the client needs to see it.
///
/// The file travels through the content pipeline like a texture — by hash, on
/// request — so this carries the hash rather than the bytes. **It is hostile
/// input** (charter rule 14): a client decodes audio from servers it does not
/// trust, and the decoder bounds everything before it allocates.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SoundDef {
    /// The qualified id, e.g. `"core_tools:break"`.
    pub id: String,
    /// The mod that registered it, for attribution in the UI.
    ///
    /// Carried rather than split back out of `id`, so the settings screen
    /// attributes a sound the same way it attributes a binding: from what the
    /// server said, not from a client-side guess about a namespace.
    pub mod_id: String,
    /// The content hash of the audio file, or `None` if the mod named one that
    /// is not in its directory — the client plays nothing rather than guessing.
    pub file: Option<ContentHash>,
    /// How loud, as a multiplier on the file's own level.
    pub gain: f32,
    /// How much to vary the pitch per play, as a fraction.
    pub pitch_variance: f32,
}

/// A quantity of one material, as the wire carries it.
///
/// # Why this is a struct and not a tuple any more
///
/// It was `(u16, u32)` — material and units — in two messages. A stack can now
/// be cut to a SHAPE, and a pair of numbers with a third meaning bolted on is
/// how a wire format becomes unreadable. Named fields also make the zero case
/// say what it means: `shape: 0` is loose material, because an empty occupancy
/// mask is not a shape a player can hold.
///
/// **A field change on an existing message, not an appended variant** — the one
/// shape of change postcard does not make safe, exactly as v10 and v13 were. The
/// version check is what keeps a v23 client from reading these bytes as the old
/// pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StackDef {
    /// World material id.
    pub material: u16,
    /// How much, in units. 27 units is one whole block.
    pub units: u32,
    /// The 27-bit occupancy each item is cut to, or `0` for loose material.
    pub shape: u32,
}

/// A sound bound to a named event.
///
/// **The client needs this, not just the server.** Most cues are resolved
/// server-side and arrive as an ordinary [`ServerMessage::PlaySound`] — but the
/// handful the engine emits are the player's OWN actions, and those must not
/// wait for a round trip. A jump that thuds 80 ms late does not read as
/// latency, it reads as a worse sound. So the client is told the whole table
/// and resolves those itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SoundBinding {
    /// The event, e.g. `"engine:jump"`.
    pub cue: String,
    /// The qualified sound id to play for it.
    pub sound: String,
    /// The mod that asked, for attribution in the settings screen.
    pub mod_id: String,
}

/// A HUD script a mod wants the client to run.
///
/// # This is the one thing on the wire that is code
///
/// Everything else a server sends is data a client interprets — a widget tree,
/// a sound file, a chunk. This is a Lua source file that will run on the
/// player's machine, and it is the tier-2 half of Task 14's trust model
/// (charter rule 10): a HARD sandbox with no filesystem, no network, no `os`,
/// no `io`, no `load` at all, and an instruction and memory ceiling per FRAME.
///
/// It travels by hash through the same content pipeline as a texture or a
/// sound, so a client that already has it fetches nothing, and the bytes are
/// hostile input like every other pushed asset.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HudScriptDef {
    /// The mod that pushed it, for attribution when it misbehaves.
    ///
    /// Carried rather than inferred, for the reason [`SoundDef::mod_id`] is: a
    /// warning saying which mod's HUD went over budget must name what the
    /// SERVER said, not what a client guessed from a namespace.
    pub mod_id: String,
    /// The content hash of the Lua source, or `None` if the mod named a file
    /// that is not in its directory — the client runs nothing rather than
    /// guessing.
    pub file: Option<ContentHash>,
}

/// One material in the world's id table, as the client needs to see it.
///
/// # Why the client is told this at all
///
/// Chunk blobs carry **world** material ids — the numbers the world database
/// assigned (charter rule 8) — and nothing else. A client that only had the
/// numbers could tell two materials apart but could not tell which was stone,
/// so it could not choose a texture for either.
///
/// The alternative would be for the client to derive the table by running the
/// server's mods itself, which is both a second code path for something the
/// server has already decided and a reason to execute mod code the client has
/// no other need to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterialDef {
    /// The sound a footstep on this material makes, if a mod named one.
    ///
    /// **Played by the client, from its own movement**, so it needs no round
    /// trip and no event: a player's own footsteps are the one sound in the
    /// game whose timing they will notice being late. Other players' steps come
    /// from their entity, like every other sound.
    ///
    /// `None` is a silent material, which is every material until a mod says
    /// otherwise (charter rule 1).
    pub step_sound: Option<String>,
    /// The world id that appears in chunk blobs.
    pub id: u16,
    /// The canonical string id, e.g. `"core:white"`.
    pub name: String,
    /// Whether this can be put in the world.
    ///
    /// **False makes it an item**: a sword, a helmet, a thing you can hold and
    /// carry and not a thing you can build with. Everything else about it is a
    /// material — the atlas tile, the slot it sits in, the units it counts in —
    /// because that is what a stack is made of (charter rule 5), and only this
    /// differs.
    ///
    /// The world palette must never contain a `false` one, which is the
    /// server's job: a placement of an item is refused before it becomes an
    /// edit.
    pub placeable: bool,
    /// Content hash of this material's texture, if it registered one.
    ///
    /// A hash rather than a path: the client fetches it through the same
    /// content-addressed cache as everything else, so a texture it already has
    /// costs nothing, and a server claiming a file it did not send is caught by
    /// the hash rather than by the decoder.
    pub texture: Option<ContentHash>,
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
        /// Desired direction of travel, in **world space**, as `[x, y, z]`.
        ///
        /// World space, not body-relative, and that is a determinism decision
        /// rather than a convenience. Rotating a body-relative vector by the
        /// player's yaw needs `sin` and `cos`, which charter rule 4 bans from
        /// simulation outright — they are libm calls that differ between
        /// platforms. The client already has the yaw and is exempt (rotation is
        /// presentation), so it does the rotation once and both ends then
        /// simulate from the identical numbers, which is what makes prediction
        /// agree with the server bit for bit.
        ///
        /// Only `x` and `z` are read today; `y` is reserved for swimming and
        /// flight. Magnitude is ignored beyond direction — how fast the player
        /// actually moves is the gait's business, so a client cannot ask to go
        /// faster by sending a longer vector.
        movement: [f32; 3],
        /// Yaw and pitch, in turns rather than radians so no trigonometry is
        /// needed to transmit them.
        look: [f32; 2],
        /// Bitfield of named actions currently held. See [`actions`].
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

    /// Begin breaking the block or cell the player is pointing at.
    ///
    /// **Appended at the end** (protocol v5), for the reason on
    /// `ServerMessage::InventoryUpdate`.
    ///
    /// The client says *start* and the server counts the ticks. It cannot work
    /// the other way round: a client that decided when a block broke could
    /// break every block instantly, and charter rule 2 puts that decision on
    /// the server. What comes back is [`ServerMessage::DigProgress`], which is
    /// what the crack overlay is drawn from.
    ///
    /// Re-sending with a different target re-aims and discards progress, so a
    /// client that simply repeats this every tick while the button is held is
    /// behaving correctly.
    StartDig {
        /// The sub-node cell under the crosshair.
        target: SubNodePos,
    },

    /// Stop breaking, discarding progress.
    ///
    /// **Appended at the end** (protocol v5).
    CancelDig,

    /// Choose the tool the player is holding.
    ///
    /// **Appended at the end** (protocol v5).
    ///
    /// `None` is a bare hand, which is also what an unknown id falls back to —
    /// a client naming a tool the server's mods did not register is out of
    /// date, not hostile, and digging slowly is a better answer than a
    /// disconnect.
    SelectTool {
        /// The qualified tool id, e.g. `"core:chisel"`.
        tool: Option<String>,
    },

    /// Place material from the player's inventory into a cell.
    ///
    /// **Appended at the end** (protocol v6).
    ///
    /// A *request*, not an instruction. The server decides whether it happens
    /// at all: whether the player holds any of that material, whether the
    /// target is empty, and whether the resulting geometry would be inside
    /// somebody. Charter rule 2 puts every one of those on the server, and a
    /// client that could place by asserting it had would be able to build out
    /// of nothing and seal players inside blocks.
    ///
    /// The material is named rather than taken from a selected slot because a
    /// slot index is not stable: inventories are consolidated on every credit,
    /// so the index a client selected can name a different stack by the time
    /// its next message lands. Naming the material is unambiguous, and the
    /// server verifies the player actually has it.
    Place {
        /// The cell to fill. Its block is what actually gets written — see
        /// [`crate::inventory::placement_mask`] for the fill order within it.
        ///
        /// The client sends the cell it wants filled, already stepped across
        /// the face it is pointing at. Which cell that is depends on the
        /// camera, and the camera is presentation.
        target: SubNodePos,
        /// Which material to place, as a world material id.
        material: u16,
        /// The occupancy to place it in, or `0` for loose material.
        ///
        /// **Which STACK is being spent, not merely which material.** A player
        /// placing a stair must not have it paid for out of their loose rubble:
        /// the server matches the pair, so the stairs they crafted stay in the
        /// inventory rather than the material draining out from under them.
        shape: u32,
        /// The face this was placed against, as the outward normal of the
        /// surface (protocol v34).
        ///
        /// **Presentation decides which face, the server decides what it
        /// means.** Only the client knows what the crosshair was on, and only
        /// the server may say what a cut stack does about it — a stair
        /// arriving at a wall turns to face the floor, and one on the ground
        /// turns to face the player. Charter rule 2 puts the second half here
        /// and gives the client no say in the geometry.
        ///
        /// Exactly one component is non-zero for a real placement. Anything
        /// else — including all zeroes from a client that does not care — means
        /// "no preference", and the shape is placed as it was authored.
        face: [i8; 3],
    },

    /// Ask for how far the server should stream chunks to this player.
    ///
    /// **Appended at the end** (protocol v12).
    ///
    /// # A request, and the server's own limit is the ceiling
    ///
    /// How far a player can see is a bargain between two machines: the client
    /// knows what its GPU and its patience can take, and the server knows what
    /// it can afford to send fifty of. Neither can decide alone. So the client
    /// asks and the server answers with [`ServerMessage::ViewDistance`],
    /// clamped to its configured maximum — a client asking for the horizon does
    /// not get to make the server pay for it.
    ///
    /// **Asking for LESS is always granted**, and that direction matters more:
    /// a player on a modest machine, or on a bad link, needs a way to make the
    /// world smaller, and before this there was none — the server's setting was
    /// everyone's setting.
    ///
    /// May be sent at any time, not only on join. A player changing it should
    /// not have to reconnect.
    ViewDistance {
        /// Chunks of horizontal radius.
        horizontal: u8,
        /// Chunks of vertical radius.
        vertical: u8,
    },

    /// Hit an entity.
    ///
    /// **Appended at the end** (protocol v15).
    ///
    /// # Why the engine has no idea what this does
    ///
    /// Nothing in core reacts to a punch. It reaches the mods as
    /// `on_punch` and stops there, because what a hit *means* — damage,
    /// knockback, aggro, nothing at all — is a game decision and charter rule 1
    /// puts those in mods. The engine's job is to say who hit what, in a way
    /// nobody can lie about.
    ///
    /// # Reach is checked server-side, like everything else
    ///
    /// The id is the entity as the server named it in
    /// [`ServerMessage::EntitySpawn`], so a client cannot invent one — a stale
    /// id resolves to nothing and a punch at somebody across the map fails the
    /// reach test. Charter rule 2: the client is a viewer, and a viewer that
    /// could assert a hit could assert every hit.
    Punch {
        /// The entity being hit, as the server named it.
        entity: u64,
    },
    /// A mod-registered action was pressed or released.
    ///
    /// **Appended at the end** (protocol v16).
    ///
    /// Only actions the server itself registered are accepted — see the
    /// server's handler. The engine's own controls never come through here:
    /// walking is [`ClientMessage::PlayerInput`] and digging is
    /// [`ClientMessage::StartDig`], both of which the server already judges.
    /// A client that could name any action would be able to invent one.
    ///
    /// Held actions send both edges, so a mod can implement a "while held"
    /// control. Rate limiting is the server's, and is documented there: a
    /// client that spams this is spending the server's Lua budget.
    Action {
        /// The qualified action id, as sent in [`ServerMessage::ActionTable`].
        id: String,
        /// Whether it went down (`true`) or came up (`false`).
        pressed: bool,
    },

    /// Something happened in a dialog the server opened.
    ///
    /// **Appended at the end** (protocol v20).
    ///
    /// A REQUEST, never a result. The clearest case is a slot move: a client
    /// saying "I dragged this there" is asking, and the server's inventory
    /// stays authoritative whatever the client believes. A forged event is
    /// therefore not a special case to detect — it is the ordinary case,
    /// handled by the same validation.
    DialogEvent {
        /// Which dialog, as the server named it in [`ServerMessage::ShowDialog`].
        form: String,
        /// What happened.
        event: DialogEvent,
    },

    /// Put what is in this hand into the off-hand, and take back what was there.
    ///
    /// **Appended at the end** (protocol v26).
    ///
    /// A request like every other inventory gesture: the client says which
    /// hotbar slot the player was holding and the server does the swap against
    /// its own copy. A slot number nobody has is ignored rather than clamped —
    /// clamping would swap a slot the player did not name.
    SwapOffhand {
        /// Which slot to swap with the off-hand, zero-based.
        slot: u16,
    },

    /// Which hotbar slot the player is holding.
    ///
    /// **Appended at the end** (protocol v30).
    ///
    /// A statement of local UI state, not a request: the hotbar keys and the
    /// wheel are the client's, and nothing about the world changes when a
    /// player looks at a different slot. The server keeps it so a MOD can ask
    /// what somebody is holding — which is what makes an item that is not a
    /// block worth registering at all.
    ///
    /// Sent when it changes, not every tick. A slot out of range is clamped
    /// rather than refused: it is a display detail, and disconnecting somebody
    /// over one would be a very expensive way to say nothing.
    SelectSlot {
        /// Zero-based, into `player:main`.
        slot: u16,
    },
}

/// What a player did inside a dialog.
///
/// Every variant names the widget it came from, because a mod told the client
/// that name and has no other way to tell two buttons apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DialogEvent {
    /// A button was pressed.
    Pressed {
        /// The widget's name.
        name: String,
    },
    /// A text input was submitted.
    Submitted {
        /// The widget's name.
        name: String,
        /// What was in it.
        text: String,
    },
    /// A checkbox was ticked or unticked.
    Toggled {
        /// The widget's name.
        name: String,
        /// Its new state.
        checked: bool,
    },
    /// A slider was moved.
    Slid {
        /// The widget's name.
        name: String,
        /// Its new value.
        value: i32,
    },
    /// A dropdown selection changed.
    Chose {
        /// The widget's name.
        name: String,
        /// Which option, as an index.
        index: u16,
    },
    /// A slot was clicked, which is a request to move items.
    ///
    /// The client says what it did with the mouse; the SERVER decides what that
    /// means for an inventory. Splitting, merging and swapping are all the same
    /// message — the server works out which from the slots and the button.
    Clicked {
        /// The inventory view the slot belongs to.
        view: String,
        /// Which slot in that view.
        index: u16,
        /// How it was clicked.
        click: Click,
    },
    /// The player closed the dialog.
    Closed,
    /// A block in a shape editor was chiselled.
    ///
    /// **Appended below `Closed`** (protocol v25) rather than filed beside the
    /// other widget events, because these are position-encoded: slipping a
    /// variant into the middle renumbers every one after it, which is the one
    /// change this format does not survive.
    ///
    /// **The whole mask, not which cell moved.** The client keeps the mask
    /// anyway so a click can land before the server has heard about it, and a
    /// mod handed the whole thing can never rebuild a different shape from
    /// events that arrived out of order.
    ///
    /// The same 27-bit occupancy a block is stored with and a stack is cut to
    /// — see [`crate::inventory::Shape`] — except that it may be empty or full,
    /// which are the two states a shape cannot be and an editor must be able to
    /// reach.
    Chiselled {
        /// The widget's name.
        name: String,
        /// Which cells are filled, indexed `x + 3*y + 9*z`.
        shape: u32,
    },
}

/// How a slot was clicked.
///
/// Named for what the player did, not for what should happen: "right-click"
/// means the same thing on every machine, whereas "split" is a decision the
/// server makes and a mod may want to make differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Click {
    /// Take or place the whole stack.
    Left,
    /// Take half, or place one — the split gesture.
    Right,
    /// Move the stack to the other view in one go.
    ShiftLeft,
}

/// Messages a server sends.
///
/// **APPEND ONLY.** See the module docs.
///
/// `PartialEq` but not `Eq` as of protocol v4, for the same reason as
/// [`ClientMessage`]: [`ServerMessage::PlayerState`] carries `f32` fields and
/// float equality is not an equivalence relation. And for the same reason,
/// [`validate_server_message`] rejects non-finite values before a client can
/// feed them to its own physics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        /// Whether this player may use admin powers — flight, today.
        ///
        /// **Appended to this variant** (protocol v33). Safe because the
        /// version is agreed in the handshake before a `JoinWorld` is ever
        /// sent, so no client decodes this that did not build it.
        ///
        /// Sent rather than asked for, because a client that predicted flight
        /// the server was about to refuse would be corrected back to the
        /// ground every tick. Charter rule 2 decides; this is the decision
        /// arriving.
        may_fly: bool,
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
        /// What the player holds, in ascending material order.
        ///
        /// Two stacks of one material are two entries when they are cut
        /// differently — see [`StackDef`].
        stacks: Vec<StackDef>,
    },

    /// The world's material table for this session.
    ///
    /// **Appended at the end** (protocol v3), below `InventoryUpdate`, for the
    /// reason spelled out on that variant.
    ///
    /// Sent once, after the mod manifest and before the world, because a client
    /// cannot usefully draw a chunk it has no material names for. See
    /// [`MaterialDef`] for why the client needs it rather than deriving it.
    MaterialTable {
        /// Every material, in ascending id order.
        materials: Vec<MaterialDef>,
    },

    /// Where the server says the player is.
    ///
    /// **Appended at the end** (protocol v4), for the reason spelled out on
    /// `InventoryUpdate`.
    ///
    /// This is the authoritative answer the client reconciles against, and the
    /// whole reason it carries [`last_processed_input`](Self::PlayerState::last_processed_input):
    /// the client rewinds to that tick, replays every input it has sent since,
    /// and compares. Without it the client would have no way to know *which* of
    /// its predictions this state already accounts for, and reconciliation
    /// would fight every input still in flight.
    ///
    /// Position follows charter rule 7 — `(i32 chunk, f32 local)` — and the
    /// local part is in **sub-node cells**, `0..48`, which is the unit
    /// [`crate::phys`] works in. Sending yards instead would put a conversion
    /// on both sides of a comparison that has to agree bit for bit.
    PlayerState {
        /// The last input tick the server had applied when it sent this.
        last_processed_input: u64,
        /// Chunk half of the position.
        chunk: ChunkPos,
        /// Cell offset within that chunk, `0..48` on each axis.
        local: [f32; 3],
        /// Cells per tick.
        velocity: [f32; 3],
        /// Whether the server has the player standing on something.
        on_ground: bool,
    },

    /// How far along the player's current dig is.
    ///
    /// **Appended at the end** (protocol v5).
    ///
    /// Sent only to the digger — nobody else needs to draw their crack — and
    /// only while a dig is running. `progress` is `0.0..=1.0`.
    DigProgress {
        /// Which cell is being broken.
        target: SubNodePos,
        /// How far along, `0.0..=1.0`.
        progress: f32,
    },

    /// Every tool the loaded mods registered.
    ///
    /// **Appended at the end** (protocol v7).
    ///
    /// Sent once, beside the material table. A client cannot offer a way to
    /// choose a tool without being told which exist, and charter rule 1 means
    /// it must not simply know: the engine has no tools of its own, not even a
    /// bare hand, so `core_tools:chisel` is mod content and hard-coding it in
    /// the client would be exactly the special-casing rule 1 forbids.
    ToolTable {
        /// Every tool, in ascending id order.
        tools: Vec<ToolDef>,
    },

    /// A chunk's light levels.
    ///
    /// **Appended at the end** (protocol v8).
    ///
    /// Separate from [`ServerMessage::ChunkData`] rather than a field on it,
    /// because the two change independently: placing a lamp relights its
    /// neighbours without altering a single block in them, and re-sending whole
    /// chunk blobs to say so would cost thousands of times the bytes. It
    /// follows that this is both the initial payload and the update — a client
    /// applies it the same way whenever it arrives.
    ///
    /// The payload is [`crate::light::codec`]'s run-length form, which is three
    /// bytes for the uniform chunks that make up most of a world. **It is
    /// hostile input** (charter rule 14): the decoder bounds every run before
    /// writing and has a fuzz target.
    ChunkLight {
        /// Which chunk.
        pos: ChunkPos,
        /// Run-length encoded levels — see [`crate::light::codec::decode`].
        light: Vec<u8>,
    },

    /// The sky a mod registered, sent once on join.
    ///
    /// **Appended at the end** (protocol v9).
    ///
    /// Empty keyframes mean no mod registered a sky, which is a legitimate
    /// world with no day rather than an error — the client holds its colours
    /// fixed. Charter rule 1: the engine has no sky of its own to fall back to.
    SkyTable {
        /// Ticks in a full day.
        day_length_ticks: u32,
        /// Colour keyframes, sorted by time.
        keyframes: Vec<SkyFrame>,
    },

    /// Where the server's clock stands in the day.
    ///
    /// **Appended at the end** (protocol v9).
    ///
    /// Separate from [`ServerMessage::SkyTable`] because it is state rather
    /// than registration: this arrives repeatedly and that arrives once. The
    /// server owns it — a client that advanced its own clock would drift, and
    /// two players standing together would see different skies.
    TimeOfDay {
        /// Position in the day, `0.0..1.0`, midnight to midnight.
        time: f32,
    },

    /// A chunk's fluid.
    ///
    /// **Appended at the end** (protocol v11).
    ///
    /// # Every update is a keyframe, on purpose
    ///
    /// The obvious design is a stream of per-block deltas with an occasional
    /// full state for late joiners and loss recovery. This is the whole layer,
    /// every time, and it is both smaller and safer for the same two reasons
    /// [`ServerMessage::ChunkLight`] is:
    ///
    /// - **Fluid changes as a region, not as a block.** A spreading front moves
    ///   tens of blocks in a chunk per fluid tick, and the run-length form of
    ///   the whole layer is smaller than the deltas describing the front —
    ///   exactly in the case where bandwidth matters. A settled pond sends
    ///   nothing at all, because nothing changed.
    /// - **Loss recovery becomes the normal path rather than a rare one.** A
    ///   client that drops one of these is repaired by the next, with no
    ///   sequence numbers, no gap detection, and no code that runs once a month
    ///   in production and never in a test.
    ///
    /// The payload is [`crate::fluid::codec`]'s run-length form, which is ONE
    /// byte for a chunk with no fluid in it — which is almost every chunk, so
    /// the initial send costs a client nothing to be told there is no milk
    /// here. **It is hostile input** (charter rule 14): the decoder bounds every
    /// run before writing and has a fuzz target.
    ChunkFluid {
        /// Which chunk.
        pos: ChunkPos,
        /// Run-length encoded states — see [`crate::fluid::codec::decode`].
        fluid: Vec<u8>,
    },

    /// Every fluid the server's mods registered, sent once on join.
    ///
    /// **Appended at the end** (protocol v11).
    ///
    /// A chunk's fluid names its fluid by a numeric id, and numeric ids are per
    /// session (charter rule 8) — so without this a client would know a block
    /// held *something* and have nothing to draw it as. Sent alongside
    /// [`ServerMessage::MaterialTable`] and for the same reason.
    ///
    /// Empty is legitimate and common: a mod set that registers no fluid is a
    /// world with no fluid, not an error. The engine ships none of its own.
    FluidTable {
        /// Every fluid, in ascending id order.
        fluids: Vec<FluidDef>,
    },

    /// How far the server is actually streaming to this player.
    ///
    /// **Appended at the end** (protocol v12).
    ///
    /// The answer to [`ClientMessage::ViewDistance`], and sent unprompted on
    /// join so a client knows the server's default before it has asked for
    /// anything. **The granted value, not the requested one**, which is the
    /// point: a client that drew its fog for a radius the server refused would
    /// show the world ending in clear air well before the haze reached it.
    ViewDistance {
        /// Chunks of horizontal radius the server will send.
        horizontal: u8,
        /// Chunks of vertical radius the server will send.
        vertical: u8,
    },

    /// Entities that have come into this player's view.
    ///
    /// **Appended at the end** (protocol v14).
    ///
    /// Everything needed to start drawing one, sent once. A client that missed
    /// this would hold an id it could never draw, which is why this half of
    /// entity replication is reliable and [`Self::EntityState`] is not.
    ///
    /// Per player rather than broadcast: which entities a player can see is
    /// their own interest set, and sending everybody every mob in the world
    /// would make the cost of a populated world quadratic in the players
    /// watching it.
    EntitySpawn {
        /// The entities, in the server's own slot order.
        entities: Vec<EntityDef>,
    },

    /// Entities that have left this player's view, or stopped existing.
    ///
    /// **Appended at the end** (protocol v14).
    ///
    /// The two are one message deliberately: a client cannot do anything
    /// different about them, and telling them apart would mean the server
    /// tracking which entities died as opposed to merely walking away.
    EntityDespawn {
        /// Opaque entity ids.
        entities: Vec<u64>,
    },

    /// Where the entities a player can see are now.
    ///
    /// **Appended at the end** (protocol v14).
    ///
    /// The unreliable half. A lost one is corrected by the next, 50 ms later,
    /// so it carries no information that is not re-sent — which is what lets it
    /// be dropped rather than retransmitted behind a stalled stream.
    ///
    /// An entity that has not moved is not in here at all. A field of settled
    /// mobs therefore costs nothing, which is the case that decides what a
    /// populated world costs to run.
    EntityState {
        /// The server tick this describes, so a client can order what arrives
        /// out of order and interpolate between two of them.
        tick: u64,
        /// One entry per entity that moved.
        entities: Vec<EntityDelta>,
    },
    /// The actions a server's mods registered, sent once on join.
    ///
    /// **Appended at the end** (protocol v16).
    ///
    /// Empty is the ordinary case for a server whose mods add no controls, and
    /// is not an error. The engine's own actions are NOT in here: the client
    /// already has them and a server does not get to redefine what jump means.
    ActionTable {
        /// Every mod-registered action, in load order.
        actions: Vec<ActionDef>,
    },
    /// The sounds a server's mods registered, sent once on join.
    ///
    /// **Appended at the end** (protocol v17).
    ///
    /// Empty is the ordinary case for a server whose mods make no noise.
    SoundTable {
        /// Every registered sound, in load order.
        sounds: Vec<SoundDef>,
    },

    /// Play a sound, because something happened near this player.
    ///
    /// **Appended at the end** (protocol v17).
    ///
    /// Only sent to players inside the request's radius — a sound nobody can
    /// hear costs the check and nothing else. What it sounds like from where
    /// they are standing is the client's business: the server says what
    /// happened and where, never how loud it arrived.
    PlaySound {
        /// The qualified sound id, as sent in [`ServerMessage::SoundTable`].
        sound: String,
        /// Where it happens, in world blocks. Ignored when `entity` is set.
        pos: [f64; 3],
        /// How far it carries, in blocks, for the client's attenuation.
        radius: f32,
        /// How loud, multiplying the sound's registered gain.
        gain: f32,
        /// An entity to follow, if the sound should move with one.
        entity: Option<u64>,
    },

    /// Open a dialog on this player's screen.
    ///
    /// **Appended at the end** (protocol v20).
    ///
    /// Charter rule 14 in its sharpest form: a UI is the pushed asset with the
    /// most obvious reason to want to be code, so this carries no code. The
    /// tree is data, the client renders it, and nothing in it executes. See
    /// [`crate::ui`] for why the tree is flat.
    ShowDialog {
        /// The mod's name for this dialog, echoed back on every event.
        form: String,
        /// What to draw.
        tree: crate::ui::Tree,
        /// Whether to draw it as a small prompt sized to its contents rather
        /// than as the full sheet every other screen takes.
        ///
        /// The engine cannot tell a two-button prompt from an inventory by
        /// looking at the tree — both are containers of widgets — so the mod
        /// says which it built. Default is the sheet: a mod that says nothing
        /// gets the shape the rest of the interface has.
        compact: bool,
    },

    /// Replace the contents of a dialog already open.
    ///
    /// **Appended at the end** (protocol v20).
    ///
    /// A whole tree rather than a patch. A dialog is small, and a patch stream
    /// that ever dropped a message would leave a player looking at something
    /// the server does not believe is there — the same argument
    /// [`ServerMessage::InventoryUpdate`] makes, for the same reason.
    UpdateDialog {
        /// Which dialog.
        form: String,
        /// Its new contents.
        tree: crate::ui::Tree,
        /// As [`ServerMessage::ShowDialog::compact`].
        ///
        /// Carried on the update as well as the open, so a redraw cannot change
        /// the shape of the window it lands in — which is exactly what a
        /// remembered flag would eventually do.
        compact: bool,
    },

    /// Close a dialog.
    ///
    /// **Appended at the end** (protocol v20).
    CloseDialog {
        /// Which dialog.
        form: String,
    },

    /// What one inventory view holds, for the slots a dialog draws.
    ///
    /// **Appended at the end** (protocol v21).
    ///
    /// Separate from [`ServerMessage::InventoryUpdate`], which says what a
    /// player HAS — one consolidated stack per material, which is what a hotbar
    /// wants. This says where it is, which is what a screen wants. The server
    /// derives the first from the second, so they cannot disagree.
    ///
    /// Sent whole, for the reason `InventoryUpdate` is: a view is small, and a
    /// delta stream that dropped a message would leave a player looking at a
    /// slot the server does not believe is there.
    ViewUpdate {
        /// Which view, e.g. `"player:main"`.
        view: String,
        /// What each slot holds, or `None` for an empty slot.
        slots: Vec<Option<StackDef>>,
        /// What is on the player's cursor, if anything.
        ///
        /// Server-held: a move is two half-gestures, and a client that owned
        /// the middle of one could invent items by lying about what it took.
        held: Option<StackDef>,
    },

    /// The HUD scripts a server's mods want the client to run.
    ///
    /// **Appended at the end** (protocol v22).
    ///
    /// Sent once on join, after the tables a HUD reads — a script asking what
    /// the player is carrying before the material table has arrived would draw
    /// a hotbar of numbers. Empty is the ordinary case: a server whose mods
    /// draw no HUD, which is every server before this task.
    ///
    /// Running these is the CLIENT's decision. It is told what a server wants
    /// and applies its own limits; nothing here obliges it to execute
    /// anything.
    HudScripts {
        /// Every pushed script, in mod load order — which is the order they
        /// draw in, so a mod loaded later draws on top.
        scripts: Vec<HudScriptDef>,
    },

    /// Which sound each named event plays.
    ///
    /// **Appended at the end** (protocol v23).
    ///
    /// Sent once on join, after the sound table it refers to. Empty is the
    /// ordinary case for a mod set that binds nothing, and a client with no
    /// bindings simply makes no noise for the events it would otherwise have
    /// resolved itself.
    SoundBindings {
        /// Every binding, in mod load order. Later wins, which is the same rule
        /// the rest of the mod system resolves a conflict by.
        bindings: Vec<SoundBinding>,
    },

    /// Start a looping sound, because something is going on nearby.
    ///
    /// **Appended at the end** (protocol v23).
    ///
    /// Unlike [`ServerMessage::PlaySound`], this is a thing that CONTINUES —
    /// weather, a cave, night. Starting one already running replaces it, so a
    /// mod that makes sure its loop is playing every tick does not accumulate a
    /// tick's worth of copies.
    StartLoop {
        /// The mod's own name for this loop, and what stops it.
        id: String,
        /// The qualified sound id, as sent in [`ServerMessage::SoundTable`].
        sound: String,
        /// Where it is, in world blocks. Ignored when `everywhere`.
        pos: [f64; 3],
        /// How far it carries. Ignored when `everywhere`.
        radius: f32,
        /// How loud, multiplying the sound's registered gain.
        gain: f32,
        /// Heard at full gain wherever the listener stands, with no panning.
        ///
        /// What makes ambience expressible: a night loop is not somewhere, it
        /// is simply on.
        everywhere: bool,
    },

    /// Stop a looping sound.
    ///
    /// **Appended at the end** (protocol v23).
    ///
    /// Stopping one that is not running is not an error on either side.
    StopLoop {
        /// The id the mod gave it.
        id: String,
    },

    /// What entities in view are holding, when it changes.
    ///
    /// **Appended at the end** (protocol v32).
    ///
    /// On the reliable channel with the spawns, not with the state deltas: see
    /// [`EntityHands`] for why the two belong on opposite ones.
    EntityArmed {
        /// One entry per entity whose hands changed.
        entities: Vec<EntityHands>,
    },
}

/// An entity as a client is first told about it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityDef {
    /// Opaque id. Stable for as long as the entity exists.
    pub id: u64,
    /// Chunk half of the position (charter rule 7).
    pub chunk: ChunkPos,
    /// Cell offset within that chunk, `0..48` on each axis.
    pub local: [f32; 3],
    /// Cells per tick.
    pub velocity: [f32; 3],
    /// Facing, quantised: 256 steps around the circle.
    pub yaw: u8,
    /// Pitch, quantised over the quarter turn each way.
    pub pitch: i8,
    /// Which clip to play.
    pub anim: u8,
    /// The model's canonical string id, or `None` for something invisible.
    ///
    /// **A name, not a number.** Models are content-addressed assets a mod
    /// ships, and a client resolves the name against what it has been pushed.
    /// A per-session number would be one more table to keep in step for no
    /// saving worth having — an entity is spawned once.
    pub model: Option<String>,
    /// Footprint and height, in cells, or `None` for something with no box.
    pub collider: Option<[f32; 2]>,
    /// The stack it looks like, for an item lying on the ground.
    ///
    /// **A mod cannot draw anything** (charter rule 1 puts what an item is
    /// worth in a mod and the picture in the engine), so an entity says what
    /// stack it represents and the client draws that. `None` for anything that
    /// is not an item, which is every entity unless a mod says otherwise; an
    /// entity with a stack and no [`EntityDef::model`] is an item, and one with
    /// a model is a rig.
    pub item: Option<StackDef>,
    /// What it is holding: main hand, then off hand.
    ///
    /// **Appended at the end** (protocol v32).
    ///
    /// A different question from [`EntityDef::item`], which is the stack an
    /// entity IS. Reported from the window as every other player having empty
    /// hands: the client draws what the LOCAL player holds from its own
    /// inventory, and nothing on the wire said what anybody else had.
    ///
    /// Two slots rather than the whole hotbar. Hands are what is drawn, so
    /// hands are what has to be carried.
    pub hands: [Option<StackDef>; 2],
    /// The label above it, already resolved to the current display name.
    ///
    /// Resolved by the server because the name bound to a UUID is a fact only
    /// it has (charter rule 13), and a client holding the UUID instead would
    /// show a stale name until it reconnected.
    pub nametag: Option<String>,
}

/// What one entity is holding now.
///
/// **Its own message and not a field on [`EntityDelta`]** (protocol v32). A
/// position changes every tick and may be dropped; a hand changes rarely and
/// may not. Carrying it in the delta would pay for it twenty times a second to
/// send something usually unchanged, on the one channel where losing it leaves
/// a sword invisible until its holder next switches slots.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityHands {
    /// Opaque id.
    pub id: u64,
    /// Main hand, then off hand.
    pub hands: [Option<StackDef>; 2],
}

/// Where one entity is now.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityDelta {
    /// Opaque id.
    pub id: u64,
    /// Chunk half of the position.
    pub chunk: ChunkPos,
    /// Cell offset within that chunk.
    pub local: [f32; 3],
    /// Cells per tick.
    pub velocity: [f32; 3],
    /// Facing, quantised.
    pub yaw: u8,
    /// Pitch, quantised.
    pub pitch: i8,
    /// Which clip to play.
    pub anim: u8,
}

/// One registered fluid, as the wire carries it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FluidDef {
    /// The per-session numeric id a chunk's fluid layer refers to.
    pub id: u8,
    /// The canonical string id, `"core_milk:milk"`.
    ///
    /// Carried even though nothing draws with it, because a client that could
    /// not name what it is standing in could not report it either — and every
    /// diagnostic anybody writes about fluid will want the name rather than the
    /// number.
    pub name: String,
    /// The world material id a full block of it is drawn as.
    pub material: u16,
    /// What being inside it looks like, as sRGB `0..=255`.
    ///
    /// **Not the texture, and not derived from it.** A texture is what a surface
    /// of the fluid looks like from outside; this is what the whole world looks
    /// like when you are under it, and the two are genuinely different choices —
    /// clear water has a vivid surface and a faint tint. The engine has no
    /// opinion about either (charter rule 1): the mod that registered the fluid
    /// says.
    pub color: [u8; 3],
}

/// One moment in a mod's day, on the wire.
///
/// The protocol's own copy of [`crate::script::SkyKeyframe`] rather than a
/// re-export: the script type is what a mod's Lua produces and this is what
/// peers agree on, and letting one change the other silently would make a mod
/// API edit into a protocol break.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SkyFrame {
    /// When in the day, `0.0..=1.0`.
    pub time: f32,
    /// The sky's colour, which fog fades towards.
    pub sky: [f32; 3],
    /// The sun's colour.
    pub sun: [f32; 3],
    /// How strong the sun is, `0.0..=1.0`.
    pub intensity: f32,
    /// How the finished frame is graded at this moment.
    pub grade: SkyGrade,
}

/// How a moment's finished picture is graded, on the wire.
///
/// The protocol's own copy of [`crate::script::SkyGrade`], for the reason
/// [`SkyFrame`] is: a mod API edit must not be able to change what peers agree
/// on by accident.
///
/// **Every field is validated by the server before it reaches here** and again
/// by the client, because this arrives from a peer (charter rule 14) and a
/// `gamma` of zero or a `NaN` `tint` would come out as a frame of one colour.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SkyGrade {
    /// Multiplies the scene before the tonemap.
    pub exposure: f32,
    /// Multiplies each channel of the graded image.
    pub tint: [f32; 3],
    /// Added to each channel after `tint`.
    pub offset: [f32; 3],
    /// Pushes each channel away from mid grey.
    pub contrast: f32,
    /// Blends towards luma below 1 and away from it above.
    pub saturation: f32,
    /// Applied last, per channel.
    pub gamma: f32,
}

impl SkyGrade {
    /// No grading at all.
    pub const NONE: Self = Self {
        exposure: 1.0,
        tint: [1.0; 3],
        offset: [0.0; 3],
        contrast: 1.0,
        saturation: 1.0,
        gamma: 1.0,
    };
}

impl Default for SkyGrade {
    fn default() -> Self {
        Self::NONE
    }
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

    /// A message decoded, but what it describes is not usable.
    ///
    /// Distinct from [`ProtocolError::Malformed`], which is postcard saying the
    /// BYTES are wrong. This is the bytes being fine and their meaning not:
    /// a widget tree that nests too deep, a child range pointing off the end.
    /// A mod author reads this one, so it carries a sentence rather than a
    /// wrapped decoder error.
    #[error("{what}")]
    Unusable {
        /// What is wrong, in terms the reader can act on.
        what: String,
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
        ClientMessage::Action { id, .. } => {
            // An id a client chose. Bounded before it is looked up, so a peer
            // cannot spend the server's memory naming an action nobody has.
            check_len("action_id", id.len(), MAX_ID_BYTES)?;
        }
        ClientMessage::DialogEvent { form, event } => check_dialog_event(form, event)?,
        ClientMessage::SelectTool { tool: Some(tool) } => {
            // An id is a string from a peer. Bounded like a display name, and
            // for the same reason: it is stored and echoed.
            check_len("tool", tool.len(), MAX_NAME_BYTES)?;
        }
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
        ClientMessage::BlockDelta { edit } => check_occupancy(edit)?,

        ClientMessage::AuthResponse { .. }
        | ClientMessage::JoinWorld
        | ClientMessage::AddKey { .. }
        | ClientMessage::RotateKey { .. }
        | ClientMessage::StartDig { .. }
        | ClientMessage::CancelDig
        | ClientMessage::SelectTool { tool: None }
        | ClientMessage::Place { .. }
        // Two bytes, and `ViewDistance::clamped` bounds them on the way in —
        // there is no value a peer can put here that costs anything to hold.
        | ClientMessage::ViewDistance { .. }
        // Two bytes naming a slot. A number nobody has is ignored by the
        // server rather than clamped — clamping would swap a slot the player
        // did not name.
        | ClientMessage::SwapOffhand { .. }
        // Two bytes naming a slot, and nothing about the world changes when a
        // player looks at a different one. Clamped by the server rather than
        // refused: disconnecting somebody over a display detail would be a very
        // expensive way to say nothing.
        | ClientMessage::SelectSlot { .. }
        // Eight bytes naming an entity. Every value is decodable and almost
        // all of them resolve to nothing, which the server treats as a punch
        // at thin air — there is nothing to bound here that the entity store
        // does not bound already.
        | ClientMessage::Punch { .. }
        | ClientMessage::Disconnect => {}
    }
    Ok(())
}

/// Rejects an occupancy mask that addresses cells a block does not have.
///
/// A block has [`crate::UNITS_PER_BLOCK`] cells, so only that many
/// bits mean anything. A peer setting a higher one is broken or probing, and
/// the honest answer is to refuse the message rather than to mask the bits off
/// and act on a request nobody made.
fn check_occupancy(edit: &Edit) -> Result<(), ProtocolError> {
    let Edit::Partial { occupancy, .. } = edit else {
        return Ok(());
    };
    let addressable = (1u32 << crate::UNITS_PER_BLOCK) - 1;
    if occupancy & !addressable != 0 {
        return Err(ProtocolError::FieldTooLarge {
            field: "occupancy",
            len: occupancy.count_ones() as usize,
            limit: crate::UNITS_PER_BLOCK as usize,
        });
    }
    Ok(())
}

/// Bounds a sound table, and the ids in it.
/// Bounds a pushed HUD script table.
///
/// The SOURCE is not checked here and cannot be: it arrives later, by hash,
/// through the content pipeline, and what makes it safe is the sandbox it runs
/// in rather than anything a decoder could see. What is bounded here is what
/// this message can make a client allocate before any of that.
fn check_hud_scripts(scripts: &[HudScriptDef]) -> Result<(), ProtocolError> {
    check_len("hud_scripts", scripts.len(), MAX_HUD_SCRIPTS)?;
    for script in scripts {
        check_len("hud_script_mod_id", script.mod_id.len(), MAX_ID_BYTES)?;
    }
    Ok(())
}

/// The two messages whose floats reach the client's own arithmetic.
///
/// A non-finite position propagates into every interpolation it takes part in,
/// and a dig progress out of range is an index off the end of the crack
/// texture. Charter rule 14: a server is not trusted merely for being the
/// server.
///
/// Split out of `validate_server_message`, which was at clippy's line ceiling.
fn check_player_floats(message: &ServerMessage) -> Result<(), ProtocolError> {
    let bad = |field: &'static str| {
        Err(ProtocolError::FieldTooLarge {
            field,
            len: 0,
            limit: 0,
        })
    };
    match message {
        ServerMessage::PlayerState {
            local, velocity, ..
        } => {
            if local.iter().chain(velocity.iter()).any(|v| !v.is_finite()) {
                return bad("player_state");
            }
            Ok(())
        }
        ServerMessage::DigProgress { progress, .. } => {
            if !progress.is_finite() || !(0.0..=1.0).contains(progress) {
                return bad("dig_progress");
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Bounds the three protocol v23 messages.
///
/// One function rather than three arms, because `validate_server_message` was
/// at clippy's line ceiling again — the fourth time this task that appending to
/// a well-named function was the wrong move.
fn check_cue_message(message: &ServerMessage) -> Result<(), ProtocolError> {
    match message {
        ServerMessage::SoundBindings { bindings } => check_bindings(bindings),
        ServerMessage::StartLoop { id, sound, .. } => {
            check_len("loop_id", id.len(), MAX_ID_BYTES)?;
            check_len("loop_sound", sound.len(), MAX_ID_BYTES)
        }
        ServerMessage::StopLoop { id } => check_len("loop_id", id.len(), MAX_ID_BYTES),
        _ => Ok(()),
    }
}

/// Bounds a cue-binding table.
fn check_bindings(bindings: &[SoundBinding]) -> Result<(), ProtocolError> {
    check_len("sound_bindings", bindings.len(), MAX_SOUND_BINDINGS)?;
    for binding in bindings {
        check_len("cue", binding.cue.len(), MAX_ID_BYTES)?;
        check_len("bound_sound", binding.sound.len(), MAX_ID_BYTES)?;
        check_len("binding_mod_id", binding.mod_id.len(), MAX_ID_BYTES)?;
    }
    Ok(())
}

fn check_sounds(sounds: &[SoundDef]) -> Result<(), ProtocolError> {
    check_len("sound_table", sounds.len(), MAX_SOUNDS)?;
    for sound in sounds {
        check_len("sound_id", sound.id.len(), MAX_ID_BYTES)?;
        check_len("sound_mod_id", sound.mod_id.len(), MAX_ID_BYTES)?;
    }
    Ok(())
}

/// Bounds a play request, including the numbers that reach a mixer.
///
/// Charter rule 14: a server is not trusted. A `NaN` gain is not a quiet sound,
/// it is undefined behaviour in somebody's ears.
fn check_play(message: &ServerMessage) -> Result<(), ProtocolError> {
    let ServerMessage::PlaySound {
        sound,
        pos,
        radius,
        gain,
        ..
    } = message
    else {
        return Ok(());
    };
    check_len("sound_id", sound.len(), MAX_ID_BYTES)?;
    if !pos.iter().all(|value| value.is_finite()) || !radius.is_finite() || !gain.is_finite() {
        return Err(ProtocolError::FieldTooLarge {
            field: "play_sound",
            len: 0,
            limit: 0,
        });
    }
    Ok(())
}

/// Bounds every string in an action table.
///
/// Its own function because `validate_server_message` is at clippy's line limit,
/// and because the caps are the interesting part: charter rule 14 says a server
/// is not trusted, and each of these strings is one the client keeps for the
/// session and shows in its settings screen.
/// Bounds an inventory view a server sent.
///
/// Its own function because `validate_server_message` sits at clippy's line
/// limit — a real constraint here rather than a lint being obeyed.
fn check_view(view: &str, slots: &[Option<StackDef>]) -> Result<(), ProtocolError> {
    check_len("view", view.len(), MAX_ID_BYTES)?;
    // A server claiming a million slots is a server making the client allocate
    // a million slots (charter rule 14). The cap is the dialog schema's, so a
    // view can always be shown by a single grid.
    check_len(
        "view_slots",
        slots.len(),
        crate::ui::Limits::default().grid_slots,
    )
}

/// Bounds a dialog a server sent, and the tree in it.
///
/// Its own function because `validate_server_message` sits at clippy's line
/// limit — which is a real constraint here rather than a lint being obeyed: the
/// tree check is the substantial part and reads better named.
fn check_dialog(form: &str, tree: Option<&crate::ui::Tree>) -> Result<(), ProtocolError> {
    check_len("dialog_form", form.len(), MAX_ID_BYTES)?;
    let Some(tree) = tree else {
        return Ok(());
    };
    // **The tree's own limits, at decode.** Charter rule 14, and Task 14 asks
    // for it here in as many words: a client refuses a tree that is too large
    // or too deep BEFORE anything walks it. See `crate::ui` for why a flat
    // representation is what makes that possible at all.
    crate::ui::check(tree, crate::ui::Limits::default()).map_err(|err| ProtocolError::Unusable {
        what: format!("dialog `{form}`: {err}"),
    })
}

/// Bounds a dialog event a client sent.
///
/// Every string here is a name the SERVER gave the client, echoed back — so a
/// peer returning a megabyte where a widget name should be is the ordinary
/// hostile case rather than an exotic one.
fn check_dialog_event(form: &str, event: &DialogEvent) -> Result<(), ProtocolError> {
    check_len("dialog_form", form.len(), MAX_ID_BYTES)?;
    match event {
        DialogEvent::Pressed { name }
        | DialogEvent::Toggled { name, .. }
        | DialogEvent::Slid { name, .. }
        | DialogEvent::Chose { name, .. }
        | DialogEvent::Chiselled { name, .. } => {
            check_len("widget_name", name.len(), MAX_ID_BYTES)?;
        }
        DialogEvent::Submitted { name, text } => {
            check_len("widget_name", name.len(), MAX_ID_BYTES)?;
            // What a player typed, bounded like a chat line — it is the same
            // kind of thing and reaches the same kind of place.
            check_len("dialog_text", text.len(), MAX_CHAT_BYTES)?;
        }
        DialogEvent::Clicked { view, .. } => check_len("view", view.len(), MAX_ID_BYTES)?,
        DialogEvent::Closed => {}
    }
    Ok(())
}

fn check_actions(actions: &[ActionDef]) -> Result<(), ProtocolError> {
    check_len("action_table", actions.len(), MAX_ACTIONS)?;
    for action in actions {
        check_len("action_id", action.id.len(), MAX_ID_BYTES)?;
        check_len("action_mod_id", action.mod_id.len(), MAX_ID_BYTES)?;
        check_len(
            "action_description",
            action.description.len(),
            MAX_CHAT_BYTES,
        )?;
        // A key NAME, not a key. Bounded like a display name, which is the
        // shape it is — winit's longest is well inside this.
        check_len(
            "action_default_key",
            action.default_key.len(),
            MAX_NAME_BYTES,
        )?;
    }
    Ok(())
}

/// Checks a decoded server message before a client acts on it.
///
/// The mirror of [`validate_client_message`], and it exists for charter rule
/// 14: a client decodes messages from servers it has no reason to trust, so
/// "the server said so" is not a reason to skip a check the client would apply
/// to anyone else.
///
/// The concrete hazard today is [`ServerMessage::PlayerState`]. The client
/// replays it through the *same* [`crate::phys`] code the server runs, so a
/// non-finite position is not a display glitch — it is a `NaN` inside the
/// client's own simulation, which charter rule 4 forbids outright and whose
/// payload is not even specified across platforms.
///
/// # Errors
///
/// [`ProtocolError::FieldTooLarge`] if a field breaks its cap, or if a
/// `PlayerState` carries a non-finite coordinate or velocity.
pub fn validate_server_message(message: &ServerMessage) -> Result<(), ProtocolError> {
    match message {
        message @ (ServerMessage::PlayerState { .. } | ServerMessage::DigProgress { .. }) => {
            check_player_floats(message)?;
        }
        // **Entity positions reach the client's own frame arithmetic**, and a
        // non-finite one propagates into every interpolation it takes part in
        // — the same hazard `PlayerState` has, from a different message.
        // Charter rule 14: a server is not trusted merely for being the server.
        ServerMessage::EntitySpawn { entities } => check_entity_spawns(entities)?,
        ServerMessage::EntityDespawn { entities } => {
            check_len("entity_despawn", entities.len(), MAX_ENTITIES_PER_MESSAGE)?;
        }
        // **A held stack is a shape, and a server is not trusted** (charter
        // rule 14). The same check the spawn's item gets, for the same reason:
        // a mask with bits above the block would index past the cells a
        // renderer walks.
        ServerMessage::EntityArmed { entities } => {
            check_len("entity_armed", entities.len(), MAX_ENTITIES_PER_MESSAGE)?;
            for entity in entities {
                check_hands(&entity.hands)?;
            }
        }
        ServerMessage::EntityState { entities, .. } => {
            check_len("entity_state", entities.len(), MAX_ENTITIES_PER_MESSAGE)?;
            for entity in entities {
                let finite = entity
                    .local
                    .iter()
                    .chain(entity.velocity.iter())
                    .all(|value| value.is_finite());
                if !finite {
                    return Err(ProtocolError::FieldTooLarge {
                        field: "entity_state",
                        len: 0,
                        limit: 0,
                    });
                }
            }
        }
        ServerMessage::Chat { text, .. } => check_len("chat", text.len(), MAX_CHAT_BYTES)?,
        ServerMessage::ContentChunk { data, .. } => {
            check_len("content_chunk", data.len(), MAX_CONTENT_CHUNK_BYTES)?;
        }
        // The same bound as the client side: a server is not automatically
        // trusted either (charter rule 14), and an occupancy mask that
        // addresses cells a block does not have would be applied to the
        // client's own copy of the world.
        ServerMessage::BlockDelta { edit, .. } => check_occupancy(edit)?,

        ServerMessage::ShowDialog { form, tree, .. }
        | ServerMessage::UpdateDialog { form, tree, .. } => {
            check_dialog(form, Some(tree))?;
        }
        ServerMessage::CloseDialog { form } => check_dialog(form, None)?,
        ServerMessage::ViewUpdate { view, slots, .. } => check_view(view, slots)?,
        ServerMessage::ActionTable { actions } => check_actions(actions)?,
        ServerMessage::SoundTable { sounds } => check_sounds(sounds)?,
        ServerMessage::HudScripts { scripts } => check_hud_scripts(scripts)?,
        message @ (ServerMessage::SoundBindings { .. }
        | ServerMessage::StartLoop { .. }
        | ServerMessage::StopLoop { .. }) => check_cue_message(message)?,
        message @ ServerMessage::PlaySound { .. } => check_play(message)?,

        ServerMessage::HelloAck { .. }
        | ServerMessage::AuthChallenge { .. }
        | ServerMessage::ModManifest { .. }
        | ServerMessage::JoinWorld { .. }
        | ServerMessage::ChunkData { .. }
        | ServerMessage::ChunkUnload { .. }
        | ServerMessage::EntityStateDelta { .. }
        | ServerMessage::Disconnect { .. }
        | ServerMessage::InventoryUpdate { .. }
        | ServerMessage::MaterialTable { .. }
        | ServerMessage::ToolTable { .. }
        | ServerMessage::ChunkLight { .. }
        | ServerMessage::ChunkFluid { .. }
        | ServerMessage::FluidTable { .. }
        | ServerMessage::SkyTable { .. }
        | ServerMessage::ViewDistance { .. }
        | ServerMessage::TimeOfDay { .. } => {}
    }
    Ok(())
}

/// Refuses an entity spawn a client could not safely draw.
///
/// **Entity positions reach the client's own frame arithmetic**, and a
/// non-finite one propagates into every interpolation it takes part in — the
/// same hazard `PlayerState` has, from a different message. Charter rule 14: a
/// server is not trusted merely for being the server.
///
/// Lifted out of `validate_server_message` because that function is at clippy's
/// line ceiling and this is the longest arm in it.
fn check_entity_spawns(entities: &[EntityDef]) -> Result<(), ProtocolError> {
    check_len("entity_spawn", entities.len(), MAX_ENTITIES_PER_MESSAGE)?;
    for entity in entities {
        let finite = entity
            .local
            .iter()
            .chain(entity.velocity.iter())
            .chain(entity.collider.iter().flatten())
            .all(|value| value.is_finite());
        if !finite {
            return Err(ProtocolError::FieldTooLarge {
                field: "entity_spawn",
                len: 0,
                limit: 0,
            });
        }
        if let Some(model) = &entity.model {
            check_len("entity_model", model.len(), MAX_ID_BYTES)?;
        }
        if let Some(nametag) = &entity.nametag {
            check_len("entity_nametag", nametag.len(), MAX_NAME_BYTES)?;
        }
        // **A shape is twenty-seven bits and a server is not trusted** (charter
        // rule 14). A mask with bits above the block would index past the cells
        // a renderer walks, which is the one thing in a stack that is not
        // simply a number.
        if let Some(item) = &entity.item
            && item.shape & !crate::inventory::Shape::ALL != 0
        {
            return Err(ProtocolError::FieldTooLarge {
                field: "entity_item_shape",
                len: item.shape as usize,
                limit: crate::inventory::Shape::ALL as usize,
            });
        }
        // And what it is HOLDING, which arrives by the same road and reaches
        // the same renderer.
        check_hands(&entity.hands)?;
    }
    Ok(())
}

/// Refuses a pair of hands holding a shape that is not one.
///
/// **A shape is twenty-seven bits and a server is not trusted** (charter rule
/// 14). A mask with bits above the block would index past the cells a renderer
/// walks, which is the one thing in a stack that is not simply a number.
///
/// Its own function because hands arrive by two roads — on a spawn and in
/// their own message — and two copies of a bounds check is where one of them
/// stops matching.
fn check_hands(hands: &[Option<StackDef>; 2]) -> Result<(), ProtocolError> {
    for held in hands.iter().flatten() {
        if held.shape & !crate::inventory::Shape::ALL != 0 {
            return Err(ProtocolError::FieldTooLarge {
                field: "entity_hands_shape",
                len: held.shape as usize,
                limit: crate::inventory::Shape::ALL as usize,
            });
        }
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
    fn a_non_finite_player_state_from_a_server_is_rejected_too() {
        // The direction nobody checks. A client replays `PlayerState` through
        // the same physics the server runs, so a hostile server sending a NaN
        // position is not a rendering glitch — it is a NaN in the client's own
        // simulation state, which charter rule 4 forbids. Charter rule 14 says
        // to expect exactly this from a server you have no reason to trust.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for message in [
                ServerMessage::PlayerState {
                    last_processed_input: 1,
                    chunk: ChunkPos::new(0, 0, 0),
                    local: [bad, 0.0, 0.0],
                    velocity: [0.0; 3],
                    on_ground: true,
                },
                ServerMessage::PlayerState {
                    last_processed_input: 1,
                    chunk: ChunkPos::new(0, 0, 0),
                    local: [0.0; 3],
                    velocity: [0.0, bad, 0.0],
                    on_ground: true,
                },
            ] {
                assert!(
                    validate_server_message(&message).is_err(),
                    "{bad} must not reach the client's physics"
                );
            }
        }

        let good = ServerMessage::PlayerState {
            last_processed_input: 7,
            chunk: ChunkPos::new(1, 2, 3),
            local: [24.0, 3.0, 24.0],
            velocity: [0.1, -0.2, 0.0],
            on_ground: true,
        };
        assert!(validate_server_message(&good).is_ok());
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
        let client: [(ClientMessage, u8); 16] = [
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
            // Protocol v5, appended after Disconnect — the variant an append
            // is most likely to displace, for the same reason it is on the
            // server side: it reads like the natural end of the enum.
            (
                ClientMessage::StartDig {
                    target: SubNodePos::new(0, 0, 0),
                },
                10,
            ),
            (ClientMessage::CancelDig, 11),
            (ClientMessage::SelectTool { tool: None }, 12),
            // Protocol v6.
            (
                ClientMessage::Place {
                    target: SubNodePos::new(0, 0, 0),
                    material: 0,
                    shape: 0,
                    face: [0; 3],
                },
                13,
            ),
            // Protocol v12.
            (
                ClientMessage::ViewDistance {
                    horizontal: 0,
                    vertical: 0,
                },
                14,
            ),
            // Protocol v15.
            (ClientMessage::Punch { entity: 0 }, 15),
        ];

        for (message, expected) in client {
            let bytes = encode(&message).expect("encode");
            assert_eq!(
                bytes[0], expected,
                "{message:?} should be ordinal {expected}; a variant was inserted or reordered"
            );
        }
    }

    /// Ordinals 16 onwards. Its own function for the same reason
    /// `pin_the_later_variants` is: the one above is at clippy's line ceiling,
    /// and an ordinal that cannot be pinned because the test is too long is an
    /// ordinal that goes unpinned.
    #[test]
    fn later_client_variant_ordinals_are_pinned() {
        // Protocol v16 and v20, which went unpinned for six protocol versions.
        let action = encode(&ClientMessage::Action {
            id: String::new(),
            pressed: false,
        })
        .expect("encode");
        assert_eq!(action[0], 16);
        let dialog = encode(&ClientMessage::DialogEvent {
            form: String::new(),
            event: DialogEvent::Closed,
        })
        .expect("encode");
        assert_eq!(dialog[0], 17);
        // Protocol v26.
        let swap = encode(&ClientMessage::SwapOffhand { slot: 0 }).expect("encode");
        assert_eq!(swap[0], 18);
        // Protocol v30.
        let select = encode(&ClientMessage::SelectSlot { slot: 0 }).expect("encode");
        assert_eq!(select[0], 19);
    }

    #[test]
    fn edit_variant_ordinals_are_pinned() {
        // `Edit` is nested inside `BlockDelta` on both sides, so it is exactly
        // as position-encoded as the messages themselves and exactly as
        // dangerous to reorder — with the extra hazard that its ordinal is
        // buried in a message body rather than sitting at byte zero, so a peer
        // reading a shifted `Edit` gets a plausible-looking edit at a wrong
        // position rather than an obvious failure.
        //
        // It had no pin until protocol v6 appended `Partial`. This is that pin.
        let edits: [(Edit, u8); 3] = [
            (
                Edit::Block {
                    pos: BlockPos::new(0, 0, 0),
                    material: 0,
                },
                0,
            ),
            (
                Edit::SubNode {
                    pos: SubNodePos::new(0, 0, 0),
                    material: 0,
                },
                1,
            ),
            (
                Edit::Partial {
                    pos: BlockPos::new(0, 0, 0),
                    material: 0,
                    occupancy: 0,
                },
                2,
            ),
        ];

        for (edit, expected) in edits {
            let bytes = postcard::to_allocvec(&edit).expect("encode");
            assert_eq!(
                bytes[0], expected,
                "{edit:?} should be ordinal {expected}; a variant was inserted or reordered"
            );
        }
    }

    #[test]
    fn an_occupancy_mask_addressing_cells_a_block_does_not_have_is_refused() {
        // A mask is 27 meaningful bits in a `u32`, so five bits of it address
        // nothing. A peer setting one is broken or probing, and masking them
        // off silently would apply a request nobody made.
        let hostile = Edit::Partial {
            pos: BlockPos::new(0, 0, 0),
            material: 1,
            occupancy: 1 << crate::UNITS_PER_BLOCK,
        };
        assert!(
            validate_client_message(&ClientMessage::BlockDelta {
                edit: hostile.clone()
            })
            .is_err(),
            "an out-of-range occupancy bit was accepted from a client"
        );
        // A server is not trusted either (charter rule 14) — this one lands in
        // the client's own copy of the world.
        assert!(
            validate_server_message(&ServerMessage::BlockDelta {
                edit: hostile,
                actor: None
            })
            .is_err(),
            "an out-of-range occupancy bit was accepted from a server"
        );

        // The counter-example: a full, legitimate mask must still pass, or this
        // would be satisfied by a validator that refused everything.
        assert!(
            validate_client_message(&ClientMessage::BlockDelta {
                edit: Edit::Partial {
                    pos: BlockPos::new(0, 0, 0),
                    material: 1,
                    occupancy: (1 << crate::UNITS_PER_BLOCK) - 1,
                }
            })
            .is_ok(),
            "a legitimate full occupancy mask was refused"
        );
    }

    #[test]
    fn server_variant_ordinals_are_pinned() {
        let server: [(ServerMessage, u8); 14] = [
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
                    may_fly: false,
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
            // Protocol v7. `appended_server_variants_keep_their_ordinals`
            // covers the ones added between; this pins the newest.
            (ServerMessage::ToolTable { tools: Vec::new() }, 15),
            (
                ServerMessage::ChunkLight {
                    pos: ChunkPos::new(0, 0, 0),
                    light: Vec::new(),
                },
                16,
            ),
            (
                ServerMessage::SkyTable {
                    day_length_ticks: 0,
                    keyframes: Vec::new(),
                },
                17,
            ),
            (ServerMessage::TimeOfDay { time: 0.0 }, 18),
        ];

        for (message, expected) in server {
            let bytes = encode(&message).expect("encode");
            assert_eq!(
                bytes[0], expected,
                "{message:?} should be ordinal {expected}; a variant was inserted or reordered"
            );
        }
    }

    #[test]
    fn appended_server_variants_keep_their_ordinals() {
        // Split from `server_variant_ordinals_are_pinned` only because that
        // test outgrew the line limit; these are the variants added after the
        // original ten, and each is the one an *even later* append is most
        // likely to displace.
        //
        // Disconnect is the perennial hazard: it reads like the natural end of
        // the enum, so a new variant gets written above it. Doing exactly that
        // is what this caught during the protocol v2 change.
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

        // Protocol v3, appended after InventoryUpdate.
        let materials = encode(&ServerMessage::MaterialTable {
            materials: Vec::new(),
        })
        .expect("encode");
        assert_eq!(materials[0], 12);

        // Protocol v4, appended after MaterialTable.
        let state = encode(&ServerMessage::PlayerState {
            last_processed_input: 0,
            chunk: ChunkPos::new(0, 0, 0),
            local: [0.0; 3],
            velocity: [0.0; 3],
            on_ground: false,
        })
        .expect("encode");
        assert_eq!(state[0], 13);

        // Protocol v5, appended after PlayerState.
        //
        // Written above it first, which moved `PlayerState` from 13 to 14 and
        // would have silently reinterpreted every position update on every
        // existing peer. This test caught it on the first run — which is the
        // entire reason every ordinal is pinned rather than just the last.
        let dig = encode(&ServerMessage::DigProgress {
            target: SubNodePos::new(0, 0, 0),
            progress: 0.0,
        })
        .expect("encode");
        assert_eq!(dig[0], 14);

        // Protocol v11, appended after TimeOfDay and currently the newest.
        // Pinned here rather than in the list above only because that test is
        // at its line limit; the hazard is identical and so is the check.
        let fluid = encode(&ServerMessage::ChunkFluid {
            pos: ChunkPos::new(0, 0, 0),
            fluid: Vec::new(),
        })
        .expect("encode");
        assert_eq!(fluid[0], 19);

        let table = encode(&ServerMessage::FluidTable { fluids: Vec::new() }).expect("encode");
        assert_eq!(table[0], 20);

        // Protocol v12.
        let view = encode(&ServerMessage::ViewDistance {
            horizontal: 0,
            vertical: 0,
        })
        .expect("encode");
        assert_eq!(view[0], 21);

        // Protocol v14, appended after ViewDistance and currently the newest.
        let spawn = encode(&ServerMessage::EntitySpawn {
            entities: Vec::new(),
        })
        .expect("encode");
        assert_eq!(spawn[0], 22);

        let despawn = encode(&ServerMessage::EntityDespawn {
            entities: Vec::new(),
        })
        .expect("encode");
        assert_eq!(despawn[0], 23);

        let state = encode(&ServerMessage::EntityState {
            tick: 0,
            entities: Vec::new(),
        })
        .expect("encode");
        assert_eq!(state[0], 24);

        // The tables and messages from v16 onwards, pinned in their own
        // function: this one is at clippy's 100-line ceiling and an ordinal
        // that cannot be pinned because the test is too long is an ordinal
        // that goes unpinned, which is the exact failure this whole test
        // exists to prevent.
        pin_the_later_variants();
    }

    /// **The two enums NESTED inside a message, which the pins above miss.**
    ///
    /// `ClientMessage::DialogEvent` has had a pinned ordinal since v20 and its
    /// payload has not — so `DialogEvent` and `Widget` could be reordered
    /// freely and every ordinal test would stay green while a client and a
    /// server disagreed about what a click was. Found appending
    /// `DialogEvent::Chiselled` in v25, where the natural place to file it was
    /// beside the other widget events and directly above `Closed`.
    #[test]
    fn nested_dialog_variant_ordinals_are_pinned() {
        let ordinal = |event: &DialogEvent| encode(event).expect("encode")[0];
        assert_eq!(
            ordinal(&DialogEvent::Pressed {
                name: String::new()
            }),
            0
        );
        assert_eq!(
            ordinal(&DialogEvent::Submitted {
                name: String::new(),
                text: String::new(),
            }),
            1
        );
        assert_eq!(
            ordinal(&DialogEvent::Toggled {
                name: String::new(),
                checked: false,
            }),
            2
        );
        assert_eq!(
            ordinal(&DialogEvent::Slid {
                name: String::new(),
                value: 0,
            }),
            3
        );
        assert_eq!(
            ordinal(&DialogEvent::Chose {
                name: String::new(),
                index: 0,
            }),
            4
        );
        assert_eq!(
            ordinal(&DialogEvent::Clicked {
                view: String::new(),
                index: 0,
                click: Click::Left,
            }),
            5
        );
        assert_eq!(ordinal(&DialogEvent::Closed), 6);
        // Protocol v25, appended below `Closed` rather than filed tidily.
        assert_eq!(
            ordinal(&DialogEvent::Chiselled {
                name: String::new(),
                shape: 0,
            }),
            7
        );
    }

    /// The widget set, for the same reason and with the same history.
    #[test]
    fn widget_variant_ordinals_are_pinned() {
        use crate::ui::{Align, Direction, Widget};

        let ordinal = |widget: &Widget| encode(widget).expect("encode")[0];
        assert_eq!(
            ordinal(&Widget::Container {
                direction: Direction::Column,
                gap: 0,
                padding: 0,
                align: Align::Start,
            }),
            0
        );
        assert_eq!(
            ordinal(&Widget::Label {
                text: String::new()
            }),
            1
        );
        assert_eq!(
            ordinal(&Widget::Button {
                text: String::new()
            }),
            2
        );
        assert_eq!(ordinal(&Widget::Image { hash: [0; 32] }), 3);
        assert_eq!(
            ordinal(&Widget::TextInput {
                initial: String::new(),
                placeholder: String::new(),
            }),
            4
        );
        assert_eq!(
            ordinal(&Widget::Checkbox {
                text: String::new(),
                checked: false,
            }),
            5
        );
        assert_eq!(
            ordinal(&Widget::Slider {
                min: 0,
                max: 0,
                value: 0
            }),
            6
        );
        assert_eq!(
            ordinal(&Widget::Dropdown {
                options: Vec::new(),
                selected: 0,
            }),
            7
        );
        assert_eq!(
            ordinal(&Widget::ItemSlot {
                view: String::new(),
                index: 0,
            }),
            8
        );
        assert_eq!(
            ordinal(&Widget::ItemGrid {
                view: String::new(),
                columns: 0,
                first: 0,
                count: 0,
            }),
            9
        );
        assert_eq!(ordinal(&Widget::Scroll), 10);
        assert_eq!(ordinal(&Widget::Spacer), 11);
        assert_eq!(ordinal(&Widget::Progress { permille: 0 }), 12);
        // Protocol v25, appended at the end rather than beside `ItemGrid`.
        assert_eq!(
            ordinal(&Widget::ShapeEditor {
                shape: 0,
                material: 0
            }),
            13
        );
    }

    /// Ordinals 25 onwards. See the call site for why they are not up there.
    fn pin_the_later_variants() {
        // Protocol v16, v17 and v18 — the input and audio tables.
        let actions = encode(&ServerMessage::ActionTable {
            actions: Vec::new(),
        })
        .expect("encode");
        assert_eq!(actions[0], 25);
        let sounds = encode(&ServerMessage::SoundTable { sounds: Vec::new() }).expect("encode");
        assert_eq!(sounds[0], 26);
        let play = encode(&ServerMessage::PlaySound {
            sound: String::new(),
            pos: [0.0; 3],
            radius: 1.0,
            gain: 1.0,
            entity: None,
        })
        .expect("encode");
        assert_eq!(play[0], 27);

        // Protocol v20, the dialogs, and currently the newest. Pinned on the
        // way in rather than after something displaces them: every one of the
        // three above went unpinned for several protocol versions, which is how
        // an append lands above one of them without a test noticing.
        let show = encode(&ServerMessage::ShowDialog {
            form: String::new(),
            tree: crate::ui::Tree { nodes: Vec::new() },
            compact: false,
        })
        .expect("encode");
        assert_eq!(show[0], 28);
        let update = encode(&ServerMessage::UpdateDialog {
            form: String::new(),
            tree: crate::ui::Tree { nodes: Vec::new() },
            compact: false,
        })
        .expect("encode");
        assert_eq!(update[0], 29);
        let close = encode(&ServerMessage::CloseDialog {
            form: String::new(),
        })
        .expect("encode");
        assert_eq!(close[0], 30);

        // Protocol v21. Unpinned when it landed, which is the habit this
        // comment block exists to break — it was the newest variant for one
        // task and would have been displaced silently by the next append.
        let view = encode(&ServerMessage::ViewUpdate {
            view: String::new(),
            slots: Vec::new(),
            held: None,
        })
        .expect("encode");
        assert_eq!(view[0], 31);

        // Protocol v22, the pushed HUD scripts.
        let scripts = encode(&ServerMessage::HudScripts {
            scripts: Vec::new(),
        })
        .expect("encode");
        assert_eq!(scripts[0], 32);

        // Protocol v23, the cue table and the loops, and currently the newest.
        let bindings = encode(&ServerMessage::SoundBindings {
            bindings: Vec::new(),
        })
        .expect("encode");
        assert_eq!(bindings[0], 33);
        let start = encode(&ServerMessage::StartLoop {
            id: String::new(),
            sound: String::new(),
            pos: [0.0; 3],
            radius: 1.0,
            gain: 1.0,
            everywhere: false,
        })
        .expect("encode");
        assert_eq!(start[0], 34);
        let stop = encode(&ServerMessage::StopLoop { id: String::new() }).expect("encode");
        assert_eq!(stop[0], 35);
        // Protocol v32. Appended at the END, which this test is what enforces:
        // the first version of it sat beside `EntityState`, where it belonged
        // by subject and shifted every ordinal after it by one.
        let armed = encode(&ServerMessage::EntityArmed {
            entities: Vec::new(),
        })
        .expect("encode");
        assert_eq!(armed[0], 36);
    }

    #[test]
    fn the_dialog_client_message_keeps_its_ordinal() {
        // The client half of protocol v20. `Action` was the last variant, so
        // this is what an even later append is most likely to displace.
        let action = encode(&ClientMessage::Action {
            id: String::new(),
            pressed: true,
        })
        .expect("encode");
        let event = encode(&ClientMessage::DialogEvent {
            form: String::new(),
            event: DialogEvent::Closed,
        })
        .expect("encode");
        assert_eq!(
            event[0],
            action[0] + 1,
            "DialogEvent must stay directly after Action"
        );
    }

    #[test]
    fn an_entity_message_with_a_non_finite_position_is_refused() {
        // Charter rule 14: a server is not trusted merely for being the server.
        // These positions reach the client's own frame arithmetic, so a NaN
        // propagates into every interpolation it takes part in — the same
        // hazard `PlayerState` has, arriving through a different message.
        let poison = ServerMessage::EntityState {
            tick: 1,
            entities: vec![EntityDelta {
                id: 1,
                chunk: ChunkPos::new(0, 0, 0),
                local: [f32::NAN, 0.0, 0.0],
                velocity: [0.0; 3],
                yaw: 0,
                pitch: 0,
                anim: 0,
            }],
        };
        assert!(validate_server_message(&poison).is_err());

        let infinite = ServerMessage::EntitySpawn {
            entities: vec![EntityDef {
                hands: [None, None],
                id: 1,
                chunk: ChunkPos::new(0, 0, 0),
                local: [0.0; 3],
                velocity: [f32::INFINITY, 0.0, 0.0],
                yaw: 0,
                pitch: 0,
                anim: 0,
                model: None,
                collider: None,
                nametag: None,
                item: None,
            }],
        };
        assert!(validate_server_message(&infinite).is_err());

        // And a collider, which is the field it would be easiest to forget:
        // a non-finite box would reach the client's culling rather than its
        // physics, and would make a mob either always or never drawn.
        let bad_box = ServerMessage::EntitySpawn {
            entities: vec![EntityDef {
                hands: [None, None],
                id: 1,
                chunk: ChunkPos::new(0, 0, 0),
                local: [0.0; 3],
                velocity: [0.0; 3],
                yaw: 0,
                pitch: 0,
                anim: 0,
                model: None,
                collider: Some([f32::NAN, 1.0]),
                nametag: None,
                item: None,
            }],
        };
        assert!(validate_server_message(&bad_box).is_err());
    }

    #[test]
    fn a_material_table_round_trips_with_and_without_textures() {
        // A material with no texture is normal, not exceptional: `engine:air`
        // has none and never will. Encoding it as an absent hash rather than a
        // sentinel value keeps "no texture" from colliding with a real one.
        let message = ServerMessage::MaterialTable {
            materials: vec![
                MaterialDef {
                    id: 0,
                    name: "engine:air".to_owned(),
                    texture: None,
                    placeable: true,
                    step_sound: None,
                },
                MaterialDef {
                    id: 2,
                    name: "core:white".to_owned(),
                    texture: Some([9u8; 32]),
                    placeable: true,
                    step_sound: None,
                },
            ],
        };
        let bytes = encode(&message).expect("encode");
        let decoded: ServerMessage = decode(&bytes).expect("decode");
        assert_eq!(decoded, message);
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
