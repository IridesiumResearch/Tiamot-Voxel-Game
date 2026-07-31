// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Running a server in-process.
//!
//! Charter rule 2: the server is the game, and singleplayer is this server on
//! loopback. [`ServerHandle::start`] is the one startup path — the standalone
//! binary calls it and waits for a signal, the client calls it and keeps the
//! handle. There is no second code path that only singleplayer takes, because a
//! second path is a second set of bugs that only appear in one mode.
//!
//! # Why the transport gets its own runtime
//!
//! quinn is async; the simulation is emphatically not. Ticks run on a dedicated
//! OS thread with no executor under them, because a tick that can yield in the
//! middle is a tick whose result depends on the scheduler — the opposite of
//! what charter rule 4 requires. The two communicate through shared state, not
//! by one driving the other.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use tiamot_core::identity::Allowlist;
use tiamot_core::proto::ServerMessage;
use tiamot_core::script::{HostError, MluaVm, ModHost, ScriptVm as _, VmLimits};
use tiamot_core::session::store;
use tiamot_core::{Registry, WorldDb};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::cert::{CertError, ServerCert};
use crate::sim::{self, Control};
use crate::transport::{self, Shared, TransportError};

/// The world database file inside the world directory.
pub const WORLD_FILE: &str = "world.sqlite";

/// How often dirty chunks are written out, in ticks.
///
/// 40 ticks is two seconds. Short enough that a crash loses very little, long
/// enough that a player chiselling one block does not cause twenty writes a
/// second of the same chunk.
const SAVE_INTERVAL_TICKS: u64 = 40;

/// Anything that stops a server starting.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The world database could not be opened.
    ///
    /// Boxed: a `WorldError` carries a whole codec error chain and is far
    /// larger than the other variants, which would make every `Result` in the
    /// startup path pay for the rare case.
    #[error("world database at `{path}`")]
    World {
        /// Path we tried to use.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: Box<tiamot_core::WorldError>,
    },

    /// The server certificate could not be loaded or generated.
    #[error(transparent)]
    Cert(#[from] CertError),

    /// The identity registry could not be read.
    #[error("could not load the identity registry")]
    Identities(#[source] store::StoreError),

    /// The listener could not be started.
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// Mods could not be scanned, resolved, or loaded.
    ///
    /// A mod that fails to *load* is disabled and the server still starts
    /// (charter rule 10). This is for a set that fails to *resolve* — a missing
    /// dependency or a cycle — where there is no correct subset to fall back
    /// to.
    #[error("could not load mods")]
    Mods(#[source] Box<HostError>),

    /// A block's engine id did not match the one the VM gave its mod.
    ///
    /// Refuses to start rather than carrying on: the world would be written
    /// with ids that mean something different from what the mod placed, and
    /// nothing downstream could tell.
    #[error(
        "material `{name}` was assigned id {assigned} by the engine but {expected} by the \
         script VM — every block this mod places would be the wrong material"
    )]
    MaterialIdMismatch {
        /// The material.
        name: String,
        /// What the VM told the mod.
        expected: u16,
        /// What the engine registry assigned.
        assigned: u16,
    },

    /// A thread could not be spawned.
    #[error("could not start the {what} thread")]
    Thread {
        /// Which thread.
        what: &'static str,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },
}

/// What a server needs to start.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Address to bind. Port 0 asks the OS for a free one.
    pub bind_addr: SocketAddr,
    /// Directory holding the world database and server certificate.
    pub world_path: PathBuf,
    /// Maximum simultaneous players.
    pub max_players: u32,
    /// Who is permitted to join.
    pub allowlist: Allowlist,

    /// How far players can see, in chunks.
    pub view_distance: tiamot_core::interest::ViewDistance,

    /// Remote administration, if enabled.
    ///
    /// `None` means no admin port is open. There is no "enabled = false" flag
    /// to forget: absence is off.
    pub rcon: Option<(SocketAddr, String)>,

    /// Seed for a **new** world.
    ///
    /// Ignored if the world already has one — a world's seed is fixed at
    /// creation, because re-rolling it later would change terrain beyond the
    /// explored edge and leave a visible seam through the map.
    ///
    /// `None` draws one from system entropy.
    pub seed: Option<u64>,

    /// Directory to load mods from.
    ///
    /// `None` runs with no mods, which is a legitimate configuration — the
    /// engine is mechanisms, and a server with no content is empty rather than
    /// broken.
    pub mods_path: Option<PathBuf>,

    /// Extra material ids to register, on top of whatever mods register.
    ///
    /// Charter rule 9's lifecycle is register → FREEZE → world load, so these
    /// go in before `WorldDb::open` builds the id map. Tests use this to get a
    /// material without shipping a mod; real servers get theirs from
    /// [`mods_path`](Self::mods_path).
    pub materials: Vec<String>,
}

/// A running server.
///
/// Dropping this stops the server, but [`stop`](Self::stop) is preferred: it
/// waits for the world to be flushed and reports whether that succeeded.
pub struct ServerHandle {
    control: Control,
    simulation: Option<std::thread::JoinHandle<()>>,
    network: Option<std::thread::JoinHandle<()>>,
    /// A handle on the listener, kept solely to close it.
    ///
    /// The accept loop parks in `endpoint.accept().await`, which never returns
    /// on its own when no client is connecting — so a stop flag alone would
    /// never be re-read and joining the network thread would hang forever.
    /// Closing the endpoint makes `accept` return `None` and the loop falls
    /// out. `Endpoint` is an `Arc` inside, so this clone is a pointer.
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
    cert_fingerprint: [u8; 32],
    shared: Arc<Shared>,
    world_path: PathBuf,
}

impl ServerHandle {
    /// Starts a server and returns once it is listening.
    ///
    /// # Errors
    ///
    /// [`StartError`] if the world, certificate, identities, or listener could
    /// not be brought up.
    pub fn start(settings: &Settings) -> Result<Self, StartError> {
        // Charter rule 9's lifecycle, in order: scan → resolve → load →
        // registration window → FREEZE → world load → play. Every step below is
        // in that sequence, and the world is opened last on purpose — its id
        // map is built from the registry, so a material registered after the
        // open would have no world id, and edits using it would be accepted and
        // then fail silently at save time.
        let mut registry = Registry::new();
        let mut host = None;

        if let Some(mods_path) = &settings.mods_path {
            match ModHost::<MluaVm>::load_from(mods_path, VmLimits::default()) {
                Ok(mut loaded) => {
                    // FREEZE. After this `register_*` is a hard error.
                    if let Err(err) = loaded.freeze() {
                        return Err(StartError::Mods(Box::new(HostError::Script(err))));
                    }
                    for (mod_id, err) in loaded.failed() {
                        // A mod that fails to load is disabled, not fatal
                        // (charter rule 10) — but an operator has to hear
                        // about it, or the first sign is a player reporting
                        // that something is missing.
                        error!(mod_id = %mod_id, "mod failed to load and is disabled: {err}");
                    }

                    // Replay the VM's block registrations in ID ORDER, so the
                    // engine registry assigns exactly the numbers the VM handed
                    // its mods. Any other order and every block a mod places is
                    // a different material than it asked for.
                    for (name, expected) in loaded.vm().registered_blocks() {
                        match registry.register(&name) {
                            Ok(assigned) if assigned == expected => {}
                            Ok(assigned) => {
                                // Not recoverable by carrying on: the world
                                // would be written with ids that mean something
                                // else. Better to refuse to start.
                                return Err(StartError::MaterialIdMismatch {
                                    name,
                                    expected: expected.0,
                                    assigned: assigned.0,
                                });
                            }
                            Err(err) => {
                                error!("could not register material `{name}`: {err}");
                            }
                        }
                    }

                    info!(
                        mods = loaded.resolved().order.len(),
                        disabled = loaded.failed().len(),
                        blocks = loaded.vm().registered_blocks().len(),
                        "mods loaded and registries frozen"
                    );
                    host = Some(loaded);
                }
                Err(err) => return Err(StartError::Mods(Box::new(err))),
            }
        }

        for name in &settings.materials {
            if let Err(err) = registry.register(name) {
                error!("could not register material `{name}`: {err}");
            }
        }

        let world_file = settings.world_path.join(WORLD_FILE);
        let world =
            WorldDb::open(&world_file, &mut registry).map_err(|source| StartError::World {
                path: world_file.clone(),
                source: Box::new(source),
            })?;

        let cert = ServerCert::load_or_create(&settings.world_path)?;
        let cert_fingerprint = cert.fingerprint;
        info!(
            fingerprint = %cert.fingerprint_hex(),
            "server certificate ready — clients pin this on first connection"
        );

        let (identities, report) = store::load(&world).map_err(StartError::Identities)?;
        info!(
            identities = report.identities,
            names = report.names,
            "identity registry loaded"
        );
        for skipped in &report.skipped {
            error!("skipped a stored identity: {skipped}");
        }

        let control = Control::new();
        let shared = Arc::new(Shared {
            identities: Mutex::new(identities),
            cert_fingerprint,
            mods: Vec::new(),
            mod_set_fingerprint: 0,
            allowlist: std::sync::RwLock::new(settings.allowlist.clone()),
            max_players: settings.max_players,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: control.clone(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            // Capacity is per-receiver backlog, not a total. 1024 messages at
            // 20 Hz is roughly fifty seconds behind before a client starts
            // losing them, which is far longer than a connection worth keeping.
            outbound: tokio::sync::broadcast::channel(1024).0,
            chunk_requests: std::sync::Mutex::new(std::collections::VecDeque::new()),
            view_distance: settings.view_distance,
            kicks: tokio::sync::broadcast::channel(64).0,
            online: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        });

        // The runtime is built here rather than inside the network thread, and
        // the socket is bound inside it, so `start` returns an address that is
        // ALREADY listening. Binding on the network thread instead would let a
        // caller connect before the socket existed — a race that shows up as a
        // test failing about one run in a hundred.
        //
        // quinn also requires a tokio context to bind at all: `Endpoint::server`
        // looks for the current runtime to drive its I/O, and without one it
        // fails with "no async runtime found".
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("tiamot-net")
            .build()
            .map_err(|source| StartError::Thread {
                what: "network runtime",
                source,
            })?;

        let endpoint = {
            let _guard = runtime.enter();
            transport::bind(settings.bind_addr, cert)?
        };
        let local_addr = endpoint.local_addr().map_err(|source| StartError::Thread {
            what: "network",
            source,
        })?;

        // RCON shares the network runtime rather than getting its own. It is
        // one socket carrying a handful of commands; a second multi-threaded
        // runtime for that would cost more threads than the feature is worth.
        if let Some((rcon_addr, token)) = settings.rcon.clone() {
            let context = Arc::new(crate::rcon::RconContext {
                shared: Arc::clone(&shared),
                token,
                mods: Vec::new(),
            });
            let guard = runtime.enter();
            runtime.spawn(async move {
                if let Err(err) = crate::rcon::serve(rcon_addr, context).await {
                    error!("RCON listener stopped: {err}");
                }
            });
            drop(guard);
        }

        let network = {
            let endpoint = endpoint.clone();
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("network".to_owned())
                .spawn(move || {
                    runtime.block_on(transport::accept_loop(endpoint, shared));
                    // Dropping the runtime here, on this thread, rather than
                    // wherever the handle happens to be dropped: a tokio runtime
                    // dropped inside an async context panics, and a caller
                    // stopping the server from within their own runtime would
                    // hit exactly that.
                    drop(runtime);
                })
                .map_err(|source| StartError::Thread {
                    what: "network",
                    source,
                })?
        };

        // Drawn here rather than in the thread so a caller passing `None` still
        // gets a reproducible run if they log what was chosen.
        let new_seed = settings.seed.unwrap_or_else(|| {
            let mut bytes = [0u8; 8];
            // A failed entropy read is not worth refusing to start over: any
            // seed generates a valid world, and this one only matters for a
            // world that does not exist yet.
            let _ = getrandom::fill(&mut bytes);
            u64::from_le_bytes(bytes)
        });

        let simulation = {
            let control = control.clone();
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("simulation".to_owned())
                .spawn(move || {
                    // The seed is only used if the world has none yet — an
                    // existing world keeps the seed it was created with, or
                    // terrain beyond the explored edge would change shape.
                    let mut world = match crate::world::World::open(world, new_seed) {
                        Ok(world) => world,
                        Err(err) => {
                            error!("could not read the world seed: {err}");
                            return;
                        }
                    };
                    info!(seed = world.seed(), "world seed");

                    // Either the mods generate terrain, or there are no mods
                    // and the world is air. Both are legitimate.
                    let mut source = match host {
                        Some(host) => crate::world::Generator::Mods(Box::new(
                            crate::world::ModGenerator::new(host),
                        )),
                        None => crate::world::Generator::Air(crate::world::Air),
                    };

                    let mut clock = sim::MonotonicClock::new();
                    sim::run(&mut clock, &control, |tick| {
                        // ALL database access happens on this thread. The
                        // network side mutates the in-memory registry and this
                        // side writes it out, so a slow disk cannot stall a
                        // connection and a slow client cannot stall a tick.
                        // Sharing the connection behind a mutex instead would
                        // let either happen.
                        //
                        // `try_lock`, not `lock`: a tick must never wait on a
                        // connection that is mid-handshake. If the registry is
                        // busy the flush simply happens on the next tick, 50 ms
                        // later.
                        if let Ok(mut identities) = shared.identities.try_lock()
                            && identities.is_dirty()
                            && let Err(err) = store::flush(world.db(), &mut identities)
                        {
                            error!("could not persist identity changes: {err}");
                        }

                        // Edits, in tick order, on one thread. Applying them
                        // from the network tasks instead would make the result
                        // depend on which connection won a lock.
                        for (actor, edit) in shared.drain_edits() {
                            match world.apply(&edit, &mut source) {
                                Ok(_) => {
                                    // Broadcast only AFTER it applied. Telling
                                    // clients about an edit the server then
                                    // rejected would leave every one of them
                                    // showing a block the world does not have.
                                    shared.broadcast(ServerMessage::BlockDelta {
                                        edit,
                                        actor: Some(*actor.as_bytes()),
                                    });
                                }
                                Err(err) => {
                                    // The peer asked for something impossible.
                                    // Not fatal to the connection: a client
                                    // racing a mod unload can do this without
                                    // being hostile.
                                    debug!(actor = %actor.short(), "rejected an edit: {err}");
                                }
                            }
                        }

                        // Mod tick hooks, before edits are applied: a mod
                        // that queues an edit this tick should see it land
                        // this tick, not next.
                        //
                        // `dt_ticks` is 1 here because the loop calls `step`
                        // once per tick even when catching up — the catch-up
                        // is several calls, not one call with a bigger number.
                        for (mod_id, err) in source.tick(1) {
                            error!(mod_id = %mod_id, "mod disabled after a tick failure: {err}");
                        }

                        // Serve chunk requests. Bounded per tick by
                        // CHUNKS_PER_TICK: encoding is real work on this
                        // thread, and an unbounded drain would let one player
                        // joining stall the world for everyone.
                        for request in shared.take_chunk_requests() {
                            let blob = match world.chunk(request.pos, &mut source) {
                                Ok(chunk) => {
                                    let chunk = chunk.clone();
                                    world.db().chunk_blob(request.pos, &chunk).ok()
                                }
                                Err(err) => {
                                    debug!(pos = ?request.pos, "could not load chunk: {err}");
                                    None
                                }
                            };
                            // A failed send means the connection went away
                            // between asking and being answered, which is
                            // ordinary rather than an error.
                            let _ = request.reply.send(blob);
                        }

                        // Debounced saves. Writing every dirty chunk every tick
                        // would turn a player chiselling one block into 20
                        // writes a second of the same chunk; waiting for
                        // shutdown would lose everything on a crash.
                        if (tick % SAVE_INTERVAL_TICKS == 0 || control.take_save_request())
                            && let Err(err) = world.save_dirty()
                        {
                            error!("could not save dirty chunks: {err}");
                        }
                    });

                    // The network thread is already stopped by the time this
                    // returns, so a blocking lock here cannot contend with a
                    // live connection — and a final flush must not be skipped
                    // just because the last tick happened to find the lock
                    // taken.
                    {
                        let mut identities = shared.identities.blocking_lock();
                        if let Err(err) = store::flush(world.db(), &mut identities) {
                            error!("could not persist identity changes on shutdown: {err}");
                        }
                    }

                    // The world is owned by this thread for its whole life, so
                    // the final flush happens here where nothing else can be
                    // mid-write.
                    if let Err(err) = world.close() {
                        error!("failed to flush the world on shutdown: {err}");
                    }
                })
                .map_err(|source| StartError::Thread {
                    what: "simulation",
                    source,
                })?
        };

        info!(%local_addr, "server listening");

        Ok(Self {
            control,
            simulation: Some(simulation),
            network: Some(network),
            endpoint,
            local_addr,
            cert_fingerprint,
            shared,
            world_path: settings.world_path.clone(),
        })
    }

    /// Starts an in-process server on loopback, for singleplayer.
    ///
    /// Identical to [`start`](Self::start) except that it binds loopback on an
    /// OS-assigned port, so nothing outside the machine can reach it.
    ///
    /// # Errors
    ///
    /// [`StartError`] on any startup failure.
    pub fn start_embedded(world_path: &Path, max_players: u32) -> Result<Self, StartError> {
        Self::start(&Settings {
            bind_addr: "127.0.0.1:0".parse().expect("a valid loopback address"),
            world_path: world_path.to_path_buf(),
            max_players,
            allowlist: Allowlist::open(),
            view_distance: tiamot_core::interest::ViewDistance::DEFAULT,
            seed: None,
            mods_path: None,
            // Singleplayer has no admin port. The player already has full
            // control of the process.
            rcon: None,
            materials: Vec::new(),
        })
    }

    /// The address the server is actually listening on.
    ///
    /// Resolved, so a caller that asked for port 0 learns which port it got.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// `BLAKE3` of the server's certificate. What a client pins.
    #[must_use]
    pub const fn cert_fingerprint(&self) -> [u8; 32] {
        self.cert_fingerprint
    }

    /// The world directory.
    #[must_use]
    pub fn world_path(&self) -> &Path {
        &self.world_path
    }

    /// Shared state, for tests and for the RCON layer.
    #[must_use]
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// The simulation control handle.
    #[must_use]
    pub const fn control(&self) -> &Control {
        &self.control
    }

    /// Stops the server and waits for the world to be flushed.
    ///
    /// # Errors
    ///
    /// Nothing is returned on failure — a thread that panicked has already
    /// logged. This returns `false` if either thread panicked, so a caller can
    /// treat the save as suspect.
    pub fn stop(mut self) -> bool {
        self.shutdown()
    }

    fn shutdown(&mut self) -> bool {
        self.control.stop();
        // Wakes the accept loop. Without this the network thread is parked in
        // `accept().await` and the join below never returns.
        self.endpoint.close(0u32.into(), b"server shutting down");

        // The network first, and this order is load-bearing: the simulation
        // thread performs the final identity flush and then closes the world.
        // A connection completing a join after that flush would leave a binding
        // in memory that never reaches the database — the player's name would
        // silently be free again next time the server started.
        let network_ok = self
            .network
            .take()
            .is_none_or(|handle| handle.join().is_ok());
        let simulation_ok = self
            .simulation
            .take()
            .is_none_or(|handle| handle.join().is_ok());

        if !network_ok {
            error!("the network thread panicked");
        }
        if !simulation_ok {
            error!("the simulation thread panicked; the world may be inconsistent");
        }
        network_ok && simulation_ok
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // A handle dropped without `stop` still has to bring the threads down,
        // or a test that fails an assertion leaves a server running and the
        // next test binds a port that is already in use.
        if self.simulation.is_some() || self.network.is_some() {
            self.shutdown();
        }
    }
}
