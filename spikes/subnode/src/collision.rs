// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deliverable 3 — the collision prototype.
//!
//! Swept-AABB against the sub-node grid: a throwaway version of what Task 09
//! will build properly. The question is only whether colliding at 1/3-yard
//! resolution is affordable for a server full of players, and whether stepping
//! up a single sub-node works.
//!
//! # Why axis-separated resolution
//!
//! Movement is resolved one axis at a time — X, then Y, then Z — rather than by
//! finding the true earliest time of impact in 3D. This is what most voxel
//! games do, and it is not a shortcut for its own sake: it makes sliding along
//! a wall fall out for free, and it makes step-up expressible as "retry the
//! horizontal move from one sub-node higher" rather than as a special case in a
//! swept solver.
//!
//! # A note on floats
//!
//! This prototype uses `f32` freely. Task 09's real implementation is
//! simulation code and must stay inside the Deterministic Float Subset (charter
//! rule 4) — no transcendentals, no `mul_add`. Nothing here needs any of them:
//! the arithmetic is add, multiply, compare, and floor, which are all in the
//! allowed subset already. That is a useful finding in itself.

use crate::mesher::{N, SubNodeGrid};
use crate::scenes::Rng;

/// Player width in sub-node units. 0.6 yards ≈ 1.8 sub-nodes.
pub const PLAYER_WIDTH: f32 = 1.8;
/// Player height in sub-node units. 1.8 yards = 5.4 sub-nodes.
pub const PLAYER_HEIGHT: f32 = 5.4;
/// One sub-node — the step-up height the design promises.
pub const STEP_HEIGHT: f32 = 1.0;

/// An axis-aligned box, in sub-node units.
#[derive(Debug, Clone, Copy)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    #[must_use]
    pub fn player_at(position: [f32; 3]) -> Self {
        let half = PLAYER_WIDTH / 2.0;
        Self {
            min: [position[0] - half, position[1], position[2] - half],
            max: [
                position[0] + half,
                position[1] + PLAYER_HEIGHT,
                position[2] + half,
            ],
        }
    }
}

/// A player being simulated.
#[derive(Debug, Clone, Copy)]
pub struct Body {
    pub position: [f32; 3],
    pub velocity: [f32; 3],
    pub on_ground: bool,
    /// Counts step-ups taken, so the prototype can report that the mechanic
    /// actually fires rather than merely compiling.
    pub steps: u32,
}

/// Solidity lookup over the sub-node grid.
pub struct Solid<'a> {
    grid: &'a SubNodeGrid,
}

impl<'a> Solid<'a> {
    #[must_use]
    pub fn new(grid: &'a SubNodeGrid) -> Self {
        Self { grid }
    }

    /// Whether a cell is solid. Outside the chunk is treated as empty except
    /// below the floor, so bodies cannot fall out of the world during a
    /// measurement run.
    #[must_use]
    pub fn at(&self, x: i32, y: i32, z: i32) -> bool {
        if y < 0 {
            return true;
        }
        if x < 0 || z < 0 || x >= N as i32 || y >= N as i32 || z >= N as i32 {
            return false;
        }
        self.grid.is_solid(x as usize, y as usize, z as usize)
    }

    /// Whether a box overlaps any solid cell.
    ///
    /// The hot path: called up to four times per body per tick. Its cost scales
    /// with the *volume of the player box in cells*, which is the number
    /// sub-node resolution multiplies — a player box is 2×6×2 cells here
    /// against 1×2×1 at block resolution.
    #[must_use]
    pub fn overlaps(&self, aabb: &Aabb) -> bool {
        let min_x = aabb.min[0].floor() as i32;
        let max_x = (aabb.max[0] - f32::EPSILON).floor() as i32;
        let min_y = aabb.min[1].floor() as i32;
        let max_y = (aabb.max[1] - f32::EPSILON).floor() as i32;
        let min_z = aabb.min[2].floor() as i32;
        let max_z = (aabb.max[2] - f32::EPSILON).floor() as i32;

        for y in min_y..=max_y {
            for z in min_z..=max_z {
                for x in min_x..=max_x {
                    if self.at(x, y, z) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Advances one body by one tick, resolving collisions.
///
/// Returns the body with position, velocity, and ground state updated.
pub fn step(solid: &Solid<'_>, mut body: Body, gravity: f32) -> Body {
    body.velocity[1] -= gravity;

    // Y first, so ground contact is known before the horizontal move decides
    // whether a step-up is allowed.
    let mut moved = body.position;
    moved[1] += body.velocity[1];
    if solid.overlaps(&Aabb::player_at(moved)) {
        body.on_ground = body.velocity[1] < 0.0;
        body.velocity[1] = 0.0;
    } else {
        body.position[1] = moved[1];
        body.on_ground = false;
    }

    // Horizontal, one axis at a time so a blocked X still allows Z — this is
    // what makes a body slide along a wall instead of sticking to it.
    for axis in [0usize, 2usize] {
        let mut moved = body.position;
        moved[axis] += body.velocity[axis];
        if !solid.overlaps(&Aabb::player_at(moved)) {
            body.position[axis] = moved[axis];
            continue;
        }

        // Blocked. Try again one sub-node higher: the 1/3-yard step-up.
        if body.on_ground {
            let mut stepped = moved;
            stepped[1] += STEP_HEIGHT;
            if !solid.overlaps(&Aabb::player_at(stepped)) {
                body.position[axis] = stepped[axis];
                body.position[1] = stepped[1];
                body.steps += 1;
                continue;
            }
        }

        body.velocity[axis] = 0.0;
    }

    body
}

/// Outcome of a collision measurement run.
#[derive(Debug, Clone, Copy)]
pub struct CollisionResult {
    pub bodies: usize,
    pub ticks: usize,
    pub step_ups: u32,
    /// Bodies that ended the run inside solid geometry. Must be zero.
    pub embedded: usize,
}

/// Runs `bodies` random-walking players for `ticks` ticks.
///
/// Random walk rather than scripted paths because the interesting cost is
/// bodies grinding along chiselled surfaces, and a scripted path would have to
/// be hand-tuned to find them.
pub fn simulate(
    grid: &SubNodeGrid,
    bodies: usize,
    ticks: usize,
    seed: u64,
    mut trace: Option<&mut Vec<[f32; 3]>>,
) -> CollisionResult {
    let solid = Solid::new(grid);
    let mut rng = Rng::new(seed);

    let mut population: Vec<Body> = (0..bodies)
        .map(|_| Body {
            // Spawn above the terrain surface so bodies fall into it and settle.
            position: [4.0 + rng.below(40) as f32, 30.0, 4.0 + rng.below(40) as f32],
            velocity: [0.0; 3],
            on_ground: false,
            steps: 0,
        })
        .collect();

    for _ in 0..ticks {
        for body in &mut population {
            // Re-aim occasionally rather than every tick, so bodies travel in
            // straight-ish lines and actually run into things.
            if rng.chance(10) {
                body.velocity[0] = (rng.below(200) as f32 - 100.0) / 200.0;
                body.velocity[2] = (rng.below(200) as f32 - 100.0) / 200.0;
            }
            *body = step(&solid, *body, 0.08);
        }
        if let Some(trace) = trace.as_deref_mut() {
            trace.push(population[0].position);
        }
    }

    CollisionResult {
        bodies,
        ticks,
        step_ups: population.iter().map(|body| body.steps).sum(),
        embedded: population
            .iter()
            .filter(|body| solid.overlaps(&Aabb::player_at(body.position)))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::{STONE, Scene};
    use tiamot_core::coords::SubNodePos;
    use tiamot_core::{Chunk, ChunkPos};

    fn grid_of(scene: Scene) -> SubNodeGrid {
        SubNodeGrid::from_chunk(&scene.build(0xC0111DE))
    }

    #[test]
    fn a_body_falls_and_lands_on_the_surface() {
        let grid = grid_of(Scene::Flat);
        let solid = Solid::new(&grid);
        let mut body = Body {
            position: [24.0, 40.0, 24.0],
            velocity: [0.0; 3],
            on_ground: false,
            steps: 0,
        };
        for _ in 0..400 {
            body = step(&solid, body, 0.08);
        }
        assert!(body.on_ground, "the body should have landed");
        assert!(
            !solid.overlaps(&Aabb::player_at(body.position)),
            "a landed body must not be inside the floor"
        );
        // The flat scene's surface is 8 blocks = 24 sub-nodes high.
        assert!(
            (body.position[1] - 24.0).abs() < 1.5,
            "landed at y={}, expected ~24",
            body.position[1]
        );
    }

    #[test]
    fn a_body_steps_up_one_subnode_but_not_two() {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        // A floor.
        for z in 0..N as i32 {
            for x in 0..N as i32 {
                chunk
                    .set_subnode(SubNodePos::new(x, 0, z), STONE)
                    .expect("in chunk");
            }
        }
        // A one-sub-node lip at x=20, and a two-sub-node wall at x=30.
        for z in 0..N as i32 {
            chunk
                .set_subnode(SubNodePos::new(20, 1, z), STONE)
                .expect("in chunk");
            chunk
                .set_subnode(SubNodePos::new(30, 1, z), STONE)
                .expect("in chunk");
            chunk
                .set_subnode(SubNodePos::new(30, 2, z), STONE)
                .expect("in chunk");
        }
        let grid = SubNodeGrid::from_chunk(&chunk);
        let solid = Solid::new(&grid);

        // Walk east into the one-sub-node lip.
        let mut body = Body {
            position: [17.0, 1.0, 24.0],
            velocity: [0.4, 0.0, 0.0],
            on_ground: true,
            steps: 0,
        };
        for _ in 0..40 {
            body.velocity[0] = 0.4;
            body = step(&solid, body, 0.08);
        }
        assert!(body.steps > 0, "the 1/3-yard lip should have been stepped");
        assert!(body.position[0] > 21.0, "should be past the lip");

        // The two-sub-node wall must stop it.
        for _ in 0..80 {
            body.velocity[0] = 0.4;
            body = step(&solid, body, 0.08);
        }
        assert!(
            body.position[0] < 30.0,
            "a two-sub-node wall must not be steppable, reached x={}",
            body.position[0]
        );
    }

    #[test]
    fn bodies_never_end_up_embedded_in_geometry() {
        // The property that matters: whatever the resolution does, a player
        // must never be left inside a wall.
        for scene in [Scene::Chiselled, Scene::Realistic] {
            let grid = grid_of(scene);
            let result = simulate(&grid, 16, 200, 99, None);
            assert_eq!(
                result.embedded,
                0,
                "{}: {} bodies ended inside geometry",
                scene.label(),
                result.embedded
            );
        }
    }

    #[test]
    fn simulation_is_deterministic() {
        let grid = grid_of(Scene::Realistic);
        let first = simulate(&grid, 8, 100, 5, None);
        let second = simulate(&grid, 8, 100, 5, None);
        assert_eq!(first.step_ups, second.step_ups);
    }
}
