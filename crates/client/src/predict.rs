// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Client-side prediction and reconciliation.
//!
//! # The problem
//!
//! The server is authoritative (charter rule 2), and it is at least a round
//! trip away. A client that waited for the server to say where it was would
//! feel its own latency on every keypress. So it moves immediately and is
//! corrected later — which only works if "later" almost never disagrees.
//!
//! # Why this is not a second physics implementation
//!
//! It runs [`tiamot_core::phys::step`], the same function the server's tick
//! calls, over the same inputs, filling input gaps by the same rule
//! ([`tiamot_core::phys::InputQueue`]). Charter rule 4's determinism is what
//! makes that agreement exact rather than approximate: identical operation
//! sequences on IEEE floats give identical results on every supported target,
//! so a correct prediction differs from the server by *nothing at all*, not by
//! a small amount.
//!
//! A reimplementation "close enough for the client" would drift on every tick
//! and turn reconciliation from a rare event into a constant one.
//!
//! # What reconciliation actually does
//!
//! The naive version — take the server's position and use it — is wrong, and
//! visibly so: by the time a state arrives the client has predicted several
//! more ticks past it, so adopting it wholesale throws away every input still
//! in flight and the player snaps backwards on every packet. Instead:
//!
//! 1. drop the inputs the server has already applied, which
//!    `last_processed_input` names;
//! 2. rewind to the server's state;
//! 3. **replay** the inputs it has not seen yet;
//! 4. compare with what we had predicted, and smooth away the difference.
//!
//! When the prediction was right, step 4 finds nothing and the player sees
//! nothing. That is the normal case.
//!
//! # Smoothing is presentation, and is exempt
//!
//! Charter rule 4 explicitly does not apply to interpolation. The *simulated*
//! body jumps straight to the corrected value; what is smoothed is a visual
//! offset added on the way to the camera, so no approximation ever re-enters
//! the state that gets replayed.

use std::collections::VecDeque;

use tiamot_core::ChunkPos;
use tiamot_core::phys::{self, Body, Intent, Solid, Tuning, voxels::renormalise};

/// How long a correction takes to blend away, in ticks.
///
/// ~100 ms at 20 Hz, which the task names. Long enough that a small correction
/// is a drift rather than a jolt, short enough that the player is never far
/// from where the server has them.
pub const SMOOTH_TICKS: f32 = 2.0;

/// How far wrong a prediction has to be before it snaps instead of blending.
///
/// Two yards, in cells. Past this the client was not slightly wrong — it was
/// somewhere else, usually because it predicted through geometry the server
/// disagreed about. Blending that would walk the player visibly through a wall.
pub const SNAP_DISTANCE: f32 = 6.0;

/// The server's word on where a player is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Authoritative {
    /// The last input tick the server had applied.
    pub last_processed_input: u64,
    /// Chunk half of the position.
    pub chunk: ChunkPos,
    /// Cell offset within it.
    pub local: [f32; 3],
    /// Cells per tick.
    pub velocity: [f32; 3],
    /// Whether the server has the player on the ground.
    pub on_ground: bool,
}

/// A locally predicted body, and the inputs the server has not confirmed.
#[derive(Debug, Clone)]
pub struct Predictor {
    origin: ChunkPos,
    body: Body,
    /// Inputs applied locally but not yet confirmed, oldest first.
    pending: VecDeque<(u64, Intent)>,
    /// Visual-only offset, in cells, blended out over [`SMOOTH_TICKS`].
    error: [f32; 3],
    /// How far the body moved on the most recent tick, in cells.
    ///
    /// Held as a *delta* rather than as the previous position, and that is the
    /// detail that makes it correct rather than merely convenient. A previous
    /// position would be anchored to whichever chunk was the origin when it was
    /// taken, so [`Predictor::settle`] re-homing the origin mid-walk would
    /// leave it a whole chunk out; a delta is the same number in either frame.
    last_step: [f32; 3],
    /// How far the drawn camera trails the body after a step, in cells.
    ///
    /// **Signed.** Positive after a step UP, so the camera is drawn below the
    /// body and rises to meet it; negative after a step DOWN, so it is drawn
    /// above and sinks. Both are teleports of the same size in the same
    /// machinery, and easing only one of them is what makes walking over
    /// chiselled ground read as jerking rather than as undulating.
    ///
    /// See [`Predictor::smooth_step`]. Presentation only: it never reaches the
    /// body, so reconciliation replays exactly what it would have anyway.
    step_lag: f32,
    /// How fast the lag is being eased away, in cells per second.
    ///
    /// Recorded when the step is, so the ease is a constant-rate ramp that
    /// finishes in exactly [`STEP_SMOOTHING`] whatever the step's size or the
    /// frame rate. See [`Predictor::smooth_step`].
    step_rate: f32,
    /// The last tick predicted.
    tick: u64,
}

/// Whether a tick should record the camera's step ease.
///
/// A tick is simulated twice: once when it is predicted, and again on every
/// reconcile until the server confirms it. The BODY must be identical both times
/// — that is what reconciliation is — but the ease is a one-off visual event and
/// recording it on every replay pumps it twenty times a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ease {
    /// A tick being lived through for the first time.
    Record,
    /// A tick being re-simulated, whose ease has already been recorded.
    Replay,
}

/// How long the camera takes to catch up after a step, in seconds.
///
/// A tenth of a second: long enough to turn a one-tick teleport into a rise the
/// eye reads as movement, short enough that the camera is never meaningfully
/// behind where the player is standing. Longer feels like the world is on a
/// spring; shorter does not smooth anything at 20 Hz.
const STEP_SMOOTHING: f32 = 0.1;

/// The most the camera will ever trail the body, in cells.
///
/// Three cells is a whole block. A body climbing stairs faster than the camera
/// catches up would otherwise accumulate lag without bound and end up looking
/// out of the floor.
const MAX_STEP_LAG: f32 = 3.0;

// A proportional decay with a floor under it used to do this work. It was
// replaced because it did not do what its own constant said: `STEP_SMOOTHING` is
// 0.1 s and a one-cell step took **167 ms** to disappear, measured at 60, 240 and
// 1200 fps. That matters exactly when steps come close together — walking over
// ground with single raised cells in it steps about every 200 ms — because an
// ease that outlasts the gap never finishes, and the camera is then permanently
// mid-catch-up in one direction or the other. Reported from the window as being
// "vibrated up and down very aggressively" while walking off a one-sub-node lip.

impl Predictor {
    /// A predictor starting from a spawn position.
    #[must_use]
    pub fn new(origin: ChunkPos, local: [f32; 3], tick: u64) -> Self {
        Self {
            origin,
            body: Body::at(local),
            pending: VecDeque::new(),
            error: [0.0; 3],
            last_step: [0.0; 3],
            step_lag: 0.0,
            step_rate: 0.0,
            tick,
        }
    }

    /// The simulated body. Where the player *is*, not where they are drawn.
    #[must_use]
    pub const fn body(&self) -> &Body {
        &self.body
    }

    /// The chunk the body's local coordinates are relative to.
    #[must_use]
    pub const fn origin(&self) -> ChunkPos {
        self.origin
    }

    /// How many inputs are awaiting confirmation.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// The correction still being blended away, in cells.
    ///
    /// Exposed for the HUD: a number that is never zero is the visible symptom
    /// of a prediction that disagrees with the server every tick, which is
    /// otherwise very hard to notice and very bad.
    #[must_use]
    pub fn error(&self) -> f32 {
        let [x, y, z] = self.error;
        (x * x + y * y + z * z).sqrt()
    }

    /// How much of the outstanding correction is vertical, `0.0..=1.0`.
    ///
    /// **The half of a correction that says what kind it is.** A disagreement
    /// about a JUMP is almost entirely vertical — losing the tick that carried a
    /// press moves a body by a whole arc and nothing sideways — while one about
    /// walking into geometry is mostly horizontal. The magnitude alone cannot
    /// tell those apart, and a 5.37 that could have been either cost a round trip
    /// to find out.
    #[must_use]
    pub fn vertical_share(&self) -> f32 {
        let [x, y, z] = self.error;
        let total = (x * x + y * y + z * z).sqrt();
        if total <= 0.0 {
            return 0.0;
        }
        y.abs() / total
    }

    /// Advances one tick locally and records the input for replay.
    pub fn predict(&mut self, solid: &impl Solid, tick: u64, intent: Intent, tuning: &Tuning) {
        self.step(solid, intent, tuning, Ease::Record);
        self.pending.push_back((tick, intent));
        self.tick = tick;

        // Bounded by the same lookahead the server accepts. Anything older than
        // that will never be confirmed because the server would refuse it.
        while self.pending.len() > phys::input::MAX_LOOKAHEAD as usize {
            self.pending.pop_front();
        }
    }

    /// Takes the server's answer and replays what it has not seen.
    pub fn reconcile(&mut self, solid: &impl Solid, state: &Authoritative, tuning: &Tuning) {
        // Where we thought we were, before adopting the server's answer. The
        // difference between this and the replayed result IS the correction.
        let predicted = self.world_position();

        self.pending
            .retain(|(tick, _)| *tick > state.last_processed_input);

        self.origin = state.chunk;
        self.body = Body {
            position: state.local,
            velocity: state.velocity,
            on_ground: state.on_ground,
        };

        // Replay: the inputs the server had not applied when it spoke. Skipping
        // this is the mistake that makes a client rubber-band under any latency
        // at all — it would throw away every input still in flight.
        let replay: Vec<(u64, Intent)> = self.pending.iter().copied().collect();
        for (_, intent) in replay {
            // **`Ease::Replay`, and this is the bug that made the world shake.**
            //
            // A reconcile re-simulates every input the server has not confirmed,
            // which is up to `MAX_LOOKAHEAD` ticks, and it happens on every
            // server state message — twenty times a second. Recording the step
            // ease here as well meant a single step-up was counted again on
            // every reconcile for as long as its tick sat unconfirmed, pumping
            // `step_lag` to its `MAX_STEP_LAG` ceiling of three cells — a whole
            // block — while the ease pulled the other way.
            //
            // Reported from the window, and described exactly: "an up force is
            // being applied and that makes our body want to enter the block
            // above and the block above is rejecting us and as the two forces
            // collide we vibrate". Two forces, and this was one of them.
            //
            // The body is untouched by any of it — which is why the correction
            // read 0.00 throughout and every trace of the simulation came back
            // clean. Only what was DRAWN was moving.
            self.step(solid, intent, tuning, Ease::Replay);
        }

        let corrected = self.world_position();
        let offset = [
            predicted[0] - corrected[0],
            predicted[1] - corrected[1],
            predicted[2] - corrected[2],
        ];

        let distance = {
            let [x, y, z] = offset;
            ((x * x + y * y + z * z) as f32).sqrt()
        };
        self.error = if distance > SNAP_DISTANCE {
            // Not slightly wrong — somewhere else. Blending would drag the
            // player visibly through whatever the two disagreed about.
            [0.0; 3]
        } else {
            [offset[0] as f32, offset[1] as f32, offset[2] as f32]
        };
    }

    /// Blends the visual correction away. Call once per rendered frame.
    ///
    /// Presentation only, and charter rule 4 exempts it — the simulated body
    /// has already been corrected, so nothing approximate re-enters the state
    /// that gets replayed.
    pub fn smooth(&mut self, dt_ticks: f32) {
        let keep = 1.0 - (dt_ticks / SMOOTH_TICKS).clamp(0.0, 1.0);
        for axis in 0..3 {
            self.error[axis] *= keep;
        }
    }

    /// Where to draw the body: the simulated position plus what is left of the
    /// correction.
    #[must_use]
    pub fn render_local(&self) -> [f32; 3] {
        self.render_local_at(1.0)
    }

    /// Where to draw the body `alpha` of the way through the current tick.
    ///
    /// **This is what stops walking looking like 20 fps on a 900 fps machine.**
    /// The simulation is a fixed 20 Hz because charter rule 4 requires it, so
    /// the body occupies exactly 20 positions a second however many frames are
    /// drawn. Pinning the camera to it makes a staircase — and because
    /// mouse-look *is* per-frame, the result is a view that turns smoothly and
    /// walks in visible steps, which reads as a frame-rate problem rather than
    /// a sampling one. Charter rule 18 measures pacing, and this is pacing.
    ///
    /// `alpha` is how much of a tick has accumulated since the last one, so
    /// this interpolates between the previous tick and the current one rather
    /// than extrapolating past it. That trades up to one tick of camera latency
    /// — 50 ms, and half that on average — for never overshooting, and
    /// overshoot is what the player would actually notice: an extrapolated
    /// camera slides past every wall it stops at and snaps back.
    ///
    /// Presentation only. Charter rule 4 exempts it and nothing here re-enters
    /// the body that gets replayed, which is why it may interpolate at all.
    #[must_use]
    pub fn render_local_at(&self, alpha: f32) -> [f32; 3] {
        let behind = 1.0 - alpha.clamp(0.0, 1.0);
        [
            self.body.position[0] - self.last_step[0] * behind + self.error[0],
            self.body.position[1] - self.last_step[1] * behind + self.error[1] - self.step_lag,
            self.body.position[2] - self.last_step[2] * behind + self.error[2],
        ]
    }

    /// Eases the camera up after a step, and reports where it now stands.
    ///
    /// # Why a step needs its own smoothing at all
    ///
    /// **Step-up is a teleport, and the tick interpolation cannot hide it.**
    /// Sub-Node Contract §2 lifts a blocked body one sub-node in a single tick
    /// with no vertical velocity, so the body's height jumps a third of a block
    /// between two ticks and then holds. Interpolating between those two ticks
    /// spreads the jump over 50 ms and no further, so walking up a chiselled
    /// slope reads as *pop, flat, flat, flat, pop* — which is exactly what a
    /// player means by "the motion is not fluid". It was reported from the
    /// window before this existed.
    ///
    /// The remedy is a camera that lags the body and catches up: the step is
    /// recorded as an offset, subtracted from the drawn height, and decayed to
    /// nothing over [`STEP_SMOOTHING`]. **The body is untouched** — this is
    /// presentation, charter rule 4 exempts it, and nothing here re-enters what
    /// gets replayed for reconciliation.
    ///
    /// # Steps are smoothed in both directions; falls are not smoothed at all
    ///
    /// This once eased upward steps only, on the reasoning that "falling is a
    /// real acceleration the player should feel". That reasoning is sound for a
    /// FALL and wrong for a step DOWN, and the contract now has both: contract
    /// §2's step-down places a body a sub-node lower **in one tick with no
    /// vertical velocity**, which is the same teleport as a step-up with the
    /// sign flipped. Left unsmoothed it pops, and walking over ground with
    /// raised cells in it — which alternates up-step and down-step every few
    /// ticks — came out eased on the way up and hard on the way down. Reported
    /// from the window as "extremely unstable physics collision where I am
    /// bouncing and jerking around".
    ///
    /// A real fall is still felt in full. The two are told apart by whether the
    /// body was on the ground when the tick began: a step-down starts and ends
    /// on the ground, and a landing does not — see [`Predictor::step`].
    /// **Decay only.** The step itself is recorded once, by the tick that made
    /// it — see [`Predictor::step`]. Recording it here instead was the first
    /// version and it was wrong: this runs once per FRAME and a tick lasts
    /// several frames, so one step was counted three times at 60 fps and the
    /// camera sank further with every stair.
    pub fn smooth_step(&mut self, dt: f32) {
        if self.step_lag == 0.0 {
            return;
        }
        // Symmetric: the magnitude decays and the sign is put back. Written this
        // way rather than as two branches because the two directions must catch
        // up at exactly the same rate — an eye reading a rise and a fall at
        // different speeds is the artefact this whole function exists to remove.
        // Self-healing: a lag that arrived without a rate — a test setting the
        // field, or any future caller — still eases away in one window rather
        // than sticking for ever at whatever it was.
        if self.step_rate <= 0.0 {
            self.step_rate = self.step_lag.abs() / STEP_SMOOTHING;
        }

        // A constant-rate ramp, not a decay. Linear means it arrives — an
        // exponential approaches zero and needs a floor bolted under it to
        // finish, which is what used to overrun the window — and it means the
        // camera moves at one speed through the whole ease rather than lurching
        // at the start and crawling at the end.
        let remaining = (self.step_lag.abs() - self.step_rate * dt).max(0.0);
        self.step_lag = if self.step_lag < 0.0 {
            -remaining
        } else {
            remaining
        };
        if remaining == 0.0 {
            self.step_rate = 0.0;
        }
    }

    /// How far the drawn camera currently trails the body, in cells.
    #[must_use]
    pub const fn step_lag(&self) -> f32 {
        self.step_lag
    }

    /// One simulation tick, recording how far it moved the body.
    ///
    /// Prediction and the reconcile replay both go through here so they cannot
    /// disagree about [`Predictor::last_step`]. The delta is measured *before*
    /// [`Predictor::settle`], while both positions are still anchored to the
    /// same origin — after it, a body that crossed a chunk boundary would
    /// subtract two coordinates from different frames and report a 48-cell step.
    fn step(&mut self, solid: &impl Solid, intent: Intent, tuning: &Tuning, ease: Ease) {
        let before = self.body.position;
        let was_on_ground = self.body.on_ground;
        self.body = phys::step(solid, self.body, intent, tuning);
        self.last_step = [
            self.body.position[0] - before[0],
            self.body.position[1] - before[1],
            self.body.position[2] - before[2],
        ];

        // A rise with no upward velocity is a step-up rather than a jump: a
        // jump has velocity and should be felt immediately. Recorded here, once
        // per tick, and eased away per frame by `smooth_step`.
        let rise = self.last_step[1];
        // **A step-up begins and ends on the ground.** Testing "rose, and has no
        // upward velocity now" instead caught a jump that bumped its head: the
        // body rose, the ceiling zeroed the velocity, and the two are
        // indistinguishable after the fact. The camera then eased a rise the
        // player had felt as a jump, and eased it just as the body started
        // falling again — reported from the window as jerking "up and down only
        // for a split second" while jumping past a floating block, which is
        // exactly a body with 0.6 cells of headroom under it.
        //
        // Ending airborne is what a truncated jump does and what a step-up never
        // does: a step lifts onto something and `resolve_vertical` finds it
        // there in the same tick.
        let stepped_up = rise > 0.0 && was_on_ground && self.body.on_ground;
        // The mirror of it. **A step-down is not a fall**: it begins and ends on
        // the ground, which is exactly what tells the two apart — a body that
        // lands from a fall was airborne when the tick began, and its landing is
        // an acceleration the player should feel in full.
        let stepped_down = rise < 0.0 && was_on_ground && self.body.on_ground;

        if ease == Ease::Record {
            if stepped_up {
                self.step_lag = (self.step_lag + rise).min(MAX_STEP_LAG);
            } else if stepped_down {
                self.step_lag = (self.step_lag + rise).max(-MAX_STEP_LAG);
            }
        }
        if stepped_up || stepped_down {
            // Re-derived from whatever is now outstanding rather than added to,
            // so a step taken while the previous one is still easing still
            // finishes within one window instead of extending it.
            if ease == Ease::Record {
                self.step_rate = self.step_lag.abs() / STEP_SMOOTHING;
            }
        }

        // **And the step leaves the tick interpolation, or it is applied twice.**
        //
        // `render_local_at` walks the body back along `last_step` to find where
        // it was at the start of the tick, and `step_lag` walks it back again by
        // the same sub-node — so the drawn camera dived a full TWO cells on the
        // frame a step landed and then climbed out of it. Measured over ground
        // that steps every few ticks: 0.583 cells of movement in a single frame,
        // which is most of the jerk the easing was supposed to remove.
        //
        // The vertical part of the step now belongs to the easing alone. Every
        // other kind of vertical movement — falling, jumping, being pushed —
        // still interpolates, because none of them is a teleport.
        if stepped_up || stepped_down {
            self.last_step[1] = 0.0;
        }

        self.settle();
    }

    /// Keeps the local coordinates inside the origin chunk (charter rule 7).
    fn settle(&mut self) {
        let (origin, local) = renormalise(self.origin, self.body.position);
        self.origin = origin;
        self.body.position = local;
    }

    /// The body in world cells, as `f64`, for comparing two frames.
    ///
    /// `f64` because the two positions being compared may be anchored to
    /// different chunks, and the subtraction has to happen somewhere wide
    /// enough to hold both. This is a presentation measurement, not simulation
    /// state.
    fn world_position(&self) -> [f64; 3] {
        let span = f64::from(tiamot_core::CHUNK_SUBNODES);
        [
            f64::from(self.origin.x) * span + f64::from(self.body.position[0]),
            f64::from(self.origin.y) * span + f64::from(self.body.position[1]),
            f64::from(self.origin.z) * span + f64::from(self.body.position[2]),
        ]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_step_up_is_eased_rather_than_teleported() {
        // **The reported bug**: "jumping past a block into a hole ... the
        // motion isn't fluid". Sub-Node Contract §2 lifts a blocked body one
        // sub-node in a single tick with no vertical velocity, so the drawn
        // height jumps a third of a block between two ticks and then holds.
        // Interpolating between ticks spreads that over 50 ms and no further,
        // which reads as pop, flat, flat, pop.
        let mut predictor = Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0], 0);

        // What a tick that stepped up leaves behind, exactly as
        // `resolve_horizontal` does: a rise with no upward velocity.
        predictor.last_step = [0.0, 1.0, 0.0];
        predictor.body.position[1] += 1.0;
        predictor.body.velocity[1] = 0.0;
        predictor.step_lag = 1.0;

        let lagged = predictor.render_local_at(1.0)[1];
        assert!(
            lagged < predictor.body.position[1],
            "the drawn camera is not behind the body: {lagged} against {}",
            predictor.body.position[1]
        );

        // **The body is untouched.** Anything else would feed the smoothing
        // back into what reconciliation replays, and presentation must not.
        assert!(
            (predictor.body.position[1] - 1.0).abs() < 1e-6,
            "smoothing moved the body: {}",
            predictor.body.position[1]
        );

        // And it catches up rather than leaving the world permanently low.
        // Sixty frames is a second, six times the smoothing window.
        for _ in 0..60 {
            predictor.smooth_step(1.0 / 60.0);
        }
        assert!(
            predictor.step_lag() == 0.0,
            "the camera never finished catching up: {} cells behind",
            predictor.step_lag()
        );
    }

    #[test]
    fn falling_is_not_smoothed() {
        // Only upward steps are eased. Falling is a real acceleration a player
        // should feel, and easing it makes every drop feel like a float.
        let mut predictor = Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 10.0, 24.0], 0);
        predictor.last_step = [0.0, -2.0, 0.0];
        predictor.body.velocity[1] = -2.0;

        // Nothing recorded it, because only a tick records a step and a fall is
        // not one. Compared against zero exactly rather than `<= 0`: the lag is
        // signed now, and a step DOWN is legitimately negative, so "no lag" and
        // "eased downward" would otherwise be the same assertion.
        assert!(
            predictor.step_lag() == 0.0,
            "a fall was smoothed: {} cells of lag",
            predictor.step_lag()
        );
    }

    #[test]
    fn a_jump_that_bumps_its_head_is_not_eased_like_a_step() {
        // **Reported from the window: jerking "up and down only for a split
        // second" while jumping past a floating block.** A body with 0.6 cells
        // of headroom rises into the block and the bump zeroes its vertical
        // velocity, which after the fact is indistinguishable from a step-up —
        // rose, no upward velocity — so the camera eased a rise the player had
        // felt as a jump, right as the body began falling again.
        //
        // A step-up ends ON the ground; a truncated jump ends airborne.
        let mut ground = Ground::flat();
        // A ceiling 6 cells up: a standing body is 5.4 tall, so this leaves the
        // 0.6 cells that made the report reproducible.
        for x in -64..64 {
            for y in 6..9 {
                for z in -64..64 {
                    ground.0.insert((x, y, z));
                }
            }
        }

        let mut client = predictor();
        client.body.on_ground = true;
        let jumping = Intent {
            walk: [0.0, 0.0],
            jump: true,
            gait: Gait::Walk,
        };
        client.predict(&ground, 1, jumping, &Tuning::DEFAULT);

        assert!(
            client.body().position[1] > 0.0,
            "the jump never left the floor, so this proves nothing: {:?}",
            client.body()
        );
        assert!(
            !client.body().on_ground,
            "the jump ended on the ground, so it was not truncated in mid-air"
        );
        assert_eq!(
            client.step_lag().to_bits(),
            0.0f32.to_bits(),
            "a jump into a ceiling was eased as though it were a step: {} cells of lag",
            client.step_lag()
        );
    }

    #[test]
    fn a_jump_is_felt_immediately_rather_than_eased() {
        // A jump has upward velocity; a step does not. Easing a jump would take
        // the punch out of the one movement players notice most.
        let mut predictor = Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0], 0);
        predictor.last_step = [0.0, 1.5, 0.0];
        predictor.body.velocity[1] = 1.5;

        assert!(
            predictor.step_lag() == 0.0,
            "a jump was smoothed: {} cells of lag",
            predictor.step_lag()
        );
    }

    use std::collections::BTreeSet;

    use tiamot_core::phys::Gait;

    use super::*;

    /// A floor at cell 0 and nothing else.
    struct Ground(BTreeSet<(i32, i32, i32)>);

    impl Ground {
        fn flat() -> Self {
            Self(BTreeSet::new())
        }

        /// A one-cell lip filling everything from `x` eastward, which a walking
        /// body steps up onto.
        fn with_step(mut self, x: i32) -> Self {
            for at in x..64 {
                for z in -64..64 {
                    self.0.insert((at, 0, z));
                }
            }
            self
        }

        fn with_wall(mut self, x: i32) -> Self {
            for y in 0..8 {
                for z in -64..64 {
                    self.0.insert((x, y, z));
                }
            }
            self
        }
    }

    impl Solid for Ground {
        fn solid(&self, x: i32, y: i32, z: i32) -> bool {
            y < 0 || self.0.contains(&(x, y, z))
        }
    }

    fn walking() -> Intent {
        Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: Gait::Walk,
        }
    }

    fn predictor() -> Predictor {
        Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0], 0)
    }

    #[test]
    fn a_reconcile_does_not_re_record_the_steps_it_replays() {
        // **The bug that made the world shake.** A reconcile re-simulates every
        // unconfirmed input, on every server message — twenty times a second —
        // and the step ease was recorded by the same function. So one step-up
        // was counted again on every reconcile for as long as its tick sat
        // unconfirmed, driving `step_lag` to its three-cell ceiling — a whole
        // block — while the ease pulled the other way.
        //
        // Reported from the window as the body "vibrating up and down violently"
        // in third person, with the right instinct about it: "an up force is
        // being applied ... and as the two forces collide we vibrate."
        //
        // The server here AGREES exactly, so the correction is zero and the body
        // is untouched: what is under test is purely what gets drawn.
        // The lip is east of where the body starts, so it is walked INTO rather
        // than stood inside — `predictor()`'s spawn at x = 24 sits within any
        // step built from there, which is how the first version of this test
        // came to pass against both the bug and the fix.
        let ground = Ground::flat().with_step(28);
        let mut client = Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0], 0);
        client.body.on_ground = true;

        // Walk up to the lip but not over it, and remember the world as the
        // server would last have seen it — BEFORE the step.
        for tick in 1..=4 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }
        let confirmed = Authoritative {
            last_processed_input: 4,
            chunk: client.origin(),
            local: client.body().position,
            velocity: client.body().velocity,
            on_ground: client.body().on_ground,
        };

        // Then step up. These ticks are unconfirmed, so every reconcile until
        // the server catches up replays them — step and all.
        for tick in 5..=12 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }
        let once = client.step_lag();
        assert!(
            once > 0.0,
            "the walk never climbed the lip, so this test proves nothing"
        );

        // Five server messages that confirm nothing new — which is what arriving
        // twenty times a second with inputs in flight looks like.
        for _ in 0..5 {
            client.reconcile(&ground, &confirmed, &Tuning::DEFAULT);
        }

        assert!(
            client.step_lag() <= once,
            "five reconciles grew the camera's step lag from {once} to {} cells while the body \
             ended up in exactly the same place",
            client.step_lag()
        );
    }

    #[test]
    fn a_step_eases_away_in_exactly_the_window_it_documents() {
        // **`STEP_SMOOTHING` said 0.1 s and the ease took 167 ms**, measured at
        // 60, 240 and 1200 fps. The old shape was a proportional decay with a
        // floor bolted under it to make it finish, and the two together overran
        // the window by two thirds.
        //
        // That is not a rounding error, it is the difference between an ease
        // that finishes between steps and one that does not. Walking over ground
        // with single raised cells in it steps about every 200 ms, so an ease
        // outlasting its own window leaves the camera permanently mid-catch-up
        // — reported from the window as being "vibrated up and down very
        // aggressively" while walking off a one-sub-node lip.
        for fps in [20.0f32, 60.0, 240.0, 1200.0] {
            let mut predictor = Predictor::new(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0], 0);
            predictor.step_lag = 1.0;

            let dt = 1.0 / fps;
            let mut elapsed = 0.0;
            while predictor.step_lag() != 0.0 && elapsed < 1.0 {
                predictor.smooth_step(dt);
                elapsed += dt;
            }

            // One frame of slack, because the last frame of the ramp can only
            // land on a frame boundary.
            assert!(
                elapsed <= STEP_SMOOTHING + dt,
                "at {fps} fps a one-cell step took {elapsed} s to ease away, against a window of \
                 {STEP_SMOOTHING} s"
            );
            assert!(
                elapsed >= STEP_SMOOTHING - dt,
                "at {fps} fps the ease finished in {elapsed} s, faster than the window it is \
                 supposed to spread the step over"
            );
        }
    }

    #[test]
    fn the_drawn_camera_never_jerks_over_ground_that_makes_the_body_bounce() {
        // **Reported from the window: "extremely unstable physics collision
        // where I am bouncing and jerking around ... when walking over subnodes
        // or nodes missing subnodes and when jumping in a tunnel."**
        //
        // The body genuinely does go up and down here, and it should: a floor
        // with single raised cells in it is a floor with single raised cells in
        // it. Searching every 9-cell pattern for the worst case found cells at
        // x = 2 and x = 6, where the body's 1.8-cell footprint gains and loses
        // support every few ticks — **eleven vertical reversals in forty ticks**,
        // each one a full sub-node.
        //
        // What must not happen is that the CAMERA takes those in one frame. The
        // up-steps were eased and the down-steps were not, so the same walk read
        // as smooth on the way up and a hard drop on the way down.
        // Three frames a tick, which is what 60 fps against a 20 Hz tick gives.
        const FRAMES: usize = 3;

        let mut ground = Ground::flat();
        for x in [2, 6, 11, 15, 20, 24, 29, 33] {
            for z in -64..64 {
                ground.0.insert((x, 0, z));
            }
        }

        let mut client = predictor();
        client.body.position = [0.5, 1.0, 24.0];
        client.body.on_ground = true;
        let mut previous = client.render_local_at(0.0)[1];
        let mut worst: f32 = 0.0;
        for tick in 1..=40 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
            for frame in 1..=FRAMES {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a frame index, not a measurement"
                )]
                let alpha = frame as f32 / FRAMES as f32;
                client.smooth_step(1.0 / (20.0 * FRAMES as f32));
                let drawn = client.render_local_at(alpha)[1];
                worst = worst.max((drawn - previous).abs());
                previous = drawn;
            }
        }

        // A full sub-node is 1.0. Unsmoothed, a step-down delivers all of it in
        // the single frame the tick landed on; eased over `STEP_SMOOTHING` at 60
        // fps it arrives about a sixth at a time. A third of a cell is
        // comfortably between the two, and is an eighth of a block — below what
        // reads as a jolt.
        assert!(
            worst < 0.34,
            "the drawn camera moved {worst} cells in one frame over ground the body bounces on, \
             which is the jerk this easing exists to remove"
        );
    }

    #[test]
    fn the_body_is_drawn_between_ticks_and_not_only_on_them() {
        // The bug: the camera was pinned to the simulated body, which advances
        // exactly 20 times a second, so walking looked like 20 fps on a machine
        // drawing 900. Sampling within one tick has to produce distinct,
        // advancing positions.
        let ground = Ground::flat();
        let mut client = predictor();
        for tick in 1..=4 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        let samples: Vec<f32> = (0..=4)
            .map(|i| client.render_local_at(i as f32 / 4.0)[0])
            .collect();

        // The counter-example that makes this non-vacuous: the OLD code
        // returned the body position whatever the alpha, so every sample here
        // would be identical and the walk would be a staircase.
        assert!(
            samples[0] < samples[4],
            "sampling across one tick gave {samples:?}, which never moves — the camera is \
             still pinned to the tick boundary"
        );
        for pair in samples.windows(2) {
            assert!(
                pair[1] > pair[0],
                "the interpolated position went backwards inside a tick: {samples:?}"
            );
        }

        // And the two ends are exactly the tick's start and finish, so the
        // camera neither lags further than one tick nor runs ahead of the
        // simulation.
        assert_eq!(
            samples[4].to_bits(),
            client.render_local()[0].to_bits(),
            "a full alpha must land on the simulated body, not past it"
        );
        let step = client.last_step[0];
        assert!(
            (samples[0] - (samples[4] - step)).abs() < 1e-4,
            "a zero alpha should sit one whole tick behind; it was {} rather than {}",
            samples[0],
            samples[4] - step
        );
    }

    #[test]
    fn interpolation_survives_the_body_changing_origin_chunk() {
        // `settle` re-homes the origin when the body leaves its chunk, so a
        // previous *position* would be measured against a different corner and
        // the camera would fly a whole chunk sideways for one frame. Holding a
        // delta instead is what makes this pass; the test walks far enough to
        // guarantee at least one re-home.
        let ground = Ground::flat();
        let mut client = Predictor::new(ChunkPos::new(0, 0, 0), [47.0, 0.0, 24.0], 0);
        let start = client.origin();

        let mut worst = 0.0f32;
        for tick in 1..=60 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
            let span = client.render_local_at(1.0)[0] - client.render_local_at(0.0)[0];
            worst = worst.max(span.abs());
        }

        assert_ne!(
            client.origin(),
            start,
            "the body never left its chunk, so this never exercised the re-home it is about"
        );
        assert!(
            worst < 8.0,
            "one tick of interpolation spanned {worst} cells; a chunk is {}, so the origin \
             re-home leaked into the drawn position",
            tiamot_core::CHUNK_SUBNODES
        );
    }

    #[test]
    fn a_client_moves_without_waiting_for_the_server() {
        // The entire point. If this needed a round trip the player would feel
        // their ping on every keypress.
        let ground = Ground::flat();
        let mut predictor = predictor();
        let start = predictor.body().position[0];

        for tick in 1..=10 {
            predictor.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        assert!(
            predictor.body().position[0] > start,
            "the client did not move on its own"
        );
        assert_eq!(predictor.pending(), 10, "every input awaits confirmation");
    }

    #[test]
    fn a_server_state_that_agrees_corrects_nothing() {
        // The normal case, and the one that must be silent. Determinism is what
        // makes it exact: the same inputs through the same code give the same
        // answer, so a correct prediction differs by nothing at all.
        let ground = Ground::flat();
        let mut client = predictor();
        let mut server = predictor();

        for tick in 1..=6 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
            server.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        client.reconcile(
            &ground,
            &Authoritative {
                last_processed_input: 6,
                chunk: server.origin(),
                local: server.body().position,
                velocity: server.body().velocity,
                on_ground: server.body().on_ground,
            },
            &Tuning::DEFAULT,
        );

        assert_eq!(
            client.error().to_bits(),
            0.0f32.to_bits(),
            "an agreeing server produced a correction of {}",
            client.error()
        );
        assert_eq!(client.pending(), 0, "confirmed inputs should be dropped");
    }

    #[test]
    fn inputs_the_server_has_not_seen_are_replayed_rather_than_discarded() {
        // Without replay the client snaps back to an old position on every
        // packet — it would be throwing away every input still in flight, which
        // under real latency is most of them.
        let ground = Ground::flat();
        let mut client = predictor();

        for tick in 1..=10 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }
        let predicted_x = client.body().position[0];

        // The server has only seen the first four, and agreed about them.
        let mut server = predictor();
        for tick in 1..=4 {
            server.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        client.reconcile(
            &ground,
            &Authoritative {
                last_processed_input: 4,
                chunk: server.origin(),
                local: server.body().position,
                velocity: server.body().velocity,
                on_ground: server.body().on_ground,
            },
            &Tuning::DEFAULT,
        );

        assert_eq!(
            client.body().position[0].to_bits(),
            predicted_x.to_bits(),
            "replaying six unconfirmed inputs should land exactly where the client already was"
        );
        assert_eq!(client.pending(), 6, "the unconfirmed six are still pending");
    }

    #[test]
    fn a_disagreement_is_blended_rather_than_snapped() {
        let ground = Ground::flat();
        let mut client = predictor();
        for tick in 1..=5 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        // The server has the player a little behind where the client thinks.
        let mut local = client.body().position;
        local[0] -= 1.0;
        client.reconcile(
            &ground,
            &Authoritative {
                last_processed_input: 5,
                chunk: client.origin(),
                local,
                velocity: [0.0; 3],
                on_ground: true,
            },
            &Tuning::DEFAULT,
        );

        assert!(client.error() > 0.5, "the correction was not recorded");
        // The drawn position still includes the error, so the player has not
        // jumped; the simulated one has already been corrected.
        assert!(
            (client.render_local()[0] - client.body().position[0]).abs() > 0.5,
            "the correction was applied to the body instead of to the drawing"
        );

        for _ in 0..8 {
            client.smooth(1.0);
        }
        assert!(
            client.error() < 0.01,
            "the correction never blended away: {}",
            client.error()
        );
    }

    #[test]
    fn a_correction_bigger_than_two_yards_snaps_instead() {
        // Past this the client did not mispredict slightly, it predicted
        // through something. Blending would walk the player through a wall in
        // full view.
        let ground = Ground::flat();
        let mut client = predictor();
        for tick in 1..=5 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }

        let mut local = client.body().position;
        local[0] -= SNAP_DISTANCE + 2.0;
        client.reconcile(
            &ground,
            &Authoritative {
                last_processed_input: 5,
                chunk: client.origin(),
                local,
                velocity: [0.0; 3],
                on_ground: true,
            },
            &Tuning::DEFAULT,
        );

        assert_eq!(
            client.error().to_bits(),
            0.0f32.to_bits(),
            "a correction of {} cells should snap, not blend",
            SNAP_DISTANCE + 2.0
        );
        assert_eq!(
            client.render_local().map(f32::to_bits),
            client.body().position.map(f32::to_bits),
            "a snap draws the corrected position immediately"
        );
    }

    #[test]
    fn a_prediction_through_a_wall_is_corrected_by_the_replay() {
        // The case reconciliation exists for: the client had not received the
        // chunk holding a wall, walked through where it is, and the server
        // knows better. The replay runs the same inputs against the client's
        // now-updated world.
        let mut client = predictor();
        let open = Ground::flat();
        for tick in 1..=20 {
            client.predict(&open, tick, walking(), &Tuning::DEFAULT);
        }
        assert!(client.body().position[0] > 27.0, "should have walked east");

        // The wall arrives, and so does a server state from before it mattered.
        let walled = Ground::flat().with_wall(27);
        let mut server = predictor();
        for tick in 1..=20 {
            server.predict(&walled, tick, walking(), &Tuning::DEFAULT);
        }

        client.reconcile(
            &walled,
            &Authoritative {
                last_processed_input: 20,
                chunk: server.origin(),
                local: server.body().position,
                velocity: server.body().velocity,
                on_ground: server.body().on_ground,
            },
            &Tuning::DEFAULT,
        );

        assert!(
            client.body().position[0] < 27.0,
            "the client is still inside the wall at {}",
            client.body().position[0]
        );
    }

    #[test]
    fn the_pending_queue_cannot_grow_without_bound() {
        // A client that never hears back must not accumulate inputs forever.
        // The cap is the server's own lookahead: older than that and the server
        // would refuse the input anyway, so keeping it buys nothing.
        let ground = Ground::flat();
        let mut client = predictor();
        for tick in 1..=5_000 {
            client.predict(&ground, tick, walking(), &Tuning::DEFAULT);
        }
        assert!(
            client.pending() <= phys::input::MAX_LOOKAHEAD as usize,
            "pending grew to {}",
            client.pending()
        );
    }
}
