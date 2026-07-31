// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The simulation thread.
//!
//! Owns the world and advances it at a fixed 20 Hz. Everything that mutates
//! world state happens here, on one thread, in tick order — which is what makes
//! the world reproducible. The network layer hands work *in* and reads state
//! *out*; it never touches the world directly.
//!
//! # Why the loop is generic over its clock
//!
//! The pacing rules — when to sleep, when to run several ticks, when to give up
//! on catching up — are the part most likely to be wrong, and they are exactly
//! the part that a wall-clock test cannot pin down without being slow and
//! flaky. So [`run`] takes a [`Clock`], and the tests drive it with a fake one
//! that advances by however much the test says. A test for "the server survives
//! a ten-second stall" then takes microseconds and gives the same answer every
//! time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tiamot_core::tick::{Accumulator, TICK_DURATION};
use tracing::{info, warn};

/// A source of elapsed time and a way to wait.
///
/// Deliberately elapsed-only. There is no way to ask this for "now", so there
/// is no way for pacing code to accidentally depend on wall-clock time, and no
/// way for a clock adjustment to move the simulation.
pub trait Clock {
    /// Time since the previous call. The first call reports time since start.
    fn tick_elapsed(&mut self) -> Duration;

    /// Waits for approximately `duration`.
    fn sleep(&mut self, duration: Duration);
}

/// The real clock: a monotonic [`Instant`] and a thread sleep.
pub struct MonotonicClock {
    last: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock {
    /// Starts the clock now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn tick_elapsed(&mut self) -> Duration {
        // `Instant` is monotonic by contract on every platform Rust supports,
        // so this can never go backwards — which is the whole reason the
        // accumulator takes durations rather than timestamps.
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        self.last = now;
        elapsed
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Shared control surface for a running simulation.
///
/// Cloneable and cheap; the network and RCON layers hold one to ask the
/// simulation to stop and to read its progress without touching world state.
#[derive(Debug, Clone, Default)]
pub struct Control {
    inner: Arc<ControlInner>,
}

#[derive(Debug, Default)]
struct ControlInner {
    stop: AtomicBool,
    tick: AtomicU64,
    dropped: AtomicU64,
    /// Longest single tick observed, in microseconds.
    slowest_micros: AtomicU64,
    /// Ticks that took longer than the 50 ms budget.
    over_budget: AtomicU64,
    /// Set when an operator asks for a save; cleared when the tick performs it.
    save_requested: AtomicBool,
}

impl Control {
    /// A fresh, running control handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the simulation to finish the current tick and stop.
    pub fn stop(&self) {
        // Release so that everything the caller did before asking to stop is
        // visible to the simulation thread when it observes the flag.
        self.inner.stop.store(true, Ordering::Release);
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn stopping(&self) -> bool {
        self.inner.stop.load(Ordering::Acquire)
    }

    /// The number of the next tick to run.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.inner.tick.load(Ordering::Relaxed)
    }

    /// Ticks abandoned to the catch-up cap since start.
    ///
    /// The single most useful number for diagnosing a struggling server: if
    /// this is climbing, the machine is not keeping up with 20 Hz.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// The longest single tick observed, in microseconds.
    ///
    /// Measured around the simulation step alone, not the sleep — this is how
    /// long the server spent *working*, which is the number that says whether
    /// there is headroom left.
    #[must_use]
    pub fn slowest_tick_micros(&self) -> u64 {
        self.inner.slowest_micros.load(Ordering::Relaxed)
    }

    /// Asks the simulation to write dirty chunks on its next tick.
    ///
    /// A request rather than a call: the simulation owns the database, and a
    /// save performed from an RCON task would mean two threads writing chunks
    /// — the exact thing `world.rs` exists to prevent.
    pub fn request_save(&self) {
        self.inner.save_requested.store(true, Ordering::Release);
    }

    /// Takes the save request, if there is one.
    ///
    /// Clears the flag, so a request is honoured once rather than every tick
    /// thereafter.
    #[must_use]
    pub fn take_save_request(&self) -> bool {
        self.inner.save_requested.swap(false, Ordering::AcqRel)
    }

    /// How many ticks ran over the 50 ms budget.
    ///
    /// A tick over budget has not necessarily hurt anyone — the accumulator
    /// absorbs a single slow tick — but a rising count means the server is
    /// living on the catch-up allowance rather than inside its budget.
    #[must_use]
    pub fn over_budget_ticks(&self) -> u64 {
        self.inner.over_budget.load(Ordering::Relaxed)
    }
}

/// Runs the fixed-rate loop until [`Control::stop`] is called.
///
/// `step` is called once per tick with the tick number. It is the only place
/// world state may change.
///
/// Returns the number of ticks run.
pub fn run<C: Clock, F: FnMut(u64)>(clock: &mut C, control: &Control, mut step: F) -> u64 {
    // Charter rule 4: this thread runs simulation floats, so it must be in
    // IEEE default mode. A thread that inherited flush-to-zero or a non-nearest
    // rounding mode — from a driver, an audio library, or a mod's native
    // code — would silently produce a different world. Fail here, loudly, at
    // startup, rather than at the first cross-platform hash mismatch.
    tiamot_core::assert_ieee_mode();

    let mut accumulator = Accumulator::new();
    let mut ran = 0u64;

    // Discard the time between thread spawn and loop entry. Otherwise process
    // startup — opening the world, loading mods — is charged to the simulation
    // as debt, and the server begins life already behind.
    clock.tick_elapsed();

    while !control.stopping() {
        let elapsed = clock.tick_elapsed();
        let advance = accumulator.advance(elapsed);

        if advance.dropped > 0 {
            warn!(
                dropped = advance.dropped,
                total_dropped = accumulator.dropped(),
                tick = accumulator.tick(),
                "simulation fell behind; dropped ticks rather than chasing them"
            );
            control
                .inner
                .dropped
                .store(accumulator.dropped(), Ordering::Relaxed);
        }

        if advance.ticks == 0 {
            // Sleep the remainder of the tick, but never so long that a stop
            // request waits on it. A shutdown that takes a whole tick to be
            // noticed is not a problem at 50 ms; capping it keeps that true if
            // the tick rate ever drops.
            clock.sleep(advance.sleep.min(TICK_DURATION));
            continue;
        }

        // Tick numbers are SEQUENTIAL, with no gaps for dropped ticks. A tick
        // is a simulation step, and its number is its index; dropping means the
        // world advanced less than real time did, which is the honest account.
        // Numbering by real time instead would leave holes, and a mod computing
        // an interval from tick numbers would silently get it wrong.
        for _ in 0..advance.ticks {
            // Measured with a real `Instant` rather than through the `Clock`.
            // The clock exists so PACING can be tested without sleeping; how
            // long the work took is a fact about the machine, and faking it
            // would make this number meaningless.
            let started = Instant::now();
            step(ran);
            let elapsed = started.elapsed();
            ran += 1;

            let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
            control
                .inner
                .slowest_micros
                .fetch_max(micros, Ordering::Relaxed);
            if elapsed > TICK_DURATION {
                control.inner.over_budget.fetch_add(1, Ordering::Relaxed);
            }
        }

        control.inner.tick.store(ran, Ordering::Relaxed);
    }

    info!(
        ticks = ran,
        dropped = accumulator.dropped(),
        "simulation stopped"
    );
    debug_assert_eq!(
        ran,
        accumulator.tick(),
        "the loop's tick count and the accumulator's must not diverge"
    );
    ran
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A clock the test drives by hand.
    struct FakeClock {
        /// Elapsed values to return, in order. Repeats the last one forever.
        script: Vec<Duration>,
        index: usize,
        /// Every sleep requested, for asserting on pacing.
        slept: Vec<Duration>,
        /// Stops the loop after this many readings, for tests where no tick is
        /// ever due and nothing else could end it.
        stop_after: Option<(usize, Control)>,
    }

    impl FakeClock {
        fn new(script: Vec<Duration>) -> Self {
            Self {
                script,
                index: 0,
                slept: Vec::new(),
                stop_after: None,
            }
        }

        /// Stops `control` once the loop has asked for time `readings` times.
        fn stopping_after(mut self, readings: usize, control: &Control) -> Self {
            self.stop_after = Some((readings, control.clone()));
            self
        }

        /// A clock that always reports exactly one tick of elapsed time.
        fn real_time() -> Self {
            Self::new(vec![TICK_DURATION])
        }
    }

    impl Clock for FakeClock {
        fn tick_elapsed(&mut self) -> Duration {
            let value = self
                .script
                .get(self.index)
                .copied()
                .or_else(|| self.script.last().copied())
                .unwrap_or(Duration::ZERO);
            self.index += 1;
            if let Some((readings, control)) = &self.stop_after
                && self.index >= *readings
            {
                control.stop();
            }
            value
        }

        fn sleep(&mut self, duration: Duration) {
            self.slept.push(duration);
        }
    }

    #[test]
    fn the_loop_runs_ticks_in_order_and_stops_when_asked() {
        let control = Control::new();
        let mut clock = FakeClock::real_time();
        let seen = Mutex::new(Vec::new());

        let stopper = control.clone();
        let ran = run(&mut clock, &control, |tick| {
            seen.lock().expect("lock").push(tick);
            if tick == 9 {
                stopper.stop();
            }
        });

        assert_eq!(ran, 10);
        let seen = seen.into_inner().expect("lock");
        assert_eq!(
            seen,
            (0..10).collect::<Vec<_>>(),
            "ticks must be sequential"
        );
    }

    #[test]
    fn a_stop_before_the_first_tick_runs_nothing() {
        let control = Control::new();
        control.stop();
        let mut clock = FakeClock::real_time();

        let ran = run(&mut clock, &control, |_| panic!("should not tick"));
        assert_eq!(ran, 0);
    }

    #[test]
    fn startup_time_is_not_charged_as_debt() {
        // The first elapsed reading covers thread spawn and world loading. If
        // it were fed to the accumulator, a server that took two seconds to
        // start would immediately run its catch-up cap and log dropped ticks
        // before doing anything at all.
        let control = Control::new();
        // A huge first reading (startup), then normal frames.
        let mut clock = FakeClock::new(vec![Duration::from_secs(30), TICK_DURATION]);

        let stopper = control.clone();
        let ran = run(&mut clock, &control, move |tick| {
            if tick == 2 {
                stopper.stop();
            }
        });

        assert_eq!(ran, 3);
        assert_eq!(
            control.dropped(),
            0,
            "startup time must not be charged to the simulation"
        );
    }

    #[test]
    fn an_idle_loop_sleeps_rather_than_spinning() {
        let control = Control::new();
        // Frames far shorter than a tick: nothing is ever due, so the loop must
        // sleep every iteration rather than spinning a core at 100%.
        let mut clock = FakeClock::new(vec![Duration::ZERO, Duration::from_millis(5)])
            .stopping_after(6, &control);

        let ran = run(&mut clock, &control, |_| panic!("nothing is due"));

        assert_eq!(ran, 0);
        assert!(!clock.slept.is_empty(), "an idle loop must sleep");
        assert!(
            clock.slept.iter().all(|d| *d <= TICK_DURATION),
            "no sleep may exceed one tick, or shutdown would lag: {:?}",
            clock.slept
        );
        // 5 ms a frame against a 50 ms tick: the sleep should shrink as debt
        // builds, not sit at a constant.
        assert!(
            clock.slept.windows(2).any(|w| w[1] < w[0]),
            "the sleep should shorten as the next tick approaches: {:?}",
            clock.slept
        );
    }

    #[test]
    fn a_stall_drops_ticks_instead_of_chasing_them() {
        let control = Control::new();
        // One ten-second stall, then back to real time.
        let mut clock =
            FakeClock::new(vec![Duration::ZERO, Duration::from_secs(10), TICK_DURATION]);

        let stopper = control.clone();
        let ran = run(&mut clock, &control, move |tick| {
            if tick == 20 {
                stopper.stop();
            }
        });

        assert_eq!(ran, 21);
        assert!(
            control.dropped() > 0,
            "a ten-second stall must drop ticks rather than chase 200 of them"
        );
        assert!(
            control.dropped() < 200,
            "and it must not drop more than were owed"
        );
    }

    #[test]
    fn the_control_handle_reports_progress() {
        let control = Control::new();
        let mut clock = FakeClock::real_time();

        let stopper = control.clone();
        let observer = control.clone();
        run(&mut clock, &control, move |tick| {
            if tick == 4 {
                stopper.stop();
            }
        });

        assert_eq!(observer.tick(), 5);
        assert!(observer.stopping());
    }
}
