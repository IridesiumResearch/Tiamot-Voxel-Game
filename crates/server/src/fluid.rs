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
//! [`Fluidics::chunk_loaded`] and [`Fluidics::layer`] are the two ends of that
//! and are used by the network path today. **The database side is not wired
//! yet** — see the task's remaining work — so a pond currently survives a chunk
//! leaving memory only for as long as the server runs. Written here rather than
//! left to be discovered: an unwired half of a round trip is the kind of thing
//! that reads as finished from either end alone.
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
use tiamot_core::fluid::{Flow, Fluid, FluidLayer, Fluids, Neighbourhood, Solver, Tuning};

use crate::world::World;

/// Simulation ticks between fluid updates.
///
/// Two, so fluid runs at 10 Hz against the simulation's 20. The task names this
/// and the reason is cost rather than feel.
pub const TICKS_PER_FLUID_TICK: u64 = 2;

/// The most blocks one fluid tick will visit.
///
/// **Measured against the budget rather than guessed.** Charter rule 18 gives
/// all of simulation 50 ms; the spring-field benchmark is what says whether this
/// number is right, and it is reported as a share of that budget rather than in
/// isolation. Blocks past it are carried, not dropped.
pub const VISITS_PER_TICK: usize = 4_096;

/// A handle on the fluid store, for the mod API.
///
/// The same arrangement `light::Shared` uses, and behind a lock for the same
/// reason: `game.set_fluid` runs inside a tick, on the simulation thread, and
/// cannot borrow what the tick is holding. Uncontended in practice — both sides
/// are that one thread — and never held across a mod callback, which is the
/// arrangement that would deadlock.
pub struct Shared {
    fluidics: std::sync::Arc<std::sync::RwLock<Fluidics>>,
}

impl Shared {
    /// Wraps a store the simulation thread owns.
    #[must_use]
    pub const fn new(fluidics: std::sync::Arc<std::sync::RwLock<Fluidics>>) -> Self {
        Self { fluidics }
    }
}

impl tiamot_core::fluid::Access for Shared {
    fn fluid_at(&self, pos: BlockPos) -> Fluid {
        // A poisoned lock means the simulation thread panicked, in which case
        // there is no world to have milk in. Empty is the honest answer, and
        // panicking inside a mod callback would blame the mod.
        self.fluidics
            .read()
            .map_or(Fluid::EMPTY, |fluidics| fluidics.at(pos))
    }

    fn set_fluid_at(&self, pos: BlockPos, value: Fluid) -> bool {
        self.fluidics
            .write()
            .is_ok_and(|mut fluidics| fluidics.set(pos, value))
    }

    fn fluid_id(&self, name: &str) -> Option<tiamot_core::fluid::FluidId> {
        self.fluidics
            .read()
            .ok()
            .and_then(|fluidics| fluidics.fluids().id_of(name))
    }
}

/// Every loaded chunk's fluid, and what the mods registered.
#[derive(Debug, Default)]
pub struct Fluidics {
    layers: HashMap<ChunkPos, FluidLayer>,
    fluids: Fluids,
    solver: Solver,
}

impl Fluidics {
    /// A store for a world whose mods registered these fluids.
    #[must_use]
    pub fn new(fluids: Fluids) -> Self {
        Self {
            layers: HashMap::new(),
            fluids,
            solver: Solver::new(),
        }
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

    /// Takes a chunk's fluid as it came out of the database.
    ///
    /// An empty layer is not stored: the map is for chunks that hold something,
    /// so a world with one pond in it has one entry however far anybody walks.
    /// Everything in the chunk is queued, because milk saved mid-flow has to
    /// carry on flowing when it comes back.
    pub fn chunk_loaded(&mut self, pos: ChunkPos, layer: FluidLayer) {
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
    pub fn forget(&mut self, pos: ChunkPos) {
        self.layers.remove(&pos);
    }

    /// Records what a block holds and wakes its neighbourhood.
    ///
    /// The one way fluid enters the world from outside the solver: a mod's
    /// `game.set_fluid`, a player pouring, a chunk being edited.
    pub fn set(&mut self, pos: BlockPos, value: Fluid) -> bool {
        let changed = self.write(pos, value);
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
    pub fn tick(&mut self, world: &World, fluid_tick: u64) -> Vec<Flow> {
        if self.solver.is_settled() {
            return Vec::new();
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
            return Vec::new();
        }
        // Taken apart so the solver can borrow the store mutably while the world
        // is borrowed immutably, which is the same trick `Lit` plays for light.
        let mut solver = std::mem::take(&mut self.solver);
        let mut view = Wet {
            world,
            layers: &mut self.layers,
        };
        let changes = solver.tick(&mut view, tuning, VISITS_PER_TICK);
        self.solver = solver;
        changes
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
        self.fluids
            .iter()
            .next()
            .map_or(Tuning::DEFAULT, |(_, f)| Tuning {
                flow_range: f.flow_range,
                hole_search: Tuning::DEFAULT.hole_search,
                tick_rate: f.tick_rate,
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
        changed
    }
}

/// The world plus its fluid, as the solver needs to see it.
struct Wet<'a> {
    world: &'a World,
    layers: &'a mut HashMap<ChunkPos, FluidLayer>,
}

impl Neighbourhood for Wet<'_> {
    /// Sub-Node Contract §4: a block accepts fluid iff its occupancy is empty.
    ///
    /// `resident` rather than `chunk`, for the reason light documents at the
    /// same call: a flow must never generate terrain. Milk reaching the edge of
    /// what is loaded would otherwise pull chunks into memory at whatever rate
    /// it spread, inside the tick.
    fn accepts_fluid(&self, pos: BlockPos) -> bool {
        let Some(chunk) = self.world.resident(pos.chunk()) else {
            return false;
        };
        chunk.get_block_local(pos.local()).is_empty()
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
            flow_range: rule.flow_range,
            tick_rate: rule.tick_rate,
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
    use tiamot_core::fluid::FluidId;

    use super::*;

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
            Fluid::flowing(milk, 4),
        );

        let mut fluidics = Fluidics::new(Fluids::new());
        fluidics.chunk_loaded(ChunkPos::new(0, 0, 0), layer);

        assert!(!fluidics.is_empty());
        assert!(!fluidics.is_settled(), "a loaded pond was not queued");
        assert_eq!(fluidics.at(BlockPos::new(1, 2, 3)).level(), 4);
    }

    #[test]
    fn a_block_that_drains_gives_its_chunk_back() {
        let milk = FluidId(1);
        let mut fluidics = Fluidics::new(Fluids::new());
        assert!(fluidics.set(BlockPos::new(0, 0, 0), Fluid::source(milk)));
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
