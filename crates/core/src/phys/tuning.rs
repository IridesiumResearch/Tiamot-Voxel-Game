// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Every number that decides how movement feels, in one place.
//!
//! # Units
//!
//! All of it is **sub-node cells per tick**, because that is what the physics
//! step works in — the collision grid is one cell wide, so a cell is the unit
//! that makes the arithmetic grid-aligned (see [`super`]). The doc comment on
//! each constant gives the value a human recognises, in yards per second, and
//! the conversion.
//!
//! The conversion factor is 3 cells per yard divided by 20 ticks per second:
//! **yards/s × 0.15 = cells/tick**, and **yards/s² × 0.0075 = cells/tick²**.
//!
//! # Why these numbers
//!
//! They are Minecraft's, converted. The task asks for "Minecraft-grade first
//! person feel", and the numbers that produce it are known rather than worth
//! rediscovering by taste. They are a starting point for the [H] feel gate, not
//! a result — the gate is a human's judgment, and this is the dial they turn.
//!
//! Charter scope discipline applies: this is a mechanism with defaults, not
//! game design. A future movement mod overrides [`Tuning`] wholesale.

/// Cells per yard. Charter rule 5.
const CELLS_PER_YARD: f32 = 3.0;

/// Ticks per second. Mirrors [`crate::tick::TICK_RATE_HZ`] as an `f32` so the
/// conversions below stay in one expression.
const TICKS_PER_SECOND: f32 = 20.0;

/// Converts yards per second to cells per tick.
const fn speed(yards_per_second: f32) -> f32 {
    yards_per_second * CELLS_PER_YARD / TICKS_PER_SECOND
}

/// Converts yards per second squared to cells per tick squared.
const fn accel(yards_per_second_squared: f32) -> f32 {
    yards_per_second_squared * CELLS_PER_YARD / (TICKS_PER_SECOND * TICKS_PER_SECOND)
}

/// The constants a body moves by.
///
/// Passed to [`super::step`] rather than read from a global, so a test can
/// simulate in vacuum or at half gravity without touching the defaults, and so
/// a future mod API can hand a body its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    /// Downward acceleration, cells/tick². 32 yd/s².
    pub gravity: f32,

    /// The fastest a body may fall, cells/tick. 78.4 yd/s.
    ///
    /// A cap rather than air resistance. Without one a long fall reaches a
    /// speed that skips whole chunks between ticks, and the sweep would have to
    /// walk hundreds of cells to catch the landing.
    pub terminal_velocity: f32,

    /// Upward velocity a jump starts with, cells/tick.
    ///
    /// Chosen for the apex, not picked: `sqrt(2 × gravity × height)` clears
    /// 1.25 yards, which is a block and a quarter — enough to jump onto a full
    /// block, not enough to reach two.
    pub jump_speed: f32,

    /// Top horizontal speed walking, cells/tick. 4.3 yd/s.
    pub walk_speed: f32,

    /// Top horizontal speed sprinting, cells/tick. 5.6 yd/s.
    pub sprint_speed: f32,

    /// Top horizontal speed sneaking, cells/tick. 1.3 yd/s.
    pub sneak_speed: f32,

    /// Fraction of horizontal velocity kept per tick while on the ground.
    ///
    /// The counterweight to [`ground_acceleration`](Self::ground_acceleration):
    /// together they settle at `accel × friction / (1 − friction)`, which is
    /// what actually determines top speed. Raising one without the other
    /// changes the top speed, not the responsiveness.
    pub ground_friction: f32,

    /// Fraction of horizontal velocity kept per tick while airborne.
    ///
    /// Near 1: an airborne body keeps its momentum, which is what makes a jump
    /// commit to its arc instead of stopping dead in mid-air.
    pub air_drag: f32,

    /// Horizontal acceleration applied on the ground, cells/tick².
    pub ground_acceleration: f32,

    /// Horizontal acceleration applied in the air, cells/tick².
    ///
    /// A small fraction of the ground figure. Enough to adjust a jump, not
    /// enough to turn one into flight.
    pub air_acceleration: f32,

    /// How high a body will step without jumping, in cells.
    ///
    /// **One cell, and this is a contract value, not a taste one** — Sub-Node
    /// Contract §2 fixes the step at one sub-node (1/3 yard) so a chiselled
    /// surface is climbable and a two-cell lip is not. Changing it means
    /// editing the contract first.
    pub step_height: f32,

    /// How much of a body's weight a full submersion carries.
    ///
    /// **Above one on purpose.** At exactly one a body is weightless wherever it
    /// happens to be and never finds a surface; below one it sinks like a stone.
    /// Above one it rises until it is `1 / buoyancy` submerged and settles
    /// there, which is what "floating" is. 1.25 puts the equilibrium at 80% of
    /// the box under — for a player, the eyes 0.54 cells clear of the surface.
    pub buoyancy: f32,

    /// Fraction of velocity kept per tick while fully submerged.
    ///
    /// Between [`ground_friction`](Self::ground_friction) and
    /// [`air_drag`](Self::air_drag), and it applies to all three axes rather
    /// than the horizontal pair: it is what damps the bob at the surface into a
    /// settle instead of an oscillation that never ends.
    pub fluid_drag: f32,

    /// Upward acceleration from holding jump while submerged, cells/tick².
    pub swim_up: f32,

    /// Downward acceleration from holding sneak while submerged, cells/tick².
    ///
    /// Larger than [`swim_up`](Self::swim_up) because it is working against
    /// buoyancy rather than with it, and diving has to feel deliberate.
    pub swim_down: f32,

    /// Top horizontal speed while fully submerged, cells/tick. 2.0 yd/s.
    pub swim_speed: f32,

    /// Horizontal acceleration while fully submerged, cells/tick².
    ///
    /// Derived from [`swim_speed`](Self::swim_speed) and
    /// [`fluid_drag`](Self::fluid_drag) the same way the ground pair is derived
    /// from each other, and checked by a unit test for the same reason: top
    /// speed is what the two settle at, not what either one says.
    pub swim_acceleration: f32,

    /// The fastest a body sinks through fluid, cells/tick. 6 yd/s.
    ///
    /// Far below [`terminal_velocity`](Self::terminal_velocity), and that gap is
    /// the whole mechanism behind milk breaking a fall.
    pub fluid_terminal_velocity: f32,

    /// Upward velocity from holding jump while breaking the surface, cells/tick.
    ///
    /// **A velocity, not an acceleration, and that is the point.** Swimming up
    /// is an acceleration that fights buoyancy and drag, and it asymptotes
    /// exactly where a body stops being submerged — which is the surface. So a
    /// swimmer holding jump rises to the waterline and stays there forever, and
    /// cannot get out of a pool whose lip is at the waterline, because the one
    /// place the rise weakens to nothing is the place they need to leave from.
    ///
    /// This is the kick that gets them over it: while any part of the body is
    /// out of the fluid, jump sets a floor under the vertical velocity instead
    /// of adding to it. Scaled by how much of the body is still IN the fluid,
    /// because a swimmer with their shoulders clear has less to push against —
    /// so it fades as the body leaves rather than firing at full strength on
    /// the last tick and launching them.
    ///
    /// [`jump_speed`](Self::jump_speed)'s own value, so climbing out of milk
    /// costs the same effort as climbing the equivalent step on land.
    pub surface_leap: f32,
}

impl Tuning {
    /// The defaults, documented field by field above.
    pub const DEFAULT: Self = Self {
        gravity: accel(32.0),
        terminal_velocity: speed(78.4),
        // sqrt(2 × 0.24 × 3.75) for a 1.25-yard apex. Written as a literal
        // because `sqrt` is not permitted in a `const fn`; the unit test
        // `the_jump_speed_clears_a_block_and_a_quarter` checks the arithmetic
        // rather than trusting the comment.
        jump_speed: 1.341_640_8,
        walk_speed: speed(4.3),
        sprint_speed: speed(5.6),
        sneak_speed: speed(1.3),
        // **A little slide.** Reported from the window as wanting movement to
        // start and stop softly rather than snapping: "a tiny bit of slide...
        // when you stop the player slides just a tiny bit to make it feel more
        // interactive and smooth."
        //
        // Was 0.6. Stopping distance is `v × f / (1 − f)`, so this takes it
        // from about a third of a block to about half — noticeable underfoot
        // and nowhere near ice. The acceleration below is derived from it and
        // MUST move with it, or the top speed changes as a side effect.
        ground_friction: 0.7,
        air_drag: 0.91,
        // Derived from the friction and the walk speed: a = v × (1 − f) / f.
        // 0.645 × 0.3 / 0.7. Lower than it was because the friction is higher,
        // which is what keeps the top speed where it was — and is also what
        // gives the start its ramp rather than a step.
        ground_acceleration: 0.276_428_58,
        air_acceleration: 0.06,
        step_height: 1.0,
        buoyancy: 1.25,
        fluid_drag: 0.8,
        // A sustained rise of 2.5 yd/s once the drag has settled, which clears
        // a block every three ticks — fast enough to escape a pond you fell
        // into, slow enough that milk is not a lift.
        swim_up: accel(4.5),
        // And 2 yd/s down, against the buoyancy.
        swim_down: accel(18.0),
        swim_speed: speed(2.0),
        // v × (1 − f) / f, as on the ground: 0.3 × 0.2 / 0.8.
        swim_acceleration: 0.075,
        fluid_terminal_velocity: speed(6.0),
        // **Four fifths of the jump speed**, from the window rather than from a
        // derivation: at the full jump it was reported as "about 20% too
        // strong", which is a judgement only a person can make and is exactly
        // what this constant is for. 1.341_640_8 × 0.8.
        surface_leap: 1.073_312_6,
    };
}

impl Default for Tuning {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// How fast a body is trying to move.
///
/// Not a speed — a *choice of gait*, which the step turns into a speed via
/// [`Tuning`]. Sneak additionally refuses to walk off an edge; see
/// [`super::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Gait {
    /// Ordinary movement.
    #[default]
    Walk,
    /// Faster, and no edge guard.
    Sprint,
    /// Slower, and will not step off a ledge.
    Sneak,
}

impl Gait {
    /// The top speed for this gait, in cells per tick.
    #[must_use]
    pub const fn top_speed(self, tuning: &Tuning) -> f32 {
        match self {
            Self::Walk => tuning.walk_speed,
            Self::Sprint => tuning.sprint_speed,
            Self::Sneak => tuning.sneak_speed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_acceleration_and_the_friction_still_settle_at_the_walk_speed() {
        // **The pair that must move together.** A body accelerating against a
        // friction settles at `a × f / (1 − f)`, so changing one alone changes
        // the TOP SPEED as a side effect — which is how "a tiny bit of slide"
        // would quietly become "and everybody walks faster".
        //
        // Asserted rather than commented, because the derivation lives in a
        // comment beside two literals and nothing else was checking it.
        let tuning = Tuning::DEFAULT;
        let settled =
            tuning.ground_acceleration * tuning.ground_friction / (1.0 - tuning.ground_friction);
        assert!(
            (settled - tuning.walk_speed).abs() < 0.005,
            "acceleration {} against friction {} settles at {settled}, not the walk speed {}",
            tuning.ground_acceleration,
            tuning.ground_friction,
            tuning.walk_speed
        );
    }

    #[test]
    fn a_body_slides_a_little_after_the_keys_are_released() {
        // The reported feel, as a distance. `v × f / (1 − f)` is how far a body
        // travels once nothing is driving it: far enough to notice underfoot
        // and nowhere near ice.
        let tuning = Tuning::DEFAULT;
        let slide = tuning.walk_speed * tuning.ground_friction / (1.0 - tuning.ground_friction);
        let cells = f32::from(u8::try_from(crate::SUBNODES_PER_AXIS).unwrap_or(3));
        assert!(
            slide > cells * 0.3,
            "a stop of {slide} cells is not a slide anybody can feel"
        );
        assert!(
            slide < cells * 0.9,
            "a stop of {slide} cells is most of a block: that is ice, not a slide"
        );
    }

    #[test]
    fn the_jump_speed_clears_a_block_and_a_quarter() {
        // The literal in DEFAULT is a computed value written by hand, which is
        // exactly the kind of number that rots when gravity is retuned. This
        // recomputes it from the gravity actually in force.
        let tuning = Tuning::DEFAULT;
        let apex_cells = 1.25 * CELLS_PER_YARD;
        let expected = (2.0 * tuning.gravity * apex_cells).sqrt();

        assert!(
            (tuning.jump_speed - expected).abs() < 1e-6,
            "jump_speed is {} but {expected} clears {apex_cells} cells at gravity {}",
            tuning.jump_speed,
            tuning.gravity
        );
    }

    #[test]
    fn friction_and_acceleration_settle_at_the_walk_speed() {
        // Top speed is an emergent property of the pair, so a plausible-looking
        // change to either silently changes how fast a player walks. This is
        // the equilibrium of `v = (v + a) × f`.
        let tuning = Tuning::DEFAULT;
        let settled =
            tuning.ground_acceleration * tuning.ground_friction / (1.0 - tuning.ground_friction);

        assert!(
            (settled - tuning.walk_speed).abs() < 0.02,
            "acceleration {} and friction {} settle at {settled} cells/tick, but walk_speed is {}",
            tuning.ground_acceleration,
            tuning.ground_friction,
            tuning.walk_speed
        );
    }

    #[test]
    fn a_sprint_outruns_a_walk_which_outruns_a_sneak() {
        let tuning = Tuning::DEFAULT;
        assert!(Gait::Sprint.top_speed(&tuning) > Gait::Walk.top_speed(&tuning));
        assert!(Gait::Walk.top_speed(&tuning) > Gait::Sneak.top_speed(&tuning));
    }

    #[test]
    fn swim_acceleration_and_fluid_drag_settle_at_the_swim_speed() {
        // The same equilibrium the ground pair has, and the same hazard: a
        // plausible-looking change to either silently changes how fast a player
        // swims, because top speed is what the two settle at rather than what
        // `swim_speed` says.
        let tuning = Tuning::DEFAULT;
        let settled = tuning.swim_acceleration * tuning.fluid_drag / (1.0 - tuning.fluid_drag);

        assert!(
            (settled - tuning.swim_speed).abs() < 0.02,
            "acceleration {} and drag {} settle at {settled} cells/tick, but swim_speed is {}",
            tuning.swim_acceleration,
            tuning.fluid_drag,
            tuning.swim_speed
        );
    }

    #[test]
    fn a_floating_body_keeps_its_eyes_out_of_the_milk() {
        // **Where buoyancy above one actually shows.** A body rises until the
        // fluid carries its weight, which is `1 / buoyancy` of it submerged —
        // and whether that number leaves the eyes above the surface is the
        // difference between floating and drowning while afloat.
        let tuning = Tuning::DEFAULT;
        assert!(
            tuning.buoyancy > 1.0,
            "buoyancy {} is not above one, so a body has no surface to find",
            tuning.buoyancy
        );

        let submerged = super::super::PLAYER_HEIGHT / tuning.buoyancy;
        assert!(
            submerged < super::super::EYE_HEIGHT,
            "a floating body sits {submerged} cells under with its eyes at {}",
            super::super::EYE_HEIGHT
        );
    }

    #[test]
    fn climbing_out_of_milk_is_easier_than_a_standing_jump_but_not_free() {
        // The leap has to clear a bank, or a pool has no exit — and it has to
        // stay under a jump, or milk is a trampoline. Reported from the window
        // at the full jump speed as "about 20% too strong", which is the kind of
        // judgement only a person makes and exactly what this constant is for.
        let tuning = Tuning::DEFAULT;
        assert!(
            tuning.surface_leap < tuning.jump_speed,
            "leaving the water ({}) should not beat jumping off the ground ({})",
            tuning.surface_leap,
            tuning.jump_speed
        );
        // And well clear of the sustained swim, or holding jump underwater
        // would be indistinguishable from breaking the surface.
        assert!(
            tuning.surface_leap > tuning.swim_speed * 2.0,
            "the leap {} is not decisively more than swimming",
            tuning.surface_leap
        );
    }

    #[test]
    fn milk_is_a_far_lower_terminal_velocity_than_air() {
        // The gap IS the fall-breaking mechanism — see `phys::swim::vertical`.
        let tuning = Tuning::DEFAULT;
        assert!(
            tuning.fluid_terminal_velocity * 4.0 < tuning.terminal_velocity,
            "fluid terminal velocity {} is not decisively below the dry {}",
            tuning.fluid_terminal_velocity,
            tuning.terminal_velocity
        );
    }

    #[test]
    fn terminal_velocity_is_slower_than_a_chunk_per_tick() {
        // The sweep walks cell by cell, so a body moving further than a chunk
        // in one tick would make a single step's cost unbounded.
        let tuning = Tuning::DEFAULT;
        assert!(
            tuning.terminal_velocity < crate::CHUNK_SUBNODES as f32,
            "terminal velocity {} exceeds a chunk of {} cells",
            tuning.terminal_velocity,
            crate::CHUNK_SUBNODES
        );
    }
}
