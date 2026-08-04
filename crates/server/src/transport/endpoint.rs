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
    /// Who is permitted to join.
    ///
    /// Behind a lock because RCON changes it at runtime: an operator adding
    /// someone to the allowlist should not have to restart the server, which
    /// would disconnect everyone already playing.
    pub allowlist: std::sync::RwLock<Allowlist>,
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

    /// Messages to fan out to every connected player.
    ///
    /// A `broadcast` channel: each connection holds a receiver and forwards
    /// what arrives. A slow client falls behind and its receiver lags, which
    /// costs that client messages rather than stalling the simulation — the
    /// right trade, because the alternative is one bad connection pausing the
    /// world for everyone.
    pub outbound: tokio::sync::broadcast::Sender<ServerMessage>,

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
    /// Task 09 owns the rest of the player-interaction loop — placement does
    /// not yet consume from this. What is here is the half `mine_3x3.lua`
    /// needs to be a real proof of the 27-unit design rather than a vacuous
    /// one.
    pub inventories: std::sync::Mutex<
        std::collections::BTreeMap<PlayerUuid, Vec<tiamot_core::inventory::Stack>>,
    >,

    /// Players whose inventory changed and have not been told yet.
    pub inventory_dirty: std::sync::Mutex<std::collections::BTreeSet<PlayerUuid>>,

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

    /// The tool a player digs with when they have selected nothing.
    ///
    /// `None` when the loaded mods registered no default — and then nobody can
    /// dig at all, which is deliberate. See [`Shared::resolve_tool`].
    pub default_tool: Option<String>,

    /// Seconds to break each material with a bare hand.
    ///
    /// Keyed by WORLD material id, because that is what a chunk holds. A
    /// material with no entry gets the engine default rather than being
    /// unbreakable — see `BlockRules::DEFAULT_HARDNESS`.
    pub hardness: std::collections::BTreeMap<tiamot_core::MaterialId, f32>,

    /// Named `bodies` rather than `players` because `players` is already the
    /// connected *count* on this struct, and two fields whose names differ only
    /// by what they happen to hold is how the wrong one gets locked.
    pub bodies: std::sync::Mutex<std::collections::BTreeMap<PlayerUuid, PlayerSim>>,
}

/// One player's simulated body and the inputs waiting to move it.
#[derive(Debug, Clone)]
pub struct PlayerSim {
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
            origin,
            body: tiamot_core::phys::Body::at(local),
            inputs: tiamot_core::phys::InputQueue::new(tick),
            dig: None,
            tool: None,
        }
    }
}

/// Turns a wire input into something the physics can step.
///
/// The movement vector is already world-space (see
/// [`ClientMessage::PlayerInput`]), so there is no rotation here and therefore
/// no trigonometry in the simulation path — charter rule 4.
#[must_use]
pub fn intent_from_wire(movement: [f32; 3], actions: u32) -> tiamot_core::phys::Intent {
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
    }
}

/// A connection asking the simulation for a chunk.
pub struct ChunkRequest {
    /// Which chunk.
    pub pos: tiamot_core::ChunkPos,
    /// Where to send the encoded blob.
    ///
    /// A oneshot, so a connection that goes away between asking and being
    /// answered simply drops the receiver and the simulation's send fails
    /// harmlessly. A shared queue of replies would need the simulation to know
    /// which connections still exist.
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<u8>>>,
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

/// Most chunk requests that may be queued before new ones are refused.
pub const MAX_QUEUED_CHUNK_REQUESTS: usize = 512;

/// How many unapplied edits may be queued before new ones are dropped.
///
/// At 20 Hz with 50 players this is several seconds of backlog — far more than
/// a healthy server ever holds, and small enough that a client flooding edits
/// cannot grow it into a memory problem.
pub const MAX_QUEUED_EDITS: usize = 4096;

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
    ) -> bool {
        let Ok(mut bodies) = self.bodies.lock() else {
            return false;
        };
        bodies
            .get_mut(uuid)
            .is_some_and(|player| player.inputs.offer(tick, intent))
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

    /// How long a material takes to break with a bare hand, in seconds.
    #[must_use]
    pub fn hardness_of(&self, material: tiamot_core::MaterialId) -> f32 {
        self.hardness
            .get(&material)
            .copied()
            .unwrap_or(tiamot_core::script::BlockRules::DEFAULT_HARDNESS)
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
    pub fn advance_dig(&self, uuid: &PlayerUuid, hardness: f32) -> Option<bool> {
        let mut bodies = self.bodies.lock().ok()?;
        let player = bodies.get_mut(uuid)?;
        let speed = self
            .resolve_tool(player.tool.as_deref())
            .map(|tool| tool.speed_multiplier)?;
        Some(player.dig.as_mut()?.advance(hardness, speed))
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

    /// Asks the simulation to encode a chunk.
    ///
    /// Returns `None` if the queue is full, in which case the caller retries
    /// later — a dropped chunk request costs a moment of missing terrain, not
    /// correctness, because the interest set is recomputed every pass.
    pub fn request_chunk(
        &self,
        pos: tiamot_core::ChunkPos,
    ) -> Option<tokio::sync::oneshot::Receiver<Option<Vec<u8>>>> {
        let mut queue = self.chunk_requests.lock().ok()?;
        if queue.len() >= MAX_QUEUED_CHUNK_REQUESTS {
            return None;
        }
        let (reply, receiver) = tokio::sync::oneshot::channel();
        queue.push_back(ChunkRequest { pos, reply });
        Some(receiver)
    }

    /// Takes up to [`CHUNKS_PER_TICK`] requests for the simulation to serve.
    #[must_use]
    pub fn take_chunk_requests(&self) -> Vec<ChunkRequest> {
        self.chunk_requests
            .lock()
            .map(|mut queue| {
                let take = queue.len().min(CHUNKS_PER_TICK);
                queue.drain(..take).collect()
            })
            .unwrap_or_default()
    }

    /// Credits stacks to a player and marks them for an update.
    pub fn credit(&self, uuid: PlayerUuid, stacks: Vec<tiamot_core::inventory::Stack>) {
        if stacks.is_empty() {
            return;
        }
        if let Ok(mut inventories) = self.inventories.lock() {
            let held = inventories.entry(uuid).or_default();
            let merged = tiamot_core::inventory::consolidate(held.drain(..).chain(stacks));
            *held = merged;
        }
        if let Ok(mut dirty) = self.inventory_dirty.lock() {
            dirty.insert(uuid);
        }
    }

    /// What a player is carrying.
    #[must_use]
    pub fn inventory_of(&self, uuid: &PlayerUuid) -> Vec<tiamot_core::inventory::Stack> {
        self.inventories
            .lock()
            .map(|inventories| inventories.get(uuid).cloned().unwrap_or_default())
            .unwrap_or_default()
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
        let _ = self.outbound.send(message);
    }

    /// Asks an identity's connections to disconnect.
    ///
    /// Returns `false` only if nothing is listening at all.
    pub fn kick(&self, uuid: PlayerUuid, reason: String) -> bool {
        self.kicks.send((uuid, reason)).is_ok()
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
    let mut pending: Vec<(
        tiamot_core::ChunkPos,
        tokio::sync::oneshot::Receiver<Option<Vec<u8>>>,
    )> = Vec::new();

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
                        .map(|stack| (stack.material.0, stack.units))
                        .collect();
                    frame::write(&mut send, &ServerMessage::InventoryUpdate { stacks }).await?;
                }
                // Follow the player before serving chunks, so a move and the
                // terrain it needs happen on the same beat rather than a tick
                // apart. THIS is what makes the world stream as you walk: until
                // Task 09 the interest set was pinned to spawn, because nothing
                // knew where the player was.
                if let Some(uuid) = session.uuid()
                    && let Some(chunk) = shared.player_chunk(&uuid)
                    && let Some(streamer) = streamer.as_mut()
                {
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
                        if session.phase() == tiamot_core::session::Phase::InWorld {
                            frame::write(&mut send, &outbound).await?;
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
                allowlist: &allowlist,
                max_players: shared.max_players,
                current_players: shared.players.load(Ordering::Acquire),
                spawn: shared.spawn,
                tick: shared.tick(),
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
            streamer = Some(Streamer::new(shared.spawn.chunk(), shared.view_distance));
            info!(
                player = session.display_name().unwrap_or("<unnamed>"),
                uuid = session.uuid().map(|id| id.short()).unwrap_or_default(),
                "player joined"
            );
        }

        for outbound in &response.send {
            frame::write(&mut send, outbound).await?;
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
            Action::Chat { text } => {
                if let Some(uuid) = session.uuid() {
                    shared.broadcast(ServerMessage::Chat {
                        from: Some(*uuid.as_bytes()),
                        text: text.clone(),
                    });
                }
            }
            // Filed under the tick it claims, not applied here. The simulation
            // takes one input per tick per player, in a fixed order — see the
            // player step in `handle.rs` for why that cannot happen on this
            // task.
            Action::Input {
                tick,
                movement,
                actions,
                ..
            } => {
                if let Some(uuid) = session.uuid() {
                    // A refusal is ordinary traffic: a duplicate from the
                    // three-input redundancy, or an input whose tick has
                    // already been simulated.
                    shared.queue_input(&uuid, *tick, intent_from_wire(*movement, *actions));
                }
            }
            Action::Dig { target } => {
                if let Some(uuid) = session.uuid() {
                    shared.set_dig(&uuid, *target);
                }
            }
            Action::SelectTool { tool } => {
                if let Some(uuid) = session.uuid() {
                    shared.select_tool(&uuid, tool.clone());
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

/// Requests, collects, and sends chunks for one connection.
///
/// Split out of the connection loop because it is the only part with an
/// interesting failure mode: a request that is answered but never accounted
/// for leaks the client's budget, and the leak is silent — the player's world
/// simply stops filling in.
async fn pump_chunks(
    streamer: &mut Streamer,
    pending: &mut Vec<(
        tiamot_core::ChunkPos,
        tokio::sync::oneshot::Receiver<Option<Vec<u8>>>,
    )>,
    shared: &Shared,
    send: &mut quinn::SendStream,
) -> Result<(), frame::FrameError> {
    // Deliveries first, so budget freed this pass can be spent this pass.
    let mut still_waiting = Vec::with_capacity(pending.len());
    for (pos, mut receiver) in pending.drain(..) {
        match receiver.try_recv() {
            Ok(Some(blob)) => {
                frame::write(send, &ServerMessage::ChunkData { pos, blob }).await?;
                // `delivered` clears the in-flight entry too, so the chunk goes
                // straight from "requested" to "held" with no window in which
                // it looks un-requested and gets asked for again.
                streamer.delivered(pos);
            }
            Ok(None) => {
                // The simulation could not produce it. Cleared exactly like a
                // success, and NOT marked delivered, so the next pass asks
                // again.
                streamer.completed(pos);
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                still_waiting.push((pos, receiver));
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

    // Then new requests, up to this client's share.
    let budget = streamer.budget(CHUNKS_IN_FLIGHT_PER_CLIENT);
    for pos in streamer.next_needed(budget) {
        let Some(receiver) = shared.request_chunk(pos) else {
            // Queue full. Nothing is marked, so the next pass retries.
            break;
        };
        streamer.requested(pos);
        pending.push((pos, receiver));
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
            allowlist: std::sync::RwLock::new(Allowlist::open()),
            max_players: 2,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: Control::new(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            outbound: tokio::sync::broadcast::channel(16).0,
            chunk_requests: std::sync::Mutex::new(std::collections::VecDeque::new()),
            view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
            content: tiamot_core::content::ContentIndex::new(),
            inventories: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            inventory_dirty: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            kicks: tokio::sync::broadcast::channel(4).0,
            online: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            bodies: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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
