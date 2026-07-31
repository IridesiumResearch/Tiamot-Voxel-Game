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
            () = tokio::time::sleep(tiamot_core::tick::TICK_DURATION), if streamer.is_some() => {
                if let Some(streamer) = streamer.as_mut()
                    && let Err(err) = pump_chunks(streamer, &mut pending, shared, &mut send).await
                {
                    return Err(err);
                }
                continue;
            }
            incoming = frame::read(&mut recv) => match incoming {
                Ok(message) => message,
                Err(err) if err.is_clean_close() => return Ok(()),
                Err(err) => {
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
            // Movement is applied by the simulation from the input queue, which
            // Task 09 adds along with the physics that consume it.
            Action::Input { .. } | Action::None => {}
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
            allowlist: std::sync::RwLock::new(Allowlist::open()),
            max_players: 2,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: Control::new(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            outbound: tokio::sync::broadcast::channel(16).0,
            chunk_requests: std::sync::Mutex::new(std::collections::VecDeque::new()),
            view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
            kicks: tokio::sync::broadcast::channel(4).0,
            online: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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
