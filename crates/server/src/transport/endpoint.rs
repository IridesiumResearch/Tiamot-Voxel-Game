// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The QUIC listener and per-connection handling.
//!
//! # Why one task per connection
//!
//! Each peer's control stream is an independent sequence of messages, and a
//! peer that stops reading must not stall anyone else. A task per connection is
//! the shape QUIC already has — connections are independent, streams within
//! them are independent — and it means a slow or hostile client blocks only
//! itself.
//!
//! # What this layer is allowed to decide
//!
//! Nothing. Every rule about who may do what lives in
//! [`tiamot_core::session`], which is a pure state machine and is tested as
//! one. This module moves bytes, calls [`Session::handle`], and sends whatever
//! it is told to send. If a decision appears here it is in the wrong place —
//! the test suite for it would need a socket, and it would not run on every
//! `cargo test`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use quinn::{Endpoint, ServerConfig};
use tiamot_core::identity::{Allowlist, PlayerUuid, SelfSovereign};
use tiamot_core::proto::Edit;
use tiamot_core::proto::{ClientMessage, ModEntry, ServerMessage};
use tiamot_core::session::{Action, IdentityRegistry, JoinContext, Session};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::frame;
use super::stream::Streamer;
use crate::cert::ServerCert;
use crate::sim::Control;

/// The QUIC ALPN protocol identifier.
///
/// Versioned separately from the wire protocol: this is negotiated during the
/// TLS handshake, before a single Tiamot message is exchanged, so a client
/// built for a different engine is refused at the transport layer rather than
/// getting far enough to send a `Hello`.
const ALPN: &[u8] = b"tiamot/1";

/// Starting or running the listener failed.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The socket could not be bound.
    #[error("could not bind {addr}")]
    Bind {
        /// The address we tried.
        addr: SocketAddr,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// TLS could not be configured from the server certificate.
    #[error("could not configure TLS from the server certificate")]
    Tls(#[source] Box<rustls::Error>),

    /// quinn rejected the configuration.
    #[error("could not configure the QUIC endpoint")]
    Config(#[source] Box<quinn::ConnectError>),
}

/// Everything a connection needs that is shared between all of them.
/// What one tick of digging took out of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bite {
    /// Sub-nodes removed this tick. `0` on a tick where nothing was due.
    pub chips: u32,
    /// Whether that finished the target.
    pub done: bool,
    /// Whether this was the FIRST bite out of this target.
    ///
    /// **What the mods are asked on, and only that.** `on_dig_complete` fires
    /// once per block, not once per sub-node: a mod playing a break sound from
    /// it made twenty-seven noises for one block, which is how this was
    /// reported. Asking on the first bite rather than the last also keeps the
    /// veto meaningful — a refusal arrives before anything has been removed.
    pub first: bool,
}

pub struct Shared {
    /// Who exists, and what they are called.
    ///
    /// A `tokio::sync::Mutex` rather than a `std` one because it is held across
    /// an `await` when a join writes bindings back to the database. A blocking
    /// mutex held across an await can deadlock the runtime.
    pub identities: Mutex<IdentityRegistry>,
    /// `BLAKE3` of this server's certificate, bound into every auth signature.
    pub cert_fingerprint: [u8; 32],
    /// The resolved mod set from Task 05.
    pub mods: Vec<ModEntry>,
    /// The mod set's fingerprint.
    pub mod_set_fingerprint: u64,
    /// The world's material table, in ascending world-id order.
    ///
    /// Built once at startup and never changed: registries freeze before the
    /// world opens (charter rule 9), so there is nothing that could change it.
    pub materials: Vec<tiamot_core::proto::MaterialDef>,
    /// The same tools, in the shape a client is sent on join.
    ///
    /// Separate from [`Shared::tools`], which is a lookup keyed by id for the
    /// simulation. This one is an ordered list of the wire type, because the
    /// order a client shows tools in should be a property of the mod set rather
    /// than of a map's iteration.
    pub tool_table: Vec<tiamot_core::proto::ToolDef>,
    /// Every fluid the mods registered, for the join tables.
    pub fluid_table: Vec<tiamot_core::proto::FluidDef>,
    /// Every action the mods registered, for the join tables.
    ///
    /// Charter rule 11: the engine owns bindings and a mod owns only the name,
    /// so this is the whole of what a mod gets to say about controls.
    pub action_table: Vec<tiamot_core::proto::ActionDef>,
    /// Every sound the mods registered, for the join tables.
    pub sound_table: Vec<tiamot_core::proto::SoundDef>,
    /// The HUD scripts the mods asked to push, in load order.
    pub hud_scripts: Vec<tiamot_core::proto::HudScriptDef>,
    /// Which sound each named event plays, in load order.
    pub sound_bindings: Vec<tiamot_core::proto::SoundBinding>,

    /// Ticks in a full day, or 0 if no mod registered a sky.
    pub sky_day_length: u32,

    /// The sky's colour keyframes, sorted by time.
    pub sky_keyframes: Vec<tiamot_core::proto::SkyFrame>,

    /// Where the clock stands in the day, in ticks since midnight.
    ///
    /// **The server owns this.** A client advancing its own clock would drift,
    /// and two players standing together would see different skies — which is
    /// the same reason the simulation owns everything else players share.
    pub time_of_day: std::sync::atomic::AtomicU64,
    /// Who is permitted to join.
    ///
    /// Behind a lock because RCON changes it at runtime: an operator adding
    /// someone to the allowlist should not have to restart the server, which
    /// would disconnect everyone already playing.
    pub allowlist: std::sync::RwLock<Allowlist>,
    /// Who may use admin powers, by UUID.
    ///
    /// Fixed at startup: an operator list that could change while the server
    /// ran would want an audit trail and a way to say who changed it, and
    /// neither exists yet. `server.toml` for a hosted world, and the client's
    /// own identity for the one it hosts itself.
    pub operators: std::collections::BTreeSet<PlayerUuid>,
    /// Maximum simultaneous players.
    pub max_players: u32,
    /// Where a new player starts.
    pub spawn: tiamot_core::BlockPos,
    /// How many players are connected right now.
    ///
    /// Incremented only once a connection reaches the world, and decremented on
    /// the way out **whatever** the exit path — see [`PlayerSlot`].
    pub players: AtomicU32,
    /// The simulation, for the authoritative tick number.
    pub control: Control,

    /// Edits waiting to be applied, oldest first.
    ///
    /// A plain queue rather than a channel: the simulation drains it once per
    /// tick from a synchronous thread, and a channel's async receive would need
    /// an executor there — which is exactly what the simulation must not have.
    ///
    /// Bounded by [`MAX_QUEUED_EDITS`]. Unbounded, a client could send edits
    /// faster than 20 Hz can apply them and grow this until the server died.
    pub edits: std::sync::Mutex<std::collections::VecDeque<(PlayerUuid, Edit)>>,

    /// Hits waiting for the tick to judge.
    ///
    /// Queued rather than acted on here for the reason every other action is:
    /// deciding whether a punch lands needs the world and every body in it, and
    /// both belong to the tick thread. Two connections punching at once would
    /// otherwise resolve in whichever order the OS woke them.
    pub punches: std::sync::Mutex<std::collections::VecDeque<(PlayerUuid, u64)>>,
    /// Mod-registered actions waiting to be handed to the mods.
    ///
    /// Queued on the connection thread and drained by the tick, like every
    /// other thing a client asks for: running a Lua hook on the network thread
    /// would put a mod's runtime inside a connection's read loop.
    pub actions: std::sync::Mutex<std::collections::VecDeque<(PlayerUuid, String, bool)>>,
    /// Chat waiting for the tick to offer to the mods.
    pub chat: std::sync::Mutex<std::collections::VecDeque<(PlayerUuid, String)>>,
    /// Dialog events waiting for the tick to hand to the owning mods.
    pub dialog_events: std::sync::Mutex<
        std::collections::VecDeque<(PlayerUuid, String, tiamot_core::proto::DialogEvent)>,
    >,

    /// World edits queued by the operator rather than by a player.
    ///
    /// **Not reachable from the network.** These come from
    /// [`crate::ServerHandle::seed_block`] — a test arranging a world, or an
    /// operator fixing one — and are applied with no inventory effect at all:
    /// nobody is credited for what they remove and nobody is charged for what
    /// they add, because nobody did it.
    ///
    /// Separate from [`Shared::edits`] for exactly that reason. That queue
    /// carries a `PlayerUuid` because a player is answerable for what it does;
    /// this one has no actor, and giving it a synthetic one would mean every
    /// consumer had to remember which uuids were real.
    pub seeds: std::sync::Mutex<std::collections::VecDeque<(String, Edit)>>,

    /// Placements waiting for the tick that will decide them.
    ///
    /// Separate from [`Shared::edits`] because they are a different kind of
    /// thing. An edit has already been decided and is waiting to be applied; a
    /// placement is a *request* that the tick may refuse, and it needs the
    /// player's identity to charge the material to, which is why it carries a
    /// uuid rather than being turned into an `Edit` on the connection task.
    pub placements: std::sync::Mutex<std::collections::VecDeque<PlacementRequest>>,

    /// Messages to fan out to every connected player.
    ///
    /// A `broadcast` channel: each connection holds a receiver and forwards
    /// what arrives. A slow client falls behind and its receiver lags, which
    /// costs that client messages rather than stalling the simulation — the
    /// right trade, because the alternative is one bad connection pausing the
    /// world for everyone.
    pub outbound: tokio::sync::broadcast::Sender<Broadcast>,

    /// Identities an admin has asked to disconnect.
    ///
    /// Separate from [`outbound`](Self::outbound) so a kick cannot be lost
    /// behind a backlog of world updates, and so a lagging receiver — which
    /// silently drops messages — cannot drop the one message that matters most.
    pub kicks: tokio::sync::broadcast::Sender<(PlayerUuid, String)>,

    /// Display names of everyone currently in world, for `status`.
    pub online: std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, String>>,

    /// Chunks connections have asked the simulation to encode.
    ///
    /// The simulation owns the world, so it is the only thing that may read a
    /// chunk. A connection asks; the next tick answers on the reply channel.
    pub chunk_requests: std::sync::Mutex<std::collections::VecDeque<ChunkRequest>>,

    /// How far each player can see.
    pub view_distance: tiamot_core::interest::ViewDistance,

    /// What each player is carrying, in units (charter rule 5).
    ///
    /// Server-authoritative. The client is told what it has; it never asserts
    /// it, because an inventory a client could edit is not an inventory.
    ///
    /// Digging credits it and placing debits it, so the 27-unit arithmetic of
    /// charter rule 5 is a round trip rather than a one-way accumulation.
    pub inventories:
        std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, tiamot_core::inventory::Slots>>,

    /// Players whose inventory changed and have not been told yet.
    pub inventory_dirty: std::sync::Mutex<std::collections::BTreeSet<PlayerUuid>>,

    /// Text waiting to be sent to one particular player.
    ///
    /// For things only the player who asked should see — chiefly why a
    /// placement was refused. Not [`Shared::outbound`], which goes to everyone:
    /// telling the whole server that somebody tried to build into a wall is
    /// noise at best.
    pub notices: std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, Vec<String>>>,

    /// Entity messages waiting for one player.
    ///
    /// **Per player, not broadcast**, because which entities somebody can see
    /// is their own interest set — sending everybody every mob in the world
    /// would make a populated world cost the square of the people watching it.
    ///
    /// Drained on that player's own connection task, like `notices`. Bounded,
    /// for the reason notices are: a client that reads slower than the
    /// simulation produces would otherwise grow this without limit. When the
    /// bound is hit the queue is CLEARED and the tracker reset, so the player
    /// is re-told everything from scratch — dropping the oldest would drop a
    /// spawn and leave an id the client can never draw.
    pub entity_messages:
        std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, Vec<ServerMessage>>>,

    /// What each mod wants each player's HUD to show, and whether that player
    /// has been told yet.
    ///
    /// **A map, not a queue**, and that is the whole reason this is not sent
    /// through `entity_messages`: these are the latest STATE rather than a
    /// stream of events, so a mod that sets a value sixty times between two
    /// network passes costs one message and cannot overflow anything. A queue
    /// would have to bound itself and then decide which health bar to drop.
    pub hud_values: std::sync::Mutex<
        std::collections::BTreeMap<PlayerUuid, std::collections::BTreeMap<String, HudSlot>>,
    >,

    /// Every distributable file the loaded mods supply, by hash.
    ///
    /// Built once at startup and immutable thereafter. Rebuilding it while the
    /// server runs would mean a file edited mid-session is served under its old
    /// hash — the one thing content addressing exists to make impossible.
    pub content: tiamot_core::content::ContentIndex,

    /// Where every player's body is, and what they have asked to do next.
    ///
    /// Written by the simulation thread and read by the connection tasks, the
    /// same way edits go the other direction. The bodies live here rather than
    /// in the connection task because charter rule 2 allows exactly one
    /// simulation: a player's physics must run on the tick, in a fixed order,
    /// against a world only that thread may read.
    /// Every registered tool, by qualified id.
    ///
    /// Built once at startup from the frozen registries (charter rule 9), so
    /// there is nothing that could change it while the server runs.
    pub tools: std::collections::BTreeMap<String, tiamot_core::script::Tool>,

    /// The extra inventory views mods asked for, in id order.
    ///
    /// Built once at startup from the frozen registries (charter rule 9), like
    /// the tools beside it — every player gets the same set, and a view
    /// appearing halfway through a session would be a place a client had never
    /// been told about.
    pub views: Vec<tiamot_core::inventory::ViewDef>,

    /// The tool a player digs with when they have selected nothing.
    ///
    /// `None` when the loaded mods registered no default — and then nobody can
    /// dig at all, which is deliberate. See [`Shared::resolve_tool`].
    pub default_tool: Option<String>,

    /// How each material resists a tool: its bare-handed seconds, and how
    /// strongly it imposes that on a block it is only part of.
    ///
    /// Keyed by WORLD material id, because that is what a chunk holds. A
    /// material with no entry gets the engine default rather than being
    /// unbreakable — see `BlockRules::DEFAULT_HARDNESS`.
    pub hardness: std::collections::BTreeMap<tiamot_core::MaterialId, tiamot_core::dig::Resistance>,

    /// Which hotbar slot each player is holding.
    ///
    /// **The client's own UI state, kept here so a mod can ask.** The hotbar
    /// keys and the wheel never reached the server before — nothing but a
    /// `Place` said what was in hand — which is enough for building and not
    /// enough for anything a mod wants to do with an item that is not a block.
    /// Absent means slot zero, which is where a client starts.
    pub held_slot: std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, usize>>,

    /// Materials that may NOT be put in the world: the items.
    ///
    /// **The world palette must never contain one.** Everything a player can
    /// carry shares one id space — see `register_item` — and this is the set
    /// that is a sword rather than a stone. Built once from the frozen
    /// registries, like the hardness beside it.
    pub items: std::collections::BTreeSet<tiamot_core::MaterialId>,

    /// Named `bodies` rather than `players` because `players` is already the
    /// connected *count* on this struct, and two fields whose names differ only
    /// by what they happen to hold is how the wrong one gets locked.
    pub bodies: Arc<PlayerBodies>,
}

/// One mod's HUD values for one player, and whether they have been sent.
#[derive(Debug, Clone, Default)]
pub struct HudSlot {
    /// What the mod last asked for.
    pub values: tiamot_core::hud::Values,
    /// Whether the player has been told this version.
    pub sent: bool,
}

/// The connected players' authoritative bodies, behind the tick's lock.
///
/// **Named and shared** because a mod moving a player has to write THESE — the
/// mirrors in the entity store are copies the tick overwrites, so a position
/// written to one does nothing, silently. See `ent::Access::move_player`.
pub type PlayerBodies = std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, PlayerSim>>;

/// One player's simulated body and the inputs waiting to move it.
#[derive(Debug, Clone)]
pub struct PlayerSim {
    /// Which simulation space this player is in.
    ///
    /// **Authoritative, and the one place it is decided.** Everything a player
    /// is shown or collides with is scoped by it: the terrain their body steps
    /// against, the chunks streamed to them, the entities they are told about.
    /// A player is in exactly one domain, and moving between them is a
    /// deliberate handoff rather than a walk (`core::domain`).
    pub domain: String,
    /// The chunk the body's local coordinates are relative to (charter rule 7).
    pub origin: tiamot_core::ChunkPos,
    /// Position, velocity and ground contact, in sub-node cells.
    pub body: tiamot_core::phys::Body,
    /// Inputs filed under the tick that will apply them.
    pub inputs: tiamot_core::phys::InputQueue,
    /// What this player is breaking, if anything.
    pub dig: Option<tiamot_core::dig::Dig>,
    /// The tool they say they are holding, or `None` for a bare hand.
    pub tool: Option<String>,
    /// Where they are looking, in turns, as the wire carries it.
    ///
    /// **Presentation, and the simulation never reads it.** Movement arrives
    /// already rotated into world space precisely so the tick needs no
    /// trigonometry (charter rule 4), so this exists only to point the drawn
    /// body: without it every other player faces north for ever.
    pub look: [f32; 2],
    /// What a client should draw this body doing.
    ///
    /// Set by the tick from the input that moved it. **Server-side animation is
    /// state tags only** — the server says "walking" and the client picks a clip
    /// and advances its own time, which is what keeps skeletal animation out of
    /// the deterministic simulation entirely (charter rule 4 does not reach
    /// presentation, and interpolating a joint is transcendental work).
    pub anim: tiamot_core::ent::AnimTag,
    /// The tick a swing was thrown on, or zero.
    ///
    /// A punch has no duration of its own — it is one message — so the body has
    /// to be told to keep swinging for a moment or the tag is gone before the
    /// next update is sent and nobody ever sees it. See `SWING_TICKS`.
    pub swung_on: u64,
    /// The tick the current `look` came with.
    ///
    /// Inputs are sent three times over for redundancy, so they arrive out of
    /// order routinely. Without this an older duplicate overwrites a newer
    /// look and heads twitch backwards.
    pub look_tick: u64,
}

impl PlayerSim {
    /// A body standing at a block position, at rest.
    #[must_use]
    pub fn spawned_at(spawn: tiamot_core::BlockPos, tick: u64) -> Self {
        let origin = spawn.chunk();
        let corner = tiamot_core::BlockPos::from_chunk_corner(origin);
        let cells = tiamot_core::SUBNODES_PER_AXIS as f32;
        // Centred on the spawn block rather than on its corner, so a player
        // does not start half inside the wall next to it.
        let local = [
            (spawn.x - corner.x) as f32 * cells + cells / 2.0,
            (spawn.y - corner.y) as f32 * cells,
            (spawn.z - corner.z) as f32 * cells + cells / 2.0,
        ];
        Self {
            // A player joins the overworld. Which domain a spawn belongs to is
            // a mod's decision, made afterwards with a transfer — the engine
            // has no opinion about where anybody starts (charter rule 1).
            domain: tiamot_core::domain::OVERWORLD.to_owned(),
            origin,
            body: tiamot_core::phys::Body::at(local),
            inputs: tiamot_core::phys::InputQueue::new(tick),
            dig: None,
            tool: None,
            anim: tiamot_core::ent::AnimTag::IDLE,
            swung_on: 0,
            look: [0.0; 2],
            look_tick: 0,
        }
    }
}

/// The tag a client should draw a body with, from what it was asked to do and
/// what it ended up doing.
///
/// Intent first, then speed: a player holding sneak is sneaking whether or not
/// they are moving, because a crouch is a posture rather than a gait. Below the
/// idle threshold everything else is standing still — a body drifting to a stop
/// under friction should not keep walking on the spot for the three ticks it
/// takes.
/// How long a punch keeps the arm moving, in ticks.
///
/// Four, which is the length of the rig's swing clip at twenty ticks a second.
/// A punch is one message and has no duration of its own, so without this the
/// tag would be gone before the next entity update went out and nobody would
/// ever see an arm move.
pub const SWING_TICKS: u64 = 8;

#[must_use]
pub fn anim_from_motion(
    intent: tiamot_core::phys::Intent,
    body: &tiamot_core::phys::Body,
    digging: bool,
) -> tiamot_core::ent::AnimTag {
    use tiamot_core::ent::AnimTag;
    use tiamot_core::phys::Gait;

    // Swinging beats everything, including standing still: the arm is the part
    // anyone is looking at. This is the only reason the server knows about a
    // dig here at all — it already tracks one to decide when the block breaks,
    // and a body that mines without moving its arms reads as broken.
    if digging {
        return AnimTag::SWING;
    }

    if matches!(intent.gait, Gait::Sneak) {
        return AnimTag::SNEAK;
    }

    // Squared, so the comparison needs no root.
    let [vx, _, vz] = body.velocity;
    let speed = vx * vx + vz * vz;
    // A tenth of a walk, which is slower than anything a player can hold and
    // faster than the tail of the friction curve.
    let idle = tiamot_core::phys::Tuning::DEFAULT.walk_speed * 0.1;
    if speed < idle * idle {
        return AnimTag::IDLE;
    }
    // Faster than a walk can go means they are sprinting. Reading the gait
    // instead would show a run the moment the key went down, before the body
    // had accelerated into one.
    let walk = tiamot_core::phys::Tuning::DEFAULT.walk_speed;
    if speed > walk * walk {
        AnimTag::RUN
    } else {
        AnimTag::WALK
    }
}

/// Turns a wire input into something the physics can step.
///
/// The movement vector is already world-space (see
/// [`ClientMessage::PlayerInput`]), so there is no rotation here and therefore
/// no trigonometry in the simulation path — charter rule 4.
#[must_use]
pub fn intent_from_wire(
    movement: [f32; 3],
    actions: u32,
    may_fly: bool,
) -> tiamot_core::phys::Intent {
    use tiamot_core::phys::Gait;
    use tiamot_core::proto::actions as bits;

    // Sneak wins over sprint. A client asserting both is buggy rather than
    // expressing a preference, and the edge guard is the safer reading.
    let gait = if actions & bits::SNEAK != 0 {
        Gait::Sneak
    } else if actions & bits::SPRINT != 0 {
        Gait::Sprint
    } else {
        Gait::Walk
    };

    tiamot_core::phys::Intent {
        walk: [movement[0], movement[2]],
        jump: actions & bits::JUMP != 0,
        gait,
        // **Asked AND allowed.** Every client can set the bit; only a player
        // the server has made an operator gets it honoured. Charter rule 2: a
        // client says what it wants and the server decides what happens, which
        // is the same rule that refuses a placement into occupied space.
        fly: may_fly && actions & bits::FLY != 0,
    }
}

/// A connection asking the simulation for a chunk.
pub struct ChunkRequest {
    /// Which domain's chunk.
    ///
    /// A position on its own does not name a chunk: every domain has one at
    /// each coordinate. Answering from the wrong one hands a player terrain
    /// from somewhere else, in a place they are standing.
    pub domain: String,
    /// Which chunk.
    pub pos: tiamot_core::ChunkPos,
    /// The summary level wanted, or `None` for the chunk itself.
    ///
    /// One request type for both, because they are the same question asked at
    /// different resolutions and they share the budget, the queue and the
    /// in-flight accounting. A second queue would have let a horizon starve a
    /// player's own neighbourhood, or the reverse.
    pub level: Option<u8>,
    /// Where to send the encoded blob.
    ///
    /// A oneshot, so a connection that goes away between asking and being
    /// answered simply drops the receiver and the simulation's send fails
    /// harmlessly. A shared queue of replies would need the simulation to know
    /// which connections still exist.
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
}

/// One broadcast, and who it is for.
///
/// The domain is carried rather than resolved at the sender: who is in which
/// space changes every tick, and a sender that wanted to know would have to
/// take the body lock to send a message.
#[derive(Debug, Clone)]
pub struct Broadcast {
    /// The domain this is for, or `None` for everyone in the world.
    ///
    /// `None` is right for anything that is not about a place — chat, the
    /// tables sent at join, a player list. Anything about the world says which
    /// world.
    pub only: Option<String>,
    /// What to send.
    pub message: ServerMessage,
}

/// A player asking to place material.
///
/// Deliberately not an [`Edit`]: an edit says what the world will become, and
/// this says only what somebody asked for. What it becomes — a `Uniform` block,
/// a `Partial` one, or nothing at all — is decided on the tick thread against
/// the world and every body in it.
#[derive(Debug, Clone)]
pub struct PlacementRequest {
    /// Who asked, and who pays for it.
    pub actor: PlayerUuid,
    /// The cell they want filled.
    pub target: tiamot_core::SubNodePos,
    /// The material they claim to be holding.
    pub material: u16,
    /// The cut they claim to be holding, or `0` for loose material.
    ///
    /// A CLAIM, like the material: the server matches it against what the
    /// player actually has and places nothing if they have no such stack.
    pub shape: u32,
    /// The surface it was placed against, for turning a cut to face the
    /// player. All zeroes means no preference.
    pub face: [i8; 3],
    /// Which stack they claim to be spending, when a mod says two of the same
    /// material and cut are different things.
    pub detail: Option<String>,
}

/// How many chunk requests the simulation serves per tick, across all players.
///
/// This is the shared budget, not a per-client one: the work is encoding, it
/// happens on the single simulation thread, and it comes out of the same 50 ms
/// every other system spends. Serving requests oldest-first shares it fairly
/// without needing per-client accounting.
pub const CHUNKS_PER_TICK: usize = 16;

/// How many chunks one connection may have outstanding at once.
///
/// Caps a single player's share of the queue so one client joining cannot
/// starve everyone else's updates while its 1800-chunk interest set drains.
pub const CHUNKS_IN_FLIGHT_PER_CLIENT: usize = 4;

/// How many horizon summaries one connection may have outstanding at once.
///
/// **Its own allowance rather than what the chunks leave**, which is what it
/// was: a client streaming a large view has thousands of chunks to fetch, takes
/// the whole in-flight budget every pass for as long as that lasts, and the
/// horizon never begins. The ground under somebody's feet still goes first —
/// this is one slot beside four, not equal footing.
pub const SUMMARIES_IN_FLIGHT_PER_CLIENT: usize = 1;

/// How many of a tick's chunk budget the horizon may take.
///
/// A quarter. The horizon is scenery, it is allowed to arrive late, and the
/// ground under somebody's feet is not — see [`Shared::take_chunk_requests`],
/// which is where the split is enforced. Small enough that a fresh horizon
/// fills over a couple of minutes rather than instantly, which is the trade the
/// tick budget is worth.
pub const SUMMARIES_PER_TICK: usize = CHUNKS_PER_TICK / 4;

/// Most chunk requests that may be queued before new ones are refused.
use tiamot_core::inventory::{PLAYER_HOTBAR_SLOTS, PLAYER_MAIN};

pub const MAX_QUEUED_CHUNK_REQUESTS: usize = 512;

/// How many unapplied edits may be queued before new ones are dropped.
///
/// At 20 Hz with 50 players this is several seconds of backlog — far more than
/// a healthy server ever holds, and small enough that a client flooding edits
/// cannot grow it into a memory problem.
pub const MAX_QUEUED_EDITS: usize = 4096;

/// How many unread entity messages one player may accumulate.
///
/// Three seconds of a busy tick at twenty a second, which is far more slack
/// than a healthy connection needs and far less than an unbounded queue. Past
/// it the queue is cleared and the player re-told from scratch.
const MAX_QUEUED_ENTITY_MESSAGES: usize = 60;

/// How many unread notices one player may accumulate.
///
/// Small on purpose. These are answers to things the player just did, so a
/// backlog of them is already stale by the time it would be read; dropping the
/// surplus is better than delivering a minute-old explanation.
pub const MAX_QUEUED_NOTICES: usize = 16;

impl Shared {
    /// The current tick, for stamping outbound messages.
    fn tick(&self) -> u64 {
        self.control.tick()
    }

    /// Queues an edit for the simulation to apply.
    ///
    /// Returns `false` if the queue is full, in which case the edit is dropped.
    /// Dropping is deliberate: blocking here would let one client's flood stall
    /// the connection task, and growing without bound would let it exhaust
    /// memory.
    /// Queues a placement request for the next tick.
    ///
    /// Returns whether it was accepted. Bounded like the edit queue and for the
    /// same reason: a client can send these faster than 20 Hz can decide them.
    pub fn queue_placement(
        &self,
        actor: PlayerUuid,
        target: tiamot_core::SubNodePos,
        material: u16,
        shape: u32,
        face: [i8; 3],
        detail: Option<String>,
    ) -> bool {
        let Ok(mut queue) = self.placements.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back(PlacementRequest {
            actor,
            target,
            material,
            shape,
            face,
            detail,
        });
        true
    }

    /// Queues an operator edit, bypassing every player-facing rule.
    ///
    /// Returns whether it was accepted; the queue is bounded like the others.
    pub fn queue_seed(&self, domain: &str, edit: Edit) -> bool {
        let Ok(mut queue) = self.seeds.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((domain.to_owned(), edit));
        true
    }

    /// Takes every queued operator edit, leaving the queue empty.
    #[must_use]
    pub fn drain_seeds(&self) -> Vec<(String, Edit)> {
        self.seeds
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Takes every queued placement, leaving the queue empty.
    #[must_use]
    pub fn drain_placements(&self) -> Vec<PlacementRequest> {
        self.placements
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Where a player's eyes are, for the reach check.
    ///
    /// The chunk origin and the eye offset within it (charter rule 7), which is
    /// the frame `place::within_reach` expects.
    #[must_use]
    pub fn player_eye(&self, uuid: &PlayerUuid) -> Option<(tiamot_core::ChunkPos, [f32; 3])> {
        let bodies = self.bodies.lock().ok()?;
        let player = bodies.get(uuid)?;
        Some((player.origin, player.body.eye()))
    }

    /// Every player's chunk origin and body box, for the placement check.
    ///
    /// A snapshot, like [`Shared::digs_in_progress`]: the caller writes the
    /// world between reading this and acting on it, and holding the lock across
    /// that puts a connection task's contention inside the tick.
    #[must_use]
    pub fn body_boxes(&self) -> Vec<(tiamot_core::ChunkPos, tiamot_core::phys::Aabb)> {
        self.bodies
            .lock()
            .map(|bodies| {
                bodies
                    .values()
                    .map(|player| (player.origin, player.body.aabb()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Takes up to `units` of a material from a player, returning how many.
    ///
    /// Marks the inventory dirty only when something was actually taken, so a
    /// refused placement does not cost a sync message.
    pub fn debit(
        &self,
        uuid: &PlayerUuid,
        material: tiamot_core::MaterialId,
        shape: Option<tiamot_core::inventory::Shape>,
        detail: Option<&str>,
        units: u32,
    ) -> u32 {
        let Ok(mut inventories) = self.inventories.lock() else {
            return 0;
        };
        let Some(held) = inventories.get_mut(uuid) else {
            return 0;
        };
        let taken = held.take(PLAYER_MAIN, material, shape, detail, units);
        if taken > 0
            && let Ok(mut dirty) = self.inventory_dirty.lock()
        {
            dirty.insert(*uuid);
        }
        taken
    }

    pub fn queue_edit(&self, actor: PlayerUuid, edit: Edit) -> bool {
        let Ok(mut queue) = self.edits.lock() else {
            // The mutex is poisoned, which means a previous holder panicked
            // while editing. Refusing new work is the honest response.
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((actor, edit));
        true
    }

    /// Starts simulating a player, at spawn.
    pub fn add_player(&self, uuid: PlayerUuid, spawn: tiamot_core::BlockPos) {
        if let Ok(mut bodies) = self.bodies.lock() {
            bodies.insert(uuid, PlayerSim::spawned_at(spawn, self.tick()));
        }
    }

    /// Stops simulating a player.
    pub fn remove_player(&self, uuid: &PlayerUuid) {
        if let Ok(mut bodies) = self.bodies.lock() {
            bodies.remove(uuid);
        }
        // **And what their HUD was being told**, or a server nobody restarts
        // accumulates a set of values per player who has ever joined. A mod
        // sets them again when they come back, because it is the mod that
        // knows what they should say.
        self.forget_hud_values(uuid);
    }

    /// Files an input against the tick it belongs to.
    ///
    /// Returns whether it was kept; see [`tiamot_core::phys::InputQueue::offer`]
    /// for why a refusal is ordinary traffic rather than an error.
    pub fn queue_input(
        &self,
        uuid: &PlayerUuid,
        tick: u64,
        intent: tiamot_core::phys::Intent,
        look: [f32; 2],
    ) -> bool {
        let Ok(mut bodies) = self.bodies.lock() else {
            return false;
        };
        let Some(player) = bodies.get_mut(uuid) else {
            return false;
        };
        // Recorded even when the intent is refused as a duplicate: the two
        // answer different questions. A repeat of an already-simulated tick
        // must not move anybody again, and it still carries where they were
        // looking — which is presentation, and where they are looking NOW is
        // closer to the truth than where they were looking when they joined.
        if tick >= player.look_tick {
            player.look = look;
            player.look_tick = tick;
        }
        player.inputs.offer(tick, intent)
    }

    /// Starts, re-aims, or stops a player's dig.
    ///
    /// Re-aiming discards progress — see [`tiamot_core::dig::Dig::retarget`]
    /// for why it must not bank.
    pub fn set_dig(&self, uuid: &PlayerUuid, target: Option<tiamot_core::SubNodePos>) {
        let Ok(mut bodies) = self.bodies.lock() else {
            return;
        };
        let Some(player) = bodies.get_mut(uuid) else {
            return;
        };
        match target {
            None => player.dig = None,
            Some(target) => {
                // No tool, no dig. A mod says what a player digs with, so a
                // world with no tools mod is one nobody can dig in.
                let Some(brush) = self
                    .resolve_tool(player.tool.as_deref())
                    .map(|tool| tool.brush)
                else {
                    player.dig = None;
                    return;
                };
                match player.dig.as_mut() {
                    Some(dig) => {
                        dig.retarget(target, brush);
                    }
                    None => player.dig = Some(tiamot_core::dig::Dig::start(target, brush)),
                }
            }
        }
    }

    /// Records which tool a player says they are holding.
    ///
    /// An id the loaded mods did not register becomes a bare hand rather than a
    /// refusal: a client that is out of date with the server's mod set is
    /// wrong, not hostile, and digging slowly is a better answer than a
    /// disconnect.
    pub fn select_tool(&self, uuid: &PlayerUuid, tool: Option<String>) {
        let known = tool.filter(|id| self.tools.contains_key(id));
        if let Ok(mut bodies) = self.bodies.lock()
            && let Some(player) = bodies.get_mut(uuid)
        {
            // The brush changes, so anything in progress starts over.
            player.dig = None;
            player.tool = known;
        }
    }

    /// The brush a player's placements write with.
    ///
    /// The same tool that decides what they dig decides what they build, which
    /// is what makes sculpting reversible: a chisel takes one cell out and puts
    /// one cell back, into the cell that was aimed at.
    ///
    /// **Falls back to [`Brush::Block`] where digging falls back to refusing.**
    /// The two are not the same question. A world whose mods registered no
    /// tools is one nobody can dig in — the engine has no bare hand of its own
    /// (charter rule 1) — but placing is putting down material you already
    /// hold, and there is no rule to be missing. Refusing it would mean a mod
    /// set could strand a player's inventory with no way to spend it.
    #[must_use]
    pub fn place_brush(&self, uuid: &PlayerUuid) -> tiamot_core::dig::Brush {
        let Ok(bodies) = self.bodies.lock() else {
            return tiamot_core::dig::Brush::Block;
        };
        bodies
            .get(uuid)
            .and_then(|player| self.resolve_tool(player.tool.as_deref()))
            .map_or(tiamot_core::dig::Brush::Block, |tool| tool.brush)
    }

    /// Advances the clock by one tick and returns where the day now stands.
    ///
    /// `0.0..1.0`, midnight to midnight. A world with no sky mod has no day
    /// length, so its clock never moves and this is always zero — which the
    /// client renders as a sky that does not change.
    pub fn advance_day(&self) -> f32 {
        use std::sync::atomic::Ordering;
        if self.sky_day_length == 0 {
            return 0.0;
        }
        let length = u64::from(self.sky_day_length);
        // Wrapped in the counter rather than allowed to grow: a server up for a
        // year would otherwise accumulate a number whose `as f32` conversion
        // has lost the precision this divides by.
        let next = (self.time_of_day.load(Ordering::Relaxed) + 1) % length;
        self.time_of_day.store(next, Ordering::Relaxed);
        next as f32 / length as f32
    }

    /// Where the day stands now, without advancing it.
    #[must_use]
    pub fn day_fraction(&self) -> f32 {
        use std::sync::atomic::Ordering;
        if self.sky_day_length == 0 {
            return 0.0;
        }
        self.time_of_day.load(Ordering::Relaxed) as f32 / f64::from(self.sky_day_length) as f32
    }

    /// The tool a player is actually using: what they chose, or the mod's
    /// default, or nothing.
    ///
    /// `None` means they cannot dig, and that is not a failure path — it is a
    /// world whose mods registered no tools. The engine knows how to *count* a
    /// dig and nothing about what a player breaks things with, so it has no
    /// opinion of its own to fall back on (charter rule 1).
    #[must_use]
    pub fn resolve_tool(&self, chosen: Option<&str>) -> Option<&tiamot_core::script::Tool> {
        chosen
            .and_then(|id| self.tools.get(id))
            .or_else(|| self.tools.get(self.default_tool.as_deref()?))
    }

    /// How a material resists a tool, defaulted for anything unregistered.
    #[must_use]
    pub fn resistance_of(&self, material: tiamot_core::MaterialId) -> tiamot_core::dig::Resistance {
        self.hardness.get(&material).copied().unwrap_or_else(|| {
            tiamot_core::dig::Resistance::new(tiamot_core::script::BlockRules::DEFAULT_HARDNESS)
        })
    }

    /// How long a material takes to break with a bare hand, in seconds.
    #[must_use]
    pub fn hardness_of(&self, material: tiamot_core::MaterialId) -> f32 {
        self.resistance_of(material).hardness
    }

    /// How long one sub-node cell of a material takes to break, in seconds.
    ///
    /// A thirteen-and-a-half-th of the whole-block figure, so chiselling a block
    /// out cell by cell costs twice what smashing it does. See
    /// `tiamot_core::dig::hardness`.
    #[must_use]
    pub fn subnode_hardness_of(&self, material: tiamot_core::MaterialId) -> f32 {
        tiamot_core::dig::subnode_hardness(material, |id| self.resistance_of(id))
    }

    /// How long a whole block takes to break, blended over what it is made of.
    ///
    /// The blend lives in core (`dig::hardness`) and the material table lives
    /// here, so this is the seam between them and the only place the two meet.
    #[must_use]
    pub fn block_hardness_of(&self, view: &tiamot_core::block::BlockView<'_>) -> f32 {
        tiamot_core::dig::block_hardness(view, |id| self.resistance_of(id))
    }

    /// Every dig currently running, as `(player, target, brush)`.
    ///
    /// A snapshot rather than a borrow: the caller reads and writes the world
    /// between entries, and holding the lock across that would put a
    /// connection task's contention inside the tick.
    #[must_use]
    pub fn digs_in_progress(
        &self,
    ) -> Vec<(PlayerUuid, tiamot_core::SubNodePos, tiamot_core::dig::Brush)> {
        let Ok(bodies) = self.bodies.lock() else {
            return Vec::new();
        };
        bodies
            .iter()
            .filter_map(|(uuid, player)| {
                let dig = player.dig.as_ref()?;
                Some((*uuid, dig.target(), dig.brush()))
            })
            .collect()
    }

    /// Advances a player's dig by one tick.
    ///
    /// Returns whether it completed, or `None` if they are no longer digging —
    /// they may have cancelled between the snapshot and here.
    /// Advances a player's dig by one tick, returning how many sub-nodes came
    /// off and whether that finished the target.
    ///
    /// `cells` is how many sub-nodes the target still has — only the caller can
    /// see the block. A `SubNode` brush passes 1 and behaves exactly as it
    /// always did: nothing until the timer, then the one cell.
    pub fn advance_dig(&self, uuid: &PlayerUuid, hardness: f32, cells: u32) -> Option<Bite> {
        let mut bodies = self.bodies.lock().ok()?;
        let player = bodies.get_mut(uuid)?;
        let speed = self
            .resolve_tool(player.tool.as_deref())
            .map(|tool| tool.speed_multiplier)?;
        let dig = player.dig.as_mut()?;
        let before = dig.chipped();
        let chips = dig.advance(hardness, speed, cells);
        Some(Bite {
            chips,
            done: dig.is_done(),
            first: before == 0 && chips > 0,
        })
    }

    /// A player's dig progress, for the crack overlay.
    #[must_use]
    pub fn dig_progress(&self, uuid: &PlayerUuid) -> Option<ServerMessage> {
        let bodies = self.bodies.lock().ok()?;
        let dig = bodies.get(uuid)?.dig.as_ref()?;
        Some(ServerMessage::DigProgress {
            target: dig.target(),
            progress: dig.progress(),
        })
    }

    /// A snapshot of where the simulation has a player, for sending on.
    #[must_use]
    pub fn player_state(&self, uuid: &PlayerUuid) -> Option<ServerMessage> {
        let bodies = self.bodies.lock().ok()?;
        let player = bodies.get(uuid)?;
        Some(ServerMessage::PlayerState {
            last_processed_input: player.inputs.last_applied(),
            chunk: player.origin,
            local: player.body.position,
            velocity: player.body.velocity,
            on_ground: player.body.on_ground,
        })
    }

    /// The chunk a player is standing in, for recentring their interest set.
    #[must_use]
    pub fn player_chunk(&self, uuid: &PlayerUuid) -> Option<tiamot_core::ChunkPos> {
        let bodies = self.bodies.lock().ok()?;
        bodies.get(uuid).map(|player| player.origin)
    }

    /// How many connected players are in a domain.
    ///
    /// The other half of what `domain::Registry::destroy` asks: a mob inside is
    /// `Population::occupants`, and a PLAYER is here, because a player's
    /// authoritative body is not in the entity store — the mirror there is a
    /// copy. Counting only the mirrors would let a domain be destroyed out from
    /// under somebody standing in it in the moment before the tick refreshed
    /// them.
    #[must_use]
    pub fn players_in(&self, domain: &str) -> usize {
        self.bodies.lock().map_or(0, |bodies| {
            bodies
                .values()
                .filter(|player| player.domain == domain)
                .count()
        })
    }

    /// Which domain a player is in.
    ///
    /// The overworld for anybody the server has no body for, which is the
    /// honest answer for a request that arrived as its sender disconnected.
    #[must_use]
    pub fn player_domain(&self, uuid: &PlayerUuid) -> String {
        self.bodies
            .lock()
            .ok()
            .and_then(|bodies| bodies.get(uuid).map(|player| player.domain.clone()))
            .unwrap_or_else(|| tiamot_core::domain::OVERWORLD.to_owned())
    }

    /// Which domain a player is in, and where they are in it.
    ///
    /// Read together under one lock, because a connection acts on the pair: a
    /// domain read a moment before the position it is paired with would
    /// re-centre the new domain's stream on the old domain's chunk.
    #[must_use]
    pub fn player_place(&self, uuid: &PlayerUuid) -> Option<(String, tiamot_core::ChunkPos)> {
        let bodies = self.bodies.lock().ok()?;
        bodies
            .get(uuid)
            .map(|player| (player.domain.clone(), player.origin))
    }

    /// Takes everything queued, leaving the queue empty.
    ///
    /// Called once per tick by the simulation.
    #[must_use]
    pub fn drain_edits(&self) -> Vec<(PlayerUuid, Edit)> {
        self.edits
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Records a punch for the next tick to judge.
    ///
    /// Bounded like the edit queue: a client that spams this cannot make the
    /// server hold an unbounded list of them.
    pub fn queue_punch(&self, actor: PlayerUuid, entity: u64) -> bool {
        let Ok(mut queue) = self.punches.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((actor, entity));
        true
    }

    /// Queues a mod-registered action, if it is one this server has.
    ///
    /// **Two limits, both deliberate and both documented here** because charter
    /// rule 14 says a client is not trusted:
    ///
    /// 1. **The id must be registered.** A client that could name any string
    ///    would reach the Lua dispatcher with it, and every unknown id would
    ///    cost a hook run and a table. Checked against the same table the
    ///    client was sent, so there is nothing to guess.
    /// 2. **The queue is capped** at `MAX_QUEUED_EDITS`, shared with edits and
    ///    punches. A client spamming a key spends its own tick's worth of slots
    ///    and is dropped after that, rather than growing the server's memory
    ///    until the tick catches up. Dropping is right: an action is an EVENT,
    ///    and a player who genuinely pressed a key that many times in 50 ms did
    ///    not.
    ///
    /// Returns whether it was queued, which the caller does not currently use —
    /// a refused action is not worth a disconnection, because the ordinary
    /// cause is a mod that unregistered between one tick and the next.
    pub fn queue_action(&self, actor: PlayerUuid, id: String, pressed: bool) -> bool {
        if !self.action_table.iter().any(|action| action.id == id) {
            return false;
        }
        let Ok(mut queue) = self.actions.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((actor, id, pressed));
        true
    }

    /// Queues a line of chat for the tick to offer to the mods.
    ///
    /// Bounded like the others: a client typing as fast as it can must not make
    /// the server hold an unbounded list.
    pub fn queue_chat(&self, actor: PlayerUuid, text: String) -> bool {
        let Ok(mut queue) = self.chat.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((actor, text));
        true
    }

    /// Takes the chat waiting to be offered to the mods.
    pub fn drain_chat(&self) -> Vec<(PlayerUuid, String)> {
        self.chat
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Queues a dialog event for the tick to hand to the owning mod.
    ///
    /// **Queued rather than delivered here**, the same as every other thing a
    /// client asks for: mods run on the tick thread, in a fixed order, and two
    /// connection tasks calling into the VM at once would resolve in whichever
    /// order the OS woke them.
    ///
    /// Bounded like the action queue. A client clicking a button as fast as it
    /// can must not be able to make the server hold an unbounded list.
    pub fn queue_dialog_event(
        &self,
        actor: PlayerUuid,
        form: String,
        event: tiamot_core::proto::DialogEvent,
    ) -> bool {
        let Ok(mut queue) = self.dialog_events.lock() else {
            return false;
        };
        if queue.len() >= MAX_QUEUED_EDITS {
            return false;
        }
        queue.push_back((actor, form, event));
        true
    }

    /// Takes the dialog events waiting to be handed to the mods.
    pub fn drain_dialog_events(
        &self,
    ) -> Vec<(PlayerUuid, String, tiamot_core::proto::DialogEvent)> {
        self.dialog_events
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Takes the actions waiting to be handed to the mods.
    pub fn drain_actions(&self) -> Vec<(PlayerUuid, String, bool)> {
        self.actions
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Takes the punches waiting to be judged.
    pub fn drain_punches(&self) -> Vec<(PlayerUuid, u64)> {
        self.punches
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Asks the simulation to encode a chunk.
    ///
    /// Returns `None` if the queue is full, in which case the caller retries
    /// later — a dropped chunk request costs a moment of missing terrain, not
    /// correctness, because the interest set is recomputed every pass.
    pub fn request_chunk(
        &self,
        domain: &str,
        pos: tiamot_core::ChunkPos,
    ) -> Option<tokio::sync::oneshot::Receiver<Option<Vec<u8>>>> {
        self.request_chunk_at(domain, pos, None)
    }

    /// Asks the simulation to encode a chunk, or a summary of one.
    ///
    /// `level` is `None` for the chunk itself and `Some(level)` for a horizon.
    /// The two share this queue deliberately — see [`ChunkRequest::level`].
    ///
    /// Returns `None` if the queue is full, as [`Shared::request_chunk`] does.
    pub fn request_chunk_at(
        &self,
        domain: &str,
        pos: tiamot_core::ChunkPos,
        level: Option<u8>,
    ) -> Option<tokio::sync::oneshot::Receiver<Option<Vec<u8>>>> {
        let domain = domain.to_owned();
        let mut queue = self.chunk_requests.lock().ok()?;
        if queue.len() >= MAX_QUEUED_CHUNK_REQUESTS {
            return None;
        }
        let (reply, receiver) = tokio::sync::oneshot::channel();
        queue.push_back(ChunkRequest {
            domain,
            pos,
            level,
            reply,
        });
        Some(receiver)
    }

    /// Takes up to [`CHUNKS_PER_TICK`] requests for the simulation to serve.
    ///
    /// **Chunks first, and at most [`SUMMARIES_PER_TICK`] summaries.** A
    /// summary costs the simulation what a chunk costs — an unvisited one has
    /// to be GENERATED before it can be summarised — and the difference is that
    /// a detail radius is hundreds of chunks and a horizon is tens of
    /// thousands. Sharing one budget, a single player standing still would keep
    /// the tick generating terrain for as long as they were connected, where
    /// before it went quiet once their neighbourhood had arrived.
    ///
    /// Charter rule 18: 50 ms is shared by all simulation for all players.
    /// Scenery a mile away does not get to spend it. What is not taken stays
    /// queued in order, so nothing is dropped and nothing has to be re-asked.
    #[must_use]
    pub fn take_chunk_requests(&self) -> Vec<ChunkRequest> {
        self.chunk_requests
            .lock()
            .map(|mut queue| {
                let mut taken = Vec::with_capacity(CHUNKS_PER_TICK);
                let mut deferred = std::collections::VecDeque::new();
                let mut summaries = 0;
                while let Some(request) = queue.pop_front() {
                    if taken.len() >= CHUNKS_PER_TICK {
                        deferred.push_back(request);
                        continue;
                    }
                    if request.level.is_some() {
                        if summaries >= SUMMARIES_PER_TICK {
                            deferred.push_back(request);
                            continue;
                        }
                        summaries += 1;
                    }
                    taken.push(request);
                }
                *queue = deferred;
                taken
            })
            .unwrap_or_default()
    }

    /// Credits stacks to a player and marks them for an update.
    pub fn credit(&self, uuid: PlayerUuid, stacks: Vec<tiamot_core::inventory::Stack>) {
        if stacks.is_empty() {
            return;
        }
        if let Ok(mut inventories) = self.inventories.lock() {
            let held = inventories
                .entry(uuid)
                .or_insert_with(|| tiamot_core::inventory::Slots::for_player_with(&self.views));
            for stack in stacks {
                held.insert(PLAYER_MAIN, stack);
            }
        }
        if let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(uuid);
        }
    }

    /// What a player is carrying in one named view.
    ///
    /// Consolidated, so a mod asking "how much stone" gets one number rather
    /// than a slot layout it did not ask about.
    #[must_use]
    pub fn contents_of(&self, uuid: &PlayerUuid, view: &str) -> Vec<tiamot_core::inventory::Stack> {
        self.inventories
            .lock()
            .map(|inventories| {
                inventories
                    .get(uuid)
                    .map(|slots| slots.consolidated(view))
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// Puts a stack into one of a player's views, for a mod.
    ///
    /// **Refuses a player who is not connected**, rather than creating slots
    /// for them: an inventory made here would belong to nobody and would be
    /// gone the moment they joined properly.
    pub fn give(
        &self,
        uuid: &PlayerUuid,
        view: &str,
        stack: tiamot_core::inventory::Stack,
    ) -> bool {
        // **Connected is the test, not "has dug something".** An inventory
        // record is created the first time a player is credited, so somebody
        // who has just joined has none — and a mod handing them a starting kit
        // in `on_player_join` would have been refused for it.
        let connected = self
            .bodies
            .lock()
            .is_ok_and(|bodies| bodies.contains_key(uuid));
        if !connected {
            return false;
        }
        let took = self.inventories.lock().is_ok_and(|mut inventories| {
            inventories
                .entry(*uuid)
                .or_insert_with(|| tiamot_core::inventory::Slots::for_player_with(&self.views))
                .insert(view, stack)
        });
        if took && let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(*uuid);
        }
        took
    }

    /// Swaps a hotbar slot with the off-hand, for a player who pressed the key.
    ///
    /// Refused rather than clamped for a slot nobody has: clamping would swap
    /// a slot the player did not name, which is worse than doing nothing.
    pub fn swap_offhand(&self, uuid: &PlayerUuid, slot: usize) {
        let swapped =
            self.inventories
                .lock()
                .is_ok_and(|mut inventories| match inventories.get_mut(uuid) {
                    Some(slots) => slots.swap_offhand(PLAYER_MAIN, slot),
                    None => false,
                });
        if swapped && let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(*uuid);
        }
    }

    /// Takes up to `units` of one material and cut out of a view, for a mod.
    ///
    /// Returns how many it got, which may be fewer than asked.
    pub fn take(
        &self,
        uuid: &PlayerUuid,
        view: &str,
        material: tiamot_core::material::MaterialId,
        shape: Option<tiamot_core::inventory::Shape>,
        detail: Option<&str>,
        units: u32,
    ) -> u32 {
        let took = self
            .inventories
            .lock()
            .map(|mut inventories| match inventories.get_mut(uuid) {
                Some(slots) => slots.take(view, material, shape, detail, units),
                None => 0,
            })
            .unwrap_or(0);
        if took > 0
            && let Ok(mut dirty) = self.inventory_dirty.lock()
        {
            dirty.insert(*uuid);
        }
        took
    }

    /// What a player has in each hand: main, then off.
    ///
    /// **Read fresh every tick and mirrored onto the player's entity**, so that
    /// everybody who can see them can be told. Reported from the window as
    /// every other player having empty hands: a client draws what the LOCAL
    /// player holds from its own inventory, and nothing on the wire said what
    /// anyone else had.
    ///
    /// The main hand is the selected hotbar slot — absent means slot zero,
    /// which is where a client starts and stays until somebody presses a
    /// hotbar key.
    #[must_use]
    pub fn hands_of(&self, uuid: &PlayerUuid) -> tiamot_core::ent::Hands {
        let slot = self
            .held_slot
            .lock()
            .ok()
            .and_then(|held| held.get(uuid).copied())
            .unwrap_or(0);
        let Ok(inventories) = self.inventories.lock() else {
            return tiamot_core::ent::Hands::default();
        };
        let Some(slots) = inventories.get(uuid) else {
            return tiamot_core::ent::Hands::default();
        };
        let Some(view) = slots.view(PLAYER_MAIN) else {
            return tiamot_core::ent::Hands::default();
        };
        tiamot_core::ent::Hands {
            main: view.slots.get(slot).cloned().flatten(),
            off: view
                .slots
                .get(tiamot_core::inventory::PLAYER_OFFHAND_SLOT)
                .cloned()
                .flatten(),
        }
    }

    /// What a player is carrying.
    #[must_use]
    pub fn inventory_of(&self, uuid: &PlayerUuid) -> Vec<tiamot_core::inventory::Stack> {
        self.inventories
            .lock()
            .map(|inventories| {
                inventories
                    .get(uuid)
                    .map(|slots| slots.consolidated(PLAYER_MAIN))
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// One `ViewUpdate` per view a player has.
    ///
    /// Built here rather than at each call site so the two halves of an
    /// inventory — what you have and where it is — always leave together.
    #[must_use]
    pub fn view_updates(&self, uuid: &PlayerUuid) -> Vec<ServerMessage> {
        let Some(slots) = self.slots_of(uuid) else {
            return Vec::new();
        };
        let wire = |stack: &tiamot_core::inventory::Stack| tiamot_core::proto::StackDef {
            material: stack.material.0,
            units: stack.units,
            shape: stack
                .shape
                .map_or(0, tiamot_core::inventory::Shape::occupancy),
            detail: stack.detail.clone(),
        };
        let held = slots.grab.held.as_ref().map(wire);
        slots
            .views
            .iter()
            .map(|view| ServerMessage::ViewUpdate {
                view: view.name.clone(),
                slots: view
                    .slots
                    .iter()
                    .map(|slot| slot.as_ref().map(wire))
                    .collect(),
                held: held.clone(),
            })
            .collect()
    }

    /// Makes sure a player HAS an inventory, with every registered view in it.
    ///
    /// **A player's slots used to spring into existence the first time they
    /// dug**, because every path that touched them created them on the way.
    /// That is invisible while the only question anybody asks is "what have you
    /// got" — the answer is nothing either way — and wrong the moment a mod
    /// registers a view and draws a screen over it, because a view that does
    /// not exist yet is never sent and the screen is over nothing.
    ///
    /// Empty is a fact about a place, not the absence of one.
    /// Whether this player may use admin powers.
    #[must_use]
    pub fn is_operator(&self, uuid: &PlayerUuid) -> bool {
        self.operators.contains(uuid)
    }

    pub fn ensure_inventory(&self, uuid: PlayerUuid) {
        if let Ok(mut inventories) = self.inventories.lock() {
            inventories
                .entry(uuid)
                .or_insert_with(|| tiamot_core::inventory::Slots::for_player_with(&self.views));
        }
    }

    /// The inventory a player who has never played here would get.
    ///
    /// The template a stored blob is laid over — this session's views, at the
    /// sizes this session's mods registered.
    #[must_use]
    pub fn fresh_inventory(&self) -> tiamot_core::inventory::Slots {
        tiamot_core::inventory::Slots::for_player_with(&self.views)
    }

    /// Replaces a player's whole inventory, and tells their client.
    ///
    /// **How a saved inventory gets back in.** Loading happens on the tick
    /// thread, which is where the world database is; this is the door into the
    /// endpoint's own copy. Marks the inventory dirty, because a client that
    /// joined a moment earlier has already been sent the empty one.
    pub fn restore_inventory(&self, uuid: PlayerUuid, slots: tiamot_core::inventory::Slots) {
        if let Ok(mut inventories) = self.inventories.lock() {
            inventories.insert(uuid, slots);
        }
        if let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(uuid);
        }
    }

    /// Every player's inventory, for saving.
    ///
    /// A snapshot, taken under the lock and handed out by value: the caller is
    /// the tick thread and must not hold this lock while it writes to a
    /// database.
    #[must_use]
    pub fn all_inventories(&self) -> Vec<(PlayerUuid, tiamot_core::inventory::Slots)> {
        self.inventories
            .lock()
            .map(|inventories| {
                inventories
                    .iter()
                    .map(|(uuid, slots)| (*uuid, slots.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Forgets a player's inventory, once it has been written.
    pub fn forget_inventory(&self, uuid: &PlayerUuid) {
        if let Ok(mut inventories) = self.inventories.lock() {
            inventories.remove(uuid);
        }
    }

    /// A player's slots, for a dialog to click on.
    #[must_use]
    pub fn slots_of(&self, uuid: &PlayerUuid) -> Option<tiamot_core::inventory::Slots> {
        self.inventories.lock().ok()?.get(uuid).cloned()
    }

    /// Puts whatever is on a player's cursor back into their inventory.
    ///
    /// **Called when a screen closes.** A stack in hand has no picture once the
    /// screen it was picked up on is gone, so leaving it there is how an item
    /// seems to disappear for good — and it stays there across a save now that
    /// inventories persist. Returns whether anything moved.
    pub fn return_held(&self, uuid: &PlayerUuid) -> bool {
        let moved = self.inventories.lock().is_ok_and(|mut inventories| {
            inventories
                .get_mut(uuid)
                .is_some_and(|slots| slots.return_held(PLAYER_MAIN))
        });
        if moved && let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(*uuid);
        }
        moved
    }

    /// Applies a slot click to a player's inventory, on the server's own copy.
    ///
    /// **The whole authority story in one function.** A client says which view
    /// and which slot it clicked; this is where that becomes a change, against
    /// state the client cannot reach. A click on a slot that is not there does
    /// nothing (charter rule 14), and the caller learns nothing changed.
    /// Runs `visit` against one player's inventory, if they are connected.
    ///
    /// **The only way in from outside**, so nothing else has to know the lock
    /// is there. Used by the container store, which moves a view between the
    /// world and a player and needs both halves under one lock or a container
    /// can exist in two places for an instant.
    pub fn with_slots<T>(
        &self,
        uuid: &PlayerUuid,
        visit: impl FnOnce(&mut tiamot_core::inventory::Slots) -> T,
    ) -> Option<T> {
        let mut inventories = self.inventories.lock().ok()?;
        let slots = inventories.get_mut(uuid)?;
        Some(visit(slots))
    }

    /// Says a player's inventory has changed, so the next pass sends it.
    pub fn mark_inventory_dirty(&self, uuid: &PlayerUuid) {
        if let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(*uuid);
        }
    }

    pub fn click_slot(
        &self,
        uuid: &PlayerUuid,
        view: &str,
        index: usize,
        click: tiamot_core::proto::Click,
    ) -> bool {
        let Ok(mut inventories) = self.inventories.lock() else {
            return false;
        };
        let Some(slots) = inventories.get_mut(uuid) else {
            return false;
        };
        let changed = match click {
            tiamot_core::proto::Click::Left => slots.left_click(view, index),
            tiamot_core::proto::Click::Right => slots.right_click(view, index),
            tiamot_core::proto::Click::ShiftLeft => {
                // **Within the player's own view**, between the hotbar band and
                // the rest of it — which is what the gesture means now that the
                // hotbar is the first nine slots of one inventory rather than a
                // second view to shuffle things into. A mod's container becomes
                // a destination when views can be mod-owned.
                slots.stow(view, index, PLAYER_HOTBAR_SLOTS)
            }
        };
        if changed && let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(*uuid);
        }
        changed
    }

    /// Takes and clears the dirty flag for one player.
    #[must_use]
    pub fn take_inventory_dirty(&self, uuid: &PlayerUuid) -> bool {
        self.inventory_dirty
            .lock()
            .map(|mut dirty| dirty.remove(uuid))
            .unwrap_or(false)
    }

    /// Fans a message out to every connected player.
    ///
    /// A send with no receivers is not an error — it means nobody is connected.
    pub fn broadcast(&self, message: ServerMessage) {
        let _ = self.outbound.send(Broadcast {
            only: None,
            message,
        });
    }

    /// Queues a message for every in-world player IN ONE DOMAIN.
    ///
    /// **What makes an edit stay in the space it happened in.** Positions are
    /// identical between domains, so a block delta that reached everybody would
    /// land as terrain changing under somebody standing nowhere near it — which
    /// is what an untested `broadcast` did, and what the two-domain interest
    /// test caught.
    ///
    /// Filtered at the connection rather than at the sender, because who is
    /// where changes every tick and the sender would have to hold the body lock
    /// to find out.
    pub fn broadcast_in(&self, domain: &str, message: ServerMessage) {
        let _ = self.outbound.send(Broadcast {
            only: Some(domain.to_owned()),
            message,
        });
    }

    /// Queues a line of text for one player.
    ///
    /// **The counterpart to every server-side refusal.** Charter rule 2 puts
    /// the decision on the server, which means a client that asked to place
    /// something and saw nothing happen cannot work out why — it does not know
    /// what the player is carrying, what is already in the target, or where
    /// everyone else is standing. Without this, every refusal is
    /// indistinguishable from a dropped packet.
    ///
    /// Bounded per player: the queue is drained on that player's own connection
    /// task, and a client that spams refusable requests faster than it reads
    /// would otherwise grow this without limit.
    /// Sets what one mod's HUD script should show one player.
    ///
    /// Replaces that mod's whole set for that player, because a mod computes
    /// what it wants shown and says so — merging would make "this value is
    /// gone now" impossible to express without a second call.
    ///
    /// Nothing is queued when the values are unchanged: a mod setting the same
    /// health sixty times a second should cost nothing on the wire.
    pub fn set_hud_values(
        &self,
        uuid: &PlayerUuid,
        mod_id: &str,
        values: tiamot_core::hud::Values,
    ) {
        let Ok(mut all) = self.hud_values.lock() else {
            return;
        };
        let slot = all
            .entry(*uuid)
            .or_default()
            .entry(mod_id.to_owned())
            .or_default();
        if slot.values == values && slot.sent {
            return;
        }
        slot.values = values;
        slot.sent = false;
    }

    /// Takes the HUD values one player has not been told yet.
    pub fn unsent_hud_values(&self, uuid: &PlayerUuid) -> Vec<ServerMessage> {
        let Ok(mut all) = self.hud_values.lock() else {
            return Vec::new();
        };
        let Some(mods) = all.get_mut(uuid) else {
            return Vec::new();
        };
        mods.iter_mut()
            .filter(|(_, slot)| !slot.sent)
            .map(|(mod_id, slot)| {
                slot.sent = true;
                ServerMessage::HudValues {
                    mod_id: mod_id.clone(),
                    values: slot
                        .values
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                }
            })
            .collect()
    }

    /// Forgets everything a player was being shown, when they leave.
    pub fn forget_hud_values(&self, uuid: &PlayerUuid) {
        if let Ok(mut all) = self.hud_values.lock() {
            all.remove(uuid);
        }
    }

    pub fn tell(&self, uuid: &PlayerUuid, text: String) {
        if let Ok(mut notices) = self.notices.lock() {
            let queue = notices.entry(*uuid).or_default();
            if queue.len() < MAX_QUEUED_NOTICES {
                queue.push(text);
            }
        }
    }

    /// Queues entity messages for one player.
    ///
    /// Returns whether the queue overflowed, in which case it was cleared and
    /// the caller must reset that player's tracker: a queue that dropped a
    /// spawn leaves the client holding an id it can never draw, so starting
    /// over is the only recoverable answer.
    pub fn push_entity_messages(
        &self,
        uuid: &PlayerUuid,
        messages: impl IntoIterator<Item = ServerMessage>,
    ) -> bool {
        let Ok(mut queues) = self.entity_messages.lock() else {
            return false;
        };
        let queue = queues.entry(*uuid).or_default();
        queue.extend(messages);
        if queue.len() > MAX_QUEUED_ENTITY_MESSAGES {
            queue.clear();
            return true;
        }
        false
    }

    /// Takes every entity message queued for one player.
    #[must_use]
    pub fn take_entity_messages(&self, uuid: &PlayerUuid) -> Vec<ServerMessage> {
        self.entity_messages
            .lock()
            .map(|mut queues| queues.remove(uuid).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Takes everything queued for one player.
    #[must_use]
    pub fn take_notices(&self, uuid: &PlayerUuid) -> Vec<String> {
        self.notices
            .lock()
            .map(|mut notices| notices.remove(uuid).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Asks an identity's connections to disconnect.
    ///
    /// Returns `false` only if nothing is listening at all.
    pub fn kick(&self, uuid: PlayerUuid, reason: String) -> bool {
        self.kicks.send((uuid, reason)).is_ok()
    }

    /// Whether one player is in world right now.
    #[must_use]
    pub fn is_online(&self, uuid: &PlayerUuid) -> bool {
        self.online
            .lock()
            .is_ok_and(|online| online.contains_key(uuid))
    }

    /// Everyone currently in world, name and identity.
    #[must_use]
    pub fn online_players(&self) -> Vec<(PlayerUuid, String)> {
        self.online
            .lock()
            .map(|online| {
                online
                    .iter()
                    .map(|(uuid, name)| (*uuid, name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Holds a player slot for as long as it exists.
///
/// The count must come back down when a connection ends, and connections end in
/// more ways than there are code paths to remember: a clean disconnect, a
/// protocol error, a timeout, a dropped cable, a panic in the handler. A guard
/// releases the slot in every one of those cases, including unwinding. An
/// explicit decrement at the end of the handler would leak a slot on any path
/// that did not reach it, and a server that slowly fills with ghosts until it
/// reports itself full is a genuinely hard bug to find.
struct PlayerSlot<'a> {
    shared: &'a Shared,
    uuid: PlayerUuid,
}

impl<'a> PlayerSlot<'a> {
    fn claim(shared: &'a Shared, uuid: PlayerUuid, name: String) -> Self {
        shared.players.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut online) = shared.online.lock() {
            online.insert(uuid, name);
        }
        // The simulated body is claimed and released by the same guard as the
        // slot and the roster. Creating it at the join site instead would leak
        // one every time a handler returned early — and the simulation would go
        // on stepping a body whose player left.
        shared.add_player(uuid, shared.spawn);
        Self { shared, uuid }
    }
}

impl Drop for PlayerSlot<'_> {
    fn drop(&mut self) {
        self.shared.players.fetch_sub(1, Ordering::AcqRel);
        // The roster and the count are released together, by the same guard.
        // Two separate cleanups would eventually disagree, and `status` would
        // list players who left.
        if let Ok(mut online) = self.shared.online.lock() {
            online.remove(&self.uuid);
        }
        self.shared.remove_player(&self.uuid);
    }
}

/// Builds the QUIC server configuration from a certificate.
///
/// # Errors
///
/// [`TransportError::Tls`] if the certificate and key do not form a usable TLS
/// configuration.
pub fn server_config(cert: ServerCert) -> Result<ServerConfig, TransportError> {
    let ServerCert { chain, key, .. } = cert;

    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|err| TransportError::Tls(Box::new(err)))?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|err| TransportError::Tls(Box::new(err)))?;

    // Clients that do not speak this exact protocol are refused during the TLS
    // handshake rather than after a round trip of Tiamot messages.
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|err| TransportError::Tls(Box::new(rustls::Error::General(err.to_string()))))?;

    let mut config = ServerConfig::with_crypto(Arc::new(tls));
    let transport =
        Arc::get_mut(&mut config.transport).expect("the transport config is not shared yet");
    // A client that stops responding must not hold a player slot forever.
    // Thirty seconds is long enough to survive a phone switching networks and
    // short enough that a crashed client frees its slot before anyone notices.
    transport.max_idle_timeout(Some(
        std::time::Duration::from_secs(30)
            .try_into()
            .expect("30s is a valid idle timeout"),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));

    Ok(config)
}

/// Binds the QUIC listener.
///
/// # Errors
///
/// [`TransportError`] if the socket cannot be bound or TLS cannot be built.
pub fn bind(addr: SocketAddr, cert: ServerCert) -> Result<Endpoint, TransportError> {
    let config = server_config(cert)?;
    Endpoint::server(config, addr).map_err(|source| TransportError::Bind { addr, source })
}

/// Accepts connections until the simulation is asked to stop.
pub async fn accept_loop(endpoint: Endpoint, shared: Arc<Shared>) {
    info!(addr = ?endpoint.local_addr().ok(), "listening for QUIC connections");

    while let Some(incoming) = endpoint.accept().await {
        if shared.control.stopping() {
            break;
        }

        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(connection) => {
                    if let Err(err) = serve(connection, &shared).await {
                        // A peer going away is normal. A peer sending nonsense
                        // is worth a line, but not worth a stack trace: the
                        // whole point of charter rule 14 is that hostile input
                        // is expected, not exceptional.
                        if err.is_clean_close() {
                            debug!(%remote, "connection closed");
                        } else {
                            warn!(%remote, "connection failed: {err}");
                        }
                    }
                }
                Err(err) => debug!(%remote, "handshake failed: {err}"),
            }
        });
    }

    // Give peers a moment to see the close frame rather than discovering the
    // server is gone by timing out thirty seconds later.
    endpoint.close(0u32.into(), b"server shutting down");
    endpoint.wait_idle().await;
}

/// Drives one connection's control stream to completion.
async fn serve(connection: quinn::Connection, shared: &Shared) -> Result<(), frame::FrameError> {
    let (mut send, mut recv) = connection.accept_bi().await.map_err(to_io)?;

    // Reading happens in its own task, feeding a channel.
    //
    // `frame::read` is NOT cancellation-safe: it reads a 4-byte length prefix
    // and then the body as two sequential awaits. `tokio::select!` cancels the
    // branches that do not win, so a timer or a broadcast firing between those
    // two reads discards the partially-read frame and leaves the stream
    // mid-message. The next read then interprets body bytes as a length
    // prefix, the decode fails, and the client is disconnected for a protocol
    // error it did not commit.
    //
    // That is what "connection stream failed" was: not flow control, and not
    // the client failing to read. It appeared under load and in debug builds
    // because both widen the window between the two awaits.
    //
    // A channel receive IS cancellation-safe, so selecting on one is correct.
    let (incoming_tx, mut incoming) =
        tokio::sync::mpsc::channel::<Result<ClientMessage, frame::FrameError>>(64);
    let reader = tokio::spawn(async move {
        loop {
            let message = frame::read::<_, ClientMessage>(&mut recv).await;
            let failed = message.is_err();
            if incoming_tx.send(message).await.is_err() || failed {
                return;
            }
        }
    });
    // Aborted on every exit path below, so a departing connection does not
    // leave a task holding the stream.
    let _reader = AbortOnDrop(reader);

    let mut session = Session::new();
    let mut slot: Option<PlayerSlot<'_>> = None;
    let auth = SelfSovereign;

    // Subscribed from the start, not on reaching the world. A receiver created
    // later would miss everything sent between the join completing and the
    // subscription — a narrow window, but the messages lost in it are exactly
    // the edits made while a player was loading in.
    let mut broadcasts = shared.outbound.subscribe();
    let mut kicks = shared.kicks.subscribe();
    let mut streamer: Option<Streamer> = None;
    // Whether the player reached the world on this message, so the opening
    // view-distance answer can be sent after the join flow rather than inside
    // it.
    let mut just_joined = false;

    // The streaming beat is an `interval`, NOT a `sleep` inside the select.
    //
    // A `sleep` future constructed in the select expression is built fresh on
    // every iteration, so it restarts from zero each time any other branch
    // wins. A peer that says anything more often than the beat therefore
    // starves it forever — the timer never gets 50 ms of quiet in which to
    // elapse. Nothing sent that often until Task 09 gave the client an input
    // per tick, and then it showed up as a client that joined, received its
    // material table, and never received a single chunk.
    //
    // An `interval` keeps its own schedule across cancellation, which is the
    // property this needs. `Delay` rather than `Burst` so that a connection
    // which stalls does not come back to a pile of instantly-ready ticks.
    let mut beat = tokio::time::interval(tiamot_core::tick::TICK_DURATION);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut transfers = crate::content::Transfers::new();
    // Chunk deliveries the simulation has answered, waiting to be written.
    let mut pending: Vec<Awaiting> = Vec::new();

    loop {
        // Read from the client and forward broadcasts on the same task. A
        // connection that only read would never deliver another player's edits;
        // a second task writing to the same stream would interleave two
        // messages' bytes and corrupt the framing.
        let message: ClientMessage = tokio::select! {
            // Streaming runs on its own beat rather than piggybacking on client
            // traffic. A player standing still sends nothing, and their world
            // must still finish loading.
            _ = beat.tick(),
                if streamer.is_some() || transfers.queued() > 0 =>
            {
                // Content before terrain: a client still waiting on textures
                // cannot render the chunks it is about to receive, so sending
                // terrain ahead of the assets for it just fills a buffer.
                for message in transfers.next_slices(&shared.content) {
                    frame::write(&mut send, &message).await?;
                }
                if let Some(uuid) = session.uuid()
                    && shared.take_inventory_dirty(&uuid)
                {
                    let stacks = shared
                        .inventory_of(&uuid)
                        .into_iter()
                        .map(|stack| tiamot_core::proto::StackDef {
                            material: stack.material.0,
                            units: stack.units,
                            shape: stack.shape.map_or(0, tiamot_core::inventory::Shape::occupancy),
                            detail: stack.detail,
                        })
                        .collect();
                    frame::write(&mut send, &ServerMessage::InventoryUpdate { stacks }).await?;
                    // **And where it all is**, on the same flag. The two are
                    // derived from one set of slots, so sending one without the
                    // other is how a dialog ends up showing a stack the hotbar
                    // says is gone. Digging with an inventory screen open is
                    // exactly that case.
                    for message in shared.view_updates(&uuid) {
                        frame::write(&mut send, &message).await?;
                    }
                }
                // Follow the player before serving chunks, so a move and the
                // terrain it needs happen on the same beat rather than a tick
                // apart. THIS is what makes the world stream as you walk: until
                // Task 09 the interest set was pinned to spawn, because nothing
                // knew where the player was.
                if let Some(uuid) = session.uuid()
                    && let Some((domain, chunk)) = shared.player_place(&uuid)
                    && let Some(streamer) = streamer.as_mut()
                {
                    // **A domain change first, and it is not an unload.** The
                    // client's chunks all belong to the space it is leaving,
                    // and their positions mean different chunks in the one it
                    // is entering — so it is told once to throw the world away
                    // rather than a thousand times to drop a position.
                    if streamer.domain() != domain {
                        let dropped = streamer.switch_to(&domain, chunk);
                        debug!(
                            player = session.display_name().unwrap_or("<unnamed>"),
                            %domain,
                            chunks = dropped.len(),
                            "moved to another domain"
                        );
                        frame::write(
                            &mut send,
                            &ServerMessage::DomainChanged {
                                domain: domain.clone(),
                            },
                        )
                        .await?;
                    }
                    for pos in streamer.recentre(chunk) {
                        frame::write(&mut send, &ServerMessage::ChunkUnload { pos }).await?;
                    }
                }

                if let Some(streamer) = streamer.as_mut()
                    && let Err(err) = pump_chunks(streamer, &mut pending, shared, &mut send).await
                {
                    return Err(err);
                }

                // The authoritative answer the client reconciles against. Sent
                // every tick rather than only on change: a client that missed
                // the one state saying "you stopped" would predict straight
                // through a wall until the next change, and states are small.
                if let Some(uuid) = session.uuid() {
                    if let Some(state) = shared.player_state(&uuid) {
                        frame::write(&mut send, &state).await?;
                    }
                    // Only to the digger, and only while a dig is running:
                    // nobody else draws their crack.
                    if let Some(progress) = shared.dig_progress(&uuid) {
                        frame::write(&mut send, &progress).await?;
                    }
                    // Everything about the entities this player can see —
                    // spawns and despawns because they must arrive, states
                    // because they are superseded. All on the reliable stream
                    // for now; the unreliable channel is a later optimisation
                    // and the split that matters (what is re-sent and what is
                    // not) is already in the message shapes.
                    for message in shared.take_entity_messages(&uuid) {
                        frame::write(&mut send, &message).await?;
                    }
                    // What a mod wants this player's own HUD to show. Sent
                    // only when it differs from what they were last told, so a
                    // mod recomputing the same health every tick costs nothing.
                    for message in shared.unsent_hud_values(&uuid) {
                        frame::write(&mut send, &message).await?;
                    }
                    // Why the last thing they asked for did not happen. Sent
                    // as chat from nobody: a refusal the player never sees is
                    // indistinguishable from the server having lost the
                    // message, and they will simply try again.
                    for text in shared.take_notices(&uuid) {
                        frame::write(&mut send, &ServerMessage::Chat { from: None, text }).await?;
                    }
                }
                continue;
            }
            received = incoming.recv() => match received {
                Some(Ok(message)) => message,
                // The reader ended: the peer went away.
                None => return Ok(()),
                Some(Err(err)) if err.is_clean_close() => return Ok(()),
                Some(Err(err)) => {
                    let reason = frame_error_reason(&err);
                    let _ = frame::write(&mut send, &ServerMessage::Disconnect { reason }).await;
                    flush_and_close(&mut send, &connection).await;
                    return Err(err);
                }
            },
            kick = kicks.recv() => {
                match kick {
                    Ok((target, reason)) if session.uuid() == Some(target) => {
                        let _ = frame::write(
                            &mut send,
                            &ServerMessage::Disconnect {
                                reason: tiamot_core::proto::DisconnectReason::Kicked { reason },
                            },
                        )
                        .await;
                        flush_and_close(&mut send, &connection).await;
                        return Ok(());
                    }
                    // Somebody else's kick, or a lagged/closed channel. A
                    // missed kick for another player is not this connection's
                    // problem.
                    _ => continue,
                }
            }
            broadcast = broadcasts.recv() => {
                match broadcast {
                    Ok(outbound) => {
                        // Only in-world players receive world state. A peer
                        // mid-handshake must not learn what anyone is building.
                        //
                        // And only players in the domain it happened in, when
                        // it says one: a body sees the space it is in and no
                        // other, and the positions mean different places
                        // between them.
                        let mine = outbound.only.is_none_or(|domain| {
                            streamer
                                .as_ref()
                                .is_some_and(|streamer| streamer.domain() == domain)
                        });
                        if mine && session.phase() == tiamot_core::session::Phase::InWorld {
                            // An edit out in the horizon is not something a
                            // client holding a summary can apply — it has
                            // nowhere to put one cell of twenty-seven. So the
                            // delta is still sent (it is two dozen bytes, and
                            // the client drops what it cannot place) and the
                            // summary is dropped, which puts the chunk back on
                            // the next pass's list at whatever level it is due.
                            if let ServerMessage::BlockDelta { edit, .. } = &outbound.message
                                && let Some(streamer) = streamer.as_mut()
                            {
                                let pos = chunk_of(edit);
                                if streamer.summary_level(pos).is_some() {
                                    streamer.resummarise(pos);
                                }
                            }
                            frame::write(&mut send, &outbound.message).await?;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        // This client could not keep up and lost messages. It
                        // sees a stale world until it next receives a chunk.
                        // Logged rather than fatal: dropping the connection
                        // would punish a slow link harder than the desync does.
                        warn!(missed, "client fell behind the broadcast stream");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        };

        let was_in_world = session.phase() == tiamot_core::session::Phase::InWorld;

        let response = {
            let mut identities = shared.identities.lock().await;
            // A read guard, taken and released within this block. Holding it
            // across the await above would block an RCON allowlist change
            // behind a client mid-handshake.
            let allowlist = shared
                .allowlist
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let context = JoinContext {
                cert_fingerprint: &shared.cert_fingerprint,
                mods: &shared.mods,
                mod_set_fingerprint: shared.mod_set_fingerprint,
                materials: &shared.materials,
                tools: &shared.tool_table,
                fluids: &shared.fluid_table,
                actions: &shared.action_table,
                sounds: &shared.sound_table,
                hud_scripts: &shared.hud_scripts,
                sound_bindings: &shared.sound_bindings,
                sky: (shared.sky_day_length, &shared.sky_keyframes),
                allowlist: &allowlist,
                max_players: shared.max_players,
                current_players: shared.players.load(Ordering::Acquire),
                spawn: shared.spawn,
                tick: shared.tick(),
                // **Decided here and sent with the join.** A client never asks
                // whether it may fly; it is told, so it never predicts a power
                // the server is about to ignore.
                may_fly: session.uuid().is_some_and(|uuid| shared.is_operator(&uuid)),
                now: unix_now(),
            };
            session.handle(&message, &context, &auth, &mut identities)
        };

        // Claim the slot the moment the session reaches the world, so the
        // "server full" check counts players rather than connections.
        if !was_in_world && session.phase() == tiamot_core::session::Phase::InWorld {
            slot = session.uuid().map(|uuid| {
                PlayerSlot::claim(
                    shared,
                    uuid,
                    session.display_name().unwrap_or("<unnamed>").to_owned(),
                )
            });
            // Interest starts at spawn. Task 09's physics will call
            // `recentre` as the player moves; see `stream.rs` on why the
            // client is not asked where it is.
            streamer = Some(Streamer::new(
                tiamot_core::domain::OVERWORLD,
                shared.spawn.chunk(),
                shared.view_distance,
            ));
            just_joined = true;
            info!(
                player = session.display_name().unwrap_or("<unnamed>"),
                uuid = session.uuid().map(|id| id.short()).unwrap_or_default(),
                "player joined"
            );
        }

        for outbound in &response.send {
            frame::write(&mut send, outbound).await?;
        }

        // **After the join flow, not inside it.** `JoinWorld` is the message
        // that completes the handshake, and clients and tests alike key off its
        // position in the sequence — slipping this in ahead of it displaced the
        // whole flow, which `a_bot_completes_the_whole_join_flow` caught.
        //
        // Unprompted, so a client that never asks still knows the radius it is
        // being sent and draws its fog there rather than at a number of its own.
        if std::mem::take(&mut just_joined) {
            frame::write(
                &mut send,
                &ServerMessage::ViewDistance {
                    horizontal: shared.view_distance.horizontal,
                    vertical: shared.view_distance.vertical,
                },
            )
            .await?;

            // **Every view once, even the empty ones.** They are otherwise sent
            // only when something changes, so a view that has never held
            // anything is a view the client has never been told exists — and a
            // mod that registered one and drew a screen over it before anybody
            // put anything in it showed a screen over nothing. Empty is a fact
            // about a place, not the absence of one.
            if let Some(uuid) = session.uuid() {
                shared.ensure_inventory(uuid);
                for message in shared.view_updates(&uuid) {
                    frame::write(&mut send, &message).await?;
                }
            }
        }

        // **The client's say in how far it sees, clamped by the server's.**
        //
        // Handled here rather than in the session for the reason content
        // requests are: the session is a state machine over the handshake and
        // owns no streamer. Charter rule 2 keeps the decision on the server —
        // this is a request, and the answer is what the server is willing to
        // send rather than what the client asked for.
        //
        // **`!response.close` is not belt-and-braces.** `close` is honoured at
        // the BOTTOM of this loop, so without the guard every transport-served
        // message is still acted on once after the session has refused it. That
        // is exactly how this shipped broken: the session had no accepting arm
        // for `ViewDistance`, so the server answered the request and
        // disconnected the client for sending it — and the bot tests, which read
        // the answer and never looked at the connection, went green.
        if !response.close
            && let ClientMessage::ViewDistance {
                horizontal,
                vertical,
            } = &message
            && let Some(streamer) = streamer.as_mut()
        {
            // Capped by the server's configured radius on both axes, then by
            // the engine's own bounds. Asking for LESS is always granted, which
            // is the direction that matters: a player on a modest machine needs
            // a way to make the world smaller, and before this there was none.
            let granted = tiamot_core::interest::ViewDistance::clamped(
                (*horizontal).min(shared.view_distance.horizontal),
                (*vertical).min(shared.view_distance.vertical),
            );
            for pos in streamer.resize(granted) {
                frame::write(&mut send, &ServerMessage::ChunkUnload { pos }).await?;
            }
            // Echoed even when it changed nothing, so a client that asked for
            // more than it may have still learns what it actually got and draws
            // its fog there.
            frame::write(
                &mut send,
                &ServerMessage::ViewDistance {
                    horizontal: granted.horizontal,
                    vertical: granted.vertical,
                },
            )
            .await?;
        }

        // Content requests are served here rather than in the session, which
        // has no business knowing about files. The session has already refused
        // this message if the peer had not authenticated — reaching here means
        // the phase check passed.
        if let ClientMessage::ContentRequest { hashes } = &message
            && matches!(
                session.phase(),
                tiamot_core::session::Phase::Authenticated | tiamot_core::session::Phase::InWorld
            )
        {
            let accepted = transfers.request(hashes, &shared.content);
            debug!(asked = hashes.len(), accepted, "content requested");
        }

        // The session decided this was allowed; carrying it out is this
        // layer's job. See `session::Action`.
        match &response.action {
            Action::Edit(edit) => {
                if let Some(uuid) = session.uuid()
                    && !shared.queue_edit(uuid, edit.clone())
                {
                    warn!("edit queue is full; dropping an edit");
                }
            }
            // **Queued, not broadcast here.** Mods get a veto on chat, mods
            // run on the tick thread, and a line already sent cannot be
            // unsent — so this waits for the tick like every other thing a
            // client asks for.
            Action::Chat { text } => {
                if let Some(uuid) = session.uuid() {
                    shared.queue_chat(uuid, text.clone());
                }
            }
            // Filed under the tick it claims, not applied here. The simulation
            // takes one input per tick per player, in a fixed order — see the
            // player step in `handle.rs` for why that cannot happen on this
            // task.
            Action::Input {
                tick,
                movement,
                look,
                actions,
            } => {
                if let Some(uuid) = session.uuid() {
                    // A refusal is ordinary traffic: a duplicate from the
                    // three-input redundancy, or an input whose tick has
                    // already been simulated.
                    shared.queue_input(
                        &uuid,
                        *tick,
                        intent_from_wire(*movement, *actions, shared.is_operator(&uuid)),
                        *look,
                    );
                }
            }
            Action::Dig { target } => {
                if let Some(uuid) = session.uuid() {
                    shared.set_dig(&uuid, *target);
                }
            }
            Action::Punch { entity } => {
                if let Some(uuid) = session.uuid() {
                    shared.queue_punch(uuid, *entity);
                }
            }
            Action::SwapOffhand { slot } => {
                if let Some(uuid) = session.uuid() {
                    shared.swap_offhand(&uuid, usize::from(*slot));
                }
            }
            Action::SelectSlot { slot } => {
                if let Some(uuid) = session.uuid()
                    && let Ok(mut held) = shared.held_slot.lock()
                {
                    held.insert(uuid, usize::from(*slot));
                }
            }
            Action::Action { id, pressed } => {
                if let Some(uuid) = session.uuid() {
                    shared.queue_action(uuid, id.clone(), *pressed);
                }
            }
            Action::Dialog { form, event } => {
                if let Some(uuid) = session.uuid() {
                    shared.queue_dialog_event(uuid, form.clone(), event.clone());
                }
            }
            Action::SelectTool { tool } => {
                if let Some(uuid) = session.uuid() {
                    shared.select_tool(&uuid, tool.clone());
                }
            }
            // Queued, not carried out. Deciding a placement needs the world
            // and every body in it, and both belong to the tick thread — the
            // same reason digging and movement are queued rather than applied
            // here. Two connection tasks placing at once would otherwise
            // resolve in whichever order the OS woke them.
            Action::Place {
                target,
                material,
                shape,
                face,
                detail,
            } => {
                if let Some(uuid) = session.uuid()
                    && !shared.queue_placement(
                        uuid,
                        *target,
                        *material,
                        *shape,
                        *face,
                        detail.clone(),
                    )
                {
                    warn!("placement queue is full; dropping a placement");
                }
            }
            Action::None => {}
        }

        if response.close {
            flush_and_close(&mut send, &connection).await;
            break;
        }
    }

    drop(slot);
    Ok(())
}

/// Seconds since the Unix epoch, for stamping `added_at` on a first join.
///
/// Only ever a record of when something happened — nothing in the simulation
/// reads it, so a machine with a wrong clock produces a confusing audit trail
/// rather than a divergent world.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

/// A chunk or summary the simulation has been asked for and not yet answered.
///
/// The level is what was ASKED for, not what came back: the reply is a blob
/// either way, and this is the only thing that says which message to put it in.
type Awaiting = (
    tiamot_core::ChunkPos,
    Option<u8>,
    tokio::sync::oneshot::Receiver<Option<Vec<u8>>>,
);

/// Which chunk an edit is in.
///
/// An [`Edit`] names a block, a sub-node or a partial block, and all three are
/// somewhere; the chunk is what the streaming cares about.
fn chunk_of(edit: &Edit) -> tiamot_core::ChunkPos {
    match edit {
        Edit::Block { pos, .. } | Edit::Partial { pos, .. } => pos.chunk(),
        Edit::SubNode { pos, .. } => pos.chunk(),
    }
}

/// Requests, collects, and sends chunks for one connection.
///
/// Split out of the connection loop because it is the only part with an
/// interesting failure mode: a request that is answered but never accounted
/// for leaks the client's budget, and the leak is silent — the player's world
/// simply stops filling in.
async fn pump_chunks(
    streamer: &mut Streamer,
    pending: &mut Vec<Awaiting>,
    shared: &Shared,
    send: &mut quinn::SendStream,
) -> Result<(), frame::FrameError> {
    // Deliveries first, so budget freed this pass can be spent this pass.
    let mut still_waiting = Vec::with_capacity(pending.len());
    for (pos, level, mut receiver) in pending.drain(..) {
        match receiver.try_recv() {
            Ok(Some(blob)) => {
                // Whichever was asked for. Sending the wrong message for a
                // blob would be a client decoding a summary as a chunk, which
                // is not a thing the codec can catch — both are byte strings.
                let message = match level {
                    None => ServerMessage::ChunkData { pos, blob },
                    Some(_) => ServerMessage::ChunkSummary { pos, blob },
                };
                frame::write(send, &message).await?;
                // `delivered` clears the in-flight entry too, so the chunk goes
                // straight from "requested" to "held" with no window in which
                // it looks un-requested and gets asked for again.
                match level {
                    None => streamer.delivered(pos),
                    Some(level) => streamer.summarised(pos, level),
                }
            }
            Ok(None) => {
                // The simulation could not produce it. Cleared exactly like a
                // success, and NOT marked delivered, so the next pass asks
                // again.
                streamer.completed(pos);
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                still_waiting.push((pos, level, receiver));
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                // The simulation dropped the request without answering. Same
                // accounting: the slot must come back or this connection
                // slowly stops asking for anything.
                streamer.completed(pos);
            }
        }
    }
    *pending = still_waiting;

    // Then new requests, up to this client's share. **Full chunks first, and
    // they take the whole budget if they want it.** A player's own
    // neighbourhood is the ground under their feet; the horizon is scenery, and
    // scenery that arrives a second late is scenery. The reverse order would
    // let a joining player's horizon delay the chunk they are standing in.
    let budget = streamer.budget(CHUNKS_IN_FLIGHT_PER_CLIENT);
    for pos in streamer.next_needed(budget) {
        let Some(receiver) = shared.request_chunk(streamer.domain(), pos) else {
            // Queue full. Nothing is marked, so the next pass retries.
            break;
        };
        streamer.requested(pos);
        pending.push((pos, None, receiver));
    }

    // **The horizon's own allowance, not the chunks' leftovers.** It had the
    // leftovers, and that is a priority that becomes a starvation: a client
    // streaming a view of 17 has thousands of chunks to fetch and takes the
    // whole in-flight budget every pass for as long as that lasts, so the
    // horizon never started. Reported from the window as "0 held" after a
    // thousand ticks.
    //
    // One slot is enough. The simulation serves at most SUMMARIES_PER_TICK of
    // them anyway, so a bigger number here would only queue work the tick has
    // already decided not to do.
    let outstanding = pending
        .iter()
        .filter(|(_, level, _)| level.is_some())
        .count();
    let horizon = SUMMARIES_IN_FLIGHT_PER_CLIENT.saturating_sub(outstanding);
    for (pos, level) in streamer.next_summaries(horizon) {
        let Some(receiver) = shared.request_chunk_at(streamer.domain(), pos, Some(level)) else {
            break;
        };
        streamer.requested(pos);
        pending.push((pos, Some(level), receiver));
    }

    Ok(())
}

/// Aborts a task when dropped.
///
/// The reader task owns the receive stream, so letting it outlive the
/// connection handler would keep the stream — and the connection — alive.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Waits for the peer to actually receive what was written, then closes.
///
/// `finish` only says "no more data"; the bytes are still in flight. Returning
/// straight after it drops the `Connection`, which sends CONNECTION_CLOSE and
/// throws away anything not yet delivered — so a client refused for a bad
/// version would see the connection vanish rather than the reason it was
/// refused, which is precisely the failure mode a `Disconnect` message exists
/// to prevent.
///
/// The timeout matters: a peer that stops reading must not hold a task open
/// forever, and a disconnect is not worth waiting on.
async fn flush_and_close(send: &mut quinn::SendStream, connection: &quinn::Connection) {
    let _ = send.finish();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), send.stopped()).await;
    connection.close(0u32.into(), b"disconnected");
}

fn frame_error_reason(err: &frame::FrameError) -> tiamot_core::proto::DisconnectReason {
    match err {
        frame::FrameError::Protocol(protocol) => protocol.to_disconnect(),
        other => tiamot_core::proto::DisconnectReason::ProtocolError {
            detail: other.to_string(),
        },
    }
}

fn to_io(err: quinn::ConnectionError) -> frame::FrameError {
    let kind = match err {
        quinn::ConnectionError::ApplicationClosed(_)
        | quinn::ConnectionError::ConnectionClosed(_)
        | quinn::ConnectionError::LocallyClosed => std::io::ErrorKind::UnexpectedEof,
        _ => std::io::ErrorKind::ConnectionAborted,
    };
    frame::FrameError::Io(std::io::Error::new(kind, err))
}

#[cfg(test)]
mod fly_permission_tests {
    use super::*;
    use tiamot_core::proto::actions as bits;

    #[test]
    fn asking_to_fly_is_not_being_allowed_to() {
        // **Charter rule 2**, on the one power a client can currently ask for.
        // Every client can set the bit — the protocol has no way to stop it —
        // and the server honours it for an operator and nobody else, exactly as
        // it ignores a placement into occupied space.
        let asked = bits::FLY | bits::JUMP;
        assert!(
            !intent_from_wire([0.0; 3], asked, false).fly,
            "a player the server did not make an operator flew by asking"
        );
        assert!(intent_from_wire([0.0; 3], asked, true).fly);
    }

    #[test]
    fn an_operator_who_does_not_press_it_does_not_fly() {
        // The permission is not the state: being allowed to fly is not flying,
        // or an operator could never walk.
        assert!(!intent_from_wire([0.0; 3], bits::JUMP, true).fly);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct identity per index, so roster entries do not collide.
    fn player(index: u8) -> (PlayerUuid, String) {
        (
            PlayerUuid::from_bytes([index; 32]),
            format!("Player{index}"),
        )
    }

    fn shared() -> Shared {
        Shared {
            identities: Mutex::new(IdentityRegistry::default()),
            cert_fingerprint: [0xAB; 32],
            mods: Vec::new(),
            mod_set_fingerprint: 0,
            materials: Vec::new(),
            tool_table: Vec::new(),
            views: Vec::new(),
            items: std::collections::BTreeSet::new(),
            held_slot: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            action_table: Vec::new(),
            sound_table: Vec::new(),
            hud_scripts: Vec::new(),
            sound_bindings: Vec::new(),
            fluid_table: Vec::new(),
            sky_day_length: 0,
            sky_keyframes: Vec::new(),
            time_of_day: std::sync::atomic::AtomicU64::new(0),
            allowlist: std::sync::RwLock::new(Allowlist::open()),
            operators: std::collections::BTreeSet::new(),
            max_players: 2,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: Control::new(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            punches: std::sync::Mutex::new(std::collections::VecDeque::new()),
            actions: std::sync::Mutex::new(std::collections::VecDeque::new()),
            dialog_events: std::sync::Mutex::new(std::collections::VecDeque::new()),
            chat: std::sync::Mutex::new(std::collections::VecDeque::new()),
            placements: std::sync::Mutex::new(std::collections::VecDeque::new()),
            seeds: std::sync::Mutex::new(std::collections::VecDeque::new()),
            outbound: tokio::sync::broadcast::channel(16).0,
            chunk_requests: std::sync::Mutex::new(std::collections::VecDeque::new()),
            view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
            content: tiamot_core::content::ContentIndex::new(),
            inventories: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            inventory_dirty: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            notices: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            entity_messages: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            hud_values: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            kicks: tokio::sync::broadcast::channel(4).0,
            online: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            bodies: Arc::default(),
            tools: std::collections::BTreeMap::new(),
            hardness: std::collections::BTreeMap::new(),
            default_tool: None,
        }
    }

    #[test]
    fn a_player_slot_is_released_when_dropped() {
        let shared = shared();
        {
            let (uuid, name) = player(1);
            let _slot = PlayerSlot::claim(&shared, uuid, name);
            assert_eq!(shared.players.load(Ordering::Acquire), 1);
            assert_eq!(shared.online_players().len(), 1);
        }
        assert_eq!(shared.players.load(Ordering::Acquire), 0);
        assert!(
            shared.online_players().is_empty(),
            "the roster and the count must be released together"
        );
    }

    #[test]
    fn a_player_slot_is_released_when_the_handler_panics() {
        // The case the guard exists for. An explicit decrement at the end of
        // the handler would leak the slot here, and the server would slowly
        // fill with ghosts until it reported itself full.
        let shared = shared();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (uuid, name) = player(2);
            let _slot = PlayerSlot::claim(&shared, uuid, name);
            assert_eq!(shared.players.load(Ordering::Acquire), 1);
            panic!("connection handler blew up");
        }));

        assert!(result.is_err(), "the panic should have propagated");
        assert_eq!(
            shared.players.load(Ordering::Acquire),
            0,
            "a panicking handler must not leak a player slot"
        );
        assert!(
            shared.online_players().is_empty(),
            "nor leave a ghost on the roster"
        );
    }

    #[test]
    fn the_horizon_never_takes_more_than_its_quarter_of_a_tick() {
        // **The regression this exists for**: before the split, a player
        // standing still kept the simulation generating horizon terrain for as
        // long as they stayed connected, because a summary request and a chunk
        // request came out of one queue and looked identical. The tick went
        // from quiet-once-arrived to permanently busy.
        let shared = shared();
        // Far more summaries than a tick may serve, and a couple of chunks
        // behind them — which is the ordering that matters, since the chunks
        // are the ones a player is standing on.
        for n in 0..64 {
            let _ = shared.request_chunk_at(
                tiamot_core::domain::OVERWORLD,
                tiamot_core::ChunkPos::new(n, 0, 0),
                Some(tiamot_core::lod::FINEST),
            );
        }
        for n in 0..4 {
            let _ = shared.request_chunk(
                tiamot_core::domain::OVERWORLD,
                tiamot_core::ChunkPos::new(0, n, 0),
            );
        }

        let taken = shared.take_chunk_requests();
        let summaries = taken.iter().filter(|r| r.level.is_some()).count();
        let chunks = taken.len() - summaries;
        assert_eq!(
            summaries, SUMMARIES_PER_TICK,
            "the horizon took {summaries} of a tick, not its {SUMMARIES_PER_TICK}"
        );
        assert_eq!(
            chunks, 4,
            "chunks queued BEHIND a wall of summaries were not served this tick"
        );

        // And nothing was dropped: the rest are still there, next tick.
        let next = shared.take_chunk_requests();
        assert_eq!(
            next.iter().filter(|r| r.level.is_some()).count(),
            SUMMARIES_PER_TICK
        );
    }

    #[test]
    fn slots_count_independently() {
        let shared = shared();
        let (uuid_a, name_a) = player(3);
        let (uuid_b, name_b) = player(4);
        let first = PlayerSlot::claim(&shared, uuid_a, name_a);
        let second = PlayerSlot::claim(&shared, uuid_b, name_b);
        assert_eq!(shared.players.load(Ordering::Acquire), 2);
        assert_eq!(shared.online_players().len(), 2);
        drop(first);
        assert_eq!(shared.players.load(Ordering::Acquire), 1);
        assert_eq!(shared.online_players().len(), 1);
        drop(second);
        assert_eq!(shared.players.load(Ordering::Acquire), 0);
        assert!(shared.online_players().is_empty());
    }
}
