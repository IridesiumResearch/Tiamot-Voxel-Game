// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The client's connection to a server.
//!
//! # The render loop never blocks on the network
//!
//! A tokio runtime on its own thread owns the connection; the render loop drains
//! an [`Event`] queue once a frame and pushes [`Command`]s back. Frame *pacing*
//! is the metric this project cares about (charter rule 18), and a loop that
//! could stall on a packet has no pacing to measure.
//!
//! Everything expensive happens on that thread: zstd decompression, chunk
//! decoding, PNG decoding, hashing. What reaches the render loop is finished
//! work.
//!
//! # The reader is not the writer
//!
//! Reading happens continuously, independent of anything the client wants to
//! say. The bot learned this the hard way in Task 07: a client that only read
//! when it had something to do let the server's broadcast back up until QUIC
//! flow control stopped the server writing, at which point the server stopped
//! draining its side and both ends waited for each other. Linux socket buffers
//! happened to absorb it and Windows CI did not.
//!
//! # Everything the server says is hostile input
//!
//! Charter rule 14. Chunk blobs decode through the same bounded decoder the
//! world file uses, content is verified against the hash it was requested by
//! before it is written to the cache, and PNG decoding is panic-isolated with
//! limits set before allocation. A malformed asset becomes a magenta checker
//! and a warning; it never takes the client down.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;

use rustls_pki_types::CertificateDer;
use tiamot_core::identity::{Identity, challenge_payload};
use tiamot_core::proto::{
    ClientMessage, ContentHash, DisconnectReason, Edit, MaterialDef, PROTOCOL_VERSION,
    ServerMessage, WireSignature,
};
use tiamot_core::{BlockPos, Chunk, ChunkPos};
use tokio::sync::mpsc;

use crate::cache::ContentCache;
use crate::texture::{Image, decode_or_missing};
use crate::trust::{TrustStore, to_hex};

/// The ALPN the server requires. Must match `server::transport::endpoint`.
const ALPN: &[u8] = b"tiamot/1";

/// How long to wait for content before entering the world without it.
///
/// A server that never finishes a transfer must not leave a player staring at a
/// loading screen. Going in with placeholder textures is worse than going in
/// with the right ones and far better than not going in.
const CONTENT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long to wait for a QUIC handshake before giving up.
///
/// QUIC's own idle timeout is generous, and a client that inherited it sits for
/// half a minute on a mistyped address looking indistinguishable from one that
/// has hung. Ten seconds is far longer than any handshake that is going to
/// succeed and short enough that "it is not there" arrives while the player is
/// still looking at the screen.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Anything that stops a connection being established.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The QUIC endpoint could not be created or the connection failed.
    #[error("could not connect to {address}: {reason}")]
    Connect {
        /// Where we tried.
        address: String,
        /// Why it failed.
        reason: String,
    },

    /// The server's certificate did not match the one pinned for this address.
    #[error(
        "the certificate for {address} has CHANGED.\n  pinned:    {expected}\n  presented: \
         {actual}\nThis is either an operator who regenerated the server's certificate or \
         somebody sitting between you and it, and a client cannot tell which. If you know it was \
         the former, remove this address from `{store}` and reconnect."
    )]
    FingerprintChanged {
        /// The server.
        address: String,
        /// What was pinned.
        expected: String,
        /// What arrived.
        actual: String,
        /// Where the pin is recorded.
        store: String,
    },

    /// The background thread could not be started.
    #[error("could not start the network thread")]
    Thread(#[source] std::io::Error),
}

/// Something that happened on the connection.
///
/// Delivered in order. The render loop applies them to its world and its
/// renderer; nothing here needs interpreting beyond that.
#[derive(Debug)]
pub enum Event {
    /// The connection is up and the certificate has been accepted.
    Connected {
        /// The address connected to.
        address: String,
        /// The certificate fingerprint, as hex, for display.
        fingerprint: String,
        /// Whether this was the first time this address was seen.
        first_use: bool,
    },

    /// The material table and every texture that could be resolved for it.
    ///
    /// Sent once, before [`Event::Joined`], because a renderer cannot build an
    /// atlas without it.
    Materials {
        /// The table, in ascending world-id order.
        table: Vec<MaterialDef>,
        /// Decoded textures by material id. Missing entries draw the placeholder.
        images: BTreeMap<u16, Image>,
    },

    /// Every tool the server's mods registered.
    ///
    /// Sent once, on join. Charter rule 1: the engine has no tools of its own,
    /// so this is the only way a client learns that a chisel exists.
    Tools {
        /// The tools, in ascending id order.
        tools: Vec<tiamot_core::proto::ToolDef>,
    },

    /// Every sound the server's mods registered.
    ///
    /// Sent once, on join. The files arrive afterwards through the content
    /// pipeline, by hash, exactly as textures do.
    Sounds {
        /// The sounds, in mod load order.
        sounds: Vec<tiamot_core::proto::SoundDef>,
    },

    /// A sound has been fetched and decoded, and can now be played.
    ///
    /// Arrives after the join rather than before it: a client should be in the
    /// world while its audio is still loading, not staring at nothing until a
    /// sound file it may never hear has arrived.
    SoundReady {
        /// The qualified sound id.
        id: String,
        /// The decoded samples.
        clip: crate::audio::Clip,
    },

    /// Something happened near enough to hear.
    ///
    /// The server has already decided this player is in earshot; what it
    /// sounds like from where they are standing is the client's business.
    PlaySound {
        /// The qualified sound id.
        sound: String,
        /// Where it happened, in world blocks.
        pos: [f64; 3],
        /// How far it carries, for attenuation.
        radius: f32,
        /// How loud, multiplying the sound's registered gain.
        gain: f32,
        /// An entity to follow, if it should move.
        entity: Option<u64>,
    },

    /// Every action the server's mods registered.
    ///
    /// Sent once, on join. The engine's own actions are not in here — the
    /// client already has those and a server does not get to say what jump
    /// means (charter rule 11).
    Actions {
        /// The actions, in mod load order.
        actions: Vec<tiamot_core::proto::ActionDef>,
    },

    /// What the player is carrying, in **units** (charter rule 5).
    ///
    /// Whole, not a delta: an inventory is tens of stacks at most, and a delta
    /// stream that ever dropped a message would leave the client permanently
    /// wrong with no way to notice.
    Inventory {
        /// Material id and unit count, in ascending material order.
        stacks: Vec<(u16, u32)>,
    },

    /// The player is in the world.
    Joined {
        /// The identity the server resolved.
        uuid: [u8; 32],
        /// Where the player starts.
        spawn: BlockPos,
        /// The server's tick when it said so.
        tick: u64,
    },

    /// A decoded chunk.
    ///
    /// Boxed: a `Chunk` is far larger than every other variant, and an unboxed
    /// one would make the whole queue pay for it.
    Chunk(Box<Chunk>),

    /// A chunk's light levels, initial or updated.
    ///
    /// Boxed for the same reason [`Event::Chunk`] is, if less dramatically: a
    /// dense layer is 8 KiB and every other variant is a handful of bytes.
    ChunkLight(ChunkPos, Box<tiamot_core::light::LightLayer>),

    /// A chunk's fluid, as a whole layer.
    ///
    /// Boxed like the light layer, and for the same reason. Every update is a
    /// full layer rather than a delta — see `ServerMessage::ChunkFluid` — so
    /// applying one is a replacement rather than a merge, and a client that
    /// missed the last one is repaired by this one.
    ChunkFluid(ChunkPos, Box<tiamot_core::fluid::FluidLayer>),

    /// Entities that have come into view.
    ///
    /// Reliable: a client that missed one holds an id it can never draw. A
    /// spawn for an entity already known REPLACES it, which is the recovery
    /// path when a server's queue to this client overflowed and it re-sent
    /// everything from scratch.
    EntitySpawn(Vec<tiamot_core::proto::EntityDef>),

    /// Entities that have left view or stopped existing.
    ///
    /// One event for both, because a client cannot do anything different about
    /// them — see `ServerMessage::EntityDespawn`.
    EntityDespawn(Vec<u64>),

    /// Where the entities in view are now.
    ///
    /// Superseded by the next one 50 ms later, which is what makes it safe to
    /// lose. `tick` orders them; it is not a clock — see
    /// `crate::entities` for why the interpolation buffer stamps arrival time
    /// instead.
    EntityState {
        /// Server tick, for ordering.
        tick: u64,
        /// One entry per entity that moved.
        entities: Vec<tiamot_core::proto::EntityDelta>,
    },

    /// Every fluid the server's mods registered, sent once on join.
    ///
    /// Without it a chunk's fluid names a number the client cannot draw — see
    /// `ServerMessage::FluidTable`.
    Fluids {
        /// In ascending id order.
        fluids: Vec<tiamot_core::proto::FluidDef>,
    },

    /// How far the server is actually streaming.
    ///
    /// The GRANTED radius, not the one this client asked for — the server
    /// clamps to its own limit. The fog is drawn from this, so a client that
    /// asked for more than it was given still ends the world in haze rather
    /// than in clear air.
    ViewDistance {
        /// Chunks of horizontal radius.
        horizontal: u8,
        /// Chunks of vertical radius.
        vertical: u8,
    },

    /// The sky a mod registered, sent once on join.
    Sky(crate::sky::Sky),

    /// Where the server's clock stands in the day, `0.0..1.0`.
    TimeOfDay(f32),

    /// A chunk left the interest set.
    ChunkUnload(ChunkPos),

    /// A block or sub-node changed.
    Edit(Edit),

    /// A chat line.
    Chat {
        /// Who said it, or `None` for the server.
        from: Option<[u8; 32]>,
        /// What they said.
        text: String,
    },

    /// Where the server says the player is.
    ///
    /// Forwarded rather than acted on here: reconciliation needs the chunk
    /// store, and this task has no business touching it.
    PlayerState(crate::predict::Authoritative),

    /// How far along the local player's dig is, for the crack overlay.
    DigProgress {
        /// Which cell is being broken.
        target: tiamot_core::SubNodePos,
        /// How far along, `0.0..=1.0`.
        progress: f32,
    },

    /// Something went wrong that the player should see but that is not fatal.
    ///
    /// A poisoned texture, a chunk that would not decode, a content transfer
    /// that timed out. Charter rule 14 asks for a per-server warning rather
    /// than a crash, and this is how it reaches the HUD.
    Warning(String),

    /// The connection ended.
    Disconnected {
        /// Why, in words a player can act on.
        reason: String,
    },
}

/// Something the render loop wants to say.
#[derive(Debug, Clone)]
pub enum Command {
    /// Say something.
    Chat(String),
    /// Report this frame's movement intent.
    Input {
        /// The tick the client believes it is on.
        tick: u64,
        /// Movement axes.
        movement: [f32; 3],
        /// Yaw and pitch, in turns.
        look: [f32; 2],
        /// Held actions.
        actions: u32,
    },
    /// Start or re-aim a dig, or stop one with `None`.
    Dig {
        /// The cell under the crosshair, or `None` to cancel.
        target: Option<tiamot_core::SubNodePos>,
    },
    /// Hit an entity the crosshair is on.
    Punch {
        /// The entity, as the server named it when it spawned.
        entity: u64,
    },
    /// Ask the server for a streaming radius.
    ///
    /// A request. The server clamps it to its own configured maximum and
    /// answers with [`Event::ViewDistance`] carrying what was actually granted.
    /// May be sent at any time, so changing it does not need a reconnect.
    ViewDistance {
        /// Chunks of horizontal radius.
        horizontal: u8,
        /// Chunks of vertical radius.
        vertical: u8,
    },
    /// Choose the held tool.
    SelectTool {
        /// Qualified tool id, or `None` for a bare hand.
        tool: Option<String>,
    },
    /// Ask to place material from the inventory into a cell.
    ///
    /// A request: the server decides whether it happens, and says why when it
    /// does not. See `tiamot_core::place`.
    Place {
        /// The cell to fill, already stepped across the face being looked at.
        target: tiamot_core::SubNodePos,
        /// Which material, as a world material id.
        material: u16,
    },

    /// Report that a mod-registered action was pressed or released.
    ///
    /// Only actions a server registered — the engine's own controls travel as
    /// movement, digging and placement, all of which the server already judges.
    Action {
        /// The qualified action id.
        id: String,
        /// Whether it went down or came up.
        pressed: bool,
    },
    /// Leave.
    Disconnect,

    /// Start simulating a bad network on everything sent from here on.
    ///
    /// **A test affordance, and the only `Command` that never reaches the
    /// wire.** It exists because impairing from the moment the socket opens
    /// makes the *handshake* lossy, and the handshake has no retries —
    /// `Hello`, `AuthResponse` and `JoinWorld` are each sent exactly once, so a
    /// 5% drop
    /// there is a 5% chance the client simply never joins. Real loss does not
    /// work that way: QUIC retransmits, and what this simulates is a message
    /// the application never sends again. So a test joins cleanly and impairs
    /// afterwards, exactly as the bot harness does.
    Impair(tiamot_server::transport::Impairment),
}

/// A live connection, as the render loop sees it.
///
/// `Debug` is hand-written rather than derived: a derived one would print the
/// channel internals and the whole tokio runtime, which is pages of noise
/// wherever a test unwraps a `Result<Connection, NetError>`.
pub struct Connection {
    events: mpsc::UnboundedReceiver<Event>,
    commands: mpsc::UnboundedSender<Command>,
    /// Kept so dropping the connection stops the runtime that drives it.
    ///
    /// `Option` so [`Connection::shutdown`] can take it and drop it on a thread
    /// that is not inside an async context — dropping a tokio runtime from
    /// within one panics.
    runtime: Option<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("running", &self.runtime.is_some())
            .field("events_waiting", &self.events.len())
            .field("open", &!self.commands.is_closed())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Connects and runs the join flow in the background.
    ///
    /// Returns as soon as the transport is up; the join flow's progress arrives
    /// as [`Event`]s.
    ///
    /// # Errors
    ///
    /// [`NetError::Connect`] if the transport fails,
    /// [`NetError::FingerprintChanged`] if the server presented a certificate
    /// other than the one pinned for its address.
    pub fn open(
        address: SocketAddr,
        identity: Identity,
        display_name: String,
        cache: ContentCache,
        trust_path: &std::path::Path,
    ) -> Result<Self, NetError> {
        Self::open_impaired(
            address,
            identity,
            display_name,
            cache,
            trust_path,
            tiamot_server::transport::Impairment::default(),
        )
    }

    /// Opens a connection over a deliberately bad network.
    ///
    /// **For tests, and only for tests.** Prediction and reconciliation are
    /// correct on loopback by construction — the round trip is microseconds, so
    /// the client is barely ahead of the server and there is nothing to
    /// reconcile — which means every bug in them hides on the only network the
    /// suite has. See [`tiamot_server::transport::Impairment`].
    ///
    /// # Errors
    ///
    /// The same as [`Connection::open`].
    pub fn open_impaired(
        address: SocketAddr,
        identity: Identity,
        display_name: String,
        cache: ContentCache,
        trust_path: &std::path::Path,
        impairment: tiamot_server::transport::Impairment,
    ) -> Result<Self, NetError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tiamot-client-net")
            .build()
            .map_err(NetError::Thread)?;

        let (event_tx, events) = mpsc::unbounded_channel();
        let (commands, command_rx) = mpsc::unbounded_channel();

        // Connect on the runtime and wait for it, so a bad address or a changed
        // fingerprint is an error from `open` rather than a `Disconnected`
        // event a caller might not be listening for yet.
        let connected = runtime.block_on(connect(address, trust_path))?;

        runtime.spawn(session(
            connected,
            identity,
            display_name,
            cache,
            event_tx,
            command_rx,
            impairment,
        ));

        Ok(Self {
            events,
            commands,
            runtime: Some(runtime),
        })
    }

    /// Takes the next event, or `None` if none is waiting.
    ///
    /// Non-blocking on purpose: this is called from the render loop.
    pub fn poll(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Sends a command. Returns whether the connection is still up.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }

    /// Closes cleanly and waits for the runtime to stop.
    pub fn shutdown(mut self) {
        let _ = self.commands.send(Command::Disconnect);
        if let Some(runtime) = self.runtime.take() {
            // `shutdown_timeout` rather than a plain drop: a drop waits forever
            // for a task that is blocked, and a client quitting must quit.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        }
    }
}

/// What [`connect`] hands to the session task.
struct Connected {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    address: String,
    fingerprint: [u8; 32],
    first_use: bool,
}

/// Establishes the QUIC connection and settles the trust question.
async fn connect(address: SocketAddr, trust_path: &std::path::Path) -> Result<Connected, NetError> {
    let label = address.to_string();
    let mut store = TrustStore::load(trust_path);
    let pinned = store.pinned(&label);

    // The verifier records what the server presented. On a repeat visit it also
    // enforces the pin, so a mismatch fails during the TLS handshake rather
    // than after the client has already talked to whoever answered.
    let seen = Arc::new(Mutex::new(None));
    let verifier = Arc::new(RecordingVerifier {
        pinned,
        seen: Arc::clone(&seen),
    });

    let fail = |reason: String| NetError::Connect {
        address: label.clone(),
        reason,
    };

    // Bound to the same address family as the target: connecting to an IPv6
    // server from an IPv4 socket fails with an error that reads like the server
    // is down.
    let bind: SocketAddr = if address.is_ipv6() {
        "[::]:0"
            .parse()
            .map_err(|_| fail("bad bind address".into()))?
    } else {
        "0.0.0.0:0"
            .parse()
            .map_err(|_| fail("bad bind address".into()))?
    };

    let mut endpoint = quinn::Endpoint::client(bind).map_err(|err| fail(err.to_string()))?;
    endpoint.set_default_client_config(client_config(verifier));

    let attempt = endpoint
        .connect(address, "tiamot-server")
        .map_err(|err| fail(err.to_string()))?;

    let connection = tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| {
            fail(format!(
                "no answer within {} seconds. Check the address and port, and that the server is \
                 running.",
                CONNECT_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|err| {
            // A pin mismatch surfaces from quinn as a generic handshake
            // failure, which tells a player nothing. If the verifier saw a
            // fingerprint that is not the pinned one, say so instead.
            let presented = seen.lock().ok().and_then(|seen| *seen);
            match (pinned, presented) {
                (Some(expected), Some(actual)) if expected != actual => {
                    NetError::FingerprintChanged {
                        address: label.clone(),
                        expected: to_hex(&expected),
                        actual: to_hex(&actual),
                        store: trust_path.display().to_string(),
                    }
                }
                _ => fail(err.to_string()),
            }
        })?;

    let fingerprint = seen
        .lock()
        .ok()
        .and_then(|seen| *seen)
        .or(pinned)
        .unwrap_or([0u8; 32]);

    if pinned.is_none() {
        // First use: remember it. A failure to write is a warning rather than a
        // refusal — a read-only data directory should not stop someone playing,
        // it should stop them being protected, and they should hear about it.
        store.remember(&label, fingerprint);
        if let Err(err) = store.save() {
            tracing::warn!("could not record the server's fingerprint: {err}");
        }
    }
    let first_use = pinned.is_none();

    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|err| fail(err.to_string()))?;

    Ok(Connected {
        endpoint,
        connection,
        send,
        recv,
        address: label,
        fingerprint,
        first_use,
    })
}

/// State the join flow accumulates before the world.
#[derive(Default)]
struct Pending {
    table: Vec<MaterialDef>,
    /// Partially received content, by hash.
    partial: BTreeMap<ContentHash, Vec<u8>>,
    /// Bytes for a hash, once complete and verified.
    complete: BTreeMap<ContentHash, Vec<u8>>,
    /// What was asked for and has not arrived.
    outstanding: usize,
    /// Whether `JoinWorld` has been sent.
    joined: bool,
}

/// Drives one connection until it ends.
#[expect(
    clippy::too_many_lines,
    reason = "the join flow is a linear sequence of message cases; splitting it into per-message \
              functions would thread six pieces of shared state through each of them and make the \
              ORDER — which is the security property, see `session` in core — harder to read \
              rather than easier"
)]
async fn session(
    connected: Connected,
    identity: Identity,
    display_name: String,
    cache: ContentCache,
    events: mpsc::UnboundedSender<Event>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    impairment: tiamot_server::transport::Impairment,
) {
    let Connected {
        endpoint,
        connection,
        send,
        mut recv,
        address,
        fingerprint,
        first_use,
    } = connected;

    // Everything this task writes goes through the link, so a test can make
    // the network bad without any of the code below knowing. Unimpaired — the
    // only case in production — it is a direct write.
    let mut send = tiamot_server::transport::Link::new(send);

    // The sound table, kept so an arriving content chunk can be matched to the
    // sound that wanted it.
    let mut awaited_sounds: Vec<tiamot_core::proto::SoundDef> = Vec::new();
    send.impair(impairment);

    let _ = events.send(Event::Connected {
        address,
        fingerprint: to_hex(&fingerprint),
        first_use,
    });

    let say = |text: String| {
        let _ = events.send(Event::Warning(text));
    };
    let finish = |reason: String| {
        let _ = events.send(Event::Disconnected { reason });
    };

    // Hello, and then everything else is driven by what arrives.
    if let Err(err) = send
        .write(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: *identity.public_key().as_bytes(),
            display_name,
        })
        .await
    {
        finish(format!("could not greet the server: {err}"));
        return;
    }

    let mut pending = Pending::default();
    // A passthrough map: a client has no `id_map` table to reconcile against,
    // so the world ids in a blob ARE the ids it works in. The names come from
    // the material table.
    let materials = tiamot_core::persist::idmap::MaterialMap::passthrough();
    let content_deadline = tokio::time::Instant::now() + CONTENT_DEADLINE;

    // Reading happens in its own task, feeding a channel — the same shape the
    // server's connection loop uses, and for the same reason.
    //
    // `frame::read` is NOT cancellation-safe: it reads a 4-byte length prefix
    // and then the body, as two sequential awaits. `tokio::select!` cancels the
    // branches that do not win, so an outbound command arriving between those
    // two reads discards the partially-read frame and leaves the stream
    // mid-message. Every later read then misinterprets body bytes as a length
    // prefix.
    //
    // The server was fixed for this in `cf2b7a4`; the client had the identical
    // pattern and simply never sent enough to trigger it. Task 09 made it send
    // an input every tick, and the symptom was that a client joined, received
    // its material table, and then silently received no chunks at all — the
    // stream had desynchronised on the first input it sent.
    //
    // A channel receive IS cancellation-safe, so selecting on one is correct.
    let (incoming_tx, mut incoming) = tokio::sync::mpsc::channel::<
        Result<ServerMessage, tiamot_server::transport::frame::FrameError>,
    >(64);
    let reader = tokio::spawn(async move {
        loop {
            let message =
                tiamot_server::transport::frame::read::<_, ServerMessage>(&mut recv).await;
            let failed = message.is_err();
            if incoming_tx.send(message).await.is_err() || failed {
                break;
            }
        }
    });

    loop {
        let message = tokio::select! {
            // Reading is biased first. A client that preferred its own outbound
            // traffic could let the server's broadcast back up until flow
            // control stalled both ends — the Task 07 bot bug, in the other
            // direction.
            biased;

            incoming = incoming.recv() => {
                let Some(incoming) = incoming else {
                    finish("the server closed the connection".to_owned());
                    break;
                };
                match incoming {
                    // Charter rule 14 in the direction that is easy to forget:
                    // a decoded message is not a trustworthy one. The server is
                    // as unvalidated a peer as the client is, and a `PlayerState`
                    // carrying a NaN would land in the client's own physics.
                    Ok(message) => match tiamot_core::proto::validate_server_message(&message) {
                        Ok(()) => Some(message),
                        Err(err) => {
                            finish(format!("the server sent a message that failed validation: {err}"));
                            break;
                        }
                    },
                    Err(err) if err.is_clean_close() => {
                        finish("the server closed the connection".to_owned());
                        break;
                    }
                    Err(err) => {
                        finish(format!("connection lost: {err}"));
                        break;
                    }
                }
            }

            command = commands.recv() => {
                match command {
                    Some(Command::Disconnect) | None => {
                        let _ = send.write(&ClientMessage::Disconnect).await;
                        send.finish();
                        connection.close(0u32.into(), b"bye");
                        endpoint.wait_idle().await;
                        finish("you left".to_owned());
                        break;
                    }
                    // Applied here rather than sent: see `Command::Impair`.
                    Some(Command::Impair(impairment)) => {
                        send.impair(impairment);
                        None
                    }
                    Some(command) => {
                        if let Err(err) = send.write(&to_wire(command)).await {
                            finish(format!("could not send to the server: {err}"));
                            break;
                        }
                        None
                    }
                }
            }

            // The content deadline. Without it a server that starts a transfer
            // and never finishes leaves the player on a loading screen forever.
            () = tokio::time::sleep_until(content_deadline),
                if !pending.joined && !pending.table.is_empty() =>
            {
                say(format!(
                    "{} of this server's textures did not arrive in time; those blocks will \
                     draw as missing-texture placeholders.",
                    pending.outstanding
                ));
                pending.outstanding = 0;
                None
            }
        };

        let Some(message) = message else {
            // A command was handled, or a timer fired. Either may have made the
            // materials complete.
            if !pending.joined && !pending.table.is_empty() && pending.outstanding == 0 {
                emit_materials(&mut pending, &events);
                if let Err(err) = send.write(&ClientMessage::JoinWorld).await {
                    finish(format!("could not ask to join: {err}"));
                    break;
                }
                pending.joined = true;
            }
            continue;
        };

        match message {
            ServerMessage::AuthChallenge { nonce } => {
                let signature =
                    identity.sign(&challenge_payload(&nonce, &fingerprint, PROTOCOL_VERSION));
                if let Err(err) = send
                    .write(&ClientMessage::AuthResponse {
                        signature: WireSignature(signature.to_bytes()),
                    })
                    .await
                {
                    finish(format!("could not answer the challenge: {err}"));
                    break;
                }
            }

            ServerMessage::MaterialTable { materials: table } => {
                pending.table = table;

                let wanted: Vec<ContentHash> = pending
                    .table
                    .iter()
                    .filter_map(|entry| entry.texture)
                    .collect();
                // Anything already cached is already done. This is the whole
                // point of content addressing: a texture seen on any previous
                // server, on any previous run, costs nothing here.
                for hash in &wanted {
                    if let Some(bytes) = cache.get(hash) {
                        pending.complete.insert(*hash, bytes);
                    }
                }
                let missing = cache.missing(&wanted);
                pending.outstanding = missing.len();

                if missing.is_empty() {
                    emit_materials(&mut pending, &events);
                    if let Err(err) = send.write(&ClientMessage::JoinWorld).await {
                        finish(format!("could not ask to join: {err}"));
                        break;
                    }
                    pending.joined = true;
                } else if let Err(err) = send
                    .write(&ClientMessage::ContentRequest { hashes: missing })
                    .await
                {
                    finish(format!("could not ask for content: {err}"));
                    break;
                }
            }

            ServerMessage::ContentChunk {
                hash,
                offset,
                total_len,
                data,
            } => {
                // A sound's file may be what just arrived. Checked before the
                // material bookkeeping below, which only knows about textures.
                if let Some(sound) = awaited_sounds.iter().find(|sound| sound.file == Some(hash)) {
                    let sound = sound.clone();
                    let cache = cache.clone();
                    let events = events.clone();
                    // Deferred by a beat so the slice is written first; the
                    // helper simply does nothing if it is not there yet.
                    tokio::spawn(async move {
                        decode_when_ready(&sound, &cache, &events);
                    });
                }
                match accept_slice(&mut pending, &cache, hash, offset, total_len, &data) {
                    Ok(true) => pending.outstanding = pending.outstanding.saturating_sub(1),
                    Ok(false) => {}
                    Err(reason) => {
                        pending.outstanding = pending.outstanding.saturating_sub(1);
                        say(reason);
                    }
                }

                if !pending.joined && pending.outstanding == 0 && !pending.table.is_empty() {
                    emit_materials(&mut pending, &events);
                    if let Err(err) = send.write(&ClientMessage::JoinWorld).await {
                        finish(format!("could not ask to join: {err}"));
                        break;
                    }
                    pending.joined = true;
                }
            }

            ServerMessage::JoinWorld {
                player_uuid,
                spawn,
                tick,
            } => {
                let _ = events.send(Event::Joined {
                    uuid: player_uuid,
                    spawn,
                    tick,
                });
            }

            ServerMessage::ChunkData { pos, blob } => {
                // The same bounded decoder the world file uses. A blob that
                // does not decode costs one chunk and a warning; there is no
                // version of this that should end a session.
                match tiamot_core::persist::codec::decode_chunk(pos, &blob, &materials, &[]) {
                    Ok(chunk) => {
                        let _ = events.send(Event::Chunk(Box::new(chunk)));
                    }
                    Err(err) => say(format!(
                        "the server sent a chunk at {pos:?} that would not decode: {err}"
                    )),
                }
            }

            ServerMessage::ChunkLight { pos, light } => {
                // Hostile input, charter rule 14. A payload that will not
                // decode costs that chunk its light and earns a warning; it
                // does not end the session, and it does not reach the store
                // half-applied.
                match tiamot_core::light::codec::decode(&light) {
                    Ok(layer) => {
                        let _ = events.send(Event::ChunkLight(pos, Box::new(layer)));
                    }
                    Err(err) => say(format!(
                        "the server sent light for {pos:?} that would not decode: {err}"
                    )),
                }
            }

            ServerMessage::SkyTable {
                day_length_ticks,
                keyframes,
            } => {
                let _ = events.send(Event::Sky(crate::sky::Sky::new(
                    day_length_ticks,
                    keyframes,
                )));
            }

            ServerMessage::TimeOfDay { time } => {
                let _ = events.send(Event::TimeOfDay(time));
            }

            ServerMessage::EntitySpawn { entities } => {
                let _ = events.send(Event::EntitySpawn(entities));
            }

            ServerMessage::EntityDespawn { entities } => {
                let _ = events.send(Event::EntityDespawn(entities));
            }

            ServerMessage::EntityState { tick, entities } => {
                let _ = events.send(Event::EntityState { tick, entities });
            }

            ServerMessage::ChunkFluid { pos, fluid } => {
                // **Decoded here, on the network task, and not on the frame
                // loop.** Charter rule 14: this is hostile input, and the
                // decoder is where the bounds are. Doing it here also keeps a
                // malformed payload from costing a frame — a warning and a
                // dropped message rather than a hitch.
                match tiamot_core::fluid::codec::decode(&fluid) {
                    Ok(layer) => {
                        let _ = events.send(Event::ChunkFluid(pos, Box::new(layer)));
                    }
                    Err(err) => say(format!(
                        "the server sent fluid for {pos:?} that would not decode: {err}"
                    )),
                }
            }

            ServerMessage::ChunkUnload { pos } => {
                let _ = events.send(Event::ChunkUnload(pos));
            }

            ServerMessage::BlockDelta { edit, .. } => {
                let _ = events.send(Event::Edit(edit));
            }

            ServerMessage::Chat { from, text } => {
                let _ = events.send(Event::Chat { from, text });
            }

            ServerMessage::Disconnect { reason } => {
                finish(describe(&reason));
                break;
            }

            // Correct messages this client has nothing to do with yet, and
            // each for its own reason. `HelloAck` is acknowledged by the
            // challenge that follows it. `ModManifest` names content the
            // material table fetches by hash instead, and nothing needs the mod
            // list until client-side scripts exist. Entity state waits for Task
            // 12. They are ignored rather than warned about — a warning here
            // would mean the client complaining about a server behaving
            // correctly.
            ServerMessage::PlayerState {
                last_processed_input,
                chunk,
                local,
                velocity,
                on_ground,
            } => {
                let _ = events.send(Event::PlayerState(crate::predict::Authoritative {
                    last_processed_input,
                    chunk,
                    local,
                    velocity,
                    on_ground,
                }));
            }

            ServerMessage::DigProgress { target, progress } => {
                let _ = events.send(Event::DigProgress { target, progress });
            }

            ServerMessage::InventoryUpdate { stacks } => {
                let _ = events.send(Event::Inventory { stacks });
            }

            ServerMessage::ToolTable { tools } => {
                let _ = events.send(Event::Tools { tools });
            }

            ServerMessage::ActionTable { actions } => {
                let _ = events.send(Event::Actions { actions });
            }

            ServerMessage::SoundTable { sounds } => {
                // **Fetched after the join, not before it.** The material
                // textures gate the join because a world drawn without them is
                // a grid of grey; a world with no sound yet is merely quiet.
                let wanted: Vec<tiamot_core::proto::ContentHash> =
                    sounds.iter().filter_map(|sound| sound.file).collect();
                let missing = cache.missing(&wanted);
                for sound in &sounds {
                    decode_when_ready(sound, &cache, &events);
                }
                if !missing.is_empty()
                    && let Err(err) = send
                        .write(&ClientMessage::ContentRequest { hashes: missing })
                        .await
                {
                    say(format!("could not ask for sounds: {err}"));
                }
                awaited_sounds = sounds.clone();
                let _ = events.send(Event::Sounds { sounds });
            }

            ServerMessage::PlaySound {
                sound,
                pos,
                radius,
                gain,
                entity,
            } => {
                let _ = events.send(Event::PlaySound {
                    sound,
                    pos,
                    radius,
                    gain,
                    entity,
                });
            }

            ServerMessage::FluidTable { fluids } => {
                let _ = events.send(Event::Fluids { fluids });
            }

            ServerMessage::ViewDistance {
                horizontal,
                vertical,
            } => {
                let _ = events.send(Event::ViewDistance {
                    horizontal,
                    vertical,
                });
            }

            ServerMessage::HelloAck { .. }
            | ServerMessage::ModManifest { .. }
            | ServerMessage::EntityStateDelta { .. } => {}
        }
    }

    // The reader borrows the receive stream, so it would otherwise outlive this
    // function and hold the connection open after the player has left.
    reader.abort();
}

/// Accepts one content slice, returning whether it completed an item.
///
/// An `Err` names something the player should see: the server sent slices out
/// of order, or bytes that do not hash to what was asked for.
fn accept_slice(
    pending: &mut Pending,
    cache: &ContentCache,
    hash: ContentHash,
    offset: u64,
    total_len: u64,
    data: &[u8],
) -> Result<bool, String> {
    // Bounded before the allocation, not after: `total_len` is a claim.
    if total_len > crate::cache::MAX_ITEM_BYTES {
        return Err(format!(
            "the server offered a {total_len}-byte asset, over the {}-byte limit; it was refused \
             before anything was allocated for it.",
            crate::cache::MAX_ITEM_BYTES
        ));
    }

    let plain = zstd::decode_all(data).map_err(|err| {
        format!("the server sent a content slice that would not decompress: {err}")
    })?;

    let buffer = pending.partial.entry(hash).or_default();
    if buffer.len() as u64 != offset {
        let held = buffer.len();
        pending.partial.remove(&hash);
        return Err(format!(
            "the server sent content slices out of order (offset {offset} with {held} bytes \
             held); that asset was discarded."
        ));
    }
    buffer.extend_from_slice(&plain);

    if (buffer.len() as u64) < total_len {
        return Ok(false);
    }

    let bytes = pending.partial.remove(&hash).unwrap_or_default();
    // The hash is the only part of the server's claim that can be checked. A
    // server that sent different bytes than were asked for is caught here
    // rather than by the decoder they were aimed at.
    if let Err(err) = cache.put(&hash, &bytes) {
        return Err(format!("a server-pushed asset was rejected: {err}"));
    }
    pending.complete.insert(hash, bytes);
    Ok(true)
}

/// Decodes every gathered texture and emits the materials event.
///
/// Decoding happens here, on the network thread, rather than in the render
/// loop: it is unbounded work on untrusted input, and the render loop's job is
/// to keep pace.
/// Decodes a sound if its file is in the cache, and says so.
///
/// **Charter rule 14's worker with panic isolation.** The decode happens off
/// this task — a poisoned asset must not stall a connection — and through
/// `decode_isolated`, so a panic disables that one sound rather than the
/// client. The warning is per server and per sound, which is what the rule
/// asks for.
fn decode_when_ready(
    sound: &tiamot_core::proto::SoundDef,
    cache: &ContentCache,
    events: &mpsc::UnboundedSender<Event>,
) {
    let Some(hash) = sound.file else {
        // A mod named a file that is not in its directory. The server already
        // logged it; there is nothing here to fetch.
        return;
    };
    let Some(bytes) = cache.get(&hash) else {
        return;
    };
    let id = sound.id.clone();
    let events = events.clone();
    // A real worker, not this task: decoding a minute of audio is milliseconds
    // of work that has no business inside a network read loop.
    tokio::task::spawn_blocking(move || {
        match crate::audio::decode_isolated(&bytes, crate::audio::Limits::default()) {
            Ok(clip) => {
                let _ = events.send(Event::SoundReady { id, clip });
            }
            Err(err) => {
                let _ = events.send(Event::Warning(format!(
                    "sound `{id}` could not be decoded and is disabled: {err}"
                )));
            }
        }
    });
}

fn emit_materials(pending: &mut Pending, events: &mpsc::UnboundedSender<Event>) {
    let mut images = BTreeMap::new();
    for entry in &pending.table {
        let Some(hash) = entry.texture else { continue };
        let Some(bytes) = pending.complete.get(&hash) else {
            continue;
        };
        // Panic-isolated, limits applied before decode. A poisoned texture
        // disables that texture with a reason, never the client (charter
        // rule 14).
        let (image, failure) = decode_or_missing(bytes);
        if let Some(err) = failure {
            let _ = events.send(Event::Warning(format!(
                "`{}` has an unusable texture and will draw as a magenta checker: {err}",
                entry.name
            )));
        }
        images.insert(entry.id, image);
    }

    let _ = events.send(Event::Materials {
        table: std::mem::take(&mut pending.table),
        images,
    });
    pending.complete.clear();
    pending.partial.clear();
}

/// A command as it goes on the wire.
fn to_wire(command: Command) -> ClientMessage {
    match command {
        Command::Chat(text) => ClientMessage::Chat { text },
        Command::Input {
            tick,
            movement,
            look,
            actions,
        } => ClientMessage::PlayerInput {
            tick,
            movement,
            look,
            actions,
        },
        // Handled before this is reached; the connection is torn down rather
        // than a message written.
        Command::Dig {
            target: Some(target),
        } => ClientMessage::StartDig { target },
        Command::Dig { target: None } => ClientMessage::CancelDig,
        Command::Punch { entity } => ClientMessage::Punch { entity },
        Command::SelectTool { tool } => ClientMessage::SelectTool { tool },
        Command::ViewDistance {
            horizontal,
            vertical,
        } => ClientMessage::ViewDistance {
            horizontal,
            vertical,
        },
        Command::Place { target, material } => ClientMessage::Place { target, material },
        Command::Action { id, pressed } => ClientMessage::Action { id, pressed },
        Command::Disconnect => ClientMessage::Disconnect,
        // Unreachable: the session loop applies it and never gets here. A
        // panic rather than a placeholder message, because sending SOMETHING
        // for it would be a silent protocol violation.
        Command::Impair(_) => unreachable!("Command::Impair is applied, never sent"),
    }
}

/// A disconnect reason, in words a player can act on.
fn describe(reason: &DisconnectReason) -> String {
    match reason {
        DisconnectReason::VersionMismatch { server, client } => format!(
            "this server speaks protocol v{server} and this client speaks v{client}. One of you \
             needs updating."
        ),
        DisconnectReason::AuthFailed { detail } => format!("the server refused your key: {detail}"),
        DisconnectReason::NameTaken { name } => {
            format!("`{name}` is already claimed on this server by someone else's identity")
        }
        DisconnectReason::NotAllowlisted => {
            "this server has an allowlist and your identity is not on it".to_owned()
        }
        DisconnectReason::ServerFull { max_players } => {
            format!("the server is full ({max_players} players)")
        }
        DisconnectReason::ProtocolError { detail } => {
            format!("the server rejected something this client sent: {detail}")
        }
        DisconnectReason::Kicked { reason } => format!("you were kicked: {reason}"),
        DisconnectReason::ServerStopping => "the server is shutting down".to_owned(),
    }
}

/// A quinn client config using the given certificate verifier.
fn client_config(
    verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
) -> quinn::ClientConfig {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 is supported by the ring provider")
    .dangerous()
    .with_custom_certificate_verifier(verifier)
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .expect("a TLS 1.3 client config is a valid QUIC client config");
    quinn::ClientConfig::new(Arc::new(tls))
}

/// Records the fingerprint a server presented, and enforces a pin if there is one.
///
/// This replaces CA validation rather than relaxing it. There is no CA in this
/// model — see [`crate::trust`] — so the question "who vouches for this key" has
/// no answer, and trust-on-first-use answers "is this the same key as last
/// time" instead. The signature check below is untouched: the peer must still
/// hold the key for the certificate it presented, or a copied certificate could
/// simply be replayed.
#[derive(Debug)]
struct RecordingVerifier {
    pinned: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
}

impl rustls::client::danger::ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = tiamot_server::cert::fingerprint_of(end_entity);
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(actual);
        }

        match self.pinned {
            // First use. Accepted, and `connect` records it.
            None => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Some(expected) if expected == actual => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            Some(_) => Err(rustls::Error::General(
                "server certificate does not match the fingerprint pinned for this address"
                    .to_owned(),
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General(
            "TLS 1.2 is not supported by this engine".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(u16, &str, Option<ContentHash>)]) -> Vec<MaterialDef> {
        entries
            .iter()
            .map(|(id, name, texture)| MaterialDef {
                id: *id,
                name: (*name).to_owned(),
                texture: *texture,
                step_sound: None,
            })
            .collect()
    }

    fn cache(name: &str) -> ContentCache {
        let dir = std::env::temp_dir().join("tiamot-net-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        ContentCache::open(&dir).expect("cache")
    }

    fn png(image: &Image) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&image.rgba).expect("data");
        drop(writer);
        bytes
    }

    #[test]
    fn a_complete_transfer_verifies_and_caches() {
        let cache = cache("complete");
        let bytes = png(&Image::white_with_border());
        let hash = tiamot_core::content::hash_bytes(&bytes);
        let compressed = zstd::encode_all(bytes.as_slice(), 3).expect("compress");

        let mut pending = Pending::default();
        let completed = accept_slice(
            &mut pending,
            &cache,
            hash,
            0,
            bytes.len() as u64,
            &compressed,
        )
        .expect("accepted");

        assert!(completed);
        assert!(cache.contains(&hash), "and it must be cached for next time");
    }

    #[test]
    fn a_server_that_sends_the_wrong_bytes_is_caught_by_the_hash() {
        // The one part of a server's claim a client can check for itself. A
        // client that skipped it would hand whatever arrived to a decoder.
        let cache = cache("wrong-bytes");
        let asked_for = tiamot_core::content::hash_bytes(b"the texture I asked for");
        let sent = b"something else".to_vec();
        let compressed = zstd::encode_all(sent.as_slice(), 3).expect("compress");

        let mut pending = Pending::default();
        let err = accept_slice(
            &mut pending,
            &cache,
            asked_for,
            0,
            sent.len() as u64,
            &compressed,
        )
        .expect_err("must be refused");

        assert!(err.contains("rejected"), "got {err}");
        assert!(!cache.contains(&asked_for));
    }

    #[test]
    fn slices_out_of_order_discard_the_item_rather_than_splicing_it() {
        // Accepting a slice at the wrong offset would assemble bytes in an
        // order the server never sent, and the hash check would then reject a
        // file the server transferred correctly — blaming the wrong thing.
        let cache = cache("out-of-order");
        let hash = tiamot_core::content::hash_bytes(b"whatever");
        let compressed = zstd::encode_all(&b"tail"[..], 3).expect("compress");

        let mut pending = Pending::default();
        let err = accept_slice(&mut pending, &cache, hash, 99, 200, &compressed)
            .expect_err("an offset past what we hold is a protocol error");
        assert!(err.contains("out of order"), "got {err}");
        assert!(
            pending.partial.is_empty(),
            "the partial item must be dropped"
        );
    }

    #[test]
    fn an_asset_larger_than_the_limit_is_refused_from_its_declared_length() {
        // The claim is refused before anything is allocated for it — the same
        // rule as the frame length prefix and the PNG header.
        let cache = cache("oversized");
        let hash = tiamot_core::content::hash_bytes(b"x");

        let mut pending = Pending::default();
        let err = accept_slice(
            &mut pending,
            &cache,
            hash,
            0,
            crate::cache::MAX_ITEM_BYTES + 1,
            &[],
        )
        .expect_err("must be refused");
        assert!(err.contains("before anything was allocated"), "got {err}");
    }

    #[test]
    fn a_poisoned_texture_becomes_a_placeholder_with_a_warning() {
        // Charter rule 14: a bad asset disables that asset, never the client.
        let (events, mut received) = mpsc::unbounded_channel();
        let garbage = b"this is not a PNG".to_vec();
        let hash = tiamot_core::content::hash_bytes(&garbage);

        let mut pending = Pending {
            table: table(&[(2, "core:white", Some(hash))]),
            ..Pending::default()
        };
        pending.complete.insert(hash, garbage);
        emit_materials(&mut pending, &events);

        let mut warned = false;
        let mut drew = false;
        while let Ok(event) = received.try_recv() {
            match event {
                Event::Warning(text) => {
                    assert!(
                        text.contains("core:white"),
                        "the warning must name the block: {text}"
                    );
                    warned = true;
                }
                Event::Materials { images, .. } => {
                    assert_eq!(
                        images.get(&2),
                        Some(&Image::missing()),
                        "an unusable texture must draw as the magenta checker"
                    );
                    drew = true;
                }
                _ => {}
            }
        }
        assert!(warned && drew, "expected both a warning and a placeholder");
    }

    #[test]
    fn a_material_with_no_texture_simply_has_no_image() {
        // `engine:air` has no texture and never will. An entry for it would be
        // an atlas tile nothing samples.
        let (events, mut received) = mpsc::unbounded_channel();
        let mut pending = Pending {
            table: table(&[(0, "engine:air", None)]),
            ..Pending::default()
        };
        emit_materials(&mut pending, &events);

        let Some(Event::Materials { images, table }) = received.try_recv().ok() else {
            panic!("expected a materials event");
        };
        assert!(images.is_empty());
        assert_eq!(table.len(), 1, "but the table itself still names it");
    }

    #[test]
    fn every_disconnect_reason_says_something_a_player_can_act_on() {
        // A reason a player cannot act on is a bug report that says "it did not
        // work". Every variant gets a sentence.
        for reason in [
            DisconnectReason::VersionMismatch {
                server: 3,
                client: 2,
            },
            DisconnectReason::AuthFailed {
                detail: "bad signature".to_owned(),
            },
            DisconnectReason::NameTaken {
                name: "Alice".to_owned(),
            },
            DisconnectReason::NotAllowlisted,
            DisconnectReason::ServerFull { max_players: 50 },
            DisconnectReason::ProtocolError {
                detail: "malformed".to_owned(),
            },
            DisconnectReason::Kicked {
                reason: "spam".to_owned(),
            },
            DisconnectReason::ServerStopping,
        ] {
            let text = describe(&reason);
            assert!(
                text.len() > 20 && !text.contains("Reason"),
                "{reason:?} produced an unhelpful message: {text}"
            );
        }
    }
}
