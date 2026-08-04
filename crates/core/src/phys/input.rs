// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The reorder buffer between a player's inputs and the tick that applies them.
//!
//! # Why a buffer at all
//!
//! Inputs are sent with redundancy — Task 09's design puts the last three in
//! every packet — over a network that reorders and drops. So the server sees
//! the same input several times, sees tick 41 after tick 42, and sometimes
//! never sees tick 40 at all. The tick loop needs one answer per tick anyway,
//! because a fixed timestep is what makes the simulation deterministic
//! (charter rule 4), and "wait for the input" is not available to it.
//!
//! This turns that stream into exactly one [`Intent`] per tick:
//!
//! - **duplicates are ignored**, so the redundancy costs bandwidth and nothing
//!   else;
//! - **reordering is absorbed**, because an input is filed under its own tick
//!   rather than the order it arrived in;
//! - **a short gap repeats the last intent** rather than stopping the player,
//!   which makes a single lost packet invisible instead of a stutter — but a
//!   LONG gap gives up and stands still, because a client that has stopped
//!   talking should not keep walking;
//! - **the far future is refused**, so a peer cannot make the server hold an
//!   unbounded map by claiming enormous tick numbers.
//!
//! # It has to be exactly reproducible
//!
//! The client runs the same physics over the same inputs to predict, and
//! rewinds and replays when the server disagrees. If the server filled a gap
//! differently from the client, every dropped packet would show up as a
//! correction. The gap rule is therefore part of the protocol, not an
//! implementation detail: **repeat the last applied intent**.

use std::collections::BTreeMap;

use super::Intent;

/// How far ahead of the tick being applied an input may claim to be.
///
/// A client legitimately runs ahead of the server by its own latency, so this
/// has to cover a bad connection — 64 ticks is 3.2 seconds at 20 Hz. Beyond it
/// the input is not early, it is wrong, and accepting it would let one peer
/// grow the map without bound.
pub const MAX_LOOKAHEAD: u64 = 64;

/// How many consecutive ticks a missing input may be covered by repeating the
/// last one.
///
/// Half a second. Repeating is what makes a dropped packet invisible; repeating
/// *forever* is what makes a player whose client hitched keep sprinting into a
/// hole with nobody driving. Past this the body gets a neutral intent and comes
/// to a stop, which is both safer and easier to understand from the outside.
pub const MAX_REPEAT_TICKS: u64 = 10;

/// One player's inputs, waiting for their ticks.
#[derive(Debug, Clone)]
pub struct InputQueue {
    /// Inputs not yet applied, keyed by the tick they belong to.
    ///
    /// `BTreeMap` for ordered iteration — charter rule 4 bans a hash map
    /// anywhere iteration order could reach a simulation result, and this is
    /// as close to one as it gets.
    pending: BTreeMap<u64, Intent>,
    /// The last tick [`take`](Self::take) answered for.
    last_applied: u64,
    /// What it answered, for repeating across a gap.
    last_intent: Intent,
    /// The tick a real input was last applied on.
    ///
    /// A tick number rather than a count of calls: silence is a duration, and
    /// measuring it in `take` calls would give a different answer if the
    /// caller ever skipped one.
    last_fresh: u64,
}

impl InputQueue {
    /// An empty queue whose first applied tick will be `start`.
    #[must_use]
    pub fn new(start: u64) -> Self {
        Self {
            pending: BTreeMap::new(),
            last_applied: start,
            last_intent: Intent::default(),
            last_fresh: start,
        }
    }

    /// Files an input under its tick.
    ///
    /// Returns whether it was kept. A `false` is normal traffic, not an error:
    /// it means the input was a duplicate of one already held, older than the
    /// tick the server has reached, or so far ahead it cannot be genuine.
    pub fn offer(&mut self, tick: u64, intent: Intent) -> bool {
        if tick <= self.last_applied || tick > self.last_applied + MAX_LOOKAHEAD {
            return false;
        }
        // `insert` would overwrite; the first copy of a redundant input wins so
        // that a duplicate can never change an answer already computed from it.
        if self.pending.contains_key(&tick) {
            return false;
        }
        self.pending.insert(tick, intent);
        true
    }

    /// The intent to simulate for `tick`, consuming it.
    ///
    /// Everything older than `tick` is dropped at the same time: an input that
    /// missed its tick is not useful later, and keeping it would let a lagging
    /// client accumulate a backlog that replays as a burst of movement.
    pub fn take(&mut self, tick: u64) -> Intent {
        // Drop what is now in the past, remembering the newest of them as the
        // most recent thing the player actually asked for.
        let mut fresh = false;
        while let Some((&first, _)) = self.pending.iter().next() {
            if first > tick {
                break;
            }
            let (_, intent) = self
                .pending
                .pop_first()
                .unwrap_or((first, self.last_intent));
            self.last_intent = intent;
            fresh = true;
        }

        self.last_applied = tick;
        if fresh {
            self.last_fresh = tick;
        } else if tick.saturating_sub(self.last_fresh) > MAX_REPEAT_TICKS {
            // Nobody is driving. Stop rather than carry on with a stale
            // instruction — see MAX_REPEAT_TICKS.
            self.last_intent = Intent::default();
        }
        self.last_intent
    }

    /// The last tick this queue answered for.
    #[must_use]
    pub const fn last_applied(&self) -> u64 {
        self.last_applied
    }

    /// How many inputs are waiting.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phys::Gait;

    /// Identifies an intent by its steering value's bits.
    ///
    /// Exact identity is the point — an input must arrive verbatim or not at
    /// all — and comparing bit patterns says that, where comparing two `f32`s
    /// with `==` asks a question floats cannot answer.
    fn tag(intent: Intent) -> u32 {
        intent.walk[0].to_bits()
    }

    fn walking(x: f32) -> Intent {
        Intent {
            walk: [x, 0.0],
            jump: false,
            gait: Gait::Walk,
        }
    }

    #[test]
    fn inputs_apply_on_their_own_tick() {
        let mut queue = InputQueue::new(0);
        assert!(queue.offer(1, walking(1.0)));
        assert!(queue.offer(2, walking(2.0)));

        assert_eq!(tag(queue.take(1)), tag(walking(1.0)));
        assert_eq!(tag(queue.take(2)), tag(walking(2.0)));
    }

    #[test]
    fn a_reordered_input_still_lands_on_its_own_tick() {
        // The whole reason inputs carry their tick number. Arrival order is a
        // property of the network; the simulation must not depend on it.
        let mut queue = InputQueue::new(0);
        assert!(queue.offer(2, walking(2.0)));
        assert!(queue.offer(1, walking(1.0)));

        assert_eq!(
            tag(queue.take(1)),
            tag(walking(1.0)),
            "tick 1 got tick 2's input"
        );
        assert_eq!(tag(queue.take(2)), tag(walking(2.0)));
    }

    #[test]
    fn a_duplicate_is_ignored_rather_than_applied_twice() {
        // Every packet carries the last three inputs, so the server sees most
        // of them three times. The first copy wins, because a later duplicate
        // must not be able to change an answer already computed from it.
        let mut queue = InputQueue::new(0);
        assert!(queue.offer(1, walking(1.0)));
        assert!(
            !queue.offer(1, walking(99.0)),
            "a second copy of tick 1 should be refused"
        );

        assert_eq!(tag(queue.take(1)), tag(walking(1.0)));
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn a_gap_repeats_the_last_intent_rather_than_stopping_the_player() {
        // A single lost packet must be invisible. Stopping dead for one tick
        // and resuming is a stutter the player feels, and — worse — the client
        // predicting the same gap differently would turn one dropped packet
        // into a correction.
        let mut queue = InputQueue::new(0);
        queue.offer(1, walking(1.0));
        assert_eq!(tag(queue.take(1)), tag(walking(1.0)));

        // Tick 2 never arrived.
        assert_eq!(
            tag(queue.take(2)),
            tag(walking(1.0)),
            "a gap should hold the last intent, not reset to standing still"
        );

        queue.offer(3, walking(3.0));
        assert_eq!(
            tag(queue.take(3)),
            tag(walking(3.0)),
            "and recover on the next input"
        );
    }

    #[test]
    fn an_input_that_missed_its_tick_is_refused_rather_than_queued() {
        // Applying it later would replay old movement at the wrong moment, and
        // a client lagging badly would build a backlog that arrives as a burst.
        let mut queue = InputQueue::new(0);
        queue.offer(1, walking(1.0));
        queue.take(1);

        assert!(!queue.offer(1, walking(9.0)), "tick 1 has already happened");
        assert!(!queue.offer(0, walking(9.0)));
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn a_late_input_is_dropped_when_its_tick_passes() {
        // It arrived in time to be filed but the tick loop overtook it. Taking
        // tick 5 must not leave tick 3 sitting in the map to fire later.
        let mut queue = InputQueue::new(0);
        queue.offer(3, walking(3.0));
        queue.offer(5, walking(5.0));

        assert_eq!(tag(queue.take(5)), tag(walking(5.0)));
        assert_eq!(
            queue.pending(),
            0,
            "tick 3 should have been dropped, not held"
        );
    }

    #[test]
    fn the_far_future_is_refused_so_a_peer_cannot_grow_the_map() {
        // Charter rule 14: a tick number from a peer is a claim. Without a
        // bound, one connection can make the server hold as many inputs as it
        // cares to invent.
        let mut queue = InputQueue::new(100);
        assert!(
            queue.offer(100 + MAX_LOOKAHEAD, walking(1.0)),
            "still early"
        );
        assert!(
            !queue.offer(100 + MAX_LOOKAHEAD + 1, walking(1.0)),
            "past the lookahead this is not early, it is wrong"
        );

        for tick in 0..10_000 {
            queue.offer(200_000 + tick, walking(1.0));
        }
        assert!(
            queue.pending() <= MAX_LOOKAHEAD as usize,
            "the queue grew to {} entries",
            queue.pending()
        );
    }

    #[test]
    fn a_long_silence_stops_the_player_rather_than_repeating_forever() {
        // Repeating covers a dropped packet. Repeating for ever means a player
        // whose client hitched keeps sprinting with nobody driving — and it is
        // what made a real client look "stuck": its last accepted input was
        // "standing still", so the server held it there while the client
        // predicted movement and was corrected twenty times a second.
        let mut queue = InputQueue::new(0);
        queue.offer(1, walking(1.0));
        assert_eq!(tag(queue.take(1)), tag(walking(1.0)));

        // Within the window, the intent holds.
        for tick in 2..=MAX_REPEAT_TICKS {
            assert_eq!(
                tag(queue.take(tick)),
                tag(walking(1.0)),
                "gave up at tick {tick}, inside the repeat window"
            );
        }

        // Past it, the body is given nothing to do.
        let stopped = queue.take(MAX_REPEAT_TICKS + 2);
        assert_eq!(
            tag(stopped),
            tag(walking(0.0)),
            "still repeating a stale input after {MAX_REPEAT_TICKS} ticks of silence"
        );
    }

    #[test]
    fn the_same_arrival_orders_produce_the_same_answers() {
        // Underwrites prediction: the client fills gaps with this same rule, so
        // two queues fed the same inputs in different orders must agree tick
        // for tick, or every reordered packet becomes a correction.
        let mut in_order = InputQueue::new(0);
        let mut shuffled = InputQueue::new(0);

        for tick in [1u64, 2, 4, 5] {
            in_order.offer(tick, walking(tick as f32));
        }
        for tick in [5u64, 1, 4, 2] {
            shuffled.offer(tick, walking(tick as f32));
        }

        for tick in 1..=6 {
            assert_eq!(
                tag(in_order.take(tick)),
                tag(shuffled.take(tick)),
                "the two disagreed at tick {tick}"
            );
        }
    }
}
