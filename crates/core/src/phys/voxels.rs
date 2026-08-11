// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The bridge from [`Solid`] to real chunks.
//!
//! [`super`] deliberately knows nothing about chunks — it collides against an
//! integer lattice in a frame the caller chooses. This is the adapter that
//! makes that lattice the world: it anchors the frame to a chunk and answers
//! solidity out of whatever the caller is holding chunks in.
//!
//! # Why the frame is anchored to a chunk
//!
//! Charter rule 7: authoritative positions are `(i32 chunk, f32 local)` and
//! world-space `f32` is never accumulated. A body's coordinates are therefore
//! cells relative to its **origin chunk's corner**, which keeps them inside
//! `0..48` however far from the origin the player is — and keeps the physics
//! working at 120,000 blocks out with the same precision it has at spawn.
//!
//! # Who owns the chunks
//!
//! Neither the server's world nor the client's store, but a trait either can
//! implement. The same physics has to run in both — that is what makes client
//! prediction agree with the server — and the two keep chunks in different
//! places for good reasons.

use crate::chunk::Chunk;
use crate::coords::{ChunkPos, SubNodePos};
use crate::material::MaterialId;

use super::Solid;

/// Somewhere already-loaded chunks can be looked up.
///
/// Deliberately **not** named `ChunkSource` — the server already has a trait by
/// that name and it means the opposite thing: a terrain *generator*, which takes
/// `&mut self` and makes a chunk that did not exist. This one only reads what
/// is already resident, which is what a `&self` collision query can use.
///
/// Nothing here generates. Collision that could generate terrain would make a
/// player's movement pull chunks into memory, on the tick thread, at whatever
/// rate they walk.
pub trait ChunkLookup {
    /// The chunk at a position, or `None` if it is not resident.
    fn chunk(&self, pos: ChunkPos) -> Option<&Chunk>;
}

/// A [`Solid`] view of loaded chunks, in a frame anchored to `origin`.
pub struct Voxels<'a, S: ChunkLookup> {
    source: &'a S,
    /// The chunk whose corner is cell `[0, 0, 0]` in this frame.
    origin: ChunkPos,
    /// Whether any query has landed on a chunk that is not resident.
    ///
    /// **Purely diagnostic, and it changes no answer.** An absent chunk reads as
    /// solid either way; this only records that it happened, so a caller can
    /// tell "the body was stopped by the world" from "the body was stopped by
    /// part of the world that has not arrived". On a client those are very
    /// different: the second one is a wall the server does not have, and the
    /// divergence it causes arrives as a correction the player sees.
    absent: std::cell::Cell<bool>,
}

impl<'a, S: ChunkLookup> Voxels<'a, S> {
    /// Views `source` with the frame anchored at `origin`'s corner.
    pub const fn new(source: &'a S, origin: ChunkPos) -> Self {
        Self {
            source,
            origin,
            absent: std::cell::Cell::new(false),
        }
    }

    /// Whether any query since construction touched a chunk that is not
    /// resident.
    ///
    /// A client asks this after replaying a tick: if prediction consulted
    /// geometry it does not have, whatever it decided about being blocked is not
    /// what the server decided, and the correction that follows is explained
    /// rather than mysterious.
    #[must_use]
    pub fn touched_absent(&self) -> bool {
        self.absent.get()
    }

    /// The world sub-node position of a cell in this frame.
    #[must_use]
    pub const fn to_world(&self, x: i32, y: i32, z: i32) -> SubNodePos {
        let span = crate::CHUNK_SUBNODES as i32;
        SubNodePos::new(
            self.origin.x * span + x,
            self.origin.y * span + y,
            self.origin.z * span + z,
        )
    }

    /// The frame cell of a world sub-node position.
    #[must_use]
    pub const fn from_world(&self, pos: SubNodePos) -> [i32; 3] {
        let span = crate::CHUNK_SUBNODES as i32;
        [
            pos.x - self.origin.x * span,
            pos.y - self.origin.y * span,
            pos.z - self.origin.z * span,
        ]
    }

    /// The material at a frame cell, or `None` where nothing is loaded.
    #[must_use]
    pub fn material(&self, x: i32, y: i32, z: i32) -> Option<MaterialId> {
        let world = self.to_world(x, y, z);
        let span = crate::CHUNK_SUBNODES as i32;
        let chunk = ChunkPos::new(
            world.x.div_euclid(span),
            world.y.div_euclid(span),
            world.z.div_euclid(span),
        );
        let Some(chunk) = self.source.chunk(chunk) else {
            // Recorded, not acted on — see the field's docs. The answer this
            // produces is identical to what it always was.
            self.absent.set(true);
            return None;
        };
        chunk.get_subnode(world)
    }
}

impl<S: ChunkLookup> Solid for Voxels<'_, S> {
    /// Contract §2: solid iff the cell is occupied, whatever storage form the
    /// block uses — which is exactly what `get_subnode` answers, so `Uniform`,
    /// `Partial` and `Mixed` need no cases here.
    ///
    /// **An unloaded chunk is solid.** A player must not fall through a world
    /// that has not arrived yet; on the client that is a chunk still in flight,
    /// and on the server a chunk beyond what is kept resident. Treating absence
    /// as air makes the failure mode "fall out of the world", which is
    /// unrecoverable, rather than "stand still for a moment", which is not.
    fn solid(&self, x: i32, y: i32, z: i32) -> bool {
        self.material(x, y, z)
            .is_none_or(|material| !material.is_air())
    }

    /// Whether the chunk holding this cell is resident.
    ///
    /// The honest half of [`Solid::solid`]'s answer above: that one reports an
    /// absent chunk as solid on purpose, and this is how a caller that needs to
    /// know the difference finds out.
    fn loaded(&self, x: i32, y: i32, z: i32) -> bool {
        let world = self.to_world(x, y, z);
        let span = crate::CHUNK_SUBNODES as i32;
        let chunk = ChunkPos::new(
            world.x.div_euclid(span),
            world.y.div_euclid(span),
            world.z.div_euclid(span),
        );
        self.source.chunk(chunk).is_some()
    }
}

/// Moves a body's frame so its local coordinates stay inside one chunk.
///
/// Returns the anchor and local position describing the same place, with every
/// local axis in `0..48`. Call it after each step: without it a body walking
/// east accumulates a local coordinate that grows without bound, which is
/// precisely the world-space `f32` charter rule 7 forbids — and the precision
/// loss would arrive gradually, as movement getting coarser the further from
/// spawn a player walked.
///
/// The arithmetic is exact rather than approximately right: the shift is a
/// whole number of chunks, and `48 × small integer` is exactly representable,
/// so the returned position denotes the same point as the one passed in.
#[must_use]
pub fn renormalise(origin: ChunkPos, position: [f32; 3]) -> (ChunkPos, [f32; 3]) {
    let span = crate::CHUNK_SUBNODES as f32;
    let mut chunk = [origin.x, origin.y, origin.z];
    let mut local = position;

    for axis in 0..3 {
        let shift = crate::detgen::floor_to_i32(local[axis] / span);
        if shift != 0 {
            chunk[axis] += shift;
            local[axis] -= shift as f32 * span;
        }
    }

    (ChunkPos::new(chunk[0], chunk[1], chunk[2]), local)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::block::BlockValue;
    use crate::coords::BlockPos;
    use crate::phys::{Body, Intent, Tuning, step};

    const STONE: MaterialId = MaterialId(2);

    #[derive(Default)]
    struct Loaded(BTreeMap<ChunkPos, Chunk>);

    impl Loaded {
        /// A chunk filled to `height` blocks with stone, the rest air.
        fn with_floor(mut self, pos: ChunkPos, height: u32) -> Self {
            let mut chunk = Chunk::new(pos, MaterialId::AIR);
            let corner = BlockPos::from_chunk_corner(pos);
            for y in 0..height {
                for z in 0..crate::CHUNK_BLOCKS {
                    for x in 0..crate::CHUNK_BLOCKS {
                        chunk
                            .set_block(
                                BlockPos::new(
                                    corner.x + x as i32,
                                    corner.y + y as i32,
                                    corner.z + z as i32,
                                ),
                                BlockValue::Uniform(STONE),
                            )
                            .expect("in chunk");
                    }
                }
            }
            self.0.insert(pos, chunk);
            self
        }

        fn insert(mut self, chunk: Chunk) -> Self {
            self.0.insert(chunk.pos(), chunk);
            self
        }
    }

    impl ChunkLookup for Loaded {
        fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
            self.0.get(&pos)
        }
    }

    #[test]
    fn a_body_lands_on_a_chunks_floor() {
        let origin = ChunkPos::new(0, 0, 0);
        let world = Loaded::default().with_floor(origin, 1);
        let voxels = Voxels::new(&world, origin);

        // One block of floor is 3 cells, so its surface is at cell 3.
        let mut body = Body::at([24.0, 20.0, 24.0]);
        for _ in 0..60 {
            body = step(&voxels, body, Intent::default(), &Tuning::DEFAULT);
        }

        assert!(body.on_ground, "never landed: {body:?}");
        assert!(
            (body.position[1] - 3.0).abs() < 0.01,
            "landed at {} rather than on the floor at cell 3",
            body.position[1]
        );
    }

    #[test]
    fn a_body_at_a_chunk_boundary_is_not_shoved_by_the_chunk_next_door() {
        // **Reported from the window: "it almost feels like chunk boundaries have
        // their own collision", with a log full of pushes at local coordinates of
        // 0.x and 47.x — the two edges of a chunk, over and over, a full sub-node
        // each.**
        //
        // A body standing at a boundary reaches into the chunk across it, and an
        // absent chunk reads as solid so nobody falls out of a world in flight.
        // So a boundary whose neighbour has not arrived looks exactly like being
        // half inside a wall, and step-out obligingly shoved the body a sub-node
        // a tick to escape geometry that does not exist.
        //
        // Stopping a body against a chunk that may not be there costs a moment
        // standing still. MOVING it on the same guess costs a player their
        // position.
        let mut loaded = Loaded::default();
        let here = ChunkPos::new(0, 0, 0);
        let mut chunk = Chunk::new(here, MaterialId::AIR);
        // A floor across the bottom of the resident chunk, so the body has
        // something honest to stand on.
        for x in 0..crate::CHUNK_BLOCKS {
            for z in 0..crate::CHUNK_BLOCKS {
                chunk
                    .set_block(
                        BlockPos::new(x as i32, 0, z as i32),
                        BlockValue::Uniform(STONE),
                    )
                    .expect("in chunk");
            }
        }
        loaded.0.insert(here, chunk);
        // The chunk to the east is NOT inserted: it is still in flight.

        let span = crate::CHUNK_SUBNODES as f32;
        let voxels = Voxels::new(&loaded, here);
        let start = Body {
            // Hard against the eastern edge, exactly where the log's 47.x
            // positions sat, standing on the floor.
            position: [span - 0.5, 3.0, 24.0],
            velocity: [0.0; 3],
            on_ground: true,
        };

        let mut body = start;
        for tick in 0..8 {
            body = step(&voxels, body, Intent::default(), &Tuning::DEFAULT);
            for axis in 0..3 {
                assert!(
                    (body.position[axis] - start.position[axis]).abs() < crate::phys::SKIN * 4.0,
                    "tick {tick}: shoved from {:?} to {:?} by a chunk that has not arrived",
                    start.position,
                    body.position
                );
            }
        }
    }

    #[test]
    fn an_absent_chunk_is_reported_and_a_resident_one_is_not() {
        // The instrument behind the HUD's "(predicted into unloaded chunks)".
        // A correction with this set is the client having invented a wall the
        // server does not have; without it, the two simulations genuinely
        // disagree. They want opposite fixes, so the flag has to be right.
        let mut loaded = Loaded::default();
        let pos = ChunkPos::new(0, 0, 0);
        loaded.0.insert(pos, Chunk::new(pos, MaterialId::AIR));

        // Inside the one resident chunk: nothing to report.
        let voxels = Voxels::new(&loaded, pos);
        assert!(!voxels.solid(1, 1, 1), "air in a loaded chunk");
        assert!(
            !voxels.touched_absent(),
            "a query inside a resident chunk reported the world missing"
        );

        // One cell past its top face is the chunk above, which is not resident.
        let span = crate::CHUNK_SUBNODES as i32;
        assert!(
            voxels.solid(1, span, 1),
            "an absent chunk must still read as solid, or a body falls out of the world"
        );
        assert!(
            voxels.touched_absent(),
            "a query that landed in an absent chunk went unreported"
        );
    }

    #[test]
    fn an_unloaded_chunk_is_solid_so_a_body_cannot_fall_out_of_the_world() {
        // Nothing loaded at all.
        let world = Loaded::default();
        let voxels = Voxels::new(&world, ChunkPos::new(0, 0, 0));
        assert!(voxels.solid(0, 0, 0));

        let mut body = Body::at([24.0, 20.0, 24.0]);
        let before = body.position[1];
        for _ in 0..20 {
            body = step(&voxels, body, Intent::default(), &Tuning::DEFAULT);
        }
        assert_eq!(
            body.position[1].to_bits(),
            before.to_bits(),
            "fell through a world that has not arrived yet, reaching {}",
            body.position[1]
        );
    }

    #[test]
    fn a_partial_block_collides_at_the_cells_it_actually_occupies() {
        // The claim sub-nodes exist to make: a half-mined block is a shape.
        // The bottom layer of one block is left, the rest carved away.
        let origin = ChunkPos::new(0, 0, 0);
        let mut chunk = Chunk::new(origin, MaterialId::AIR);
        let target = BlockPos::new(1, 0, 1);
        // The 27-bit mask's bottom 3×3 layer: y = 0, all x and z.
        let mut occupancy = 0u32;
        for z in 0..3 {
            for x in 0..3 {
                occupancy |= 1 << crate::block::subnode_index(x, 0, z);
            }
        }
        chunk
            .set_block(
                target,
                BlockValue::Partial {
                    material: STONE,
                    occupancy,
                },
            )
            .expect("in chunk");

        let world = Loaded::default().insert(chunk);
        let voxels = Voxels::new(&world, origin);

        // Block (1, 0, 1) spans cells x 3..6, y 0..3, z 3..6.
        assert!(voxels.solid(3, 0, 3), "the occupied bottom layer is solid");
        assert!(voxels.solid(5, 0, 5), "and all of it, not just one corner");
        assert!(
            !voxels.solid(3, 1, 3),
            "the carved-away layer above must be air, or sub-nodes buy nothing"
        );
        assert!(!voxels.solid(3, 2, 3));
    }

    #[test]
    fn the_frame_round_trips_and_holds_up_at_the_edge_of_the_world() {
        // 120,000 blocks is 7,500 chunks; the far corner is what a naive
        // conversion overflows or loses precision on.
        for origin in [
            ChunkPos::new(0, 0, 0),
            ChunkPos::new(-1, -1, -1),
            ChunkPos::new(3_750, 100, -3_750),
        ] {
            let world = Loaded::default();
            let voxels = Voxels::new(&world, origin);
            for cell in [[0, 0, 0], [47, 47, 47], [-1, 5, 100]] {
                let round = voxels.from_world(voxels.to_world(cell[0], cell[1], cell[2]));
                assert_eq!(round, cell, "round trip failed at origin {origin:?}");
            }
        }
    }

    #[test]
    fn a_body_walks_from_one_chunk_into_the_next() {
        // The seam is where an adapter that forgot `div_euclid`, or that
        // assumed the body stays in its origin chunk, comes apart.
        let origin = ChunkPos::new(0, 0, 0);
        let world = Loaded::default()
            .with_floor(origin, 1)
            .with_floor(ChunkPos::new(1, 0, 0), 1);
        let voxels = Voxels::new(&world, origin);

        // Start near the +x edge of the origin chunk and walk across it.
        let mut body = Body {
            position: [44.0, 3.0, 24.0],
            velocity: [0.0, 0.0, 0.0],
            on_ground: true,
        };
        let intent = Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: crate::phys::Gait::Sprint,
        };
        for _ in 0..40 {
            body = step(&voxels, body, intent, &Tuning::DEFAULT);
        }

        assert!(
            body.position[0] > 48.0,
            "did not cross the chunk boundary at cell 48: {body:?}"
        );
        assert!(
            body.on_ground && (body.position[1] - 3.0).abs() < 0.01,
            "fell through the seam between chunks: {body:?}"
        );
    }

    #[test]
    fn renormalising_inside_a_chunk_changes_nothing() {
        let origin = ChunkPos::new(2, 0, -3);
        let (chunk, local) = renormalise(origin, [0.0, 24.5, 47.9]);
        assert_eq!(chunk, origin);
        assert_eq!(
            local.map(f32::to_bits),
            [0.0f32, 24.5, 47.9].map(f32::to_bits)
        );
    }

    #[test]
    fn renormalising_across_a_boundary_names_the_same_place() {
        // The property that matters is not "the numbers get smaller" but "the
        // point does not move". A frame shift that drifted by a fraction of a
        // cell would teleport a player very slightly every time they crossed a
        // chunk line — a rare, tiny, unreproducible jitter.
        let origin = ChunkPos::new(0, 0, 0);
        let span = crate::CHUNK_SUBNODES as f32;

        for position in [
            [48.5f32, 3.0, 0.0],
            [-0.5, 3.0, 0.0],
            [-49.0, 3.0, 96.25],
            [144.0, -1.0, -144.0],
        ] {
            let (chunk, local) = renormalise(origin, position);
            for axis in 0..3 {
                assert!(
                    local[axis] >= 0.0 && local[axis] < span,
                    "local {} is outside 0..{span} for {position:?}",
                    local[axis]
                );
                let chunk_axis = [chunk.x, chunk.y, chunk.z][axis];
                let world = chunk_axis as f32 * span + local[axis];
                assert_eq!(
                    world.to_bits(),
                    position[axis].to_bits(),
                    "axis {axis} moved: {position:?} became chunk {chunk:?} local {local:?}"
                );
            }
        }
    }

    #[test]
    fn a_body_walking_east_keeps_its_local_coordinate_small() {
        // The reason this exists. Without renormalising, a body walking away
        // from spawn accumulates a world-space f32 and loses precision as it
        // goes — charter rule 7's whole concern.
        let mut origin = ChunkPos::new(0, 0, 0);
        let mut position = [24.0f32, 3.0, 24.0];

        for _ in 0..1_000 {
            position[0] += 0.645;
            let (chunk, local) = renormalise(origin, position);
            origin = chunk;
            position = local;
        }

        assert!(
            position[0] >= 0.0 && position[0] < crate::CHUNK_SUBNODES as f32,
            "local x drifted to {}",
            position[0]
        );
        assert!(
            origin.x > 10,
            "the frame never followed the body: {origin:?}"
        );
    }
}
