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

use tiamot_core::fluid::FluidLayer;
use tiamot_core::light::{Light, LightLayer};
use tiamot_core::phys::ChunkLookup as _;
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
    /// Light levels, keyed by chunk.
    ///
    /// Separate from the chunks rather than a field on them, because the two
    /// arrive independently and either can outlive the other by a frame: light
    /// is its own message (protocol v8), and a lamp placed next door relights a
    /// chunk whose blocks did not change.
    light: BTreeMap<ChunkPos, LightLayer>,
    /// Fluid, for the chunks that have any.
    ///
    /// Absent rather than empty for a dry chunk, which is almost all of them:
    /// the map holds what there is milk in, so a world with one pond has one
    /// entry however far anybody walks.
    fluid: BTreeMap<ChunkPos, FluidLayer>,
    /// What each registered fluid is drawn as, indexed by fluid id.
    ///
    /// Zero means "no fluid registered under that id", which is both the
    /// untouched state and the honest answer for a payload naming a fluid this
    /// client was never told about.
    fluid_materials: [u16; tiamot_core::fluid::MAX_FLUIDS + 1],
    /// How deep each level of each fluid sits, in twenty-sevenths.
    ///
    /// Sent by the server rather than recomputed here — see
    /// `tiamot_core::proto::FluidDef`. Two sides disagreeing about where a
    /// surface is would show as milk at one height on screen and another under
    /// your feet.
    fluid_depths: [[u8; 8]; tiamot_core::fluid::MAX_FLUIDS + 1],
    /// What each fluid looks like from inside, as `0..=1` per channel.
    ///
    /// **The same space the sky's colours are in**, because this is fed to the
    /// same `set_sky` they are — a fluid tinted in a different space would be a
    /// different colour on screen from the one the mod chose.
    fluid_colours: [[f32; 3]; tiamot_core::fluid::MAX_FLUIDS + 1],
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

    /// Stores a chunk's light, marking it for remeshing.
    ///
    /// **Only the chunk itself, not its neighbours.** Light is baked into
    /// vertex colours, so a level changing means this chunk's mesh is stale —
    /// but the neighbours' meshes sample their own light and are unaffected.
    /// Marking them too would triple the remesh cost of every lamp for no
    /// visible difference, and lamps are placed constantly.
    ///
    /// Light for a chunk that is not held is kept anyway: the server sends it
    /// alongside the chunk rather than after it, and the two can arrive in
    /// either order.
    pub fn set_light(&mut self, pos: ChunkPos, layer: LightLayer) {
        self.light.insert(pos, layer);
        self.mark(pos);
        // **And its neighbours**, because smooth lighting reads across the
        // boundary: a face in the next chunk takes part of its corner light
        // from blocks in this one (see [`crate::shade`]). Marking only this
        // chunk was reported from the window as a lamp placed near a seam
        // lighting one side of it and not the other, and as its glow staying
        // behind on the far side when the lamp was broken. The light values
        // were right throughout — the server's own tests and a bot over the
        // wire both agreed — and the stale thing was the neighbour's mesh.
        //
        // Six extra chunks queued per light update, which sounds worse than it
        // is: the queue is bounded per frame by `REMESH_TIME_BUDGET`, a chunk
        // already dirty stays one entry, and during streaming the neighbours
        // are arriving and being marked anyway.
        self.mark_neighbours(pos);
    }

    /// Takes the fluid table the server sent on join.
    ///
    /// Everything already meshed is marked dirty, because until this arrives a
    /// chunk holding milk has no idea what to draw it as and meshed it dry. In
    /// practice the table lands before the first chunk, so this marks nothing —
    /// which is the point of doing it anyway rather than relying on the order.
    pub fn set_fluid_table(&mut self, fluids: &[tiamot_core::proto::FluidDef]) {
        self.fluid_materials = [0; tiamot_core::fluid::MAX_FLUIDS + 1];
        self.fluid_depths = [[0; 8]; tiamot_core::fluid::MAX_FLUIDS + 1];
        self.fluid_colours = [[1.0; 3]; tiamot_core::fluid::MAX_FLUIDS + 1];
        for def in fluids {
            let Some(slot) = usize::from(def.id).checked_sub(0) else {
                continue;
            };
            if slot >= self.fluid_materials.len() {
                continue;
            }
            self.fluid_materials[slot] = def.material;
            self.fluid_depths[slot] = def.depths;
            self.fluid_colours[slot] = def.color.map(|channel| f32::from(channel) / 255.0);
        }
        if !self.fluid.is_empty() {
            self.mark_all_dirty();
        }
    }

    /// Every fluid source the client holds, in world blocks.
    ///
    /// **Temporary, for tracking sources while Task 11 is built.** A source and
    /// a full flow block are the same colour and the same height, so from
    /// inside a pond there is no way to see which block is feeding it. Walks
    /// every held fluid layer, which is cheap only because dry chunks hold no
    /// layer at all — a world with one pond walks one chunk.
    #[must_use]
    pub fn fluid_sources(&self) -> Vec<tiamot_core::BlockPos> {
        let span = tiamot_core::CHUNK_BLOCKS as i32;
        let mut out = Vec::new();
        for (pos, layer) in &self.fluid {
            for (index, value) in layer.blocks().enumerate() {
                if !value.is_source() {
                    continue;
                }
                let local = tiamot_core::coords::LocalBlock::from_index(index);
                out.push(tiamot_core::BlockPos::new(
                    pos.x * span + local.x as i32,
                    pos.y * span + local.y as i32,
                    pos.z * span + local.z as i32,
                ));
            }
        }
        out
    }

    /// What being inside a fluid looks like, `0..=1` per channel.
    ///
    /// White for a fluid this client was never told about, which is the same
    /// answer as no tint at all rather than a black screen.
    #[must_use]
    pub fn fluid_colour(&self, fluid: tiamot_core::fluid::FluidId) -> [f32; 3] {
        self.fluid_colours
            .get(usize::from(fluid.0))
            .copied()
            .unwrap_or([1.0; 3])
    }

    /// The fluid in one chunk, as the mesher wants it.
    ///
    /// Returns something that answers per block, so a remesh does one map
    /// lookup rather than 4,096.
    #[must_use]
    pub fn fluid_for(&self, pos: ChunkPos) -> ChunkFluid<'_> {
        ChunkFluid { store: self, pos }
    }

    /// What one block of fluid is drawn as, and how deep it sits.
    ///
    /// `None` for an empty block, and for a fluid this client was never told
    /// about — which is a server sending a fluid it did not register, and is
    /// drawn as nothing rather than guessed at.
    #[must_use]
    pub fn fluid_fill(&self, value: tiamot_core::fluid::Fluid) -> Option<(u16, u8)> {
        if value.is_empty() {
            return None;
        }
        let slot = usize::from(value.fluid().0);
        let material = *self.fluid_materials.get(slot)?;
        if material == 0 {
            return None;
        }
        let depth = *self
            .fluid_depths
            .get(slot)?
            .get(usize::from(value.level()))?;
        (depth > 0).then_some((material, depth))
    }

    /// Takes a chunk's fluid, as a whole layer.
    ///
    /// Every update is a full layer rather than a delta (see
    /// `ServerMessage::ChunkFluid`), so this replaces rather than merges — and
    /// an empty layer is the server saying the pond drained, which has to
    /// replace what is held or the milk never goes away.
    ///
    /// Neighbours are marked for the same reason light marks them: a fluid
    /// surface takes its corner heights from the blocks across a chunk
    /// boundary, so a pond that ends at a seam would otherwise be smooth on one
    /// side of it and a step on the other.
    pub fn set_fluid(&mut self, pos: ChunkPos, layer: FluidLayer) {
        if layer.is_empty() {
            self.fluid.remove(&pos);
        } else {
            self.fluid.insert(pos, layer);
        }
        self.mark(pos);
        self.mark_neighbours(pos);
    }

    /// What a block holds, or nothing where no layer has arrived.
    ///
    /// Nothing rather than an error for the absent case: a chunk whose fluid
    /// has not arrived draws dry for a frame, which is the right way round —
    /// inventing milk that is not there would be visible and wrong, and the
    /// keyframe that follows corrects it either way.
    #[must_use]
    pub fn fluid_at(&self, pos: tiamot_core::BlockPos) -> tiamot_core::fluid::Fluid {
        self.fluid
            .get(&pos.chunk())
            .map_or(tiamot_core::fluid::Fluid::EMPTY, |layer| {
                layer.get(pos.local())
            })
    }

    /// Whether any chunk holds fluid at all.
    ///
    /// A world with no milk in it should cost the mesher nothing, and this is
    /// what lets it skip the fluid pass entirely rather than walking every
    /// block to discover there is none.
    #[must_use]
    pub fn has_fluid(&self) -> bool {
        !self.fluid.is_empty()
    }

    /// The light level at a block, or [`Light::DARK`] where nothing is held.
    ///
    /// Dark rather than daylight for the absent case, and it matters which:
    /// a chunk whose light has not arrived yet renders dark for a frame, where
    /// guessing daylight would flash the inside of a cave white as it streamed
    /// in.
    #[must_use]
    pub fn light_at(&self, pos: tiamot_core::BlockPos) -> Light {
        self.light
            .get(&pos.chunk())
            .map_or(Light::DARK, |layer| layer.get(pos.local()))
    }

    /// A light sampler for meshing one chunk.
    ///
    /// Reads across chunk boundaries, which is what smooth lighting at a seam
    /// needs. A neighbour whose light has not arrived reads as dark rather than
    /// as daylight — the honest answer, and the safe one: the faces against an
    /// absent neighbour are not drawn at all (see [`Absent`]), so the dark only
    /// ever applies where a real, loaded, genuinely dark chunk is.
    #[must_use]
    pub const fn light_for(&self, pos: ChunkPos) -> ChunkLight<'_> {
        ChunkLight { store: self, pos }
    }

    /// Whether any light has arrived for a chunk.
    #[must_use]
    pub fn has_light(&self, pos: ChunkPos) -> bool {
        self.light.contains_key(&pos)
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
        // Light goes with the chunk. Keeping it would be a slow leak across a
        // session of walking, and stale light for a chunk that comes back is
        // worse than none — the server sends fresh light with it.
        self.light.remove(&pos);
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
        self.light.clear();
    }

    /// Applies a server edit to the local copy.
    ///
    /// Returns whether it landed. An edit for a chunk the client does not hold
    /// is dropped rather than buffered: the chunk is outside the interest set,
    /// and when it comes back it comes back with the edit already in it.
    pub fn apply(&mut self, edit: &Edit) -> bool {
        match *edit {
            Edit::Block { pos, material } => {
                self.write_block(pos, BlockValue::Uniform(MaterialId(material)))
            }
            Edit::SubNode { pos, material } => self.set_subnode(pos, MaterialId(material)),
            Edit::Partial {
                pos,
                material,
                occupancy,
            } => self.write_block(
                pos,
                // The same normalisation the server does: a fully-set mask is
                // a `Uniform` block. Storing it as a `Partial` with 27 cells
                // would be the same geometry in a different representation, and
                // the two would then mesh from different code paths for a
                // difference nobody can see.
                if occupancy == (1 << tiamot_core::UNITS_PER_BLOCK) - 1 {
                    BlockValue::Uniform(MaterialId(material))
                } else {
                    BlockValue::Partial {
                        material: MaterialId(material),
                        occupancy,
                    }
                },
            ),
        }
    }

    fn write_block(&mut self, pos: BlockPos, value: BlockValue) -> bool {
        let chunk_pos = pos.chunk();
        let Some(chunk) = self.chunks.get_mut(&chunk_pos) else {
            return false;
        };
        if chunk.set_block(pos, value).is_err() {
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

    /// Puts chunks a remesh did not get to back in the queue.
    ///
    /// The other half of a [`ChunkStore::take_dirty`] that was abandoned part
    /// way through — without it, giving up on a frame's remaining chunks would
    /// silently drop them and leave holes in the world that nothing ever
    /// rebuilt.
    pub fn requeue(&mut self, positions: &[ChunkPos]) {
        self.dirty.extend(positions.iter().copied());
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

/// And lets it float in the milk it has been sent.
///
/// A second trait rather than a method on the first because the server keeps
/// its geometry and its fluid in different places behind different locks. The
/// client keeps both here, so it implements both on the one store and hands it
/// to `phys::Voxels::with_fluid` twice — which is the arrangement that makes a
/// client's prediction of a swim agree with the server's answer instead of
/// approximating it.
impl tiamot_core::phys::FluidLookup for ChunkStore {
    fn fluid_layer(&self, pos: ChunkPos) -> Option<&FluidLayer> {
        self.fluid.get(&pos)
    }
}

/// One chunk's view of the light field, in chunk-local block coordinates.
///
/// Coordinates outside `0..16` reach into the neighbours, which is ordinary:
/// a vertex on a chunk's edge samples blocks on both sides of it.
#[derive(Debug, Clone, Copy)]
pub struct ChunkLight<'a> {
    store: &'a ChunkStore,
    pos: ChunkPos,
}

impl crate::shade::BlockLight for ChunkLight<'_> {
    fn at(&self, x: i32, y: i32, z: i32) -> Light {
        let corner = BlockPos::from_chunk_corner(self.pos);
        self.store
            .light_at(BlockPos::new(corner.x + x, corner.y + y, corner.z + z))
    }
}

/// One chunk's fluid, as [`crate::mesher::FluidFill`] asks for it.
///
/// Borrows the store rather than copying a layer: a remesh is on the frame
/// loop, and 4 KiB memcpy per chunk per remesh is a cost with nothing to show
/// for it.
pub struct ChunkFluid<'a> {
    store: &'a ChunkStore,
    pos: ChunkPos,
}

impl crate::mesher::FluidFill for ChunkFluid<'_> {
    /// Whether a dry block is terrain the milk is held in by.
    ///
    /// The store rather than the mesher's own occupancy, because the mesher
    /// only has the chunk it is meshing and this has to answer the same way for
    /// the same block from either side of a seam — see `FluidFill::solid`.
    ///
    /// A block the client has not been sent counts as a wall: a shoreline that
    /// tapered into terrain that simply has not arrived would rise back up the
    /// moment it did, which is a visible flicker along the edge of the streamed
    /// world.
    fn solid(&self, x: i32, y: i32, z: i32) -> bool {
        let span = tiamot_core::CHUNK_BLOCKS as i32;
        let at = tiamot_core::BlockPos::new(
            self.pos.x * span + x,
            self.pos.y * span + y,
            self.pos.z * span + z,
        );
        self.store
            .chunk(at.chunk())
            .is_none_or(|chunk| chunk.get_block(at).is_some_and(|block| !block.is_empty()))
    }

    fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
        let span = tiamot_core::CHUNK_BLOCKS as i32;
        let at = tiamot_core::BlockPos::new(
            self.pos.x * span + x,
            self.pos.y * span + y,
            self.pos.z * span + z,
        );
        let (material, depth) = self.store.fluid_fill(self.store.fluid_at(at))?;

        // **A block with fluid above it is FULL, whatever its own level says.**
        //
        // Without this a column of milk draws as a stack of slabs with an air
        // gap between each — the surface rule applied to a block that has no
        // surface, because the block above is milk too. The same rule every
        // fluid renderer has, and the reason a waterfall reads as a column.
        let above = tiamot_core::BlockPos::new(at.x, at.y + 1, at.z);
        if !self.store.fluid_at(above).is_empty() {
            return Some((
                material,
                u8::try_from(tiamot_core::UNITS_PER_BLOCK).unwrap_or(u8::MAX),
            ));
        }
        Some((material, depth))
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
    fn a_chunk_can_see_one_block_of_its_neighbours_milk() {
        // **The wiring behind `FluidFill`'s one block of overlap.**
        //
        // The mesher culls a fluid face at a chunk's edge against the block
        // beyond it, and that block belongs to the chunk next door. Nothing in
        // the mesher can fetch it — the store can, because a fluid lookup is by
        // world position and always could have been. This is the half that says
        // `ChunkFluid` actually answers for coordinates outside its own chunk,
        // which is what the mesher's seam test assumes and cannot check.
        use tiamot_core::coords::LocalBlock;
        use tiamot_core::fluid::{Fluid, FluidId, FluidLayer};

        let mut store = ChunkStore::new();
        store.set_fluid_table(&[tiamot_core::proto::FluidDef {
            id: 1,
            name: "test:milk".into(),
            material: STONE.get(),
            depths: [0, 3, 6, 10, 13, 17, 20, 24],
            color: [255, 255, 255],
        }]);

        // Milk in the neighbour only, in the block against the shared face.
        let mut layer = FluidLayer::empty();
        let id = FluidId(1);
        layer.set(LocalBlock::new(0, 4, 4), Fluid::source(id));
        store.set_fluid(ChunkPos::new(1, 0, 0), layer);

        let fluid = store.fluid_for(ChunkPos::new(0, 0, 0));
        let span = tiamot_core::CHUNK_BLOCKS as i32;
        assert!(
            crate::mesher::FluidFill::fill(&fluid, span, 4, 4).is_some(),
            "chunk 0 cannot see the milk one block past its own edge, so it \
             will draw a wall of faces down the seam against it"
        );
        assert!(
            crate::mesher::FluidFill::fill(&fluid, span - 1, 4, 4).is_none(),
            "chunk 0 reports milk in its own last block, which is dry"
        );
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
    fn a_remesh_that_runs_out_of_time_puts_the_rest_back() {
        // `take_dirty` REMOVES what it hands out, so a caller that abandons the
        // frame part way through owns those positions. Dropping them loses the
        // chunks for ever — no further edit is coming to re-dirty them — and the
        // symptom is a permanent hole in the world rather than a slow one.
        let mut store = ChunkStore::new();
        for x in 0..6 {
            store.insert(chunk_at(x, 0, 0));
        }

        let due = store.take_dirty(ChunkPos::new(0, 0, 0), 6);
        assert_eq!(due.len(), 6);
        assert_eq!(store.dirty_len(), 0, "take_dirty must hand over ownership");

        // One rebuilt, five abandoned.
        store.requeue(&due[1..]);
        assert_eq!(store.dirty_len(), 5);

        let rest = store.take_dirty(ChunkPos::new(0, 0, 0), 8);
        assert_eq!(
            rest.len(),
            5,
            "the abandoned chunks did not come back and will never be remeshed"
        );
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
            &crate::shade::Uniform(Light::DAYLIGHT),
            &crate::mesher::NoFluid,
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

    #[test]
    fn light_arriving_for_one_chunk_dirties_the_neighbours_that_sample_it() {
        // Reported from the window: a lamp placed near a chunk boundary lit its
        // own chunk and left the next one looking unchanged, and removing it
        // left the glow behind on the far side of the seam.
        //
        // The light values were right the whole time — the server's own tests
        // and a bot over the wire both agree. What was wrong is here: **smooth
        // lighting samples across the boundary**, so a face in the NEXT chunk
        // takes part of its corner light from blocks in this one. Change this
        // chunk's light and that face is stale, and nothing was marking it.
        let mut store = ChunkStore::new();
        store.insert(chunk_at(0, 0, 0));
        store.insert(chunk_at(1, 0, 0));
        let _ = store.take_dirty(ChunkPos::new(0, 0, 0), 8);
        assert_eq!(store.dirty_len(), 0, "the fixture starts clean");

        // A layer that is not what the neighbour last assumed.
        let mut layer = LightLayer::dark();
        layer.set(
            tiamot_core::coords::LocalBlock::new(15, 8, 8),
            Light::new(0, 15, 0, 0),
        );
        store.set_light(ChunkPos::new(0, 0, 0), layer);

        let due = store.take_dirty(ChunkPos::new(0, 0, 0), 8);
        assert!(
            due.contains(&ChunkPos::new(1, 0, 0)),
            "the neighbour was not remeshed, so its seam still shows the old light: {due:?}"
        );
        assert!(
            due.contains(&ChunkPos::new(0, 0, 0)),
            "the chunk whose light changed was not remeshed either: {due:?}"
        );
    }
}
