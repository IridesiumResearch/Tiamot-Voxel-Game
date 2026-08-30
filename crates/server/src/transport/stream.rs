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

use std::collections::{BTreeMap, BTreeSet};

use tiamot_core::ChunkPos;
use tiamot_core::interest::{self, ViewDistance};
use tiamot_core::lod::{Level, Rings, horizon_for};

/// What one connection has been sent, and what it still needs.
pub struct Streamer {
    /// The domain every position in here belongs to.
    ///
    /// **Interest is domain-scoped**, which is not a rule enforced on top of
    /// this but what the sets already mean: a chunk at `(0, 0, 0)` in one
    /// domain and a chunk at `(0, 0, 0)` in another are different chunks, and
    /// a set of positions can only be about one of them.
    domain: String,
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
    /// How far the horizon reaches, past the detail radius.
    ///
    /// Chunks between `view` and this are sent as summaries. Kept separate
    /// from `view` because they are answers to different questions: `view` is
    /// what the client asked for and the server granted, and this is how much
    /// further the engine is willing to draw the shape of the land for free.
    horizon: ViewDistance,
    /// Which level each distance band takes, and the hysteresis on the edges.
    rings: Rings,
    /// Summaries the client holds, and the level each was sent at.
    ///
    /// A position is in this OR in `sent`, never both: it is one chunk, and a
    /// client holding a coarse copy and a fine one would draw both, with the
    /// coarse one poking through.
    summaries: BTreeMap<ChunkPos, u8>,
}

impl Streamer {
    /// A streamer centred on a player's spawn.
    #[must_use]
    pub fn new(domain: &str, centre: ChunkPos, view: ViewDistance) -> Self {
        Self {
            domain: domain.to_owned(),
            centre,
            view,
            sent: BTreeSet::new(),
            in_flight: BTreeSet::new(),
            horizon: horizon_for(view),
            rings: Rings::new(u32::from(view.horizontal), Rings::MARGIN),
            summaries: BTreeMap::new(),
        }
    }

    /// The current interest centre.
    #[must_use]
    pub const fn centre(&self) -> ChunkPos {
        self.centre
    }

    /// Which domain this connection is being streamed from.
    #[must_use]
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Moves this connection to another domain, returning everything to drop.
    ///
    /// **Everything, not what left range.** The client holds a set of chunks
    /// belonging to the domain it was in, and none of them mean anything in the
    /// new one — the positions are the same and the contents are not. So the
    /// whole sent-set comes back, the in-flight requests are abandoned, and the
    /// new domain streams in from nothing.
    ///
    /// The caller is expected to tell the client with a single domain-switch
    /// message rather than to send an unload per position: at the default view
    /// distance that is upwards of a thousand messages to say one thing.
    ///
    /// Switching to the domain already being streamed does nothing and returns
    /// nothing, so a caller that checks every tick costs a string compare.
    pub fn switch_to(&mut self, domain: &str, centre: ChunkPos) -> Vec<ChunkPos> {
        if domain == self.domain {
            return Vec::new();
        }
        self.domain = domain.to_owned();
        self.centre = centre;
        // Abandoned rather than awaited. A reply carrying a chunk of the domain
        // this connection has just left would be decoded into the new one at
        // the same coordinates, which is terrain from somewhere else appearing
        // in a place a player is standing.
        self.in_flight.clear();
        let mut dropped: Vec<ChunkPos> = std::mem::take(&mut self.sent).into_iter().collect();
        dropped.extend(std::mem::take(&mut self.summaries).into_keys());
        dropped
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

    /// The radius this client is being streamed at.
    #[must_use]
    pub const fn view(&self) -> ViewDistance {
        self.view
    }

    /// Changes the radius, returning chunks that left range.
    ///
    /// The same contract as [`Streamer::recentre`] and for the same reason —
    /// the caller sends a `ChunkUnload` for each returned position, and a
    /// client told to unload has thrown the chunk away, so shrinking and then
    /// growing again streams it back.
    ///
    /// Growing returns nothing: no chunk leaves range when the range gets
    /// bigger, and the new ones arrive through [`Streamer::next_needed`] on the
    /// next pump like any other.
    pub fn resize(&mut self, view: ViewDistance) -> Vec<ChunkPos> {
        if view == self.view {
            return Vec::new();
        }
        self.view = view;
        self.horizon = horizon_for(view);
        self.rings = Rings::new(u32::from(view.horizontal), Rings::MARGIN);
        self.in_flight
            .retain(|pos| interest::contains(self.centre, self.horizon, *pos));
        self.departed()
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
        // that the new neighbourhood needs. Against the HORIZON rather than the
        // detail radius: a chunk that left the detail radius has not left the
        // client's world, it has become a summary.
        self.in_flight
            .retain(|pos| interest::contains(centre, self.horizon, *pos));
        self.departed()
    }

    /// Forgets everything past the horizon, and says what left.
    ///
    /// Shared by [`Streamer::recentre`] and [`Streamer::resize`], which differ
    /// only in which of the two numbers moved. A chunk that fell out of the
    /// detail radius but is still inside the horizon is NOT departed — it is
    /// about to be re-sent as a summary, and unloading it first would blink a
    /// hole in the world the size of a chunk.
    fn departed(&mut self) -> Vec<ChunkPos> {
        let horizon = self.horizon;
        let centre = self.centre;
        let mut departed: Vec<ChunkPos> = self
            .sent
            .iter()
            .copied()
            .filter(|pos| !interest::contains(centre, horizon, *pos))
            .collect();
        departed.extend(
            self.summaries
                .keys()
                .copied()
                .filter(|pos| !interest::contains(centre, horizon, *pos)),
        );
        for pos in &departed {
            self.sent.remove(pos);
            self.summaries.remove(pos);
        }
        departed.sort_unstable();
        departed.dedup();
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
    ///
    /// Takes the position out of the summary set for the reason given on
    /// [`Streamer::summarised`]: a client holds one copy of a chunk, and this
    /// one has just replaced a coarse copy with the real thing.
    pub fn delivered(&mut self, pos: ChunkPos) {
        self.in_flight.remove(&pos);
        self.summaries.remove(&pos);
        self.sent.insert(pos);
    }

    /// How far the horizon reaches for this connection.
    #[must_use]
    pub const fn horizon(&self) -> ViewDistance {
        self.horizon
    }

    /// How many summaries this client holds.
    #[must_use]
    pub fn summary_count(&self) -> usize {
        self.summaries.len()
    }

    /// The level a client holds a chunk at, if it holds a summary of it.
    #[must_use]
    pub fn summary_level(&self, pos: ChunkPos) -> Option<u8> {
        self.summaries.get(&pos).copied()
    }

    /// Up to `limit` chunks in the horizon whose summary the client does not
    /// have at the level its distance calls for, nearest first.
    ///
    /// **Hysteresis lives here**, not at the caller: a chunk already held at a
    /// level keeps it until the player is a whole margin past the ring edge, so
    /// somebody pacing across a boundary does not re-send — and the client does
    /// not rebuild — a band of the horizon every step. See
    /// [`tiamot_core::lod::Rings::stable_level`].
    ///
    /// Does not mark anything sent, for the same reason [`Streamer::next_needed`]
    /// does not.
    #[must_use]
    pub fn next_summaries(&self, limit: usize) -> Vec<(ChunkPos, u8)> {
        if limit == 0 {
            return Vec::new();
        }
        interest::chunks_around(self.centre, self.horizon)
            .into_iter()
            .filter(|pos| !self.in_flight.contains(pos))
            .filter_map(|pos| {
                let held = self.summaries.get(&pos).map(|level| Level::Summary(*level));
                // A chunk the client holds in full is at the detail level as
                // far as the hysteresis is concerned: that is what it is
                // drawing, and it is what a level change would replace.
                let held = if self.sent.contains(&pos) {
                    Some(Level::Chunk)
                } else {
                    held
                };
                match self.rings.stable_level(held, self.distance(pos)) {
                    Level::Chunk => None,
                    Level::Summary(level) if held == Some(Level::Summary(level)) => None,
                    Level::Summary(level) => Some((pos, level)),
                }
            })
            .take(limit)
            .collect()
    }

    /// The Chebyshev distance from the centre, in chunks.
    ///
    /// Chebyshev because the interest set is a box: a sphere's distance would
    /// put the corners of the box in a further ring than the sides, and a
    /// player turning on the spot would watch the horizon change resolution.
    fn distance(&self, pos: ChunkPos) -> u32 {
        let dx = pos.x.abs_diff(self.centre.x);
        let dy = pos.y.abs_diff(self.centre.y);
        let dz = pos.z.abs_diff(self.centre.z);
        dx.max(dy).max(dz)
    }

    /// Records that a summary reached the client.
    ///
    /// Takes the position out of the full-chunk set: it is one chunk, and the
    /// client has just replaced what it held with a coarser copy.
    pub fn summarised(&mut self, pos: ChunkPos, level: u8) {
        self.in_flight.remove(&pos);
        self.sent.remove(&pos);
        self.summaries.insert(pos, level);
    }

    /// Forgets a summary, so the next pass sends it again.
    ///
    /// What an edit in a summarised chunk costs. A block delta is no use to a
    /// client holding a summary — it has nowhere to put one cell of 27 — so the
    /// horizon is re-sent instead.
    pub fn resummarise(&mut self, pos: ChunkPos) {
        self.summaries.remove(&pos);
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
        Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::MINIMUM,
        )
    }

    #[test]
    fn moving_domain_takes_back_every_chunk_and_not_just_the_ones_out_of_range() {
        // **The whole set, because none of it means anything any more.** The
        // client holds chunks belonging to the space it is leaving, and every
        // one of their positions names a different chunk in the space it is
        // entering. A switch that only returned what left range would leave
        // terrain from somewhere else standing exactly where the player is.
        let mut streamer = streamer();
        let needed = streamer.next_needed(8);
        assert!(!needed.is_empty(), "nothing to send makes this vacuous");
        for pos in &needed {
            streamer.requested(*pos);
            streamer.delivered(*pos);
        }
        assert_eq!(streamer.sent_count(), needed.len());

        let dropped = streamer.switch_to("mod:ship/17", ORIGIN);
        assert_eq!(
            dropped.len(),
            needed.len(),
            "a domain switch kept chunks the client can no longer use"
        );
        assert_eq!(streamer.sent_count(), 0);
        assert_eq!(streamer.domain(), "mod:ship/17");
    }

    #[test]
    fn a_request_outstanding_when_the_domain_changes_is_abandoned() {
        // A reply carrying a chunk of the domain just left would be decoded
        // into the new one at the same coordinates — terrain from somewhere
        // else, in a place somebody is standing.
        let mut streamer = streamer();
        let pos = streamer.next_needed(1)[0];
        streamer.requested(pos);
        assert_eq!(streamer.in_flight(), 1);

        streamer.switch_to("mod:ship/17", ORIGIN);
        assert_eq!(
            streamer.in_flight(),
            0,
            "a chunk of the old domain was still expected after the move"
        );
        assert!(
            streamer.next_needed(usize::MAX).contains(&pos),
            "the new domain's chunk at that position was never asked for"
        );
    }

    #[test]
    fn switching_to_the_domain_already_being_streamed_costs_nothing() {
        // The connection checks every tick, so the common answer has to be
        // cheap and has to change nothing.
        let mut streamer = streamer();
        let pos = streamer.next_needed(1)[0];
        streamer.requested(pos);
        streamer.delivered(pos);

        let dropped = streamer.switch_to(tiamot_core::domain::OVERWORLD, ORIGIN);
        assert!(dropped.is_empty(), "a no-op switch threw the world away");
        assert_eq!(streamer.sent_count(), 1);
    }

    #[test]
    fn shrinking_the_radius_unloads_what_left_range() {
        // The client asking to see less has to actually cost less, and the
        // chunks it can no longer see have to be taken off it — otherwise
        // "reduce your view distance" would free nothing on either side, which
        // is the entire reason somebody reaches for the setting.
        //
        // **What "range" means changed with Task 15b.** A chunk that falls out
        // of the detail radius has not left the client's world; it becomes a
        // summary. Only the horizon unloads.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        for pos in streamer.next_needed(usize::MAX) {
            streamer.requested(pos);
            streamer.completed(pos);
            streamer.delivered(pos);
        }
        let before = streamer.sent_count();
        assert!(streamer.is_complete());

        let departed = streamer.resize(ViewDistance::MINIMUM);
        assert!(
            !departed.is_empty(),
            "shrinking to the minimum unloaded nothing"
        );
        assert_eq!(
            streamer.sent_count(),
            before - departed.len(),
            "the unload list and what is still held must agree"
        );
        assert_eq!(
            streamer.sent_count(),
            interest::chunks_around(ORIGIN, horizon_for(ViewDistance::MINIMUM)).len(),
            "what is left should be exactly the smaller horizon"
        );
        for pos in &departed {
            assert!(
                !interest::contains(ORIGIN, horizon_for(ViewDistance::MINIMUM), *pos),
                "{pos:?} was unloaded but is still inside the horizon"
            );
        }
    }

    #[test]
    fn growing_the_radius_unloads_nothing_and_asks_for_the_rest() {
        // No chunk leaves range when the range gets bigger, and the new ones
        // arrive through the ordinary pump rather than through a special path.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::MINIMUM,
        );
        for pos in streamer.next_needed(usize::MAX) {
            streamer.requested(pos);
            streamer.completed(pos);
            streamer.delivered(pos);
        }
        let held = streamer.sent_count();

        assert!(
            streamer.resize(ViewDistance::DEFAULT).is_empty(),
            "growing the radius unloaded something"
        );
        assert_eq!(streamer.sent_count(), held, "growing dropped a held chunk");
        assert!(
            !streamer.is_complete(),
            "growing the radius asked for nothing new"
        );
    }

    #[test]
    fn resizing_to_the_same_radius_is_a_no_op() {
        // A client re-sending its preference — on a reconnect, or every time a
        // settings screen closes — must not churn its whole interest set.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        for pos in streamer.next_needed(usize::MAX) {
            streamer.requested(pos);
            streamer.completed(pos);
            streamer.delivered(pos);
        }
        assert!(streamer.resize(ViewDistance::DEFAULT).is_empty());
        assert!(streamer.is_complete());
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

        // Far enough east that the whole original neighbourhood is outside the
        // HORIZON, not merely outside the detail radius — the second only turns
        // a chunk into a summary, which is not an unload.
        let away = ChunkPos::new(32, 0, 0);
        let departed = streamer.recentre(away);

        assert!(!departed.is_empty(), "moving away must unload something");
        for pos in &departed {
            assert!(
                !interest::contains(away, streamer.horizon(), *pos),
                "{pos:?} was unloaded but is still inside the horizon"
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
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
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
    fn the_horizon_starts_where_the_detail_radius_ends_and_never_overlaps_it() {
        // A client holding a chunk AND a summary of it would draw both, and the
        // coarse one would poke through the fine one. The two sets are disjoint
        // by construction, and this is the assertion that keeps them so.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        for (pos, level) in streamer.next_summaries(usize::MAX) {
            assert!(
                !streamer.holds(pos),
                "{pos:?} was to be summarised while the client held it in full"
            );
            assert!(level >= tiamot_core::lod::FINEST);
            streamer.summarised(pos, level);
        }
        assert!(streamer.summary_count() > 0, "no horizon was produced");
        assert!(
            streamer.next_summaries(usize::MAX).is_empty(),
            "a horizon already sent was asked for a second time"
        );
    }

    #[test]
    fn walking_forward_turns_a_summary_into_a_chunk_and_back_without_an_unload() {
        // The transition a player actually experiences. Neither direction is an
        // unload: a summary replaced by a chunk, and a chunk replaced by a
        // summary, are both one message about one position. Unloading first
        // would blink a chunk-sized hole in the world.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        let far = ChunkPos::new(12, 0, 0);
        let (pos, level) = streamer
            .next_summaries(usize::MAX)
            .into_iter()
            .find(|(pos, _)| *pos == far)
            .expect("a chunk twelve out should be summarised at a default view of eight");
        streamer.summarised(pos, level);
        assert_eq!(streamer.summary_level(far), Some(level));

        // Walk towards it until it is inside the detail radius.
        let departed = streamer.recentre(ChunkPos::new(8, 0, 0));
        assert!(
            !departed.contains(&far),
            "walking towards a chunk unloaded it"
        );
        assert!(
            streamer.next_needed(usize::MAX).contains(&far),
            "a chunk that came inside the detail radius was not asked for in full"
        );
        streamer.delivered(far);
        assert_eq!(
            streamer.summary_level(far),
            None,
            "the client was left holding a summary of a chunk it now has in full"
        );

        // And back out again.
        let departed = streamer.recentre(ORIGIN);
        assert!(!departed.contains(&far), "walking away unloaded it");
        assert!(
            streamer
                .next_summaries(usize::MAX)
                .iter()
                .any(|(at, _)| *at == far),
            "a chunk that left the detail radius was not re-sent as a summary"
        );
    }

    #[test]
    fn a_player_pacing_across_a_ring_edge_is_sent_nothing() {
        // **Criterion T3, at the level that costs bandwidth rather than
        // frames.** The client's rebuild count is downstream of this: a
        // summary it is not sent is one it cannot rebuild.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        for pos in streamer.next_needed(usize::MAX) {
            streamer.delivered(pos);
        }
        for (pos, level) in streamer.next_summaries(usize::MAX) {
            streamer.summarised(pos, level);
        }

        // Step back and forth across the level-1/level-2 edge, which at a
        // detail radius of eight sits sixteen chunks out.
        let mut sends = 0;
        for step in 0..20 {
            let centre = if step % 2 == 0 {
                ChunkPos::new(0, 0, 0)
            } else {
                ChunkPos::new(1, 0, 0)
            };
            streamer.recentre(centre);
            for (pos, level) in streamer.next_summaries(usize::MAX) {
                // Chunks genuinely entering the horizon for the first time are
                // not churn — count only re-sends of what is already held.
                if streamer.summary_level(pos).is_some() {
                    sends += 1;
                }
                streamer.summarised(pos, level);
            }
        }
        assert_eq!(
            sends, 0,
            "pacing one chunk back and forth re-sent {sends} summaries the client \
             already held"
        );
    }

    #[test]
    fn an_edit_in_a_summarised_chunk_re_sends_the_summary() {
        // A block delta is no use to a client holding a summary: it has nowhere
        // to put one cell out of twenty-seven. The horizon is re-sent instead,
        // and this is what makes a distant explosion eventually show up.
        let mut streamer = Streamer::new(
            tiamot_core::domain::OVERWORLD,
            ORIGIN,
            ViewDistance::DEFAULT,
        );
        let far = ChunkPos::new(12, 0, 0);
        let (pos, level) = streamer
            .next_summaries(usize::MAX)
            .into_iter()
            .find(|(at, _)| *at == far)
            .expect("a summarised chunk");
        streamer.summarised(pos, level);
        assert!(
            !streamer
                .next_summaries(usize::MAX)
                .iter()
                .any(|(at, _)| *at == far)
        );

        streamer.resummarise(far);
        assert!(
            streamer
                .next_summaries(usize::MAX)
                .iter()
                .any(|(at, _)| *at == far),
            "an edited chunk's horizon was never sent again"
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
