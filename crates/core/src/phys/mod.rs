// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Swept-AABB collision against the sub-node grid.
//!
//! Implements **Sub-Node Contract §2**, quoted where each rule is enforced:
//! collision is solid at sub-node resolution, a cell is solid iff occupied,
//! step-up is one sub-node, and movement resolves one axis at a time.
//!
//! **A body that begins a tick inside geometry is left there**, which is a
//! change from the contract's original §2 and is recorded in it. The engine used
//! to ease such a body out; see [`step`] for why it no longer does and what a
//! damage rule is meant to read instead.
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
pub mod swim;
pub mod tuning;
pub mod voxels;

use crate::detgen::floor_to_i32;
use crate::fluid::Fluid;

pub use input::InputQueue;
pub use ray::{Hit, REACH};
pub use swim::{Submersion, submersion};
pub use tuning::{Gait, Tuning};
pub use voxels::{ChunkLookup, Dry, FluidLookup, Voxels};

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
    #[must_use]
    pub fn cell_span(&self, axis: usize) -> (i32, i32) {
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

    /// Whether the world actually knows what is at this cell.
    ///
    /// **Absence and solidity are different questions, and only one of them is a
    /// fact.** A cell in a chunk that has not arrived reads as solid from
    /// [`Solid::solid`] so that a body cannot fall out of a world still in
    /// flight — a deliberate guess, and the right one, because it fails by
    /// standing a player still rather than by losing them.
    ///
    /// Anything that MOVES a body rather than merely stopping it has to know the
    /// difference. Defaults to `true`: a caller with the whole world in hand —
    /// every test scene here — is never guessing.
    fn loaded(&self, _x: i32, _y: i32, _z: i32) -> bool {
        true
    }

    /// Re-anchors the frame this view answers in.
    ///
    /// **A body's coordinates only mean something next to the origin they are
    /// measured from** (charter rule 7), so a view built for one origin answers
    /// a different question once the body has been renormalised into another.
    /// Any caller that steps a body more than once — a client replaying the
    /// inputs the server has not confirmed — has to say when that happened.
    ///
    /// Defaults to nothing, which is right for a view with no frame: a test grid
    /// in absolute coordinates answers the same question whatever the body's
    /// origin is. [`voxels::Voxels`] overrides it.
    ///
    /// # The bug this exists for
    ///
    /// Reconciliation adopted the server's `(chunk, local)` and then replayed
    /// against a view still anchored to the origin the client had a moment
    /// earlier. The two differ for exactly as long as it takes both sides to
    /// cross a chunk plane — a tick or two, every crossing — and in that window
    /// the replay collided against a world displaced by a whole chunk. Measured
    /// from a session log: the client crossed y=0 on tick 168 and renormalised,
    /// the server crossed on 169, and the replay in between fell 3.6 cells
    /// through solid ground with `vy` at exactly five ticks of gravity, because
    /// there was nothing under it 48 cells up. Reported as "if I run into a
    /// chunk corner, I often glitch ... if I am within a chunk I am completely
    /// fine".
    fn rebase(&self, _origin: crate::coords::ChunkPos) {}

    /// What fluid fills the block at these **block** coordinates.
    ///
    /// # Mind the units
    ///
    /// Every other method here answers per sub-node cell. This one answers per
    /// BLOCK — three cells to a side — because fluid is block-resolution by
    /// design (Sub-Node Contract §4), and pretending otherwise by taking cell
    /// coordinates would invite a caller to believe the three cells of a block
    /// could hold different amounts. A frame cell `c` lies in frame block
    /// `c.div_euclid(3)`.
    ///
    /// Defaults to empty, which is the right answer for every view that has no
    /// fluid to report: a test grid, and the collision-only path a caller takes
    /// when it holds no fluid layers. **Dryness is also what keeps this
    /// backwards compatible with itself** — a body in air must take exactly the
    /// arithmetic it took before fluid existed, and it does, because
    /// [`step`] branches on the submerged fraction being zero.
    fn fluid(&self, _x: i32, _y: i32, _z: i32) -> Fluid {
        Fluid::EMPTY
    }

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

    // **A body inside geometry stays inside it.**
    //
    // There used to be a depenetration pass here that eased a body out along the
    // shortest axis, and it is gone on purpose rather than because it was
    // broken. Two things were wrong with the idea, not the implementation.
    //
    // The first is that it could not be made to work. Every case where the
    // escape would have been *useful* — a body at a chunk boundary, a body
    // inside an unloaded chunk, a body genuinely entombed — is a case where the
    // cells cannot say which way out is real, so the pass had to refuse; and
    // what it refused on was most of what a player actually hits. The comments
    // it left behind are a record of that retreat.
    //
    // The second is that being stuck in a block is supposed to be bad. A player
    // squeezed into geometry should suffer for it and eventually die, the way
    // suffocation works in every game that has this problem — not be quietly
    // teleported to safety by the engine. So the engine no longer has an
    // opinion: the body stays where it is, and the tick runs normally around it.
    //
    // **A body inside geometry can still walk out**, which is what makes this
    // survivable rather than a soft-lock: [`sweep`] only tests the cells a
    // leading face would ENTER, never the ones a body already occupies, so a
    // step toward open air is unobstructed and a step deeper is refused.
    //
    // The damage is not here and cannot be yet. It needs health, which is an
    // entity property, and entities are Task 12 — implementing them now would be
    // building a future prompt's feature early. What this leaves is the
    // mechanism a damage rule will read: a body whose own volume overlaps solid
    // geometry, every tick, with nothing hiding it.

    // **How wet the body is, decided once, before anything moves.**
    //
    // Every fluid effect below scales by this one number, so there is no
    // threshold anywhere between walking and swimming — which is what stops
    // waist-deep milk from flickering between the two as a player steps. It is
    // measured from the box the tick STARTED in, for the same reason
    // `was_on_ground` is last tick's answer: the resolutions that follow move
    // the body, and a force computed halfway through them is a force applied to
    // a body that was somewhere else.
    //
    // Zero for a dry world, and every branch below is guarded on that, so a
    // body in air takes bit-identical arithmetic to what it took before fluid
    // existed. Charter rule 4 is not negotiable retroactively: the determinism
    // goldens in `crates/core/tests/determinism.rs` were hashed by the old code
    // and must still match.
    let wet = swim::submersion(solid, &body.aabb()).fraction;

    // --- vertical velocity -------------------------------------------------
    if intent.jump && body.on_ground {
        // A push off the bottom is still a jump. Standing in a shallow pool and
        // leaping out of it is the same action as leaping off dry ground, and
        // routing it through the swim branch would turn it into a feeble drift.
        body.velocity[1] = tuning.jump_speed;
    } else if wet > 0.0 {
        // Asked only when there is fluid to be in, so a dry tick pays nothing
        // for a question about milk.
        let head_clear = swim::head_is_clear(solid, &body.aabb());
        swim::vertical(&mut body.velocity[1], wet, head_clear, intent, tuning);
    } else {
        body.velocity[1] -= tuning.gravity;
        if body.velocity[1] < -tuning.terminal_velocity {
            body.velocity[1] = -tuning.terminal_velocity;
        }
    }

    // --- horizontal steering -----------------------------------------------
    let mut acceleration = if body.on_ground {
        tuning.ground_acceleration
    } else {
        tuning.air_acceleration
    };
    let mut friction = if body.on_ground {
        tuning.ground_friction
    } else {
        tuning.air_drag
    };
    let mut top_speed = intent.gait.top_speed(tuning);
    if wet > 0.0 {
        // Blended from whatever the body's dry state was rather than replacing
        // it, so a player wading a ford loses speed in proportion to how much of
        // them is in the ford. The gait still matters underwater — a sprint is
        // faster than a sneak there too — because the blend moves the ENDS of
        // the range and not the choice between them.
        acceleration += (tuning.swim_acceleration - acceleration) * wet;
        friction += (tuning.fluid_drag - friction) * wet;
        top_speed += (tuning.swim_speed - top_speed) * wet;
    }

    let [wish_x, wish_z] = normalise(intent.walk);
    body.velocity[0] += wish_x * acceleration;
    body.velocity[2] += wish_z * acceleration;

    body.velocity[0] *= friction;
    body.velocity[2] *= friction;
    clamp_horizontal(&mut body.velocity, top_speed);

    // --- resolution --------------------------------------------------------
    let was_on_ground = body.on_ground;
    let sneaking = intent.gait == Gait::Sneak && was_on_ground;

    // **Decided once, before anything moves, and for both axes.**
    //
    // Whether being blocked costs a body its horizontal speed is a question
    // about the tick: a body resting on the ground and walking into a wall
    // should stop dead, and a body climbing past something should keep the speed
    // it took the climb at (see [`resolve_horizontal`]).
    //
    // It must not be re-derived per axis from `velocity[1]`, because
    // `resolve_vertical` runs BETWEEN the two horizontal resolves and zeroes
    // that velocity on a head bump. X asked before the bump and kept its speed;
    // Z asked after it and lost all of it — from identical input, in a scene
    // symmetric in both. Reported from the window as physics being "very glitchy
    // when I am in a tight area (blocks above or messy all around me)", and a
    // ceiling is exactly what it takes: without one, nothing zeroes the vertical
    // velocity mid-tick and the two axes happen to agree.
    let stops_dead = was_on_ground && body.velocity[1] <= 0.0;
    // Whether this tick is climbing. A jump is the only way to be rising while
    // starting on the ground, and it is the case step-down must keep its hands
    // off — see [`step_down`].
    let was_rising = body.velocity[1] > 0.0;
    // Where the feet were before anything moved, so a stride can be put back at
    // the height it started from rather than a tick of gravity below it.
    let entry_height = body.position[1];

    resolve_horizontal(
        solid,
        &mut body,
        0,
        was_on_ground,
        stops_dead,
        sneaking,
        tuning,
    );
    resolve_vertical(solid, &mut body);
    resolve_horizontal(
        solid,
        &mut body,
        2,
        was_on_ground,
        stops_dead,
        sneaking,
        tuning,
    );

    // After BOTH horizontal axes, because a body leaves a lip sideways and how
    // far it has to fall is only known once it has finished moving.
    step_down(
        solid,
        &mut body,
        was_on_ground,
        was_rising,
        entry_height,
        tuning,
    );

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
    stops_dead: bool,
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
            // edge allows.
            body.position[axis] += supported.distance;

            // **And lose the speed, which an earlier version deliberately kept.**
            //
            // The reasoning for keeping it was that zeroing would make a player
            // holding sneak against an edge release and re-press before they
            // could move along it. That is not so — steering re-accelerates from
            // the intent every tick, and the perpendicular axis is resolved
            // separately and never touched here — and the cost of keeping it was
            // far worse than the imagined cost of losing it.
            //
            // A body held at the brink accumulates the speed the guard is
            // suppressing, tick after tick. Release sneak and that speed is
            // suddenly free: measured, a body sneaked to the edge of a hole and
            // then given NO input at all slid forward 0.117 cells on the first
            // ungated tick and fell straight in. Reported from the window as
            // "standing on the edge with shift and letting go makes me glitch as
            // I fall in" — pressing nothing, and falling anyway.
            //
            // Stopping dead at a ledge is the same answer walking into a wall
            // gets, and for the same reason: a body that is not moving should not
            // be storing a shove.
            body.velocity[axis] = 0.0;
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

    // **Stopping is not the same as losing your momentum, and in the air it
    // must not be.**
    //
    // Contract §2 resolves X before Y, so a horizontal move is always tested at
    // the height the body had at the START of the tick. A body rising past a
    // step therefore meets the riser one tick before it clears it — and zeroing
    // the velocity there threw away a whole tick of speed that the very next
    // tick would have been free to use. With `air_acceleration` a fifteenth of
    // the ground figure, getting it back takes half a second.
    //
    // Reported from the window as a tunnel staircase being "close to impossible
    // it is so jumpy". Traced: climbing three-cell steps in a nine-cell
    // passage, the body's x froze for three and four ticks at a time at every
    // riser and then crawled forward at a twelfth of walking pace.
    //
    // Standing on the ground it still zeroes, because there stopping IS the
    // answer: a body walking into a wall should not keep a speed it is not
    // using, or it leaves the wall like a released spring.
    //
    // **`was_on_ground` alone is not the right test, because the tick a jump
    // starts is a tick that begins on the ground.** The jump has already been
    // applied to the vertical velocity by the time this runs — see [`step`] —
    // so a body pressed against a riser and pushing off it was having exactly
    // the tick of speed it most needed taken away. Rising means climbing past
    // the thing in the way, not resting against it.
    //
    // Which is why the answer arrives as an argument rather than being read from
    // `body.velocity[1]` here: this function runs once per horizontal axis with
    // the vertical resolve between the two, and that resolve zeroes the velocity
    // this rule used to consult. See [`step`].
    if stops_dead {
        body.velocity[axis] = 0.0;
    }
}

/// Puts a body back on the ground when it has walked off something small.
///
/// Contract §2: "Step-down is the same height, and is not optional. A body that
/// began its tick on the ground, is not rising, and would end it airborne looks
/// one sub-node below its feet; if there is ground there it is placed on it and
/// stays on the ground."
///
/// # Why this is not cosmetic
///
/// Step-up without step-down does not make sub-node terrain walkable, it makes
/// it *skimmable*. A body crosses the tops of raised cells, drops through every
/// gap wider than its own footprint, and while airborne it loses ground
/// acceleration — a fifteenth of the grounded figure — AND is stopped by the
/// side of the next raised cell, which it may not step over until it lands
/// again. Measured over cells raised every third cell, before this existed:
/// **forward motion froze for three ticks at a time** and the body bobbed a full
/// sub-node, once per gap.
///
/// # The three guards, and what each one is for
///
/// * `was_on_ground` — a body already in mid-air is falling and must keep
///   falling. Without this, every fall would be caught a sub-node above every
///   surface it passed.
/// * `!was_rising` — a jump starts on the ground and must be allowed to leave
///   it. Without this, the tick a jump began would be undone by the tick that
///   began it.
/// * The sweep — a drop of more than one sub-node is a fall, and is left alone.
///   This is the same height as step-up on purpose: what a body can climb is
///   exactly what it can be glued to, which is what makes a lip feel like part
///   of the floor rather than like a cliff.
fn step_down(
    solid: &impl Solid,
    body: &mut Body,
    was_on_ground: bool,
    was_rising: bool,
    entry_height: f32,
    tuning: &Tuning,
) {
    if !was_on_ground || was_rising || body.on_ground {
        return;
    }

    // Downward, so a blocked sweep is ground within reach and its distance is
    // how far to place the body. The sweep is what keeps this from ever putting
    // a body inside geometry — contract §2's overriding invariant — because it
    // stops a skin short of the surface exactly as landing does.
    let reach = sweep(solid, &body.aabb(), 1, -tuning.step_height);
    if !reach.blocked {
        // Nothing within a sub-node: this is a hole, not a rut. Fall.
        return;
    }

    // **Stride over it rather than dipping into it — but only because the drop
    // is shallow, which the sweep above has just established.**
    //
    // Contract §2: "A body strides over a gap narrower than its own footprint."
    // Without it, crossing chiselled ground fell a whole sub-node and climbed
    // back out on the NEXT tick — a 30 cm spike lasting 50 ms, five times in
    // forty ticks over random rubble.
    //
    // **Depth is what separates a rut from a hole, and width cannot do it.** The
    // first version asked only whether there was ground a footprint ahead, and a
    // one-block hole dug two deep is three cells across — the same span as the
    // gaps between rubble lips. A body walking at it found the far rim within
    // reach and strode straight over the top, reported from the window as "if I
    // dig a hole straight down two I can currently walk right across it without
    // falling in". A rut has its floor within a sub-node; a hole does not.
    if strides_over_a_gap(solid, body) {
        // Back to the height the tick began at. The vertical resolve has already
        // applied a tick of gravity by now, and leaving that in place turned a
        // 30 cm spike into an 8 cm one rather than removing it — the body still
        // sagged into every crack, just less. Swept rather than assigned, so
        // this can never push the body into something that has come between it
        // and where it stood.
        let recover = entry_height - body.position[1];
        if recover > 0.0 {
            body.position[1] += sweep(solid, &body.aabb(), 1, recover).distance;
        }
        body.on_ground = true;
        body.velocity[1] = 0.0;
        return;
    }

    body.position[1] += reach.distance;
    body.on_ground = true;
    // Zeroed, because the body is resting on something. Left alone, the tick of
    // gravity it had already accumulated would still be there next tick and the
    // body would read as falling while standing still.
    body.velocity[1] = 0.0;
}

/// Whether there is ground at the body's own height one footprint ahead.
///
/// Contract §2's stride rule. A body with no horizontal motion has no "ahead"
/// and strides nowhere — standing still at the edge of a crack, it drops into
/// it, which is what standing still over a hole should do.
fn strides_over_a_gap(solid: &impl Solid, body: &Body) -> bool {
    let [x, _, z] = body.velocity;
    let speed = (x * x + z * z).sqrt();
    if speed <= 0.0 {
        return false;
    }

    // One footprint, because that is the span a body's own feet cover: the
    // question this asks is whether the far side of the gap is within the
    // stance, not whether the body could reach it eventually.
    let reach = PLAYER_WIDTH / speed;
    let ahead = [
        body.position[0] + x * reach,
        body.position[1],
        body.position[2] + z * reach,
    ];
    standing_on_ground(solid, ahead)
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
///
/// Public because a client needs to ask it about a position that is not its
/// own: given where the server says a body is, does *this* copy of the world
/// have anything under it? Asking with a slightly different probe than the
/// simulation uses would answer a slightly different question, which is exactly
/// what a diagnostic must not do.
#[must_use]
pub fn standing_on_ground(solid: &impl Solid, position: [f32; 3]) -> bool {
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
