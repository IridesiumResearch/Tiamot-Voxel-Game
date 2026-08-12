// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The reorder buffer between a player's inputs and the tick that applies them.
//!
//! # Why a buffer at all
//!
//! Task 09's design has inputs sent with redundancy — the last three in every
//! packet — over a network that reorders and drops. So the server may see the
//! same input several times, see tick 41 after tick 42, or never see tick 40 at
//! all. The tick loop needs one answer per tick regardless, because a fixed
//! timestep is what makes the simulation deterministic (charter rule 4), and
//! "wait for the input" is not available to it.
//!
//! **The transport delivers none of that today**, and this buffer is built for
//! it anyway. Inputs travel on a bidirectional QUIC stream, which is reliable
//! and ordered, so `App::report_input` sends one input per tick and no
//! duplicates — see its docs for why triplicating them over a reliable stream
//! would be bandwidth spent against a loss that cannot happen. What this
//! absorbs today is the client's own tick drift and the gaps a stall leaves.
//! What it is READY for is the day inputs move to datagrams, which is a change
//! on the sending side alone.
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

        // **A jump is an edge, and only the movement around it is a state.**
        //
        // Repeating the last intent is what makes a dropped packet invisible for
        // walking: a player holding forward through a lost tick should keep
        // walking. Repeating a JUMP re-presses the key — so a client that sends
        // one jump gets a server that jumps again on the next tick nobody spoke
        // for, and the two simulations part company at exactly the moment the
        // player is in the air.
        //
        // It became reachable the day the client started sending one jump per
        // press instead of one per tick held. Before that the repeat was covered
        // by the client sending the same thing anyway, which is why it sat here
        // unnoticed: the bug was latent in this file and armed from another.
        //
        // Cleared after it is answered rather than filtered out of the repeat, so
        // there is one place where a jump can be consumed and it cannot be
        // reached twice.
        let intent = self.last_intent;
        self.last_intent.jump = false;
        intent
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

    /// A jump, and the walk that carries on around it.
    fn jumping() -> Intent {
        Intent {
            walk: [1.0, 0.0],
            jump: true,
            gait: Gait::Walk,
        }
    }

    #[test]
    fn a_repeated_input_keeps_walking_but_does_not_jump_again() {
        // **A jump is an edge; the movement around it is a state.** Repeating the
        // last intent is what makes a dropped packet invisible for walking. Doing
        // it for a jump re-presses the key, so a client that sends one jump gets
        // a server that jumps again on the next tick nobody spoke for — and the
        // two part company while the player is in the air.
        //
        // Reported from the window as a jolt right after a jump, with the client's
        // own footing counter reading 5 changes where a jump is 2.
        let mut queue = InputQueue::new(0);
        assert!(queue.offer(1, jumping()));

        let first = queue.take(1);
        assert!(first.jump, "the tick the input arrived for must jump");

        // Nothing arrives for the next few ticks: a dropped packet, or a client
        // whose frame hitched.
        for tick in 2..=5 {
            let repeated = queue.take(tick);
            assert!(
                !repeated.jump,
                "tick {tick} jumped again from a repeat; one press is one jump"
            );
            assert!(
                (repeated.walk[0] - 1.0).abs() < 1e-6,
                "tick {tick} stopped walking; only the jump is an edge: {repeated:?}"
            );
        }
    }

    #[test]
    fn a_second_press_still_jumps() {
        // The other half: clearing the edge must not make the queue deaf to the
        // next one.
        let mut queue = InputQueue::new(0);
        assert!(queue.offer(1, jumping()));
        assert!(queue.take(1).jump);
        assert!(!queue.take(2).jump);

        assert!(queue.offer(3, jumping()));
        assert!(
            queue.take(3).jump,
            "a fresh press after a repeat did not reach the body"
        );
    }

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
