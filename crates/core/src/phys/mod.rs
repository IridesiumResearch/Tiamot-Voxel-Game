// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Swept-AABB collision against the sub-node grid.
//!
//! Implements **Sub-Node Contract §2**, quoted where each rule is enforced:
//! collision is solid at sub-node resolution, a cell is solid iff occupied,
//! step-up is one sub-node, movement resolves one axis at a time, and *a body
//! must never be left inside geometry*.
//!
//! # This is simulation code
//!
//! Charter rule 4 applies in full: no transcendentals, no `mul_add`, no `NaN`
//! in state, no `f32::floor` (it lowers to libm without SSE4.1 — this module
//! uses [`crate::detgen::floor_to_i32`]). Only `+ - * /`, `sqrt`, `abs` and
//! comparisons appear below, all of which Rust guarantees to be IEEE 754. The
//! same code runs on the server and inside the client's prediction, and
//! reconciliation is only correct because the two agree bit for bit.
//!
//! # Units and frame
//!
//! Everything here is in **sub-node cells**: one cell is 1/3 yard, and the
//! collision grid is therefore the integer lattice, which is what keeps the
//! sweep free of scale factors. A cell `c` spans `[c, c+1)`.
//!
//! Positions are frame-local `f32`, and the frame is the caller's business —
//! [`Solid`] receives the same integer cell coordinates the body is expressed
//! in. Charter rule 7 forbids accumulating a world-space `f32`, so a caller
//! holding a body across many ticks anchors the frame to a chunk and
//! renormalises as the body crosses into the next one; a caller simulating a
//! scene in a unit test just uses the scene's own coordinates.
//!
//! # Why one axis at a time
//!
//! Contract §2: "Movement resolves one axis at a time (X, then Y, then Z),
//! which is what makes a body slide along a wall rather than stick to it." It
//! also makes step-up expressible as "retry the horizontal move one cell
//! higher" rather than as a special case inside a 3D solver.

pub mod input;
pub mod ray;
pub mod tuning;
pub mod voxels;

use crate::detgen::floor_to_i32;

pub use input::InputQueue;
pub use ray::{Hit, REACH};
pub use tuning::{Gait, Tuning};
pub use voxels::{ChunkSource, Voxels};

/// Player box width, in cells. 0.6 yards.
pub const PLAYER_WIDTH: f32 = 1.8;

/// Player box height, in cells. 1.8 yards.
pub const PLAYER_HEIGHT: f32 = 5.4;

/// Eye height above the feet, in cells. 1.62 yards.
pub const EYE_HEIGHT: f32 = 4.86;

/// How far a body is kept clear of the geometry it lands against, in cells.
///
/// A body placed exactly flush on a boundary sits at a coordinate that is
/// simultaneously the last of one cell and the first of the next, so the very
/// next overlap test can find it inside the floor it just landed on. Backing
/// off by a sliver makes "resting on" and "inside" distinguishable.
///
/// 1/1024 of a cell is a third of a millimetre — far below anything visible,
/// and far above an `f32` ulp at chunk-local magnitudes (~4e-6 at 48 cells),
/// which is what a naive `f32::EPSILON` would have been *below*, making it a
/// silent no-op.
pub const SKIN: f32 = 1.0 / 1024.0;

/// An axis-aligned box, in cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    /// Low corner.
    pub min: [f32; 3],
    /// High corner.
    pub max: [f32; 3],
}

impl Aabb {
    /// The player box standing with its feet at `feet`.
    ///
    /// The position is the centre of the footprint, not a corner, because that
    /// is what every other system means by "where the player is".
    #[must_use]
    pub fn player_at(feet: [f32; 3]) -> Self {
        let half = PLAYER_WIDTH / 2.0;
        Self {
            min: [feet[0] - half, feet[1], feet[2] - half],
            max: [feet[0] + half, feet[1] + PLAYER_HEIGHT, feet[2] + half],
        }
    }

    /// The box moved by a delta.
    #[must_use]
    pub fn translated(&self, delta: [f32; 3]) -> Self {
        Self {
            min: [
                self.min[0] + delta[0],
                self.min[1] + delta[1],
                self.min[2] + delta[2],
            ],
            max: [
                self.max[0] + delta[0],
                self.max[1] + delta[1],
                self.max[2] + delta[2],
            ],
        }
    }

    /// The range of cells this box touches on one axis.
    ///
    /// The high end is pulled in by [`SKIN`] so that a box whose face lies
    /// exactly on a cell boundary does not claim the cell beyond it. Without
    /// that, a body standing flush against a wall reads as already inside it.
    fn cell_span(&self, axis: usize) -> (i32, i32) {
        (
            floor_to_i32(self.min[axis]),
            floor_to_i32(self.max[axis] - SKIN),
        )
    }
}

/// Solidity of the voxel grid, in the body's frame.
///
/// Contract §2: "A sub-node cell is solid iff occupied (not air). `Uniform`,
/// `Partial`, and `Mixed` are all treated identically — only the per-cell
/// occupancy matters, not which storage form holds it."
pub trait Solid {
    /// Whether the cell at these coordinates is solid.
    ///
    /// Called several times per body per tick over the body's whole volume, so
    /// an implementation that reaches for a chunk should cache the lookup.
    /// Coordinates outside anything loaded should answer **solid**: a body must
    /// not fall through a world that has not arrived yet.
    fn solid(&self, x: i32, y: i32, z: i32) -> bool;

    /// Whether any solid cell overlaps a box.
    fn overlaps(&self, aabb: &Aabb) -> bool {
        let (min_x, max_x) = aabb.cell_span(0);
        let (min_y, max_y) = aabb.cell_span(1);
        let (min_z, max_z) = aabb.cell_span(2);

        // y outermost, x innermost: chunk storage is x-fastest, so this is the
        // order that walks memory forwards.
        for y in min_y..=max_y {
            for z in min_z..=max_z {
                for x in min_x..=max_x {
                    if self.solid(x, y, z) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// What a body is trying to do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Intent {
    /// Desired horizontal direction as `[x, z]`.
    ///
    /// Direction only — the magnitude is ignored beyond normalising, because
    /// how fast the body actually goes is [`Gait`]'s business. A zero vector
    /// means "stop steering", not "stop": momentum still carries.
    pub walk: [f32; 2],
    /// Whether to jump, honoured only when on the ground.
    pub jump: bool,
    /// How fast to try to move, and whether to guard against edges.
    pub gait: Gait,
}

/// A body being simulated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Body {
    /// Centre of the footprint, at the feet. Cells.
    pub position: [f32; 3],
    /// Cells per tick.
    pub velocity: [f32; 3],
    /// Whether the body ended last tick standing on something.
    pub on_ground: bool,
}

impl Body {
    /// A body at rest at a position.
    #[must_use]
    pub const fn at(position: [f32; 3]) -> Self {
        Self {
            position,
            velocity: [0.0, 0.0, 0.0],
            on_ground: false,
        }
    }

    /// The box this body occupies.
    #[must_use]
    pub fn aabb(&self) -> Aabb {
        Aabb::player_at(self.position)
    }

    /// Where the eyes are.
    #[must_use]
    pub fn eye(&self) -> [f32; 3] {
        [
            self.position[0],
            self.position[1] + EYE_HEIGHT,
            self.position[2],
        ]
    }
}

/// Advances a body by one tick.
///
/// The order is gravity, then jump, then steering, then the three axis
/// resolutions — X, Y, Z, per contract §2.
///
/// `on_ground` entering the function is last tick's answer, and that is
/// deliberate: contract §2 allows a step-up when the body "was on the ground",
/// and the horizontal axes resolve before the vertical one has recomputed it.
#[must_use]
pub fn step(solid: &impl Solid, body: Body, intent: Intent, tuning: &Tuning) -> Body {
    let mut body = body;

    // --- vertical velocity -------------------------------------------------
    if intent.jump && body.on_ground {
        body.velocity[1] = tuning.jump_speed;
    } else {
        body.velocity[1] -= tuning.gravity;
        if body.velocity[1] < -tuning.terminal_velocity {
            body.velocity[1] = -tuning.terminal_velocity;
        }
    }

    // --- horizontal steering -----------------------------------------------
    let acceleration = if body.on_ground {
        tuning.ground_acceleration
    } else {
        tuning.air_acceleration
    };
    let [wish_x, wish_z] = normalise(intent.walk);
    body.velocity[0] += wish_x * acceleration;
    body.velocity[2] += wish_z * acceleration;

    let friction = if body.on_ground {
        tuning.ground_friction
    } else {
        tuning.air_drag
    };
    body.velocity[0] *= friction;
    body.velocity[2] *= friction;
    clamp_horizontal(&mut body.velocity, intent.gait.top_speed(tuning));

    // --- resolution --------------------------------------------------------
    let was_on_ground = body.on_ground;
    let sneaking = intent.gait == Gait::Sneak && was_on_ground;

    resolve_horizontal(solid, &mut body, 0, was_on_ground, sneaking, tuning);
    resolve_vertical(solid, &mut body);
    resolve_horizontal(solid, &mut body, 2, was_on_ground, sneaking, tuning);

    debug_assert!(
        body.position.iter().all(|v| v.is_finite()) && body.velocity.iter().all(|v| v.is_finite()),
        "physics produced a non-finite value, which charter rule 4 forbids in simulation state: \
         {body:?}"
    );
    body
}

/// Moves the body along one horizontal axis, stepping up if it is blocked.
///
/// `sneaking` applies contract §2's edge guard: "sneak = edge-safe: cannot walk
/// off a block edge". It is enforced here, per axis and *before* the move is
/// committed, rather than by undoing a completed tick. Undoing was the first
/// implementation and it was wrong twice over: the vertical resolve had already
/// dropped the body by a tick of gravity, so restoring only x and z left it
/// falling with `on_ground` false — after which the guard stopped applying and
/// the body walked off the edge anyway — and the restored horizontal position
/// was only known to be clear at the *old* height, so the body could be put
/// back inside geometry, which contract §2 forbids outright.
fn resolve_horizontal(
    solid: &impl Solid,
    body: &mut Body,
    axis: usize,
    was_on_ground: bool,
    sneaking: bool,
    tuning: &Tuning,
) {
    let delta = body.velocity[axis];
    if delta == 0.0 {
        return;
    }

    let swept = sweep(solid, &body.aabb(), axis, delta);
    if sneaking {
        let supported = longest_supported_move(solid, body.position, axis, swept.distance);
        if supported.blocked {
            // Stopped by the ledge rather than by a wall: move as far as the
            // edge allows, but leave the velocity alone. Zeroing it would make
            // a player holding sneak against an edge release and re-press
            // before they could move along it.
            body.position[axis] += supported.distance;
            return;
        }
    }

    if !swept.blocked {
        body.position[axis] += swept.distance;
        return;
    }

    // Blocked. Contract §2: "A body blocked horizontally retries the move one
    // sub-node higher; if that is clear and it was on the ground, it steps."
    if was_on_ground {
        let lifted = body.aabb().translated([0.0, tuning.step_height, 0.0]);
        let head_room = sweep(solid, &body.aabb(), 1, tuning.step_height);
        if !head_room.blocked && !sweep(solid, &lifted, axis, delta).blocked {
            body.position[1] += tuning.step_height;
            body.position[axis] += delta;
            return;
        }
    }

    body.position[axis] += swept.distance;
    body.velocity[axis] = 0.0;
}

/// Moves the body vertically and recomputes ground contact.
fn resolve_vertical(solid: &impl Solid, body: &mut Body) {
    let delta = body.velocity[1];
    if delta == 0.0 {
        // Still needs a ground answer: a body that neither rose nor fell this
        // tick is standing on something if something is under it.
        body.on_ground = standing_on_ground(solid, body.position);
        return;
    }

    let swept = sweep(solid, &body.aabb(), 1, delta);
    body.position[1] += swept.distance;

    if swept.blocked {
        // Landed if it was going down, bumped its head if it was going up.
        body.on_ground = delta < 0.0;
        body.velocity[1] = 0.0;
    } else {
        body.on_ground = false;
    }
}

/// How far along an axis a sneaking body may move and still be standing on
/// something.
///
/// Support is monotone along the axis — walking further off a ledge never puts
/// ground back — so a bisection finds the edge. Four halvings land within a
/// sixteenth of the attempted step, which at a sneak's 0.195 cells/tick is
/// about four millimetres: close enough to read as hugging the edge, and a
/// fixed count so the cost is bounded and the result identical everywhere.
fn longest_supported_move(
    solid: &impl Solid,
    position: [f32; 3],
    axis: usize,
    delta: f32,
) -> Sweep {
    let mut moved = position;
    moved[axis] = position[axis] + delta;
    if standing_on_ground(solid, moved) {
        return Sweep::clear(delta);
    }

    // Bisection written as a halving span rather than a midpoint of two bounds:
    // same sequence of probes, no `(a + b) / 2`.
    let mut supported = 0.0;
    let mut span = delta;
    for _ in 0..4 {
        span /= 2.0;
        let probe = supported + span;
        moved[axis] = position[axis] + probe;
        if standing_on_ground(solid, moved) {
            supported = probe;
        }
    }
    Sweep::stopped(supported)
}

/// Whether there is solid ground immediately below a body's feet.
fn standing_on_ground(solid: &impl Solid, position: [f32; 3]) -> bool {
    let probe = Aabb::player_at(position).translated([0.0, -SKIN * 2.0, 0.0]);
    let (min_x, max_x) = probe.cell_span(0);
    let (min_z, max_z) = probe.cell_span(2);
    let y = floor_to_i32(probe.min[1]);

    for z in min_z..=max_z {
        for x in min_x..=max_x {
            if solid.solid(x, y, z) {
                return true;
            }
        }
    }
    false
}

/// The outcome of a sweep: how far the box got, and whether something stopped
/// it.
///
/// `blocked` is a fact the sweep already knows, and carrying it is what lets
/// every caller ask "did this move complete" without comparing two `f32`s for
/// exact equality. That comparison is the kind that is right until the day
/// someone changes the arithmetic slightly and it silently becomes never-true.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sweep {
    /// How far the box may actually move.
    distance: f32,
    /// Whether geometry stopped it short.
    blocked: bool,
}

impl Sweep {
    /// The whole move, unobstructed.
    const fn clear(distance: f32) -> Self {
        Self {
            distance,
            blocked: false,
        }
    }

    /// Stopped short at this distance.
    const fn stopped(distance: f32) -> Self {
        Self {
            distance,
            blocked: true,
        }
    }
}

/// How far a box may move along an axis before it touches something.
///
/// Walks cell by cell rather than solving for a time of impact: the grid is
/// unit-spaced, so the loop runs once per cell crossed, and
/// [`Tuning::terminal_velocity`] is capped below a chunk per tick to bound it.
fn sweep(solid: &impl Solid, aabb: &Aabb, axis: usize, delta: f32) -> Sweep {
    if delta == 0.0 {
        return Sweep::clear(0.0);
    }

    let (perp_a, perp_b) = match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    let (min_a, max_a) = aabb.cell_span(perp_a);
    let (min_b, max_b) = aabb.cell_span(perp_b);

    let blocked = |cell: i32| -> bool {
        for a in min_a..=max_a {
            for b in min_b..=max_b {
                let solid_here = match axis {
                    0 => solid.solid(cell, a, b),
                    1 => solid.solid(a, cell, b),
                    _ => solid.solid(a, b, cell),
                };
                if solid_here {
                    return true;
                }
            }
        }
        false
    };

    if delta > 0.0 {
        // The leading face is `max`. It currently sits inside cell
        // `floor(max - SKIN)`; the first cell it could enter is the next one.
        let from = floor_to_i32(aabb.max[axis] - SKIN);
        let to = floor_to_i32(aabb.max[axis] + delta - SKIN);
        for cell in (from + 1)..=to {
            if blocked(cell) {
                // The face may advance to this cell's near boundary, less a
                // skin so the two do not test as touching next tick.
                let room = cell as f32 - aabb.max[axis] - SKIN;
                return Sweep::stopped(if room < 0.0 { 0.0 } else { room });
            }
        }
        Sweep::clear(delta)
    } else {
        let from = floor_to_i32(aabb.min[axis]);
        let to = floor_to_i32(aabb.min[axis] + delta);
        let mut cell = from - 1;
        while cell >= to {
            if blocked(cell) {
                let room = (cell + 1) as f32 - aabb.min[axis] + SKIN;
                return Sweep::stopped(if room > 0.0 { 0.0 } else { room });
            }
            cell -= 1;
        }
        Sweep::clear(delta)
    }
}

/// Scales a steering vector to unit length, or to zero if it has none.
///
/// The zero check is not defensive tidiness: normalising `[0, 0]` divides zero
/// by zero, and charter rule 4 forbids simulation state that has been anywhere
/// near a `NaN`.
fn normalise(v: [f32; 2]) -> [f32; 2] {
    let square = v[0] * v[0] + v[1] * v[1];
    if square <= 0.0 {
        return [0.0, 0.0];
    }
    let length = square.sqrt();
    [v[0] / length, v[1] / length]
}

/// Caps horizontal speed without touching the vertical component.
fn clamp_horizontal(velocity: &mut [f32; 3], limit: f32) {
    let square = velocity[0] * velocity[0] + velocity[2] * velocity[2];
    if square <= limit * limit {
        return;
    }
    let scale = limit / square.sqrt();
    velocity[0] *= scale;
    velocity[2] *= scale;
}

#[cfg(test)]
mod tests;
