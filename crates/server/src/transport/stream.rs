// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Per-connection chunk streaming.
//!
//! Tracks what one player has been sent, works out what they still need, and
//! paces the sending so a joining player does not monopolise the simulation.
//!
//! # The interest centre is server-side state
//!
//! There is no protocol message carrying a player's position, and there should
//! not be one: a client that told the server where it was could tell it
//! anything, and every anti-cheat problem in a voxel game starts there. The
//! centre is authoritative server state, initialised to spawn, and Task 09's
//! physics moves it. Until then a player's interest set is the spawn
//! neighbourhood — which is a real limitation, not a placeholder that will be
//! swapped out: the streaming machinery below is complete, and physics only
//! needs to write [`Streamer::recentre`].
//!
//! # Why a sent-set rather than a diff
//!
//! Recomputing "everything within range, minus everything already sent" every
//! pass is O(interest), around 1800 set lookups. Computing a diff against the
//! previous centre would be cheaper, but it goes wrong quietly: a chunk whose
//! send failed, or that was dropped because the queue was full, is absent from
//! both sets and is never retried. The sent-set is self-correcting — anything
//! missing gets picked up on the next pass, whatever went wrong.

use std::collections::BTreeSet;

use tiamot_core::ChunkPos;
use tiamot_core::interest::{self, ViewDistance};

/// What one connection has been sent, and what it still needs.
pub struct Streamer {
    centre: ChunkPos,
    view: ViewDistance,
    /// Chunks the client has, or has been sent.
    ///
    /// A `BTreeSet` rather than a `HashSet`: the iteration order matters for
    /// reproducible unload ordering in tests, and at ~1800 entries the lookup
    /// difference is not measurable next to encoding a chunk.
    sent: BTreeSet<ChunkPos>,
    /// Chunks requested from the simulation but not yet answered.
    ///
    /// A **set of positions**, not a count. A count is not enough: `next_needed`
    /// filters against what has been *delivered*, so a chunk still in flight
    /// looks un-requested and gets asked for a second time. On a fast machine
    /// the reply usually lands before the next pass and it never shows; on a
    /// slower one the client receives the same chunk twice. CI on macOS caught
    /// exactly that.
    in_flight: BTreeSet<ChunkPos>,
}

impl Streamer {
    /// A streamer centred on a player's spawn.
    #[must_use]
    pub fn new(centre: ChunkPos, view: ViewDistance) -> Self {
        Self {
            centre,
            view,
            sent: BTreeSet::new(),
            in_flight: BTreeSet::new(),
        }
    }

    /// The current interest centre.
    #[must_use]
    pub const fn centre(&self) -> ChunkPos {
        self.centre
    }

    /// How many chunks this client has been sent.
    #[must_use]
    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }

    /// How many requests are outstanding.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether every chunk in range has been sent.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.in_flight.is_empty() && self.next_needed(usize::MAX).is_empty()
    }

    /// Moves the interest centre, returning chunks that left range.
    ///
    /// The caller sends a `ChunkUnload` for each. They are forgotten here, so
    /// walking back into an area streams it again — which is correct, because
    /// a client told to unload a chunk has thrown it away.
    pub fn recentre(&mut self, centre: ChunkPos) -> Vec<ChunkPos> {
        if centre == self.centre {
            return Vec::new();
        }
        self.centre = centre;
        // Requests for chunks that just left range are abandoned. Keeping them
        // would deliver a chunk the client was told to unload, and hold budget
        // that the new neighbourhood needs.
        self.in_flight
            .retain(|pos| interest::contains(centre, self.view, *pos));

        let departed: Vec<ChunkPos> = self
            .sent
            .iter()
            .copied()
            .filter(|pos| !interest::contains(centre, self.view, *pos))
            .collect();
        for pos in &departed {
            self.sent.remove(pos);
        }
        departed
    }

    /// Up to `limit` chunks in range that are neither sent nor in flight,
    /// nearest first.
    ///
    /// Does not mark them sent — the caller does that once the send succeeds,
    /// so a failure leaves them to be retried.
    #[must_use]
    pub fn next_needed(&self, limit: usize) -> Vec<ChunkPos> {
        if limit == 0 {
            return Vec::new();
        }
        interest::chunks_around(self.centre, self.view)
            .into_iter()
            .filter(|pos| !self.sent.contains(pos) && !self.in_flight.contains(pos))
            .take(limit)
            .collect()
    }

    /// How many more requests this connection may have outstanding.
    #[must_use]
    pub fn budget(&self, in_flight_cap: usize) -> usize {
        in_flight_cap.saturating_sub(self.in_flight.len())
    }

    /// Records that a chunk has been asked for.
    pub fn requested(&mut self, pos: ChunkPos) {
        self.in_flight.insert(pos);
    }

    /// Records that a request came back, whether or not it produced a chunk.
    ///
    /// Called on **every** outcome — delivered, empty, failed. An in-flight
    /// entry only cleared on success would hold its slot forever, until the
    /// connection stopped asking for anything and the player's world stopped
    /// filling in.
    pub fn completed(&mut self, pos: ChunkPos) {
        self.in_flight.remove(&pos);
    }

    /// Records that a chunk reached the client.
    pub fn delivered(&mut self, pos: ChunkPos) {
        self.in_flight.remove(&pos);
        self.sent.insert(pos);
    }

    /// Whether a chunk is one this client holds.
    ///
    /// Used to decide whether a block edit is worth forwarding: an edit in a
    /// chunk the client has never seen is noise.
    #[must_use]
    pub fn holds(&self, pos: ChunkPos) -> bool {
        self.sent.contains(&pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: ChunkPos = ChunkPos::new(0, 0, 0);

    fn streamer() -> Streamer {
        Streamer::new(ORIGIN, ViewDistance::MINIMUM)
    }

    #[test]
    fn a_fresh_streamer_needs_its_whole_interest_set() {
        let streamer = streamer();
        let needed = streamer.next_needed(usize::MAX);
        assert_eq!(
            needed.len(),
            interest::chunks_around(ORIGIN, ViewDistance::MINIMUM).len()
        );
        assert_eq!(needed[0], ORIGIN, "nearest first");
        assert!(!streamer.is_complete());
    }

    #[test]
    fn delivered_chunks_are_not_requested_again() {
        let mut streamer = streamer();
        let first = streamer.next_needed(3);
        for pos in &first {
            streamer.delivered(*pos);
        }

        let second = streamer.next_needed(usize::MAX);
        for pos in &first {
            assert!(
                !second.contains(pos),
                "{pos:?} was delivered and must not be needed again"
            );
        }
    }

    #[test]
    fn a_streamer_becomes_complete_once_everything_is_delivered() {
        let mut streamer = streamer();
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        assert!(streamer.is_complete());
        assert!(streamer.next_needed(usize::MAX).is_empty());
    }

    #[test]
    fn the_limit_is_respected() {
        let streamer = streamer();
        assert_eq!(streamer.next_needed(2).len(), 2);
        assert_eq!(streamer.next_needed(0).len(), 0);
    }

    #[test]
    fn moving_unloads_what_left_range_and_loads_what_entered() {
        let mut streamer = streamer();
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        assert!(streamer.is_complete());

        // Three chunks east: the whole original neighbourhood is out of a
        // radius-1 view.
        let departed = streamer.recentre(ChunkPos::new(3, 0, 0));

        assert!(!departed.is_empty(), "moving away must unload something");
        for pos in &departed {
            assert!(
                !interest::contains(ChunkPos::new(3, 0, 0), ViewDistance::MINIMUM, *pos),
                "{pos:?} was unloaded but is still in range"
            );
        }
        assert!(
            !streamer.is_complete(),
            "the new neighbourhood still needs streaming"
        );
    }

    #[test]
    fn an_unloaded_chunk_is_streamed_again_on_return() {
        // A client told to unload a chunk has thrown it away. Remembering that
        // it once had it would leave a hole in the world when the player walked
        // back.
        let mut streamer = streamer();
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }

        let departed = streamer.recentre(ChunkPos::new(5, 0, 0));
        let returned = streamer.recentre(ORIGIN);
        let needed = streamer.next_needed(usize::MAX);

        assert!(
            departed.iter().all(|pos| needed.contains(pos)),
            "chunks unloaded on the way out must be re-sent on the way back"
        );
        let _ = returned;
    }

    #[test]
    fn recentring_to_the_same_place_changes_nothing() {
        let mut streamer = streamer();
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        assert!(streamer.recentre(ORIGIN).is_empty());
        assert!(streamer.is_complete(), "a no-op move must not re-stream");
    }

    #[test]
    fn moving_within_range_keeps_the_overlap() {
        // A player taking one step must not be re-sent their whole
        // neighbourhood. If this failed, walking would saturate the link.
        let mut streamer = Streamer::new(ORIGIN, ViewDistance::DEFAULT);
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        let before = streamer.sent_count();

        let departed = streamer.recentre(ChunkPos::new(1, 0, 0));

        assert!(
            departed.len() < before / 4,
            "one chunk of movement unloaded {} of {before} chunks, which means the \
             overlap is not being kept",
            departed.len()
        );
        assert!(
            streamer.sent_count() > before / 2,
            "most of the neighbourhood should have been retained"
        );
    }

    #[test]
    fn a_chunk_in_flight_is_not_requested_again() {
        // The bug macOS CI caught. `next_needed` used to filter only against
        // what had been DELIVERED, so a chunk still in flight looked
        // un-requested and was asked for a second time — and the client
        // received it twice. On a fast machine the reply landed before the next
        // pass and it never showed.
        let mut streamer = streamer();

        let first = streamer.next_needed(2);
        assert_eq!(first.len(), 2);
        for pos in &first {
            streamer.requested(*pos);
        }

        let second = streamer.next_needed(usize::MAX);
        for pos in &first {
            assert!(
                !second.contains(pos),
                "{pos:?} is in flight and must not be requested again"
            );
        }
    }

    #[test]
    fn in_flight_accounting_survives_a_failed_request() {
        // If `completed` were only called on success, dropped requests would
        // hold their slots permanently and the player's world would simply stop
        // filling in — with nothing logged.
        let mut streamer = streamer();
        assert_eq!(streamer.budget(4), 4);

        let targets = streamer.next_needed(2);
        for pos in &targets {
            streamer.requested(*pos);
        }
        assert_eq!(streamer.budget(4), 2);

        // One delivered, one failed.
        streamer.delivered(targets[0]);
        streamer.completed(targets[1]);

        assert_eq!(
            streamer.budget(4),
            4,
            "a failed request must return its slot too"
        );
        assert!(
            streamer.next_needed(usize::MAX).contains(&targets[1]),
            "the failed chunk must be retried"
        );
        assert!(
            !streamer.next_needed(usize::MAX).contains(&targets[0]),
            "the delivered chunk must not be"
        );
    }

    #[test]
    fn completing_something_never_requested_is_harmless() {
        let mut streamer = streamer();
        streamer.completed(ORIGIN);
        streamer.completed(ORIGIN);
        assert_eq!(streamer.in_flight(), 0);
        assert_eq!(streamer.budget(4), 4);
    }

    #[test]
    fn moving_away_abandons_requests_for_chunks_that_left_range() {
        // Otherwise the reply arrives for a chunk the client was told to
        // unload, and the slot it holds is one the new neighbourhood needs.
        let mut streamer = streamer();
        let targets = streamer.next_needed(3);
        for pos in &targets {
            streamer.requested(*pos);
        }
        assert_eq!(streamer.in_flight(), 3);

        streamer.recentre(ChunkPos::new(20, 0, 0));
        assert_eq!(
            streamer.in_flight(),
            0,
            "requests for chunks now out of range must be abandoned"
        );
        assert_eq!(streamer.budget(4), 4);
    }

    #[test]
    fn holds_reports_what_the_client_actually_has() {
        let mut streamer = streamer();
        assert!(!streamer.holds(ORIGIN));
        streamer.delivered(ORIGIN);
        assert!(streamer.holds(ORIGIN));

        streamer.recentre(ChunkPos::new(9, 0, 0));
        assert!(
            !streamer.holds(ORIGIN),
            "an unloaded chunk is no longer held"
        );
    }

    #[test]
    fn a_streamer_is_not_complete_while_requests_are_outstanding() {
        // Otherwise a connection would decide it had finished streaming while
        // chunks were still on their way, and stop asking.
        let mut streamer = streamer();
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        streamer.requested(ORIGIN);
        assert!(!streamer.is_complete());
        streamer.completed(ORIGIN);
        assert!(streamer.is_complete());
    }
}
