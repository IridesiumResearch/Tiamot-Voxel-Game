// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! How much of a body is under the milk, and what that does to it.
//!
//! # This is simulation code
//!
//! Charter rule 4 in full, exactly as [`super`]: only `+ - * /`, comparisons and
//! [`crate::detgen::floor_to_i32`] appear below. Nothing here can produce a
//! `NaN` — the one division is by a box volume that is checked positive first.
//!
//! # Two resolutions meeting
//!
//! Collision is sub-node and fluid is block-resolution (Sub-Node Contract §4),
//! so this is where the two are reconciled. A block's fluid fills the bottom
//! `depth_units` twenty-sevenths of its HEIGHT and all of its footprint, which
//! makes the wetted region of any one block a box — and the fluid volume inside
//! a body is then the sum of a handful of box intersections rather than a scan
//! of cells. A standing player spans about twelve blocks, so that is twelve
//! lookups and twelve multiplies per body per tick.
//!
//! **Volume, not a depth line.** Buoyancy proportional to "how deep the feet
//! are" would make a body straddling a pond's edge behave as though all of it
//! were under, and a body lying against a waterfall float on a column it barely
//! touches. The submerged *fraction of the box* is the quantity the task asks
//! for and it is the one that behaves at boundaries.

use crate::detgen::floor_to_i32;
use crate::fluid::{FluidId, MAX_FLUIDS};

use super::{Aabb, Solid};

/// Units of a block's height per sub-node cell.
///
/// [`crate::UNITS_PER_BLOCK`] is 27 and a block is [`crate::SUBNODES_PER_AXIS`]
/// cells tall, so nine units of depth is one cell. This is the number that
/// converts [`crate::fluid::Fluid::depth_units`] into the cells the physics
/// works in.
const UNITS_PER_CELL: f32 = (crate::UNITS_PER_BLOCK / crate::SUBNODES_PER_AXIS) as f32;

/// Cells to a block, as an `f32` for the box arithmetic below.
const CELLS_PER_BLOCK: f32 = crate::SUBNODES_PER_AXIS as f32;

/// How much fluid a body is in, and which.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Submersion {
    /// The fraction of the body's box filled with fluid, `0.0..=1.0`.
    ///
    /// Every effect below scales by this, which is what makes wading a puddle
    /// and swimming a lake the same code with no threshold between them. A
    /// threshold is exactly what produces the classic "waist-deep water flips
    /// between walking and swimming as you step" artefact.
    pub fraction: f32,

    /// Which fluid most of that volume is, or [`FluidId::NONE`].
    ///
    /// A body straddling two fluids is a legitimate state — the engine supports
    /// fifteen — and something has to name one for a tint or a mod hook. The
    /// largest volume wins, and the lowest id breaks a tie, so the answer does
    /// not depend on the order blocks happened to be visited in.
    pub fluid: FluidId,
}

impl Submersion {
    /// Nothing wet at all.
    pub const DRY: Self = Self {
        fraction: 0.0,
        fluid: FluidId::NONE,
    };

    /// Whether the body is touching any fluid.
    #[must_use]
    pub fn any(self) -> bool {
        self.fraction > 0.0
    }
}

/// How much of a box is fluid, and which fluid.
///
/// The frame is the body's, exactly as everywhere else in [`super`]: cell
/// coordinates relative to whatever origin the caller anchored [`Solid`] to.
#[must_use]
pub fn submersion(solid: &impl Solid, aabb: &Aabb) -> Submersion {
    // The box's own volume, which the fraction is measured against. Computed
    // from the box rather than from the player constants because a mod's body
    // is not obliged to be player-shaped.
    let volume =
        (aabb.max[0] - aabb.min[0]) * (aabb.max[1] - aabb.min[1]) * (aabb.max[2] - aabb.min[2]);
    if volume <= 0.0 {
        // A degenerate box has no inside to flood, and dividing by its volume
        // is how charter rule 4's ban on `NaN` in simulation state gets broken.
        return Submersion::DRY;
    }

    // One accumulator per registered fluid, so the winner can be chosen by id
    // rather than by whichever block came last. Sixteen floats on the stack.
    let mut by_fluid = [0.0f32; MAX_FLUIDS + 1];
    let mut any = false;

    let (min_x, max_x) = block_span(aabb, 0);
    let (min_y, max_y) = block_span(aabb, 1);
    let (min_z, max_z) = block_span(aabb, 2);

    // y outermost, x innermost, matching [`Solid::overlaps`] — the order chunk
    // storage is laid out in, and a FIXED order, which is what charter rule 4
    // requires of anything that accumulates floats.
    for y in min_y..=max_y {
        for z in min_z..=max_z {
            for x in min_x..=max_x {
                let fluid = solid.fluid(x, y, z);
                if fluid.is_empty() {
                    continue;
                }

                // The wetted part of this block: all of its footprint, and its
                // depth in height.
                let floor = y as f32 * CELLS_PER_BLOCK;
                let depth = surface_height(solid, x, y, z, fluid);

                let wetted = overlap(
                    aabb.min[0],
                    aabb.max[0],
                    x as f32 * CELLS_PER_BLOCK,
                    (x + 1) as f32 * CELLS_PER_BLOCK,
                ) * overlap(aabb.min[1], aabb.max[1], floor, floor + depth)
                    * overlap(
                        aabb.min[2],
                        aabb.max[2],
                        z as f32 * CELLS_PER_BLOCK,
                        (z + 1) as f32 * CELLS_PER_BLOCK,
                    );
                if wetted <= 0.0 {
                    continue;
                }

                by_fluid[fluid.fluid().0 as usize] += wetted;
                any = true;
            }
        }
    }

    if !any {
        return Submersion::DRY;
    }

    // Summed in id order, and the winner picked with a strict `>` while
    // scanning ids upward, so ties go to the lower id.
    let mut total = 0.0;
    let mut best = FluidId::NONE;
    let mut best_volume = 0.0;
    for (id, &wetted) in by_fluid.iter().enumerate() {
        total += wetted;
        if wetted > best_volume {
            best_volume = wetted;
            best = FluidId(id as u8);
        }
    }

    // Clamped rather than trusted. The box intersections cannot exceed the box
    // in exact arithmetic, but a fraction that came out at 1.000001 would put
    // `1.0 - buoyancy * fraction` on the wrong side of zero at the surface,
    // which is where the equilibrium lives.
    let fraction = if total >= volume { 1.0 } else { total / volume };

    Submersion {
        fraction,
        fluid: best,
    }
}

/// Which fluid is at a single point, or [`FluidId::NONE`].
///
/// What a camera asks: an eye is a point, not a box, and "is the view
/// underwater" has no sensible fractional answer. Presentation code (charter
/// rule 4 explicitly does not bind it) is the caller, but the query lives here
/// because it must read the same surface height the physics does — a tint that
/// arrives a fraction of a cell before or after the body starts floating is the
/// kind of mismatch nobody can debug from a screenshot.
#[must_use]
pub fn fluid_at(solid: &impl Solid, point: [f32; 3]) -> FluidId {
    let block = [
        floor_to_i32(point[0]).div_euclid(crate::SUBNODES_PER_AXIS as i32),
        floor_to_i32(point[1]).div_euclid(crate::SUBNODES_PER_AXIS as i32),
        floor_to_i32(point[2]).div_euclid(crate::SUBNODES_PER_AXIS as i32),
    ];
    let fluid = solid.fluid(block[0], block[1], block[2]);
    if fluid.is_empty() {
        return FluidId::NONE;
    }

    let floor = block[1] as f32 * CELLS_PER_BLOCK;
    let surface = floor + surface_height(solid, block[0], block[1], block[2], fluid);
    if point[1] < surface {
        fluid.fluid()
    } else {
        FluidId::NONE
    }
}

/// How deep the fluid in one block stands, in cells above the block's floor.
///
/// # The rule that has to match the renderer exactly
///
/// Contract §4.4: a block's fluid stands at its volume, in cells of 27, and **a
/// block with fluid above it has no surface** — it is full to all 27 whatever
/// its own volume says. The client's mesher says the same thing in
/// `ChunkFluid::fill`, and without the same rule here a body would float at a
/// level the milk is not drawn at.
///
/// The interior of a pond would otherwise be air by the fraction each block was
/// short: every block in a submerged column stopping below the one above it,
/// for a buoyancy weaker than the constants say and a body that sinks
/// fractionally through each block boundary it passes. The renderer had this
/// bug first and fixed it; the physics does not get to rediscover it.
fn surface_height(solid: &impl Solid, x: i32, y: i32, z: i32, fluid: crate::fluid::Fluid) -> f32 {
    if !solid.fluid(x, y + 1, z).is_empty() {
        return CELLS_PER_BLOCK;
    }
    fluid.volume() as f32 / UNITS_PER_CELL
}

/// Whether the top of a box is out of the fluid.
///
/// "Can this body see over the water" — which is what decides whether it can
/// push itself up out of it. A point query at the top face rather than anything
/// derived from the submerged fraction, because the fraction cannot tell a body
/// floating with its head out from one wedged under a ledge with a bubble of air
/// against its feet.
#[must_use]
pub fn head_is_clear(solid: &impl Solid, aabb: &Aabb) -> bool {
    // A skin below the top face: exactly on a boundary is the one place the
    // block lookup could land on the block above and answer about the wrong one.
    let head = [
        aabb.min[0] + (aabb.max[0] - aabb.min[0]) / 2.0,
        aabb.max[1] - super::SKIN,
        aabb.min[2] + (aabb.max[2] - aabb.min[2]) / 2.0,
    ];
    fluid_at(solid, head).is_none()
}

/// The blocks a box touches on one axis, as an inclusive range.
///
/// Derived from [`Aabb::cell_span`] rather than from a second float division,
/// so the blocks considered here are exactly the blocks holding the cells the
/// collision considers. Two roundings of the same edge that disagree by one
/// would let a body float on a block it is not touching.
fn block_span(aabb: &Aabb, axis: usize) -> (i32, i32) {
    let (min_cell, max_cell) = aabb.cell_span(axis);
    let per_block = crate::SUBNODES_PER_AXIS as i32;
    (
        min_cell.div_euclid(per_block),
        max_cell.div_euclid(per_block),
    )
}

/// How much two intervals share, or zero if they are disjoint.
///
/// Written with comparisons rather than `f32::min`/`max` to keep every operation
/// in this file visibly inside the Deterministic Float Subset, which is the
/// habit charter rule 4 asks for even where the alternative would have been
/// fine.
fn overlap(a_lo: f32, a_hi: f32, b_lo: f32, b_hi: f32) -> f32 {
    let lo = if a_lo > b_lo { a_lo } else { b_lo };
    let hi = if a_hi < b_hi { a_hi } else { b_hi };
    if hi > lo { hi - lo } else { 0.0 }
}

/// Whether a body is deep enough in fluid for a fall to have been broken.
///
/// The threshold the task names — "fall damage cancelled by ≥2 deep milk" — as
/// a question the engine can answer, because the damage itself is not the
/// engine's business. There is no health in this engine and there will not be:
/// hurting a player is game design and belongs in a mod (charter scope
/// discipline). What a mod cannot compute for itself is how much fluid a body
/// is actually inside, so that is what is exported.
///
/// Two blocks deep means the body's feet have two full blocks of fluid beneath
/// the surface, which for a player box is most of it — hence the fraction this
/// compares against rather than a separate downward probe.
#[must_use]
pub fn breaks_a_fall(submersion: Submersion) -> bool {
    // Two blocks is six cells; a player is 5.4 tall, so a body that has sunk
    // into two blocks of milk is entirely under. Asking for "entirely" would be
    // brittle at the surface, so this asks for most of it.
    submersion.fraction >= 0.75
}

/// The vertical acceleration a submerged body gets this tick, in cells/tick².
///
/// Split out so the equilibrium is testable without running a whole step: the
/// float depth a body settles at is where this returns zero, and that number is
/// the single most visible thing about how a fluid feels.
#[must_use]
pub fn buoyant_acceleration(fraction: f32, tuning: &super::Tuning) -> f32 {
    // Weight, less the share of it the fluid carries. With `buoyancy` above one
    // a fully submerged body has more lift than weight and rises until it is
    // `1 / buoyancy` submerged — which is the surface, and is where it bobs.
    -tuning.gravity * (1.0 - tuning.buoyancy * fraction)
}

/// Applies one tick of fluid to a body's vertical velocity.
///
/// Called from [`super::step`] instead of the dry gravity branch, never as well
/// as it — a body in air must take arithmetic bit-identical to what it took
/// before swimming existed, or every determinism golden in the repo moves.
pub(super) fn vertical(
    velocity: &mut f32,
    fraction: f32,
    head_clear: bool,
    intent: super::Intent,
    tuning: &super::Tuning,
) {
    *velocity += buoyant_acceleration(fraction, tuning);

    // Jump rises, sneak sinks, both in proportion to how much of the body has
    // fluid to push against. A swimmer with their shoulders out of the milk
    // cannot pull themselves up by it.
    if intent.jump {
        *velocity += tuning.swim_up * fraction;
    } else if intent.gait == super::Gait::Sneak {
        *velocity -= tuning.swim_down * fraction;
    }

    // Drag, blended from the body's dry state so that ankle-deep milk barely
    // slows a walk. `fluid_drag` is a fraction KEPT, like `air_drag`.
    *velocity *= 1.0 + (tuning.fluid_drag - 1.0) * fraction;

    // **The kick out of the water, or a pool has no exit.**
    //
    // Swimming up is an acceleration fighting buoyancy and drag, so it settles
    // exactly where the body stops being submerged — the waterline. A swimmer
    // holding jump therefore rises to the surface and stays there, and the lip
    // of a pool at that same waterline is unclimbable, because the one place the
    // rise weakens to nothing is the place they need to leave from. Measured
    // before this existed: a body swimming at a bank stalled 0.33 cells below
    // the top of it, and stayed there.
    //
    // **At full strength, not scaled by how submerged the body is.** Scaling was
    // the first attempt and it failed for the same reason `swim_up` does — the
    // kick fades to nothing exactly as the body nears the surface, which is
    // where it is needed. `head_clear` carries the physical story instead: a
    // swimmer can push themselves up out of water they can see over and cannot
    // do it from underneath, so this never fires on a submerged body and is not
    // a way to swim upward faster in open water.
    //
    // **After the drag, so `surface_leap` IS the speed the body leaves at**
    // rather than a number the drag then takes a fifth of. A floor under the
    // velocity rather than a term added to it, because the point is to reach a
    // speed; adding would compound tick after tick into a launch.
    if intent.jump && head_clear && *velocity < tuning.surface_leap {
        *velocity = tuning.surface_leap;
    }

    // **The clamp that makes deep milk break a fall.** A body arriving at
    // terminal velocity carries 11.76 cells/tick, and drag alone would take
    // three-quarters of a second and fifteen blocks of depth to bleed that off
    // — so a pond would swallow anyone who jumped into it. Blended by the
    // fraction, so the deceleration arrives over the ticks the body takes to
    // go under rather than as a wall at the surface.
    let floor = tuning.terminal_velocity
        + (tuning.fluid_terminal_velocity - tuning.terminal_velocity) * fraction;
    if *velocity < -floor {
        *velocity = -floor;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::fluid::{Fluid, MAX_VOLUME};
    use crate::phys::{Body, EYE_HEIGHT, Gait, Intent, PLAYER_HEIGHT, Tuning, step};

    const MILK: FluidId = FluidId(1);
    const OIL: FluidId = FluidId(2);

    /// Ground at cell 0, and `depth` blocks of fluid standing on it.
    ///
    /// Blocks rather than cells for the fluid, because that is the resolution it
    /// is stored at (Contract §4) and building the scene in the units the thing
    /// under test uses is what keeps a failing assertion readable.
    struct Pool {
        depth: i32,
        level: u8,
        fluid: FluidId,
    }

    impl Pool {
        const fn new(depth: i32) -> Self {
            Self {
                depth,
                level: MAX_VOLUME as u8,
                fluid: MILK,
            }
        }

        const fn at_level(mut self, level: u8) -> Self {
            self.level = level;
            self
        }
    }

    impl Solid for Pool {
        fn solid(&self, _x: i32, y: i32, _z: i32) -> bool {
            y < 0
        }

        fn fluid(&self, _x: i32, y: i32, _z: i32) -> Fluid {
            if y >= 0 && y < self.depth {
                Fluid::new(self.fluid, u32::from(self.level))
            } else {
                Fluid::EMPTY
            }
        }
    }

    /// The same ground with nothing in it.
    struct DryGround;

    impl Solid for DryGround {
        fn solid(&self, _x: i32, y: i32, _z: i32) -> bool {
            y < 0
        }
    }

    fn simulate(scene: &impl Solid, mut body: Body, intent: Intent, ticks: usize) -> Body {
        for _ in 0..ticks {
            body = step(scene, body, intent, &Tuning::DEFAULT);
        }
        body
    }

    #[test]
    fn a_dry_world_steps_a_body_bit_for_bit_as_it_did_before_fluid_existed() {
        // **The regression this whole feature is one branch away from causing.**
        //
        // Charter rule 4's determinism goldens were hashed by the code that ran
        // before swimming existed, and every one of them steps bodies through
        // air. If a dry tick picks up so much as one extra multiply by 1.0, the
        // cross-platform hash gate moves and every replay in the repo is wrong.
        //
        // A pool of depth zero exercises the fluid-aware path — `submersion` is
        // called, the blocks are scanned — and must still produce a body
        // identical to one stepped against a scene that has no fluid at all.
        let empty = Pool::new(0);
        let intent = Intent {
            walk: [1.0, 0.3],
            jump: true,
            gait: Gait::Sprint,
            fly: false,
        };

        let start = Body::at([24.0, 12.0, 24.0]);
        let wet_path = simulate(&empty, start, intent, 40);
        let dry_path = simulate(&DryGround, start, intent, 40);

        for axis in 0..3 {
            assert_eq!(
                wet_path.position[axis].to_bits(),
                dry_path.position[axis].to_bits(),
                "axis {axis} diverged: {wet_path:?} against {dry_path:?}"
            );
            assert_eq!(
                wet_path.velocity[axis].to_bits(),
                dry_path.velocity[axis].to_bits(),
                "axis {axis} velocity diverged: {wet_path:?} against {dry_path:?}"
            );
        }
    }

    #[test]
    fn a_body_clear_of_the_milk_is_dry_and_one_in_it_is_not() {
        let pool = Pool::new(2);

        // Standing on the bottom of a two-block pool: cells 0..5.4 of the box,
        // and the milk stands 6 cells deep because the lower block has fluid
        // above it and is therefore brim-full.
        let under = submersion(&pool, &Body::at([24.0, 0.0, 24.0]).aabb());
        assert_eq!(under.fluid, MILK);
        assert!(
            under.fraction > 0.99,
            "a body under six cells of milk read {}",
            under.fraction
        );

        // Standing on the surface of it.
        let above = submersion(&pool, &Body::at([24.0, 6.0, 24.0]).aabb());
        assert_eq!(above, Submersion::DRY);
    }

    #[test]
    fn a_full_block_is_full_and_a_part_filled_one_stops_at_its_volume() {
        // **The rule that has to match the renderer**, asserted as the number
        // rather than as a behaviour: Contract §4.4 puts the surface at
        // `volume / 27`, and a block with fluid above it is full whatever its
        // own volume says.
        //
        // This used to assert the opposite of its first half — a brim-full
        // block stopped at 24 of 27 so that a pond showed a surface under the
        // block above it. That was a hack for a waterfall reading as a solid
        // column, and conservation retired it: falling milk is thin because it
        // genuinely holds few cells, not because the renderer was told to lie.
        let pool = Pool::new(3);

        // A one-cell-tall probe inside the lower block, entirely in the region
        // that only exists if the block is full.
        let probe = Aabb {
            min: [24.0, 2.7, 24.0],
            max: [24.5, 2.95, 24.5],
        };
        assert!(
            submersion(&pool, &probe).fraction > 0.99,
            "the top of a submerged block read dry, so the pond has gaps in it"
        );

        // And the same probe in the TOP block, which is also full: 27 cells is
        // the whole block, so a brim-full pond reaches the top of it.
        let surface = Aabb {
            min: [24.0, 8.7, 24.0],
            max: [24.5, 8.95, 24.5],
        };
        assert!(
            submersion(&pool, &surface).fraction > 0.99,
            "a brim-full top block read dry at the brim"
        );

        // A block holding a third of its volume stops a third of the way up.
        // Block 0 spans cells 0..3, so nine cells of 27 is one cell deep and
        // anything above that is out of the milk.
        let shallow = Pool::new(1).at_level(9);
        let dry = Aabb {
            min: [24.0, 1.2, 24.0],
            max: [24.5, 1.4, 24.5],
        };
        assert_eq!(
            submersion(&shallow, &dry),
            Submersion::DRY,
            "a third-full block wet a probe above its surface"
        );
    }

    #[test]
    fn a_shallow_puddle_wets_only_the_part_of_the_box_inside_it() {
        // Known answer. One full block of milk is 27/27 = 3 cells deep
        // (Contract §4.4); a player box is 5.4 cells tall, so a body standing
        // in it is 3/5.4 submerged and no more.
        let pool = Pool::new(1);
        let wet = submersion(&pool, &Body::at([24.0, 0.0, 24.0]).aabb());

        let expected = (27.0 / 9.0) / PLAYER_HEIGHT;
        assert!(
            (wet.fraction - expected).abs() < 1e-4,
            "read {} rather than {expected}",
            wet.fraction
        );
        assert_eq!(wet.fluid, MILK);
    }

    #[test]
    fn a_lower_level_is_a_shallower_puddle() {
        // Monotone, which is what makes wading feel graded rather than stepped.
        let mut last = 0.0;
        for level in 1..=MAX_VOLUME {
            let pool = Pool::new(1).at_level(level as u8);
            let wet = submersion(&pool, &Body::at([24.0, 0.0, 24.0]).aabb()).fraction;
            assert!(
                wet > last,
                "level {level} read {wet}, no deeper than the level below at {last}"
            );
            last = wet;
        }
    }

    #[test]
    fn a_body_settles_at_the_surface_rather_than_the_bottom_or_the_sky() {
        // **The single most visible thing about how a fluid feels**, and the
        // reason `buoyancy` is above one. A body dropped into deep milk must
        // come to rest floating, with its head out — not sink to the bottom and
        // not be spat into the air.
        let pool = Pool::new(6);
        let body = simulate(&pool, Body::at([24.0, 20.0, 24.0]), Intent::default(), 200);

        let tuning = Tuning::DEFAULT;
        let wet = submersion(&pool, &body.aabb()).fraction;
        let expected = 1.0 / tuning.buoyancy;
        assert!(
            (wet - expected).abs() < 0.05,
            "settled {wet} submerged, but buoyancy {} puts equilibrium at {expected}",
            tuning.buoyancy
        );

        // Come to rest, not still oscillating: `fluid_drag` is what damps it.
        assert!(
            body.velocity[1].abs() < 0.01,
            "still bobbing after ten seconds at {} cells/tick",
            body.velocity[1]
        );

        // And the eyes above the milk, which is what "floating" means to the
        // person looking through them.
        let eye = fluid_at(&pool, body.eye());
        assert_eq!(
            eye,
            FluidId::NONE,
            "floating with the camera under the surface: feet at {}, eye at {}",
            body.position[1],
            body.position[1] + EYE_HEIGHT
        );
    }

    #[test]
    fn holding_jump_swims_up_and_holding_sneak_swims_down() {
        let pool = Pool::new(8);
        let start = Body::at([24.0, 12.0, 24.0]);

        let up = simulate(
            &pool,
            start,
            Intent {
                jump: true,
                ..Intent::default()
            },
            40,
        );
        let down = simulate(
            &pool,
            start,
            Intent {
                gait: Gait::Sneak,
                ..Intent::default()
            },
            40,
        );

        assert!(
            up.position[1] > start.position[1] + 3.0,
            "holding jump rose only to {} from {}",
            up.position[1],
            start.position[1]
        );
        assert!(
            down.position[1] < start.position[1] - 3.0,
            "holding sneak sank only to {} from {}",
            down.position[1],
            start.position[1]
        );
    }

    #[test]
    fn a_swimmer_can_climb_out_of_a_pool_onto_its_edge() {
        // **What the surface leap is for, tested as the thing somebody actually
        // wants to do rather than as the constant.** Reported from the window:
        // "I would like to be pushed up out of the water quite a bit when
        // surfacing so I can step up out of a pool."
        //
        // Before the leap this was impossible in principle, not just hard:
        // swimming up is an acceleration fighting buoyancy, so it settles
        // exactly at the waterline, and a lip at the waterline is the one height
        // a swimmer can never reach.
        //
        // The scene: milk filling x < 0 to a surface at cell 12, and solid
        // ground from x >= 0 up to that same height. Swim at the bank holding
        // jump and end up standing on it.
        struct Pool;

        impl Solid for Pool {
            fn solid(&self, x: i32, y: i32, _z: i32) -> bool {
                // The bank: solid ground east of x=0, up to the waterline.
                y < 0 || (x >= 0 && y < 12)
            }

            fn fluid(&self, x: i32, y: i32, _z: i32) -> Fluid {
                // Blocks are three cells; the water occupies block y 0..4,
                // which is cells 0..12, west of the bank.
                if x < 0 && (0..4).contains(&y) {
                    Fluid::new(MILK, MAX_VOLUME)
                } else {
                    Fluid::EMPTY
                }
            }
        }

        // Floating in the milk, a little way from the bank, swimming at it.
        let start = Body::at([-4.0, 8.0, 24.0]);
        let swimming = Intent {
            walk: [1.0, 0.0],
            jump: true,
            gait: Gait::Walk,
            fly: false,
        };
        let out = simulate(&Pool, start, swimming, 60);
        assert!(
            out.position[0] > 0.0,
            "never reached the bank: ended at {:?}",
            out.position
        );

        // Jump released, and a moment to settle. Held, it would bunny-hop east
        // along the bank forever and "on the ground" would be a coin flip about
        // which tick the sample landed on.
        let standing = simulate(
            &Pool,
            out,
            Intent {
                walk: [0.0, 0.0],
                jump: false,
                gait: Gait::Walk,
                fly: false,
            },
            40,
        );

        assert!(
            standing.on_ground,
            "never came to rest on the bank: ended at {:?}",
            standing.position
        );
        assert!(
            (standing.position[1] - 12.0).abs() < 0.1,
            "settled at {} rather than on the bank at 12",
            standing.position[1]
        );
        assert_eq!(
            submersion(&Pool, &standing.aabb()),
            Submersion::DRY,
            "standing on the bank but still reading as wet"
        );
    }

    #[test]
    fn the_leap_needs_a_head_above_the_water() {
        // The guard that keeps this from being a way to swim up faster: a
        // swimmer can push themselves up out of water they can see over, and
        // cannot do it from underneath.
        let pool = Pool::new(10);

        let deep = Body::at([24.0, 6.0, 24.0]);
        assert!(
            !head_is_clear(&pool, &deep.aabb()),
            "the staging is wrong: this body's head is not under"
        );
        let floating = Body::at([24.0, 26.0, 24.0]);
        assert!(
            head_is_clear(&pool, &floating.aabb()),
            "a body at the surface read as having its head under"
        );

        let intent = Intent {
            jump: true,
            ..Intent::default()
        };

        let mut under = 0.0;
        vertical(&mut under, 1.0, false, intent, &Tuning::DEFAULT);
        assert!(
            under < Tuning::DEFAULT.surface_leap * 0.5,
            "a submerged body got the surface kick: {under} cells/tick"
        );

        let mut surfacing = 0.0;
        vertical(&mut surfacing, 0.8, true, intent, &Tuning::DEFAULT);
        assert!(
            surfacing >= Tuning::DEFAULT.surface_leap,
            "a body with its head out did not get the kick: {surfacing} cells/tick"
        );
    }

    #[test]
    fn milk_breaks_a_terminal_velocity_fall_within_a_couple_of_blocks() {
        // The task's "fall damage cancelled by ≥2 deep milk", as the mechanism
        // rather than the damage: a body arriving at terminal velocity has to
        // actually be STOPPED by the milk, and stopped near the top of it. Drag
        // alone would take fifteen blocks, which is why the clamp exists.
        let pool = Pool::new(12);
        let tuning = Tuning::DEFAULT;
        let surface = 12.0 * CELLS_PER_BLOCK;

        // How far a body dropped at terminal velocity from `from` gets, both as
        // a distance travelled and as a depth below the surface.
        let plunge = |from: f32| {
            let mut body = Body::at([24.0, from, 24.0]);
            body.velocity[1] = -tuning.terminal_velocity;
            let mut deepest = body.position[1];
            for _ in 0..60 {
                body = step(&pool, body, Intent::default(), &tuning);
                if body.position[1] < deepest {
                    deepest = body.position[1];
                }
            }
            (from - deepest, surface - deepest, body)
        };

        // **Already fully under, so this measures the clamp and nothing else.**
        // Well below the surface rather than a cell under it: a player box is
        // 5.4 cells tall, so feet one cell down is a body four-fifths in the
        // air, which is barely wet and rightly barely slowed.
        //
        // `fluid_terminal_velocity` is 0.9 cells/tick against the dry 11.76, so
        // a submerged body has to stop inside a block.
        let (travelled, _, floated) = plunge(surface - 8.0);
        assert!(
            travelled < CELLS_PER_BLOCK,
            "milk let a terminal-velocity body run {travelled} cells before it caught it"
        );

        // **And from the air, where the timestep sets a floor nothing here can
        // beat.** A tick is 50 ms and terminal velocity is 11.76 cells — 3.92
        // blocks — so a body that is dry when the tick begins and submerged when
        // it ends has already travelled a tick's worth before the milk got a
        // vote. Arresting it inside a further block is the whole of what the
        // clamp can do, and is what this asserts; going below one tick of
        // travel would need sub-tick entry detection, which is a much larger
        // change than the plunge depth justifies.
        let (_, from_air, _) = plunge(surface + 1.0);
        assert!(
            from_air < tuning.terminal_velocity + CELLS_PER_BLOCK,
            "a fall from above sank {from_air} cells, more than the tick of free \
             travel ({}) plus a block it is allowed",
            tuning.terminal_velocity
        );

        // Caught rather than merely slowed: the body ends up floating, and two
        // blocks of milk is the depth the task names as enough to break a fall.
        assert!(
            breaks_a_fall(submersion(
                &pool,
                &Body::at([24.0, surface - 2.0 * CELLS_PER_BLOCK, 24.0]).aabb()
            )),
            "two blocks under the surface did not read as having a fall broken"
        );
        assert!(
            floated.position[1] > surface - CELLS_PER_BLOCK * 2.0,
            "never came back up: rested at {}",
            floated.position[1]
        );
    }

    #[test]
    fn wading_is_slower_than_walking_and_swimming_is_slower_still() {
        /// How far a body walks in `ticks`, from a standing start.
        fn travelled(scene: &impl Solid, intent: Intent, ticks: usize) -> f32 {
            let start = Body::at([24.0, 0.0, 24.0]);
            let body = simulate(scene, start, intent, ticks);
            body.position[0] - start.position[0]
        }

        // The task's "reduced walk speed in shallow milk", and the reason it is
        // a blend rather than a threshold: each of these is the same code with
        // a different fraction.
        let intent = Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: Gait::Walk,
            fly: false,
        };

        let dry = travelled(&DryGround, intent, 60);
        let ankles = travelled(&Pool::new(1).at_level(1), intent, 60);
        let waist = travelled(&Pool::new(1), intent, 60);
        let under = travelled(&Pool::new(4), intent, 60);

        assert!(
            dry > ankles,
            "a puddle did not slow a walk: {dry} then {ankles}"
        );
        assert!(
            ankles > waist,
            "deeper milk did not slow it further: {ankles} then {waist}"
        );
        assert!(
            waist > under,
            "swimming was not slower than wading: {waist} then {under}"
        );
    }

    #[test]
    fn two_fluids_in_one_box_name_the_one_there_is_more_of() {
        // Which fluid a body is "in" has to be a fact rather than whichever
        // block happened to be visited last — a tint that flickered between two
        // fluids at a boundary would be the visible symptom.
        struct Layered;

        impl Solid for Layered {
            fn solid(&self, _x: i32, y: i32, _z: i32) -> bool {
                y < 0
            }

            fn fluid(&self, _x: i32, y: i32, _z: i32) -> Fluid {
                match y {
                    // Oil in the bottom block, milk in the three above it.
                    0 => Fluid::new(OIL, MAX_VOLUME),
                    1..=3 => Fluid::new(MILK, MAX_VOLUME),
                    _ => Fluid::EMPTY,
                }
            }
        }

        // Feet on the bottom: three cells of oil, then 2.4 of milk.
        assert_eq!(
            submersion(&Layered, &Body::at([24.0, 0.0, 24.0]).aabb()).fluid,
            OIL
        );
        // A block higher: no oil at all.
        assert_eq!(
            submersion(&Layered, &Body::at([24.0, 3.0, 24.0]).aabb()).fluid,
            MILK
        );
    }

    #[test]
    fn a_degenerate_box_is_dry_rather_than_a_division_by_zero() {
        // Charter rule 4 bans `NaN` in simulation state outright, and a box with
        // no volume is the one input that would produce one here.
        let flat = Aabb {
            min: [24.0, 1.0, 24.0],
            max: [24.0, 1.0, 24.0],
        };
        assert_eq!(submersion(&Pool::new(4), &flat), Submersion::DRY);
    }

    proptest! {
        #[test]
        fn the_fraction_is_always_a_fraction_and_never_a_nan(
            depth in 0i32..6,
            level in 1u32..=MAX_VOLUME,
            height in -2.0f32..20.0,
        ) {
            // Whatever the scene, the answer is a fraction of a box: in `0..=1`,
            // finite, and zero exactly when nothing wet overlaps.
            let pool = Pool::new(depth).at_level(level as u8);
            let wet = submersion(&pool, &Body::at([24.0, height, 24.0]).aabb());

            prop_assert!(wet.fraction.is_finite(), "not finite: {}", wet.fraction);
            prop_assert!(
                (0.0..=1.0).contains(&wet.fraction),
                "outside 0..=1: {}",
                wet.fraction
            );
            prop_assert_eq!(
                wet.fraction > 0.0,
                !wet.fluid.is_none(),
                "a fraction and a fluid must agree about whether there is any"
            );
        }

        #[test]
        fn a_body_left_in_milk_never_leaves_it_and_never_reaches_the_bottom(
            start in 1.0f32..30.0,
        ) {
            // The float equilibrium is stable from anywhere: dropped from the
            // air or released at the bottom, a body ends up at the surface and
            // stays. A buoyancy that overshot would launch a body out of the
            // pond, and one that undershot would drown it.
            let pool = Pool::new(6);
            let body = simulate(&pool, Body::at([24.0, start, 24.0]), Intent::default(), 300);

            prop_assert!(
                body.position[1] > 1.0,
                "sank to the bottom at {}",
                body.position[1]
            );
            let wet = submersion(&pool, &body.aabb()).fraction;
            prop_assert!(wet > 0.5, "left the milk entirely, {wet} submerged");
        }
    }
}
