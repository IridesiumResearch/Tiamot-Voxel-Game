// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! How long something takes to break, and how far along a player is.
//!
//! # Why the server counts, not the client
//!
//! Charter rule 2 makes the server authoritative, and digging is the clearest
//! case for it: a client that decided when a block broke could break every
//! block instantly. So the client says *start* and *stop*, the server counts
//! the ticks, and the client is told the progress so it can draw a crack
//! overlay. The crack is presentation; the counting is simulation.
//!
//! # Determinism
//!
//! Progress accumulates by repeated `f32` addition in a fixed order, one step
//! per tick, which charter rule 4 allows and makes reproducible. The tick count
//! for a given hardness and tool is therefore identical everywhere — which is
//! what lets a client predict the crack overlay's timing without the two ever
//! disagreeing about when the block actually went.
//!
//! # Where the hardness comes from
//!
//! This module counts ticks against a hardness; [`hardness`] works out what that
//! hardness is for a block made of several materials, and what one sub-node of
//! it costs. The two are separate because the counting has no opinion about
//! composition and the blend has none about time.

pub mod hardness;

pub use hardness::{Resistance, SUBNODE_SHARE, block_hardness, subnode_hardness};

use crate::block::SUBNODES_PER_BLOCK;
use crate::coords::SubNodePos;

/// How a tool removes material.
///
/// The shape a mod chooses when it registers a tool, and the reason
/// `game.register_tool` takes a table rather than a string: `"block"` and
/// `"subnode"` are the two the engine implements, and a mod that wants a 3×3
/// column later should be able to say so without the API changing shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Brush {
    /// Removes the whole block containing the targeted cell.
    #[default]
    Block,
    /// Removes only the cell under the crosshair.
    ///
    /// The mechanism the whole sub-node design exists for. `core:chisel` in the
    /// reference mods is the proof that a mod can reach it.
    SubNode,
}

impl Brush {
    /// Parses the wire/script spelling.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "block" => Some(Self::Block),
            "subnode" => Some(Self::SubNode),
            _ => None,
        }
    }

    /// The spelling a mod writes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::SubNode => "subnode",
        }
    }
}

/// Ticks per second, as an `f32`. Mirrors [`crate::tick::TICK_RATE_HZ`].
const TICKS_PER_SECOND: f32 = 20.0;

/// How many ticks a block takes to break at a given hardness and tool speed.
///
/// **Integer ticks, not accumulated fractions.** The obvious implementation
/// adds `1.0 / ticks` to a running float each tick and stops at 1.0, and it is
/// wrong in a way that only shows up for some inputs: 1/40 is not representable
/// in `f32`, so forty additions land just short and a two-second block takes
/// forty-one ticks. Charter rule 4 warns about exactly this — float
/// accumulation is order- and precision-dependent — and the fix is not to
/// accumulate at all. A tick count is an integer; the fraction a crack overlay
/// wants is derived from it.
///
/// A hardness of zero, or anything a mod managed to make non-positive or
/// non-finite, takes one tick rather than dividing by zero.
#[must_use]
pub fn ticks_to_break(hardness: f32, speed: f32) -> u32 {
    let exact = hardness * TICKS_PER_SECOND / speed;
    if !exact.is_finite() || exact <= 1.0 {
        return 1;
    }
    // Ceiling, without `f32::ceil` — it lowers to a libm call without SSE4.1
    // (charter rule 4, float-determinism.md §5).
    let whole = crate::detgen::floor_to_i32(exact);
    let ticks = if (whole as f32) < exact {
        whole + 1
    } else {
        whole
    };
    ticks.max(1) as u32
}

/// A dig in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dig {
    /// The cell being dug.
    target: SubNodePos,
    /// What shape will be removed when it completes.
    brush: Brush,
    /// Ticks spent so far.
    elapsed: u32,
    /// Sub-nodes already taken off this target.
    ///
    /// **A block comes apart rather than vanishing.** See [`Dig::advance`].
    chipped: u32,
    /// How many sub-nodes this target had when the dig started.
    ///
    /// **Captured once, not read every tick.** The caller counts what is still
    /// there, and what is still there SHRINKS as the dig eats it — so measuring
    /// against it would mean the dig thought it was finished halfway through,
    /// with `chipped` overtaking a falling count. The plan is fixed when the
    /// dig starts; `0` until the first advance sees the block.
    total: u32,
    /// Ticks the current target needs, from the last [`Dig::advance`].
    ///
    /// Recomputed every tick rather than fixed at the start, so swapping tools
    /// mid-dig takes effect immediately instead of finishing at the old speed.
    needed: u32,
}

impl Dig {
    /// Starts a fresh dig on a cell.
    #[must_use]
    pub const fn start(target: SubNodePos, brush: Brush) -> Self {
        Self {
            target,
            brush,
            elapsed: 0,
            chipped: 0,
            total: 0,
            needed: 1,
        }
    }

    /// The cell being dug.
    #[must_use]
    pub const fn target(&self) -> SubNodePos {
        self.target
    }

    /// What will be removed on completion.
    #[must_use]
    pub const fn brush(&self) -> Brush {
        self.brush
    }

    /// How far along, `0.0..=1.0`.
    ///
    /// Derived from the tick counts rather than stored, so nothing accumulates.
    /// Still sent to clients, and a mod's HUD may still draw it — but it is no
    /// longer what a player reads a dig from, because the block itself is
    /// visibly coming apart.
    #[must_use]
    pub fn progress(&self) -> f32 {
        if self.elapsed >= self.needed {
            return 1.0;
        }
        self.elapsed as f32 / self.needed as f32
    }

    /// How many sub-nodes have already come off this target.
    #[must_use]
    pub const fn chipped(&self) -> u32 {
        self.chipped
    }

    /// Ticks spent on this target.
    #[must_use]
    pub const fn elapsed(&self) -> u32 {
        self.elapsed
    }

    /// Advances one tick, returning how many sub-nodes came off.
    ///
    /// # A block comes apart; it does not pop
    ///
    /// A dig used to be a timer with a bar over it: nothing happened for a
    /// second and a half, and then a whole block vanished at once. What a
    /// player got for stopping halfway was nothing at all.
    ///
    /// Now the same total time is divided by the number of sub-nodes in the
    /// target, and one comes off at each step. **Stopping halfway leaves half a
    /// block standing and half a block's material in your inventory**, because
    /// each sub-node is removed and credited as it goes rather than at the end.
    /// There is nothing to bank and nothing to lose.
    ///
    /// `cells` is how many sub-nodes are still there — the caller counts them,
    /// because only it can see the block. A `SubNode` brush passes 1 and gets
    /// exactly the old behaviour: one chip, at the end.
    ///
    /// The total time is unchanged, so a block takes as long to clear as it
    /// always did. What changed is that the time is now spent visibly.
    ///
    /// Returns `0` on a tick where nothing is due, and never more than `cells`
    /// in total across a dig: a caller that keeps advancing a finished dig gets
    /// `0`, because the material is already paid out.
    pub fn advance(&mut self, hardness: f32, speed: f32, cells: u32) -> u32 {
        self.needed = ticks_to_break(hardness, speed);
        if cells == 0 {
            return 0;
        }
        // The plan, fixed the first time this target is seen. `cells` is what
        // is LEFT and falls as the dig eats it; measuring against that would
        // finish the dig halfway through.
        if self.total == 0 {
            self.total = cells;
        }
        let cells = self.total;
        if self.chipped >= cells {
            return 0;
        }
        if self.elapsed < self.needed {
            self.elapsed += 1;
        }

        // How many should have come off by now, from the share of the time
        // spent. Integer arithmetic on ticks rather than a float accumulator:
        // there is nothing to drift, and the last chip lands exactly when the
        // timer does.
        let due = if self.elapsed >= self.needed {
            cells
        } else {
            (u64::from(self.elapsed) * u64::from(cells) / u64::from(self.needed)) as u32
        };
        let chips = due.saturating_sub(self.chipped).min(cells - self.chipped);
        self.chipped += chips;
        chips
    }

    /// Whether every sub-node of the target has come off.
    ///
    /// Against the count captured at the start, not whatever is left now — see
    /// [`Dig::advance`]. A dig that has not started yet is not done.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.total > 0 && self.chipped >= self.total
    }

    /// Points the dig at a different cell, discarding progress.
    ///
    /// Returns whether anything was discarded, which is worth telling the
    /// client: the crack overlay has to reset.
    ///
    /// **Progress does not bank.** Chipping at one block, switching to another
    /// and coming back must start over, or a player could keep several blocks
    /// nearly broken and finish them all at once — and the crack a player sees
    /// would stop meaning what it says.
    pub fn retarget(&mut self, target: SubNodePos, brush: Brush) -> bool {
        if self.target == target && self.brush == brush {
            return false;
        }
        let had_progress = self.elapsed > 0;
        self.target = target;
        self.brush = brush;
        self.elapsed = 0;
        // The chips do NOT come back — they are already out of the world and in
        // somebody's inventory. What resets is the clock, and the plan, because
        // the new target is a different block with a different amount in it.
        self.chipped = 0;
        self.total = 0;
        had_progress
    }
}

/// Which sub-node of a block comes off next, and in what order.
///
/// # Why the order is random, and why it is not
///
/// A block that came apart in index order would peel in flat layers, which
/// reads as a bug rather than as breaking. A random order reads as material
/// giving way.
///
/// But it must be the SAME random order for everybody. Two players watching one
/// block come apart see the same shape at the same moment, a rejoining player
/// sees what the world already looks like, and the world file does not depend
/// on who was standing there — so this is a seeded stream keyed by the block's
/// own position (charter rule 4's rule for randomness), not `rand`.
///
/// Returns the cell indices in the order they should be taken. Every index
/// appears exactly once, so a caller that walks it and skips the empty ones
/// visits every occupied cell.
#[must_use]
pub fn crumble_order(world_seed: u64, block: crate::coords::BlockPos) -> [u8; SUBNODES_PER_BLOCK] {
    let mut order = [0u8; SUBNODES_PER_BLOCK];
    for (index, slot) in order.iter_mut().enumerate() {
        *slot = u8::try_from(index).unwrap_or(0);
    }

    // Keyed by the BLOCK, through the chunk-stream constructor: two blocks in
    // one chunk must not crumble identically, and the same block must crumble
    // the same way every time it is dug.
    let mut rng = crate::detgen::StreamRng::new(
        world_seed,
        crate::coords::ChunkPos::new(block.x, block.y, block.z),
        "dig:crumble",
    );
    // Fisher-Yates, downward, which is the version with no modulo bias when the
    // bound comes from an unbiased `below`.
    for index in (1..order.len()).rev() {
        let swap = rng.below(index as u64 + 1) as usize;
        order.swap(index, swap);
    }
    order
}

/// Where `game.get_tool` and `game.set_tool` reach.
///
/// The same seam shape as [`crate::storage::Access`] and
/// [`crate::ent::Access`], and for the same reason: which tool a player is
/// holding lives with the connected bodies, above `core`, and the VM lives
/// inside it (charter rule 3).
///
/// # Why a mod may set this at all
///
/// A tool decides what a dig REMOVES and what a placement WRITES, so it is the
/// one piece of a player's state a mod needs in order to build a control that
/// changes how digging behaves — the reference `core_tools:chisel_mode` is
/// exactly that. Reading it back matters just as much: a mod that swaps the
/// tool for the duration of a held key has to put back what was there, and
/// guessing "a bare hand" would take a tool off any player who was already
/// holding one.
pub trait Tools: Send + Sync {
    /// The tool a player is holding, or `None` for a bare hand.
    fn tool(&self, player: [u8; 32]) -> Option<String>;

    /// Puts a tool in a player's hand. `None` is a bare hand.
    ///
    /// Returns whether it took: a tool no mod registered, or a player who is
    /// not connected, is refused rather than stored, because a tool id that
    /// never resolves would be a dig that silently never progresses.
    fn set_tool(&self, player: [u8; 32], tool: Option<&str>) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: SubNodePos = SubNodePos::new(1, 2, 3);
    const OTHER: SubNodePos = SubNodePos::new(4, 5, 6);

    #[test]
    fn a_one_second_block_takes_a_second_of_ticks() {
        // The number a mod author is predicting when they write `hardness`.
        assert_eq!(ticks_to_break(1.0, 1.0), 20);
        assert_eq!(ticks_to_break(2.0, 1.0), 40);
    }

    #[test]
    fn a_faster_tool_takes_proportionally_fewer_ticks() {
        let bare = ticks_to_break(2.0, 1.0);
        let quick = ticks_to_break(2.0, 2.0);
        assert_eq!(quick * 2, bare, "twice the speed should halve the ticks");

        // And a SLOWER tool takes more, which is what the chisel is: precision
        // costs time, so `speed_multiplier` below 1 has to work too.
        let slow = ticks_to_break(2.0, 0.5);
        assert_eq!(slow, bare * 2);
    }

    #[test]
    fn a_hardness_of_zero_breaks_in_one_tick_rather_than_dividing_by_zero() {
        assert_eq!(ticks_to_break(0.0, 1.0), 1);
        let mut dig = Dig::start(CELL, Brush::Block);
        assert_eq!(
            dig.advance(0.0, 1.0, 1),
            1,
            "a one-tick break should take its only cell immediately"
        );
    }

    #[test]
    fn every_sub_node_is_paid_for_exactly_once() {
        // Advancing a finished dig again must not chip anything more: the
        // material is already out of the world and in somebody's inventory, and
        // a second payout is duplication.
        let mut dig = Dig::start(CELL, Brush::Block);
        let mut chips = 0;
        for _ in 0..200 {
            chips += dig.advance(1.0, 1.0, 27);
        }
        assert_eq!(chips, 27, "the block yielded {chips} sub-nodes, not 27");
        assert!(dig.is_done());
        assert!((dig.progress() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_block_comes_apart_over_the_whole_dig_rather_than_at_the_end() {
        // **The change, stated as a property.** Half the time spent must have
        // taken roughly half the block — not none of it, which is what a timer
        // with a bar over it did.
        let mut dig = Dig::start(CELL, Brush::Block);
        let total = ticks_to_break(2.0, 1.0);
        let mut chips = 0;
        for _ in 0..total / 2 {
            chips += dig.advance(2.0, 1.0, 27);
        }
        assert!(
            (11..=16).contains(&chips),
            "half a dig took {chips} of 27 sub-nodes, which is not half a block"
        );

        // And what it took is KEPT. Walking away is not losing it.
        assert_eq!(dig.chipped(), chips);
    }

    #[test]
    fn a_sub_node_brush_still_takes_one_thing_at_the_end() {
        // The chisel is unchanged: one cell, and nothing until the timer is up.
        let mut dig = Dig::start(CELL, Brush::SubNode);
        let total = ticks_to_break(1.0, 1.0);
        for tick in 1..total {
            assert_eq!(
                dig.advance(1.0, 1.0, 1),
                0,
                "the chisel gave something up on tick {tick}"
            );
        }
        assert_eq!(dig.advance(1.0, 1.0, 1), 1);
        assert!(dig.is_done());
    }

    #[test]
    fn a_two_second_block_takes_exactly_forty_ticks() {
        // The case that caught the original implementation. It accumulated
        // `1.0 / 40.0` per tick, which is not representable in f32, so forty
        // additions fell short and the block took forty-one.
        assert_eq!(ticks_to_break(2.0, 1.0), 40);

        let mut dig = Dig::start(CELL, Brush::Block);
        let mut ticks = 0;
        while !dig.is_done() {
            dig.advance(2.0, 1.0, 27);
            ticks += 1;
            assert!(ticks < 100, "never finished");
        }
        assert_eq!(ticks, 40, "took {ticks} ticks");
    }

    #[test]
    fn progress_never_runs_past_one() {
        // The client draws the crack overlay from this, and an overlay index
        // off the end of the texture is a panic or a garbage frame.
        let mut dig = Dig::start(CELL, Brush::Block);
        for _ in 0..100 {
            dig.advance(0.1, 4.0, 27);
            assert!(
                (0.0..=1.0).contains(&dig.progress()),
                "progress left its range: {}",
                dig.progress()
            );
        }
    }

    #[test]
    fn switching_targets_discards_progress_rather_than_banking_it() {
        // Otherwise a player chips every block in reach to 99% and harvests
        // them all at once, and the crack they see stops meaning anything.
        let mut dig = Dig::start(CELL, Brush::Block);
        for _ in 0..10 {
            dig.advance(1.0, 1.0, 27);
        }
        assert!(dig.progress() > 0.4);

        assert!(dig.retarget(OTHER, Brush::Block), "progress was discarded");
        assert_eq!(dig.target(), OTHER);
        assert_eq!(dig.elapsed(), 0);

        // Coming back starts over.
        assert!(!dig.retarget(OTHER, Brush::Block), "same target is a no-op");
        dig.retarget(CELL, Brush::Block);
        assert_eq!(dig.elapsed(), 0);
    }

    #[test]
    fn changing_the_brush_on_the_same_cell_starts_over() {
        // Switching from a chisel to a bare hand mid-dig means removing a
        // different amount of material, so the work done does not carry.
        let mut dig = Dig::start(CELL, Brush::SubNode);
        dig.advance(1.0, 1.0, 1);
        assert!(dig.retarget(CELL, Brush::Block), "the brush changed");
        assert_eq!(dig.brush(), Brush::Block);
        assert_eq!(dig.elapsed(), 0);
    }

    #[test]
    fn a_block_crumbles_the_same_way_for_everybody_and_differently_from_its_neighbour() {
        use crate::coords::BlockPos;

        let here = crumble_order(7, BlockPos::new(4, 9, -2));

        // **The same, every time and for everyone.** Two players watching one
        // block come apart must see the same shape at the same moment, and a
        // player who rejoins must see what the world already looks like.
        assert_eq!(here, crumble_order(7, BlockPos::new(4, 9, -2)));

        // Different for the block next to it, or a wall peels in one motion.
        assert_ne!(here, crumble_order(7, BlockPos::new(5, 9, -2)));
        // And different in another world, like every other seeded stream.
        assert_ne!(here, crumble_order(8, BlockPos::new(4, 9, -2)));

        // Every cell exactly once, so a caller walking it reaches all of them.
        let mut seen = here;
        seen.sort_unstable();
        let expected: Vec<u8> = (0..SUBNODES_PER_BLOCK as u8).collect();
        assert_eq!(seen.as_slice(), expected.as_slice());

        // Not the identity, or the block peels in flat layers — which reads as
        // a bug rather than as breaking, and is the whole reason for shuffling.
        assert_ne!(here.as_slice(), expected.as_slice());
    }

    #[test]
    fn the_same_dig_takes_the_same_ticks_every_time() {
        // Underwrites the client predicting the crack overlay: both ends run
        // this arithmetic and must agree on the tick the block goes.
        for (hardness, speed) in [(1.0, 1.0), (2.5, 0.5), (0.75, 3.0), (10.0, 1.5)] {
            let first = ticks_to_break(hardness, speed);
            let second = ticks_to_break(hardness, speed);
            assert_eq!(first, second, "hardness {hardness} at speed {speed}");
        }
    }
}
