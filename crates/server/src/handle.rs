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

use tiamot_core::identity::{Allowlist, PlayerUuid};
use tiamot_core::phys::Occupant;
use tiamot_core::proto::{ModEntry, ServerMessage};
use tiamot_core::script::{HostError, MluaVm, ModHost, ScriptVm as _, VmLimits};
use tiamot_core::session::store;
use tiamot_core::{Registry, WorldDb};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

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
    block_rules: &[tiamot_core::script::BlockRules],
    items: &[String],
) -> Vec<tiamot_core::proto::MaterialDef> {
    use std::collections::BTreeMap;

    // Everything a player can carry is in one table; an ITEM is one that may
    // not be put in the world. See `register_item` for why it is one flag and
    // not a second id space.
    let items: std::collections::BTreeSet<&str> = items.iter().map(String::as_str).collect();

    let rules: BTreeMap<&str, &tiamot_core::script::BlockRules> = block_rules
        .iter()
        .map(|rules| (rules.block.as_str(), rules))
        .collect();

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
                // What walking on it sounds like, if the mod said. The client
                // plays its own footsteps from its own movement, so this is the
                // only way it can know.
                step_sound: rules.get(name).and_then(|rules| rules.step_sound.clone()),
                name: name.to_owned(),
                texture,
                placeable: !items.contains(name),
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
/// Turns one viewer's update into the messages that carry it.
///
/// Split out because it is the whole of the wire format's opinion about
/// entities, and because a nametag has to be resolved here — the current
/// display name bound to a UUID is a fact only the server has (charter rule
/// 13), and a client holding the UUID instead would show a stale name until it
/// reconnected.
/// A label as text, or `None` for a player the server does not have a name for.
///
/// `None` rather than a placeholder: a client that is sent no label draws none,
/// and inventing `"<unnamed>"` would put engine copy on somebody's head.
/// One stack, as the wire carries it.
///
/// Its own function because three messages send stacks now — what an entity IS,
/// what it is HOLDING, and what is in a view — and three copies of the same
/// four lines is where one of them quietly stops matching.
fn stack_def(stack: &tiamot_core::inventory::Stack) -> tiamot_core::proto::StackDef {
    tiamot_core::proto::StackDef {
        material: stack.material.0,
        units: stack.units,
        shape: stack
            .shape
            .map_or(0, tiamot_core::inventory::Shape::occupancy),
        detail: stack.detail.clone(),
    }
}

fn resolve_nametag(label: &tiamot_core::ent::Nametag, shared: &Shared) -> Option<String> {
    match label {
        tiamot_core::ent::Nametag::Text(text) => Some(text.clone()),
        // **The identity registry, not the list of who is connected.** Charter
        // rule 13 makes a display name a per-server claim bound to a UUID, and
        // the binding outlives the session — so a label naming somebody who
        // logged out an hour ago still has a name to render. Reading the online
        // roster instead made every such label blank, which looks like a bug in
        // whatever put the label there.
        //
        // `try_lock`: a tick must never wait on a registry that is mid-join.
        // The online roster is the fallback for exactly that moment, and it has
        // the answer for anyone connected, which is when it matters most.
        tiamot_core::ent::Nametag::Player(uuid) => shared
            .identities
            .try_lock()
            .ok()
            .and_then(|identities| identities.name_of(uuid).map(str::to_owned))
            .or_else(|| {
                shared
                    .online
                    .lock()
                    .ok()
                    .and_then(|online| online.get(uuid).cloned())
            }),
    }
}

fn entity_messages(
    update: tiamot_core::ent::Update,
    tick: u64,
    shared: &Shared,
) -> Vec<ServerMessage> {
    let mut messages = Vec::new();

    if !update.spawned.is_empty() {
        let entities = update
            .spawned
            .into_iter()
            .map(|spawn| tiamot_core::proto::EntityDef {
                id: spawn.id.0,
                chunk: spawn.transform.chunk,
                local: spawn.transform.local,
                velocity: spawn.velocity.0,
                yaw: tiamot_core::ent::replicate::quantise_yaw(spawn.transform.yaw),
                pitch: tiamot_core::ent::replicate::quantise_pitch(spawn.transform.pitch),
                anim: spawn.anim.0,
                model: spawn.model,
                collider: spawn.collider.map(|box_| [box_.width, box_.height]),
                item: spawn.item.as_ref().map(stack_def),
                hands: [
                    spawn.hands.main.as_ref().map(stack_def),
                    spawn.hands.off.as_ref().map(stack_def),
                ],
                // Resolved here, where the roster is. Charter rule 13: a
                // display name is a per-server claim bound to a UUID, so the
                // engine stores the UUID and looks the name up at send time —
                // which is what makes a player renaming themselves change the
                // label over their own head and over anything a mod tagged
                // with them.
                nametag: spawn
                    .nametag
                    .and_then(|label| resolve_nametag(&label, shared)),
            })
            .collect();
        messages.push(ServerMessage::EntitySpawn { entities });
    }

    if !update.despawned.is_empty() {
        messages.push(ServerMessage::EntityDespawn {
            entities: update.despawned.into_iter().map(|id| id.0).collect(),
        });
    }

    if !update.moved.is_empty() {
        let entities = update
            .moved
            .into_iter()
            .map(|delta| tiamot_core::proto::EntityDelta {
                id: delta.id.0,
                chunk: delta.chunk,
                local: delta.local,
                velocity: delta.velocity,
                yaw: delta.yaw,
                pitch: delta.pitch,
                anim: delta.anim.0,
            })
            .collect();
        messages.push(ServerMessage::EntityState { tick, entities });
    }

    // **After the spawns and on the reliable channel.** A viewer told what
    // somebody is holding before it has been told they exist has nothing to
    // apply it to; the tracker only reports a change for an entity it has
    // already sent a spawn for, and this keeps that order on the wire.
    if !update.rearmed.is_empty() {
        let entities = update
            .rearmed
            .into_iter()
            .map(|armed| tiamot_core::proto::EntityHands {
                id: armed.id.0,
                hands: [
                    armed.hands.main.as_ref().map(stack_def),
                    armed.hands.off.as_ref().map(stack_def),
                ],
            })
            .collect();
        messages.push(ServerMessage::EntityArmed { entities });
    }

    let _ = shared;
    messages
}

/// Writes back every mod whose storage changed since the last save.
///
/// Its own function because the tick has two save sites — the debounced one and
/// the shutdown flush — and a mod's facts have to survive both. A failure is
/// logged rather than fatal: losing a mod's bookkeeping is bad, and taking the
/// server down over it is worse.
fn flush_mod_storage(
    world: &crate::world::World,
    storage: &std::sync::RwLock<crate::storage::ModStorage>,
) {
    let Ok(mut held) = storage.write() else {
        return;
    };
    for (mod_id, bag) in held.take_dirty() {
        if let Err(err) = world.save_mod_storage(&mod_id, &bag) {
            error!("could not save storage for mod `{mod_id}`: {err}");
        }
    }
}

/// Writes the containers whose contents changed, and forgets the broken ones.
///
/// Logged rather than fatal, like a mod's storage: a chest that could not be
/// saved is bad and a server that stopped over it is worse. The names are
/// drained, so a failure here means the change is written on the next pass
/// rather than lost — the store keeps it either way, because the version in
/// memory is the one players are looking at.
fn flush_containers(
    world: &crate::world::World,
    containers: &std::sync::Mutex<crate::containers::Containers>,
) {
    let Ok(mut store) = containers.lock() else {
        return;
    };
    let (dirty, gone) = store.take_pending();
    for name in dirty {
        // A container somebody has open is IN their slots, so there is nothing
        // here to write — and what they do with it is written when they close
        // it. Skipped rather than saved empty, which is what reading a lent
        // container would produce.
        if let Some(view) = store.contents(&name)
            && let Err(err) = world.save_container(&name, view)
        {
            error!("could not save container `{name}`: {err}");
        }
    }
    for name in gone {
        if let Err(err) = world.delete_container(&name) {
            error!("could not forget container `{name}`: {err}");
        }
    }
}

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

/// How many resident chunks get random ticks in one tick of the simulation.
///
/// **A budget, not a rule.** Charter rule 18 gives all of simulation 50 ms for
/// fifty players, and sampling is cheap but not free — three block lookups per
/// chunk, plus a Lua call for each hit. A world with more resident chunks than
/// this gets through them over several ticks rather than all at once, which
/// changes how often a given block comes up and stops nothing.
const MAX_RANDOM_TICK_CHUNKS: usize = 512;

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
    /// Who may use admin powers, as hex UUIDs.
    ///
    /// **Flight is the first of them.** A permission and not a mode: every
    /// client can ask, and the server honours it for these players and nobody
    /// else (charter rule 2). A singleplayer client puts its own identity in
    /// here when it starts its embedded world, which is what makes "fly for
    /// testing" work without a command; a hosted server names them in
    /// `server.toml`.
    ///
    /// Empty is the safe default and the right one for a public server.
    pub operators: Vec<String>,

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

    /// Which mods to load, by id. `None` loads every mod in
    /// [`mods_path`](Self::mods_path).
    ///
    /// **The server owner's decision, and that includes a player running one
    /// on their own machine.** The launcher's mod tick-boxes arrive here. A
    /// selection that leaves out something an enabled mod depends on fails to
    /// resolve and the world does not start, which is right: half a mod set is
    /// not a smaller mod set.
    pub enabled_mods: Option<Vec<String>>,

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
            match ModHost::<MluaVm>::load_selected(
                mods_path,
                VmLimits::default(),
                settings.enabled_mods.as_deref(),
            ) {
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
                    // What each mod asked to publish beyond the extension
                    // allowlist. `.lua` is not distributable by extension —
                    // server mod code has no business on a client — so a HUD
                    // script reaches one only because the mod named it.
                    let declared = loaded.vm().registered_hud_scripts();
                    for entry in &loaded.resolved().order {
                        let extra: Vec<String> = declared
                            .iter()
                            .filter(|script| script.mod_id == entry.id)
                            .map(|script| script.file.clone())
                            .collect();
                        match content_index.add_mod_with(&entry.id, &entry.dir, &extra) {
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
        let mut world =
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
        // The mods' own rules, read here as well as below: the material table
        // carries the step sound, which is the only way a client can know what
        // walking on something sounds like.
        let block_rules = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_block_rules())
            .unwrap_or_default();
        let items = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_items())
            .unwrap_or_default();
        let materials = material_table(
            &registry,
            world.materials(),
            &content_index,
            &block_textures,
            &block_rules,
            &items,
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
        // Charter rule 11: a mod registers a NAME and the engine owns the key,
        // so this list is names — and the client, which owns keys, is the thing
        // that needs it.
        let action_table: Vec<tiamot_core::proto::ActionDef> = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_actions())
            .unwrap_or_default()
            .into_iter()
            .map(|action| tiamot_core::proto::ActionDef {
                id: action.id,
                description: action.description,
                mod_id: action.mod_id,
                default_key: action.default_key,
            })
            .collect();
        info!(actions = action_table.len(), "action table built");

        // Charter rule 1 once more: the engine has no sounds, so this is
        // whatever the mods registered. The file travels by hash through the
        // same content pipeline a texture does.
        let sound_table: Vec<tiamot_core::proto::SoundDef> = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_sounds())
            .unwrap_or_default()
            .into_iter()
            .map(|sound| {
                let file = content_index.hash_of(&sound.mod_id, &sound.file);
                if file.is_none() {
                    error!(
                        mod_id = %sound.mod_id,
                        path = %sound.file,
                        sound = %sound.id,
                        "sound declares a file that is not in the mod directory; clients will \
                         play nothing"
                    );
                }
                tiamot_core::proto::SoundDef {
                    id: sound.id,
                    mod_id: sound.mod_id,
                    file,
                    gain: sound.gain,
                    pitch_variance: sound.pitch_variance,
                }
            })
            .collect();
        info!(sounds = sound_table.len(), "sound table built");

        // Which sound each named event plays. Charter rule 1 again: the engine
        // emits cues and has no opinion about what any of them sounds like.
        let sound_bindings: Vec<tiamot_core::proto::SoundBinding> = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_bindings())
            .unwrap_or_default()
            .into_iter()
            .map(|binding| tiamot_core::proto::SoundBinding {
                cue: binding.cue,
                sound: binding.sound,
                mod_id: binding.mod_id,
            })
            .collect();
        info!(bindings = sound_bindings.len(), "cue bindings built");

        // Charter rule 10's tier 2: a mod may push a script that DRAWS. The
        // file travels by hash like a sound's, and what makes it safe is the
        // sandbox on the other end — the server does not run these and never
        // reads them.
        let hud_scripts: Vec<tiamot_core::proto::HudScriptDef> = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_hud_scripts())
            .unwrap_or_default()
            .into_iter()
            .map(|script| {
                let file = content_index.hash_of(&script.mod_id, &script.file);
                if file.is_none() {
                    error!(
                        mod_id = %script.mod_id,
                        path = %script.file,
                        "HUD script is not in the mod directory; clients will draw nothing for it"
                    );
                }
                tiamot_core::proto::HudScriptDef {
                    mod_id: script.mod_id,
                    file,
                }
            })
            .collect();
        info!(hud_scripts = hud_scripts.len(), "HUD script table built");

        info!(
            materials = materials.len(),
            textured = materials.iter().filter(|m| m.texture.is_some()).count(),
            "material table built"
        );

        // The same table, the way `game.set_block` needs it: a mod names a
        // block and the engine resolves the number. Built from `materials`
        // rather than from the registry so that both sides of the API agree by
        // construction — a mod placing what a client is told about cannot be
        // one number out (charter rule 8).
        let block_names: std::collections::BTreeMap<String, u16> = materials
            .iter()
            .map(|material| (material.name.clone(), material.id))
            .collect();

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
                Some((tiamot_core::MaterialId(world_id), rules.resistance()))
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
        let mut fluids = crate::fluid::fluids_from_rules(
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

        // Which materials drink, keyed by world id for the same reason
        // emissions are — and with the SUCCESSOR resolved through the same
        // table, so `becomes = "damp_dirt"` names the same block on a world
        // that has seen a different mod set.
        let absorbency = crate::fluid::absorbency_from_rules(&block_rules, |block| {
            let runtime = registry
                .iter()
                .find(|(_, name)| *name == block)
                .map(|(id, _)| id)?;
            world
                .materials()
                .to_world(runtime)
                .ok()
                .map(tiamot_core::MaterialId)
        });

        // **Stable ids for the fluids, and adoption of any the world already
        // knew.** Charter rule 8: a fluid byte on disk carries a number, and
        // `Fluids::register` numbers positionally in registration order — so
        // without this a mod loading ahead of another silently turns every
        // stored pond into a different fluid.
        //
        // Here rather than inside `WorldDb::open` because fluids are registered
        // during mod load, which has only just finished. `fluids` is mutated:
        // anything the world knows and no mod supplied this session is added as
        // an inert placeholder so its bytes round-trip.
        world
            .reconcile_fluids(&mut fluids)
            .map_err(|source| StartError::World {
                path: world_file.clone(),
                source: Box::new(source),
            })?;

        // The same registry, as the wire carries it. Built here rather than in
        // the tick because the join tables are assembled once, before the
        // simulation thread starts, and a client needs this before its first
        // chunk rather than after its first pond.
        //
        // `iter_registered`, so a placeholder for an absent mod's fluid is not
        // offered to clients as something to draw. There is nothing to draw: it
        // exists to hold an id so stored bytes survive.
        let fluid_table: Vec<tiamot_core::proto::FluidDef> = fluids
            .iter_registered()
            .map(|(id, registered)| {
                // **No depth table any more.** It existed so a client and a
                // server could not disagree about where a surface sits, back
                // when a level had to be converted into twenty-sevenths to find
                // out. A block's volume IS that number now (Sub-Node Contract
                // §4.1), so sending a lookup table beside it would be shipping
                // a second source of truth for a value both ends already hold.
                tiamot_core::proto::FluidDef {
                    color: registered.color,
                    id: id.0,
                    name: registered.name.clone(),
                    material: registered.material.get(),
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
        // The extra places a mod wants stacks to be able to sit. Read here for
        // the same reason the tools are: the registries are frozen (charter
        // rule 9), so this is settled once and cannot change under a player.
        let views = host
            .as_ref()
            .map(|loaded| loaded.vm().registered_views())
            .unwrap_or_default();
        info!(views = views.len(), "inventory views registered");
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
            views,
            action_table,
            sound_table,
            sound_bindings,
            hud_scripts,
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
            // **Parsed once, here, and anything unreadable is refused loudly.**
            // A typo in an operator list is a player who silently does not get
            // the powers somebody meant to give them, which is the kind of
            // thing nobody notices until it matters.
            operators: settings
                .operators
                .iter()
                .filter_map(|hex| match tiamot_core::PlayerUuid::from_hex(hex) {
                    Ok(uuid) => Some(uuid),
                    Err(_) => {
                        error!(operator = %hex, "operator is not a UUID and was ignored");
                        None
                    }
                })
                .collect(),
            max_players: settings.max_players,
            spawn: tiamot_core::BlockPos::new(0, 1, 0),
            players: AtomicU32::new(0),
            control: control.clone(),
            edits: std::sync::Mutex::new(std::collections::VecDeque::new()),
            punches: std::sync::Mutex::new(std::collections::VecDeque::new()),
            actions: std::sync::Mutex::new(std::collections::VecDeque::new()),
            dialog_events: std::sync::Mutex::new(std::collections::VecDeque::new()),
            chat: std::sync::Mutex::new(std::collections::VecDeque::new()),
            placements: std::sync::Mutex::new(std::collections::VecDeque::new()),
            seeds: std::sync::Mutex::new(std::collections::VecDeque::new()),
            notices: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            entity_messages: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            hud_values: std::sync::Mutex::new(std::collections::BTreeMap::new()),
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
            bodies: std::sync::Arc::default(),
            hardness,
            held_slot: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            // The runtime ids of everything a mod registered as an item. Looked
            // up here, where the registry is, so the tick loop compares numbers.
            items: items
                .iter()
                .filter_map(|name| tiamot_core::material::MaterialRegistry::id_of(&registry, name))
                .collect(),
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
                    let world = match crate::world::World::open(world, new_seed) {
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
                        {
                            let mut fluidics = crate::fluid::Fluidics::new(fluids);
                            fluidics.set_absorbency(absorbency);
                            fluidics
                        },
                    ));
                    // And the entities, behind the same kind of lock for the
                    // same reason: `game.spawn_entity` runs on this thread
                    // inside a tick.
                    let population = std::sync::Arc::new(std::sync::RwLock::new(
                        crate::ent::Population::new(),
                    ));
                    let mod_storage = std::sync::Arc::new(std::sync::RwLock::new(
                        crate::storage::ModStorage::new(),
                    ));
                    // Chests, furnaces, hoppers: inventories that belong to the
                    // world rather than to a player. A `Mutex` and not an
                    // `RwLock` because opening one MOVES a view between here and
                    // a player's own slots, and there is no read half of that.
                    let containers = std::sync::Arc::new(std::sync::Mutex::new(
                        crate::containers::Containers::new(),
                    ));
                    // Materials some mod asked for a random tick on. Empty on a
                    // server whose mods want none, which is the ordinary case
                    // and costs nothing.
                    let mut random_tick_materials: std::collections::BTreeSet<
                        tiamot_core::MaterialId,
                    > = std::collections::BTreeSet::new();

                    // The world itself, lent to the mods for the part of each
                    // tick that runs their callbacks — everything they may learn
                    // about terrain comes through here. Not a lock like the four
                    // above, and `crates/server/src/lease.rs` says why: the tick
                    // holds the world mutably through generation and every edit,
                    // so the only safe handle is one that is empty except while
                    // it is deliberately lent.
                    let sight = crate::lease::Lease::new();

                    // Who owns each open dialog, for routing its events back.
                    // `None` on a server with no mods, which cannot open one.
                    let mut dialog_screens: Option<std::sync::Arc<Screens>> = None;
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
                            // And the entities, which is the second writable
                            // handle in the frozen API and the reason
                            // `ent::Access` also takes `&self`.
                            host.vm_mut().set_entity_access(std::sync::Arc::new(
                                crate::ent::Shared::new(
                                    std::sync::Arc::clone(&population),
                                    std::sync::Arc::clone(&shared.bodies),
                                ),
                            ));
                            // And a mod's own numbers on a mod's own HUD. The
                            // engine has no health bar and should not (charter
                            // rule 1); this is the channel it carries and does
                            // not read.
                            host.vm_mut().set_hud_access(std::sync::Arc::new(
                                crate::hud::Shared::new(std::sync::Arc::clone(&shared)),
                            ));
                            // A mod's own facts, read from the world before it
                            // can ask for them. Loaded per loaded mod rather
                            // than by scanning the table, so a mod that is no
                            // longer installed keeps its rows untouched instead
                            // of being resurrected into memory and written back.
                            // **Before any mod runs**, so a mod that opens a
                            // chest in its first tick finds what was in it
                            // rather than an empty one it then overwrites.
                            //
                            // Sizes are not known here: the mods have not said
                            // which container is which, and a container whose
                            // mod is gone must keep the rows it has rather than
                            // be trimmed on behalf of nobody. `sized` therefore
                            // answers `None` and each comes back the size it
                            // was stored at.
                            match world.load_containers(&|_| None) {
                                Ok((restored, dropped)) => {
                                    if dropped > 0 {
                                        warn!(
                                            dropped,
                                            "some stacks in containers hold a material this \
                                             world can no longer name"
                                        );
                                    }
                                    if let Ok(mut store) = containers.lock() {
                                        for (name, view) in restored {
                                            store.restore(name, view);
                                        }
                                    }
                                }
                                Err(err) => {
                                    error!("could not read the world's containers: {err}");
                                }
                            }
                            // **Which materials any mod wants a turn at.**
                            // Asked once, after the freeze: the alternative is
                            // a call into the VM for every cell the engine
                            // samples, thousands a tick, almost all of them
                            // stone.
                            random_tick_materials.extend(host.vm().random_tick_materials());
                            host.vm_mut().set_container_access(std::sync::Arc::new(
                                crate::containers::Shared::new(
                                    std::sync::Arc::clone(&containers),
                                    std::sync::Arc::clone(&shared),
                                ),
                            ));
                            if let Ok(mut storage) = mod_storage.write() {
                                for mod_id in host.resolved().ids() {
                                    match world.load_mod_storage(mod_id) {
                                        Ok(bag) => storage.load(mod_id, bag),
                                        Err(err) => error!(
                                            "could not load storage for mod `{mod_id}`: {err}"
                                        ),
                                    }
                                }
                            }
                            host.vm_mut().set_storage_access(std::sync::Arc::new(
                                crate::storage::Shared::new(std::sync::Arc::clone(&mod_storage)),
                            ));
                            // And `game.set_block`, which until now was
                            // installed by the VM's own tests and by nothing
                            // else — so a mod that placed a block did nothing at
                            // all on a running server, silently, because an
                            // uninstalled edit queue is exactly what worldgen
                            // sees and is not an error there.
                            host.vm_mut().set_world_edit(std::sync::Arc::new(
                                crate::fluid::Edits::new(
                                    std::sync::Arc::clone(&shared),
                                    block_names,
                                ),
                            ));
                            // And the world, which is how a mod finds out
                            // whether it can see something. There is nothing to
                            // clone here: the handle is empty until a tick lends
                            // the world into it.
                            host.vm_mut()
                                .set_sight_access(std::sync::Arc::new(sight.handle()));
                            // And where it can walk, through the same lease and
                            // the same window.
                            host.vm_mut()
                                .set_path_access(std::sync::Arc::new(sight.handle()));
                            // And what each player is holding, which is how a
                            // mod builds a control that changes how digging
                            // behaves — `core_tools:chisel_mode` is exactly
                            // that. Installed HERE and not only in the VM's
                            // tests: `game.set_block` was dead on every real
                            // server for three tasks because it was wired up in
                            // tests alone, and this is the same shape.
                            host.vm_mut()
                                .set_tools_access(std::sync::Arc::new(HeldTools {
                                    shared: std::sync::Arc::clone(&shared),
                                }));
                            // And where a mod's crafting puts what it made.
                            host.vm_mut()
                                .set_inventory_access(std::sync::Arc::new(Carried {
                                    shared: std::sync::Arc::clone(&shared),
                                }));
                            // And who is close enough to hear a mod's sounds.
                            host.vm_mut()
                                .set_sound_access(std::sync::Arc::new(Earshot {
                                    shared: std::sync::Arc::clone(&shared),
                                }));
                            // And whose screen a mod's dialogs open on. Kept
                            // by this loop as well as by the VM, because
                            // routing an event back needs the owner and the
                            // VM's copy is behind the script boundary.
                            let screens = std::sync::Arc::new(Screens {
                                shared: std::sync::Arc::clone(&shared),
                                owners: std::sync::Mutex::new(
                                    std::collections::BTreeMap::new(),
                                ),
                            });
                            host.vm_mut()
                                .set_dialog_access(std::sync::Arc::clone(&screens)
                                    as std::sync::Arc<dyn tiamot_core::ui::host::Access>);
                            dialog_screens = Some(std::sync::Arc::clone(&screens));
                            crate::world::Generator::Mods(Box::new(
                                crate::world::ModGenerator::new(host),
                            ))
                        }
                        None => crate::world::Generator::Air(crate::world::Air),
                    };

                    // One per connected player, kept across ticks: a tracker
                    // IS the record of what that client has been told.
                    let mut trackers: std::collections::BTreeMap<
                        tiamot_core::PlayerUuid,
                        tiamot_core::ent::Tracker,
                    > = std::collections::BTreeMap::new();

                    // The world lives in an `Option` for the tick's duration so
                    // that the tick body can MOVE it into the sight lease and
                    // take it back. Moving rather than borrowing is what makes
                    // the lending window compiler-enforced: between the two the
                    // tick has no world, so there is nothing for a mod's read to
                    // race with. The cost is one `Option` move per tick.
                    // Who the tick has already told the mods about. Arrivals
                    // are a diff against this rather than an event from the
                    // network, so a join the tick never saw cannot be missed.
                    let mut known_players: std::collections::BTreeSet<tiamot_core::PlayerUuid> =
                        std::collections::BTreeSet::new();
                    // **What each of them was called**, kept because the leave
                    // hook needs it and the online roster does not have it any
                    // more: by the time the tick notices somebody has gone,
                    // they are out of it. A name is a per-server claim bound to
                    // a UUID (charter rule 13), so this is a convenience for
                    // saying something about them and never their identity.
                    let mut known_names: std::collections::BTreeMap<
                        tiamot_core::PlayerUuid,
                        String,
                    > = std::collections::BTreeMap::new();

                    let mut held = Some(world);

                    let mut clock = sim::MonotonicClock::new();
                    sim::run(&mut clock, &control, |tick| {
                        let mut world = held
                            .take()
                            .expect("the world is put back at the end of every tick");
                        // Refill what the mods may spend on pathfinding this
                        // tick. One pool for every mod and every mob, because a
                        // per-call ceiling bounds one search and says nothing
                        // about two hundred of them.
                        sight.open_tick();
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
                            match world.apply(tiamot_core::domain::OVERWORLD, &edit, &mut source) {
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
                            match world.apply(tiamot_core::domain::OVERWORLD, &edit, &mut source) {
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
                        //
                        // The fluid is read alongside the geometry so a player
                        // floats in milk the server can see. **The guard is
                        // scoped to this block deliberately**: the digging loop
                        // below runs mod callbacks, and `game.set_fluid` takes
                        // the same lock for writing — a read guard held across
                        // one would deadlock the tick thread against itself.
                        // Nothing inside this loop enters a mod.
                        // **Bodies make room for each other, before they
                        // move.** Reported from the window as players wanting
                        // to collide "very subtly" with mobs and each other.
                        //
                        // A velocity nudge rather than a constraint, added
                        // here so the step that follows resolves it against
                        // terrain — see `phys::crowd`. Computed from where
                        // everything was at the end of last tick, which is one
                        // tick of lag on a lean that takes a second.
                        //
                        // The two locks are taken one after the other and
                        // neither is held across the other, which is what the
                        // punch loop below does for the same pair.
                        {
                            let mobs: Vec<(tiamot_core::ent::EntityId, Occupant)> = population
                                .read()
                                .expect("entity lock")
                                .crowd()
                                .collect();
                            let mut occupants: Vec<Occupant> = Vec::new();
                            let mut people: Vec<PlayerUuid> = Vec::new();
                            if let Ok(bodies) = shared.bodies.lock() {
                                for (uuid, player) in bodies.iter() {
                                    people.push(*uuid);
                                    occupants.push(Occupant {
                                        origin: player.origin,
                                        position: player.body.position,
                                        height: tiamot_core::phys::PLAYER_HEIGHT,
                                        width: tiamot_core::phys::PLAYER_WIDTH,
                                        movable: true,
                                    });
                                }
                            }
                            occupants.extend(mobs.iter().map(|(_, occupant)| *occupant));
                            let pushes = tiamot_core::phys::separate(&occupants);
                            let (for_people, for_mobs) = pushes.split_at(people.len());

                            if let Ok(mut bodies) = shared.bodies.lock() {
                                for (uuid, push) in people.iter().zip(for_people) {
                                    if let Some(player) = bodies.get_mut(uuid) {
                                        player.body.velocity[0] += push[0];
                                        player.body.velocity[2] += push[2];
                                    }
                                }
                            }
                            if for_mobs.iter().any(|push| push[0] != 0.0 || push[2] != 0.0) {
                                let mut mobs_out = population.write().expect("entity lock");
                                for ((id, _), push) in mobs.iter().zip(for_mobs) {
                                    mobs_out.nudge(*id, *push);
                                }
                            }
                        }

                        if let Ok(mut bodies) = shared.bodies.lock() {
                            let fluid = fluidics.read().expect("fluid lock");
                            for player in bodies.values_mut() {
                                let intent = player.inputs.take(tick);
                                // Bound to the domain this body is in before
                                // the physics sees it: `ChunkLookup` takes a
                                // position and no domain, so a body would
                                // otherwise collide against whichever domain
                                // the lookup happened to be built over.
                                let terrain = world.solid(tiamot_core::domain::OVERWORLD);
                                let voxels = tiamot_core::phys::Voxels::with_fluid(
                                    &terrain,
                                    &*fluid,
                                    player.origin,
                                );
                                let before = player.body;
                                player.body = tiamot_core::phys::step(
                                    &voxels,
                                    player.body,
                                    intent,
                                    &tiamot_core::phys::Tuning::DEFAULT,
                                );
                                // What a client draws this body doing, decided
                                // where both halves are known: what was asked
                                // for, and what came of it.
                                player.anim = crate::transport::anim_from_motion(
                                    intent,
                                    &player.body,
                                    player.dig.is_some()
                                        || tick.saturating_sub(player.swung_on)
                                            < crate::transport::SWING_TICKS,
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

                        // **Every player is also an entity** (charter rule 2).
                        // The body that moves is the `PlayerSim` stepped above;
                        // this mirrors it into the entity store so that
                        // everything that asks "what is near me" — a mod, the
                        // replication tracker, another client's renderer — gets
                        // one answer with one shape, rather than each growing
                        // its own idea of where the people are.
                        //
                        // The mirrors are transient: never saved, never stepped,
                        // never dirtying a chunk. `ent::Population::transient`
                        // says what each of those would otherwise cost.
                        let mut joined: Vec<tiamot_core::script::JoinEvent> = Vec::new();
                        {
                            let mut mobs = population.write().expect("entity lock");
                            let mut present = std::collections::BTreeSet::new();
                            if let Ok(bodies) = shared.bodies.lock() {
                                for (uuid, player) in bodies.iter() {
                                    present.insert(*uuid);
                                    let turn = std::f32::consts::TAU;
                                    mobs.sync_player(
                                        *uuid,
                                        tiamot_core::ent::Transform {
                                            chunk: player.origin,
                                            local: player.body.position,
                                            // Turns on the wire so transmitting
                                            // a heading needs no trigonometry;
                                            // radians here because that is what
                                            // a transform holds. One multiply.
                                            //
                                            // **Converted, because a camera and
                                            // a body count yaw in opposite
                                            // directions.** This stored the
                                            // camera's angle unconverted, so
                                            // every OTHER player saw a body
                                            // facing the mirror image of where
                                            // its owner was looking — the same
                                            // mistake `figure_yaw` was written
                                            // for after third person showed it
                                            // on the local player's own figure.
                                            // Invisible at yaw zero, where the
                                            // two agree.
                                            yaw: tiamot_core::ent::figure_yaw(
                                                player.look[0] * turn,
                                            ),
                                            pitch: player.look[1] * turn,
                                        },
                                        tiamot_core::ent::Velocity(player.body.velocity),
                                        player.body.on_ground,
                                        player.anim,
                                        // Read fresh, like the position above:
                                        // a hand is a view of a live inventory
                                        // and is never persisted with the body.
                                        shared.hands_of(uuid),
                                    );
                                }
                            }
                            // Whoever is not in the roster has gone. Driven by
                            // who IS here rather than by a disconnect event: a
                            // disconnection the tick never saw would otherwise
                            // leave a body standing in the world for ever.
                            mobs.retain_players(&present);

                            // Arrivals are the same diff read the other way.
                            // Derived rather than delivered as an event from
                            // the connection task, because a hook must run on
                            // this thread inside the tick — a mod called from a
                            // connection could spawn an entity while the tick
                            // was iterating them.
                            for uuid in &present {
                                if known_players.insert(*uuid) {
                                    // **What they were carrying when they last
                                    // left.** Loaded here rather than at the
                                    // handshake because this is the thread the
                                    // world database is on; the client has
                                    // already been sent the empty inventory a
                                    // moment ago, and `restore_inventory`
                                    // marks it dirty so the real one follows.
                                    match world.load_player_slots(
                                        uuid,
                                        &shared.fresh_inventory(),
                                    ) {
                                        Ok(Some((slots, dropped))) => {
                                            if dropped > 0 {
                                                warn!(
                                                    player = %uuid.to_hex(),
                                                    dropped,
                                                    "some stored stacks could not be restored"
                                                );
                                            }
                                            shared.restore_inventory(*uuid, slots);
                                        }
                                        Ok(None) => {}
                                        Err(err) => {
                                            error!(
                                                player = %uuid.to_hex(),
                                                "could not read a saved inventory: {err}"
                                            );
                                        }
                                    }
                                    let name = shared
                                        .online
                                        .lock()
                                        .ok()
                                        .and_then(|online| online.get(uuid).cloned())
                                        .unwrap_or_default();
                                    known_names.insert(*uuid, name.clone());
                                    joined.push(tiamot_core::script::JoinEvent {
                                        player: *uuid.as_bytes(),
                                        name,
                                    });
                                }
                            }
                            // A player who left takes their open dialogs with
                            // them. Without this the owner map keeps a row per
                            // form per player for the life of the server, and a
                            // rejoining player could receive an event routed by
                            // an ownership nobody holds any more.
                            if let Some(screens) = dialog_screens.as_ref() {
                                for uuid in &known_players {
                                    if !present.contains(uuid) {
                                        screens.forget_player(&uuid.to_hex());
                                    }
                                }
                            }
                            // **And what they were carrying goes to disk.**
                            // Driven by who IS here rather than by a disconnect
                            // event, exactly as the body above is: a
                            // disconnection the tick never saw would otherwise
                            // lose an inventory rather than a mirror.
                            for uuid in &known_players {
                                if !present.contains(uuid) {
                                    // **Anything they had open comes back
                                    // first.** A container lent to somebody
                                    // who has gone is one nobody can open
                                    // again, and its contents would be written
                                    // into that player's row as theirs —
                                    // duplicating them for the player and
                                    // losing them from the world.
                                    if let Ok(mut store) = containers.lock() {
                                        let _ = shared.with_slots(uuid, |slots| {
                                            store.close_all(*uuid, slots)
                                        });
                                    }
                                    // **Then the mods, and then the save.** A
                                    // mod dropping what somebody was carrying
                                    // has to be able to read and empty their
                                    // inventory, so the hook runs before the
                                    // row is written and after the containers
                                    // are back — or it would find a chest's
                                    // contents among their own.
                                    let outcome =
                                        source.player_left(&tiamot_core::script::LeaveEvent {
                                            player: *uuid.as_bytes(),
                                            name: known_names
                                                .get(uuid)
                                                .cloned()
                                                .unwrap_or_default(),
                                        });
                                    for (mod_id, err) in &outcome.faults {
                                        error!(
                                            mod_id = %mod_id,
                                            "mod disabled after an on_player_leave failure: {err}"
                                        );
                                    }
                                    if let Some(slots) = shared.slots_of(uuid)
                                        && let Err(err) = world.save_player_slots(uuid, &slots)
                                    {
                                        error!(
                                            player = %uuid.to_hex(),
                                            "could not save an inventory: {err}"
                                        );
                                    }
                                    shared.forget_inventory(uuid);
                                }
                            }
                            known_players.retain(|uuid| present.contains(uuid));
                            // And their names, or the map keeps a row per
                            // person who has ever joined for the life of the
                            // server.
                            known_names.retain(|uuid, _| present.contains(uuid));
                        }

                        // Digging, after movement so a dig is judged against
                        // where the player actually ended up this tick.
                        //
                        // The whole loop is on this thread for the same reason
                        // movement is: it reads the world, it writes the world,
                        // and doing it from a connection task would make the
                        // result depend on which one woke first.
                        for (uuid, target, brush) in shared.digs_in_progress() {
                            // **What is being dug, which is not always the cell
                            // that was aimed at.**
                            //
                            // A block brush takes a block apart one sub-node at
                            // a time in a scattered order, so the cell under
                            // the crosshair is very often one of the first to
                            // go. Reading the material from that cell alone
                            // meant the dig stopped the moment its own aim
                            // point crumbled — measured at ten sub-nodes of
                            // twenty-seven before this distinction existed.
                            //
                            // So a block brush asks the BLOCK what it is made
                            // of and stops when the block is empty; a chisel
                            // asks its one cell, as it always did.
                            let material = match brush {
                                tiamot_core::dig::Brush::SubNode => world
                                    .subnode(tiamot_core::domain::OVERWORLD, target, &mut source)
                                    .unwrap_or(tiamot_core::MaterialId::AIR),
                                tiamot_core::dig::Brush::Block => world
                                    .block_cells(tiamot_core::domain::OVERWORLD, target.block(), &mut source)
                                    .ok()
                                    .and_then(|cells| {
                                        cells.into_iter().find(|material| !material.is_air())
                                    })
                                    .unwrap_or(tiamot_core::MaterialId::AIR),
                            };
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

                            // **What the brush removes is what has to be paid
                            // for.** A sub-node costs a thirteen-and-a-half-th
                            // of its own material; a whole block costs a blend
                            // over everything in it, so an ore seam runs at a
                            // speed between the ore and the rock around it
                            // rather than at whichever of the two the crosshair
                            // happened to land on. See
                            // `tiamot_core::dig::hardness`.
                            let hardness = match brush {
                                tiamot_core::dig::Brush::SubNode => {
                                    shared.subnode_hardness_of(material)
                                }
                                tiamot_core::dig::Brush::Block => {
                                    let cells = world
                                        .block_cells(tiamot_core::domain::OVERWORLD, target.block(), &mut source)
                                        .unwrap_or(tiamot_core::block::EMPTY_CELLS);
                                    shared.block_hardness_of(&tiamot_core::block::BlockView::Mixed(
                                        &cells,
                                    ))
                                }
                            };
                            // **How many sub-nodes are left to take.** A block
                            // brush comes apart one cell at a time; a chisel
                            // takes its one cell at the end, as it always did.
                            let cells = match brush {
                                tiamot_core::dig::Brush::SubNode => 1,
                                tiamot_core::dig::Brush::Block => world
                                    .block_cells(tiamot_core::domain::OVERWORLD, target.block(), &mut source)
                                    .map(|cells| {
                                        cells
                                            .iter()
                                            .filter(|material| {
                                                **material != tiamot_core::MaterialId::AIR
                                            })
                                            .count()
                                    })
                                    .unwrap_or(0)
                                    as u32,
                            };
                            if cells == 0 {
                                // Nothing there any more — somebody else took
                                // it, or a mod did. Not an error and not worth
                                // telling anybody about.
                                shared.set_dig(&uuid, None);
                                continue;
                            }
                            let Some(bite) = shared.advance_dig(&uuid, hardness, cells) else {
                                continue;
                            };
                            let chips = bite.chips;
                            if chips == 0 {
                                continue;
                            }

                            // **The mods are asked ONCE per block, on the first
                            // bite.** `on_dig_complete` is one event about one
                            // block: a mod playing a break sound from it made
                            // twenty-seven noises for one block when this was
                            // asked per bite, which is how it was reported.
                            //
                            // The first bite rather than the last, so the veto
                            // still means something — a refusal arrives before
                            // anything has been removed, which is what "BEFORE
                            // anything is removed" has always promised.
                            let verdict = if bite.first {
                                source.may_dig(&tiamot_core::script::DigEvent {
                                    player: *uuid.as_bytes(),
                                    target,
                                    material,
                                    brush,
                                })
                            } else {
                                tiamot_core::script::HookOutcome::allow()
                            };
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_dig_complete failure: {err}");
                            }
                            if !verdict.allowed {
                                // The dig is abandoned, not paused: leaving it
                                // running would re-ask the mods every tick
                                // forever, and the player would watch a block
                                // that never comes apart.
                                shared.set_dig(&uuid, None);
                                // Only when the mod supplied one. A silent
                                // refusal was this path's behaviour before mods
                                // could say anything, and inventing wording for
                                // it now would put game copy in the engine.
                                if let Some(notice) = verdict.reason.as_deref().filter(|reason| {
                                    !reason.is_empty()
                                }) {
                                    shared.tell(&uuid, notice.to_owned());
                                }
                                continue;
                            }

                            // Contract §2 and §9: the brush decides what comes
                            // out, and `break_block` decides what it yields.
                            let edits = match brush {
                                tiamot_core::dig::Brush::SubNode => {
                                    vec![tiamot_core::proto::Edit::SubNode {
                                        pos: target,
                                        material: tiamot_core::MaterialId::AIR.0,
                                    }]
                                }
                                // **The block comes apart in a fixed random
                                // order**, seeded by its own position so every
                                // client sees the same shape at the same moment
                                // and a rejoining player sees what is already
                                // there (charter rule 4's rule for randomness).
                                tiamot_core::dig::Brush::Block => {
                                    // Which side the digger is on, so the block
                                    // comes apart from the face they can see.
                                    // Inferred from the eye rather than sent by
                                    // the client: a normal on the wire would be
                                    // one more thing a client could lie about,
                                    // and this is already known here.
                                    let toward = shared
                                        .player_eye(&uuid)
                                        .map_or([0.0, 1.0, 0.0], |(origin, eye)| {
                                            let at = tiamot_core::ent::Transform::at(origin, eye)
                                                .to_world();
                                            let block = target.block();
                                            [
                                                at[0] - (f64::from(block.x) + 0.5),
                                                at[1] - (f64::from(block.y) + 0.5),
                                                at[2] - (f64::from(block.z) + 0.5),
                                            ]
                                        });
                                    crumble_bites(
                                        &mut world,
                                        &mut source,
                                        target.block(),
                                        chips,
                                        toward,
                                    )
                                }
                            };
                            for edit in edits {
                                match world.apply(tiamot_core::domain::OVERWORLD, &edit, &mut source) {
                                    Ok((_, removed)) => {
                                        relight.push(edited_block(&edit));
                                        // A pond finds out there is somewhere
                                        // new to go the same way it finds out a
                                        // wall came down: every edit wakes it,
                                        // whichever path the edit arrived by.
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
                                        debug!(actor = %uuid.short(), "a dig bite would not apply: {err}");
                                    }
                                }
                            }
                            if !bite.done {
                                continue;
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
                            // **What they hold of THIS stack**, not of this
                            // material. A player with one named block and
                            // sixty plain ones has sixty-one of neither, and
                            // counting the total would let them place a block
                            // they do not have — the shape defect the cut
                            // fixed, in the detail's clothes.
                            let held = tiamot_core::inventory::units_of_exactly(
                                &shared.inventory_of(&request.actor),
                                material,
                                tiamot_core::inventory::Shape::new(request.shape),
                                request.detail.as_deref(),
                            );

                            // **An item is not a block.** Everything a player
                            // can carry shares one id space (see
                            // `register_item`), and the world palette must
                            // never contain one that cannot be placed — a
                            // sword written into a chunk is a chunk that
                            // meshes a sword. Refused here rather than
                            // filtered later, because a refusal a player can
                            // read is the whole point of this loop.
                            if shared.items.contains(&material) {
                                shared.tell(
                                    &request.actor,
                                    "that is not something you can build with".to_owned(),
                                );
                                continue;
                            }

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

                            // The cut the player claims to be holding. A shape
                            // the engine does not consider a shape — empty, or
                            // a full block — is loose material, which is what a
                            // client that knows nothing about shapes sends.
                            let shape = tiamot_core::inventory::Shape::new(request.shape);

                            // **Turned to face whoever placed it**, and toward
                            // their feet against a wall — reported from the
                            // window, and the reason a cut has a front at all.
                            // The charge below matches on the AUTHORED shape,
                            // which is what the player is carrying; only the
                            // geometry turns.
                            let placed_shape = shape.and_then(|shape| {
                                let block = request.target.block();
                                let toward = shared
                                    .player_eye(&request.actor)
                                    .map_or([0.0, 1.0], |(origin, eye)| {
                                        // In cells, through the chunk
                                        // difference — never a world-space f32
                                        // (charter rule 7). The block's centre
                                        // is a cell and a half in from its
                                        // corner.
                                        let span = |axis: usize| {
                                            let chunks = match axis {
                                                0 => origin.x - block.chunk().x,
                                                _ => origin.z - block.chunk().z,
                                            } as f32
                                                * tiamot_core::CHUNK_SUBNODES as f32;
                                            let corner = match axis {
                                                0 => block.x,
                                                _ => block.z,
                                            }
                                            .rem_euclid(tiamot_core::CHUNK_BLOCKS as i32)
                                                as f32
                                                * tiamot_core::SUBNODES_PER_AXIS as f32;
                                            chunks + eye[axis] - (corner + 1.5)
                                        };
                                        [span(0), span(2)]
                                    });
                                tiamot_core::inventory::Shape::new(
                                    tiamot_core::place::oriented(
                                        shape.occupancy(),
                                        request.face,
                                        toward,
                                    ),
                                )
                            });

                            // **What a block already holds**, as a mask of
                            // occupied cells and whether any of it is somebody
                            // else's material.
                            let contents = |at: tiamot_core::BlockPos,
                                                world: &mut crate::world::World,
                                                source: &mut dyn crate::world::ChunkSource| {
                                world.block_cells(tiamot_core::domain::OVERWORLD, at, source).map_or((0, false), |cells| {
                                    let mut mask = 0;
                                    let mut other = false;
                                    for (index, cell) in cells.iter().enumerate() {
                                        if cell.is_air() {
                                            continue;
                                        }
                                        mask |= 1 << index;
                                        other |= *cell != material;
                                    }
                                    (mask, other)
                                })
                            };

                            // **A block brush tops up its OWN material.** A
                            // block brush fills the gaps in a partly-mined
                            // block rather than colliding with what is left of
                            // it — reported from the window — but somebody
                            // building a dirt wall against a chiselled stone
                            // one is not asking to mix the two, so a different
                            // material steps to the next block along the face.
                            // Reported from the window as well; the two halves
                            // are Sub-Node Contract §7.1.
                            //
                            // A sub-node brush is exempt: placing one cell of
                            // anything into a block with room is how a mixed
                            // block gets made on purpose.
                            let (there, holds_other) =
                                contents(request.target.block(), &mut world, &mut source);
                            let target = if matches!(brush, tiamot_core::dig::Brush::Block)
                                && placed_shape.is_none()
                            {
                                match tiamot_core::place::landing(
                                    request.target,
                                    holds_other && there != 0,
                                    request.face,
                                ) {
                                    Some(target) => target,
                                    None => {
                                        shared.tell(
                                            &request.actor,
                                            tiamot_core::place::Refusal::Occupied.to_string(),
                                        );
                                        continue;
                                    }
                                }
                            } else {
                                request.target
                            };
                            let filled = if target == request.target {
                                there
                            } else {
                                contents(target.block(), &mut world, &mut source).0
                            };

                            let outcome =
                                tiamot_core::place::plan(target, held, placed_shape, brush, filled)
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
                            tiamot_core::domain::OVERWORLD,
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
                                // **A cancelled placement is not always a
                                // refused one.** `core_milk` pours milk and
                                // cancels the block write, because the block was
                                // only ever a way of naming what to pour — and
                                // it was being answered with "you cannot build
                                // there" every single time it worked. The mod
                                // now says whether the player should hear
                                // anything; see `HookOutcome::notice`.
                                if let Some(notice) = verdict.notice("you cannot build there") {
                                    shared.tell(&request.actor, notice.to_owned());
                                }
                                continue;
                            }

                            // Charged BEFORE the write, and only what was
                            // actually taken is placed. Writing first and
                            // charging after would hand a player free material
                            // on any path where the debit came up short —
                            // another connection of theirs spending it between
                            // the check and here is enough.
                            let paid = shared.debit(
                                &request.actor,
                                material,
                                shape,
                                request.detail.as_deref(),
                                plan.units,
                            );
                            if paid == 0 {
                                shared.tell(
                                    &request.actor,
                                    tiamot_core::place::Refusal::NothingHeld.to_string(),
                                );
                                continue;
                            }

                            // **The plan's OWN cells.** Sub-Node Contract §7.2:
                            // what gets written is never re-derived from how
                            // many units were paid. `placement_mask(paid)` is
                            // the answer to "what does loose material look
                            // like", and using it here quietly replaced a
                            // crafted shape with a bottom-up lump of the same
                            // size — reported from the window as shape crafting
                            // placing "that many nodes in a block" instead of
                            // the shape — and put a gap-fill in the wrong cells
                            // for the same reason.
                            //
                            // Trimmed to what was actually paid, because the
                            // debit above can come up short.
                            let cells = tiamot_core::place::trim(plan.occupancy, paid);

                            // Whether everything already in the block is the
                            // material going in decides whether one `Partial`
                            // can carry the union or each cell has to go in on
                            // its own — a `Partial` SETS the block, so one
                            // naming only the new cells deletes the rest.
                            let same = filled == 0
                                || world
                                    .block_cells(tiamot_core::domain::OVERWORLD, plan.block, &mut source)
                                    .is_ok_and(|held| {
                                        held.iter()
                                            .all(|cell| cell.is_air() || *cell == material)
                                    });
                            let edits = tiamot_core::place::writes(
                                plan.block,
                                cells,
                                request.material,
                                filled,
                                same,
                            );

                            // Counted, because a per-cell write is several
                            // edits and refunding the whole charge after some
                            // of them landed would mint material.
                            let mut failure = None;
                            let mut applied = 0;
                            for edit in edits {
                                match world.apply(tiamot_core::domain::OVERWORLD, &edit, &mut source) {
                                    Ok(_) => {
                                        applied += tiamot_core::place::edit_units(&edit);
                                        relight.push(edited_block(&edit));
                                        // A pond finds out there is somewhere
                                        // new to go the same way it finds out a
                                        // wall came down: every edit wakes it,
                                        // whichever path the edit arrived by.
                                        fluidics
                                            .write()
                                            .expect("fluid lock")
                                            .touch(edited_block(&edit));
                                        shared.broadcast(ServerMessage::BlockDelta {
                                            edit,
                                            actor: Some(*request.actor.as_bytes()),
                                        });
                                    }
                                    Err(err) => failure = Some(err),
                                }
                            }
                            if let Some(err) = failure {
                                // The write failed after the charge, so give
                                // back what did not land. Anything else either
                                // destroys material on a path a player did not
                                // cause and cannot see, or mints it.
                                // **Given back as what it was.** A refund that
                                // dropped the cut or the detail would turn a
                                // crafted stair, or somebody's named block,
                                // into loose rubble on a path they did not
                                // cause and cannot see.
                                shared.credit(
                                    request.actor,
                                    tiamot_core::inventory::Stack::new(
                                        material,
                                        paid.saturating_sub(applied),
                                    )
                                    .map(|stack| tiamot_core::inventory::Stack {
                                        shape,
                                        detail: request.detail.clone(),
                                        ..stack
                                    })
                                    .into_iter()
                                    .collect(),
                                );
                                debug!(
                                    actor = %request.actor.short(),
                                    "a placement would not apply: {err}"
                                );
                            }
                        }

                        // Mod tick hooks, before edits are applied: a mod
                        // that queues an edit this tick should see it land
                        // this tick, not next.
                        //
                        // `dt_ticks` is 1 here because the loop calls `step`
                        // once per tick even when catching up — the catch-up
                        // is several calls, not one call with a bigger number.
                        //
                        // **The world is lent to the mods here** and taken back
                        // on the next line — see `crate::sight`. The tick cannot
                        // touch `world` inside the closure because it no longer
                        // has one, which is the point: a mod reading terrain is
                        // reading a world nothing else is halfway through
                        // changing.
                        // **Blocks that get a turn this tick.** The cells are
                        // chosen with the world in hand and offered to the mods
                        // inside the lending window below, because a handler
                        // will want to read and write the world — see
                        // `crate::lease`.
                        //
                        // Filtered by material HERE rather than in the VM: a
                        // busy world samples thousands of cells a tick and
                        // almost all of them are stone, so the Lua call has to
                        // be the rare case.
                        let mut random_ticks: Vec<tiamot_core::script::RandomTickEvent> =
                            Vec::new();
                        if !random_tick_materials.is_empty() {
                            let seed = world.seed();
                            let mut budget = MAX_RANDOM_TICK_CHUNKS;
                            for chunk in world.resident_positions(tiamot_core::domain::OVERWORLD) {
                                if budget == 0 {
                                    break;
                                }
                                budget -= 1;
                                for pos in tiamot_core::tick::random::cells(seed, chunk, tick) {
                                    let Some(material) = world.material_at(tiamot_core::domain::OVERWORLD, pos) else {
                                        continue;
                                    };
                                    // The world stores WORLD ids and a mod
                                    // speaks RUNTIME ones (charter rule 8), so
                                    // the set is compared — and the event
                                    // carried — in the mod's own space.
                                    let Some(material) = world.runtime_material(material.0) else {
                                        continue;
                                    };
                                    if random_tick_materials.contains(&material) {
                                        random_ticks.push(
                                            tiamot_core::script::RandomTickEvent {
                                                pos,
                                                material,
                                            },
                                        );
                                    }
                                }
                            }
                        }

                        let (returned, ()) = sight.lending(world, || {
                            for event in &random_ticks {
                                let outcome = source.random_ticked(event);
                                for (mod_id, err) in &outcome.faults {
                                    error!(
                                        mod_id = %mod_id,
                                        "mod disabled after a random_tick failure: {err}"
                                    );
                                }
                            }
                            // Arrivals first: a mod that spawns something for a
                            // new player should have done it before that
                            // player's first tick runs, not after.
                            for event in &joined {
                                let outcome = source.player_joined(event);
                                for (mod_id, err) in &outcome.faults {
                                    error!(
                                        mod_id = %mod_id,
                                        "mod disabled after an on_player_join failure: {err}"
                                    );
                                }
                            }
                            for (mod_id, err) in source.tick(1) {
                                error!(mod_id = %mod_id, "mod disabled after a tick failure: {err}");
                            }
                        });
                        world = returned;

                        // Serve chunk requests. Bounded per tick by
                        // CHUNKS_PER_TICK: encoding is real work on this
                        // thread, and an unbounded drain would let one player
                        // joining stall the world for everyone.
                        for request in shared.take_chunk_requests() {
                            let blob = match world.chunk(tiamot_core::domain::OVERWORLD, request.pos, &mut source) {
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
                                    light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, request.pos)
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
                                touched.extend(light.edited(tiamot_core::domain::OVERWORLD, &world, pos));
                            }
                            broadcast_light(&shared, &light, &touched);
                        }

                        // Chunks that arrived this tick, whatever brought
                        // them in — a chunk request, a player walking, a mod
                        // reading. Asking the world what arrived rather than
                        // hunting every load site means the next route somebody
                        // adds is lit too, instead of being silently black.
                        let arrived = world.take_arrived(tiamot_core::domain::OVERWORLD);
                        if !arrived.is_empty() {
                            // **The milk that was there before, before anything
                            // else looks at the chunk.** Fluid is not derived
                            // state — there is no function from terrain back to
                            // "somebody poured here" — so a chunk arriving
                            // without its saved pond is a pond that is gone.
                            //
                            // Ahead of the relight budget below on purpose: a
                            // chunk whose relight is deferred still needs its
                            // fluid, or the deferral would decide whether the
                            // milk in it survived.
                            let mut fluid = fluidics.write().expect("fluid lock");
                            for pos in &arrived {
                                // Already read: the lighting defers what it
                                // cannot relight by putting the chunk back into
                                // this same list, so chunks arrive twice.
                                // Loading again would replace the live layer
                                // with the last thing written to disk.
                                if fluid.knows(*pos) {
                                    continue;
                                }
                                match world.load_fluid(tiamot_core::domain::OVERWORLD, *pos) {
                                    // Recorded as read either way — a chunk with
                                    // no row is dry, which is an answer.
                                    Ok(layer) => fluid.chunk_loaded(*pos, layer.unwrap_or_default()),
                                    Err(err) => {
                                        // Left unread so the next arrival
                                        // retries. Treating a failed read as
                                        // "dry" would quietly delete a pond.
                                        error!(?pos, "could not load the fluid for a chunk: {err}");
                                    }
                                }
                            }
                            drop(fluid);

                            // **And the entities that live there**, for the
                            // same reason and with the same guard: a chunk
                            // arriving without its mobs is a mob that is gone,
                            // and chunks arrive twice.
                            let mut mobs = population.write().expect("entity lock");
                            for pos in &arrived {
                                if mobs.knows(*pos) {
                                    continue;
                                }
                                match world.load_entities(tiamot_core::domain::OVERWORLD, *pos) {
                                    Ok(entities) => mobs.chunk_loaded(*pos, entities),
                                    Err(err) => {
                                        // Left unread so the next arrival
                                        // retries. Treating a failed read as
                                        // "empty" would quietly delete a mob.
                                        error!(?pos, "could not load the entities for a chunk: {err}");
                                    }
                                }
                            }
                            drop(mobs);

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
                                    world.defer_arrival(tiamot_core::domain::OVERWORLD, pos);
                                    continue;
                                }
                                control.note_full_relight();
                                touched.extend(light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos));
                                done += 1;
                            }
                            broadcast_light(&shared, &light, &touched);
                        }
                        control.note_lit_chunks(lighting.read().expect("lighting lock").len());

                        // **Punches, judged here and nowhere else.** A client
                        // says which entity it hit; the server decides whether
                        // it could have. Charter rule 2: a viewer that could
                        // assert a hit could assert every hit.
                        //
                        // What a hit MEANS is not decided here at all. The
                        // engine has no damage model — it reports who hit what
                        // and the mods do the rest (charter rule 1).
                        for (uuid, target) in shared.drain_punches() {
                            let id = tiamot_core::ent::EntityId(target);
                            let Some((centre, owner)) = ({
                                let mobs = population.read().expect("entity lock");
                                mobs.get(id).map(|entity| {
                                    (entity.transform, entity.owner.map(|owner| owner.0))
                                })
                            }) else {
                                // A stale id, or one a client invented. Silence
                                // rather than an error: an entity that
                                // despawned between the click and the tick is
                                // ordinary, and telling the mods about a punch
                                // at nothing would be inventing an event.
                                continue;
                            };

                            let Some(attacker) = ({
                                let bodies = shared.bodies.lock().ok();
                                bodies.and_then(|bodies| {
                                    bodies.get(&uuid).map(|player| {
                                        tiamot_core::ent::Transform::at(
                                            player.origin,
                                            player.body.position,
                                        )
                                    })
                                })
                            }) else {
                                continue;
                            };

                            // The same reach the crosshair has, measured
                            // between the two bodies' feet. Squared, so no
                            // root: charter rule 4 allows `sqrt`, but there is
                            // no reason to spend one on a comparison.
                            let offset = centre.offset_to(&attacker);
                            let distance =
                                offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2];
                            let reach = tiamot_core::phys::ray::REACH;
                            if distance > reach * reach {
                                debug!(
                                    actor = %uuid.short(),
                                    "a punch was thrown from further away than an arm reaches"
                                );
                                continue;
                            }

                            // **The swing everybody else sees.** A punch is
                            // one message with no duration, so the attacker's
                            // body is told to keep swinging for a few ticks —
                            // otherwise the tag is gone before the next entity
                            // update goes out and a hit looks like nothing
                            // happening. Set before the hooks run, because it
                            // is about the swing rather than about whether it
                            // landed: you swung either way.
                            if let Ok(mut bodies) = shared.bodies.lock()
                                && let Some(player) = bodies.get_mut(&uuid)
                            {
                                player.swung_on = tick;
                            }

                            let verdict = source.may_punch(&tiamot_core::script::PunchEvent {
                                attacker: *uuid.as_bytes(),
                                target: id,
                                owner: owner.map(|owner| *owner.as_bytes()),
                            });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_punch failure: {err}");
                            }
                        }

                        // **Mod-registered actions, before the mods' entity
                        // logic.** A mod that flips a mode on a key press and
                        // reads that mode while stepping its mobs should see
                        // this tick's press, not the last one's.
                        //
                        // Charter rule 11 ends here: the mod is told the action
                        // and never the key. Whether the id is real was settled
                        // at the edge by `queue_action`, against the same table
                        // the client was sent.
                        for (uuid, id, pressed) in shared.drain_actions() {
                            let verdict = source.did_action(&tiamot_core::script::ActionEvent {
                                player: *uuid.as_bytes(),
                                id,
                                pressed,
                            });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_action failure: {err}");
                            }
                        }

                        // Chat, which every mod may veto. Broadcast only if
                        // nobody refused: a line already sent cannot be unsent,
                        // which is why this happens here and not on the
                        // connection task that received it.
                        for (uuid, text) in shared.drain_chat() {
                            let verdict = source.may_chat(&tiamot_core::script::ChatEvent {
                                player: *uuid.as_bytes(),
                                text: text.clone(),
                            });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_chat failure: {err}");
                            }
                            if !verdict.allowed {
                                // Told to the speaker alone, so they know it
                                // did not go out rather than wondering.
                                let reason = verdict.reason.clone().unwrap_or_else(|| {
                                    "a mod refused that message".to_owned()
                                });
                                let _ = shared.push_entity_messages(
                                    &uuid,
                                    std::iter::once(tiamot_core::proto::ServerMessage::Chat {
                                        from: None,
                                        text: reason,
                                    }),
                                );
                                continue;
                            }
                            shared.broadcast(tiamot_core::proto::ServerMessage::Chat {
                                from: Some(*uuid.as_bytes()),
                                text,
                            });
                        }

                        // And the dialogs. Delivered to the OWNER alone —
                        // `Screens` recorded who opened each form, and an
                        // event for a form nobody owns is a client describing
                        // a dialog that is not open, which is dropped rather
                        // than guessed at.
                        for (uuid, form, event) in shared.drain_dialog_events() {
                            let player = uuid.to_hex();
                            let Some(owner) = dialog_screens
                                .as_ref()
                                .and_then(|screens| screens.owner_of(&player, &form))
                            else {
                                continue;
                            };
                            // **A slot click is applied HERE, before the mod
                            // hears about it.** The server's inventory is the
                            // authority, and the mod is told what happened
                            // rather than asked to make it happen — so a mod
                            // that ignores the event still cannot leave the
                            // player's items in a state nobody agreed to.
                            if let tiamot_core::proto::DialogEvent::Clicked {
                                view, index, click
                            } = &event
                                && shared.click_slot(&uuid, view, usize::from(*index), *click)
                            {
                                // Every view, not just the clicked one: a
                                // shift-click moves a stack BETWEEN views, so
                                // telling the client about one of them leaves
                                // the other showing something that has moved.
                                let _ = shared
                                    .push_entity_messages(&uuid, shared.view_updates(&uuid));
                            }
                            let closing = matches!(
                                event,
                                tiamot_core::proto::DialogEvent::Closed
                            );
                            // **A stack in hand goes back when the screen
                            // goes.** It has no picture once the screen it was
                            // picked up on is gone, which is how an item seems
                            // to disappear for good — and it would now stay
                            // there across a save.
                            // **And any container that screen had open.** A
                            // container lent to somebody whose screen has gone
                            // is a container nobody can ever open again — and
                            // its contents would sit in a player's slots,
                            // saved as theirs.
                            if closing
                                && let Ok(mut store) = containers.lock()
                            {
                                let put_back = shared
                                    .with_slots(&uuid, |slots| store.close_all(uuid, slots))
                                    .unwrap_or_default();
                                if !put_back.is_empty() {
                                    shared.mark_inventory_dirty(&uuid);
                                }
                            }
                            if closing && shared.return_held(&uuid) {
                                let _ = shared
                                    .push_entity_messages(&uuid, shared.view_updates(&uuid));
                            }
                            let verdict =
                                source.did_dialog_event(&tiamot_core::script::DialogEvent {
                                    player: *uuid.as_bytes(),
                                    mod_id: owner,
                                    form: form.clone(),
                                    event,
                                });
                            for (mod_id, err) in &verdict.faults {
                                error!(mod_id = %mod_id, "mod disabled after an on_dialog_event failure: {err}");
                            }
                            // A player closing a dialog closes it, whatever the
                            // mod does about it. Otherwise a mod that ignored
                            // the event would leave the form owned for ever and
                            // the player unable to reopen it.
                            if closing && let Some(screens) = dialog_screens.as_ref() {
                                tiamot_core::ui::host::Access::close(
                                    screens.as_ref(),
                                    &player,
                                    &form,
                                );
                            }
                        }

                        // **The mods' own entity logic, before the physics.**
                        //
                        // A mod sets `drive` and the step that follows acts on
                        // it, so a mob reacts within the tick it decided rather
                        // than the one after — which at 20 Hz is the difference
                        // between a mob that turns when you do and one that
                        // always lags you by 50 ms.
                        //
                        // Lent the world, like the tick hooks above: this is
                        // where a mob decides what to do, so it is exactly where
                        // `game.line_of_sight` has to work.
                        {
                            let owned = population.read().expect("entity lock").owned_by_mod();
                            if !owned.is_empty() {
                                let (returned, ()) = sight.lending(world, || {
                                    for (mod_id, err) in source.entity_step(&owned, 1) {
                                        error!(mod_id = %mod_id, "entity step failed: {err}");
                                    }
                                });
                                world = returned;
                            }
                        }

                        // **Entities, every tick.** Unlike fluid there is no
                        // halving to be had: a mob stepping at 10 Hz is a mob
                        // that visibly stutters, because unlike a pond it is
                        // something a player is looking straight at.
                        {
                            let fluid = fluidics.read().expect("fluid lock");
                            population
                                .write()
                                .expect("entity lock")
                                .tick(tiamot_core::domain::OVERWORLD, &world, &fluid);
                        }

                        // **What each player is told about the entities.**
                        //
                        // One tracker per player, per `ent::replicate`. The
                        // decision of what to send is pure and lives in `core`;
                        // this is only the plumbing that asks and queues.
                        //
                        // Inside its own scope, and holding no mod-facing lock:
                        // nothing here enters a callback, which is what would
                        // deadlock the tick thread against itself.
                        {
                            let mobs = population.read().expect("entity lock");
                            let bodies = shared.bodies.lock();
                            if let Ok(bodies) = bodies {
                                for (uuid, player) in bodies.iter() {
                                    let tracker = trackers.entry(*uuid).or_default();
                                    let update = tracker.update(
                                        mobs.entities(),
                                        player.origin,
                                        shared.view_distance,
                                        // Their own body. Nobody needs to be
                                        // told where they are by the machine
                                        // they are telling, and a client drawing
                                        // itself sees the inside of its own
                                        // head.
                                        mobs.player_entity(uuid),
                                    );
                                    if update.is_empty() {
                                        continue;
                                    }
                                    let overflowed = shared.push_entity_messages(
                                        uuid,
                                        entity_messages(update, tick, &shared),
                                    );
                                    if overflowed {
                                        // The queue was cleared, so what this
                                        // player has been told is now unknown.
                                        // Forgetting is the only recoverable
                                        // answer: the next pass re-spawns
                                        // everything in view from scratch.
                                        warn!(?uuid, "entity queue overflowed; resending in full");
                                        tracker.clear();
                                    }
                                }
                                // Players who left take their tracker with
                                // them, or a long-running server accumulates
                                // one per person who has ever connected.
                                trackers.retain(|uuid, _| bodies.contains_key(uuid));
                            }
                        }

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
                            let changes = fluid.tick(
                                        tiamot_core::domain::OVERWORLD,
                                &world,
                                tick / crate::fluid::TICKS_PER_FLUID_TICK,
                                world.seed(),
                            );
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

                            // **Where the milk tried to go and could not.**
                            //
                            // The fluid layer records where milk IS, so a block
                            // it cannot enter is indistinguishable from one it
                            // never reached — which is exactly the fact a
                            // waterlogging mod needs. `on_fluid_flow` is the
                            // only way a mod can learn it.
                            //
                            // The lock is dropped before the callbacks run.
                            // A mod's `game.set_fluid` takes the same lock, and
                            // holding it across a callback is the one
                            // arrangement that deadlocks.
                            // **Saturation, applied where the registry is.**
                            // Sub-Node Contract §4.3: the solver knows a rate
                            // and reports that a block drank; turning that into
                            // "dirt is damp dirt now" needs the world and the
                            // material table, neither of which the solver has.
                            //
                            // Collected under the lock and applied after it is
                            // dropped, because a terrain edit relights and
                            // broadcasts, and both of those want the tick
                            // thread rather than the fluid lock.
                            let sinks = fluid.take_sinks();
                            let soaked: Vec<(tiamot_core::BlockPos, tiamot_core::MaterialId)> =
                                sinks
                                    .absorbed
                                    .iter()
                                    .filter_map(|taken| {
                                        let block = world.resident(tiamot_core::domain::OVERWORLD, taken.pos.chunk())?;
                                        let becomes = fluid
                                            .absorbs_block(&block.get_block_local(taken.pos.local()))?
                                            .becomes?;
                                        Some((taken.pos, becomes))
                                    })
                                    .collect();
                            let blocked = fluid.take_blocked();
                            drop(fluid);

                            for (pos, becomes) in soaked {
                                // Whatever shape the block had, kept: a
                                // chiselled step that soaks is still a step.
                                let edit = tiamot_core::proto::Edit::Block {
                                    pos,
                                    material: becomes.0,
                                };
                                match world.apply(tiamot_core::domain::OVERWORLD, &edit, &mut source) {
                                    Ok(_) => {
                                        relight.push(pos);
                                        // The pond has to hear about it too: a
                                        // block that soaked may have crossed
                                        // its own `waterlogs_at` and stopped
                                        // being somewhere fluid can go.
                                        fluidics
                                            .write()
                                            .expect("fluid lock")
                                            .touch(pos);
                                        shared.broadcast(ServerMessage::BlockDelta {
                                            edit,
                                            actor: None,
                                        });
                                    }
                                    Err(err) => {
                                        warn!("a soaked block could not be written: {err}");
                                    }
                                }
                            }
                            for event in blocked {
                                let Some(name) = fluidics
                                    .read()
                                    .expect("fluid lock")
                                    .name_of(event.fluid)
                                    .map(str::to_owned)
                                else {
                                    // A placeholder: its mod is gone, so no mod
                                    // can be listening for it by name.
                                    continue;
                                };
                                let cells = world
                                    .block_cells(tiamot_core::domain::OVERWORLD, event.into, &mut source)
                                    .unwrap_or(tiamot_core::block::EMPTY_CELLS);
                                // The first non-air cell names the block: a
                                // mixed block has no single material, and the
                                // occupancy below is what says so.
                                let world_id = cells
                                    .iter()
                                    .find(|cell| !cell.is_air())
                                    .copied()
                                    .unwrap_or(tiamot_core::MaterialId::AIR);
                                // World id to RUNTIME id: what a mod holds is
                                // never what a chunk holds (charter rule 8).
                                let material = world
                                    .runtime_material(world_id.0)
                                    .unwrap_or(tiamot_core::MaterialId::UNKNOWN);
                                let mut occupancy = 0u32;
                                for (index, cell) in cells.iter().enumerate() {
                                    if !cell.is_air() {
                                        occupancy |= 1 << index;
                                    }
                                }
                                let verdict =
                                    source.fluid_blocked(&tiamot_core::script::FluidFlowEvent {
                                        from: event.from,
                                        into: event.into,
                                        fluid: name,
                                        volume: event.volume,
                                        blocked_by: material,
                                        occupancy,
                                    });
                                for (mod_id, err) in &verdict.faults {
                                    error!(mod_id = %mod_id, "mod disabled after an on_fluid_flow failure: {err}");
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
                        if tick % SAVE_INTERVAL_TICKS == 0 || control.take_save_request() {
                            if let Err(err) = world.save_dirty() {
                                error!("could not save dirty chunks: {err}");
                            }

                            // A mod's own facts on the same debounce, for the
                            // same reason: a state machine that writes a key
                            // every tick would otherwise be a database write
                            // every tick.
                            flush_mod_storage(&world, &mod_storage);
                        flush_containers(&world, &containers);
                            flush_containers(&world, &containers);

                            let mobs = population.write().expect("entity lock").take_dirty();
                            if !mobs.is_empty()
                                && let Err(err) = world.save_entities(
                                    tiamot_core::domain::OVERWORLD,
                                    mobs.iter().map(|(pos, held)| (*pos, held.as_slice())),
                                )
                            {
                                error!("could not save entities: {err}");
                            }

                            // **Fluid on the same debounce, and it needs one
                            // just as much**: a spreading pond changes tens of
                            // blocks a tick, and writing its chunk every time
                            // would be a database write per tick for as long as
                            // the milk was moving.
                            let dirty = fluidics.write().expect("fluid lock").take_dirty();
                            if !dirty.is_empty()
                                && let Err(err) =
                                    world.save_fluid(tiamot_core::domain::OVERWORLD, dirty.iter().map(|(pos, layer)| (*pos, layer)))
                            {
                                // Put them back rather than dropping them. A
                                // failed write that also forgot what it was
                                // trying to write would lose the pond at the
                                // next chunk unload, silently.
                                error!("could not save fluid: {err}");
                                let mut fluid = fluidics.write().expect("fluid lock");
                                for (pos, _) in dirty {
                                    fluid.mark_dirty(pos);
                                }
                            }
                        }

                        held = Some(world);
                    });

                    let mut world = held.expect("the last tick put the world back");

                    // The network thread is already stopped by the time this
                    // returns, so a blocking lock here cannot contend with a
                    // live connection — and a final flush must not be skipped
                    // just because the last tick happened to find the lock
                    // taken.
                    //
                    // The last of the milk. `World::close` flushes dirty chunks
                    // and knows nothing about fluid, which lives in its own
                    // store — so without this, everything poured since the last
                    // debounced save would be lost on a clean shutdown, the one
                    // case a player has every right to expect nothing is.
                    {
                        flush_mod_storage(&world, &mod_storage);
                        // **Everybody still connected.** A clean shutdown is
                        // the one case a player has every right to expect
                        // nothing is lost, and the leave-diff above only fires
                        // for somebody the tick SAW go — which nobody does when
                        // the server stops under them.
                        // **Containers come back BEFORE the inventories are
                        // written**, or a chest somebody had open is saved as
                        // part of that player and lost from the world — which
                        // is a duplication on their side and a hole on the
                        // world's.
                        if let Ok(mut store) = containers.lock() {
                            for (uuid, _) in shared.all_inventories() {
                                let _ = shared
                                    .with_slots(&uuid, |slots| store.close_all(uuid, slots));
                            }
                            store.mark_all();
                        }
                        flush_containers(&world, &containers);
                        for (uuid, slots) in shared.all_inventories() {
                            if let Err(err) = world.save_player_slots(&uuid, &slots) {
                                error!(
                                    player = %uuid.to_hex(),
                                    "could not save an inventory on shutdown: {err}"
                                );
                            }
                        }
                        let mobs = population.write().expect("entity lock").take_dirty();
                        if !mobs.is_empty()
                            && let Err(err) = world
                                .save_entities(tiamot_core::domain::OVERWORLD, mobs.iter().map(|(pos, held)| (*pos, held.as_slice())))
                        {
                            error!("could not save entities: {err}");
                        }
                        let dirty = fluidics.write().expect("fluid lock").take_dirty();
                        if !dirty.is_empty()
                            && let Err(err) =
                                world.save_fluid(tiamot_core::domain::OVERWORLD, dirty.iter().map(|(pos, layer)| (*pos, layer)))
                        {
                            error!("could not save fluid on shutdown: {err}");
                        }
                    }

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
            operators: Vec::new(),
            view_distance: tiamot_core::interest::ViewDistance::DEFAULT,
            seed: None,
            mods_path: None,
            enabled_mods: None,
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

    /// The same, for a block that is only partly filled.
    ///
    /// For arranging the carved block a test is about to act on, without
    /// mining one cell at a time to get there.
    pub fn seed_partial(&self, pos: tiamot_core::BlockPos, material: u16, occupancy: u32) -> bool {
        self.shared.queue_seed(tiamot_core::proto::Edit::Partial {
            pos,
            material,
            occupancy,
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
    /// Suspends or resumes the simulation.
    ///
    /// **Singleplayer's pause.** Only meaningful for an embedded server: a
    /// hosted one has other people in it, and one of them opening a menu must
    /// not stop the world for everybody. The client only calls this when it
    /// owns the server it is talking to.
    pub fn set_paused(&self, paused: bool) {
        self.control.set_paused(paused);
    }

    /// Whether the simulation is suspended.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.control.paused()
    }

    pub fn stop(mut self) -> bool {
        self.shutdown()
    }

    /// Announces this server on the local network under `name`.
    ///
    /// **Off unless asked**, like binding to more than loopback and for the
    /// same reason: a beacon tells every machine on the segment that this port
    /// is open. Returns `None` if no socket could be opened, which is not a
    /// reason to stop hosting — the world is still reachable by address.
    ///
    /// Announcing stops when the returned [`crate::announce::Announcer`] is
    /// dropped, so a caller that wants it for the life of the world holds it
    /// beside the handle.
    #[must_use]
    pub fn announce(&self, name: &str) -> Option<crate::announce::Announcer> {
        crate::announce::Announcer::start(name, self.local_addr.port(), &self.shared)
    }

    /// The same, to destinations of the caller's choosing.
    ///
    /// See [`crate::announce::Announcer::start_to`] — a host with more than one
    /// interface names them, and a test points one at a port it owns rather
    /// than at a broadcast address the machine may have no route for.
    #[must_use]
    pub fn announce_to(
        &self,
        name: &str,
        destinations: Vec<std::net::SocketAddr>,
    ) -> Option<crate::announce::Announcer> {
        crate::announce::Announcer::start_to(
            name,
            self.local_addr.port(),
            &self.shared,
            destinations,
        )
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

/// `game.get_tool` and `game.set_tool`, backed by the connected players.
///
/// The seam from [`tiamot_core::dig::Tools`]. Which tool a player holds lives
/// with their body, above `core`, and the VM lives inside it (charter rule 3).
/// The seam `game.inventory`, `game.give` and `game.take` reach through.
///
/// **The mod API's route into an inventory, and the only one.** Charter rule 1
/// puts crafting in a mod, and until this existed only digging could credit an
/// inventory and only placing could debit one — so a shaped stack, which only
/// crafting produces, could not be brought into being at all.
///
/// Everything it does happens to the SERVER's copy and is followed by an
/// update to the player, the same direction digging already runs in. Installed
/// here rather than only in the VM's tests, because `game.set_block` was dead
/// on every real server for three tasks for exactly that reason.
struct Carried {
    shared: std::sync::Arc<crate::transport::endpoint::Shared>,
}

impl tiamot_core::inventory::Access for Carried {
    fn contents(&self, player: [u8; 32], view: &str) -> Vec<tiamot_core::inventory::Stack> {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        self.shared.contents_of(&uuid, view)
    }

    fn give(&self, player: [u8; 32], view: &str, stack: tiamot_core::inventory::Stack) -> bool {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        self.shared.give(&uuid, view, stack)
    }

    fn held(&self, player: [u8; 32]) -> Option<tiamot_core::inventory::Stack> {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        // Absent means slot zero, which is where a client starts and stays
        // until somebody presses a hotbar key.
        let slot = self
            .shared
            .held_slot
            .lock()
            .ok()
            .and_then(|held| held.get(&uuid).copied())
            .unwrap_or(0);
        self.shared
            .slots_of(&uuid)?
            .view(tiamot_core::inventory::PLAYER_MAIN)?
            .slots
            .get(slot)
            .cloned()
            .flatten()
    }

    fn take(
        &self,
        player: [u8; 32],
        view: &str,
        material: tiamot_core::material::MaterialId,
        shape: Option<tiamot_core::inventory::Shape>,
        detail: Option<&str>,
        units: u32,
    ) -> u32 {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        self.shared
            .take(&uuid, view, material, shape, detail, units)
    }
}

struct HeldTools {
    shared: std::sync::Arc<crate::transport::endpoint::Shared>,
}

impl tiamot_core::dig::Tools for HeldTools {
    fn tool(&self, player: [u8; 32]) -> Option<String> {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        let bodies = self.shared.bodies.lock().ok()?;
        bodies.get(&uuid).and_then(|body| body.tool.clone())
    }

    fn set_tool(&self, player: [u8; 32], tool: Option<&str>) -> bool {
        let uuid = tiamot_core::identity::PlayerUuid::from_bytes(player);
        // **Refused rather than stored** when the tool is one no mod
        // registered: a tool id that never resolves is a dig that silently
        // never progresses, which is far harder to diagnose than a `false`.
        if let Some(id) = tool
            && !self.shared.tools.contains_key(id)
        {
            return false;
        }
        // A player who is not connected is refused for the same reason — a mod
        // that swapped a tool on somebody who had left would be writing state
        // nobody owns.
        let connected = self
            .shared
            .bodies
            .lock()
            .is_ok_and(|bodies| bodies.contains_key(&uuid));
        if !connected {
            return false;
        }
        self.shared.select_tool(&uuid, tool.map(ToOwned::to_owned));
        true
    }
}

/// `game.show_dialog`, delivered to one player.
/// `game.show_dialog`, delivered to one player.
///
/// The seam from [`tiamot_core::ui::host::Access`]. Unlike a sound, there is no
/// deciding who to tell: a dialog is opened on one screen, by UUID, and nobody
/// else is told it exists.
///
/// **Who owns which form is recorded here**, because that is what routes the
/// events back. A dialog's events go to the mod that opened it and to no other
/// (see `ui::host::Owner`), and the only way to know the owner later is to have
/// written it down when it opened.
struct Screens {
    shared: std::sync::Arc<crate::transport::endpoint::Shared>,
    /// Who opened each open dialog, by (player UUID, form).
    owners: std::sync::Mutex<std::collections::BTreeMap<(String, String), String>>,
}

impl Screens {
    /// The mod that owns an open dialog, if one does.
    fn owner_of(&self, player: &str, form: &str) -> Option<String> {
        self.owners
            .lock()
            .ok()?
            .get(&(player.to_owned(), form.to_owned()))
            .cloned()
    }

    /// Forgets every dialog a departing player had open.
    fn forget_player(&self, player: &str) {
        if let Ok(mut owners) = self.owners.lock() {
            owners.retain(|(uuid, _), _| uuid != player);
        }
    }
}

impl tiamot_core::ui::host::Access for Screens {
    fn show(&self, request: &tiamot_core::ui::host::ShowRequest) -> bool {
        // The owner is the namespace of the form, which `qualify_id` has
        // already guaranteed is the calling mod's own.
        let owner = request
            .form
            .split_once(':')
            .map_or_else(|| request.form.clone(), |(owner, _)| owner.to_owned());
        let message = if request.update {
            tiamot_core::proto::ServerMessage::UpdateDialog {
                form: request.form.clone(),
                tree: request.tree.clone(),
                compact: request.compact,
            }
        } else {
            tiamot_core::proto::ServerMessage::ShowDialog {
                form: request.form.clone(),
                tree: request.tree.clone(),
                compact: request.compact,
            }
        };
        // A mod names a player by their canonical UUID hex (charter rule 13).
        // A name that is not one is a mod's mistake, and the honest answer is
        // "nobody was shown it" rather than an error nobody can act on.
        let Ok(uuid) = tiamot_core::identity::PlayerUuid::from_hex(&request.player) else {
            return false;
        };
        // **`push_entity_messages` returns whether the queue OVERFLOWED**, not
        // whether it sent — a bare `bool` whose `true` is the failure. Read
        // the other way round, this recorded no owners at all and every event
        // came back to a dialog nobody owned.
        let overflowed = self
            .shared
            .push_entity_messages(&uuid, std::iter::once(message));
        if !overflowed && let Ok(mut owners) = self.owners.lock() {
            owners.insert((request.player.clone(), request.form.clone()), owner);
        }
        // **Seed the slots with the dialog.** A dialog with an `item_grid` in
        // it would otherwise draw empty boxes until something else marked the
        // inventory dirty, and "my chest looks empty" is indistinguishable from
        // a bug.
        let _ = self
            .shared
            .push_entity_messages(&uuid, self.shared.view_updates(&uuid));
        !overflowed
    }

    fn close(&self, player: &str, form: &str) -> bool {
        let was_open = self.owners.lock().is_ok_and(|mut owners| {
            owners
                .remove(&(player.to_owned(), form.to_owned()))
                .is_some()
        });
        if let Ok(uuid) = tiamot_core::identity::PlayerUuid::from_hex(player) {
            let _ = self.shared.push_entity_messages(
                &uuid,
                std::iter::once(tiamot_core::proto::ServerMessage::CloseDialog {
                    form: form.to_owned(),
                }),
            );
        }
        was_open
    }
}

/// `game.play_sound`, delivered to whoever is close enough to hear it.
///
/// The seam from [`tiamot_core::sound::Access`]. Deciding who is in earshot
/// needs every connected player, which lives here rather than in `core`
/// (charter rule 3) — and the engine has no idea what a sound IS, only who to
/// tell about one (rule 1).
struct Earshot {
    shared: std::sync::Arc<crate::transport::endpoint::Shared>,
}

/// The next `count` sub-nodes to take out of a block, as edits.
///
/// # Why the order is fixed and random at once
///
/// A block that came apart in index order would peel in flat layers, which
/// reads as a bug rather than as breaking. A random order reads as material
/// giving way — but it has to be the SAME order for everybody, or two players
/// watching one block would see different shapes and a rejoining player would
/// see neither. `dig::crumble_order` is a seeded stream keyed by the block's own
/// position, so it is stable across clients, restarts and rejoins.
///
/// Cells already gone are skipped, so a block half dug by somebody else carries
/// on from where it is rather than spending bites on air.
///
/// `toward` points from the block to whoever is digging it, so the face they
/// are looking at comes away first and what is left is resting against the far
/// side — see [`tiamot_core::dig::crumble_order`].
fn crumble_bites(
    world: &mut crate::world::World,
    source: &mut dyn crate::world::ChunkSource,
    block: tiamot_core::BlockPos,
    count: u32,
    toward: [f64; 3],
) -> Vec<tiamot_core::proto::Edit> {
    let Ok(cells) = world.block_cells(tiamot_core::domain::OVERWORLD, block, source) else {
        return Vec::new();
    };
    let order = tiamot_core::dig::crumble_order(world.seed(), block, toward);
    let mut edits = Vec::with_capacity(count as usize);
    for index in order {
        if edits.len() >= count as usize {
            break;
        }
        let slot = usize::from(index);
        if cells.get(slot).copied() == Some(tiamot_core::MaterialId::AIR) {
            continue;
        }
        let (dx, dy, dz) = tiamot_core::block::subnode_offset(slot);
        #[expect(
            clippy::cast_possible_wrap,
            reason = "three, and sub-node offsets of 0, 1 or 2"
        )]
        let span = tiamot_core::SUBNODES_PER_AXIS as i32;
        #[expect(clippy::cast_possible_wrap, reason = "a sub-node offset is 0, 1 or 2")]
        let pos = tiamot_core::SubNodePos::new(
            block.x * span + dx as i32,
            block.y * span + dy as i32,
            block.z * span + dz as i32,
        );
        edits.push(tiamot_core::proto::Edit::SubNode {
            pos,
            material: tiamot_core::MaterialId::AIR.0,
        });
    }
    edits
}

impl tiamot_core::sound::Access for Earshot {
    fn play(&self, request: &tiamot_core::sound::PlayRequest) -> u32 {
        let message = tiamot_core::proto::ServerMessage::PlaySound {
            sound: request.sound.clone(),
            pos: request.pos,
            radius: request.radius,
            gain: request.gain,
            entity: request.entity,
        };

        // **Who is close enough, decided here and not by the client.** A client
        // told about every sound in the world could hear through walls and
        // across a continent, and would pay for the messages either way.
        //
        // A sound that follows an ENTITY is sent to everyone in radius of where
        // the mod says it starts. Following it after that is the client's job:
        // it has the entity's interpolated position every frame, and the server
        // does not.
        let Ok(bodies) = self.shared.bodies.lock() else {
            return 0;
        };
        let radius = f64::from(request.radius);
        let mut told = 0;
        for (uuid, player) in bodies.iter() {
            let at =
                tiamot_core::ent::Transform::at(player.origin, player.body.position).to_world();
            // Squared, so there is no root: charter rule 4 does not reach audio,
            // but a square root nobody needs is still a square root nobody
            // needs.
            let offset = [
                at[0] - request.pos[0],
                at[1] - request.pos[1],
                at[2] - request.pos[2],
            ];
            let distance = offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2];
            if distance > radius * radius {
                continue;
            }
            // Queued per player rather than broadcast, which is the whole point:
            // the queue is drained on that player's own connection task.
            let _ = self
                .shared
                .push_entity_messages(uuid, std::iter::once(message.clone()));
            told += 1;
        }
        told
    }

    fn start_loop(&self, request: &tiamot_core::sound::LoopRequest) -> u32 {
        let message = tiamot_core::proto::ServerMessage::StartLoop {
            id: request.id.clone(),
            sound: request.sound.clone(),
            pos: request.pos,
            radius: request.radius,
            gain: request.gain,
            everywhere: request.everywhere,
        };
        // **Ambience reaches everybody; a positional loop reaches its radius.**
        // A loop with no position has no distance to be outside of, and the
        // whole reason a mod asks for one is that it should be on wherever the
        // player is standing.
        if request.everywhere {
            self.tell_all(&message)
        } else {
            self.tell_within(&message, request.pos, request.radius)
        }
    }

    fn time_of_day(&self) -> f32 {
        self.shared.day_fraction()
    }

    fn stop_loop(&self, id: &str) -> u32 {
        // **Told to everybody, whatever the loop's radius was.** A player who
        // walked out of a positional loop's radius has already been told to
        // stop it by leaving; a player still inside must be told now. Working
        // out which is which needs the loop's original position, and keeping a
        // server-side registry of running loops to answer that would be a
        // second copy of state the clients already hold. A stop for a loop a
        // client is not running is a no-op on the client, so the cheap answer
        // is also the correct one.
        self.tell_all(&tiamot_core::proto::ServerMessage::StopLoop { id: id.to_owned() })
    }
}

impl Earshot {
    /// Queues a message for every connected player.
    fn tell_all(&self, message: &tiamot_core::proto::ServerMessage) -> u32 {
        let Ok(bodies) = self.shared.bodies.lock() else {
            return 0;
        };
        let mut told = 0;
        for uuid in bodies.keys() {
            let _ = self
                .shared
                .push_entity_messages(uuid, std::iter::once(message.clone()));
            told += 1;
        }
        told
    }

    /// Queues a message for every player within `radius` of `pos`.
    fn tell_within(
        &self,
        message: &tiamot_core::proto::ServerMessage,
        pos: [f64; 3],
        radius: f32,
    ) -> u32 {
        let Ok(bodies) = self.shared.bodies.lock() else {
            return 0;
        };
        let radius = f64::from(radius);
        let mut told = 0;
        for (uuid, player) in bodies.iter() {
            let at =
                tiamot_core::ent::Transform::at(player.origin, player.body.position).to_world();
            let offset = [at[0] - pos[0], at[1] - pos[1], at[2] - pos[2]];
            let distance = offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2];
            if distance > radius * radius {
                continue;
            }
            let _ = self
                .shared
                .push_entity_messages(uuid, std::iter::once(message.clone()));
            told += 1;
        }
        told
    }
}
