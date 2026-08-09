// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Known-answer cases from Sub-Node Contract §2, plus the invariant that
//! outranks them all: a body is never left inside geometry.

use std::collections::BTreeSet;

use proptest::prelude::*;

use super::*;

/// How far the built scenes extend from the origin, in cells. Comfortably
/// past where any of these tests can walk in the ticks it runs for.
const SPAN: i32 = 64;

/// A scene of solid cells with a floor.
///
/// `BTreeSet` rather than `HashSet` because charter rule 4 bans iteration over
/// a randomly-seeded hasher anywhere a simulation result could depend on it.
/// Nothing here iterates, but the habit is the point — the next person to add a
/// loop should not have to notice.
struct Scene {
    solid: BTreeSet<(i32, i32, i32)>,
    /// Everything strictly below this is solid, so a falling body always lands.
    floor: i32,
}

impl Scene {
    fn new(floor: i32) -> Self {
        Self {
            solid: BTreeSet::new(),
            floor,
        }
    }

    fn with(mut self, cells: &[(i32, i32, i32)]) -> Self {
        self.solid.extend(cells.iter().copied());
        self
    }

    /// A wall filling `x = wall_x`, from the floor up.
    fn with_wall(mut self, wall_x: i32, height: i32) -> Self {
        for y in self.floor..(self.floor + height) {
            for z in -SPAN..SPAN {
                self.solid.insert((wall_x, y, z));
            }
        }
        self
    }

    /// A step of `height` cells filling everything at `x >= from_x`.
    ///
    /// It runs to the end of [`SPAN`] rather than a few cells, because a body
    /// that walks off the far end falls back to the floor and the test then
    /// measures the wrong thing — which is exactly how this scene first read as
    /// "the step was not climbed".
    fn with_step(mut self, from_x: i32, height: i32) -> Self {
        for y in self.floor..(self.floor + height) {
            for x in from_x..SPAN {
                for z in -SPAN..SPAN {
                    self.solid.insert((x, y, z));
                }
            }
        }
        self
    }
}

impl Solid for Scene {
    fn solid(&self, x: i32, y: i32, z: i32) -> bool {
        y < self.floor || self.solid.contains(&(x, y, z))
    }
}

/// Runs `ticks` steps and returns the body.
fn simulate(scene: &Scene, mut body: Body, intent: Intent, ticks: usize) -> Body {
    let tuning = Tuning::DEFAULT;
    for _ in 0..ticks {
        body = step(scene, body, intent, &tuning);
    }
    body
}

#[test]
fn a_body_falls_and_lands_exactly_on_the_floor() {
    // "Exactly" is the assertion that matters. Landing approximately is what a
    // sweep that stops a whole velocity step short of the surface does, and it
    // reads in the window as hovering.
    let scene = Scene::new(0);
    let body = simulate(&scene, Body::at([0.5, 6.0, 0.5]), Intent::default(), 40);

    assert!(body.on_ground, "never landed: {body:?}");
    assert!(
        (body.position[1] - 0.0).abs() < SKIN * 4.0,
        "landed at {} rather than resting on the floor at 0",
        body.position[1]
    );
    // Bit equality: landing must zero the vertical velocity outright, and a
    // tolerance here would accept a body still creeping into the floor.
    assert_eq!(
        body.velocity[1].to_bits(),
        0.0f32.to_bits(),
        "kept falling after landing: {}",
        body.velocity[1]
    );
}

#[test]
fn a_body_slides_along_a_wall_instead_of_sticking_to_it() {
    // Contract §2: resolving one axis at a time is what buys this. A solver
    // that cancelled the whole move on contact would leave z unchanged, and
    // walking into a wall at an angle would stop the player dead.
    let scene = Scene::new(0).with_wall(4, 8);
    let start = Body {
        position: [2.0, 0.0, 0.0],
        velocity: [0.0, 0.0, 0.0],
        on_ground: true,
    };

    // Pushing diagonally into the wall: +x is blocked, +z must still happen.
    let intent = Intent {
        walk: [1.0, 1.0],
        jump: false,
        gait: Gait::Walk,
    };
    let body = simulate(&scene, start, intent, 20);

    assert!(
        body.position[0] < 4.0,
        "walked into the wall at x=4: {body:?}"
    );
    assert!(
        body.position[2] > 1.0,
        "did not slide along the wall; z barely moved: {body:?}"
    );
}

#[test]
fn a_step_up_of_one_subnode_succeeds_and_two_does_not() {
    // Contract §2 fixes the step at one sub-node. Both halves are asserted
    // together: a solver that stepped any height would pass the first on its
    // own, and one that stepped none would pass the second.
    let intent = Intent {
        walk: [1.0, 0.0],
        jump: false,
        gait: Gait::Walk,
    };
    let start = Body {
        position: [0.0, 0.0, 0.5],
        velocity: [0.0, 0.0, 0.0],
        on_ground: true,
    };

    let one = Scene::new(0).with_step(3, 1);
    let climbed = simulate(&one, start, intent, 30);
    assert!(
        climbed.position[1] >= 1.0,
        "a one-cell step was not climbed: {climbed:?}"
    );
    assert!(
        climbed.position[0] > 3.0,
        "climbed but did not get onto the step: {climbed:?}"
    );

    let two = Scene::new(0).with_step(3, 2);
    let stopped = simulate(&two, start, intent, 30);
    assert!(
        stopped.position[1] < 1.0,
        "a two-cell lip was climbed, which contract §2 forbids: {stopped:?}"
    );
    assert!(
        stopped.position[0] < 3.0,
        "walked through the two-cell lip: {stopped:?}"
    );
}

#[test]
fn sneaking_stops_at_the_edge_and_walking_does_not() {
    // The counter-example is half the test: without the walking case, a body
    // that simply could not move would pass.
    let ledge = {
        let mut scene = Scene::new(-64);
        for x in -SPAN..4 {
            for z in -SPAN..SPAN {
                scene.solid.insert((x, -1, z));
            }
        }
        scene
    };

    let start = Body {
        position: [0.0, 0.0, 0.5],
        velocity: [0.0, 0.0, 0.0],
        on_ground: true,
    };

    let sneaking = simulate(
        &ledge,
        start,
        Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: Gait::Sneak,
        },
        60,
    );
    assert!(
        sneaking.position[1] > -1.0,
        "sneaked off the edge and fell: {sneaking:?}"
    );
    assert!(
        sneaking.position[0] <= 4.0 + PLAYER_WIDTH,
        "sneak walked past the ledge: {sneaking:?}"
    );

    let walking = simulate(
        &ledge,
        start,
        Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: Gait::Walk,
        },
        60,
    );
    assert!(
        walking.position[1] < -8.0,
        "walking off a ledge should fall; the edge guard is firing for every gait: {walking:?}"
    );
}

#[test]
fn a_jump_clears_one_block_but_not_two() {
    let scene = Scene::new(0);
    let start = Body {
        position: [0.5, 0.0, 0.5],
        velocity: [0.0, 0.0, 0.0],
        on_ground: true,
    };
    let intent = Intent {
        walk: [0.0, 0.0],
        jump: true,
        gait: Gait::Walk,
    };

    let mut body = start;
    let tuning = Tuning::DEFAULT;
    let mut apex: f32 = 0.0;
    for _ in 0..40 {
        body = step(&scene, body, intent, &tuning);
        apex = apex.max(body.position[1]);
    }

    // One block is 3 cells, two is 6.
    assert!(
        apex >= 3.0,
        "a jump did not clear a full block: {apex} cells"
    );
    assert!(apex < 6.0, "a jump cleared two blocks: {apex} cells");
}

#[test]
fn a_body_in_a_one_cell_gap_does_not_fall_through_it() {
    // The case a sweep gets wrong when it tests only the leading face: a body
    // moving fast enough to cross a thin floor in one tick.
    let scene = Scene::new(-64).with(&[
        (0, 0, 0),
        (1, 0, 0),
        (-1, 0, 0),
        (0, 0, 1),
        (0, 0, -1),
        (1, 0, 1),
        (1, 0, -1),
        (-1, 0, 1),
        (-1, 0, -1),
    ]);
    let body = Body {
        position: [0.5, 20.0, 0.5],
        velocity: [0.0, -11.0, 0.0],
        on_ground: false,
    };

    let landed = simulate(&scene, body, Intent::default(), 10);
    assert!(
        landed.position[1] >= 1.0,
        "fell through a one-cell floor at 11 cells/tick: {landed:?}"
    );
}

#[test]
fn the_same_inputs_produce_the_same_body() {
    // Underwrites client prediction: the client replays inputs through this
    // code and compares with the server's answer.
    let scene = Scene::new(0).with_step(5, 1).with_wall(9, 6);
    let intent = Intent {
        walk: [0.7, 0.3],
        jump: true,
        gait: Gait::Sprint,
    };

    let first = simulate(&scene, Body::at([0.5, 3.0, 0.5]), intent, 200);
    let second = simulate(&scene, Body::at([0.5, 3.0, 0.5]), intent, 200);

    assert_eq!(
        first.position.map(f32::to_bits),
        second.position.map(f32::to_bits),
        "two runs of the same inputs diverged"
    );
    assert_eq!(
        first.velocity.map(f32::to_bits),
        second.velocity.map(f32::to_bits)
    );
}

proptest! {
    /// Contract §2: "A body must never be left inside geometry. This is the
    /// invariant that outranks every performance concern in Task 09."
    #[test]
    fn a_body_never_ends_a_tick_inside_geometry(
        cells in prop::collection::vec(
            (-6i32..6, -2i32..6, -6i32..6),
            0..40,
        ),
        moves in prop::collection::vec(
            (-1.0f32..1.0, -1.0f32..1.0, any::<bool>(), 0u8..3),
            1..40,
        ),
        start_x in -3.0f32..3.0,
        start_z in -3.0f32..3.0,
    ) {
        let scene = Scene::new(-4).with(&cells);
        let tuning = Tuning::DEFAULT;
        let mut body = Body::at([start_x, 8.0, start_z]);

        for (walk_x, walk_z, jump, gait) in moves {
            let intent = Intent {
                walk: [walk_x, walk_z],
                jump,
                gait: match gait {
                    0 => Gait::Walk,
                    1 => Gait::Sprint,
                    _ => Gait::Sneak,
                },
            };
            body = step(&scene, body, intent, &tuning);

            prop_assert!(
                !scene.overlaps(&body.aabb()),
                "body ended a tick inside geometry at {:?}",
                body.position
            );
            prop_assert!(
                body.position.iter().all(|v| v.is_finite()),
                "position went non-finite: {:?}",
                body.position
            );
        }
    }
}

/// A rising passage: the floor steps up every `run` cells and the ceiling rises
/// with it, which is what digging a staircase actually produces.
///
/// Deliberately not a [`Scene`]: filling a `BTreeSet` with a hundred blocks of
/// tunnel is slower than answering the question arithmetically, and the
/// arithmetic is what says what the scene *is*.
struct Staircase {
    /// Cells the floor rises at each step.
    rise: i32,
    /// Cells of level ground between one riser and the next.
    run: i32,
    /// Clear cells between the floor and the ceiling above it.
    headroom: i32,
}

impl Staircase {
    /// The first solid cell's height at this column.
    fn floor_at(&self, x: i32) -> i32 {
        if x < 0 { 0 } else { (x / self.run) * self.rise }
    }
}

impl Solid for Staircase {
    fn solid(&self, x: i32, y: i32, _z: i32) -> bool {
        let floor = self.floor_at(x);
        y < floor || y >= floor + self.headroom
    }
}

#[test]
fn climbing_a_stepped_passage_does_not_cost_a_walking_pace() {
    // **Reported from the window: "climbing a tunnel staircase is close to
    // impossible it is so jumpy."**
    //
    // Contract §2 resolves X before Y, so a horizontal move is tested at the
    // height the body had when the tick began. A body jumping a riser therefore
    // meets it one tick before it clears it, and zeroing the velocity there
    // spent the whole jump: with `air_acceleration` a fifteenth of the ground
    // figure, the body then crawled up the next step at a twelfth of walking
    // pace rather than stopping and carrying on.
    //
    // One-block risers every two blocks in a three-block passage, which is a
    // staircase somebody would really dig. Measured over the same 200 ticks:
    // 67.0 cells before, 81.5 after. The bound sits between them.
    let scene = Staircase {
        rise: 3,
        run: 6,
        headroom: 9,
    };
    let tuning = Tuning::DEFAULT;
    let mut body = Body::at([0.5, 0.0, 0.5]);
    let intent = Intent {
        walk: [1.0, 0.0],
        jump: true,
        gait: Gait::Walk,
    };
    for _ in 0..200 {
        body = step(&scene, body, intent, &tuning);
    }

    assert!(
        body.position[0] > 75.0,
        "a body holding forward and jump for 200 ticks got {} cells up a stepped passage; \
         it was 67 when every riser cost it a jump's worth of speed",
        body.position[0]
    );
    // And it actually climbed, rather than running along a floor this scene
    // failed to raise — which is the reading that would make the distance above
    // easy and meaningless.
    assert!(
        body.position[1] > 30.0,
        "the body is at height {} after 200 ticks, so it went along rather than up",
        body.position[1]
    );
}

#[test]
fn a_body_pushing_off_a_riser_keeps_the_speed_it_jumped_with() {
    // The mechanism under the test above, on one tick rather than two hundred.
    //
    // The tick a jump starts is a tick that BEGAN on the ground, so
    // `was_on_ground` alone cannot tell "pushing off" from "leaning on". The
    // jump has already reached the vertical velocity by the time the horizontal
    // axes resolve — see `step` — which is what makes rising the honest test.
    let scene = Scene::new(0).with_step(6, 3);
    let tuning = Tuning::DEFAULT;

    // Pressed against the riser at x = 6, at a walking clip, standing.
    let placed = Body {
        position: [5.0, 0.0, 0.5],
        velocity: [tuning.walk_speed, 0.0, 0.0],
        on_ground: true,
    };

    let jumping = step(
        &scene,
        placed,
        Intent {
            walk: [1.0, 0.0],
            jump: true,
            gait: Gait::Walk,
        },
        &tuning,
    );
    assert!(
        jumping.velocity[0] > tuning.walk_speed * 0.5,
        "a body jumping into a riser kept {} of its {} cells a tick, so the jump it is about \
         to take will start from a standstill",
        jumping.velocity[0],
        tuning.walk_speed
    );

    // The counter-example, and the behaviour that must survive: standing
    // against the same riser with no jump, the speed goes. A body leaning on a
    // wall that kept its velocity would leave it like a released spring.
    let leaning = step(
        &scene,
        placed,
        Intent {
            walk: [1.0, 0.0],
            jump: false,
            gait: Gait::Walk,
        },
        &tuning,
    );
    // Not `assert_eq!` against `0.0`: the value really is assigned zero and the
    // comparison really would be exact, but `clippy::float_cmp` is deny-level
    // here and an exemption for a test is a precedent this crate does not want.
    assert!(
        leaning.velocity[0].abs() < f32::EPSILON,
        "a body walking into a wall kept {} of its horizontal speed",
        leaning.velocity[0]
    );
}
