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
}

impl<'a, S: ChunkLookup> Voxels<'a, S> {
    /// Views `source` with the frame anchored at `origin`'s corner.
    pub const fn new(source: &'a S, origin: ChunkPos) -> Self {
        Self { source, origin }
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
        self.source.chunk(chunk)?.get_subnode(world)
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
}
