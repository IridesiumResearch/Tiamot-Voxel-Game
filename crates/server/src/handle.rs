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
use tiamot_core::proto::{ModEntry, ServerMessage};
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

/// Builds the material table a client is sent on join.
///
/// The ids are **world** ids, because those are the ids a chunk blob carries.
/// Runtime ids are per-session (charter rule 8) and would name every material
/// one number out on a world that had ever seen a different mod set.
///
/// A texture a mod declared but did not ship is a warning, not a failure: the
/// client draws its missing-texture placeholder and the operator gets a line
/// naming the mod and the path. Refusing to start would let one mislaid PNG
/// take a server down.
fn material_table(
    registry: &Registry,
    map: &tiamot_core::persist::idmap::MaterialMap,
    content: &tiamot_core::content::ContentIndex,
    textures: &[tiamot_core::script::BlockTexture],
) -> Vec<tiamot_core::proto::MaterialDef> {
    use std::collections::BTreeMap;

    let by_block: BTreeMap<&str, &tiamot_core::script::BlockTexture> = textures
        .iter()
        .map(|texture| (texture.block.as_str(), texture))
        .collect();

    let mut table: Vec<tiamot_core::proto::MaterialDef> = registry
        .iter()
        .filter_map(|(runtime, name)| {
            // A material with no world id was registered after the world was
            // opened, which charter rule 9's freeze makes impossible. Skipping
            // rather than unwrapping keeps a lifecycle bug from being a crash.
            let id = map.to_world(runtime).ok()?;
            let texture = by_block.get(name).and_then(|texture| {
                let hash = content.hash_of(&texture.mod_id, &texture.path);
                if hash.is_none() {
                    error!(
                        mod_id = %texture.mod_id,
                        path = %texture.path,
                        block = %name,
                        "block declares a texture that is not in the mod directory; clients will \
                         draw the missing-texture placeholder"
                    );
                }
                hash
            });
            Some(tiamot_core::proto::MaterialDef {
                id,
                name: name.to_owned(),
                texture,
            })
        })
        .collect();

    table.sort_by_key(|entry| entry.id);
    table
}

/// A stable fingerprint over the resolved mod set.
///
/// Lets a client tell in one comparison whether it has seen this exact set
/// before, without walking every entry. Order matters: two servers running the
/// same mods in a different load order are genuinely different, because load
/// order decides material ids.
fn mod_set_fingerprint(mods: &[ModEntry]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:mod-set:v1");
    for entry in mods {
        hasher.update(entry.id.as_bytes());
        hasher.update(entry.version.as_bytes());
        hasher.update(&entry.content_hash);
    }
    let bytes = hasher.finalize();
    u64::from_le_bytes(
        bytes.as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    )
}

/// Sends every touched chunk's light to everyone.
///
/// Broadcast rather than aimed, the same as `BlockDelta`: interest sets live in
/// the transport and the simulation thread does not hold them. A client filters
/// what it is not holding, and the payload for the uniform chunks that make up
/// most of a world is three bytes.
fn broadcast_light(
    shared: &Shared,
    lighting: &crate::light::Lighting,
    touched: &std::collections::BTreeSet<tiamot_core::ChunkPos>,
) {
    for pos in touched {
        let Some(layer) = lighting.layer(*pos) else {
            continue;
        };
        shared.broadcast(ServerMessage::ChunkLight {
            pos: *pos,
            light: tiamot_core::light::codec::encode(layer),
        });
    }
}

/// The block an edit changed.
const fn edited_block(edit: &tiamot_core::proto::Edit) -> tiamot_core::BlockPos {
    match edit {
        tiamot_core::proto::Edit::Block { pos, .. }
        | tiamot_core::proto::Edit::Partial { pos, .. } => *pos,
        tiamot_core::proto::Edit::SubNode { pos, .. } => pos.block(),
    }
}

/// How often dirty chunks are written out, in ticks.
///
/// 40 ticks is two seconds. Short enough that a crash loses very little, long
/// enough that a player chiselling one block does not cause twenty writes a
/// second of the same chunk.
const SAVE_INTERVAL_TICKS: u64 = 40;

/// How many chunks may be relit from scratch in one tick.
///
/// The cap exists because a player teleporting or a server starting can make
/// thousands of chunks resident at once, and an unbounded pass would spend the
/// whole tick on terrain nobody is looking at yet. What it does not reach this
/// tick it reaches on the next.
///
/// **Four, from a measurement rather than an estimate.** This was 32, on the
/// strength of Task 02b's spike putting a full-chunk relight at about 30 µs.
/// The real thing costs **1.38 ms** for the case that dominates a join — a
/// chunk of air under open sky, with its neighbours resident — so the old
/// number was not a cap on anything: 32 of them is 44 ms of a 50 ms tick.
/// Four is 5.5 ms, 11% of the budget, which is a bound worth having.
///
/// Honest about what this did and did not fix: the macro benchmark's 22 ms
/// ticks were **not** this. Setting the cap to 4 and back to 32 moves its p99
/// by 12 µs, because the chunks a joining player waits on are relit by the
/// request path rather than by this catch-up pass. What fixed the benchmark was
/// not relighting a chunk that is already lit. This number matters for the case
/// the benchmark does not cover — a teleport, or a mod making unexplored
/// terrain resident — where nothing else bounds the work.
const RELIGHTS_PER_TICK: usize = 4;

/// How often the time of day goes out, in ticks.
///
/// Once a second. The clock advances every tick, but a client interpolates
/// between updates and a second of drift in a twenty-minute day is a thousandth
/// of it — invisible, against twenty times the messages.
const TIME_BROADCAST_TICKS: u64 = 20;

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
        let mut content_index = tiamot_core::content::ContentIndex::new();
        let mut mods: Vec<ModEntry> = Vec::new();

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

                    // Index every mod's client-relevant files. Done once at
                    // startup and then frozen: hashing on demand would mean a
                    // file edited while the server runs is served under its old
                    // hash, which is the one thing content addressing exists to
                    // make impossible.
                    for entry in &loaded.resolved().order {
                        match content_index.add_mod(&entry.id, &entry.dir) {
                            Ok(fingerprint) => mods.push(ModEntry {
                                id: entry.id.clone(),
                                version: entry.version.to_string(),
                                content_hash: fingerprint,
                            }),
                            Err(err) => {
                                // The mod still runs; its assets just are not
                                // pushed. Refusing to start over an oversized
                                // texture would be worse than a missing one.
                                error!(
                                    mod_id = %entry.id,
                                    "could not index mod content, assets will not be pushed: {err}"
                                );
                                mods.push(ModEntry {
                                    id: entry.id.clone(),
                                    version: entry.version.to_string(),
                                    content_hash: [0u8; 32],
                                });
                            }
                        }
                    }

                    info!(
                        mods = loaded.resolved().order.len(),
                        disabled = loaded.failed().len(),
                        blocks = loaded.vm().registered_blocks().len(),
                        content_items = content_index.len(),
                        content_bytes = content_index.total_bytes(),
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

        // Built after the world opens, because it is the WORLD's ids that go on
        // the wire: chunk blobs carry them, and a table of this session's
        // runtime ids would name every material one number out.
        let block_textures = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_block_textures())
            .unwrap_or_default();
        let materials = material_table(
            &registry,
            world.materials(),
            &content_index,
            &block_textures,
        );
        // Charter rule 1: the engine has no tools of its own, so this list is
        // whatever the mods registered and nothing else. A client is told it
        // for the same reason it is told the materials — so it can offer a
        // choice without the engine knowing what a chisel is.
        let tool_table: Vec<tiamot_core::proto::ToolDef> = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_tools())
            .unwrap_or_default()
            .into_iter()
            .map(|tool| tiamot_core::proto::ToolDef {
                name: tool.name.unwrap_or_else(|| tool.id.clone()),
                id: tool.id,
                brush: tool.brush.name().to_owned(),
                default: tool.default,
            })
            .collect();
        info!(
            materials = materials.len(),
            textured = materials.iter().filter(|m| m.texture.is_some()).count(),
            "material table built"
        );

        // Breaking rules, keyed by WORLD id because that is what a chunk holds.
        // Built here, after the world's id map exists, for the same reason the
        // material table is: a table of this session's runtime ids would name
        // every material one number out on any world that has seen a different
        // mod set (charter rule 8).
        let hardness = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_block_rules())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|rules| {
                // `Registry` has no name lookup — it is an ordered list, and
                // adding an index for one caller at startup would be a data
                // structure for a hot path that does not exist.
                let runtime = registry
                    .iter()
                    .find(|(_, name)| *name == rules.block)
                    .map(|(id, _)| id)?;
                let world_id = world.materials().to_world(runtime).ok()?;
                Some((tiamot_core::MaterialId(world_id), rules.hardness))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        // Emissive blocks, keyed by world id for the same reason hardness is.
        // A world that has seen a different mod set numbers its materials
        // differently, and a table of this session's runtime ids would name
        // every lamp one number out (charter rule 8).
        let emissions = crate::light::emissions_from_rules(
            &host
                .as_ref()
                .map(|loaded| loaded.vm().registered_block_rules())
                .unwrap_or_default(),
            |block| {
                let runtime = registry
                    .iter()
                    .find(|(_, name)| *name == block)
                    .map(|(id, _)| id)?;
                world
                    .materials()
                    .to_world(runtime)
                    .ok()
                    .map(tiamot_core::MaterialId)
            },
        );

        // The fluids the mods registered, keyed by world material id for the
        // same reason emissions are.
        let fluids = crate::fluid::fluids_from_rules(
            &host
                .as_ref()
                .map(|loaded| loaded.vm().registered_fluids())
                .unwrap_or_default(),
            |block| {
                let runtime = registry
                    .iter()
                    .find(|(_, name)| *name == block)
                    .map(|(id, _)| id)?;
                world
                    .materials()
                    .to_world(runtime)
                    .ok()
                    .map(tiamot_core::MaterialId)
            },
        );

        // The same registry, as the wire carries it. Built here rather than in
        // the tick because the join tables are assembled once, before the
        // simulation thread starts, and a client needs this before its first
        // chunk rather than after its first pond.
        let fluid_table: Vec<tiamot_core::proto::FluidDef> = fluids
            .iter()
            .map(|(id, registered)| {
                let mut depths = [0u8; 8];
                for (level, depth) in depths.iter_mut().enumerate() {
                    *depth =
                        tiamot_core::fluid::Fluid::flowing(id, level as u8).depth_units() as u8;
                }
                tiamot_core::proto::FluidDef {
                    id: id.0,
                    name: registered.name.clone(),
                    material: registered.material.get(),
                    depths,
                }
            })
            .collect();

        // The sky, as the wire carries it. Absent is legitimate: a world whose
        // mods register no sky has no day, and the client holds its colours
        // fixed rather than being given one the engine invented.
        let sky = host
            .as_ref()
            .and_then(|loaded| loaded.vm().registered_sky())
            .map_or((0, Vec::new(), 0.0), |sky| {
                let frames = sky
                    .keyframes
                    .iter()
                    .map(|frame| tiamot_core::proto::SkyFrame {
                        time: frame.time,
                        sky: frame.sky,
                        sun: frame.sun,
                        intensity: frame.intensity,
                        grade: tiamot_core::proto::SkyGrade {
                            exposure: frame.grade.exposure,
                            tint: frame.grade.tint,
                            offset: frame.grade.offset,
                            contrast: frame.grade.contrast,
                            saturation: frame.grade.saturation,
                            gamma: frame.grade.gamma,
                        },
                    })
                    .collect();
                (sky.day_length_ticks, frames, sky.start_time)
            });
        if sky.0 > 0 {
            info!(
                day_length_ticks = sky.0,
                keyframes = sky.1.len(),
                "a mod registered a sky"
            );
        }

        let tools = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_tools())
            .unwrap_or_default()
            .into_iter()
            .map(|tool| (tool.id.clone(), tool))
            .collect::<std::collections::BTreeMap<_, _>>();
        // Lowest id among those marked default, so the answer does not depend
        // on which mod loaded first.
        let default_tool = tools
            .values()
            .find(|tool| tool.default)
            .map(|tool| tool.id.clone());
        info!(
            hardness = hardness.len(),
            tools = tools.len(),
            "breaking rules built"
        );

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
            mod_set_fingerprint: mod_set_fingerprint(&mods),
            mods,
            materials,
            tool_table,
            fluid_table,
            sky_day_length: sky.0,
            sky_keyframes: sky.1,
            // Where the mod said its day starts, in ticks. A counter left at
            // zero opens every new world at midnight, which is the one hour
            // with no sun, no shadows and nothing to tell two graphics settings
            // apart.
            time_of_day: std::sync::atomic::AtomicU64::new(
                (f64::from(sky.2) * f64::from(sky.0)) as u64,
            ),
            content: content_index,
            allowlist: std::sync::RwLock::new(settings.allowlist.clone()),
            max_players: settings.max_players,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: control.clone(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            placements: std::sync::Mutex::new(std::collections::VecDeque::new()),
            seeds: std::sync::Mutex::new(std::collections::VecDeque::new()),
            notices: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            // Capacity is per-receiver backlog, not a total. 1024 messages at
            // 20 Hz is roughly fifty seconds behind before a client starts
            // losing them, which is far longer than a connection worth keeping.
            outbound: tokio::sync::broadcast::channel(1024).0,
            inventories: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            inventory_dirty: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            chunk_requests: std::sync::Mutex::new(std::collections::VecDeque::new()),
            view_distance: settings.view_distance,
            kicks: tokio::sync::broadcast::channel(64).0,
            online: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            bodies: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            hardness,
            tools,
            default_tool,
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

                    // One per server, opened on the simulation thread because
                    // that is the only thread that writes to it.
                    let trace = crate::trace::Trace::from_environment();

                    // Light is derived and lives only in memory — see
                    // `crate::light`. It is built here rather than in `Shared`
                    // because only the simulation thread may touch it: every
                    // read walks the chunk cache, and a second thread doing
                    // that would need a lock around the world itself.
                    //
                    // Behind a lock ONLY so `game.get_light` can read it: a mod
                    // callback runs on this thread, inside a tick, and cannot
                    // borrow what the tick is holding. The lock is never
                    // contended — both sides are this thread — and is never
                    // held across a callback, which is what would deadlock.
                    let lighting = std::sync::Arc::new(std::sync::RwLock::new(
                        crate::light::Lighting::new(emissions),
                    ));

                    // Behind a lock for the same reason lighting is, and not
                    // for a different one: `game.set_fluid` runs on this thread
                    // inside a tick and cannot borrow what the tick is holding.
                    // Never contended — both sides are this thread — and never
                    // held across a callback, which is what would deadlock.
                    let fluidics = std::sync::Arc::new(std::sync::RwLock::new(
                        crate::fluid::Fluidics::new(fluids),
                    ));

                    // Either the mods generate terrain, or there are no mods
                    // and the world is air. Both are legitimate.
                    let mut source = match host {
                        Some(mut host) => {
                            // Point `game.get_light` at the world now that
                            // there is one. Charter rule 1: a mod deciding
                            // where something may spawn needs to be able to ask
                            // how dark it is there.
                            host.vm_mut().set_light_source(std::sync::Arc::new(
                                crate::light::Shared::new(std::sync::Arc::clone(&lighting)),
                            ));
                            // And the fluid, so a mod can pour as well as read.
                            // This is the only writable handle in the frozen
                            // API, which is why `Access` takes `&self` and the
                            // store sits behind the lock above.
                            host.vm_mut().set_fluid_access(std::sync::Arc::new(
                                crate::fluid::Shared::new(std::sync::Arc::clone(&fluidics)),
                            ));
                            crate::world::Generator::Mods(Box::new(
                                crate::world::ModGenerator::new(host),
                            ))
                        }
                        None => crate::world::Generator::Air(crate::world::Air),
                    };

                    let mut clock = sim::MonotonicClock::new();
                    sim::run(&mut clock, &control, |tick| {
                        // Every block edited this tick, relit once at the end
                        // rather than four times in the middle. Batching is
                        // what makes the cost of a swarm of players placing
                        // lamps a function of the blocks they changed rather
                        // than of where in the tick they changed them, and it
                        // is the natural place to put a per-tick cap if the
                        // load test ever needs one.
                        let mut relight: Vec<tiamot_core::BlockPos> = Vec::new();
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

                        // Operator edits first, and with no inventory effect
                        // at all: nobody is credited for what they remove and
                        // nobody is charged for what they add, because nobody
                        // did it. A test arranging a world before a player acts
                        // is the whole use, and it goes first so the player's
                        // own actions this tick see the arranged world.
                        for edit in shared.drain_seeds() {
                            match world.apply(&edit, &mut source) {
                                Ok(_) => {
                                    relight.push(edited_block(&edit));
                                    // What the block will ACCEPT has changed,
                                    // even though its fluid has not: a wall
                                    // knocked out is how a pond finds out there
                                    // is somewhere new to go.
                                    fluidics
                                        .write()
                                        .expect("fluid lock")
                                        .touch(edited_block(&edit));
                                    shared.broadcast(ServerMessage::BlockDelta {
                                        edit,
                                        actor: None,
                                    });
                                }
                                Err(err) => {
                                    debug!("an operator edit would not apply: {err}");
                                }
                            }
                        }

                        // Edits, in tick order, on one thread. Applying them
                        // from the network tasks instead would make the result
                        // depend on which connection won a lock.
                        for (actor, edit) in shared.drain_edits() {
                            match world.apply(&edit, &mut source) {
                                Ok((_, removed)) => {
                                    relight.push(edited_block(&edit));
                                    // A pond finds out there is somewhere new
                                    // to go the same way it finds out a wall
                                    // came down: every edit wakes it, whichever
                                    // path the edit arrived by.
                                    fluidics
                                        .write()
                                        .expect("fluid lock")
                                        .touch(edited_block(&edit));
                                    // Charter rule 5: what the edit took out,
                                    // in units. 27 for a block, 1 for a
                                    // sub-node.
                                    shared.credit(actor, removed);
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

                        // Players move here, on the tick thread, in a fixed
                        // order over a `BTreeMap`. Not in the connection tasks:
                        // charter rule 2 allows one simulation, and stepping a
                        // body from whichever task woke first would make the
                        // result depend on thread scheduling — which is also
                        // the one thing charter rule 4's determinism cannot
                        // survive.
                        //
                        // Collision reads only RESIDENT chunks (`World::resident`),
                        // so walking into unloaded terrain stops a player rather
                        // than generating a chunk inside the tick budget.
                        if let Ok(mut bodies) = shared.bodies.lock() {
                            for player in bodies.values_mut() {
                                let intent = player.inputs.take(tick);
                                let voxels = tiamot_core::phys::Voxels::new(&world, player.origin);
                                let before = player.body;
                                player.body = tiamot_core::phys::step(
                                    &voxels,
                                    player.body,
                                    intent,
                                    &tiamot_core::phys::Tuning::DEFAULT,
                                );
                                // **The server's half of the picture.** A client
                                // log established that the two simulations part
                                // company by cells while the PLAYER IS STANDING
                                // STILL and the server is loading chunks hard —
                                // so the body that moves is this one, and only
                                // this side can say what it was standing on when
                                // it did.
                                if let Some(trace) = trace.as_ref() {
                                    trace.tick(&crate::trace::Moment {
                                        tick,
                                        origin: player.origin,
                                        before: &before,
                                        after: &player.body,
                                        intent,
                                        touched_absent: voxels.touched_absent(),
                                        chunks_cached: world.cached(),
                                    });
                                }
                                // Charter rule 7: keep the local part inside
                                // one chunk so it never becomes a world-space
                                // f32 that loses precision as the player walks.
                                let (origin, local) = tiamot_core::phys::voxels::renormalise(
                                    player.origin,
                                    player.body.position,
                                );
                                if origin.in_world() {
                                    player.origin = origin;
                                    player.body.position = local;
                                } else {
                                    // The world is finite (charter rule 6), so a
                                    // body must not leave it. Without this a
                                    // player over a hole falls for ever, and
                                    // their interest set follows them down —
                                    // the server generating and encoding a
                                    // fresh layer of chunks every tick, without
                                    // end, for one player who is not going
                                    // anywhere they can come back from.
                                    player.body.velocity = [0.0; 3];
                                }
                            }
                        }

                        // Digging, after movement so a dig is judged against
                        // where the player actually ended up this tick.
                        //
                        // The whole loop is on this thread for the same reason
                        // movement is: it reads the world, it writes the world,
                        // and doing it from a connection task would make the
                        // result depend on which one woke first.
                        for (uuid, target, brush) in shared.digs_in_progress() {
                            let material = world
                                .subnode(target, &mut source)
                                .unwrap_or(tiamot_core::MaterialId::AIR);
                            if material.is_air() {
                                // Whatever they aimed at is already gone —
                                // someone else broke it, or they are digging
                                // air. Not an error, just nothing to do.
                                shared.set_dig(&uuid, None);
                                continue;
                            }

                            // Reach, checked every tick rather than only when
                            // the dig starts: a player who walks away from a
                            // dig in progress should stop digging, and one who
                            // never came close should never have started.
                            //
                            // Without this a client could mine anything it
                            // could name, anywhere in the world, which is a
                            // rather larger hole than the one it is standing
                            // in. The client's own raycast is bounded by
                            // `phys::REACH`, but a bound only the client
                            // enforces is not a bound.
                            if let Some((origin, eye)) = shared.player_eye(&uuid)
                                && !tiamot_core::place::within_reach(origin, eye, target)
                            {
                                shared.set_dig(&uuid, None);
                                shared.tell(&uuid, "that is too far away".to_owned());
                                continue;
                            }

                            let hardness = shared.hardness_of(material);
                            let Some(done) = shared.advance_dig(&uuid, hardness) else {
                                continue;
                            };
                            if !done {
                                continue;
                            }

                            // The mods get a veto, BEFORE anything is removed.
                            // Refusing afterwards would mean putting the block
                            // back, and that is not the same as never having
                            // taken it: the drops are computed and the removal
                            // is broadcast on the way through.
                            let verdict = source.may_dig(&tiamot_core::script::DigEvent {
                                player: *uuid.as_bytes(),
                                target,
                                material,
                                brush,
                            });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_dig_complete failure: {err}");
                            }
                            if !verdict.allowed {
                                // The dig is abandoned, not paused: leaving it
                                // running would re-ask the mods every tick
                                // forever, and the player would watch a crack
                                // that never finishes.
                                shared.set_dig(&uuid, None);
                                continue;
                            }

                            // Contract §2 and §9: the brush decides what comes
                            // out, and `break_block` decides what it yields.
                            let edit = match brush {
                                tiamot_core::dig::Brush::SubNode => tiamot_core::proto::Edit::SubNode {
                                    pos: target,
                                    material: tiamot_core::MaterialId::AIR.0,
                                },
                                tiamot_core::dig::Brush::Block => tiamot_core::proto::Edit::Block {
                                    pos: target.block(),
                                    material: tiamot_core::MaterialId::AIR.0,
                                },
                            };
                            match world.apply(&edit, &mut source) {
                                Ok((_, removed)) => {
                                    relight.push(edited_block(&edit));
                                    // A pond finds out there is somewhere new
                                    // to go the same way it finds out a wall
                                    // came down: every edit wakes it, whichever
                                    // path the edit arrived by.
                                    fluidics
                                        .write()
                                        .expect("fluid lock")
                                        .touch(edited_block(&edit));
                                    shared.credit(uuid, removed);
                                    shared.broadcast(ServerMessage::BlockDelta {
                                        edit,
                                        actor: Some(*uuid.as_bytes()),
                                    });
                                }
                                Err(err) => {
                                    debug!(actor = %uuid.short(), "a completed dig would not apply: {err}");
                                }
                            }
                            shared.set_dig(&uuid, None);
                        }

                        // Placement, after digging and after movement. The
                        // order matters and is not arbitrary: a player who dug
                        // a block this tick can place into the hole this tick,
                        // and a placement is judged against where every body
                        // actually ended up rather than where it started.
                        //
                        // Every refusal is told to the player who asked. A
                        // build that silently did nothing is the worst of the
                        // options — charter rule 2 means the client cannot work
                        // out the reason for itself, so it has to be sent one.
                        for request in shared.drain_placements() {
                            let material = tiamot_core::MaterialId(request.material);
                            let held = tiamot_core::inventory::units_of(
                                &shared.inventory_of(&request.actor),
                                material,
                            );

                            // The same bound as digging. A placement is a
                            // request and this is one more reason to refuse it.
                            let in_reach = shared
                                .player_eye(&request.actor)
                                .is_none_or(|(origin, eye)| {
                                    tiamot_core::place::within_reach(origin, eye, request.target)
                                });
                            if !in_reach {
                                shared.tell(&request.actor, "that is too far away".to_owned());
                                continue;
                            }

                            // The tool decides what a placement writes, exactly
                            // as it decides what a dig removes: a sub-node
                            // brush fills the cell that was aimed at, a block
                            // brush fills the block bottom-up.
                            let brush = shared.place_brush(&request.actor);

                            let outcome = tiamot_core::place::plan(request.target, held, brush)
                                .and_then(|plan| {
                                    // Air only, judged cell by cell. Placing
                                    // into occupied space would have to decide
                                    // what happens to what was already there,
                                    // and "it is destroyed" is a conservation
                                    // hole (charter rule 5). Per cell rather
                                    // than per block is what lets a chisel fill
                                    // the gaps in a block it carved.
                                    let occupied = tiamot_core::place::occupied_cells(&plan)
                                        .any(|cell| {
                                            i32::try_from(cell[0]).is_ok_and(|x| {
                                                let (Ok(y), Ok(z)) = (
                                                    i32::try_from(cell[1]),
                                                    i32::try_from(cell[2]),
                                                ) else {
                                                    return false;
                                                };
                                                world
                                                    .subnode(
                                                        tiamot_core::SubNodePos::new(x, y, z),
                                                        &mut source,
                                                    )
                                                    .is_ok_and(|found| !found.is_air())
                                            })
                                        });
                                    if occupied {
                                        return Err(tiamot_core::place::Refusal::Occupied);
                                    }
                                    if tiamot_core::place::blocks_a_body(
                                        &plan,
                                        &shared.body_boxes(),
                                    ) {
                                        return Err(tiamot_core::place::Refusal::InsideAPlayer);
                                    }
                                    Ok(plan)
                                });

                            let plan = match outcome {
                                Ok(plan) => plan,
                                Err(refusal) => {
                                    shared.tell(&request.actor, refusal.to_string());
                                    continue;
                                }
                            };

                            // The mods' veto, after the engine's own rules and
                            // before the player is charged — a refusal must not
                            // cost them anything.
                            // **In the id space a mod can compare against.**
                            // `material` here is a WORLD id — stable across
                            // sessions, which is what the database needs — and
                            // `game.get_block_id` hands out RUNTIME ids, which
                            // is what registration produces. Charter rule 8 says
                            // those are different numbers, and handing a mod the
                            // wrong one is a comparison that works whenever the
                            // two happen to coincide and fails when they do not.
                            //
                            // Reported from the window as milk that "sometimes
                            // just doesn't pour, it places like a block".
                            let as_registered =
                                world.runtime_material(material.0).unwrap_or(material);
                            let verdict = source.may_place(&tiamot_core::script::PlaceEvent {
                                player: *request.actor.as_bytes(),
                                block: plan.block,
                                material: as_registered,
                                occupancy: plan.occupancy,
                                units: plan.units,
                            });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_place failure: {err}");
                            }
                            if !verdict.allowed {
                                shared.tell(
                                    &request.actor,
                                    "you cannot build there".to_owned(),
                                );
                                continue;
                            }

                            // Charged BEFORE the write, and only what was
                            // actually taken is placed. Writing first and
                            // charging after would hand a player free material
                            // on any path where the debit came up short —
                            // another connection of theirs spending it between
                            // the check and here is enough.
                            let paid = shared.debit(&request.actor, material, plan.units);
                            if paid == 0 {
                                shared.tell(
                                    &request.actor,
                                    tiamot_core::place::Refusal::NothingHeld.to_string(),
                                );
                                continue;
                            }

                            // A full block goes out as `Edit::Block`, not as a
                            // `Partial` with every bit set. The two produce
                            // identical geometry, so sending the second form
                            // would mean the same result arrived as two
                            // different messages depending on how it was made —
                            // and everything watching for a block appearing
                            // would have to know about both.
                            //
                            // A sub-node placement goes out as `Edit::SubNode`
                            // for a stronger reason than symmetry with digging:
                            // `Edit::Partial` REPLACES a block, so sending one
                            // cell that way would delete whatever else was in
                            // the block — the conservation hole the occupancy
                            // check above exists to prevent, reintroduced by
                            // the message that reports the placement.
                            let edit = match brush {
                                tiamot_core::dig::Brush::SubNode => {
                                    tiamot_core::proto::Edit::SubNode {
                                        pos: request.target,
                                        material: request.material,
                                    }
                                }
                                tiamot_core::dig::Brush::Block => {
                                    let occupancy =
                                        tiamot_core::inventory::placement_mask(paid);
                                    if paid >= tiamot_core::UNITS_PER_BLOCK {
                                        tiamot_core::proto::Edit::Block {
                                            pos: plan.block,
                                            material: request.material,
                                        }
                                    } else {
                                        tiamot_core::proto::Edit::Partial {
                                            pos: plan.block,
                                            material: request.material,
                                            occupancy,
                                        }
                                    }
                                }
                            };
                            match world.apply(&edit, &mut source) {
                                Ok(_) => {
                                    relight.push(edited_block(&edit));
                                    // A pond finds out there is somewhere new
                                    // to go the same way it finds out a wall
                                    // came down: every edit wakes it, whichever
                                    // path the edit arrived by.
                                    fluidics
                                        .write()
                                        .expect("fluid lock")
                                        .touch(edited_block(&edit));
                                    shared.broadcast(ServerMessage::BlockDelta {
                                        edit,
                                        actor: Some(*request.actor.as_bytes()),
                                    });
                                }
                                Err(err) => {
                                    // The write failed after the charge, so
                                    // give it back. Anything else destroys
                                    // material on a path a player did not cause
                                    // and cannot see.
                                    shared.credit(
                                        request.actor,
                                        tiamot_core::inventory::Stack::new(material, paid)
                                            .into_iter()
                                            .collect(),
                                    );
                                    debug!(
                                        actor = %request.actor.short(),
                                        "a placement would not apply: {err}"
                                    );
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
                            // Light for a chunk that is about to be sent. A
                            // client that had blocks and no light would draw
                            // the world black until something happened to
                            // relight it, so this is not an optimisation to
                            // defer.
                            //
                            // **Only if it is not already lit.** A chunk's
                            // light is kept current by every edit, so relighting
                            // one that has a layer produces the answer it
                            // already had — at 1.38 ms a chunk (measured, see
                            // `light::Lit`). It is not a rare case either: the
                            // players who join together are the ones who ask for
                            // the same chunks, so the second and third of them
                            // were each paying full price for a chunk the first
                            // had already lit. The requester still needs the
                            // light itself, which is what the send below is.
                            if blob.is_some() {
                                let mut light = lighting.write().expect("lighting lock");
                                let touched = if light.holds(request.pos) {
                                    std::iter::once(request.pos).collect()
                                } else {
                                    control.note_full_relight();
                                    light.chunk_loaded(&world, request.pos)
                                };
                                broadcast_light(&shared, &light, &touched);
                            }
                            // Fluid travels with the chunk, and always — an
                            // empty layer is ONE byte, so telling a client
                            // there is no milk here costs less than making it
                            // wonder. Without this a joining player sees a pond
                            // only once something disturbs it.
                            if blob.is_some() {
                                let layer = fluidics
                                    .read()
                                    .expect("fluid lock")
                                    .layer(request.pos)
                                    .cloned()
                                    .unwrap_or_else(tiamot_core::fluid::FluidLayer::empty);
                                shared.broadcast(ServerMessage::ChunkFluid {
                                    pos: request.pos,
                                    fluid: tiamot_core::fluid::codec::encode(&layer),
                                });
                            }

                            // A failed send means the connection went away
                            // between asking and being answered, which is
                            // ordinary rather than an error.
                            let _ = request.reply.send(blob);
                        }

                        // Light, once, after every edit this tick has landed.
                        // Order matters: relighting between edits would do the
                        // work twice for two edits in the same room, and the
                        // second answer is the only one anybody sees.
                        if !relight.is_empty() {
                            let mut light = lighting.write().expect("lighting lock");
                            let mut touched = std::collections::BTreeSet::new();
                            for pos in relight.drain(..) {
                                touched.extend(light.edited(&world, pos));
                            }
                            broadcast_light(&shared, &light, &touched);
                        }

                        // Chunks that arrived this tick, whatever brought
                        // them in — a chunk request, a player walking, a mod
                        // reading. Asking the world what arrived rather than
                        // hunting every load site means the next route somebody
                        // adds is lit too, instead of being silently black.
                        let arrived = world.take_arrived();
                        if !arrived.is_empty() {
                            let mut light = lighting.write().expect("lighting lock");
                            let mut touched = std::collections::BTreeSet::new();
                            let mut done = 0;
                            for pos in arrived {
                                if light.holds(pos) {
                                    continue;
                                }
                                if done >= RELIGHTS_PER_TICK {
                                    // Put the rest back, in order, for the next
                                    // tick. Dropping them would leave those
                                    // chunks black for as long as they stayed
                                    // loaded, which is the kind of bug that
                                    // only shows up after a teleport.
                                    world.defer_arrival(pos);
                                    continue;
                                }
                                control.note_full_relight();
                                touched.extend(light.chunk_loaded(&world, pos));
                                done += 1;
                            }
                            broadcast_light(&shared, &light, &touched);
                        }
                        control.note_lit_chunks(lighting.read().expect("lighting lock").len());

                        // **Fluid, at half the simulation's rate.** Nobody can
                        // see the difference between milk moving ten times a
                        // second and twenty, and it halves the cost of the one
                        // system whose work is proportional to how much of the
                        // world is MOVING rather than to how much is loaded.
                        //
                        // A settled world returns here immediately: the solver
                        // checks an empty active set and allocates nothing.
                        if tick.is_multiple_of(crate::fluid::TICKS_PER_FLUID_TICK) {
                            let mut fluid = fluidics.write().expect("fluid lock");
                            let changes = fluid.tick(&world, tick / crate::fluid::TICKS_PER_FLUID_TICK);
                            if !changes.is_empty() {
                                // The whole layer, per touched chunk, rather
                                // than a delta per block — see
                                // `ServerMessage::ChunkFluid` for why that is
                                // both smaller and safer.
                                for pos in crate::fluid::Fluidics::touched_chunks(&changes) {
                                    let Some(layer) = fluid.layer(pos) else {
                                        // The chunk drained completely, so it
                                        // has no layer any more. Clients still
                                        // have to be told, or the milk they can
                                        // see never goes away.
                                        shared.broadcast(ServerMessage::ChunkFluid {
                                            pos,
                                            fluid: tiamot_core::fluid::codec::encode(
                                                &tiamot_core::fluid::FluidLayer::empty(),
                                            ),
                                        });
                                        continue;
                                    };
                                    shared.broadcast(ServerMessage::ChunkFluid {
                                        pos,
                                        fluid: tiamot_core::fluid::codec::encode(layer),
                                    });
                                }
                            }
                        }

                        // The day advances once per tick, and is broadcast at
                        // a rate a person can read rather than at the rate it
                        // changes. Twenty updates a second of a float nobody
                        // can distinguish frame to frame would be bandwidth
                        // spent on nothing; a client interpolates between
                        // these, and a second of drift in a twenty-minute day
                        // is a thousandth of it.
                        let day = shared.advance_day();
                        if tick % TIME_BROADCAST_TICKS == 0 {
                            shared.broadcast(ServerMessage::TimeOfDay { time: day });
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

    /// Writes a block directly, as the operator rather than as a player.
    ///
    /// **This is not a client capability and must never become one.** Charter
    /// rule 2 puts every world decision on the server, and the whole point of
    /// the dig and place paths is that a player pays for what they take and is
    /// refused what they may not have. This bypasses all of it, which is why it
    /// hangs off the handle — something you can only call if you are already
    /// running the server in your own process.
    ///
    /// What it is for: arranging a world before a test acts on it. That used to
    /// be done by having a bot send `ClientMessage::BlockDelta`, which meant
    /// every one of those tests quietly depended on clients being able to edit
    /// the world — the exact thing that should not be true.
    ///
    /// Applied on the next tick and broadcast like any other edit, so a
    /// connected client sees it arrive. Returns whether it was queued.
    pub fn seed_block(&self, pos: tiamot_core::BlockPos, material: u16) -> bool {
        self.shared
            .queue_seed(tiamot_core::proto::Edit::Block { pos, material })
    }

    /// The same, for one sub-node cell.
    pub fn seed_subnode(&self, pos: tiamot_core::SubNodePos, material: u16) -> bool {
        self.shared
            .queue_seed(tiamot_core::proto::Edit::SubNode { pos, material })
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
