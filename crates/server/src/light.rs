// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's light, and the adapter that runs core's propagation over it.
//!
//! # Light is derived, not stored
//!
//! Nothing here is written to the world file. Light is a pure function of block
//! content and the mods' emission table, so persisting it would be storing a
//! cache that can disagree with what it was derived from — and a world file
//! whose light said "cave" where its blocks said "hillside" is a bug nobody
//! would think to look for in the file format. A chunk is relit when it enters
//! memory.
//!
//! # What that costs, measured
//!
//! **1.38 ms** for the case that dominates a join — a chunk of air under open
//! sky with its neighbours resident — and 0.24 ms for solid rock, on the
//! reference machine. Task 02b's spike said 30 µs, which was the number the
//! tick's cap was sized on until this was written down; see
//! `handle::RELIGHTS_PER_TICK`. Charter rule 18 wants the share of a 50 ms
//! tick, and one relight is 2.8% of one.
//!
//! Relighting a chunk that already has light is therefore never free and never
//! useful: the tick skips it, and `bot`'s
//! `a_chunk_is_lit_once_however_many_players_ask_for_it` holds that line.
//!
//! # Relighting a chunk needs its neighbours
//!
//! Light crosses chunk boundaries, so a chunk relit alone is dark down its
//! edges until its neighbours arrive. The region is the chunk exactly, and the
//! ring of blocks around it acts as a boundary condition: those keep their
//! light and flood inward. [`Lighting::chunk_loaded`] reports every other chunk
//! it touched on the way, so the caller can remesh and re-send them too.

use std::collections::{BTreeSet, HashMap};

use tiamot_core::coords::{BlockPos, ChunkPos};
use tiamot_core::light::propagate::{Neighbourhood, Region};
use tiamot_core::light::{Emissions, Faces, Light, LightLayer, propagate};
use tiamot_core::{CHUNK_BLOCKS, MaterialId};

use crate::world::World;

/// The light store, as a mod may read it.
///
/// This is the whole of `game.get_light`'s implementation: the VM lives in
/// core and cannot know about [`Lighting`] (charter rule 3), so it asks through
/// [`tiamot_core::light::LightSource`] and this is what answers.
///
/// **Read-only, deliberately.** A mod that could write light would be writing a
/// derived value — the next relight would overwrite it, and the disagreement in
/// between would be invisible everywhere except in whatever the mod did next.
/// If a mod wants somewhere to be brighter, the way to say so is a block that
/// emits.
#[derive(Debug)]
pub struct Shared {
    lighting: std::sync::Arc<std::sync::RwLock<Lighting>>,
}

impl Shared {
    /// Wraps a store the simulation thread owns.
    #[must_use]
    pub const fn new(lighting: std::sync::Arc<std::sync::RwLock<Lighting>>) -> Self {
        Self { lighting }
    }
}

impl tiamot_core::light::LightSource for Shared {
    fn light_at(&self, pos: BlockPos) -> Light {
        // A poisoned lock means the simulation thread panicked, in which case
        // there is no light and no world; darkness is the honest answer and
        // panicking inside a mod callback would blame the mod.
        self.lighting
            .read()
            .map_or(Light::DARK, |lighting| lighting.at(pos))
    }
}

/// Every loaded chunk's light, and what the mods said glows.
#[derive(Debug, Default)]
pub struct Lighting {
    layers: HashMap<ChunkPos, LightLayer>,
    emissions: Emissions,
}

impl Lighting {
    /// A store for a world whose mods emit these levels.
    #[must_use]
    pub fn new(emissions: Emissions) -> Self {
        Self {
            layers: HashMap::new(),
            emissions,
        }
    }

    /// What the mods said glows.
    #[must_use]
    pub const fn emissions(&self) -> &Emissions {
        &self.emissions
    }

    /// The light at a block, or [`Light::DARK`] if its chunk is not loaded.
    ///
    /// Dark rather than an error: a mod asking about somewhere nobody is gets
    /// the honest answer that there is no light there to speak of, and an
    /// `Option` would push that judgement onto every caller.
    #[must_use]
    pub fn at(&self, pos: BlockPos) -> Light {
        self.layers
            .get(&pos.chunk())
            .map_or(Light::DARK, |layer| layer.get(pos.local()))
    }

    /// How many chunks have light.
    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Whether any chunk has light.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Whether a chunk has light at all.
    ///
    /// The question the tick's catch-up pass asks: a chunk with blocks and no
    /// light renders black, so anything resident and unlit is work to do.
    #[must_use]
    pub fn holds(&self, pos: ChunkPos) -> bool {
        self.layers.contains_key(&pos)
    }

    /// A chunk's levels, for sending to a client.
    #[must_use]
    pub fn layer(&self, pos: ChunkPos) -> Option<&LightLayer> {
        self.layers.get(&pos)
    }

    /// Forgets a chunk's light.
    ///
    /// Called when a chunk leaves memory. Keeping it would be a slow leak of
    /// 8 KiB per chunk for a world nobody is looking at.
    pub fn forget(&mut self, pos: ChunkPos) {
        self.layers.remove(&pos);
    }

    /// Relights a chunk that has just entered memory.
    ///
    /// Returns every chunk whose light changed, including this one — the caller
    /// needs that set to know what to remesh and what to re-send.
    ///
    /// Costly, and never worth doing twice: see the module docs for what it
    /// measures and for the guard the tick keeps in front of it.
    pub fn chunk_loaded(
        &mut self,
        domain: &str,
        world: &World,
        pos: ChunkPos,
    ) -> BTreeSet<ChunkPos> {
        self.layers.entry(pos).or_insert_with(LightLayer::dark);

        // Exactly the chunk. The blocks around it are handled as a boundary
        // condition by `relight` — they keep their light and flood inward —
        // rather than being relit as part of the region. Widening the region
        // instead would clear the neighbours' light and then fail to re-seed
        // it, because the sky that lit them is further away still.
        let corner = BlockPos::from_chunk_corner(pos);
        let span = CHUNK_BLOCKS as i32 - 1;
        let region = Region {
            min: corner,
            max: BlockPos::new(corner.x + span, corner.y + span, corner.z + span),
        };

        let mut touched = Touched::default();
        // The chunk itself, always — even when relighting changed nothing.
        // "Nothing changed" is measured against the dark layer this function
        // just inserted, but a client has no layer at all, so a chunk that is
        // genuinely pitch black still has to be told to it. Without this a
        // client cannot tell "dark" from "not arrived yet", and every
        // underground chunk goes unreported.
        touched.chunks.insert(pos);
        self.with_centre(domain, world, pos, &mut touched, |lit| {
            propagate::relight(lit, region);
        });
        self.compact(&touched.chunks);
        touched.chunks
    }

    /// Re-lights around a block whose content just changed.
    ///
    /// Returns every chunk whose light changed.
    pub fn edited(&mut self, domain: &str, world: &World, pos: BlockPos) -> BTreeSet<ChunkPos> {
        let mut touched = Touched::default();
        self.with_centre(domain, world, pos.chunk(), &mut touched, |lit| {
            propagate::edited(lit, pos);
        });
        self.compact(&touched.chunks);
        touched.chunks
    }

    /// Runs a propagation pass with `centre`'s layer held out of the map.
    ///
    /// Taking it out and putting it back is what lets [`Lit`] reach the layer
    /// the flood spends its time in without a lookup per visit.
    ///
    /// A chunk that arrived here **without** a layer keeps none: writes to it go
    /// nowhere and nothing is inserted, exactly as when the layer was looked up
    /// and found missing. Creating one instead would quietly mark an unlit chunk
    /// as done — [`Lighting::holds`] is what the tick's catch-up pass asks
    /// before relighting, so the chunk would stay black for as long as it stayed
    /// loaded.
    fn with_centre(
        &mut self,
        domain: &str,
        world: &World,
        centre: ChunkPos,
        touched: &mut Touched,
        pass: impl FnOnce(&mut Lit<'_>),
    ) {
        let held = self.layers.remove(&centre);
        let lit_here = held.is_some();
        let mut lit = Lit {
            terrain: world.solid(domain),
            lighting: self,
            touched,
            centre,
            lit_here,
            layer: held.unwrap_or_else(LightLayer::dark),
        };
        pass(&mut lit);
        let layer = lit.layer;
        if lit_here {
            self.layers.insert(centre, layer);
        }
    }

    /// Collapses layers that ended up uniform.
    ///
    /// Once per relight rather than per write — see [`LightLayer::compact`],
    /// which would otherwise turn a linear relight quadratic.
    fn compact(&mut self, chunks: &BTreeSet<ChunkPos>) {
        for pos in chunks {
            if let Some(layer) = self.layers.get_mut(pos) {
                layer.compact();
            }
        }
    }
}

/// Chunks whose light a propagation pass changed.
///
/// A `BTreeSet` rather than a `HashSet`: this set decides what gets remeshed and
/// re-sent, so its iteration order is observable and charter rule 4 wants it
/// fixed.
#[derive(Debug, Default)]
struct Touched {
    chunks: BTreeSet<ChunkPos>,
}

/// The world and its light, as [`Neighbourhood`] wants to see them.
///
/// # Why the chunk being relit is held apart from the others
///
/// A flood spends nearly all of its visits inside one chunk, and every one of
/// them used to hash a [`ChunkPos`] to find that chunk's layer — twice, once to
/// read the level and once to write it. Holding the centre layer as a field
/// turns those into a field access and leaves the map for the boundary, where
/// the flood goes rarely.
///
/// Measured on the reference machine, relighting a chunk of air under open sky
/// with its neighbours resident: **1.56 ms before, 1.38 ms after**. Worth
/// keeping and not the win it looks like it should be — the lookups are 12% of
/// this, and the propagation itself is the rest. Memoising the *world* chunk
/// the same way was measured first and bought 3%, so it was not kept.
struct Lit<'a> {
    terrain: crate::world::Solid<'a>,
    lighting: &'a mut Lighting,
    touched: &'a mut Touched,
    /// The chunk this pass is centred on, taken out of the map for the
    /// duration and put back by [`Lighting::with_centre`].
    centre: ChunkPos,
    /// Whether the centre chunk had light at all when the pass started.
    ///
    /// A chunk that had none keeps none — writes to it go nowhere, exactly as
    /// they did when the layer was looked up and found missing.
    lit_here: bool,
    /// Its light. Owned here, so reaching it costs nothing.
    layer: LightLayer,
}

impl Lit<'_> {
    /// The light at a block, from the centre layer where it lives there.
    fn level(&self, pos: BlockPos) -> Light {
        if pos.chunk() == self.centre {
            if !self.lit_here {
                return Light::DARK;
            }
            return self.layer.get(pos.local());
        }
        self.lighting.at(pos)
    }
}

impl Neighbourhood for Lit<'_> {
    fn faces(&self, pos: BlockPos) -> Option<Faces> {
        // `resident` rather than `chunk`: propagation must never generate a
        // chunk. A flood reaching unexplored terrain would otherwise turn a
        // lamp into unbounded worldgen inside the tick, which is the same trap
        // collision documents at `World::resident`.
        let chunk = self.terrain.resident(pos.chunk())?;
        Some(chunk.faces(pos.local()))
    }

    fn emission(&self, pos: BlockPos) -> Light {
        let Some(chunk) = self.terrain.resident(pos.chunk()) else {
            return Light::DARK;
        };
        self.lighting
            .emissions
            .block(&chunk.get_block_local(pos.local()))
    }

    fn light(&self, pos: BlockPos) -> Light {
        self.level(pos)
    }

    fn set_light(&mut self, pos: BlockPos, level: Light) {
        let chunk = pos.chunk();
        let local = pos.local();
        if chunk == self.centre {
            if !self.lit_here || self.layer.get(local) == level {
                return;
            }
            self.layer.set(local, level);
            self.touched.chunks.insert(chunk);
            return;
        }
        // Only where a chunk is actually loaded. A flood reaching the edge of
        // the loaded region has nowhere to write, and that is the bound working.
        let Some(layer) = self.lighting.layers.get_mut(&chunk) else {
            return;
        };
        if layer.get(local) == level {
            return;
        }
        layer.set(local, level);
        self.touched.chunks.insert(chunk);
    }
}

/// Builds an emission table from what the mods registered.
#[must_use]
pub fn emissions_from_rules(
    rules: &[tiamot_core::script::BlockRules],
    id_of: impl Fn(&str) -> Option<MaterialId>,
) -> Emissions {
    Emissions::new(rules.iter().filter_map(|rule| {
        let level = rule.emission();
        if level.is_dark() {
            return None;
        }
        Some((id_of(&rule.block)?, level))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::block::BlockValue;
    use tiamot_core::light::MAX_LEVEL;
    use tiamot_core::persist::WorldDb;

    const STONE: MaterialId = MaterialId(2);
    const LAMP: MaterialId = MaterialId(3);

    /// A generator that produces nothing but air, so tests decide the contents.
    struct Empty;

    impl crate::world::ChunkSource for Empty {
        fn generate(
            &mut self,
            _domain: &str,
            pos: ChunkPos,
            _seed: u64,
        ) -> tiamot_core::chunk::Chunk {
            tiamot_core::chunk::Chunk::air(pos)
        }
    }

    fn world() -> World {
        let mut registry = tiamot_core::Registry::new();
        for name in ["test:stone", "test:lamp"] {
            registry.register(name).expect("register");
        }
        let db = WorldDb::open_in_memory(&mut registry).expect("open");
        World::open(db, 1).expect("world")
    }

    fn lighting() -> Lighting {
        Lighting::new(Emissions::new([(LAMP, Light::new(0, MAX_LEVEL, 0, 0))]))
    }

    /// Loads a chunk so it is resident, without caring what is in it.
    fn resident(world: &mut World, pos: ChunkPos) {
        world
            .chunk(tiamot_core::domain::OVERWORLD, pos, &mut Empty)
            .expect("chunk");
    }

    #[test]
    fn an_air_chunk_under_open_sky_is_fully_lit() {
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, 0, 0);
        resident(&mut world, pos);

        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);

        assert_eq!(light.at(BlockPos::new(8, 15, 8)).sun(), MAX_LEVEL);
        assert_eq!(
            light.at(BlockPos::new(8, 0, 8)).sun(),
            MAX_LEVEL,
            "sunlight did not reach the bottom of an empty chunk"
        );
    }

    #[test]
    fn a_lamp_lights_the_dark_around_it() {
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, 0, 0);
        resident(&mut world, pos);
        // Fill it solid so sunlight cannot get in, then hollow out a room.
        {
            let chunk = world
                .chunk(tiamot_core::domain::OVERWORLD, pos, &mut Empty)
                .expect("chunk");
            for index in 0..tiamot_core::BLOCKS_PER_CHUNK {
                chunk.set_block_local(
                    tiamot_core::coords::LocalBlock::from_index(index),
                    BlockValue::Uniform(STONE),
                );
            }
            for x in 4..12 {
                for y in 4..12 {
                    for z in 4..12 {
                        chunk.set_block_local(
                            tiamot_core::coords::LocalBlock::new(x, y, z),
                            BlockValue::AIR,
                        );
                    }
                }
            }
            chunk.set_block_local(
                tiamot_core::coords::LocalBlock::new(8, 8, 8),
                BlockValue::Uniform(LAMP),
            );
        }

        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);

        assert_eq!(light.at(BlockPos::new(8, 8, 8)).red(), MAX_LEVEL);
        assert_eq!(light.at(BlockPos::new(9, 8, 8)).red(), MAX_LEVEL - 1);
        assert_eq!(
            light.at(BlockPos::new(8, 8, 8)).sun(),
            0,
            "a sealed room saw daylight"
        );
    }

    #[test]
    fn light_from_a_neighbouring_chunk_reaches_across_the_seam() {
        // The reason `chunk_loaded` relights a region a block wider than the
        // chunk. Without it a chunk arriving next to a lit one is dark down its
        // edge until something else disturbs it, and the seam is visible.
        let mut world = world();
        let mut light = lighting();
        let west = ChunkPos::new(-1, 0, 0);
        let east = ChunkPos::new(0, 0, 0);
        resident(&mut world, west);
        resident(&mut world, east);

        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, west);
        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, east);

        // The block either side of the boundary is lit from the sky in both
        // chunks, so instead put a lamp at the very edge of the west chunk and
        // check it crosses.
        {
            let chunk = world
                .chunk(tiamot_core::domain::OVERWORLD, west, &mut Empty)
                .expect("chunk");
            chunk.set_block_local(
                tiamot_core::coords::LocalBlock::new(15, 8, 8),
                BlockValue::Uniform(LAMP),
            );
        }
        let touched = light.edited(
            tiamot_core::domain::OVERWORLD,
            &world,
            BlockPos::new(-1, 8, 8),
        );

        assert!(
            touched.contains(&east),
            "an edit at the seam did not report the neighbour as changed: {touched:?}"
        );
        assert!(
            light.at(BlockPos::new(0, 8, 8)).red() > 0,
            "a lamp on the boundary did not light the next chunk"
        );
    }

    #[test]
    fn propagation_never_generates_a_chunk() {
        // The trap `World::resident` documents, from lighting's side: a flood
        // that could generate terrain would turn one lamp into unbounded
        // worldgen inside the tick.
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, 0, 0);
        resident(&mut world, pos);
        let before = world.cached();

        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);

        assert_eq!(
            world.cached(),
            before,
            "relighting generated {} chunks",
            world.cached() - before
        );
    }

    #[test]
    fn a_chunk_that_is_pitch_black_is_still_reported() {
        // A client has no layer for a chunk it has never been told about, so
        // "dark" and "not arrived" are the same to it. Reporting only what
        // CHANGED would leave every underground chunk unsent, and a client
        // cannot tell that from a message still in flight.
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, -4, 0);
        {
            let chunk = world
                .chunk(tiamot_core::domain::OVERWORLD, pos, &mut Empty)
                .expect("chunk");
            for index in 0..tiamot_core::BLOCKS_PER_CHUNK {
                chunk.set_block_local(
                    tiamot_core::coords::LocalBlock::from_index(index),
                    BlockValue::Uniform(STONE),
                );
            }
        }

        let touched = light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);

        assert!(
            touched.contains(&pos),
            "a chunk that relit to unchanged darkness was not reported: {touched:?}"
        );
        assert!(
            light.layer(pos).is_some_and(|layer| layer
                .is_uniform()
                .is_some_and(tiamot_core::light::Light::is_dark)),
            "solid rock with no lamps should be uniformly dark"
        );
    }

    #[test]
    fn a_forgotten_chunk_takes_its_light_with_it() {
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, 0, 0);
        resident(&mut world, pos);
        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);
        assert_eq!(light.len(), 1);

        light.forget(pos);
        assert!(light.is_empty());
        assert_eq!(light.at(BlockPos::new(0, 0, 0)), Light::DARK);
    }

    #[test]
    fn an_uninteresting_chunk_costs_one_word() {
        // A fully lit air chunk and a sealed dark one are both uniform, and the
        // layer should say so — 8 KiB per chunk for "all daylight" would be a
        // pure waste across a streamed world.
        let mut world = world();
        let mut light = lighting();
        let pos = ChunkPos::new(0, 0, 0);
        resident(&mut world, pos);
        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, pos);

        let layer = light.layer(pos).expect("lit");
        assert!(
            layer.is_compact(),
            "a uniformly lit chunk kept its dense array"
        );
        assert_eq!(layer.memory_usage(), 0);
    }

    #[test]
    fn a_lamp_lights_across_a_chunk_boundary_and_stops_when_it_goes() {
        // Reported from the window: placing a lamp beside a chunk boundary
        // sometimes left the NEIGHBOUR dark, and removing one left the
        // neighbour lit. Both directions, because they are different halves of
        // the incremental path and only one of them was broken.
        let mut world = world();
        let mut light = lighting();

        // Two chunks side by side, both resident and both lit, so this is the
        // incremental path rather than a chunk arriving.
        let here = ChunkPos::new(0, 0, 0);
        let next = ChunkPos::new(1, 0, 0);
        resident(&mut world, here);
        resident(&mut world, next);
        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, here);
        light.chunk_loaded(tiamot_core::domain::OVERWORLD, &world, next);

        // A block just inside the first chunk's far edge, and one just over the
        // line in the second.
        let lamp = BlockPos::new(15, 8, 8);
        let across = BlockPos::new(16, 8, 8);
        assert_eq!(across.chunk(), next, "the test aims at the wrong chunk");

        {
            let chunk = world
                .chunk(tiamot_core::domain::OVERWORLD, here, &mut Empty)
                .expect("chunk");
            chunk.set_block_local(
                tiamot_core::coords::LocalBlock::new(15, 8, 8),
                BlockValue::Uniform(LAMP),
            );
        }
        let touched = light.edited(tiamot_core::domain::OVERWORLD, &world, lamp);

        assert!(
            light.at(across).red() > 0,
            "the lamp lit nothing across the chunk boundary: {:?}",
            light.at(across)
        );
        assert!(
            touched.contains(&next),
            "the neighbour's light changed and was not reported, so a client never hears about \
             it: {touched:?}"
        );

        // And now take it away again.
        {
            let chunk = world
                .chunk(tiamot_core::domain::OVERWORLD, here, &mut Empty)
                .expect("chunk");
            chunk.set_block_local(
                tiamot_core::coords::LocalBlock::new(15, 8, 8),
                BlockValue::AIR,
            );
        }
        let touched = light.edited(tiamot_core::domain::OVERWORLD, &world, lamp);

        assert_eq!(
            light.at(across).red(),
            0,
            "the lamp went and its light stayed in the next chunk over: {:?}",
            light.at(across)
        );
        assert!(
            touched.contains(&next),
            "the neighbour went dark and was not reported: {touched:?}"
        );
    }
}
