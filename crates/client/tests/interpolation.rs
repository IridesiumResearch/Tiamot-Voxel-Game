// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What the interpolation buffer costs, in cells, under a bad link.
//!
//! # The bound is on the SHAPE of the path, not on where it is now
//!
//! The buffer draws at `arrival − INTERPOLATION_DELAY`, so under 150 ms of
//! latency the client is showing where an entity was a quarter of a second ago.
//! Measuring that against where it is *now* would measure the latency, which is
//! not something the client can do anything about and not what this is for.
//!
//! What the client is responsible for is reproducing the path faithfully once
//! shifted: the error between what it draws and where the entity really was at
//! the moment being drawn. That is what these bound.
//!
//! # Why the path turns corners
//!
//! Linear interpolation of a straight line at constant speed is exact — a test
//! on one would pass with the buffer deleted and the newest sample held. Every
//! path here changes direction, because a corner is where interpolation has an
//! error to have, and dropping the samples around one is the worst case a
//! moving mob offers.

use std::time::Duration;

use client::entities::{Entities, INTERPOLATION_DELAY};
use tiamot_core::ChunkPos;
use tiamot_core::proto::{EntityDef, EntityDelta};

/// The server's rate, and therefore the spacing of updates.
const TICK: Duration = Duration::from_millis(50);

/// Walk speed, in cells per tick — the fastest anything ordinarily moves.
const SPEED: f32 = 0.65;

/// How long a run lasts.
const TICKS: u64 = 200;

/// The true path: out along +x, a right-angle turn onto +z, then back along −x.
///
/// Corners at a quarter and a half, so a run crosses two of them and a sample
/// dropped near either has to be interpolated across.
fn truth(tick: u64) -> [f32; 3] {
    let quarter = TICKS / 4;
    let step = SPEED;
    let (x, z) = if tick < quarter {
        (tick as f32 * step, 0.0)
    } else if tick < quarter * 2 {
        (quarter as f32 * step, (tick - quarter) as f32 * step)
    } else {
        (
            (quarter as f32 - (tick - quarter * 2) as f32) * step,
            quarter as f32 * step,
        )
    };
    [24.0 + x, 20.0, 24.0 + z]
}

fn def(local: [f32; 3]) -> EntityDef {
    EntityDef {
        hands: [None, None],
        id: 1,
        chunk: ChunkPos::new(0, 0, 0),
        local,
        velocity: [0.0; 3],
        yaw: 0,
        pitch: 0,
        anim: 0,
        model: Some("engine:humanoid".to_owned()),
        collider: Some([1.8, 5.4]),
        nametag: None,
        item: None,
    }
}

fn delta(tick: u64, local: [f32; 3]) -> EntityDelta {
    EntityDelta {
        id: 1,
        chunk: ChunkPos::new(0, 0, 0),
        local,
        velocity: [0.0; 3],
        yaw: 0,
        pitch: 0,
        anim: 0,
    }
    .tap(tick)
}

/// `EntityDelta` has no tick field of its own — the buffer is handed one
/// alongside — so this is a shim that keeps the call sites readable.
trait Tap {
    fn tap(self, tick: u64) -> Self;
}

impl Tap for EntityDelta {
    fn tap(self, _tick: u64) -> Self {
        self
    }
}

/// Plays a run through the buffer and returns the worst error, in cells.
///
/// `deliver` decides whether a tick's update arrives and how late it is, so one
/// runner covers clean links, lossy ones and jittery ones. `base_lag` is the
/// link's nominal latency, which the comparison subtracts.
///
/// **Subtracting it is the whole method.** Arrival time is the buffer's clock,
/// so a sample that took 150 ms to arrive is stamped 150 ms after the moment it
/// describes. Comparing against the un-shifted path would measure the latency
/// rather than the interpolation — which is how the first version of this test
/// read a constant 150 ms link as two cells of error and a clean one as a
/// quarter of a cell.
fn worst_error(base_lag: Duration, deliver: impl Fn(u64) -> Option<Duration>) -> f32 {
    let mut entities = Entities::new();
    let mut worst: f32 = 0.0;

    // Everything that has been delivered so far, so the query loop can be run
    // after the fact at whatever resolution it likes.
    let mut arrivals: Vec<(u64, Duration)> = Vec::new();
    for tick in 0..TICKS {
        if let Some(lag) = deliver(tick) {
            arrivals.push((tick, TICK * u32::try_from(tick).expect("tick fits") + lag));
        }
    }
    arrivals.sort_by_key(|(_, at)| *at);

    let Some(&(first_tick, first_at)) = arrivals.first() else {
        panic!("nothing was delivered, so nothing is under test");
    };
    entities.spawned(&[def(truth(first_tick))], first_at);

    let mut next = 1;
    // Query every 10 ms, which is finer than a frame at 60 Hz.
    let mut now = first_at;
    let last = arrivals.last().expect("delivered").1;
    while now <= last {
        while next < arrivals.len() && arrivals[next].1 <= now {
            let (tick, at) = arrivals[next];
            entities.moved(tick, &[delta(tick, truth(tick))], at);
            next += 1;
        }

        if let Some(pose) = entities.get(1).and_then(|entity| entity.pose(now)) {
            // What the buffer is aiming at: where the entity really was at the
            // moment it is drawing. Time is measured in ticks, and a tick is
            // 50 ms, so the moment maps straight back onto the path.
            let target = now.saturating_sub(INTERPOLATION_DELAY);
            let at_tick = target.saturating_sub(base_lag).as_secs_f32() / TICK.as_secs_f32();
            let want = between(at_tick);
            let error = (pose.local[0] - want[0])
                .abs()
                .max((pose.local[2] - want[2]).abs());
            worst = worst.max(error);
        }
        now += Duration::from_millis(10);
    }
    worst
}

/// The true path at a fractional tick, so the comparison is not itself
/// quantised to whole server ticks.
fn between(at: f32) -> [f32; 3] {
    // `detgen::floor_to_i32` rather than `f32::floor`: the determinism lint is
    // scoped to the whole workspace and takes no exemption for a test, which is
    // the gate working — a habit that stops at test files is not a habit.
    let index = tiamot_core::detgen::floor_to_i32(at).max(0) as u64;
    let fraction = at - index as f32;
    let a = truth(index.min(TICKS - 1));
    let b = truth((index + 1).min(TICKS - 1));
    [
        a[0] + (b[0] - a[0]) * fraction,
        a[1] + (b[1] - a[1]) * fraction,
        a[2] + (b[2] - a[2]) * fraction,
    ]
}

#[test]
fn a_clean_link_reproduces_the_path_almost_exactly() {
    // Every update arrives, 20 ms after it was sent. The only error left is at
    // the two corners, where a straight blend cuts across the turn — and it
    // cannot exceed half a tick of travel, because that is the whole distance
    // between the samples either side.
    let worst = worst_error(Duration::from_millis(20), |_| {
        Some(Duration::from_millis(20))
    });
    // Measured: exactly zero, and that is the honest result rather than a weak
    // test. The buffer's job is to reproduce the path the server SAMPLED, and
    // the reference here is that same path linearly connected — asking the
    // client to know the curve between two samples would be asking it to know
    // something it was never sent.
    assert!(
        worst < SPEED,
        "a clean link should be inside one tick of travel; it was {worst:.3} cells"
    );
}

#[test]
fn a_hundred_and_fifty_milliseconds_of_latency_costs_nothing_extra() {
    // Latency shifts the whole playback and is not an error the client can do
    // anything about — the point of measuring against the shifted truth is that
    // this figure should look like the clean one.
    let clean = worst_error(Duration::from_millis(20), |_| {
        Some(Duration::from_millis(20))
    });
    let lagged = worst_error(Duration::from_millis(150), |_| {
        Some(Duration::from_millis(150))
    });
    assert!(
        lagged < clean + 0.1,
        "constant latency should not distort the path: {clean:.3} clean, {lagged:.3} lagged"
    );
}

#[test]
fn one_update_in_five_lost_stays_within_a_third_of_a_block() {
    // Twenty per cent loss on the unreliable channel, which is far worse than
    // a link anybody plays on — and the pattern is chosen to drop the samples
    // AT the corners (ticks 50 and 100), because a lost sample on a straight
    // run costs exactly nothing: linear interpolation over a longer gap on a
    // straight line is the same line.
    let worst = worst_error(Duration::from_millis(150), |tick| {
        if tick.is_multiple_of(5) {
            None
        } else {
            Some(Duration::from_millis(150))
        }
    });
    // Measured: 0.65 cells, a fifth of a block, at the corner it dropped.
    assert!(
        worst < 1.0,
        "one update in five lost should stay inside a cell; it was {worst:.3}"
    );
}

#[test]
fn a_burst_of_four_lost_updates_stays_within_a_block() {
    // Four in a row is two hundred milliseconds of silence — longer than the
    // interpolation delay, so the buffer runs out and holds the newest sample
    // rather than extrapolating. Holding is the deliberate choice: the error is
    // bounded by how far the entity travels in the gap, and a guess would not
    // be bounded by anything.
    let worst = worst_error(Duration::from_millis(150), |tick| {
        if (40..44).contains(&tick) || (120..124).contains(&tick) {
            None
        } else {
            Some(Duration::from_millis(150))
        }
    });
    // Measured: 1.82 cells, which is how far a walking body travels in the
    // part of the gap the buffer had nothing to interpolate across.
    assert!(
        worst < 3.0,
        "a four-update burst should stay inside a block; it was {worst:.3} cells"
    );
}

#[test]
fn jitter_does_not_reorder_the_path() {
    // Arrivals that overtake each other, which is what an unreliable channel
    // does under load. The buffer stamps by arrival and sorts by it, so a
    // sample that overtakes must not drag the entity backwards.
    let worst = worst_error(Duration::from_millis(120), |tick| {
        let jitter = if tick % 3 == 0 { 40 } else { 0 };
        Some(Duration::from_millis(120 + jitter))
    });
    // Measured: 0.52 cells — a sixth of a block, from samples arriving in a
    // different order than they were sent.
    assert!(
        worst < 2.0,
        "jitter should not distort the path beyond a block: {worst:.3} cells"
    );
}
