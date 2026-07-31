// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fixed-rate simulation pacing.
//!
//! The simulation advances in fixed 50 ms steps and **never sees wall-clock
//! time**. A tick is identified by its number; how long it took to compute, and
//! how far behind real time the server is, are scheduling concerns that stop at
//! this module's edge.
//!
//! # Why fixed steps rather than a delta
//!
//! A variable `dt` threaded into simulation would make every result depend on
//! how fast the machine ran, which breaks charter rule 4 outright: the same
//! inputs on two servers would produce different worlds. Fixed steps mean a
//! replay of the same inputs produces the same world on any machine at any
//! speed, and the *only* thing a slow machine loses is real-time responsiveness.
//!
//! # Why the accumulator is monotonic-only
//!
//! [`Accumulator`] is driven by elapsed [`Duration`]s, never by a timestamp. It
//! cannot be moved backwards by an NTP step, a suspend/resume, or a user
//! changing the system clock — the three things that turn a wall-clock loop into
//! either a freeze or a burst of thousands of catch-up ticks. Callers are
//! expected to feed it a monotonic source (`Instant::elapsed`), and the API
//! shape makes anything else awkward on purpose.

use core::time::Duration;

/// Simulation steps per second.
///
/// 20 Hz, matching the plan. Fast enough that block edits and movement feel
/// immediate once interpolated, slow enough that 50 players of simulation fits
/// in the budget with room left over.
pub const TICK_RATE_HZ: u32 = 20;

/// Wall-clock duration of one tick.
pub const TICK_DURATION: Duration = Duration::from_nanos(1_000_000_000 / TICK_RATE_HZ as u64);

/// The most ticks one [`Accumulator::advance`] will hand back.
///
/// Without a cap, a server that stalls for ten seconds — a long GC pause, a
/// laptop lid closing, a debugger breakpoint — comes back owing 200 ticks,
/// spends longer computing them than the stall itself, falls further behind,
/// and never recovers. That is the "spiral of death", and the fix is to admit
/// the time is gone rather than to pretend it can be caught up.
///
/// Five ticks means the server will chase up to 250 ms of drift, which covers
/// ordinary scheduling jitter, and declares anything beyond it lost.
pub const MAX_CATCH_UP_TICKS: u32 = 5;

/// Fixed-step scheduler.
///
/// Feed it elapsed real time; it hands back whole ticks to run.
#[derive(Debug, Clone)]
pub struct Accumulator {
    /// Real time owed to the simulation, always less than [`TICK_DURATION`]
    /// after an [`advance`](Self::advance).
    debt: Duration,
    /// Ticks completed since start.
    tick: u64,
    /// Ticks dropped to the catch-up cap over the process lifetime.
    dropped: u64,
}

impl Default for Accumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// What one [`Accumulator::advance`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// How many ticks to run now.
    pub ticks: u32,
    /// Ticks abandoned because the catch-up cap was hit.
    ///
    /// Non-zero means the server is not keeping up. Worth logging, and worth
    /// surfacing in `status` — it is the single most useful number for
    /// diagnosing a struggling server.
    pub dropped: u32,
    /// How long to sleep before asking again, if no ticks are due.
    ///
    /// Zero when ticks are due.
    pub sleep: Duration,
}

impl Accumulator {
    /// A scheduler at tick zero with no debt.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            debt: Duration::ZERO,
            tick: 0,
            dropped: 0,
        }
    }

    /// The number of the next tick to be run.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Ticks abandoned to the catch-up cap since start.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Fraction of the way from the last tick to the next, in `[0, 1)`.
    ///
    /// For a client to interpolate between the two most recent states. Not for
    /// simulation — nothing inside a tick may read this, or the result becomes
    /// frame-rate dependent.
    #[must_use]
    pub fn alpha(&self) -> f32 {
        // Ratio of two small durations, computed in integers and divided once.
        // Both fit in u32 comfortably (a tick is 50 ms), so the division is
        // exact enough and, more to the point, identical everywhere.
        let numerator = self.debt.subsec_nanos().min(TICK_DURATION.subsec_nanos());
        numerator as f32 / TICK_DURATION.subsec_nanos() as f32
    }

    /// Accounts for `elapsed` real time and reports what to do.
    ///
    /// Call once per loop iteration with the time since the previous call.
    pub fn advance(&mut self, elapsed: Duration) -> Step {
        // Saturating: a caller that hands over an absurd duration (a resumed
        // laptop, a mocked clock in a test) gets clamped rather than wrapping
        // into a tiny debt and silently skipping the catch-up logic entirely.
        self.debt = self.debt.saturating_add(elapsed);

        let owed = self.debt.as_nanos() / TICK_DURATION.as_nanos();
        // `owed` is bounded by the cap below, so the cast is on a value already
        // known to be small.
        let owed = u32::try_from(owed).unwrap_or(u32::MAX);

        if owed == 0 {
            return Step {
                ticks: 0,
                dropped: 0,
                sleep: TICK_DURATION.saturating_sub(self.debt),
            };
        }

        let ticks = owed.min(MAX_CATCH_UP_TICKS);
        let dropped = owed - ticks;

        // Consume the whole debt, including the dropped part. Keeping the
        // dropped time would mean the next call is instantly over the cap
        // again — the debt would never clear and the server would run at the
        // cap forever after a single stall.
        self.debt -= TICK_DURATION * ticks;
        if dropped > 0 {
            self.debt = Duration::ZERO;
        }

        self.tick += u64::from(ticks);
        self.dropped += u64::from(dropped);

        Step {
            ticks,
            dropped,
            sleep: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_duration_is_exactly_fifty_milliseconds() {
        assert_eq!(TICK_DURATION, Duration::from_millis(50));
        assert_eq!(TICK_DURATION * TICK_RATE_HZ, Duration::from_secs(1));
    }

    #[test]
    fn a_short_wait_produces_no_ticks_and_a_sleep() {
        let mut acc = Accumulator::new();
        let step = acc.advance(Duration::from_millis(10));
        assert_eq!(step.ticks, 0);
        assert_eq!(step.sleep, Duration::from_millis(40));
        assert_eq!(acc.tick(), 0);
    }

    #[test]
    fn exactly_one_tick_of_time_produces_exactly_one_tick() {
        let mut acc = Accumulator::new();
        let step = acc.advance(TICK_DURATION);
        assert_eq!(step.ticks, 1);
        assert_eq!(step.dropped, 0);
        assert_eq!(acc.tick(), 1);
    }

    #[test]
    fn fractional_time_accumulates_rather_than_being_lost() {
        // The reason for an accumulator at all: 30 ms twice is one tick plus
        // 10 ms of change, not zero ticks twice.
        let mut acc = Accumulator::new();
        assert_eq!(acc.advance(Duration::from_millis(30)).ticks, 0);
        assert_eq!(acc.advance(Duration::from_millis(30)).ticks, 1);
        assert_eq!(acc.tick(), 1);

        // And the 10 ms remainder is still there.
        assert_eq!(acc.advance(Duration::from_millis(40)).ticks, 1);
        assert_eq!(acc.tick(), 2);
    }

    #[test]
    fn a_long_run_at_real_time_drifts_by_nothing() {
        // 1000 iterations of exactly one tick's worth of time must be exactly
        // 1000 ticks. An implementation that converted to f32 seconds and back
        // would be off by a handful by now.
        let mut acc = Accumulator::new();
        for _ in 0..1000 {
            acc.advance(TICK_DURATION);
        }
        assert_eq!(acc.tick(), 1000);
        assert_eq!(acc.dropped(), 0);
    }

    #[test]
    fn irregular_frames_still_average_out() {
        // Real scheduling is jittery. Over a second of wall time the tick count
        // must land on 20 regardless of how the time arrived.
        let mut acc = Accumulator::new();
        let jitter = [
            7u64, 13, 3, 51, 2, 29, 41, 8, 17, 33, 11, 60, 5, 22, 14, 39, 9, 26, 44, 6,
        ];
        let total: u64 = jitter.iter().sum();
        for ms in jitter {
            acc.advance(Duration::from_millis(ms));
        }
        assert_eq!(
            acc.tick(),
            total / 50,
            "{total} ms should be {} ticks",
            total / 50
        );
        assert_eq!(acc.dropped(), 0, "ordinary jitter must not drop ticks");
    }

    #[test]
    fn a_stall_is_capped_rather_than_chased() {
        // The spiral-of-death guard. Ten seconds of stall is 200 ticks owed;
        // running them would take longer than the stall.
        let mut acc = Accumulator::new();
        let step = acc.advance(Duration::from_secs(10));
        assert_eq!(step.ticks, MAX_CATCH_UP_TICKS);
        assert_eq!(step.dropped, 200 - MAX_CATCH_UP_TICKS);
        assert_eq!(acc.dropped(), u64::from(200 - MAX_CATCH_UP_TICKS));
    }

    #[test]
    fn a_stall_does_not_leave_permanent_debt() {
        // The subtle half of the cap: if the dropped time stayed in the debt,
        // every subsequent call would still be over the cap and the server
        // would run at max catch-up forever, never recovering.
        let mut acc = Accumulator::new();
        acc.advance(Duration::from_secs(10));
        let dropped_after_stall = acc.dropped();

        for _ in 0..100 {
            let step = acc.advance(TICK_DURATION);
            assert_eq!(step.ticks, 1, "should be back to real time immediately");
            assert_eq!(step.dropped, 0);
        }
        assert_eq!(
            acc.dropped(),
            dropped_after_stall,
            "recovery must not keep dropping ticks"
        );
    }

    #[test]
    fn time_never_runs_backwards() {
        // Zero is the smallest thing that can be fed in — the type makes a
        // negative delta unrepresentable, which is the point of taking a
        // Duration rather than a timestamp.
        let mut acc = Accumulator::new();
        acc.advance(Duration::from_millis(30));
        for _ in 0..10 {
            assert_eq!(acc.advance(Duration::ZERO).ticks, 0);
        }
        assert_eq!(acc.tick(), 0);
    }

    #[test]
    fn an_absurd_duration_does_not_wrap() {
        // A resumed laptop or a mocked clock can produce a duration far larger
        // than anything real. It must clamp, not wrap into a small debt.
        let mut acc = Accumulator::new();
        let step = acc.advance(Duration::MAX);
        assert_eq!(step.ticks, MAX_CATCH_UP_TICKS);
        assert!(step.dropped > 0);

        // And it must still be usable afterwards.
        assert_eq!(acc.advance(TICK_DURATION).ticks, 1);
    }

    #[test]
    fn alpha_moves_between_zero_and_one_and_never_reaches_it() {
        let mut acc = Accumulator::new();
        assert!((acc.alpha() - 0.0).abs() < f32::EPSILON);

        acc.advance(Duration::from_millis(25));
        assert!((acc.alpha() - 0.5).abs() < 0.001, "got {}", acc.alpha());

        acc.advance(Duration::from_millis(24));
        assert!(
            acc.alpha() < 1.0,
            "alpha must stay below 1, got {}",
            acc.alpha()
        );

        // Crossing a tick boundary resets it towards zero.
        acc.advance(Duration::from_millis(2));
        assert!(acc.alpha() < 0.1, "got {}", acc.alpha());
    }

    #[test]
    fn the_tick_number_only_ever_increases() {
        let mut acc = Accumulator::new();
        let mut previous = 0;
        for ms in [1u64, 99, 3, 200, 0, 50, 7, 1000, 2] {
            acc.advance(Duration::from_millis(ms));
            assert!(acc.tick() >= previous, "tick went backwards");
            previous = acc.tick();
        }
    }
}
