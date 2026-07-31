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
