// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The update rule.
//!
//! # What it is
//!
//! Classic Minecraft flow, stated as four rules applied in a fixed order to one
//! block at a time:
//!
//! 1. a **source** sustains itself at [`MAX_LEVEL`] and feeds its neighbours;
//! 2. fluid over a block that accepts it **falls**, filling the column below at
//!    full level rather than thinning as it drops;
//! 3. otherwise it **spreads** sideways, losing a level per block, preferring
//!    directions that lead to a drop within [`Tuning::hole_search`];
//! 4. a block with no valid parent **drains** by one level per fluid tick.
//!
//! # Determinism
//!
//! Charter rule 4. The active set is a `BTreeSet`, so the order blocks are
//! visited in is their coordinate order and not the order they happened to be
//! inserted; neighbours are visited in a fixed face order; every arithmetic
//! operation here is on integers. Two servers given the same edits produce the
//! same milk, which the cross-platform hash gate checks.
//!
//! **A `HashSet` here would be a bug that CI catches on the third platform.**
//! Rust's default hasher is randomly seeded per process, so an active set built
//! on one would not even be stable between two runs on one machine.
//!
//! # Why blocks are queued rather than scanned
//!
//! A settled world costs nothing. Only blocks that an edit touched, or that a
//! flow is currently moving through, are in the active set; a pond that has
//! finished spreading has an empty one, and the whole system drops out of the
//! tick. That is the property the perf criterion asserts, and it is why the
//! solver is written around a work queue instead of a per-chunk sweep.

use std::collections::BTreeSet;

use crate::coords::BlockPos;

use super::{Fluid, FluidId, MAX_LEVEL};

/// The six faces, as offsets, in the order the solver visits them.
///
/// Down first, because falling beats spreading and checking it first lets the
/// common case return early. The four lateral directions follow in a fixed
/// order; up is never a flow direction — milk does not climb — and is absent.
const LATERAL: [[i32; 3]; 4] = [[-1, 0, 0], [1, 0, 0], [0, 0, -1], [0, 0, 1]];

/// The world, as the fluid solver needs to see it.
///
/// Deliberately the same shape as [`crate::light::propagate::Neighbourhood`] and
/// for the same reason: the server has a world database behind a cache and the
/// client has a map of streamed chunks, and one update rule has to run over
/// both or the client cannot predict what the server will do.
pub trait Neighbourhood {
    /// How full a block is, in cells of 27.
    ///
    /// **Sub-Node Contract §4.** The world reports a fact and the fluid decides
    /// what it means: a block is floor iff this is at or above the registering
    /// fluid's `waterlogs_at`. Two fluids in one world may disagree about what
    /// counts as floor, which is why the threshold lives with the fluid and this
    /// does not.
    ///
    /// `None` for anything not loaded — NOT zero, and the difference matters.
    /// Zero would let a flood run off the edge of the loaded world and a pond
    /// drain silently into a chunk that has not arrived.
    fn occupancy(&self, pos: BlockPos) -> Option<u32>;

    /// What a block holds now.
    fn fluid(&self, pos: BlockPos) -> Fluid;

    /// Records what a block holds.
    ///
    /// Positions outside what the implementation holds are dropped rather than
    /// being an error, exactly as light does: a flow reaching the edge of the
    /// loaded region has nowhere to write.
    fn set_fluid(&mut self, pos: BlockPos, value: Fluid);
}

/// The knobs a registered fluid brings to the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
    /// How far a source spreads on flat ground, in blocks.
    ///
    /// Capped at [`MAX_LEVEL`], because a level is what encodes the distance
    /// travelled and there are only seven of them.
    pub flow_range: u8,
    /// How far ahead the rule looks for a drop, in blocks.
    ///
    /// Four in Minecraft, and the reason milk finds a hole in the floor instead
    /// of spreading into a disc and reaching it by accident. Zero disables the
    /// preference, which makes a fluid spread evenly in all directions.
    pub hole_search: u8,
    /// How full a block has to be before this fluid treats it as floor, in
    /// cells of 27.
    ///
    /// **The number that makes fluid work on smoothed terrain.** Before it,
    /// §4 read "accepts fluid iff empty", which made a single chiselled cell
    /// waterproof — defensible on blocky terrain and wrong on the terrain this
    /// engine is for. A sub-node-smoothed hillside is a ramp INSIDE blocks, so
    /// every column's top block is `Partial`, nothing below any of them was a
    /// drop, and the solver saw a perfectly flat floor: milk spread as a disc
    /// across a hillside, floating above ground that was smooth beneath it.
    ///
    /// A mod that wants the old behaviour registers `waterlogs_at = 1`.
    pub waterlogs_at: u32,
    /// Fluid ticks between updates of this fluid.
    ///
    /// One is every fluid tick. Larger is slower and thicker — and it is the
    /// only knob that changes how fast a spring is SEEN to run, which is the
    /// first thing anybody watching a fluid has an opinion about.
    pub tick_rate: u8,
    /// How many neighbouring sources make a block a source in its own right.
    ///
    /// **Zero disables renewal**, which is the engine default and keeps the
    /// original model exactly as it was: sources are placed and only a player
    /// or a mod removes one. A fluid that wants an ocean opts in.
    ///
    /// Counted over the four LATERAL neighbours only, and they must all be the
    /// same fluid. Three means "sources on all but one side", which is stricter
    /// than the two Minecraft asks for, and deliberately: at two, any 2×2 pool
    /// is an infinite well, and the point of this rule is that **an ocean does
    /// not collapse when somebody takes a bucket out of the middle of it** —
    /// not that a bucket is a way to make more ocean.
    ///
    /// # Why this is a mod's decision and not the engine's
    ///
    /// It creates matter out of nothing, which is a game-design position rather
    /// than a mechanism. Charter rule 1: the engine makes it expressible and
    /// `game/core_milk` is where the opinion lives.
    pub renews_from: u8,
}

impl Tuning {
    /// What milk uses, and a sensible default for a fluid that behaves like it.
    pub const DEFAULT: Self = Self {
        flow_range: MAX_LEVEL,
        hole_search: 4,
        // Fourteen of 27 — over half. Under it the block is more air than
        // anything and fluid runs through; at or above it, it is more solid
        // than not and holds the fluid up.
        waterlogs_at: 14,
        tick_rate: 1,
        // Off. Creating matter is a mod's call — see the field.
        renews_from: 0,
    };
}

/// One block's worth of change, for whoever needs to hear about it.
///
/// The server broadcasts these and the client applies them; both also use them
/// to decide which chunks need re-meshing. Carrying the previous value as well
/// as the new one means a listener can tell "a puddle appeared" from "a puddle
/// got deeper" without holding its own copy of the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    /// Where it happened.
    pub pos: BlockPos,
    /// What was there.
    pub was: Fluid,
    /// What is there now.
    pub now: Fluid,
}

/// A flow that did not happen, and where it was stopped.
///
/// # What this is for
///
/// The interesting thing about a fluid is not only where it went but where it
/// *tried* to go. A mod that wants waterlogging — a block that changes when milk
/// reaches it — has no way to find out that milk is pressing against something
/// unless the engine says so: the fluid layer records where milk IS, and a block
/// milk cannot enter is by definition somewhere it is not.
///
/// Reported per lateral direction rather than per block, because "which side is
/// wet" is the question a mod is actually asking.
///
/// The blocking block's material is deliberately NOT here. This module knows
/// occupancy and nothing about materials — [`Neighbourhood`] is the whole of its
/// view of the world — and a `MaterialId` is only meaningful next to the
/// registry that issued it (charter rule 8). The server looks it up when it
/// hands the event to a mod, where the right registry is in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocked {
    /// The block holding the fluid.
    pub from: BlockPos,
    /// The block it could not get into.
    pub into: BlockPos,
    /// Which fluid was pressing.
    pub fluid: FluidId,
    /// What level it was pressing at, `1..=7`.
    pub level: u8,
}

/// The active set, and the rule that drains it.
///
/// Holds no world of its own — [`Neighbourhood`] is the world — so a server and
/// a client can each keep one over their own storage.
#[derive(Debug, Default, Clone)]
pub struct Solver {
    /// Blocks whose state might need to change, in coordinate order.
    active: BTreeSet<BlockPos>,
    /// Blocks left over from a tick that hit its cap.
    ///
    /// Carried rather than dropped: a spring field that overruns its budget
    /// finishes next tick instead of leaving milk half-spread forever.
    carried: BTreeSet<BlockPos>,
    /// Flows that could not happen, for the `on_fluid_flow` hook.
    ///
    /// Drained by the caller with [`Solver::take_blocked`] rather than returned
    /// from `tick`: it is an observation channel and not part of what the solver
    /// did, and a caller with no mods listening should be able to ignore it
    /// without the signature saying otherwise.
    ///
    /// **Capped at [`BLOCKED_PER_TICK`] and dropped rather than carried**, which
    /// is the opposite of what `carried` does and is deliberate. An unfinished
    /// flow must be finished or the world is wrong; an unreported block is a
    /// notification nobody got, and the shoreline it describes will still be
    /// there next time the pond is examined. Carrying them would let a mod that
    /// is slow to handle them grow an unbounded queue inside the tick.
    blocked: Vec<Blocked>,
}

/// How many blocked flows one tick will report.
///
/// A cap on the HOOK's cost, not on the solver's — the visit budget already
/// bounds that. Task 11 asks for `on_fluid_flow` to be budgeted, and this is
/// where: a mod's callback runs once per entry, so an ocean meeting a continent
/// must not be able to hand the script VM ten thousand events in one tick.
///
/// Sixty-four is roughly the perimeter of an eight-block pond, so an ordinary
/// pool reports its whole shoreline in a tick and only something enormous is
/// sampled rather than enumerated.
const BLOCKED_PER_TICK: usize = 64;

impl Solver {
    /// An empty solver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a block and everything that could be affected by it changing.
    ///
    /// Called for every edit — a block broken, a block placed, milk poured —
    /// and for the fluid's own neighbours when it moves. The six-neighbour halo
    /// is what makes a wall being knocked out wake the pond behind it.
    pub fn touch(&mut self, pos: BlockPos) {
        self.active.insert(pos);
        self.active.insert(BlockPos::new(pos.x, pos.y - 1, pos.z));
        self.active.insert(BlockPos::new(pos.x, pos.y + 1, pos.z));
        for offset in LATERAL {
            self.active.insert(BlockPos::new(
                pos.x + offset[0],
                pos.y + offset[1],
                pos.z + offset[2],
            ));
        }
    }

    /// How many blocks are waiting.
    ///
    /// Zero for a settled world, which is the assertion the perf criterion
    /// makes: milk that has finished moving costs nothing at all.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.len() + self.carried.len()
    }

    /// Whether there is nothing to do.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.active.is_empty() && self.carried.is_empty()
    }

    /// Takes the flows that could not happen since this was last called.
    ///
    /// Drained rather than returned from [`Solver::tick`] because it is an
    /// observation channel and not part of what the solver did: a caller with no
    /// mods listening never calls this, and the events cost nothing but the
    /// bounded `Vec` they were written into.
    pub fn take_blocked(&mut self) -> Vec<Blocked> {
        std::mem::take(&mut self.blocked)
    }

    /// Runs one fluid tick, visiting at most `budget` blocks.
    ///
    /// Returns every change made, in the order it was made, for broadcasting and
    /// re-meshing. Blocks not reached within the budget are carried to the next
    /// tick rather than dropped.
    ///
    /// # The budget is a cap on VISITS, not on changes
    ///
    /// A block that is examined and left alone still costs a lookup, and the
    /// pathological case — a hundred springs all settled but re-queued by an
    /// edit — is all examinations and no changes. Counting changes would let
    /// that case run unbounded, which is exactly the tick overrun the cap is
    /// there to prevent.
    pub fn tick(
        &mut self,
        world: &mut impl Neighbourhood,
        tuning: Tuning,
        budget: usize,
    ) -> Vec<Flow> {
        let mut changes = Vec::new();
        // Last tick's leftovers first, so a block cannot be starved forever by
        // a spring that keeps re-queueing its own neighbourhood.
        let mut pending = std::mem::take(&mut self.carried);
        pending.extend(std::mem::take(&mut self.active));

        let mut visited = 0;
        let mut woken = BTreeSet::new();
        for pos in pending {
            if visited >= budget {
                self.carried.insert(pos);
                continue;
            }
            visited += 1;
            // Where this block's milk is pressing against something that will
            // not take it. Recorded whether or not the block itself changed: a
            // settled pond against a wall changes nothing every tick and is
            // exactly the case a waterlogging mod cares about.
            if self.blocked.len() < BLOCKED_PER_TICK {
                record_blocked(world, tuning, pos, &mut self.blocked);
            }
            if let Some(change) = settle_one(world, tuning, pos) {
                // Whatever changed wakes its neighbours, including the block
                // above: milk drained from under a column is what lets the
                // column fall.
                for at in [change.pos, BlockPos::new(pos.x, pos.y + 1, pos.z)] {
                    woken.insert(at);
                }
                changes.push(change);
            }
        }
        for pos in woken {
            self.touch(pos);
        }
        changes
    }

    /// Whether the fluid in a block is on its way down rather than resting.
    ///
    /// A block whose floor accepts fluid is falling: its contents are leaving.
    fn drains_into(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos) -> bool {
        !is_floor(world, tuning, BlockPos::new(pos.x, pos.y - 1, pos.z))
    }

    /// Which lateral directions a block should feed, given the hole preference.
    ///
    /// **Contract §4's flow direction, and what makes milk find a hole.** With
    /// nothing to fall into, a block feeds every open neighbour equally and the
    /// puddle is a disc. With a drop within `hole_search` steps, it feeds only
    /// the directions whose shortest path to that drop is shortest — so a spring
    /// beside a pit pours into the pit instead of spreading around it.
    ///
    /// Returned as a fixed-length mask rather than a `Vec` so the common case
    /// allocates nothing.
    #[must_use]
    pub fn preferred(
        world: &impl Neighbourhood,
        tuning: Tuning,
        pos: BlockPos,
    ) -> [bool; LATERAL.len()] {
        let mut open = [false; LATERAL.len()];
        for (index, offset) in LATERAL.iter().enumerate() {
            let at = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
            open[index] = !is_floor(world, tuning, at);
        }
        if tuning.hole_search == 0 {
            return open;
        }

        // How far each open direction is from a drop, walking on blocks that
        // accept fluid. `None` for a direction with no drop in range.
        let mut distance = [None; LATERAL.len()];
        let mut shortest = u8::MAX;
        for (index, offset) in LATERAL.iter().enumerate() {
            if !open[index] {
                continue;
            }
            let start = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
            if let Some(steps) = drop_within(world, tuning, start) {
                distance[index] = Some(steps);
                shortest = shortest.min(steps);
            }
        }
        if shortest == u8::MAX {
            // No drop anywhere in range: spread evenly.
            return open;
        }
        let mut preferred = [false; LATERAL.len()];
        for index in 0..LATERAL.len() {
            preferred[index] = distance[index] == Some(shortest);
        }
        preferred
    }
}

/// Decides what one block should hold, and writes it if that differs.
/// Records the lateral directions this block's fluid is pressing into and
/// cannot enter.
///
/// **Only where the fluid would actually have gone.** A block at level 1 has
/// nothing left to give — the next block along would be level 0 — so it presses
/// against nothing and reports nothing, and a dry block obviously does not
/// either. Without that, every solid block adjacent to any milk anywhere would
/// generate an event every time the pond was examined.
///
/// The block BELOW is not considered. Fluid stopped by a floor is not blocked,
/// it is resting; that is the ordinary case and a mod hearing about it would
/// hear about every pond in the world having a bottom.
fn record_blocked(
    world: &impl Neighbourhood,
    tuning: Tuning,
    pos: BlockPos,
    out: &mut Vec<Blocked>,
) {
    let here = world.fluid(pos);
    if here.is_empty() {
        return;
    }
    // A source pushes at full level; a flow pushes at what it has.
    let level = if here.is_source() {
        MAX_LEVEL
    } else {
        here.level()
    };
    if level <= 1 {
        return;
    }

    for offset in &LATERAL {
        let into = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
        // Unloaded is not blocked. A flow reaching the edge of the loaded world
        // is a flow nobody can answer for yet, and reporting it would tell a mod
        // that a chunk which has not arrived is a wall.
        if world.occupancy(into).is_none() {
            continue;
        }
        if !is_floor(world, tuning, into) {
            continue;
        }
        // Already holding this fluid means it is not blocked, it is met.
        if !world.fluid(into).is_empty() {
            continue;
        }
        out.push(Blocked {
            from: pos,
            into,
            fluid: here.fluid(),
            level,
        });
    }
}

fn settle_one(world: &mut impl Neighbourhood, tuning: Tuning, pos: BlockPos) -> Option<Flow> {
    let was = world.fluid(pos);

    // A block that stopped accepting fluid — somebody built in the pond —
    // loses whatever was in it. Checked first, because everything below
    // assumes the block can hold something.
    if is_floor(world, tuning, pos) {
        if was.is_empty() {
            return None;
        }
        world.set_fluid(pos, Fluid::EMPTY);
        return Some(Flow {
            pos,
            was,
            now: Fluid::EMPTY,
        });
    }

    // A source is the one state the rule never revises. It is placed by a
    // player or a mod and only they can take it away.
    if was.is_source() {
        return None;
    }

    // **Renewal, before the ordinary supply rule**, because a block that is
    // about to become a source should not first be given a level by its
    // neighbours and then have it overwritten — that would be two flows for one
    // event, and every listener would see the block flicker.
    if let Some(renewed) = renewed_source(world, tuning, pos) {
        if renewed == was {
            return None;
        }
        world.set_fluid(pos, renewed);
        return Some(Flow {
            pos,
            was,
            now: renewed,
        });
    }

    let now = supplied(world, tuning, pos, was);
    if now == was {
        return None;
    }
    world.set_fluid(pos, now);
    Some(Flow { pos, was, now })
}

/// Whether enough neighbouring sources make this block a source too.
///
/// # What this is for
///
/// **An ocean must not collapse when somebody takes a bucket out of the middle
/// of it.** Without renewal, every source is a thing that exists exactly once:
/// scoop one from the middle of a lake and the hole fills with *flow*, which
/// has a level and a parent chain, and the lake is permanently one block of
/// source poorer. Do it a few hundred times along a shoreline and the whole
/// body of water is flow blocks hanging off a shrinking core.
///
/// [`Tuning::renews_from`] is how many of the four lateral neighbours have to be
/// sources. Zero — the engine default — means this never fires and the model is
/// exactly what it was.
///
/// # Same fluid, and laterally only
///
/// All the counted sources must be the same fluid, or two fluids meeting would
/// breed a third state that is neither. Lateral rather than including the block
/// above, because a source under a waterfall would otherwise renew from the
/// column falling into it and a single spring would turn every block beneath it
/// into a spring of its own.
fn renewed_source(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos) -> Option<Fluid> {
    if tuning.renews_from == 0 {
        return None;
    }

    let mut fluid = FluidId::NONE;
    let mut sources = 0u8;
    for offset in &LATERAL {
        let neighbour = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
        let there = world.fluid(neighbour);
        if !there.is_source() {
            continue;
        }
        if fluid.is_none() {
            fluid = there.fluid();
        } else if fluid != there.fluid() {
            // Two fluids, and neither of them gets to claim the block. Bailing
            // rather than counting the first one seen: the answer must not
            // depend on which direction the loop happened to start from.
            return None;
        }
        sources += 1;
    }

    if fluid.is_none() || sources < tuning.renews_from {
        return None;
    }
    Some(Fluid::source(fluid))
}

/// What this block should hold, given what is around it.
fn supplied(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos, was: Fluid) -> Fluid {
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    let falling = world.fluid(above);

    // **Rule 2, and it comes before spreading.** Anything above pours
    // straight through at full level rather than thinning on the way down,
    // which is what makes a waterfall a column instead of a cone.
    if !falling.is_empty() && !is_floor(world, tuning, pos) {
        return Fluid::flowing(falling.fluid(), MAX_LEVEL);
    }

    // **Rule 3.** The best lateral parent, which is the highest level among
    // neighbours that could feed this block. A neighbour at level n feeds
    // n-1 here; a source feeds MAX_LEVEL - 1.
    let mut best = Fluid::EMPTY;
    for (index, offset) in LATERAL.iter().enumerate() {
        let neighbour = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
        let there = world.fluid(neighbour);
        if there.is_empty() {
            continue;
        }
        // A neighbour that is itself falling does not feed sideways: it is
        // on its way down and its level belongs to the column, not to the
        // floor beside it.
        if is_falling(world, tuning, neighbour) {
            continue;
        }
        // **And it only feeds the way it is running.** Contract §4's flow
        // direction: a block with a drop within `hole_search` feeds only the
        // directions on a shortest path to it, so a spring beside a pit pours
        // INTO the pit instead of spreading round it and arriving by accident.
        //
        // Asked from the neighbour's side and inverted, because this is a pull:
        // what matters is the direction from the NEIGHBOUR to here, which is the
        // opposite of the one we walked to reach it.
        //
        // This call is the whole of the hole-seeking behaviour, and its absence
        // was the whole of the bug: `preferred` existed, was table-driven
        // tested in four directions, passed, and was never once consulted by the
        // solver. A tested function is not a tested behaviour.
        if !feeds(world, tuning, neighbour, opposite_lateral(index)) {
            continue;
        }
        let reach = tuning.flow_range.min(MAX_LEVEL);
        let carried = there.level().saturating_sub(MAX_LEVEL - reach + 1);
        if carried > best.level() {
            best = Fluid::flowing(there.fluid(), carried);
        }
    }

    if best.is_empty() {
        // **Rule 4.** No parent, so drain by one. One level per tick rather
        // than vanishing, so a channel visibly empties.
        if was.is_empty() {
            return Fluid::EMPTY;
        }
        return Fluid::flowing(was.fluid(), was.level().saturating_sub(1));
    }

    // Fed. A block that is over a hole takes what it is given and lets the
    // fall rule move it on; the hole preference below is about which
    // neighbours a block feeds, not about what it holds.
    best
}

/// The index in [`LATERAL`] pointing the other way.
///
/// The array is ordered `-x, +x, -z, +z`, so flipping the low bit turns a
/// direction into its opposite.
const fn opposite_lateral(index: usize) -> usize {
    index ^ 1
}

/// Whether the block at `pos` would feed the neighbour across `direction`.
///
/// # The early-out is the whole cost story
///
/// `preferred` walks outward from each of four directions looking for a drop,
/// so asking it per contributing neighbour is up to sixteen bounded searches per
/// block visited. On flat ground every one of them finds nothing and the answer
/// is "feeds everywhere" — so ask that question ONCE, from the block itself,
/// and return early.
///
/// Measured: wiring the preference in without this took the fluid unit tests
/// from 0.11 s to 3.24 s. With it they are back under a fifth of a second, and
/// the expensive path runs only where there is actually a hole to run into,
/// which is where the behaviour is worth paying for.
fn feeds(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos, direction: usize) -> bool {
    if tuning.hole_search == 0 {
        return true;
    }
    if drop_within(world, tuning, pos).is_none() {
        return true;
    }
    Solver::preferred(world, tuning, pos)[direction]
}

/// Whether a block is floor for this fluid — Contract §4's threshold.
///
/// Unloaded counts as floor, which is what stops a flood running off the edge
/// of the world.
fn is_floor(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos) -> bool {
    world
        .occupancy(pos)
        .is_none_or(|filled| filled >= tuning.waterlogs_at)
}

/// Whether the fluid at `pos` is falling rather than resting.
fn is_falling(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos) -> bool {
    !world.fluid(pos).is_empty() && Solver::drains_into(world, tuning, pos)
}

/// Steps from `start` to the nearest block that would drop, up to `limit`.
///
/// A breadth-first walk over blocks that accept fluid, which is bounded by
/// `limit` and therefore by a few dozen visits — Minecraft's four gives at most
/// forty-one blocks. Deliberately not a full path search: the preference exists
/// to make milk look like it knows where the hole is, not to be a router.
fn drop_within(world: &impl Neighbourhood, tuning: Tuning, start: BlockPos) -> Option<u8> {
    let limit = tuning.hole_search;
    if is_floor(world, tuning, start) {
        return None;
    }
    let mut seen = BTreeSet::from([start]);
    let mut frontier = vec![start];
    for steps in 0..=limit {
        if frontier.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for pos in frontier {
            if !is_floor(world, tuning, BlockPos::new(pos.x, pos.y - 1, pos.z)) {
                return Some(steps);
            }
            if steps == limit {
                continue;
            }
            for offset in LATERAL {
                let at = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
                if !is_floor(world, tuning, at) && seen.insert(at) {
                    next.push(at);
                }
            }
        }
        frontier = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::FluidId;
    use super::*;

    const MILK: FluidId = FluidId(1);

    /// A world of explicit solid blocks and fluid, with everything else air.
    ///
    /// Deliberately unbounded: `accepts_fluid` is true anywhere nothing solid
    /// was placed, so a test has to build its own floor. That is the honest
    /// shape — a fluid that spills off the edge of a test fixture is a fluid
    /// that would spill off the edge of the world, and hiding it behind an
    /// implicit wall would hide the bug too.
    #[derive(Debug, Default)]
    struct Scene {
        solid: std::collections::BTreeSet<(i32, i32, i32)>,
        /// Blocks that are partly filled, in cells of 27.
        ///
        /// What a sub-node-smoothed hillside is made of, and the case the
        /// whole-block `solid` set above cannot express.
        partial: BTreeMap<(i32, i32, i32), u32>,
        fluid: BTreeMap<(i32, i32, i32), Fluid>,
    }

    impl Scene {
        fn key(pos: BlockPos) -> (i32, i32, i32) {
            (pos.x, pos.y, pos.z)
        }

        /// A solid floor at y = 0 spanning `-span..=span` on both horizontals.
        fn with_floor(mut self, span: i32) -> Self {
            for x in -span..=span {
                for z in -span..=span {
                    self.solid.insert((x, 0, z));
                }
            }
            self
        }

        fn wall(mut self, x: i32, y: i32, z: i32) -> Self {
            self.solid.insert((x, y, z));
            self
        }

        fn at(&self, x: i32, y: i32, z: i32) -> Fluid {
            self.fluid.get(&(x, y, z)).copied().unwrap_or(Fluid::EMPTY)
        }

        fn level(&self, x: i32, y: i32, z: i32) -> u8 {
            self.at(x, y, z).level()
        }

        /// Runs until nothing changes, or gives up. Returns the ticks taken.
        fn settle(&mut self, solver: &mut Solver) -> usize {
            self.settle_with(solver, Tuning::DEFAULT)
        }

        /// The same, under a fluid that is tuned differently.
        fn settle_with(&mut self, solver: &mut Solver, tuning: Tuning) -> usize {
            for tick in 0..200 {
                if solver.is_settled() {
                    return tick;
                }
                solver.tick(self, tuning, usize::MAX);
            }
            panic!("never settled: {} blocks still active", solver.active());
        }
    }

    #[test]
    fn milk_pressing_against_a_wall_is_reported_and_a_pond_bottom_is_not() {
        // What `on_fluid_flow` is built on. A mod cannot see a flow that did
        // not happen any other way — a block milk cannot enter is a block with
        // no milk in it, indistinguishable from one milk never reached.
        let mut scene = Scene::default().with_floor(4).wall(2, 1, 0);
        scene.fluid.insert((0, 1, 0), Fluid::source(MILK));
        let mut solver = Solver::default();
        solver.touch(BlockPos::new(0, 1, 0));
        scene.settle(&mut solver);

        let blocked = solver.take_blocked();
        assert!(
            blocked
                .iter()
                .any(|event| event.into == BlockPos::new(2, 1, 0)),
            "milk spread up to the wall at x=2 and never reported being stopped \
             by it: {blocked:?}"
        );

        // **Lateral only, so a pond never reports having a bottom.** Fluid
        // stopped by the ground it is sitting on is not blocked, it is resting,
        // and a mod hearing about that would hear about every pond in the world.
        //
        // Stated as "every report is sideways" rather than "nothing at y=0",
        // because milk that spills off the edge of this fixture's floor lands at
        // y=0 and presses into the floor blocks from the side — which is a real
        // blocked flow and correctly reported.
        assert!(
            blocked.iter().all(|event| event.into.y == event.from.y),
            "a vertical block was reported: {blocked:?}"
        );

        // Every report names the fluid doing the pressing, or a mod with two
        // fluids in its world cannot tell which one reached the wall.
        assert!(blocked.iter().all(|event| event.fluid == MILK));
    }

    #[test]
    fn a_trickle_with_nothing_left_to_give_reports_nothing() {
        // Level 1 has nowhere to go: the next block along would be level 0. A
        // block pressing against nothing must not report pressing against
        // something, or every solid block next to the far edge of every puddle
        // generates an event forever.
        let mut scene = Scene::default().with_floor(4).wall(1, 1, 0);
        scene.fluid.insert((0, 1, 0), Fluid::flowing(MILK, 1));
        let mut solver = Solver::default();
        solver.touch(BlockPos::new(0, 1, 0));
        solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);

        assert!(
            solver.take_blocked().is_empty(),
            "a level-1 trickle reported a blocked flow it never had to make"
        );
    }

    #[test]
    fn the_blocked_report_is_capped_rather_than_growing_without_bound() {
        // Task 11 asks for the hook to be budgeted. The solver's visit budget
        // bounds its own work; this bounds what it hands the script VM, which
        // is a separate cost — a mod's callback runs once per entry.
        //
        // A long wall with a source against every block of it.
        let mut scene = Scene::default().with_floor(64);
        for z in -60..=60 {
            scene.solid.insert((1, 1, z));
            scene.fluid.insert((0, 1, z), Fluid::source(MILK));
        }
        let mut solver = Solver::default();
        for z in -60..=60 {
            solver.touch(BlockPos::new(0, 1, z));
        }
        solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);

        let blocked = solver.take_blocked();
        assert!(
            blocked.len() <= BLOCKED_PER_TICK,
            "{} blocked flows reported in one tick, over the cap of {}",
            blocked.len(),
            BLOCKED_PER_TICK
        );
        assert!(
            !blocked.is_empty(),
            "a hundred and twenty sources against a wall reported nothing at all"
        );

        // And taking them empties the list, so the next tick starts fresh
        // rather than re-reporting the same shoreline.
        assert!(solver.take_blocked().is_empty());
    }

    /// Milk that renews from three sides — an ocean rather than a spring.
    const RENEWING: Tuning = Tuning {
        renews_from: 3,
        ..Tuning::DEFAULT
    };

    impl Neighbourhood for Scene {
        fn occupancy(&self, pos: BlockPos) -> Option<u32> {
            // **Walled, and it took two tests failing to learn why.** An
            // unbounded fixture is a drain to infinity: milk spilling off the
            // edge of a test's floor falls forever and the scene never settles,
            // so the failure reads as a solver bug rather than as a world with
            // no bottom.
            if pos.x.abs() > 40 || pos.z.abs() > 40 || pos.y < -8 || pos.y > 40 {
                return Some(crate::UNITS_PER_BLOCK);
            }
            if self.solid.contains(&Self::key(pos)) {
                return Some(crate::UNITS_PER_BLOCK);
            }
            Some(self.partial.get(&Self::key(pos)).copied().unwrap_or(0))
        }

        fn fluid(&self, pos: BlockPos) -> Fluid {
            self.at(pos.x, pos.y, pos.z)
        }

        fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
            if value.is_empty() {
                self.fluid.remove(&Self::key(pos));
            } else {
                self.fluid.insert(Self::key(pos), value);
            }
        }
    }

    fn spring(scene: &mut Scene, solver: &mut Solver, x: i32, y: i32, z: i32) {
        scene.set_fluid(BlockPos::new(x, y, z), Fluid::source(MILK));
        solver.touch(BlockPos::new(x, y, z));
    }

    /// Fills a rectangle of source blocks at one height.
    fn pool(
        scene: &mut Scene,
        solver: &mut Solver,
        x: std::ops::Range<i32>,
        z: std::ops::Range<i32>,
        y: i32,
    ) {
        for zi in z {
            for xi in x.clone() {
                spring(scene, solver, xi, y, zi);
            }
        }
    }

    #[test]
    fn a_bucket_taken_from_the_middle_of_an_ocean_fills_back_in() {
        // **The whole point of renewal.** Without it a source is a thing that
        // exists exactly once: scoop one out of a lake and the hole fills with
        // FLOW, which has a level and a parent chain, and the lake is
        // permanently one block of source poorer. Do that a few hundred times
        // along a shoreline and the water is flow hanging off a shrinking core.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        pool(&mut scene, &mut solver, 0..5, 0..5, 1);
        scene.settle_with(&mut solver, RENEWING);

        // Take a bucket from the middle.
        scene.set_fluid(BlockPos::new(2, 1, 2), Fluid::EMPTY);
        solver.touch(BlockPos::new(2, 1, 2));
        scene.settle_with(&mut solver, RENEWING);

        assert!(
            scene.at(2, 1, 2).is_source(),
            "the hole came back as {:?} rather than a source",
            scene.at(2, 1, 2)
        );
    }

    #[test]
    fn two_sources_are_not_enough_and_three_are() {
        // The threshold, either side of it. Two would make every 2x2 pool an
        // infinite well; three is "sources on all but one side", which is what
        // was asked for and is stricter than Minecraft's rule on purpose.
        //
        // A cross with the centre missing: the centre has four lateral sources.
        // Removing one arm leaves three, removing two leaves two.
        let arms = [(-1, 0), (1, 0), (0, -1), (0, 1)];
        for present in [4usize, 3, 2] {
            let mut scene = Scene::default().with_floor(16);
            let mut solver = Solver::new();
            for (dx, dz) in arms.iter().take(present) {
                spring(&mut scene, &mut solver, *dx, 1, *dz);
            }
            solver.touch(BlockPos::new(0, 1, 0));
            scene.settle_with(&mut solver, RENEWING);

            let centre = scene.at(0, 1, 0);
            if present >= 3 {
                assert!(
                    centre.is_source(),
                    "{present} neighbouring sources should renew, got {centre:?}"
                );
            } else {
                assert!(
                    !centre.is_source(),
                    "{present} neighbouring sources must NOT renew, got {centre:?} — at two, \
                     every 2x2 pool is an infinite well"
                );
            }
        }
    }

    #[test]
    fn a_fluid_that_does_not_renew_behaves_exactly_as_it_did() {
        // The engine default is off, and off has to mean untouched — the
        // determinism goldens were hashed by the rule as it was.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        pool(&mut scene, &mut solver, 0..5, 0..5, 1);
        scene.settle(&mut solver);

        scene.set_fluid(BlockPos::new(2, 1, 2), Fluid::EMPTY);
        solver.touch(BlockPos::new(2, 1, 2));
        scene.settle(&mut solver);

        assert!(
            !scene.at(2, 1, 2).is_source(),
            "renewal fired for a fluid whose renews_from is zero"
        );
        assert!(
            !scene.at(2, 1, 2).is_empty(),
            "the staging is wrong: the hole should still have filled with FLOW"
        );
    }

    #[test]
    fn renewal_does_not_run_a_spring_down_a_waterfall() {
        // Lateral only. Counting the block above would make every block under a
        // falling column a source of its own, and one spring would become an
        // infinite vertical one.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        // A source high up, with sources beside it at the same height only.
        pool(&mut scene, &mut solver, -1..2, -1..2, 8);
        scene.settle_with(&mut solver, RENEWING);

        for y in 1..8 {
            assert!(
                !scene.at(0, y, 0).is_source(),
                "the falling column made a source at y={y}"
            );
        }
    }

    #[test]
    fn two_fluids_meeting_do_not_breed_a_third() {
        // Neither gets to claim the block, and — the part that matters for
        // charter rule 4 — the answer must not depend on which direction the
        // neighbour loop happened to start from.
        const CREAM: FluidId = FluidId(2);
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();

        scene.set_fluid(BlockPos::new(-1, 1, 0), Fluid::source(MILK));
        scene.set_fluid(BlockPos::new(1, 1, 0), Fluid::source(MILK));
        scene.set_fluid(BlockPos::new(0, 1, -1), Fluid::source(CREAM));
        scene.set_fluid(BlockPos::new(0, 1, 1), Fluid::source(CREAM));
        solver.touch(BlockPos::new(0, 1, 0));
        scene.settle_with(&mut solver, RENEWING);

        assert!(
            !scene.at(0, 1, 0).is_source(),
            "four sources of two different fluids renewed into {:?}",
            scene.at(0, 1, 0)
        );
    }

    #[test]
    fn a_source_spreads_exactly_its_flow_range() {
        // **The known-answer the criteria name.** Level 7 at the source, one
        // less per block travelled, nothing at all past the range.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        scene.settle(&mut solver);

        assert_eq!(scene.level(0, 1, 0), MAX_LEVEL, "the source drained");
        for distance in 1..=i32::from(MAX_LEVEL) {
            let expected = MAX_LEVEL - distance as u8;
            assert_eq!(
                scene.level(distance, 1, 0),
                expected,
                "at {distance} blocks the level should be {expected}"
            );
        }
        assert_eq!(
            scene.level(i32::from(MAX_LEVEL) + 1, 1, 0),
            0,
            "milk reached one block past its flow range"
        );
    }

    #[test]
    fn milk_falls_at_full_level_rather_than_thinning_on_the_way_down() {
        // A waterfall is a column, not a cone. The block below a fall is fed at
        // MAX_LEVEL however far it has dropped.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 8, 0);
        scene.settle(&mut solver);

        for y in 1..8 {
            assert_eq!(
                scene.level(0, y, 0),
                MAX_LEVEL,
                "the column thinned at y {y}"
            );
        }
    }

    #[test]
    fn removing_a_source_drains_everything_and_empties_the_active_set() {
        // **The criterion, in full**: no orphan flow blocks, and the active set
        // returns to empty so a drained world costs nothing again.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        scene.settle(&mut solver);
        assert!(scene.fluid.len() > 1, "nothing spread, so nothing to drain");

        scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::EMPTY);
        solver.touch(BlockPos::new(0, 1, 0));
        scene.settle(&mut solver);

        assert!(
            scene.fluid.is_empty(),
            "orphan milk left behind: {:?}",
            scene.fluid
        );
        assert!(
            solver.is_settled(),
            "{} blocks still active",
            solver.active()
        );
    }

    #[test]
    fn a_settled_pond_costs_nothing() {
        // The perf criterion's assertion, at the solver level.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        scene.settle(&mut solver);

        assert!(solver.is_settled());
        assert_eq!(solver.active(), 0);
        assert!(
            solver
                .tick(&mut scene, Tuning::DEFAULT, usize::MAX)
                .is_empty(),
            "a settled world still produced changes"
        );
    }

    #[test]
    fn the_budget_carries_rather_than_dropping_work() {
        // A cap that lost blocks would leave milk half-spread forever, which is
        // worse than a slow tick.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);

        // One block a tick: enough to make progress, nowhere near enough to
        // finish in one pass.
        let mut ticks = 0;
        while !solver.is_settled() {
            solver.tick(&mut scene, Tuning::DEFAULT, 1);
            ticks += 1;
            assert!(ticks < 5000, "a one-block budget never converged");
        }
        assert_eq!(
            scene.level(i32::from(MAX_LEVEL) - 1, 1, 0),
            1,
            "a starved solver reached a different answer than an unbounded one"
        );
    }

    #[test]
    fn two_sources_side_by_side_do_not_make_a_third() {
        // The scope decision, asserted: no infinite-milk duplication.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        spring(&mut scene, &mut solver, 2, 1, 0);
        scene.settle(&mut solver);

        let between = scene.at(1, 1, 0);
        assert!(
            !between.is_source(),
            "two sources made a third at the block between them"
        );
        assert_eq!(between.level(), MAX_LEVEL - 1);
    }

    #[test]
    fn milk_prefers_the_direction_of_a_hole() {
        // **Table-driven, as the criterion asks.** A floor with one gap in it:
        // whichever side the gap is on is the only direction preferred.
        //
        // The gap is three blocks away, inside the default four-block search.
        for (name, gap, want) in [
            ("west", (-3, 0, 0), [true, false, false, false]),
            ("east", (3, 0, 0), [false, true, false, false]),
            ("north", (0, 0, -3), [false, false, true, false]),
            ("south", (0, 0, 3), [false, false, false, true]),
        ] {
            let mut scene = Scene::default().with_floor(16);
            scene.solid.remove(&gap);
            let preferred = Solver::preferred(&scene, Tuning::DEFAULT, BlockPos::new(0, 1, 0));
            assert_eq!(preferred, want, "a hole to the {name} was not preferred");
        }
    }

    #[test]
    fn a_spring_beside_a_pit_runs_into_it_rather_than_around_it() {
        // **The test that was missing, and its absence was the bug.**
        //
        // `milk_prefers_the_direction_of_a_hole` calls `preferred` directly and
        // passes. It passed while `preferred` was DEAD CODE — implemented,
        // table-driven in four directions, and never once consulted by the
        // solver. Reported from the window as "it does not seem to flow into
        // holes very well; it spreads and flows into a hole".
        //
        // So this one pours actual milk and looks at where it went. A tested
        // function is not a tested behaviour.
        let mut scene = Scene::default().with_floor(16);
        // A pit three blocks east of the spring, inside the four-block search,
        // WITH A BOTTOM. The fixture's world is unbounded downwards, so a hole
        // with nothing under it is a drain to infinity and nothing ever settles
        // — which is what the first run of this test found.
        scene.solid.remove(&(3, 0, 0));
        scene.solid.insert((3, -1, 0));
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        scene.settle(&mut solver);

        assert!(
            scene.level(1, 1, 0) > 0 && scene.level(2, 1, 0) > 0,
            "no milk ran towards the pit at all"
        );
        assert!(
            scene.at(3, 0, 0).level() > 0,
            "the milk never fell into the pit it was pointed at"
        );
        // And it did NOT spread the other way, which is the half that says the
        // preference is being applied rather than the milk simply covering
        // everything and reaching the pit by accident.
        assert_eq!(
            scene.level(-1, 1, 0),
            0,
            "milk spread away from the pit as well, so nothing is steering it"
        );
        assert_eq!(
            scene.level(0, 1, 1),
            0,
            "milk spread sideways as well, so nothing is steering it"
        );
    }

    #[test]
    fn milk_runs_down_a_sub_node_smoothed_slope() {
        // **The case §4's old rule got wrong, and it is the COMMON case.**
        //
        // A smoothed hillside is a ramp INSIDE blocks: every column's top block
        // is `Partial`. Under "accepts fluid iff empty" every one of those was
        // fluid-solid, so nothing below any of them was a drop, the solver saw a
        // perfectly flat floor, and milk spread as a disc across the hill —
        // floating above ground that was smooth beneath it.
        //
        // Blocky terrain hides this completely, because a blocky hillside is a
        // staircase and every step is a real drop. That is why every test above
        // this one passed while the behaviour was wrong.
        let mut scene = Scene::default();
        // A solid floor under everything, so nothing drains out of the scene —
        // the first version of this test had none west of the ramp and the milk
        // ran off that cliff instead, which was the solver being right about a
        // world that was wrong.
        for x in -10..=10 {
            for z in -5..=5 {
                scene.solid.insert((x, -1, z));
            }
        }
        // A plateau west of the spring so there is only one way down.
        for x in -10..=-1 {
            for z in -5..=5 {
                scene.solid.insert((x, 0, z));
            }
        }
        // The ramp itself, descending eastward: 24 cells full at x=0 down to 3
        // at x=7. Above the threshold at first, below it from x=4 on, so the
        // milk runs along the top and then sinks INTO the thinning ground.
        for x in 0..=7 {
            for z in -5..=5 {
                let filled = 24u32.saturating_sub(x as u32 * 3);
                scene.partial.insert((x, 0, z), filled);
            }
        }
        let mut solver = Solver::new();
        // The spring sits at the top of the ramp, in the air above it.
        scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::source(MILK));
        solver.touch(BlockPos::new(0, 1, 0));
        scene.settle(&mut solver);

        // It reached the bottom of the slope, which under the old rule it could
        // not do at all — every block of the ramp read as floor.
        assert!(
            scene.level(7, 0, 0) > 0,
            "milk never ran down the slope: {:?}",
            scene.fluid
        );
        // And it is INSIDE the ramp blocks rather than floating on top of them,
        // which is the half that makes it look right.
        assert!(
            scene.level(5, 0, 0) > 0,
            "milk sat above the slope instead of in it"
        );
    }

    #[test]
    fn a_block_more_than_half_full_still_holds_fluid_up() {
        // The other side of the threshold, so this is not just "everything is
        // permeable now". Sixteen of 27 is above the default of fourteen.
        let mut scene = Scene::default();
        for x in -4..=4 {
            for z in -4..=4 {
                scene.partial.insert((x, 0, z), 16);
            }
        }
        let mut solver = Solver::new();
        scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::source(MILK));
        solver.touch(BlockPos::new(0, 1, 0));
        scene.settle(&mut solver);

        assert_eq!(
            scene.level(0, 0, 0),
            0,
            "milk sank through a block that is more solid than not"
        );
        assert!(
            scene.level(1, 1, 0) > 0,
            "milk did not spread across the floor it should have been held up by"
        );
    }

    #[test]
    fn with_no_hole_in_range_milk_spreads_evenly() {
        let scene = Scene::default().with_floor(16);
        let preferred = Solver::preferred(&scene, Tuning::DEFAULT, BlockPos::new(0, 1, 0));
        assert_eq!(preferred, [true; 4], "an open floor should spread evenly");
    }

    #[test]
    fn a_hole_beyond_the_search_is_not_preferred() {
        // Five blocks away against a four-block search: milk should not see it.
        let mut scene = Scene::default().with_floor(16);
        scene.solid.remove(&(6, 0, 0));
        let preferred = Solver::preferred(&scene, Tuning::DEFAULT, BlockPos::new(0, 1, 0));
        assert_eq!(
            preferred, [true; 4],
            "a hole outside the search range was still preferred"
        );
    }

    #[test]
    fn a_wall_is_not_a_direction_milk_can_go() {
        let scene = Scene::default().with_floor(16).wall(1, 1, 0);
        let preferred = Solver::preferred(&scene, Tuning::DEFAULT, BlockPos::new(0, 1, 0));
        assert!(!preferred[1], "milk was sent into a wall");
    }

    #[test]
    fn building_in_a_pond_displaces_the_milk() {
        // A block placed where milk is takes its place; Contract §4 has no
        // partially-flooded blocks.
        let mut scene = Scene::default().with_floor(16);
        let mut solver = Solver::new();
        spring(&mut scene, &mut solver, 0, 1, 0);
        scene.settle(&mut solver);
        assert!(scene.level(2, 1, 0) > 0);

        scene.solid.insert((2, 1, 0));
        solver.touch(BlockPos::new(2, 1, 0));
        solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);

        assert_eq!(
            scene.level(2, 1, 0),
            0,
            "milk stayed inside a block somebody built"
        );
    }

    #[test]
    fn the_visit_order_does_not_change_the_answer() {
        // **Charter rule 4, at the level that matters here.** The active set is
        // ordered, so the same edits give the same milk however they were
        // queued — this asserts it by queueing them backwards.
        let build = |reversed: bool| {
            let mut scene = Scene::default().with_floor(16);
            let mut solver = Solver::new();
            let springs = [(0, 1, 0), (4, 1, 2), (-3, 1, -1)];
            if reversed {
                for (x, y, z) in springs.iter().rev() {
                    spring(&mut scene, &mut solver, *x, *y, *z);
                }
            } else {
                for (x, y, z) in &springs {
                    spring(&mut scene, &mut solver, *x, *y, *z);
                }
            }
            scene.settle(&mut solver);
            scene.fluid
        };
        assert_eq!(build(false), build(true));
    }
}

#[cfg(test)]
mod properties {
    use std::collections::{BTreeMap, BTreeSet};

    use proptest::prelude::*;

    use super::super::FluidId;
    use super::*;

    const MILK: FluidId = FluidId(1);
    /// The scene is a box this many blocks across, centred on the origin.
    const SPAN: i32 = 6;

    #[derive(Debug, Default)]
    struct Scene {
        solid: BTreeSet<(i32, i32, i32)>,
        fluid: BTreeMap<(i32, i32, i32), Fluid>,
    }

    impl Neighbourhood for Scene {
        fn occupancy(&self, pos: BlockPos) -> Option<u32> {
            // **Walled in on every side.** A test box with open edges lets milk
            // pour out of the world, and then "no floating milk" holds for the
            // uninteresting reason that there is no milk.
            if pos.x.abs() > SPAN || pos.z.abs() > SPAN || pos.y < 0 || pos.y > SPAN {
                return None;
            }
            Some(if self.solid.contains(&(pos.x, pos.y, pos.z)) {
                crate::UNITS_PER_BLOCK
            } else {
                0
            })
        }

        fn fluid(&self, pos: BlockPos) -> Fluid {
            self.fluid
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Fluid::EMPTY)
        }

        fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
            if value.is_empty() {
                self.fluid.remove(&(pos.x, pos.y, pos.z));
            } else {
                self.fluid.insert((pos.x, pos.y, pos.z), value);
            }
        }
    }

    /// One thing a player might do.
    #[derive(Debug, Clone, Copy)]
    enum Edit {
        Place(i32, i32, i32),
        Break(i32, i32, i32),
        Pour(i32, i32, i32),
        Scoop(i32, i32, i32),
    }

    fn edits() -> impl Strategy<Value = Vec<Edit>> {
        let coord = -SPAN..=SPAN;
        let height = 0..=SPAN;
        let edit = (0u8..4, coord.clone(), height, coord).prop_map(|(kind, x, y, z)| match kind {
            0 => Edit::Place(x, y, z),
            1 => Edit::Break(x, y, z),
            2 => Edit::Pour(x, y, z),
            _ => Edit::Scoop(x, y, z),
        });
        proptest::collection::vec(edit, 1..12)
    }

    fn apply(scene: &mut Scene, solver: &mut Solver, edit: Edit) {
        match edit {
            Edit::Place(x, y, z) => {
                scene.solid.insert((x, y, z));
            }
            Edit::Break(x, y, z) => {
                scene.solid.remove(&(x, y, z));
            }
            Edit::Pour(x, y, z) => {
                if !is_floor(&*scene, Tuning::DEFAULT, BlockPos::new(x, y, z)) {
                    scene.set_fluid(BlockPos::new(x, y, z), Fluid::source(MILK));
                }
            }
            Edit::Scoop(x, y, z) => scene.set_fluid(BlockPos::new(x, y, z), Fluid::EMPTY),
        }
        solver.touch(BlockPos::new(
            match edit {
                Edit::Place(x, _, _)
                | Edit::Break(x, _, _)
                | Edit::Pour(x, _, _)
                | Edit::Scoop(x, _, _) => x,
            },
            match edit {
                Edit::Place(_, y, _)
                | Edit::Break(_, y, _)
                | Edit::Pour(_, y, _)
                | Edit::Scoop(_, y, _) => y,
            },
            match edit {
                Edit::Place(_, _, z)
                | Edit::Break(_, _, z)
                | Edit::Pour(_, _, z)
                | Edit::Scoop(_, _, z) => z,
            },
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// **The two invariants the criteria name**, after any edit sequence.
        ///
        /// Every flow block traces back to a source, so there is no milk hanging
        /// in the air with nothing feeding it; and no block that cannot accept
        /// fluid is holding any, which is Contract §4 with no exceptions.
        #[test]
        fn settled_milk_has_a_parent_chain_and_never_sits_inside_a_block(edits in edits()) {
            let mut scene = Scene::default();
            let mut solver = Solver::new();
            for edit in edits {
                apply(&mut scene, &mut solver, edit);
            }
            for _ in 0..400 {
                if solver.is_settled() {
                    break;
                }
                solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);
            }
            prop_assert!(solver.is_settled(), "never settled");

            for (&(x, y, z), &value) in &scene.fluid {
                let pos = BlockPos::new(x, y, z);
                prop_assert!(
                    !is_floor(&scene, Tuning::DEFAULT, pos),
                    "{pos:?} holds milk and does not accept fluid"
                );
                if value.is_source() {
                    continue;
                }
                // A parent is the block above, if that is falling into this one,
                // or a lateral neighbour strictly brighter — the same rule the
                // solver applied, asked in reverse.
                let above = BlockPos::new(x, y + 1, z);
                let fed_from_above = !scene.fluid(above).is_empty();
                let fed_sideways = LATERAL.iter().any(|offset| {
                    let at = BlockPos::new(x + offset[0], y + offset[1], z + offset[2]);
                    let there = scene.fluid(at);
                    !there.is_empty() && there.level() > value.level()
                });
                prop_assert!(
                    fed_from_above || fed_sideways,
                    "{pos:?} holds level {} with nothing feeding it",
                    value.level()
                );
            }
        }

        /// Draining every source empties the world and the active set.
        ///
        /// The stronger half of "removing a source drains completely": it holds
        /// after ANY history, not only after the tidy single-spring case.
        #[test]
        fn taking_every_source_away_leaves_nothing_behind(edits in edits()) {
            let mut scene = Scene::default();
            let mut solver = Solver::new();
            for edit in edits {
                apply(&mut scene, &mut solver, edit);
            }
            for _ in 0..400 {
                if solver.is_settled() {
                    break;
                }
                solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);
            }

            let sources: Vec<_> = scene
                .fluid
                .iter()
                .filter(|(_, value)| value.is_source())
                .map(|(at, _)| *at)
                .collect();
            for (x, y, z) in sources {
                scene.set_fluid(BlockPos::new(x, y, z), Fluid::EMPTY);
                solver.touch(BlockPos::new(x, y, z));
            }
            for _ in 0..400 {
                if solver.is_settled() {
                    break;
                }
                solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX);
            }

            prop_assert!(solver.is_settled(), "never settled after draining");
            prop_assert!(
                scene.fluid.is_empty(),
                "orphan milk survived every source going away: {:?}",
                scene.fluid
            );
        }
    }
}
