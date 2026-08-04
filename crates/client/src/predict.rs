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
    /// The last tick predicted.
    tick: u64,
}

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

    /// Advances one tick locally and records the input for replay.
    pub fn predict(&mut self, solid: &impl Solid, tick: u64, intent: Intent, tuning: &Tuning) {
        self.step(solid, intent, tuning);
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
            self.step(solid, intent, tuning);
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
            self.body.position[1] - self.last_step[1] * behind + self.error[1],
            self.body.position[2] - self.last_step[2] * behind + self.error[2],
        ]
    }

    /// One simulation tick, recording how far it moved the body.
    ///
    /// Prediction and the reconcile replay both go through here so they cannot
    /// disagree about [`Predictor::last_step`]. The delta is measured *before*
    /// [`Predictor::settle`], while both positions are still anchored to the
    /// same origin — after it, a body that crossed a chunk boundary would
    /// subtract two coordinates from different frames and report a 48-cell step.
    fn step(&mut self, solid: &impl Solid, intent: Intent, tuning: &Tuning) {
        let before = self.body.position;
        self.body = phys::step(solid, self.body, intent, tuning);
        self.last_step = [
            self.body.position[0] - before[0],
            self.body.position[1] - before[1],
            self.body.position[2] - before[2],
        ];
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
    use std::collections::BTreeSet;

    use tiamot_core::phys::Gait;

    use super::*;

    /// A floor at cell 0 and nothing else.
    struct Ground(BTreeSet<(i32, i32, i32)>);

    impl Ground {
        fn flat() -> Self {
            Self(BTreeSet::new())
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
