// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's fluid: where it is kept, and what runs it.
//!
//! # Not derived, so it has to be saved
//!
//! Light is derived state — thrown away on shutdown and recomputed on load,
//! because recomputing it gives the same answer. **Fluid is not.** A pond is a
//! record of what somebody poured, and there is no function from terrain back to
//! "there was milk here", so a fluid layer has to be written with its chunk or
//! every pond in the world empties on restart.
//!
//! The round trip runs through a `chunk_fluid` table keyed by chunk position —
//! its own table rather than a column on `chunks` or a field in the chunk blob,
//! because [`FluidLayer`]'s whole design is that a dry chunk costs nothing, and
//! only a separate table keeps that true on disk. A chunk arriving reads its
//! row and hands it to [`Fluidics::chunk_loaded`], which queues everything in
//! it so milk saved mid-flow carries on flowing; a chunk whose milk changed
//! goes on the dirty list and is written on the same debounce as terrain.
//!
//! **A chunk that drained is written too, as empty, which deletes its row.**
//! The layer is dropped from memory when it empties, so a dirty list that
//! followed the layer would never hear about the drain — and the pond would
//! come back the next time the chunk loaded.
//!
//! # Two clocks
//!
//! The simulation ticks at 20 Hz; fluid ticks at half that. Nobody can see the
//! difference — milk moving ten times a second already looks continuous — and
//! it halves the cost of the one system in the engine whose work is
//! proportional to how much of the world is *moving* rather than to how much of
//! it is loaded.
//!
//! # The cap is not a nicety
//!
//! Charter rule 18: a 50 ms tick shared by fifty players. A hundred springs
//! opened at once would otherwise spread across a chunk in one tick and take the
//! whole budget with them, so the solver visits at most [`VISITS_PER_TICK`]
//! blocks and carries the rest. The carry is what makes that safe — work is
//! deferred, never dropped, so a spring field finishes late rather than wrong.

use std::collections::{BTreeSet, HashMap};

use tiamot_core::coords::{BlockPos, ChunkPos};
use tiamot_core::fluid::{
    Absorbency, Absorbs, Flow, Fluid, FluidLayer, Fluids, Neighbourhood, Sinks, Solver, Tuning,
};

use crate::world::World;

/// Simulation ticks between fluid updates.
///
/// Two, so fluid runs at 10 Hz against the simulation's 20. The task names this
/// and the reason is cost rather than feel.
pub const TICKS_PER_FLUID_TICK: u64 = 2;

/// The most blocks one fluid tick will visit.
///
/// **Set from a measurement, and the first guess was four times too high.**
/// Charter rule 18 gives all of simulation 50 ms shared by fifty players.
/// `cargo bench -p tiamot-core --bench fluid` measured the hundred-spring field
/// at **15.5 ms for one capped tick at 4,096 visits — 31% of a whole tick, for
/// one system, while nobody is even looking at it**. At 512 the same tick
/// measures **2.4 ms, 4.8%**, which is a share fluid can defend.
///
/// The spread simply takes more ticks, which is the right trade twice over: the
/// carry means work is deferred and never dropped, and a fluid that takes a
/// moment to find its level is what a fluid is supposed to look like.
pub const VISITS_PER_TICK: usize = 512;

/// Builds an absorbency table from what the mods registered.
///
/// Keyed by WORLD material id, like emissions and hardness: a world that has
/// seen a different mod set numbers its materials differently, and a table of
/// this session's runtime ids would name every block one number out (charter
/// rule 8). The successor goes through the same lookup, so a saturation chain
/// survives a world being opened by a client with other mods installed.
///
/// A `becomes` naming a block nobody registered is dropped to `None` rather
/// than refused: the chain simply ends there, which is what a mod that removed
/// the last link of its own chain should get.
#[must_use]
pub fn absorbency_from_rules(
    rules: &[tiamot_core::script::BlockRules],
    id_of: impl Fn(&str) -> Option<tiamot_core::MaterialId>,
) -> Absorbency {
    Absorbency::new(rules.iter().filter_map(|rule| {
        let (rate, becomes) = &rule.absorbs;
        if *rate == 0 {
            return None;
        }
        let becomes = becomes.as_deref().and_then(&id_of);
        Some((
            id_of(&rule.block)?,
            Absorbs {
                rate: *rate,
                becomes,
            },
        ))
    }))
}

/// A handle on the fluid store, for the mod API.
///
/// The same arrangement `light::Shared` uses, and behind a lock for the same
/// reason: `game.set_fluid` runs inside a tick, on the simulation thread, and
/// cannot borrow what the tick is holding. Uncontended in practice — both sides
/// are that one thread — and never held across a mod callback, which is the
/// arrangement that would deadlock.
pub struct Shared {
    fluidics: std::sync::Arc<std::sync::RwLock<Ponds>>,
}

/// One [`Fluidics`] per domain, made on first use.
///
/// **Milk is per-space, like everything else about a place.** A layer is keyed
/// by chunk position, and the same position is different terrain in another
/// domain — so one store shared between them would pour a ship's milk into the
/// overworld at those coordinates, and let a pond flow through a hull it cannot
/// see.
///
/// A store per domain rather than a domain inside the store, for the reason
/// `light::Lights` gives: the solver walks neighbouring layers constantly and
/// wants a map it can index by position alone, and nothing about fluid is
/// cross-domain — milk does not run between worlds.
#[derive(Debug)]
pub struct Ponds {
    fluids: Fluids,
    absorbency: Absorbency,
    domains: std::collections::BTreeMap<String, Fluidics>,
}

impl Ponds {
    /// A set of stores for a world whose mods registered these fluids.
    #[must_use]
    pub fn new(fluids: Fluids, absorbency: Absorbency) -> Self {
        Self {
            fluids,
            absorbency,
            domains: std::collections::BTreeMap::new(),
        }
    }

    /// One domain's fluid, created on first use.
    pub fn of(&mut self, domain: &str) -> &mut Fluidics {
        self.domains.entry(domain.to_owned()).or_insert_with(|| {
            let mut fluidics = Fluidics::new(self.fluids.clone());
            fluidics.set_absorbency(self.absorbency.clone());
            fluidics
        })
    }

    /// One domain's fluid, if anything has poured there.
    #[must_use]
    pub fn get(&self, domain: &str) -> Option<&Fluidics> {
        self.domains.get(domain)
    }

    /// What the mods registered. The same table for every domain: a fluid is a
    /// kind of thing, not a thing in a place.
    #[must_use]
    pub const fn fluids(&self) -> &Fluids {
        &self.fluids
    }

    /// What a fluid is called. The same answer for every domain.
    #[must_use]
    pub fn name_of(&self, id: tiamot_core::fluid::FluidId) -> Option<&str> {
        self.fluids
            .iter_registered()
            .find(|(registered, _)| *registered == id)
            .map(|(_, entry)| entry.name.as_str())
    }

    /// Everything to save, as `(domain, chunk, layer)`.
    ///
    /// **A row is `(domain, chunk)`**, the same as a chunk's and an entity's,
    /// so a pond in a ship is written under the ship and not into the overworld
    /// at those coordinates.
    pub fn take_dirty(&mut self) -> Vec<(String, ChunkPos, FluidLayer)> {
        let mut out = Vec::new();
        for (domain, fluid) in &mut self.domains {
            for (pos, layer) in fluid.take_dirty() {
                out.push((domain.clone(), pos, layer));
            }
        }
        out
    }

    /// Every domain anything has poured in.
    #[must_use]
    pub fn wet_domains(&self) -> Vec<&str> {
        self.domains.keys().map(String::as_str).collect()
    }
}

impl Shared {
    /// Wraps a store the simulation thread owns.
    #[must_use]
    pub const fn new(fluidics: std::sync::Arc<std::sync::RwLock<Ponds>>) -> Self {
        Self { fluidics }
    }
}

impl tiamot_core::fluid::Access for Shared {
    fn fluid_at(&self, pos: BlockPos) -> Fluid {
        // A poisoned lock means the simulation thread panicked, in which case
        // there is no world to have milk in. Empty is the honest answer, and
        // panicking inside a mod callback would blame the mod.
        // **The overworld's**, because `game.get_fluid(position)` names a
        // position and no domain — a mod asking about a place in a ship has no
        // way to say which ship. Widening this needs the API to carry a domain,
        // which is a change to what mods write and not a change here.
        self.fluidics.read().map_or(Fluid::EMPTY, |ponds| {
            ponds
                .get(tiamot_core::domain::OVERWORLD)
                .map_or(Fluid::EMPTY, |fluidics| fluidics.at(pos))
        })
    }

    fn set_fluid_at(&self, pos: BlockPos, value: Fluid) -> bool {
        self.fluidics
            .write()
            .is_ok_and(|mut ponds| ponds.of(tiamot_core::domain::OVERWORLD).set(pos, value))
    }

    fn fluid_id(&self, name: &str) -> Option<tiamot_core::fluid::FluidId> {
        self.fluidics
            .read()
            .ok()
            .and_then(|ponds| ponds.fluids().id_of(name))
    }
}

/// A mod's runtime block writes, queued for the tick.
///
/// Rides the same queue an operator's edits use — see `Shared::queue_seed` —
/// which is what lets a mod change the world without a lock on it and without
/// the tick having to be re-entrant. The edit lands on the next tick.
pub struct Edits {
    shared: std::sync::Arc<crate::transport::Shared>,
    /// Block name to WORLD material id, resolved once at startup.
    ///
    /// Names rather than numbers at the API boundary — charter rule 8 makes
    /// runtime and world ids different numbers, and handing a mod either one
    /// gives it a value it cannot safely compare or store. This is the table
    /// that keeps that decision from costing a lookup per call.
    by_name: std::collections::BTreeMap<String, u16>,
}

impl Edits {
    /// Wraps the shared queue with a name table.
    #[must_use]
    pub const fn new(
        shared: std::sync::Arc<crate::transport::Shared>,
        by_name: std::collections::BTreeMap<String, u16>,
    ) -> Self {
        Self { shared, by_name }
    }
}

impl tiamot_core::script::WorldEdit for Edits {
    fn set_block(&self, pos: BlockPos, block: &str) -> bool {
        let Some(&material) = self.by_name.get(block) else {
            tracing::debug!(block, "a mod asked to place a block nothing registered");
            return false;
        };
        self.shared
            .queue_seed(tiamot_core::proto::Edit::Block { pos, material })
    }
}

/// Every loaded chunk's fluid, and what the mods registered.
#[derive(Debug, Default)]
pub struct Fluidics {
    layers: HashMap<ChunkPos, FluidLayer>,
    /// Chunks whose fluid has changed since the last save.
    ///
    /// A `BTreeSet` rather than a `HashSet` for the reason the active set is
    /// one: `HashMap` iteration order is per-process random, and while the
    /// order rows are written in is not a simulation result, the habit is what
    /// stops the next person who reads this set inside the tick from
    /// introducing one.
    ///
    /// **A drained chunk stays in here after its layer is dropped**, which is
    /// the case that matters: the save turns "dirty with no layer" into an
    /// empty layer, and an empty layer deletes the row. Without that a pond
    /// somebody emptied would come straight back the next time its chunk
    /// loaded.
    dirty: std::collections::BTreeSet<ChunkPos>,
    /// Chunks whose fluid has been read from the database this session.
    ///
    /// Separate from `layers` because a chunk that was read and found dry
    /// belongs here and not there — see [`Fluidics::knows`] for what goes wrong
    /// without the distinction.
    loaded: std::collections::BTreeSet<ChunkPos>,
    fluids: Fluids,
    absorbency: Absorbency,
    solver: Solver,
    /// Writes a MOD made, waiting to go out with the next tick's changes.
    ///
    /// **A mod's `game.set_fluid` is a change nobody was told about**, and the
    /// old model hid that: a source fed its neighbours or renewed itself, so
    /// the solver always produced something on the next tick and the touched
    /// chunk went out with it. A conserved pour into a sealed block moves
    /// nothing at all, and the milk existed only on the server — visible to
    /// `game.get_fluid` and to nobody looking at it.
    written: Vec<Flow>,
}

impl Fluidics {
    /// A store for a world whose mods registered these fluids.
    #[must_use]
    pub fn new(fluids: Fluids) -> Self {
        Self {
            layers: HashMap::new(),
            dirty: std::collections::BTreeSet::new(),
            loaded: std::collections::BTreeSet::new(),
            fluids,
            absorbency: Absorbency::default(),
            solver: Solver::new(),
            written: Vec::new(),
        }
    }

    /// Takes the chunks whose fluid needs writing, and what to write.
    ///
    /// Yields an empty layer for a chunk that drained, so the caller can hand
    /// the whole sequence to [`crate::world::World::save_fluid`] and let the
    /// removals and the writes land in one transaction.
    pub fn take_dirty(&mut self) -> Vec<(ChunkPos, FluidLayer)> {
        std::mem::take(&mut self.dirty)
            .into_iter()
            .map(|pos| {
                let layer = self.layers.get(&pos).cloned().unwrap_or_default();
                (pos, layer)
            })
            .collect()
    }

    /// How many chunks are waiting to be written.
    #[must_use]
    pub fn dirty(&self) -> usize {
        self.dirty.len()
    }

    /// Puts a chunk back on the list of things to write.
    ///
    /// For a save that failed: the batch is one transaction, so nothing landed,
    /// and forgetting what it was trying to write would lose the pond at the
    /// next unload without anything going wrong visibly.
    pub fn mark_dirty(&mut self, pos: ChunkPos) {
        self.dirty.insert(pos);
    }

    /// Records which materials drink, and what they turn into.
    ///
    /// Set after the registries freeze, because the successor material has to
    /// be resolvable — `becomes = "damp_dirt"` is a string until there is a
    /// registry to look it up in (charter rule 8).
    pub fn set_absorbency(&mut self, absorbency: Absorbency) {
        self.absorbency = absorbency;
    }

    /// What one material does to fluid touching it.
    #[must_use]
    pub fn absorbs(&self, material: tiamot_core::MaterialId) -> Option<Absorbs> {
        self.absorbency.get(material)
    }

    /// The same for a whole block, which is what the tick actually holds.
    #[must_use]
    pub fn absorbs_block(&self, block: &tiamot_core::block::BlockView<'_>) -> Option<Absorbs> {
        self.absorbency.block(block)
    }

    /// Takes what the solver destroyed since this was last called.
    ///
    /// The caller applies the material swaps [`Sinks::absorbed`] describes: this
    /// module knows a rate and a successor, and turning "three cells went into
    /// this block" into a terrain edit needs the world, which is the tick
    /// thread's rather than this lock's.
    pub fn take_sinks(&mut self) -> Sinks {
        self.solver.take_sinks()
    }

    /// What the mods registered.
    #[must_use]
    pub const fn fluids(&self) -> &Fluids {
        &self.fluids
    }

    /// What a block holds, or nothing if its chunk is not loaded.
    #[must_use]
    pub fn at(&self, pos: BlockPos) -> Fluid {
        self.layers
            .get(&pos.chunk())
            .map_or(Fluid::EMPTY, |layer| layer.get(pos.local()))
    }

    /// A chunk's fluid, for sending to a client or writing to the database.
    #[must_use]
    pub fn layer(&self, pos: ChunkPos) -> Option<&FluidLayer> {
        self.layers.get(&pos)
    }

    /// Whether any chunk holds fluid.
    ///
    /// The settled-world assertion's other half: no layers and no active blocks
    /// is a world where fluid costs nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// How many blocks are waiting to be looked at.
    #[must_use]
    pub fn active(&self) -> usize {
        self.solver.active()
    }

    /// Whether there is no fluid work outstanding.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.solver.is_settled()
    }

    /// Whether this chunk's fluid has already been read from the database.
    ///
    /// **The guard against loading a chunk twice**, and it is not hypothetical:
    /// the tick's arrival list is shared with the lighting, which defers what it
    /// cannot relight this tick by putting the chunk *back* into that list. A
    /// deferred chunk therefore arrives again a tick later, and a second load
    /// would replace the live layer with whatever was last written to disk —
    /// discarding a pour the player made in between, or resurrecting a pond that
    /// drained.
    ///
    /// Distinct from "holds a layer": a chunk that was read and found dry is
    /// known and has no layer, and asking `layers.contains_key` would send it
    /// back to the database on every arrival.
    #[must_use]
    pub fn knows(&self, pos: ChunkPos) -> bool {
        self.loaded.contains(&pos)
    }

    /// Takes a chunk's fluid as it came out of the database.
    ///
    /// An empty layer is not stored: the map is for chunks that hold something,
    /// so a world with one pond in it has one entry however far anybody walks.
    /// The chunk is still recorded as read — see [`Fluidics::knows`].
    /// Everything in the chunk is queued, because milk saved mid-flow has to
    /// carry on flowing when it comes back.
    pub fn chunk_loaded(&mut self, pos: ChunkPos, layer: FluidLayer) {
        self.loaded.insert(pos);
        if layer.is_empty() {
            return;
        }
        for (index, value) in layer.blocks().enumerate() {
            if !value.is_empty() {
                self.solver.touch(block_at(pos, index));
            }
        }
        self.layers.insert(pos, layer);
    }

    /// Forgets a chunk's fluid.
    ///
    /// Called when a chunk leaves memory, after it has been written back.
    ///
    /// Forgets that it was read as well as what it held, so that a chunk which
    /// comes back later is loaded from the database again rather than treated
    /// as dry.
    pub fn forget(&mut self, pos: ChunkPos) {
        self.layers.remove(&pos);
        self.loaded.remove(&pos);
    }

    /// Records what a block holds and wakes its neighbourhood.
    ///
    /// The one way fluid enters the world from outside the solver: a mod's
    /// `game.set_fluid`, a player pouring, a chunk being edited.
    pub fn set(&mut self, pos: BlockPos, value: Fluid) -> bool {
        let was = self.at(pos);
        let changed = self.write(pos, value);
        if changed {
            // Reported like any other flow, so a pour that settles instantly
            // still reaches the clients that have to draw it.
            self.written.push(Flow {
                pos,
                was,
                now: value,
            });
        }
        self.solver.touch(pos);
        changed
    }

    /// Wakes a block without changing it.
    ///
    /// For a terrain edit: the block itself did not gain or lose fluid, but what
    /// it will accept has changed, and the pond next door needs to find out.
    pub fn touch(&mut self, pos: BlockPos) {
        self.solver.touch(pos);
    }

    /// Runs one fluid tick against `world`.
    ///
    /// Returns every change, for broadcasting and re-meshing. Empty for a
    /// settled world, and it does not even take a lock to find that out.
    pub fn tick(&mut self, domain: &str, world: &World, fluid_tick: u64, seed: u64) -> Vec<Flow> {
        // **Taken on every path**, including the two that do no work. A write a
        // mod made is a change whatever the solver decides afterwards, and
        // dropping it on the viscosity check would lose it for good rather
        // than delaying it.
        let mut changes = std::mem::take(&mut self.written);
        if self.solver.is_settled() {
            return changes;
        }
        // **A fluid's own rate, and until now it was registered and ignored.**
        // `register_fluid{ tick_rate }` was accepted, stored, carried all the
        // way here and consulted by nothing, so every fluid ran at the engine's
        // rate whatever its mod asked for. Reported from the window as milk
        // spreading about three times faster than wanted.
        //
        // One rate for the whole tick rather than per block: the solver's queue
        // does not know which fluid a block holds until it looks, and a queue
        // partitioned by fluid would be four data structures to make a puddle
        // slower. Ships one fluid; the shape is here for when that changes.
        let tuning = self.tuning();
        if !fluid_tick.is_multiple_of(u64::from(tuning.tick_rate.max(1))) {
            return changes;
        }
        // Taken apart so the solver can borrow the store mutably while the world
        // is borrowed immutably, which is the same trick `Lit` plays for light.
        let mut solver = std::mem::take(&mut self.solver);
        let mut view = Wet {
            terrain: world.solid(domain),
            layers: &mut self.layers,
            absorbency: &self.absorbency,
        };
        changes.extend(solver.tick(&mut view, tuning, VISITS_PER_TICK, seed, fluid_tick));
        self.solver = solver;
        changes
    }

    /// The flows that could not happen, for the `on_fluid_flow` hook.
    ///
    /// Drained rather than returned from [`Fluidics::tick`] for the reason
    /// `Solver::take_blocked` gives: it is an observation channel, and a server
    /// with no mods listening never asks.
    pub fn take_blocked(&mut self) -> Vec<tiamot_core::fluid::Blocked> {
        self.solver.take_blocked()
    }

    /// A registered fluid's string id, for handing to a mod.
    ///
    /// `None` for a placeholder — a fluid whose mod is gone has no name any mod
    /// could act on, and inventing one would put a stand-in's id into a
    /// comparison that will never match (charter rule 8).
    #[must_use]
    pub fn name_of(&self, id: tiamot_core::fluid::FluidId) -> Option<&str> {
        self.fluids
            .iter_registered()
            .find(|(registered, _)| *registered == id)
            .map(|(_, entry)| entry.name.as_str())
    }

    /// Every chunk a set of changes touched.
    ///
    /// What the caller re-meshes and re-sends. A `BTreeSet` so the order is the
    /// same on every platform — these end up in broadcast order, and charter
    /// rule 4 covers what the clients are told as much as what the server did.
    #[must_use]
    pub fn touched_chunks(changes: &[Flow]) -> BTreeSet<ChunkPos> {
        changes.iter().map(|change| change.pos.chunk()).collect()
    }

    /// What the solver runs with.
    ///
    /// The first registered fluid's settings, or the defaults where nothing is
    /// registered. Honest about its own limit: with several fluids this takes
    /// the first one's rate for all of them, which is wrong and is a smaller
    /// wrong than silently ignoring the field, which is what it did before.
    fn tuning(&self) -> Tuning {
        // **`iter_registered`, and this is not tidiness.** A world that has ever
        // held a fluid whose mod is now gone registers an inert placeholder for
        // it (charter rule 8, see `persist::fluidmap`), and placeholders are
        // numbered alongside real fluids — so one could land ahead of milk and
        // hand the solver an inert fluid's rules for the whole world. Milk would
        // stop spreading, and the cause would be a mod somebody removed months
        // earlier.
        self.fluids
            .iter_registered()
            .next()
            .map_or(Tuning::DEFAULT, |(_, f)| Tuning {
                waterlogs_at: f.waterlogs_at,
                tick_rate: f.tick_rate,
                evaporates: f.evaporates,
            })
    }

    fn write(&mut self, pos: BlockPos, value: Fluid) -> bool {
        let chunk = pos.chunk();
        if value.is_empty() && !self.layers.contains_key(&chunk) {
            return false;
        }
        let layer = self.layers.entry(chunk).or_default();
        let changed = layer.set(pos.local(), value);
        if layer.is_empty() {
            // The chunk drained. Dropped rather than kept, so a world that was
            // flooded and then emptied costs what it did before.
            self.layers.remove(&chunk);
        }
        if changed {
            // Marked here rather than at each caller because this is the single
            // funnel every write goes through — the solver's, a mod's, a
            // player's. A pond that changed and was not recorded is a pond that
            // reverts to its last save the next time its chunk loads.
            self.dirty.insert(chunk);
        }
        changed
    }
}

/// Lets a player's physics float in the milk the server is holding.
///
/// The counterpart to [`World`]'s `ChunkLookup`, and separate from it because
/// the two are separately owned here: geometry is the tick thread's, fluid is
/// behind [`Shared`]'s lock. `phys::Voxels::with_fluid` puts them back together
/// for the length of one step.
impl tiamot_core::phys::FluidLookup for Fluidics {
    fn fluid_layer(&self, pos: ChunkPos) -> Option<&FluidLayer> {
        self.layer(pos)
    }
}

/// The world plus its fluid, as the solver needs to see it.
struct Wet<'a> {
    terrain: crate::world::Solid<'a>,
    layers: &'a mut HashMap<ChunkPos, FluidLayer>,
    absorbency: &'a Absorbency,
}

impl Neighbourhood for Wet<'_> {
    /// Sub-Node Contract §4: how full the block is, in cells of 27.
    ///
    /// The world reports the fact and the fluid decides what it means — see
    /// `Tuning::waterlogs_at`.
    ///
    /// `resident` rather than `chunk`, for the reason light documents at the
    /// same call: a flow must never generate terrain. Milk reaching the edge of
    /// what is loaded would otherwise pull chunks into memory at whatever rate
    /// it spread, inside the tick. `None` is that case, and the solver treats it
    /// as floor.
    fn occupancy(&self, pos: BlockPos) -> Option<u32> {
        let chunk = self.terrain.resident(pos.chunk())?;
        Some(chunk.get_block_local(pos.local()).filled_cells())
    }

    /// Sub-Node Contract §4.3: how many cells this block drinks per tick.
    ///
    /// The material's own rate, and nothing about what it becomes — the solver
    /// has no registry, so it reports that a block absorbed and whoever holds
    /// one decides that dirt is now damp dirt.
    fn absorbency(&self, pos: BlockPos) -> u32 {
        if self.absorbency.is_empty() {
            return 0;
        }
        let Some(chunk) = self.terrain.resident(pos.chunk()) else {
            return 0;
        };
        self.absorbency
            .block(&chunk.get_block_local(pos.local()))
            .map_or(0, |absorbs| absorbs.rate)
    }

    fn fluid(&self, pos: BlockPos) -> Fluid {
        self.layers
            .get(&pos.chunk())
            .map_or(Fluid::EMPTY, |layer| layer.get(pos.local()))
    }

    fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
        let chunk = pos.chunk();
        // Writing nothing to a chunk that holds nothing must not allocate one.
        // Without this a flow reaching the edge of a pond would leave a 4 KiB
        // layer in every chunk it merely looked at.
        if value.is_empty() && !self.layers.contains_key(&chunk) {
            return;
        }
        let layer = self.layers.entry(chunk).or_default();
        layer.set(pos.local(), value);
        if layer.is_empty() {
            self.layers.remove(&chunk);
        }
    }
}

/// Builds the fluid registry from what the mods registered.
///
/// `id_of` maps a block id to its **world** material id, exactly as light's
/// equivalent does and for the same reason: a world that has seen a different
/// mod set numbers its materials differently, and a table of this session's
/// runtime ids would name every fluid's material one number out (charter rule
/// 8).
///
/// A fluid naming a block nothing registered is dropped with a warning rather
/// than refused. The alternative is a server that will not start because one
/// mod misspelled a block name, and a fluid nobody can see is a far smaller
/// problem than a world nobody can join.
#[must_use]
pub fn fluids_from_rules(
    rules: &[tiamot_core::script::FluidRules],
    id_of: impl Fn(&str) -> Option<tiamot_core::MaterialId>,
) -> Fluids {
    let mut fluids = Fluids::new();
    for rule in rules {
        let Some(material) = id_of(&rule.material) else {
            tracing::warn!(
                fluid = %rule.fluid,
                material = %rule.material,
                "a fluid names a block nothing registered; it will not be drawn"
            );
            continue;
        };
        if let Err(err) = fluids.register(tiamot_core::fluid::Registered {
            name: rule.fluid.clone(),
            waterlogs_at: rule.waterlogs_at,
            tick_rate: rule.tick_rate,
            evaporates: rule.evaporates,
            color: rule.color,
            material,
        }) {
            tracing::warn!(fluid = %rule.fluid, "could not register a fluid: {err}");
        }
    }
    fluids
}

/// The block at a flat index within a chunk.
fn block_at(chunk: ChunkPos, index: usize) -> BlockPos {
    let span = tiamot_core::CHUNK_BLOCKS as usize;
    let x = index % span;
    let y = (index / span) % span;
    let z = index / (span * span);
    BlockPos::new(
        chunk.x * tiamot_core::CHUNK_BLOCKS as i32 + x as i32,
        chunk.y * tiamot_core::CHUNK_BLOCKS as i32 + y as i32,
        chunk.z * tiamot_core::CHUNK_BLOCKS as i32 + z as i32,
    )
}

#[cfg(test)]
mod tests {
    use tiamot_core::fluid::{FluidId, MAX_VOLUME};

    use super::*;

    #[test]
    fn a_mod_write_that_settles_instantly_is_still_reported() {
        // **The bug the conserved model exposed.** `game.set_fluid` writes and
        // wakes the block, but only the SOLVER's changes were returned — so a
        // pour into a block with nowhere to flow produced an empty change list
        // and no `ChunkFluid` ever went out. The milk existed on the server,
        // answered `game.get_fluid`, and was invisible to everybody.
        //
        // The old model hid it: a source fed its neighbours or renewed itself,
        // so there was always something for the next tick to report.
        let mut fluids = Fluids::new();
        let milk = fluids
            .register(tiamot_core::fluid::Registered {
                name: "test:milk".into(),
                waterlogs_at: 14,
                tick_rate: 1,
                evaporates: 0,
                color: [255, 255, 255],
                material: tiamot_core::MaterialId(4),
            })
            .expect("register");
        let mut fluidics = Fluidics::new(fluids);

        let block = BlockPos::new(1, 2, 3);
        assert!(fluidics.set(block, Fluid::new(milk, MAX_VOLUME)));

        let dir = std::env::temp_dir().join("tiamot-fluid-write-report");
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("world.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let mut registry = tiamot_core::Registry::new();
        let db = tiamot_core::persist::WorldDb::open(&path, &mut registry).expect("open");
        let world = crate::world::World::open(db, 1).expect("world");
        let changes = fluidics.tick(tiamot_core::domain::OVERWORLD, &world, 0, 0);

        assert!(
            changes.iter().any(|flow| flow.pos == block),
            "a mod's own write was not reported, so no client is ever told about it"
        );
        // And exactly once: a write reported again on the next tick would
        // re-broadcast a chunk for ever.
        let again = fluidics.tick(tiamot_core::domain::OVERWORLD, &world, 1, 0);
        assert!(
            !again
                .iter()
                .any(|flow| flow.pos == block && flow.was.is_empty()),
            "the same write was reported twice"
        );
    }

    #[test]
    fn the_bench_and_the_server_agree_on_the_cap() {
        // `crates/core` cannot depend on the server (charter rule 3), so the
        // benchmark that SET this number has to hold its own copy. This is what
        // stops the two drifting and the published figure quietly becoming a
        // measurement of something the server does not do.
        let bench = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../core/benches/fluid.rs"
        ))
        .expect("the fluid benchmark");
        let declared = format!("const VISITS: usize = {VISITS_PER_TICK};");
        assert!(
            bench.contains(&declared),
            "the benchmark does not declare `{declared}`, so its numbers are not this server's"
        );
    }

    #[test]
    fn a_pour_marks_its_chunk_for_writing_and_a_load_does_not() {
        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());
        let pos = ChunkPos::new(1, 0, 0);
        let corner = block_at(pos, 0);

        // Arriving from the database is not a change: the layer already agrees
        // with what is on disk, and marking it dirty would rewrite every chunk
        // a player walks past.
        let mut saved = FluidLayer::empty();
        saved.set(
            tiamot_core::coords::LocalBlock::new(0, 0, 0),
            Fluid::new(milk, MAX_VOLUME),
        );
        fluidics.chunk_loaded(pos, saved);
        assert_eq!(fluidics.dirty(), 0, "a load dirtied the chunk it loaded");

        // A pour is.
        fluidics.set(
            BlockPos::new(corner.x + 1, corner.y, corner.z),
            Fluid::new(milk, MAX_VOLUME),
        );
        assert_eq!(fluidics.dirty(), 1);

        let taken = fluidics.take_dirty();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, pos);
        assert_eq!(taken[0].1.filled(), 2);
        assert_eq!(fluidics.dirty(), 0, "taking the list did not clear it");
    }

    #[test]
    fn a_chunk_that_drained_is_still_written_so_the_row_goes_away() {
        // **The bug this guards.** When a layer empties it is dropped from the
        // map to give the memory back — and if the dirty list followed it, the
        // save would never hear about the drain and the pond would come back
        // the next time the chunk loaded.
        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());
        let pos = ChunkPos::new(-2, 3, 1);
        let block = block_at(pos, 0);

        fluidics.set(block, Fluid::new(milk, MAX_VOLUME));
        fluidics.take_dirty();

        fluidics.set(block, Fluid::EMPTY);
        assert!(fluidics.layer(pos).is_none(), "the layer was not dropped");

        let taken = fluidics.take_dirty();
        assert_eq!(taken.len(), 1, "a drained chunk was not queued for writing");
        assert_eq!(taken[0].0, pos);
        assert!(
            taken[0].1.is_empty(),
            "a drained chunk must be written as empty, which is what deletes its row"
        );
    }

    #[test]
    fn a_chunk_that_arrives_twice_is_only_read_once() {
        // **The bug this guards.** The tick's arrival list is shared with the
        // lighting, which defers what it cannot relight by putting the chunk
        // BACK into that list — so chunks genuinely arrive twice, a tick apart.
        // A second load would replace the live layer with whatever was last
        // written to disk, discarding a pour made in between.
        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());
        let pos = ChunkPos::new(0, 0, 0);

        assert!(!fluidics.knows(pos), "knew a chunk it has never seen");
        fluidics.chunk_loaded(pos, FluidLayer::empty());
        assert!(
            fluidics.knows(pos),
            "a chunk read and found dry must still count as read, or every \
             arrival goes back to the database"
        );

        // A player pours into it, and then it arrives again.
        let block = block_at(pos, 0);
        fluidics.set(block, Fluid::new(milk, MAX_VOLUME));
        assert!(fluidics.knows(pos), "the pour lost the chunk's read mark");

        // Forgetting it — a real unload — puts it back to unread.
        fluidics.forget(pos);
        assert!(!fluidics.knows(pos));
        assert!(fluidics.layer(pos).is_none());
    }

    #[test]
    fn a_player_standing_in_a_pond_reads_as_submerged_wherever_the_pond_is() {
        // **The seam this test exists for.** The physics works in cells of a
        // frame anchored to the player's origin chunk; the fluid is stored in
        // world blocks keyed by chunk. `Voxels::with_fluid` converts between
        // them, and a conversion that was off by a chunk would be invisible at
        // the origin and wrong everywhere else — which is exactly the bug that
        // cost this project a session once already (the chunk-frame bug), so it
        // gets tested at a negative, non-zero origin rather than at 0,0,0.
        use tiamot_core::phys::{Body, Solid, Voxels};

        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());

        // Four blocks of milk in a column, in a chunk a long way from spawn.
        let origin = ChunkPos::new(-3, 5, 7);
        let corner = BlockPos::new(
            origin.x * tiamot_core::CHUNK_BLOCKS as i32,
            origin.y * tiamot_core::CHUNK_BLOCKS as i32,
            origin.z * tiamot_core::CHUNK_BLOCKS as i32,
        );
        for y in 0..4 {
            fluidics.set(
                BlockPos::new(corner.x + 2, corner.y + y, corner.z + 2),
                Fluid::new(milk, MAX_VOLUME),
            );
        }

        // No chunks resident at all: this is testing the fluid half of the
        // view, and absent geometry reads as solid without affecting what the
        // milk says.
        struct NoChunks;

        impl tiamot_core::phys::ChunkLookup for NoChunks {
            fn chunk(&self, _pos: ChunkPos) -> Option<&tiamot_core::Chunk> {
                None
            }
        }

        let voxels = Voxels::with_fluid(&NoChunks, &fluidics, origin);

        // Frame block (2, 0, 2) is the bottom of that column, and the frame's
        // origin IS the chunk, so the local block coordinates are the frame's.
        assert_eq!(
            voxels.fluid(2, 0, 2).fluid(),
            milk,
            "the milk did not survive the frame conversion at origin {origin:?}"
        );
        assert!(
            voxels.fluid(2, 4, 2).is_empty(),
            "found milk above the column"
        );
        assert!(voxels.fluid(3, 0, 2).is_empty(), "found milk beside it");

        // And a body standing in it floats. Frame block 2 spans cells 6..9.
        let wet = tiamot_core::phys::submersion(&voxels, &Body::at([7.0, 0.0, 7.0]).aabb());
        assert_eq!(wet.fluid, milk);
        assert!(
            wet.fraction > 0.9,
            "a body inside four blocks of milk read {} submerged",
            wet.fraction
        );
    }

    #[test]
    fn a_flat_index_becomes_the_block_it_names() {
        // The inverse of `LocalBlock::index`, and getting it wrong would queue
        // the wrong blocks when a saved pond comes back.
        let chunk = ChunkPos::new(2, -1, 3);
        for (index, want) in [
            (0, (32, -16, 48)),
            (1, (33, -16, 48)),
            (16, (32, -15, 48)),
            (256, (32, -16, 49)),
            (4095, (47, -1, 63)),
        ] {
            let pos = block_at(chunk, index);
            assert_eq!((pos.x, pos.y, pos.z), want, "index {index}");
            assert_eq!(pos.chunk(), chunk);
        }
    }

    #[test]
    fn an_empty_chunk_is_not_stored_when_it_loads() {
        // A world of dry chunks must cost nothing, and the load path is where
        // that is easiest to get wrong: every chunk arrives, every chunk has a
        // layer, and the map grows without bound.
        let mut fluidics = Fluidics::new(Fluids::new());
        fluidics.chunk_loaded(ChunkPos::new(0, 0, 0), FluidLayer::empty());
        assert!(fluidics.is_empty());
        assert!(fluidics.is_settled());
    }

    #[test]
    fn a_saved_pond_comes_back_queued() {
        // Milk saved mid-flow has to carry on flowing when it loads.
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        layer.set(
            tiamot_core::coords::LocalBlock::new(1, 2, 3),
            Fluid::new(milk, 4),
        );

        let mut fluidics = Fluidics::new(Fluids::new());
        fluidics.chunk_loaded(ChunkPos::new(0, 0, 0), layer);

        assert!(!fluidics.is_empty());
        assert!(!fluidics.is_settled(), "a loaded pond was not queued");
        assert_eq!(fluidics.at(BlockPos::new(1, 2, 3)).volume(), 4);
    }

    #[test]
    fn a_block_that_drains_gives_its_chunk_back() {
        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());
        assert!(fluidics.set(BlockPos::new(0, 0, 0), Fluid::new(milk, MAX_VOLUME)));
        assert!(!fluidics.is_empty());

        assert!(fluidics.set(BlockPos::new(0, 0, 0), Fluid::EMPTY));
        assert!(
            fluidics.is_empty(),
            "a drained chunk is still holding a layer"
        );
    }

    #[test]
    fn clearing_a_block_in_a_dry_chunk_allocates_nothing() {
        let mut fluidics = Fluidics::new(Fluids::new());
        assert!(!fluidics.set(BlockPos::new(5, 5, 5), Fluid::EMPTY));
        assert!(fluidics.is_empty());
    }
}
