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
use tiamot_core::session::store;
use tiamot_core::{Registry, WorldDb};
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::cert::{CertError, ServerCert};
use crate::sim::{self, Control};
use crate::transport::{self, Shared, TransportError};

/// The world database file inside the world directory.
pub const WORLD_FILE: &str = "world.sqlite";

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
        let mut registry = Registry::new();
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
            allowlist: settings.allowlist.clone(),
            max_players: settings.max_players,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: control.clone(),
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

        let simulation = {
            let control = control.clone();
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("simulation".to_owned())
                .spawn(move || {
                    let mut clock = sim::MonotonicClock::new();
                    sim::run(&mut clock, &control, |_tick| {
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
                            && let Err(err) = store::flush(&world, &mut identities)
                        {
                            error!("could not persist identity changes: {err}");
                        }

                        // World stepping hangs here: mod `on_tick` callbacks,
                        // then queued edits, then chunk streaming.
                    });

                    // The network thread is already stopped by the time this
                    // returns, so a blocking lock here cannot contend with a
                    // live connection — and a final flush must not be skipped
                    // just because the last tick happened to find the lock
                    // taken.
                    {
                        let mut identities = shared.identities.blocking_lock();
                        if let Err(err) = store::flush(&world, &mut identities) {
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
