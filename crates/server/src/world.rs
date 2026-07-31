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
//! space; generating it through the Task 04 path means a fresh chunk is the
//! same on every server that shares the seed.

use std::collections::HashMap;

use tiamot_core::proto::Edit;
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

/// The live world.
pub struct World {
    db: WorldDb,
    /// Chunks currently in memory.
    cache: HashMap<ChunkPos, Chunk>,
    /// Chunks changed since the last save.
    ///
    /// Separate from the cache so a save iterates only what moved. A server
    /// holding ten thousand chunks typically dirties a handful per tick.
    dirty: Vec<ChunkPos>,
}

impl World {
    /// Wraps a database.
    #[must_use]
    pub fn new(db: WorldDb) -> Self {
        Self {
            db,
            cache: HashMap::new(),
            dirty: Vec::new(),
        }
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
    pub fn chunk(&mut self, pos: ChunkPos) -> Result<&mut Chunk, WorldError> {
        if !self.cache.contains_key(&pos) {
            let chunk = match self.db.load_chunk(pos)? {
                Some(chunk) => chunk,
                // Never visited. Worldgen (Task 04) fills this in; until it
                // is wired to the tick loop a fresh chunk is air, which is at
                // least consistent rather than a hole in a generated surface.
                None => Chunk::new(pos, MaterialId::AIR),
            };
            self.cache.insert(pos, chunk);
        }
        Ok(self
            .cache
            .get_mut(&pos)
            .expect("just inserted if it was absent"))
    }

    /// Applies one edit and returns the chunk it touched.
    ///
    /// # Errors
    ///
    /// [`EditError`] if the material is unknown or the chunk is unreachable.
    pub fn apply(&mut self, edit: &Edit) -> Result<ChunkPos, EditError> {
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
            .chunk(chunk_pos)
            .map_err(|source| EditError::Unreachable {
                pos: chunk_pos,
                source: Box::new(source),
            })?;

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

        if !self.dirty.contains(&chunk_pos) {
            self.dirty.push(chunk_pos);
        }
        Ok(chunk_pos)
    }

    /// The material filling a block, if it is uniform.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk cannot be reached.
    pub fn block_material(&mut self, pos: BlockPos) -> Result<MaterialId, WorldError> {
        // Sub-node zero, which for a uniform block is the whole block. A caller
        // that needs the full contents asks for the chunk.
        Ok(self
            .chunk(pos.chunk())?
            .get_block(pos)
            .map_or(MaterialId::AIR, |view| view.subnode(0)))
    }

    /// Reads one sub-node's material.
    ///
    /// # Errors
    ///
    /// [`WorldError`] if the chunk cannot be reached.
    pub fn subnode(&mut self, pos: SubNodePos) -> Result<MaterialId, WorldError> {
        Ok(self
            .chunk(pos.chunk())?
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
        (World::new(db), ids)
    }

    #[test]
    fn an_edit_marks_its_chunk_dirty() {
        let (mut world, ids) = world("dirty");
        let pos = BlockPos::new(3, 4, 5);

        world
            .apply(&Edit::Block {
                pos,
                material: ids[0].0,
            })
            .expect("apply");

        assert_eq!(world.dirty(), 1);
        assert_eq!(world.block_material(pos).expect("read"), ids[0]);
    }

    #[test]
    fn two_edits_to_one_chunk_dirty_it_once() {
        // Otherwise a busy chunk is written once per edit rather than once per
        // save, and a player chiselling gets a write amplification of 27.
        let (mut world, ids) = world("dirty-once");
        for x in 0..5 {
            world
                .apply(&Edit::Block {
                    pos: BlockPos::new(x, 0, 0),
                    material: ids[0].0,
                })
                .expect("apply");
        }
        assert_eq!(world.dirty(), 1, "all five blocks are in one chunk");
    }

    #[test]
    fn an_unknown_material_is_refused_without_loading_a_chunk() {
        // The cheap-memory-exhaustion case: a peer spraying edits with nonsense
        // ids must not make the server generate a chunk per message.
        let (mut world, _) = world("unknown-material");
        let before = world.cached();

        let err = world
            .apply(&Edit::Block {
                pos: BlockPos::new(9999, 0, 9999),
                material: 60_000,
            })
            .expect_err("must refuse");

        assert!(matches!(err, EditError::UnknownMaterial { id: 60_000 }));
        assert_eq!(
            world.cached(),
            before,
            "a refused edit must not have loaded a chunk"
        );
        assert_eq!(world.dirty(), 0);
    }

    #[test]
    fn a_material_the_world_does_not_map_is_refused_rather_than_failing_at_save() {
        // The bug this check exists for. An id that passes validation but has
        // no row in the world's id map applies cleanly to the chunk and then
        // fails at save time — the edit looks like it worked and disappears on
        // restart. Validating against the same map the save uses means an
        // accepted edit is a saveable edit.
        let (mut world, ids) = world("unmapped");
        let unregistered = MaterialId(ids.last().expect("ids").0 + 1);

        let err = world
            .apply(&Edit::Block {
                pos: BlockPos::new(0, 0, 0),
                material: unregistered.0,
            })
            .expect_err("an unmapped material must be refused up front");
        assert!(matches!(err, EditError::UnknownMaterial { .. }), "{err}");

        // And every material that IS accepted must survive a save.
        for id in &ids {
            world
                .apply(&Edit::Block {
                    pos: BlockPos::new(0, 0, 0),
                    material: id.0,
                })
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
        let pos = BlockPos::new(1, 2, 3);
        world
            .apply(&Edit::Block {
                pos,
                material: ids[1].0,
            })
            .expect("apply");

        assert_eq!(world.save_dirty().expect("save"), 1);
        assert_eq!(world.dirty(), 0, "a save clears the dirty set");

        // Drop the cache and read it back from the database.
        world.cache.clear();
        assert_eq!(
            world.block_material(pos).expect("read"),
            ids[1],
            "the edit must have reached the database"
        );
    }

    #[test]
    fn a_subnode_edit_round_trips() {
        // Sub-node resolution is the whole point of the engine; an edit path
        // that only handled whole blocks would be half a feature.
        let (mut world, ids) = world("subnode");
        let pos = SubNodePos::new(4, 5, 6);
        world
            .apply(&Edit::SubNode {
                pos,
                material: ids[2].0,
            })
            .expect("apply");
        world.save_dirty().expect("save");
        world.cache.clear();

        assert_eq!(
            world.subnode(pos).expect("read"),
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
        let target = SubNodePos::new(3, 3, 3);
        world
            .apply(&Edit::Block {
                pos: target.block(),
                material: ids[0].0,
            })
            .expect("fill the block");
        world
            .apply(&Edit::SubNode {
                pos: target,
                material: ids[3].0,
            })
            .expect("chisel one cell");
        world.save_dirty().expect("save");
        world.cache.clear();

        assert_eq!(world.subnode(target).expect("read"), ids[3]);
        assert_eq!(
            world
                .subnode(SubNodePos::new(target.x + 1, target.y, target.z))
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
