// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The client's copy of the world, and the queue of chunks needing a remesh.
//!
//! # This is a cache, not a simulation
//!
//! Charter rule 2: the headless server is the game and the client is a viewer.
//! Nothing here decides anything — every chunk arrived as a `ChunkData` and
//! every change arrived as a `BlockDelta`. The client applies deltas to its own
//! copy rather than waiting for the chunk to be re-sent, which is a bandwidth
//! decision, not a simulation one: the server has already decided, and this is
//! the same decision arriving cheaply.
//!
//! # The dirty queue exists because meshing is not free
//!
//! A chunk arriving or changing invalidates its mesh. It also invalidates its
//! *neighbours'* meshes, because border faces are culled against them
//! (`mesher::Neighbours`) — a chunk meshed while its neighbour was missing hid
//! the faces it shares with it, and when the neighbour lands those faces are
//! either right or a hole.
//!
//! Marking all six neighbours on every edit would triple the remesh cost of
//! chiselling, so an edit marks a neighbour only when it actually touched the
//! plane they share. An edit in the middle of a chunk cannot change what its
//! neighbours draw.

use std::collections::{BTreeMap, BTreeSet};

use tiamot_core::proto::Edit;
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId, SubNodePos};

use crate::mesher::{Absent, Neighbours};

/// The six neighbour directions, in the order [`Neighbours`] expects.
///
/// Indexed by `axis * 2 + positive`, which is the mesher's convention. Keeping
/// one table rather than open-coding the offsets is what stops a transposed
/// neighbour — a bug that shows up as a seam on exactly one side of the world.
const NEIGHBOUR_OFFSETS: [(i32, i32, i32); 6] = [
    (-1, 0, 0),
    (1, 0, 0),
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
];

/// How an unloaded neighbour is treated when meshing.
///
/// [`Absent::Solid`]: a chunk that has not arrived is almost always a chunk
/// that exists, so drawing a wall of faces against it produces a shell around
/// the loaded region that pops away a moment later.
pub const ABSENT_POLICY: Absent = Absent::Solid;

/// The client's chunks, and which of them need remeshing.
#[derive(Debug, Default)]
pub struct ChunkStore {
    chunks: BTreeMap<ChunkPos, Chunk>,
    dirty: BTreeSet<ChunkPos>,
}

impl ChunkStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many chunks are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Whether no chunks are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// How many chunks are waiting to be remeshed.
    #[must_use]
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    /// The chunk at a position, if it is held.
    #[must_use]
    pub fn get(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.chunks.get(&pos)
    }

    /// Every held position, in a stable order.
    pub fn positions(&self) -> impl Iterator<Item = ChunkPos> + '_ {
        self.chunks.keys().copied()
    }

    /// Stores a chunk, marking it and its neighbours for remeshing.
    ///
    /// The neighbours matter as much as the chunk itself: each of them was
    /// meshed while this one was absent, and hid the faces it shares with it.
    pub fn insert(&mut self, chunk: Chunk) {
        let pos = chunk.pos();
        self.chunks.insert(pos, chunk);
        self.mark(pos);
        self.mark_neighbours(pos);
    }

    /// Drops a chunk, marking its neighbours for remeshing.
    ///
    /// Returns whether anything was held there.
    pub fn remove(&mut self, pos: ChunkPos) -> bool {
        let held = self.chunks.remove(&pos).is_some();
        self.dirty.remove(&pos);
        if held {
            self.mark_neighbours(pos);
        }
        held
    }

    /// Forgets everything.
    ///
    /// For a reconnection: the material ids in a new session are a different
    /// session's ids (charter rule 8), so carrying chunks across would render
    /// the old world with the new world's textures.
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.dirty.clear();
    }

    /// Applies a server edit to the local copy.
    ///
    /// Returns whether it landed. An edit for a chunk the client does not hold
    /// is dropped rather than buffered: the chunk is outside the interest set,
    /// and when it comes back it comes back with the edit already in it.
    pub fn apply(&mut self, edit: &Edit) -> bool {
        match *edit {
            Edit::Block { pos, material } => self.set_block(pos, MaterialId(material)),
            Edit::SubNode { pos, material } => self.set_subnode(pos, MaterialId(material)),
        }
    }

    fn set_block(&mut self, pos: BlockPos, material: MaterialId) -> bool {
        let chunk_pos = pos.chunk();
        let Some(chunk) = self.chunks.get_mut(&chunk_pos) else {
            return false;
        };
        if chunk.set_block(pos, BlockValue::Uniform(material)).is_err() {
            return false;
        }

        let local = pos.local();
        self.mark(chunk_pos);
        // A block spans three sub-node cells, so a block at local index 0 or 15
        // touches the chunk's own boundary plane on that axis.
        self.mark_touched_neighbours(
            chunk_pos,
            [local.x, local.y, local.z],
            tiamot_core::CHUNK_BLOCKS,
        );
        true
    }

    fn set_subnode(&mut self, pos: SubNodePos, material: MaterialId) -> bool {
        let chunk_pos = pos.chunk();
        let Some(chunk) = self.chunks.get_mut(&chunk_pos) else {
            return false;
        };
        if chunk.set_subnode(pos, material).is_err() {
            return false;
        }

        // NOT `SubNodePos::local()` — that is the offset within the *block*
        // (0..3), not within the chunk (0..48). Using it here marks a
        // neighbour whenever a chisel lands on a block's own edge, which is
        // most chisels, and the remesh budget goes on chunks nothing changed
        // in. Caught by `a_sub_node_edit_on_the_last_cell_dirties_the_neighbour`,
        // which failed the other way: the cell it edits is at block-local 2,
        // so the real border was never marked at all.
        let cells = tiamot_core::CHUNK_SUBNODES as i32;
        let local = [
            pos.x.rem_euclid(cells) as u32,
            pos.y.rem_euclid(cells) as u32,
            pos.z.rem_euclid(cells) as u32,
        ];
        self.mark(chunk_pos);
        self.mark_touched_neighbours(chunk_pos, local, tiamot_core::CHUNK_SUBNODES);
        true
    }

    /// Marks the neighbours whose shared plane an edit at `local` touched.
    ///
    /// `span` is the number of cells per axis in whichever resolution `local`
    /// is expressed in. Only the first and last cell on an axis can change what
    /// the neighbour across that axis draws.
    fn mark_touched_neighbours(&mut self, pos: ChunkPos, local: [u32; 3], span: u32) {
        for (axis, coordinate) in local.iter().enumerate() {
            if *coordinate == 0 {
                self.mark_offset(pos, axis, false);
            }
            if *coordinate == span - 1 {
                self.mark_offset(pos, axis, true);
            }
        }
    }

    fn mark_offset(&mut self, pos: ChunkPos, axis: usize, positive: bool) {
        let (dx, dy, dz) = NEIGHBOUR_OFFSETS[axis * 2 + usize::from(positive)];
        let neighbour = ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
        if self.chunks.contains_key(&neighbour) {
            self.dirty.insert(neighbour);
        }
    }

    /// Marks a position for remeshing, whether or not it is held.
    ///
    /// Held is checked on the way out rather than here: a chunk can be marked
    /// and then unloaded before the remesh runs.
    fn mark(&mut self, pos: ChunkPos) {
        self.dirty.insert(pos);
    }

    fn mark_neighbours(&mut self, pos: ChunkPos) {
        for (dx, dy, dz) in NEIGHBOUR_OFFSETS {
            let neighbour = ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
            if self.chunks.contains_key(&neighbour) {
                self.dirty.insert(neighbour);
            }
        }
    }

    /// The six chunks adjacent to `pos`, for border-aware meshing.
    #[must_use]
    pub fn neighbours(&self, pos: ChunkPos) -> Neighbours<'_> {
        let mut sides = [None; 6];
        for (index, (dx, dy, dz)) in NEIGHBOUR_OFFSETS.iter().enumerate() {
            sides[index] = self
                .chunks
                .get(&ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz));
        }
        Neighbours { sides }
    }

    /// Takes up to `budget` chunks to remesh, nearest to `centre` first.
    ///
    /// **Nearest first is the whole point of the budget.** A queue drained in
    /// arrival order spends a frame's remesh time on chunks behind the player
    /// while the block they just dug stays visibly unchanged in front of them.
    ///
    /// Positions no longer held are dropped here rather than when they were
    /// unloaded, so an unload never has to walk the queue.
    pub fn take_dirty(&mut self, centre: ChunkPos, budget: usize) -> Vec<ChunkPos> {
        if budget == 0 || self.dirty.is_empty() {
            return Vec::new();
        }

        let mut candidates: Vec<ChunkPos> = std::mem::take(&mut self.dirty)
            .into_iter()
            .filter(|pos| self.chunks.contains_key(pos))
            .collect();

        // Ties broken by position so the order is a property of the data
        // rather than of the allocator — the same reason the simulation avoids
        // `HashMap`, even though nothing here is part of the hash gate.
        candidates.sort_by_key(|pos| {
            (
                tiamot_core::interest::squared_distance(centre, *pos),
                pos.x,
                pos.y,
                pos.z,
            )
        });

        let overflow = candidates.split_off(candidates.len().min(budget));
        self.dirty.extend(overflow);
        candidates
    }

    /// Marks every held chunk for remeshing.
    ///
    /// For a render-mode change, where nothing about the world moved but every
    /// mesh has to be rebuilt.
    pub fn mark_all_dirty(&mut self) {
        self.dirty.extend(self.chunks.keys().copied());
    }
}

/// Lets the client's physics collide against the chunks it has been sent.
///
/// The same trait the server implements over its own store, so
/// `tiamot_core::phys` runs unchanged on both sides — which is what makes a
/// client's prediction agree with the server's answer rather than approximate
/// it. Chunks still in flight are absent here, and `Voxels` treats absent as
/// solid, so a player at the edge of what has arrived stops rather than
/// falling through the world.
impl tiamot_core::phys::ChunkLookup for ChunkStore {
    fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
        self.get(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::BlockPos;

    const STONE: MaterialId = MaterialId(2);

    fn chunk_at(x: i32, y: i32, z: i32) -> Chunk {
        Chunk::new(ChunkPos::new(x, y, z), MaterialId::AIR)
    }

    fn solid_at(x: i32, y: i32, z: i32) -> Chunk {
        Chunk::new(ChunkPos::new(x, y, z), STONE)
    }

    #[test]
    fn an_arriving_chunk_dirties_its_neighbours_too() {
        // The neighbours were meshed while this chunk was absent, and hid the
        // faces they share with it. Leaving them alone leaves a hole.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.insert(chunk_at(1, 0, 0));

        let taken = store.take_dirty(ChunkPos::new(0, 0, 0), 16);
        assert!(taken.contains(&ChunkPos::new(0, 0, 0)));
        assert!(
            taken.contains(&ChunkPos::new(1, 0, 0)),
            "the neighbour must be remeshed when a chunk lands beside it"
        );
    }

    #[test]
    fn an_edit_in_the_middle_of_a_chunk_does_not_dirty_its_neighbours() {
        // The optimisation the whole dirty-marking scheme exists for. Marking
        // all six neighbours on every chisel would triple the remesh cost of
        // the engine's headline interaction.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.insert(chunk_at(1, 0, 0));
        let _ = store.take_dirty(ChunkPos::new(0, 0, 0), 16);

        assert!(store.apply(&Edit::Block {
            pos: BlockPos::new(8, 8, 8),
            material: STONE.0,
        }));

        let taken = store.take_dirty(ChunkPos::new(0, 0, 0), 16);
        assert_eq!(taken, vec![ChunkPos::new(0, 0, 0)]);
    }

    #[test]
    fn an_edit_on_a_border_plane_dirties_the_chunk_across_it() {
        // And the case the optimisation must not break: an edit at the very
        // edge changes what the neighbour draws.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.insert(chunk_at(1, 0, 0));
        let _ = store.take_dirty(ChunkPos::new(0, 0, 0), 16);

        assert!(store.apply(&Edit::Block {
            pos: BlockPos::new(15, 8, 8),
            material: STONE.0,
        }));

        let taken = store.take_dirty(ChunkPos::new(0, 0, 0), 16);
        assert!(
            taken.contains(&ChunkPos::new(1, 0, 0)),
            "an edit on the shared plane must remesh the chunk across it: {taken:?}"
        );
    }

    #[test]
    fn a_sub_node_edit_on_the_last_cell_dirties_the_neighbour() {
        // The sub-node case is off by one from the block case — 48 cells, not
        // 16 — and getting it wrong leaves a one-cell seam that is only visible
        // when someone chisels exactly at a chunk boundary.
        let mut store = ChunkStore::new();
        store.insert(solid_at(0, 0, 0));
        store.insert(solid_at(1, 0, 0));
        let _ = store.take_dirty(ChunkPos::new(0, 0, 0), 16);

        assert!(store.apply(&Edit::SubNode {
            pos: SubNodePos::new(47, 20, 20),
            material: MaterialId::AIR.0,
        }));

        let taken = store.take_dirty(ChunkPos::new(0, 0, 0), 16);
        assert!(
            taken.contains(&ChunkPos::new(1, 0, 0)),
            "chiselling the last cell must remesh across the boundary: {taken:?}"
        );
    }

    #[test]
    fn an_edit_for_a_chunk_we_do_not_hold_is_dropped() {
        // Not buffered. The chunk is outside the interest set, and when it
        // comes back it comes back with the edit already applied.
        let mut store = ChunkStore::new();
        assert!(!store.apply(&Edit::Block {
            pos: BlockPos::new(8, 8, 8),
            material: STONE.0,
        }));
        assert_eq!(store.dirty_len(), 0);
    }

    #[test]
    fn the_remesh_budget_takes_the_nearest_chunks_first() {
        // A queue drained in arrival order spends the frame's remesh time
        // behind the player while the block they just dug stays unchanged in
        // front of them.
        let mut store = ChunkStore::new();
        for x in 0..6 {
            store.insert(chunk_at(x, 0, 0));
        }

        let taken = store.take_dirty(ChunkPos::new(5, 0, 0), 2);
        assert_eq!(taken.len(), 2);
        assert!(
            taken.contains(&ChunkPos::new(5, 0, 0)) && taken.contains(&ChunkPos::new(4, 0, 0)),
            "expected the two nearest chunks, got {taken:?}"
        );
    }

    #[test]
    fn what_the_budget_leaves_behind_stays_queued() {
        // The bug this guards against loses chunks permanently: they are
        // dropped from the queue, never remeshed, and the world has holes in it
        // that nothing will ever fix.
        let mut store = ChunkStore::new();
        for x in 0..6 {
            store.insert(chunk_at(x, 0, 0));
        }

        let mut seen = BTreeSet::new();
        for _ in 0..6 {
            seen.extend(store.take_dirty(ChunkPos::new(0, 0, 0), 2));
        }
        assert_eq!(seen.len(), 6, "every chunk must eventually be remeshed");
        assert_eq!(store.dirty_len(), 0);
    }

    #[test]
    fn an_unloaded_chunk_leaves_the_queue_without_being_hunted_down() {
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.insert(chunk_at(1, 0, 0));
        assert!(store.remove(ChunkPos::new(0, 0, 0)));

        let taken = store.take_dirty(ChunkPos::new(0, 0, 0), 16);
        assert!(
            !taken.contains(&ChunkPos::new(0, 0, 0)),
            "a chunk that is gone must not be remeshed: {taken:?}"
        );
        assert!(
            taken.contains(&ChunkPos::new(1, 0, 0)),
            "but its neighbour must be, or it keeps hiding the faces they shared"
        );
    }

    #[test]
    fn neighbours_are_reported_in_the_meshers_order() {
        // `Neighbours` is indexed by `axis * 2 + positive`. A transposed entry
        // here culls against the wrong chunk, and the symptom is a seam on one
        // side of the world only.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        for (dx, dy, dz) in NEIGHBOUR_OFFSETS {
            store.insert(chunk_at(dx, dy, dz));
        }

        let neighbours = store.neighbours(ChunkPos::new(0, 0, 0));
        for (index, (dx, dy, dz)) in NEIGHBOUR_OFFSETS.iter().enumerate() {
            let side = neighbours.sides[index].expect("every neighbour was inserted");
            assert_eq!(
                side.pos(),
                ChunkPos::new(*dx, *dy, *dz),
                "side {index} pointed at the wrong chunk"
            );
        }
    }

    #[test]
    fn a_missing_neighbour_hides_the_border_rather_than_walling_it_off() {
        // The policy this module fixes, checked against the mesher rather than
        // asserted in a comment: a lone chunk with no neighbours loaded must
        // draw nothing, so the loaded region has no shell around it.
        let mut store = ChunkStore::new();
        store.insert(solid_at(0, 0, 0));

        let chunk = store.get(ChunkPos::new(0, 0, 0)).expect("just inserted");
        let mesh = crate::mesher::mesh_chunk(
            chunk,
            &store.neighbours(ChunkPos::new(0, 0, 0)),
            ABSENT_POLICY,
        );
        assert!(
            mesh.is_empty(),
            "a solid chunk with nothing loaded around it should draw no shell, got {} quads",
            mesh.quads.len()
        );
    }

    #[test]
    fn clearing_the_store_forgets_the_queue_as_well() {
        // On reconnect the material ids are a different session's ids (charter
        // rule 8). A stale queue entry would remesh a chunk that is not there.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.dirty_len(), 0);
    }
}
