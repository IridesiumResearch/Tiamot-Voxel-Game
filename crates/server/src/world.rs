// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The live world: a chunk cache over the database, owned by the simulation.
//!
//! Every chunk the simulation touches is loaded once and kept. Edits mutate the
//! cached copy and mark it dirty; a save writes only what changed.
//!
//! # Only the simulation thread touches this
//!
//! Not a hedge — a structural rule. Chunk edits arrive from the network as
//! messages on a queue and are applied here, in tick order, on one thread. Two
//! threads editing the same chunk would produce a result that depended on which
//! got the lock, which is precisely what charter rule 4 rules out.
//!
//! # Missing chunks are generated, not empty
//!
//! A chunk that is not in the database has never been visited. Returning an
//! empty one would mean players fall through the floor at the edge of explored
//! space; generating it through the mods' `on_generate` callbacks means a fresh
//! chunk is the same on every server that shares the seed.
//!
//! # Generated chunks are persisted, not regenerated on demand
//!
//! Worldgen is deterministic, so a regenerated chunk is byte-identical *today*
//! and storing it looks like wasted disk. It is not. A worldgen mod update, a
//! change to the noise implementation, or a different mod set would silently
//! rewrite terrain a player has already seen and built next to — their house
//! would end up half-buried in a hill that was not there yesterday.
//!
//! Writing a chunk the first time it is generated freezes it. That is what
//! makes explored land stable across everything that might change underneath
//! it, and it is what charter rule 6 means by "only modified **or generated**
//! chunks persist".

use std::collections::HashMap;

use tiamot_core::block::{BlockView, Cells, EMPTY_CELLS};
use tiamot_core::fluid::FluidLayer;
use tiamot_core::inventory::{self, Stack};
use tiamot_core::proto::Edit;
use tiamot_core::script::ScriptVm as _;
use tiamot_core::{
    BlockPos, BlockValue, Chunk, ChunkPos, MaterialId, SubNodePos, WorldDb, WorldError,
};
use tracing::warn;

/// An edit could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The chunk could not be read or generated.
    #[error("could not reach the chunk holding {pos:?}")]
    Unreachable {
        /// The position that could not be reached.
        pos: ChunkPos,
        /// Why.
        #[source]
        source: Box<WorldError>,
    },

    /// The material id is not registered.
    #[error("material id {id} is not registered on this server")]
    UnknownMaterial {
        /// The offending id.
        id: u16,
    },

    /// The position is outside the world's coordinate range.
    #[error("position is outside the world")]
    OutOfWorld,
}

/// Produces chunks the world has never seen.
///
/// A trait rather than a direct call into the mod host so that `World` does not
/// depend on the script VM: the tests below drive it with a flat generator, and
/// a server with no mods uses [`Air`].
pub trait ChunkSource {
    /// Generates the chunk at `pos`.
    ///
    /// Failure is not an option in the signature on purpose. A generator that
    /// errors has already been disabled by the host (charter rule 10), and what
    /// this must return is *some* chunk — a hole in the world is worse than a
    /// plain one.
    fn generate(&mut self, pos: ChunkPos, world_seed: u64) -> Chunk;
}

/// A generator that produces nothing but air.
///
/// For a server with no mods. Charter rule 1: content is mods, so an engine
/// with no mods loaded having no terrain is correct rather than broken.
#[derive(Debug, Clone, Copy, Default)]
pub struct Air;

impl ChunkSource for Air {
    fn generate(&mut self, pos: ChunkPos, _world_seed: u64) -> Chunk {
        Chunk::new(pos, MaterialId::AIR)
    }
}

/// Generates chunks by running the loaded mods' `on_generate` callbacks.
///
/// Wraps the script host so `World` never mentions the VM.
pub struct ModGenerator<V: tiamot_core::script::ScriptVm> {
    host: tiamot_core::script::ModHost<V>,
}

impl<V: tiamot_core::script::ScriptVm> ModGenerator<V> {
    /// Wraps a loaded, frozen mod host.
    pub const fn new(host: tiamot_core::script::ModHost<V>) -> Self {
        Self { host }
    }

    /// The host, for tick hooks and diagnostics.
    pub const fn host_mut(&mut self) -> &mut tiamot_core::script::ModHost<V> {
        &mut self.host
    }
}

impl<V: tiamot_core::script::ScriptVm> ChunkSource for ModGenerator<V> {
    fn generate(&mut self, pos: ChunkPos, world_seed: u64) -> Chunk {
        match self.host.generate_chunk(world_seed, pos, MaterialId::AIR) {
            Ok(chunk) => chunk,
            Err(err) => {
                // The host has already disabled the offending mod (charter
                // rule 10). Air is the honest result: the mods that would have
                // filled this chunk are gone, so it is empty rather than
                // wrong — and the player sees a hole they can report, not
                // terrain that quietly differs from everyone else's.
                warn!(?pos, "chunk generation failed, falling back to air: {err}");
                Chunk::new(pos, MaterialId::AIR)
            }
        }
    }
}

/// What the simulation generates chunks with.
///
/// An enum rather than a boxed [`ChunkSource`]: the simulation also needs to
/// run the mods' tick hooks, and a trait object would have erased the host.
/// Adding `tick` to `ChunkSource` was the alternative, and it would have made
/// "produce a chunk" and "run mod callbacks" the same interface for no reason
/// beyond convenience here.
pub enum Generator {
    /// Terrain from the loaded mods.
    Mods(Box<ModGenerator<tiamot_core::script::MluaVm>>),
    /// No mods loaded, so no terrain.
    Air(Air),
}

impl Generator {
    /// Runs every mod's `on_tick`, returning any that faulted.
    ///
    /// Empty for an [`Air`] generator — there are no mods to tick.
    pub fn tick(&mut self, dt_ticks: u32) -> Vec<(String, tiamot_core::script::ScriptError)> {
        match self {
            Self::Mods(generator) => match generator.host_mut().vm_mut().tick(dt_ticks) {
                Ok(faults) => faults,
                Err(err) => {
                    // A VM-level failure, not a mod fault. Logged rather than
                    // fatal: the world is still coherent and players are still
                    // connected.
                    warn!("script VM failed during tick: {err}");
                    Vec::new()
                }
            },
            Self::Air(_) => Vec::new(),
        }
    }

    /// Runs every mod's per-entity step callback over the entities it owns.
    ///
    /// `owned` groups live entity ids by the mod that spawned them, which the
    /// engine works out because it is the only side that knows — a mod asking
    /// which entities are its own would have to be trusted with the answer.
    ///
    /// Returns the mods that faulted, exactly as [`Self::tick`] does.
    pub fn entity_step(
        &mut self,
        owned: &std::collections::BTreeMap<String, Vec<u64>>,
        dt_ticks: u32,
    ) -> Vec<(String, tiamot_core::script::ScriptError)> {
        let Self::Mods(generator) = self else {
            return Vec::new();
        };
        let vm = generator.host_mut().vm_mut();
        let mut faults = Vec::new();
        // In the VM's own registration order rather than the map's, so two
        // servers running the same mod set call callbacks in the same order —
        // which is load order, which the resolver already made deterministic.
        for mod_id in vm.entity_steppers() {
            let Some(ids) = owned.get(&mod_id) else {
                continue;
            };
            match vm.entity_step(&mod_id, ids, dt_ticks) {
                Ok(Some(fault)) => faults.push(fault),
                Ok(None) => {}
                Err(err) => warn!("script VM failed during an entity step: {err}"),
            }
        }
        faults
    }

    /// Asks the mods whether a dig may proceed.
    ///
    /// A server with no mods allows everything: charter rule 1 puts the rules
    /// in mods, and no mods means no rules rather than no actions.
    pub fn may_dig(
        &mut self,
        event: &tiamot_core::script::DigEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().dig_complete(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Asks the mods whether a placement may proceed.
    pub fn may_place(
        &mut self,
        event: &tiamot_core::script::PlaceEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().place(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Tells the mods somebody arrived.
    pub fn player_joined(
        &mut self,
        event: &tiamot_core::script::JoinEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().player_join(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Offers one randomly-chosen block to whichever mod asked for it.
    pub fn random_ticked(
        &mut self,
        event: &tiamot_core::script::RandomTickEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().random_tick(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Which materials have a random-tick handler.
    #[must_use]
    pub fn random_tick_materials(&self) -> Vec<tiamot_core::MaterialId> {
        match self {
            Self::Mods(generator) => generator.host.vm().random_tick_materials(),
            Self::Air(_) => Vec::new(),
        }
    }

    /// Tells the mods somebody has gone.
    pub fn player_left(
        &mut self,
        event: &tiamot_core::script::LeaveEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().player_leave(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Tells the mods somebody hit something.
    ///
    /// A mod may refuse it, which is what "the hit did not land" means — and
    /// what the hit DOES if it lands is entirely a mod's business, because the
    /// engine has no damage model (charter rule 1).
    pub fn may_punch(
        &mut self,
        event: &tiamot_core::script::PunchEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().punch(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Tells the mods a player used one of their registered actions.
    ///
    /// Named `did_` rather than `may_` because there is nothing to permit: the
    /// key is already down and no mod can un-press it. The outcome carries only
    /// which mods errored, which is what disables them (charter rule 10).
    pub fn did_action(
        &mut self,
        event: &tiamot_core::script::ActionEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().action(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Asks the mods whether a line of chat may be said.
    ///
    /// Named `may_` rather than `did_`, unlike the dialog and action hooks:
    /// this one is a VETO, and the line does not go out if anybody refuses.
    pub fn may_chat(
        &mut self,
        event: &tiamot_core::script::ChatEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().chat(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Tells the OWNING mod that something happened in its dialog.
    ///
    /// Named `did_` for the same reason as [`Self::did_action`]: the click has
    /// happened and no mod can un-click it. What the click MEANS — whether a
    /// stack moves — is decided after this, against the server's own inventory.
    pub fn did_dialog_event(
        &mut self,
        event: &tiamot_core::script::DialogEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().dialog_event(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }

    /// Tells the mods a flow was blocked.
    ///
    /// Nothing is being asked — the flow already failed — so the outcome
    /// carries only the faults. See `ScriptVm::fluid_flow`.
    pub fn fluid_blocked(
        &mut self,
        event: &tiamot_core::script::FluidFlowEvent,
    ) -> tiamot_core::script::HookOutcome {
        match self {
            Self::Mods(generator) => generator.host_mut().vm_mut().fluid_flow(event),
            Self::Air(_) => tiamot_core::script::HookOutcome::allow(),
        }
    }
}

impl ChunkSource for Generator {
    fn generate(&mut self, pos: ChunkPos, world_seed: u64) -> Chunk {
        match self {
            Self::Mods(generator) => generator.generate(pos, world_seed),
            Self::Air(air) => air.generate(pos, world_seed),
        }
    }
}

/// The live world.
pub struct World {
    db: WorldDb,
    /// The seed every generator is handed. Fixed for the world's lifetime.
    seed: u64,
    /// Chunks currently in memory.
    cache: HashMap<ChunkPos, Chunk>,
    /// Chunks changed since the last save.
    ///
    /// Separate from the cache so a save iterates only what moved. A server
    /// holding ten thousand chunks typically dirties a handful per tick.
    dirty: Vec<ChunkPos>,
    /// Chunks that have entered memory and not yet been reported.
    ///
    /// See [`World::take_arrived`]. A list rather than a set because a chunk
    /// can only arrive once between drains — it is already in the cache the
    /// second time.
    arrived: Vec<ChunkPos>,
}

impl World {
    /// Wraps a database, reading or assigning its seed.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the seed cannot be read or written.
    pub fn open(db: WorldDb, new_seed: u64) -> Result<Self, WorldError> {
        // Read first. A world's seed is fixed at creation: re-rolling it on a
        // later start would generate different terrain beyond what has already
        // been explored, leaving a visible seam through the middle of the map.
        let seed = match db.world_seed()? {
            Some(existing) => existing,
            None => {
                db.set_world_seed(new_seed)?;
                new_seed
            }
        };
        Ok(Self {
            db,
            seed,
            cache: HashMap::new(),
            dirty: Vec::new(),
            arrived: Vec::new(),
        })
    }

    /// The world's generation seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// The underlying database.
    #[must_use]
    pub const fn db(&self) -> &WorldDb {
        &self.db
    }

    /// How many chunks are cached.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    /// How many chunks are waiting to be written.
    #[must_use]
    pub fn dirty(&self) -> usize {
        self.dirty.len()
    }

    /// A chunk if it is already in memory, without generating or loading one.
    ///
    /// The read-only counterpart to [`chunk`](Self::chunk), and the difference
    /// is the whole point: `chunk` takes `&mut self` and a generator because it
    /// will *make* a chunk that has never existed. Player collision must not be
    /// able to do that. A body walking into unexplored terrain would otherwise
    /// generate chunks on the simulation thread at whatever rate it moves,
    /// turning a movement input into unbounded work inside the 50 ms tick.
    ///
    /// Collision treats absence as solid (see [`tiamot_core::phys::Voxels`]),
    /// so the honest failure here is a player standing still at the edge of
    /// what is loaded rather than falling through it.
    /// Translates a WORLD material id back into the RUNTIME one.
    ///
    /// So the tick can hand a mod's hook the id that mod's own
    /// `game.get_block_id` returned. Charter rule 8 makes those different
    /// numbers — world ids are stable across sessions because the database
    /// needs them to be, runtime ids come from registration order — and a hook
    /// reporting the wrong one gives a mod a comparison that works whenever the
    /// two coincide and silently fails when they do not.
    ///
    /// `None` for an id this world has never seen, which the caller should
    /// treat as "leave it alone" rather than as an error.
    #[must_use]
    pub fn runtime_material(&self, world: u16) -> Option<MaterialId> {
        self.db.materials().to_runtime(world).ok()
    }

    /// Reads a player's saved inventory, translated into this session's ids.
    ///
    /// `None` for somebody who has never played here, which is not an error —
    /// it is what a first join looks like, and the caller keeps the fresh
    /// inventory it already built. The count is how many stacks could not be
    /// restored, so the caller can say so out loud.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the row cannot be read or does not decode.
    pub fn load_player_slots(
        &self,
        uuid: &tiamot_core::PlayerUuid,
        template: &inventory::Slots,
    ) -> Result<Option<(inventory::Slots, usize)>, WorldError> {
        let Some(blob) = self.db.load_player(&uuid.to_hex())? else {
            return Ok(None);
        };
        // The version is the first byte, written by `save_player_slots`. A
        // blob shorter than that is a row somebody truncated.
        let Some((&version, rest)) = blob.split_first() else {
            return Err(WorldError::Player {
                player: uuid.to_hex(),
                reason: "the stored row is empty".to_owned(),
            });
        };
        tiamot_core::persist::playerdata::decode(version, rest, template, self.db.materials())
            .map(Some)
            .map_err(|source| WorldError::Player {
                player: uuid.to_hex(),
                reason: source.to_string(),
            })
    }

    /// Writes a player's inventory, in world ids.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the row cannot be written.
    pub fn save_player_slots(
        &self,
        uuid: &tiamot_core::PlayerUuid,
        slots: &inventory::Slots,
    ) -> Result<(), WorldError> {
        let (bytes, dropped) = tiamot_core::persist::playerdata::encode(slots, self.db.materials());
        if dropped > 0 {
            tracing::warn!(
                player = %uuid.to_hex(),
                dropped,
                "some stacks hold a material this world cannot name and were not saved"
            );
        }
        // **The version travels WITH the blob**, in its first byte, rather than
        // in the `version` column: `WorldDb::save_player` takes both, and
        // keeping them together means a row read by anything else still says
        // what shape it is.
        let mut row = Vec::with_capacity(bytes.len() + 1);
        row.push(tiamot_core::persist::playerdata::PLAYER_FORMAT_VERSION);
        row.extend_from_slice(&bytes);
        self.db.save_player(
            &uuid.to_hex(),
            tiamot_core::persist::playerdata::PLAYER_FORMAT_VERSION,
            &row,
        )
    }

    /// Every container the world holds, decoded into views.
    ///
    /// The count is how many stacks could not be restored — a material whose
    /// mod is gone, or a row that no longer fits — so a caller can say so out
    /// loud rather than have a chest quietly come back short.
    ///
    /// `sized` says how many slots a container of that name should have. A
    /// container the mods no longer know about keeps whatever it was stored
    /// with, because forgetting the size would be deciding to throw away rows
    /// of somebody's chest on behalf of a mod that is not there to ask.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the rows cannot be read.
    pub fn load_containers(
        &self,
        sized: &dyn Fn(&str) -> Option<usize>,
    ) -> Result<(Vec<(String, inventory::View)>, usize), WorldError> {
        let mut out = Vec::new();
        let mut dropped = 0;
        for (name, blob) in self.db.load_containers()? {
            let Some((&version, rest)) = blob.split_first() else {
                tracing::warn!(container = %name, "a container row is empty and was skipped");
                continue;
            };
            let stored_slots = sized(&name).unwrap_or(tiamot_core::inventory::MAX_VIEW_SLOTS);
            match tiamot_core::persist::containers::decode(
                version,
                rest,
                &name,
                stored_slots,
                self.db.materials(),
            ) {
                Ok((view, lost)) => {
                    dropped += lost;
                    out.push((name, view));
                }
                Err(err) => {
                    // One unreadable container is not a world that will not
                    // open. Said out loud, and the rest come back.
                    tracing::warn!(container = %name, "could not read a container: {err}");
                }
            }
        }
        Ok((out, dropped))
    }

    /// Writes one container.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the row cannot be written.
    pub fn save_container(&self, name: &str, view: &inventory::View) -> Result<(), WorldError> {
        let (bytes, dropped) = tiamot_core::persist::containers::encode(view, self.db.materials());
        if dropped > 0 {
            tracing::warn!(
                container = %name,
                dropped,
                "some stacks hold a material this world cannot name and were not saved"
            );
        }
        // The version travels with the blob, in its first byte, for the reason
        // a player's does: a row read by anything else still says its shape.
        let mut row = Vec::with_capacity(bytes.len() + 1);
        row.push(tiamot_core::persist::containers::CONTAINER_FORMAT_VERSION);
        row.extend_from_slice(&bytes);
        self.db.save_container(
            name,
            tiamot_core::persist::containers::CONTAINER_FORMAT_VERSION,
            &row,
        )
    }

    /// Forgets a container whose block has been broken.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the row cannot be deleted.
    pub fn delete_container(&self, name: &str) -> Result<(), WorldError> {
        self.db.delete_container(name)
    }

    /// Every resident chunk's position, in a fixed order.
    ///
    /// **Sorted, and that is not tidiness.** The cache is a `HashMap`, whose
    /// iteration order is not even stable run to run on one machine (charter
    /// rule 4). A caller that walks a bounded number of chunks — the random
    /// tick does — would otherwise pick a different set each run, so which
    /// crops grew would depend on the allocator.
    #[expect(
        clippy::disallowed_methods,
        reason = "the keys are SORTED before they leave this function, which is the whole \
                  point of it — the lint's hazard is an unordered iteration reaching a \
                  result, and the only way out of here is ordered"
    )]
    #[must_use]
    pub fn resident_positions(&self) -> Vec<ChunkPos> {
        let mut out: Vec<ChunkPos> = self.cache.keys().copied().collect();
        out.sort_unstable_by_key(|pos| (pos.x, pos.y, pos.z));
        out
    }

    /// What one block is made of, without loading anything.
    ///
    /// `None` where the chunk is not resident, or the block is air or mixed —
    /// a caller asking this wants "is this block one particular material", and
    /// a mixed block is not one. The world id, as the chunk stores it.
    #[must_use]
    pub fn material_at(&self, pos: tiamot_core::BlockPos) -> Option<MaterialId> {
        match self.resident(pos.chunk())?.get_block(pos)? {
            tiamot_core::BlockView::Uniform(material)
            | tiamot_core::BlockView::Partial { material, .. } => {
                (!material.is_air()).then_some(material)
            }
            tiamot_core::BlockView::Mixed(_) => None,
        }
    }

    #[must_use]
    pub fn resident(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.cache.get(&pos)
    }

    /// Puts a chunk back on the arrival list.
    ///
    /// For a caller that drained the list and could not process everything this
    /// tick — lighting caps how many chunks it relights per tick, and what it
    /// does not reach has to come back rather than stay dark for as long as it
    /// remains loaded.
    pub fn defer_arrival(&mut self, pos: ChunkPos) {
        self.arrived.push(pos);
    }

    /// Chunks that have entered memory since this was last called, and clears
    /// the list.
    ///
    /// **In arrival order, which is deterministic**, and that is the point:
    /// scanning the cache for new chunks would mean iterating a `HashMap`,
    /// whose order is per-process random and which charter rule 4 forbids
    /// anything observable from depending on. Recording arrivals as they happen
    /// is also O(new) rather than O(loaded) — a server holding two thousand
    /// chunks does not walk them every tick to find the one that just arrived.
    ///
    /// Lighting is the caller: a chunk with blocks and no light renders black.
    pub fn take_arrived(&mut self) -> Vec<ChunkPos> {
        std::mem::take(&mut self.arrived)
    }

    /// Loads a chunk, generating it if the world has never seen it.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the database read or the decode fails.
    pub fn chunk(
        &mut self,
        pos: ChunkPos,
        source: &mut dyn ChunkSource,
    ) -> Result<&mut Chunk, WorldError> {
        if !self.cache.contains_key(&pos) {
            let chunk = match self.db.load_chunk(pos)? {
                Some(chunk) => chunk,
                None => {
                    // Never visited. Generate it and mark it dirty so it is
                    // written — see the module docs on why a generated chunk is
                    // stored rather than regenerated later.
                    let generated = source.generate(pos, self.seed);
                    if !self.dirty.contains(&pos) {
                        self.dirty.push(pos);
                    }
                    generated
                }
            };
            self.cache.insert(pos, chunk);
            self.arrived.push(pos);
        }
        Ok(self
            .cache
            .get_mut(&pos)
            .expect("just inserted if it was absent"))
    }

    /// Applies one edit, returning the chunk it touched and what it removed.
    ///
    /// The removed stacks are whatever the edit took out of the world, in
    /// units — 27 for a whole block, 1 for a single sub-node. Computed by
    /// diffing the block's cells before and after, so digging, chiselling, and
    /// replacing are all the same operation and none can be got wrong
    /// separately.
    ///
    /// # Errors
    ///
    /// [`EditError`] if the material is unknown or the chunk is unreachable.
    pub fn apply(
        &mut self,
        edit: &Edit,
        source: &mut dyn ChunkSource,
    ) -> Result<(ChunkPos, Vec<Stack>), EditError> {
        let (chunk_pos, material) = match edit {
            Edit::Block { pos, material } => (pos.chunk(), *material),
            Edit::SubNode { pos, material } => (pos.chunk(), *material),
            Edit::Partial { pos, material, .. } => (pos.chunk(), *material),
        };

        // Validate the material BEFORE loading a chunk. A peer spraying edits
        // with nonsense ids would otherwise make the server generate and cache
        // a chunk per message — a cheap way to exhaust memory from a client
        // that never even joined the area.
        //
        // Checked against the world's OWN id map, which is the same table the
        // save goes through. An earlier version compared against a "highest
        // known id" number, which was a fiction: an id below the ceiling but
        // absent from the map passed validation, was applied to the chunk, and
        // then failed silently at save time — the edit appeared to work and
        // vanished on restart.
        let material = MaterialId(material);
        if self.db.materials().to_world(material).is_err() {
            return Err(EditError::UnknownMaterial { id: material.0 });
        }

        let chunk = self
            .chunk(chunk_pos, source)
            .map_err(|err| EditError::Unreachable {
                pos: chunk_pos,
                source: Box::new(err),
            })?;

        // Snapshot the affected block's cells BEFORE the edit. A `BlockView`
        // borrows the chunk, so it cannot outlive the mutation — the 27 cells
        // are copied out instead.
        let block_pos = match edit {
            Edit::Block { pos, .. } => *pos,
            Edit::SubNode { pos, .. } => pos.block(),
            Edit::Partial { pos, .. } => *pos,
        };
        let before: Cells = chunk
            .get_block(block_pos)
            .map_or(EMPTY_CELLS, |view| std::array::from_fn(|i| view.subnode(i)));

        match edit {
            Edit::Block { pos, .. } => {
                chunk
                    .set_block(*pos, BlockValue::Uniform(material))
                    .map_err(|_| EditError::OutOfWorld)?;
            }
            Edit::SubNode { pos, .. } => {
                chunk
                    .set_subnode(*pos, material)
                    .map_err(|_| EditError::OutOfWorld)?;
            }
            Edit::Partial { pos, occupancy, .. } => {
                // A mask with every bit set is a `Uniform` block, not a
                // `Partial` one with 27 cells: the two are the same geometry,
                // and letting the second form exist would make a placed full
                // block and a generated full block store and hash differently
                // for no reason a player could observe.
                let value = if *occupancy == (1 << tiamot_core::UNITS_PER_BLOCK) - 1 {
                    BlockValue::Uniform(material)
                } else {
                    BlockValue::Partial {
                        material,
                        occupancy: *occupancy,
                    }
                };
                chunk
                    .set_block(*pos, value)
                    .map_err(|_| EditError::OutOfWorld)?;
            }
        }

        let after: Cells = chunk
            .get_block(block_pos)
            .map_or(EMPTY_CELLS, |view| std::array::from_fn(|i| view.subnode(i)));
        let removed = inventory::removed_units(BlockView::Mixed(&before), BlockView::Mixed(&after));

        if !self.dirty.contains(&chunk_pos) {
            self.dirty.push(chunk_pos);
        }
        Ok((chunk_pos, removed))
    }

    /// The material filling a block, if it is uniform.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk cannot be reached.
    pub fn block_material(
        &mut self,
        pos: BlockPos,
        source: &mut dyn ChunkSource,
    ) -> Result<MaterialId, WorldError> {
        // Sub-node zero, which for a uniform block is the whole block. A caller
        // that needs the full contents asks for the chunk.
        Ok(self
            .chunk(pos.chunk(), source)?
            .get_block(pos)
            .map_or(MaterialId::AIR, |view| view.subnode(0)))
    }

    /// Reads a block's 27 sub-node cells.
    ///
    /// For callers that need the block's *composition* rather than one cell of
    /// it — how hard it is to break, chiefly, which is a question about the
    /// mixture (see `tiamot_core::dig::hardness`). Cells rather than a
    /// [`BlockView`] because the view borrows the chunk, and the chunk is
    /// behind a cache this method has to let go of.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk cannot be reached.
    pub fn block_cells(
        &mut self,
        pos: BlockPos,
        source: &mut dyn ChunkSource,
    ) -> Result<Cells, WorldError> {
        Ok(self
            .chunk(pos.chunk(), source)?
            .get_block(pos)
            .map_or(EMPTY_CELLS, |view| {
                std::array::from_fn(|index| view.subnode(index))
            }))
    }

    /// Reads one sub-node's material.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk cannot be reached.
    pub fn subnode(
        &mut self,
        pos: SubNodePos,
        source: &mut dyn ChunkSource,
    ) -> Result<MaterialId, WorldError> {
        Ok(self
            .chunk(pos.chunk(), source)?
            .get_subnode(pos)
            .unwrap_or(MaterialId::AIR))
    }

    /// Writes every dirty chunk.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if a write fails. Chunks that failed stay dirty, so the
    /// next save retries rather than dropping the edit.
    pub fn save_dirty(&mut self) -> Result<usize, WorldError> {
        if self.dirty.is_empty() {
            return Ok(0);
        }

        let mut written = 0;
        let mut failed = Vec::new();
        for pos in std::mem::take(&mut self.dirty) {
            let Some(chunk) = self.cache.get(&pos) else {
                // Evicted between being dirtied and being saved. That would be
                // a lost edit, so it is a bug rather than a condition — but
                // dropping it silently is worse than saying so.
                warn!(?pos, "a dirty chunk was evicted before it could be saved");
                continue;
            };
            match self.db.save_chunk(pos, chunk) {
                Ok(()) => written += 1,
                Err(err) => {
                    warn!(?pos, "could not save chunk: {err}");
                    failed.push(pos);
                }
            }
        }
        // Keep the failures dirty so the next save tries again.
        self.dirty = failed;
        Ok(written)
    }

    /// Reads a chunk's stored fluid, if it has any.
    ///
    /// The load half of fluid persistence. Its counterpart is
    /// [`crate::fluid::Fluidics::chunk_loaded`], which queues everything the
    /// layer holds so milk saved mid-flow carries on flowing.
    ///
    /// **Not folded into [`World::chunk`]**, which would have been the obvious
    /// place: the fluid lives in `Fluidics`, and a `World` that wrote into it
    /// would need to hold its lock inside a chunk load. The tick joins them
    /// instead, in the same place it hands new chunks to the lighting.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the read or the decode fails.
    pub fn load_fluid(&self, pos: ChunkPos) -> Result<Option<FluidLayer>, WorldError> {
        self.db.load_chunk_fluid(pos)
    }

    /// Writes fluid layers for chunks whose milk has changed.
    ///
    /// A layer that has drained to empty is passed in as empty and its row is
    /// deleted, which is what stops a pond somebody emptied from coming back
    /// the next time its chunk loads.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the write fails. The batch is one transaction, so a
    /// failure changes nothing and the caller can retry.
    pub fn save_fluid<'a>(
        &mut self,
        layers: impl IntoIterator<Item = (ChunkPos, &'a FluidLayer)>,
    ) -> Result<usize, WorldError> {
        self.db.save_chunk_fluid_batch(layers)
    }

    /// Reads the entities anchored to a chunk.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable entity.
    pub fn load_entities(
        &self,
        pos: ChunkPos,
    ) -> Result<Vec<tiamot_core::ent::Entity>, WorldError> {
        self.db.load_chunk_entities(pos)
    }

    /// Replaces the entities anchored to each chunk given.
    ///
    /// A chunk with an empty slice has its rows deleted, which is how the last
    /// mob to leave stops coming back.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable entity.
    pub fn save_entities<'a>(
        &mut self,
        chunks: impl IntoIterator<Item = (ChunkPos, &'a [tiamot_core::ent::Entity])>,
    ) -> Result<usize, WorldError> {
        let mut written = 0;
        for (pos, entities) in chunks {
            self.db.save_chunk_entities(pos, entities)?;
            written += entities.len();
        }
        Ok(written)
    }

    /// Everything one mod has stored.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an undecodable value.
    pub fn load_mod_storage(&self, mod_id: &str) -> Result<tiamot_core::storage::Bag, WorldError> {
        self.db.load_mod_storage(mod_id)
    }

    /// Replaces everything one mod has stored.
    ///
    /// # Errors
    ///
    /// [`WorldError`] on a SQL failure or an unencodable value.
    pub fn save_mod_storage(
        &self,
        mod_id: &str,
        bag: &tiamot_core::storage::Bag,
    ) -> Result<(), WorldError> {
        self.db.save_mod_storage(mod_id, bag)
    }

    /// Flushes and closes the database.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the final write fails.
    pub fn close(mut self) -> Result<(), WorldError> {
        self.save_dirty()?;
        self.db.close()
    }
}

/// Lets the physics collide against the world without being able to change it.
///
/// Note which trait this is: [`tiamot_core::phys::ChunkLookup`] reads resident
/// chunks, and is not the [`ChunkSource`] above, which generates them. Both
/// exist here and they mean opposite things — see [`World::resident`].
impl tiamot_core::phys::ChunkLookup for World {
    fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.resident(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The material ids the test world registers, in order.
    const TEST_MATERIALS: [&str; 4] = ["test:stone", "test:dirt", "test:wood", "test:glass"];

    /// A generator that fills every chunk below y=0 solid and leaves the rest
    /// air — the shape of a real generator, without a script VM.
    struct Flat {
        material: MaterialId,
        /// Every position it was asked to generate, in order.
        generated: Vec<ChunkPos>,
    }

    impl Flat {
        const fn new(material: MaterialId) -> Self {
            Self {
                material,
                generated: Vec::new(),
            }
        }
    }

    impl ChunkSource for Flat {
        fn generate(&mut self, pos: ChunkPos, _world_seed: u64) -> Chunk {
            self.generated.push(pos);
            if pos.y < 0 {
                Chunk::new(pos, self.material)
            } else {
                Chunk::new(pos, MaterialId::AIR)
            }
        }
    }

    fn world(name: &str) -> (World, Vec<MaterialId>) {
        let dir = std::env::temp_dir().join("tiamot-world-tests");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(format!("{name}.sqlite"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let mut registry = tiamot_core::Registry::new();
        let ids = TEST_MATERIALS
            .iter()
            .map(|name| registry.register(name).expect("register"))
            .collect();
        let db = WorldDb::open(&path, &mut registry).expect("open");
        (World::open(db, 12345).expect("open world"), ids)
    }

    /// Reopens the same world file, as a restart would.
    ///
    /// Deliberately does NOT wipe, unlike [`world`]: the point is that the
    /// second `World` finds what the first one left.
    fn reopen(name: &str) -> World {
        let path = std::env::temp_dir()
            .join("tiamot-world-tests")
            .join(format!("{name}.sqlite"));
        let mut registry = tiamot_core::Registry::new();
        for material in TEST_MATERIALS {
            registry.register(material).expect("register");
        }
        let db = WorldDb::open(&path, &mut registry).expect("reopen");
        World::open(db, 12345).expect("reopen world")
    }

    #[test]
    fn a_pond_written_by_one_world_is_read_by_the_next() {
        // **The round trip, across a real file and two `World`s.** Fluid is not
        // derived state, so both halves of this are load-bearing, and each half
        // passes its own unit tests while the pair is broken — a save that used
        // one domain and a load that used another, say. Only running them
        // against each other catches that.
        use crate::fluid::Fluidics;
        use tiamot_core::fluid::{Fluid, FluidId, Fluids, MAX_VOLUME};

        let milk = FluidId(1);
        let pond = BlockPos::new(20, 5, -9);
        let chunk = pond.chunk();

        {
            let (mut world, _) = world("fluid-round-trip");
            let mut fluidics = Fluidics::new(Fluids::new());
            fluidics.set(pond, Fluid::new(milk, MAX_VOLUME));

            let dirty = fluidics.take_dirty();
            assert_eq!(dirty.len(), 1, "the pour did not mark its chunk");
            assert_eq!(
                world
                    .save_fluid(dirty.iter().map(|(pos, layer)| (*pos, layer)))
                    .expect("save"),
                1
            );
            world.close().expect("close");
        }

        // A second world on the same file, with a fresh store that has never
        // heard of the pond.
        let world = reopen("fluid-round-trip");
        let layer = world
            .load_fluid(chunk)
            .expect("read")
            .expect("the pond was not written");

        let mut fluidics = Fluidics::new(Fluids::new());
        assert!(!fluidics.knows(chunk));
        fluidics.chunk_loaded(chunk, layer);

        assert!(fluidics.knows(chunk));
        assert!(
            fluidics.at(pond).volume() == MAX_VOLUME,
            "the pond came back as {:?} rather than a source, so it would drain \
             away a few ticks after the world loaded",
            fluidics.at(pond)
        );
        assert!(
            !fluidics.is_settled(),
            "a reloaded pond must be queued, or milk saved mid-flow stops where it was"
        );
    }

    #[test]
    fn a_pond_that_drained_is_gone_after_a_restart() {
        // The other direction, and the one that fails silently: a chunk whose
        // layer emptied is dropped from memory, so a dirty list that followed
        // the layer would never write the removal and the milk would come back.
        use crate::fluid::Fluidics;
        use tiamot_core::fluid::{Fluid, FluidId, Fluids, MAX_VOLUME};

        let pond = BlockPos::new(3, 3, 3);
        let chunk = pond.chunk();

        {
            let (mut world, _) = world("fluid-drain-round-trip");
            let mut fluidics = Fluidics::new(Fluids::new());

            fluidics.set(pond, Fluid::new(FluidId(1), MAX_VOLUME));
            let dirty = fluidics.take_dirty();
            world
                .save_fluid(dirty.iter().map(|(pos, layer)| (*pos, layer)))
                .expect("save the pond");

            fluidics.set(pond, Fluid::EMPTY);
            let dirty = fluidics.take_dirty();
            assert_eq!(dirty.len(), 1, "the drain was not queued for writing");
            world
                .save_fluid(dirty.iter().map(|(pos, layer)| (*pos, layer)))
                .expect("save the drain");
            world.close().expect("close");
        }

        assert!(
            reopen("fluid-drain-round-trip")
                .load_fluid(chunk)
                .expect("read")
                .is_none(),
            "a pond that was emptied came back after a restart"
        );
    }

    #[test]
    fn a_missing_chunk_is_generated_rather_than_left_empty() {
        // Returning air for an unvisited chunk would drop players through the
        // floor at the edge of explored space.
        let (mut world, ids) = world("generated");
        let mut flat = Flat::new(ids[0]);

        let material = world
            .block_material(BlockPos::new(0, -5, 0), &mut flat)
            .expect("read");

        assert_eq!(material, ids[0], "the generator should have filled this");
        assert_eq!(flat.generated, vec![BlockPos::new(0, -5, 0).chunk()]);
    }

    #[test]
    fn a_generated_chunk_is_persisted_rather_than_regenerated() {
        // Worldgen is deterministic, so regenerating would be byte-identical
        // today — but a mod update or a noise change would silently rewrite
        // terrain a player has already built next to. Freezing it on first
        // generation is what makes explored land stable.
        let (mut world, ids) = world("generated-persist");
        let mut flat = Flat::new(ids[0]);
        let pos = BlockPos::new(0, -5, 0);

        world.block_material(pos, &mut flat).expect("read");
        assert_eq!(
            world.dirty(),
            1,
            "a generated chunk must be marked for saving"
        );
        assert_eq!(world.save_dirty().expect("save"), 1);

        // Drop the cache and read again with a generator that would produce
        // something DIFFERENT. The stored chunk must win.
        world.cache.clear();
        let mut changed = Flat::new(ids[1]);
        assert_eq!(
            world.block_material(pos, &mut changed).expect("read"),
            ids[0],
            "the stored chunk must win over a changed generator"
        );
        assert!(
            changed.generated.is_empty(),
            "a stored chunk must not be regenerated at all"
        );
    }

    #[test]
    fn a_chunk_is_generated_once_not_once_per_access() {
        let (mut world, ids) = world("generate-once");
        let mut flat = Flat::new(ids[0]);
        let pos = BlockPos::new(1, -1, 1);

        for _ in 0..5 {
            world.block_material(pos, &mut flat).expect("read");
        }
        assert_eq!(
            flat.generated.len(),
            1,
            "generated {} times",
            flat.generated.len()
        );
    }

    #[test]
    fn the_seed_is_fixed_at_creation() {
        // Re-rolling a seed on a later start would change terrain beyond the
        // explored edge, leaving a visible seam through the middle of the map.
        let dir = std::env::temp_dir().join("tiamot-world-tests");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("seed-fixed.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }

        let mut registry = tiamot_core::Registry::new();
        let db = WorldDb::open(&path, &mut registry).expect("open");
        let world = World::open(db, 999).expect("open world");
        assert_eq!(world.seed(), 999);
        world.close().expect("close");

        let mut registry = tiamot_core::Registry::new();
        let db = WorldDb::open(&path, &mut registry).expect("reopen");
        let world = World::open(db, 4242).expect("open world");
        assert_eq!(
            world.seed(),
            999,
            "an existing world must keep the seed it was created with"
        );
    }

    #[test]
    fn the_air_generator_produces_air() {
        // A server with no mods has no terrain, which is correct rather than
        // broken: the engine is mechanisms and the content is mods.
        let (mut world, _) = world("air");
        let mut air = Air;
        assert_eq!(
            world
                .block_material(BlockPos::new(0, -100, 0), &mut air)
                .expect("read"),
            MaterialId::AIR
        );
    }

    #[test]
    fn an_edit_marks_its_chunk_dirty() {
        let (mut world, ids) = world("dirty");
        let mut air = Air;
        let pos = BlockPos::new(3, 4, 5);

        world
            .apply(
                &Edit::Block {
                    pos,
                    material: ids[0].0,
                },
                &mut air,
            )
            .expect("apply");

        assert_eq!(world.dirty(), 1);
        assert_eq!(world.block_material(pos, &mut air).expect("read"), ids[0]);
    }

    #[test]
    fn two_edits_to_one_chunk_dirty_it_once() {
        // Otherwise a busy chunk is written once per edit rather than once per
        // save, and a player chiselling gets a write amplification of 27.
        let (mut world, ids) = world("dirty-once");
        let mut air = Air;
        for x in 0..5 {
            world
                .apply(
                    &Edit::Block {
                        pos: BlockPos::new(x, 0, 0),
                        material: ids[0].0,
                    },
                    &mut air,
                )
                .expect("apply");
        }
        assert_eq!(world.dirty(), 1, "all five blocks are in one chunk");
    }

    #[test]
    fn an_unknown_material_is_refused_without_loading_a_chunk() {
        // The cheap-memory-exhaustion case: a peer spraying edits with nonsense
        // ids must not make the server generate a chunk per message.
        let (mut world, _) = world("unknown-material");
        let mut flat = Flat::new(MaterialId::AIR);
        let before = world.cached();

        let err = world
            .apply(
                &Edit::Block {
                    pos: BlockPos::new(9999, 0, 9999),
                    material: 60_000,
                },
                &mut flat,
            )
            .expect_err("must refuse");

        assert!(matches!(err, EditError::UnknownMaterial { id: 60_000 }));
        assert_eq!(
            world.cached(),
            before,
            "a refused edit must not load a chunk"
        );
        assert!(
            flat.generated.is_empty(),
            "nor make the generator run — that is the expensive half"
        );
        assert_eq!(world.dirty(), 0);
    }

    #[test]
    fn a_material_the_world_does_not_map_is_refused_rather_than_failing_at_save() {
        // The bug this check exists for. An id that passes validation but has
        // no row in the world's id map applies cleanly to the chunk and then
        // fails at save time — the edit looks like it worked and disappears on
        // restart.
        let (mut world, ids) = world("unmapped");
        let mut air = Air;
        let unregistered = MaterialId(ids.last().expect("ids").0 + 1);

        let err = world
            .apply(
                &Edit::Block {
                    pos: BlockPos::new(0, 0, 0),
                    material: unregistered.0,
                },
                &mut air,
            )
            .expect_err("an unmapped material must be refused up front");
        assert!(matches!(err, EditError::UnknownMaterial { .. }), "{err}");

        for id in &ids {
            world
                .apply(
                    &Edit::Block {
                        pos: BlockPos::new(0, 0, 0),
                        material: id.0,
                    },
                    &mut air,
                )
                .expect("accepted");
            assert_eq!(
                world.save_dirty().expect("save"),
                1,
                "an accepted edit must reach the database"
            );
        }
    }

    #[test]
    fn a_saved_edit_survives_a_reload() {
        let (mut world, ids) = world("persist");
        let mut air = Air;
        let pos = BlockPos::new(1, 2, 3);
        world
            .apply(
                &Edit::Block {
                    pos,
                    material: ids[1].0,
                },
                &mut air,
            )
            .expect("apply");

        assert_eq!(world.save_dirty().expect("save"), 1);
        assert_eq!(world.dirty(), 0, "a save clears the dirty set");

        world.cache.clear();
        assert_eq!(
            world.block_material(pos, &mut air).expect("read"),
            ids[1],
            "the edit must have reached the database"
        );
    }

    #[test]
    fn an_edit_survives_over_the_generated_terrain_under_it() {
        // The interaction worth checking: an edit inside a generated chunk must
        // beat the generator on reload, or a player's building would revert to
        // hillside.
        let (mut world, ids) = world("edit-over-terrain");
        let mut flat = Flat::new(ids[0]);
        let pos = BlockPos::new(2, -3, 2);

        world
            .apply(
                &Edit::Block {
                    pos,
                    material: ids[3].0,
                },
                &mut flat,
            )
            .expect("apply");
        world.save_dirty().expect("save");
        world.cache.clear();

        assert_eq!(
            world.block_material(pos, &mut flat).expect("read"),
            ids[3],
            "the edit must survive"
        );
        // And its neighbour is still the generated material.
        assert_eq!(
            world
                .block_material(BlockPos::new(3, -3, 2), &mut flat)
                .expect("read"),
            ids[0],
            "the surrounding terrain must be intact"
        );
    }

    #[test]
    fn a_subnode_edit_round_trips() {
        // Sub-node resolution is the whole point of the engine; an edit path
        // that only handled whole blocks would be half a feature.
        let (mut world, ids) = world("subnode");
        let mut air = Air;
        let pos = SubNodePos::new(4, 5, 6);
        world
            .apply(
                &Edit::SubNode {
                    pos,
                    material: ids[2].0,
                },
                &mut air,
            )
            .expect("apply");
        world.save_dirty().expect("save");
        world.cache.clear();

        assert_eq!(
            world.subnode(pos, &mut air).expect("read"),
            ids[2],
            "a sub-node edit must survive a save and reload"
        );
    }

    #[test]
    fn a_subnode_edit_leaves_its_neighbours_alone() {
        // One cell of 27. If this wrote the whole block the engine would have
        // no sub-block resolution at all, and every test above would still
        // pass.
        let (mut world, ids) = world("subnode-neighbours");
        let mut air = Air;
        let target = SubNodePos::new(3, 3, 3);
        world
            .apply(
                &Edit::Block {
                    pos: target.block(),
                    material: ids[0].0,
                },
                &mut air,
            )
            .expect("fill the block");
        world
            .apply(
                &Edit::SubNode {
                    pos: target,
                    material: ids[3].0,
                },
                &mut air,
            )
            .expect("chisel one cell");
        world.save_dirty().expect("save");
        world.cache.clear();

        assert_eq!(world.subnode(target, &mut air).expect("read"), ids[3]);
        assert_eq!(
            world
                .subnode(SubNodePos::new(target.x + 1, target.y, target.z), &mut air)
                .expect("read"),
            ids[0],
            "the neighbouring cell must be untouched"
        );
    }

    #[test]
    fn saving_nothing_is_free() {
        let (mut world, _) = world("nothing");
        assert_eq!(world.save_dirty().expect("save"), 0);
    }
}
