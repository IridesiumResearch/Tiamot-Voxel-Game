// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The update rule.
//!
//! # What it is
//!
//! **Conserved flow**, stated as four rules applied in a fixed order to one
//! block at a time. Sub-Node Contract §4.2 is authoritative; this implements it.
//!
//! 1. **Down first.** Move as much volume as the block below will accept.
//! 2. **Sideways.** For each horizontal neighbour holding less, lowest first,
//!    move half the difference. A difference of one moves nothing, so a pond
//!    **settles without a separate stability test** — that is what makes this
//!    terminate.
//! 3. **Stuck droplets.** One or two cells cannot split, so on a slope they
//!    would streak forever. They move whole, or not at all.
//! 4. **Sinks.** Absorption into ground a mod declared absorbent, and
//!    evaporation from a block open to the air.
//!
//! # Conservation, and why the sinks are counted
//!
//! Volume is never created. It leaves only through a sink, and [`Sinks`] records
//! how much left through each — charter rule 15's conservation proptest is
//! unwritable otherwise, because "volume in equals volume out" is false by
//! design and "volume in equals volume out plus what was absorbed plus what
//! evaporated" is the invariant that actually holds.
//!
//! # Determinism
//!
//! Charter rule 4. The active set is a `BTreeSet`, so blocks are visited in
//! coordinate order rather than insertion order; neighbours are visited in a
//! fixed order rotated by **the block's own coordinates**; every arithmetic
//! operation here is on integers.
//!
//! **The rotation is coordinate-derived and not tick-derived**, which matters
//! twice. A tick counter has to be persisted or a reloaded world diverges from a
//! fresh one, and a tick-derived rotation makes every block in the world favour
//! the same side on the same tick — visible as a pulse crossing a large pond.
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
use crate::detgen::rng::SplitMix64;

use super::{Fluid, FluidId, capacity};

/// The four lateral directions, in the order they are visited before rotation.
///
/// Down is handled separately and first, because falling beats spreading. Up is
/// never a flow direction — this fluid does not climb — and is absent.
const LATERAL: [[i32; 3]; 4] = [[-1, 0, 0], [1, 0, 0], [0, 0, -1], [0, 0, 1]];

/// The world, as the fluid solver needs to see it.
///
/// Deliberately the same shape as [`crate::light::propagate::Neighbourhood`] and
/// for the same reason: the server has a world database behind a cache and the
/// client has a map of streamed chunks, and one update rule has to run over
/// both or the client cannot predict what the server will do.
pub trait Neighbourhood {
    /// How full a block is of terrain, in cells of 27.
    ///
    /// **Sub-Node Contract §4.** The world reports a fact and the fluid decides
    /// what it means: [`capacity`] turns this and the fluid's own `waterlogs_at`
    /// into how much will fit. Two fluids in one world may disagree about what
    /// counts as floor, which is why the threshold lives with the fluid and this
    /// does not.
    ///
    /// `None` for anything not loaded — NOT zero, and the difference matters.
    /// Zero would let a flood run off the edge of the loaded world and a pond
    /// drain silently into a chunk that has not arrived.
    fn occupancy(&self, pos: BlockPos) -> Option<u32>;

    /// How many cells of fluid this block soaks up per fluid tick, or zero.
    ///
    /// **A fact about the block, not a policy.** Which materials are absorbent,
    /// how much they take and what they turn into when they have had it are the
    /// mod's (charter rule 1); this module knows only the number, exactly as it
    /// knows occupancy and not what the block is made of. The material swap is
    /// applied by whoever is holding the registry, from the [`Sinks::absorbed`]
    /// events this produces.
    ///
    /// Zero for anything not loaded, because the caller has already established
    /// loadedness through [`Neighbourhood::occupancy`] before asking.
    fn absorbency(&self, pos: BlockPos) -> u32 {
        let _ = pos;
        0
    }

    /// What a block holds now.
    fn fluid(&self, pos: BlockPos) -> Fluid;

    /// Records what a block holds.
    ///
    /// Positions outside what the implementation holds are dropped rather than
    /// being an error, exactly as light does: a flow reaching the edge of the
    /// loaded region has nowhere to write.
    fn set_fluid(&mut self, pos: BlockPos, value: Fluid);
}

/// How one fluid behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tuning {
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
    /// only knob that changes how fast a pour is SEEN to run, which is the
    /// first thing anybody watching a fluid has an opinion about.
    pub tick_rate: u8,
    /// One in how many fluid ticks an exposed block loses a cell, or zero.
    ///
    /// **A declared sink** (Sub-Node Contract §4.3). Only a block with air
    /// directly above it evaporates, so a wide shallow pool goes before a deep
    /// narrow one. Zero never evaporates.
    pub evaporates: u32,
}

impl Tuning {
    /// What milk uses, and a sensible default for a fluid that behaves like it.
    pub const DEFAULT: Self = Self {
        // Fourteen of 27 — over half. Under it the block is more air than
        // anything and fluid runs through; at or above it, it is more solid
        // than not and holds the fluid up.
        waterlogs_at: 14,
        tick_rate: 1,
        // Off. Destroying matter is a mod's call, exactly as creating it was.
        evaporates: 0,
    };
}

/// One block's worth of change, for whoever needs to hear about it.
///
/// The server broadcasts these and the client applies them; both also use them
/// to decide which chunks need re-meshing. Carrying the previous value as well
/// as the new one means a listener can tell "a puddle appeared" from "a puddle
/// got deeper" without holding its own copy of the world.
///
/// **A conserved transfer produces two of these**, one for each end. The old
/// model changed one block at a time; this one cannot, and a listener that
/// assumed otherwise would draw half of every flow.
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
    /// How much it was pressing with, in cells.
    pub volume: u32,
}

/// One block soaking up fluid.
///
/// The block's material is not here for the same reason [`Blocked`]'s is not:
/// the solver has no registry. Whoever holds one turns this into "dirt becomes
/// damp dirt".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Absorbed {
    /// The block that soaked it up.
    pub pos: BlockPos,
    /// Which fluid it took.
    pub fluid: FluidId,
    /// How many cells it took.
    pub cells: u32,
}

/// Where volume went when it left the world.
///
/// **Every cell destroyed is counted here.** Sub-Node Contract §4.3: the
/// conservation invariant is `in == still present + absorbed + evaporated +
/// displaced`, and a solver that quietly dropped a cell would make that
/// unwritable rather than merely false.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Sinks {
    /// Blocks that soaked fluid up, and how much each took.
    pub absorbed: Vec<Absorbed>,
    /// Cells lost to the air.
    pub evaporated: u32,
    /// Cells that had nowhere to go when terrain took their space.
    ///
    /// Somebody filling a flooded block with stone displaces what was in it.
    /// The engine pushes it out first and only destroys what nothing would
    /// accept — but destroy it it must, and an uncounted loss here is a
    /// conservation test that fails for the wrong reason.
    pub displaced: u32,
}

impl Sinks {
    /// Total cells destroyed.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.absorbed.iter().map(|entry| entry.cells).sum::<u32>()
            + self.evaporated
            + self.displaced
    }

    /// Whether nothing was destroyed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.absorbed.is_empty() && self.evaporated == 0 && self.displaced == 0
    }
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
    /// Carried rather than dropped: a pour that overruns its budget finishes
    /// next tick instead of leaving milk half-spread forever.
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
    /// Where volume went when it left the world, since this was last drained.
    ///
    /// Accumulated rather than returned per tick because the caller that
    /// applies material swaps and the caller that checks conservation are the
    /// same caller, and both want it after the tick rather than during.
    sinks: Sinks,
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

/// The most cells a single block can hold and still be a droplet.
///
/// **Sub-Node Contract §4.2 rule 3.** Half of anything at or below this rounds
/// down to nothing, so a droplet cannot split and would streak down a slope for
/// ever. Two cells rather than one because `(2 - 0) / 2` is one, which leaves a
/// single cell behind — the streak, one tick later.
const DROPLET: u32 = 2;

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
    pub fn take_blocked(&mut self) -> Vec<Blocked> {
        std::mem::take(&mut self.blocked)
    }

    /// Takes what has been destroyed since this was last called.
    ///
    /// The caller applies the material swaps [`Sinks::absorbed`] describes, and
    /// a conservation test adds [`Sinks::total`] to what is still in the world.
    pub fn take_sinks(&mut self) -> Sinks {
        std::mem::take(&mut self.sinks)
    }

    /// Runs one fluid tick, visiting at most `budget` blocks.
    ///
    /// Returns every change made, in the order it was made, for broadcasting and
    /// re-meshing. Blocks not reached within the budget are carried to the next
    /// tick rather than dropped.
    ///
    /// `seed` is the world seed and `fluid_tick` the tick number; together with
    /// a block's position they are the whole of the randomness evaporation uses
    /// (charter rule 4). A process RNG here would fail the cross-platform hash
    /// gate — or worse, would not, and two servers would drift.
    ///
    /// # The budget is a cap on VISITS, not on changes
    ///
    /// A block that is examined and left alone still costs a lookup, and the
    /// pathological case — a settled pond re-queued by an edit — is all
    /// examinations and no changes. Counting changes would let that case run
    /// unbounded, which is exactly the tick overrun the cap is there to prevent.
    pub fn tick(
        &mut self,
        world: &mut impl Neighbourhood,
        tuning: Tuning,
        budget: usize,
        seed: u64,
        fluid_tick: u64,
    ) -> Vec<Flow> {
        let mut changes = Vec::new();
        // Last tick's leftovers first, so a block cannot be starved forever by
        // a pour that keeps re-queueing its own neighbourhood.
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
            let before = changes.len();
            settle_one(
                world,
                tuning,
                pos,
                seed,
                fluid_tick,
                &mut changes,
                &mut self.sinks,
            );
            // Everything that changed wakes its own neighbourhood, including
            // the block above: milk drained from under a column is what lets
            // the column fall.
            for change in &changes[before..] {
                woken.insert(change.pos);
                woken.insert(BlockPos::new(change.pos.x, change.pos.y + 1, change.pos.z));
            }
        }
        for pos in woken {
            self.touch(pos);
        }
        changes
    }
}

/// How much more fluid a block will take of `fluid`, in cells.
///
/// Zero for a block already holding a different fluid: two fluids do not mix,
/// and the one that got there first keeps the space.
fn accepts(world: &impl Neighbourhood, tuning: Tuning, pos: BlockPos, fluid: FluidId) -> u32 {
    let Some(occupancy) = world.occupancy(pos) else {
        // Not loaded. Sub-Node Contract §4.2: unloaded is solid, or a flood
        // runs off the edge of the world.
        return 0;
    };
    let here = world.fluid(pos);
    if !here.is_empty() && here.fluid() != fluid {
        return 0;
    }
    capacity(occupancy, tuning.waterlogs_at).saturating_sub(here.volume())
}

/// Moves `cells` of `fluid` from one block to another, recording both ends.
///
/// Both ends, because a conserved transfer changes two blocks and a listener
/// that heard about one would draw half a flow.
fn transfer(
    world: &mut impl Neighbourhood,
    from: BlockPos,
    into: BlockPos,
    fluid: FluidId,
    cells: u32,
    out: &mut Vec<Flow>,
) {
    if cells == 0 {
        return;
    }
    let was_from = world.fluid(from);
    let was_into = world.fluid(into);
    let now_from = was_from.with_volume(was_from.volume() - cells);
    let now_into = Fluid::new(fluid, was_into.volume() + cells);
    world.set_fluid(from, now_from);
    world.set_fluid(into, now_into);
    out.push(Flow {
        pos: from,
        was: was_from,
        now: now_from,
    });
    out.push(Flow {
        pos: into,
        was: was_into,
        now: now_into,
    });
}

/// The four lateral directions, rotated by the block's own coordinates.
///
/// **Sub-Node Contract §4.2.** Without a rotation the same side is always
/// served first and a pour spreads lopsidedly. Rotating by the tick counter
/// instead would have to be persisted — a reloaded world would diverge from a
/// fresh one — and would make the whole world favour one side at once, which
/// reads as a pulse crossing a pond rather than as water finding its level.
const fn rotation(pos: BlockPos) -> usize {
    // `rem_euclid` rather than `%`: the world has negative coordinates and a
    // negative index is not a direction.
    (pos.x.rem_euclid(4) + pos.y.rem_euclid(4) + pos.z.rem_euclid(4)) as usize % 4
}

/// Whether this block loses a cell to the air on this tick.
///
/// Stateless: the seed, the position and the tick number are the whole input,
/// so no counter has to survive a save and two servers agree without talking.
fn evaporates(tuning: Tuning, pos: BlockPos, seed: u64, fluid_tick: u64) -> bool {
    if tuning.evaporates == 0 {
        return false;
    }
    // Mixed rather than added so that moving one block does not simply shift
    // which tick it happens on.
    let mixed = seed
        ^ (i64::from(pos.x) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (i64::from(pos.y) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (i64::from(pos.z) as u64).wrapping_mul(0x1656_67B1_9E37_79F9)
        ^ fluid_tick.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    SplitMix64::new(mixed)
        .next_u64()
        .is_multiple_of(u64::from(tuning.evaporates))
}

/// Records the lateral directions this block's fluid is pressing into and
/// cannot enter.
///
/// **Only where the fluid would actually have gone.** A block with a single cell
/// has nothing to give — half of one is nothing — so it presses against nothing
/// and reports nothing, and a dry block obviously does not either. Without that,
/// every solid block adjacent to any milk anywhere would generate an event every
/// time the pond was examined.
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
    if here.is_empty() || here.volume() <= 1 {
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
        // Somewhere it could go is not somewhere it was stopped.
        if accepts(world, tuning, into, here.fluid()) > 0 {
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
            volume: here.volume(),
        });
    }
}

/// Applies the whole rule to one block, appending what changed.
fn settle_one(
    world: &mut impl Neighbourhood,
    tuning: Tuning,
    pos: BlockPos,
    seed: u64,
    fluid_tick: u64,
    out: &mut Vec<Flow>,
    sinks: &mut Sinks,
) {
    let here = world.fluid(pos);
    if here.is_empty() {
        return;
    }
    let fluid = here.fluid();

    // **Terrain arriving in a flooded block.** Somebody placed stone where milk
    // was, so the block now holds more than fits. Pushed out below and sideways
    // first; only what nothing will take is destroyed, and it is counted.
    let room = world
        .occupancy(pos)
        .map_or(0, |occupancy| capacity(occupancy, tuning.waterlogs_at));
    if here.volume() > room {
        let excess = here.volume() - room;
        let spilled = spill(world, tuning, pos, fluid, excess, out);
        if spilled < excess {
            let lost = excess - spilled;
            let now = world.fluid(pos);
            let was = now;
            let now = now.with_volume(now.volume().saturating_sub(lost));
            world.set_fluid(pos, now);
            out.push(Flow { pos, was, now });
            sinks.displaced += lost;
        }
        if world.fluid(pos).is_empty() {
            return;
        }
    }

    // Rule 1 — down first.
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let mine = world.fluid(pos).volume();
    let falling = accepts(world, tuning, below, fluid).min(mine);
    if falling > 0 {
        transfer(world, pos, below, fluid, falling, out);
        if world.fluid(pos).is_empty() {
            return;
        }
    }

    // Rule 2 — sideways, lowest-holding first, half the difference each.
    //
    // Recomputed after every transfer so a block can never give away more than
    // it has, and sorted so the emptiest neighbour is served first: filling the
    // lowest first is what makes a pond level rather than terraced.
    let turn = rotation(pos);
    let mut neighbours: Vec<(u32, usize, BlockPos)> = (0..LATERAL.len())
        .map(|index| {
            let offset = LATERAL[(index + turn) % LATERAL.len()];
            let at = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
            (world.fluid(at).volume(), index, at)
        })
        .collect();
    // Ties break on the rotated direction index, which is a fixed order for a
    // given block — never on the address of anything.
    neighbours.sort_by_key(|(volume, index, _)| (*volume, *index));

    for (_, _, at) in neighbours {
        let mine = world.fluid(pos).volume();
        let theirs = world.fluid(at).volume();
        if theirs >= mine {
            continue;
        }
        let half = (mine - theirs) / 2;
        let moved = half.min(accepts(world, tuning, at, fluid));
        if moved > 0 {
            transfer(world, pos, at, fluid, moved, out);
        }
    }

    // Rule 3 — stuck droplets move whole or not at all.
    let mine = world.fluid(pos).volume();
    if mine > 0 && mine <= DROPLET {
        let turn = rotation(pos);
        for index in 0..LATERAL.len() {
            let offset = LATERAL[(index + turn) % LATERAL.len()];
            let at = BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2]);
            if !world.fluid(at).is_empty() || accepts(world, tuning, at, fluid) < mine {
                continue;
            }
            // Only downhill. A droplet that moved sideways onto level ground
            // would wander for ever, and two of them would swap places.
            let under = BlockPos::new(at.x, at.y - 1, at.z);
            if accepts(world, tuning, under, fluid) == 0 {
                continue;
            }
            transfer(world, pos, at, fluid, mine, out);
            return;
        }
    }

    // Rule 4 — the sinks.
    absorb(world, pos, fluid, out, sinks);
    if world.fluid(pos).is_empty() {
        return;
    }
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if world.fluid(above).is_empty()
        && world
            .occupancy(above)
            .is_some_and(|occupancy| occupancy == 0)
        && evaporates(tuning, pos, seed, fluid_tick)
    {
        let was = world.fluid(pos);
        let now = was.with_volume(was.volume() - 1);
        world.set_fluid(pos, now);
        out.push(Flow { pos, was, now });
        sinks.evaporated += 1;
    }
}

/// Pushes `cells` out of a block that no longer has room, returning how many
/// found somewhere to go.
fn spill(
    world: &mut impl Neighbourhood,
    tuning: Tuning,
    pos: BlockPos,
    fluid: FluidId,
    cells: u32,
    out: &mut Vec<Flow>,
) -> u32 {
    let mut left = cells;
    let turn = rotation(pos);
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let sideways = (0..LATERAL.len()).map(|index| {
        let offset = LATERAL[(index + turn) % LATERAL.len()];
        BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2])
    });
    for at in std::iter::once(below).chain(sideways) {
        if left == 0 {
            break;
        }
        let moved = accepts(world, tuning, at, fluid).min(left);
        if moved > 0 {
            transfer(world, pos, at, fluid, moved, out);
            left -= moved;
        }
    }
    cells - left
}

/// Lets the ground either side of and beneath a block soak fluid out of it.
///
/// One absorption per neighbour per tick, and a neighbour takes at most what is
/// there: a single cell over ground that would drink three is one cell absorbed,
/// not a debt.
fn absorb(
    world: &mut impl Neighbourhood,
    pos: BlockPos,
    fluid: FluidId,
    out: &mut Vec<Flow>,
    sinks: &mut Sinks,
) {
    let turn = rotation(pos);
    let below = BlockPos::new(pos.x, pos.y - 1, pos.z);
    let sideways: Vec<BlockPos> = (0..LATERAL.len())
        .map(|index| {
            let offset = LATERAL[(index + turn) % LATERAL.len()];
            BlockPos::new(pos.x + offset[0], pos.y + offset[1], pos.z + offset[2])
        })
        .collect();
    for at in std::iter::once(below).chain(sideways) {
        let mine = world.fluid(pos).volume();
        if mine == 0 {
            return;
        }
        if world.occupancy(at).is_none() {
            continue;
        }
        let cells = world.absorbency(at).min(mine);
        if cells == 0 {
            continue;
        }
        let was = world.fluid(pos);
        let now = was.with_volume(mine - cells);
        world.set_fluid(pos, now);
        out.push(Flow { pos, was, now });
        sinks.absorbed.push(Absorbed {
            pos: at,
            fluid,
            cells,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::fluid::MAX_VOLUME;

    /// The fluid every scene here pours.
    pub(super) const MILK: FluidId = FluidId(1);

    /// A world seed. Only evaporation reads it, and most scenes have it off.
    const SEED: u64 = 0x2E5A_11C0_77BD_9134;

    /// A world made of a set of solid blocks and a map of fluid.
    ///
    /// Everything outside `loaded` is unloaded, which is NOT the same as empty:
    /// the solver must treat it as solid, and a scene that answered zero
    /// everywhere could not test that.
    #[derive(Default)]
    pub(super) struct Scene {
        solid: BTreeSet<(i32, i32, i32)>,
        absorbent: BTreeMap<(i32, i32, i32), u32>,
        fluid: BTreeMap<(i32, i32, i32), Fluid>,
        loaded: Option<BTreeSet<(i32, i32, i32)>>,
    }

    impl Scene {
        /// A floor spanning `xs` by `zs` at `y`, with everything above it open.
        fn floored(xs: std::ops::RangeInclusive<i32>, zs: std::ops::RangeInclusive<i32>) -> Self {
            let mut scene = Self::default();
            for x in xs {
                for z in zs.clone() {
                    scene.solid.insert((x, 0, z));
                }
            }
            scene
        }

        /// A sealed box of side `2 * radius`, air inside and solid all round.
        ///
        /// **The property tests need this and the unit tests mostly do not.**
        /// An open scene reports everything outside its floor as empty and
        /// loaded, so milk poured near an edge falls out of the world for ever
        /// — which makes a conservation property fail on the fixture rather
        /// than on the rule.
        pub(super) fn sealed(radius: i32) -> Self {
            let mut scene = Self::default();
            let mut loaded = BTreeSet::new();
            for x in -radius..=radius {
                for y in -radius..=radius {
                    for z in -radius..=radius {
                        loaded.insert((x, y, z));
                        if x.abs() == radius || y.abs() == radius || z.abs() == radius {
                            scene.solid.insert((x, y, z));
                        }
                    }
                }
            }
            scene.loaded = Some(loaded);
            scene
        }

        pub(super) fn pour(&mut self, pos: BlockPos, volume: u32) {
            self.fluid
                .insert((pos.x, pos.y, pos.z), Fluid::new(MILK, volume));
        }

        /// Makes a block solid, and takes any milk in it with it.
        pub(super) fn make_solid(&mut self, x: i32, y: i32, z: i32) {
            self.solid.insert((x, y, z));
        }

        pub(super) fn make_absorbent(&mut self, x: i32, y: i32, z: i32, rate: u32) {
            self.absorbent.insert((x, y, z), rate);
        }

        fn at(&self, pos: BlockPos) -> u32 {
            self.volume_at(pos)
        }

        pub(super) fn volume_at(&self, pos: BlockPos) -> u32 {
            self.fluid
                .get(&(pos.x, pos.y, pos.z))
                .map_or(0, |value| value.volume())
        }

        /// Every block holding milk, for a property to walk.
        pub(super) fn contents(&self) -> impl Iterator<Item = (&(i32, i32, i32), &Fluid)> {
            self.fluid.iter()
        }

        /// Every cell of fluid in the scene.
        pub(super) fn total(&self) -> u32 {
            self.fluid.values().map(|value| value.volume()).sum()
        }
    }

    impl Neighbourhood for Scene {
        fn occupancy(&self, pos: BlockPos) -> Option<u32> {
            if let Some(loaded) = &self.loaded
                && !loaded.contains(&(pos.x, pos.y, pos.z))
            {
                return None;
            }
            Some(if self.solid.contains(&(pos.x, pos.y, pos.z)) {
                MAX_VOLUME
            } else {
                0
            })
        }

        fn absorbency(&self, pos: BlockPos) -> u32 {
            self.absorbent
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(0)
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

    /// Settles a scene under [`SEED`], returning what was destroyed on the way.
    fn settle(scene: &mut Scene, solver: &mut Solver, tuning: Tuning, ticks: u64) -> Sinks {
        settle_seeded(scene, solver, tuning, ticks, SEED)
    }

    /// The same, under a named seed.
    ///
    ///
    /// **Separate because the seed has to be reachable.** The first version of
    /// the determinism test below took a seed and then called a helper that
    /// used the constant, so both halves ran identically and it asserted
    /// nothing at all.
    pub(super) fn settle_seeded(
        scene: &mut Scene,
        solver: &mut Solver,
        tuning: Tuning,
        ticks: u64,
        seed: u64,
    ) -> Sinks {
        for tick in 0..ticks {
            solver.tick(scene, tuning, usize::MAX, seed, tick);
        }
        solver.take_sinks()
    }

    #[test]
    fn a_pour_keeps_every_cell_it_started_with() {
        // **The invariant the whole model exists for.** Nothing here absorbs
        // and nothing evaporates, so the only honest answer after any number of
        // ticks is the number that went in.
        let mut scene = Scene::floored(-4..=4, -4..=4);
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));

        let sinks = settle(&mut scene, &mut solver, Tuning::DEFAULT, 60);

        assert!(
            sinks.is_empty(),
            "a scene with no sink in it destroyed milk"
        );
        assert_eq!(
            scene.total(),
            MAX_VOLUME,
            "milk was created or destroyed by flowing"
        );
    }

    #[test]
    fn milk_falls_before_it_spreads() {
        // Rule 1. A block over a hole empties downward rather than sideways,
        // which is what makes a waterfall a waterfall.
        let mut scene = Scene::floored(-4..=4, -4..=4);
        // A shaft: no floor directly under the pour, walled all the way down so
        // the milk has exactly one way to go, and capped at the bottom.
        //
        // **The walls are not decoration.** `Scene` reports every block outside
        // `solid` as empty and loaded, so an unwalled shaft opens onto an
        // infinite void and the milk falls out of the world — which is a fact
        // about this fixture, not about the rule.
        scene.solid.remove(&(0, 0, 0));
        for y in -3..=0 {
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                scene.solid.insert((dx, y, dz));
            }
        }
        scene.solid.insert((0, -3, 0));
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));

        settle(&mut scene, &mut solver, Tuning::DEFAULT, 60);

        assert_eq!(
            scene.at(BlockPos::new(0, 1, 0)),
            0,
            "milk stayed up top instead of falling down the shaft"
        );
        assert!(
            scene.at(BlockPos::new(0, -2, 0)) > 0,
            "nothing reached the bottom of the shaft"
        );
        assert_eq!(scene.total(), MAX_VOLUME);
    }

    #[test]
    fn a_pond_levels_itself_and_then_stops() {
        // Rule 2, and the property that makes it terminate: a difference of one
        // transfers nothing, so settling needs no separate stability test.
        let mut scene = Scene::floored(-2..=2, -2..=2);
        // Walls, so the pond has somewhere to level INTO and nowhere to escape.
        for x in -3i32..=3 {
            for z in -3i32..=3 {
                if x.abs() == 3 || z.abs() == 3 {
                    scene.solid.insert((x, 1, z));
                }
            }
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));

        settle(&mut scene, &mut solver, Tuning::DEFAULT, 200);

        assert!(
            solver.is_settled(),
            "the pond never stopped moving, so a settled world would cost a tick forever"
        );
        let volumes: Vec<u32> = scene.fluid.values().map(|value| value.volume()).collect();
        let high = volumes.iter().copied().max().unwrap_or(0);
        let low = volumes.iter().copied().min().unwrap_or(0);
        assert!(
            high - low <= 1,
            "the pond settled uneven: {low} in one block and {high} in another"
        );
        assert_eq!(scene.total(), MAX_VOLUME);
    }

    #[test]
    fn a_settled_pond_costs_nothing() {
        // The perf property Task 11 asserts, restated for the new rule: milk
        // that has finished moving leaves the active set entirely.
        let mut scene = Scene::floored(-3..=3, -3..=3);
        for x in -4i32..=4 {
            for z in -4i32..=4 {
                if x.abs() == 4 || z.abs() == 4 {
                    scene.solid.insert((x, 1, z));
                }
            }
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));
        settle(&mut scene, &mut solver, Tuning::DEFAULT, 300);

        assert!(solver.is_settled());
        assert_eq!(solver.active(), 0, "a settled pond is still being visited");
    }

    #[test]
    fn a_droplet_moves_whole_or_not_at_all() {
        // Rule 3. Half of one cell is nothing, so without this a droplet on a
        // slope leaves a permanent streak behind it.
        let mut scene = Scene::default();
        // A step down: floor at x <= 0, then a drop.
        for x in -3..=0 {
            scene.solid.insert((x, 0, 0));
        }
        scene.solid.insert((1, -2, 0));
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), 1);
        solver.touch(BlockPos::new(0, 1, 0));

        settle(&mut scene, &mut solver, Tuning::DEFAULT, 60);

        assert_eq!(
            scene.at(BlockPos::new(0, 1, 0)),
            0,
            "a single cell stayed put beside a drop, which is the streak this rule prevents"
        );
        assert_eq!(
            scene.total(),
            1,
            "the droplet was destroyed rather than moved"
        );
    }

    #[test]
    fn a_droplet_on_level_ground_stays_where_it_is() {
        // The other half of rule 3, and the reason it is only downhill: a
        // droplet free to move sideways on the flat would wander for ever, and
        // two of them would swap places every tick.
        let mut scene = Scene::floored(-3..=3, -3..=3);
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), 1);
        solver.touch(BlockPos::new(0, 1, 0));

        settle(&mut scene, &mut solver, Tuning::DEFAULT, 60);

        assert_eq!(scene.at(BlockPos::new(0, 1, 0)), 1, "the droplet wandered");
        assert!(solver.is_settled());
    }

    #[test]
    fn unloaded_neighbours_are_solid_rather_than_empty() {
        // A flood must not run off the edge of the loaded world. `None` from
        // `occupancy` means "not loaded", and treating it as zero would drain
        // a pond into a chunk that has not arrived.
        let mut scene = Scene::floored(-4..=4, -4..=4);
        let mut loaded = BTreeSet::new();
        for x in -1..=1 {
            for y in -1..=2 {
                for z in -1..=1 {
                    loaded.insert((x, y, z));
                }
            }
        }
        scene.loaded = Some(loaded);
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));

        settle(&mut scene, &mut solver, Tuning::DEFAULT, 60);

        assert_eq!(
            scene.total(),
            MAX_VOLUME,
            "milk leaked into unloaded space, so a pond drains into chunks nobody has yet"
        );
        for at in scene.fluid.keys() {
            assert!(
                at.0.abs() <= 1 && at.2.abs() <= 1,
                "milk reached {at:?}, which is outside the loaded region"
            );
        }
    }

    #[test]
    fn terrain_takes_the_space_it_needs_and_the_rest_is_counted() {
        // Sub-Node Contract §4.1: capacity is `27 − occupancy`. A block that
        // suddenly holds more than fits pushes the excess out, and only what
        // nothing will accept is destroyed — counted as `displaced`, so the
        // conservation invariant still closes.
        let mut scene = Scene::default();
        // One sealed block: floor, ceiling and four walls, holding a full load.
        scene.solid.insert((0, 0, 0));
        scene.solid.insert((0, 2, 0));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            scene.solid.insert((dx, 1, dz));
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        // Now fill it with terrain, which leaves the milk nowhere to be.
        scene.solid.insert((0, 1, 0));
        solver.touch(BlockPos::new(0, 1, 0));

        let sinks = settle(&mut scene, &mut solver, Tuning::DEFAULT, 20);

        assert_eq!(scene.total(), 0, "milk survived inside solid terrain");
        assert_eq!(
            sinks.displaced, MAX_VOLUME,
            "displaced milk was destroyed without being counted, so conservation cannot be checked"
        );
    }

    #[test]
    fn absorbent_ground_drinks_and_says_how_much() {
        // Sub-Node Contract §4.3. The solver knows a number and nothing about
        // materials; turning "three cells went into this block" into "dirt
        // becomes damp dirt" belongs to whoever holds the registry.
        let mut scene = Scene::floored(-2..=2, -2..=2);
        scene.absorbent.insert((0, 0, 0), 3);
        // Walled, because absorption is rule 4 and spreading is rule 2: an open
        // block has already given most of its milk to its neighbours by the
        // time the ground gets a turn, so an unwalled scene would measure the
        // ORDER of the rules while claiming to measure the rate.
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            scene.solid.insert((dx, 1, dz));
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), 6);
        solver.touch(BlockPos::new(0, 1, 0));

        let sinks = settle(&mut scene, &mut solver, Tuning::DEFAULT, 1);

        assert_eq!(
            sinks.absorbed.len(),
            1,
            "exactly one block should have absorbed, once"
        );
        let taken = sinks.absorbed[0];
        assert_eq!(taken.pos, BlockPos::new(0, 0, 0));
        assert_eq!(taken.cells, 3);
        assert_eq!(taken.fluid, MILK);
        assert_eq!(
            scene.total() + sinks.total(),
            6,
            "what was absorbed plus what is left is not what was poured"
        );
    }

    #[test]
    fn ground_never_drinks_more_than_is_there() {
        // A single cell over ground that would take three is one cell absorbed,
        // not a debt against a block that has nothing left.
        let mut scene = Scene::floored(-2..=2, -2..=2);
        scene.absorbent.insert((0, 0, 0), 9);
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            scene.solid.insert((dx, 1, dz));
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), 1);
        solver.touch(BlockPos::new(0, 1, 0));

        let sinks = settle(&mut scene, &mut solver, Tuning::DEFAULT, 4);

        assert_eq!(sinks.total(), 1, "absorbed more than was there to absorb");
        assert_eq!(scene.total(), 0);
    }

    #[test]
    fn evaporation_only_takes_from_blocks_open_to_the_air() {
        // A wide shallow pool goes before a deep narrow one, because more of it
        // is exposed — which only holds if a covered block is exempt.
        let mut scene = Scene::default();
        scene.solid.insert((0, 0, 0));
        // A lid directly over the milk.
        scene.solid.insert((0, 2, 0));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            scene.solid.insert((dx, 1, dz));
        }
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), 10);
        solver.touch(BlockPos::new(0, 1, 0));

        let thirsty = Tuning {
            evaporates: 1,
            ..Tuning::DEFAULT
        };
        let sinks = settle(&mut scene, &mut solver, thirsty, 40);

        assert_eq!(
            sinks.evaporated, 0,
            "milk under a lid evaporated, so depth would not protect a pool"
        );
        assert_eq!(scene.at(BlockPos::new(0, 1, 0)), 10);
    }

    #[test]
    fn evaporation_is_the_same_everywhere_for_the_same_seed() {
        // Charter rule 4. The randomness is the seed, the position and the tick
        // and nothing else — no stored counter, so two servers agree without
        // talking and a reloaded world matches a fresh one.
        let run = |seed: u64| {
            let mut scene = Scene::floored(-3..=3, -3..=3);
            let mut solver = Solver::new();
            for x in -2..=2 {
                for z in -2..=2 {
                    scene.pour(BlockPos::new(x, 1, z), 9);
                    solver.touch(BlockPos::new(x, 1, z));
                }
            }
            let thirsty = Tuning {
                evaporates: 2,
                ..Tuning::DEFAULT
            };
            let sinks = settle_seeded(&mut scene, &mut solver, thirsty, 30, seed);
            (scene.total(), sinks.evaporated)
        };

        let (held, gone) = run(SEED);
        assert_eq!(
            run(SEED),
            (held, gone),
            "the same seed gave a different world"
        );
        assert!(gone > 0, "nothing evaporated, so this test asserts nothing");
        // A different seed is allowed to agree by chance on the total, but the
        // scene should not be identical — otherwise the seed is being ignored.
        let other = run(SEED ^ 0xFFFF_FFFF);
        assert_ne!(
            other,
            (held, gone),
            "changing the world seed changed nothing, so evaporation is not seeded by it"
        );
    }

    #[test]
    fn the_direction_order_is_the_blocks_own_and_not_the_ticks() {
        // Sub-Node Contract §4.2. A tick-derived rotation has to be persisted
        // or a reloaded world diverges; a coordinate-derived one is stateless.
        // Two neighbouring blocks must not agree on which side to serve first,
        // or a pour crawls in one direction.
        let mut seen = BTreeSet::new();
        for x in 0..4 {
            seen.insert(rotation(BlockPos::new(x, 0, 0)));
        }
        assert_eq!(
            seen.len(),
            4,
            "neighbouring blocks all favour the same side"
        );

        // And it does not depend on the sign of a coordinate: a negative
        // remainder is not a direction.
        for x in -8..8 {
            let turn = rotation(BlockPos::new(x, -3, 5));
            assert!(turn < LATERAL.len(), "rotation {turn} is not a direction");
        }
    }

    #[test]
    fn the_budget_carries_what_it_could_not_reach() {
        // An unfinished flow must be finished or the world is wrong, so blocks
        // the budget did not reach are carried rather than dropped.
        let mut scene = Scene::floored(-6..=6, -6..=6);
        let mut solver = Solver::new();
        for x in -5..=5 {
            for z in -5..=5 {
                scene.pour(BlockPos::new(x, 1, z), MAX_VOLUME);
                solver.touch(BlockPos::new(x, 1, z));
            }
        }
        let queued = solver.active();
        solver.tick(&mut scene, Tuning::DEFAULT, 4, SEED, 0);
        assert!(
            solver.active() > 0,
            "a tick with a budget of four retired all {queued} blocks"
        );
        assert_eq!(scene.total(), MAX_VOLUME * 121, "a capped tick lost milk");
    }

    #[test]
    fn a_transfer_is_reported_at_both_ends() {
        // A conserved move changes two blocks. A listener told about only one
        // would draw half of every flow, and would re-mesh the wrong chunk at
        // a chunk boundary.
        let mut scene = Scene::floored(-2..=2, -2..=2);
        let mut solver = Solver::new();
        scene.pour(BlockPos::new(0, 1, 0), MAX_VOLUME);
        solver.touch(BlockPos::new(0, 1, 0));

        let changes = solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX, SEED, 0);

        let gave: Vec<&Flow> = changes
            .iter()
            .filter(|flow| flow.now.volume() < flow.was.volume())
            .collect();
        let took: Vec<&Flow> = changes
            .iter()
            .filter(|flow| flow.now.volume() > flow.was.volume())
            .collect();
        assert!(!gave.is_empty(), "nothing moved at all");
        assert_eq!(
            gave.len(),
            took.len(),
            "every cell that left a block should have arrived in another"
        );
    }
}

/// Charter rule 15: the conservation invariant, over arbitrary worlds.
///
/// **This is the test the whole model was reshaped to make writable.** Under the
/// old rule a source created milk and a drain destroyed it, so "what went in
/// came out" had no meaning; the property below is only expressible because
/// [`Sinks`] counts every cell that leaves.
#[cfg(test)]
mod properties {
    use proptest::prelude::*;

    use super::tests::{MILK, Scene, settle_seeded};
    use super::*;
    use crate::fluid::MAX_VOLUME;

    /// How far from the origin a generated world reaches.
    const SPAN: i32 = 3;

    /// One thing a generated world contains.
    #[derive(Debug, Clone)]
    enum Feature {
        /// Solid terrain at a block.
        Solid(i32, i32, i32),
        /// Ground that drinks, at a rate.
        Absorbent(i32, i32, i32, u32),
        /// Milk, poured.
        Pour(i32, i32, i32, u32),
    }

    fn coordinate() -> impl Strategy<Value = i32> {
        -SPAN..=SPAN
    }

    fn feature() -> impl Strategy<Value = Feature> {
        prop_oneof![
            (coordinate(), -SPAN..=SPAN, coordinate())
                .prop_map(|(x, y, z)| Feature::Solid(x, y, z)),
            (coordinate(), -SPAN..=SPAN, coordinate(), 1u32..=6)
                .prop_map(|(x, y, z, rate)| Feature::Absorbent(x, y, z, rate)),
            (coordinate(), -SPAN..=SPAN, coordinate(), 1u32..=MAX_VOLUME)
                .prop_map(|(x, y, z, volume)| Feature::Pour(x, y, z, volume)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        /// Every cell poured in is either still in the world or in a sink.
        #[test]
        fn nothing_is_created_and_every_loss_is_accounted_for(
            features in prop::collection::vec(feature(), 1..24),
            evaporates in prop::option::of(1u32..=4),
            ticks in 1u64..=40,
        ) {
            let mut scene = Scene::sealed(SPAN + 1);
            let mut solver = Solver::new();
            let mut poured = 0;

            for feature in &features {
                match *feature {
                    Feature::Solid(x, y, z) => {
                        scene.make_solid(x, y, z);
                    }
                    Feature::Absorbent(x, y, z, rate) => {
                        scene.make_absorbent(x, y, z, rate);
                    }
                    Feature::Pour(x, y, z, volume) => {
                        // Only into somewhere that can hold it, and only what
                        // fits: a pour into solid rock is not a pour, and
                        // counting it as one would make the property fail for
                        // the fixture's arithmetic rather than the solver's.
                        let at = BlockPos::new(x, y, z);
                        let room = accepts(&scene, Tuning::DEFAULT, at, MILK);
                        let took = volume.min(room);
                        if took > 0 {
                            let now = scene.volume_at(at) + took;
                            scene.pour(at, now);
                            solver.touch(at);
                            poured += took;
                        }
                    }
                }
            }

            let tuning = Tuning {
                evaporates: evaporates.unwrap_or(0),
                ..Tuning::DEFAULT
            };
            let sinks = settle_seeded(&mut scene, &mut solver, tuning, ticks, 0x9E37_79B9);

            prop_assert_eq!(
                scene.total() + sinks.total(),
                poured,
                "{} cells poured, {} still in the world, {} accounted for as sinks",
                poured,
                scene.total(),
                sinks.total()
            );
        }

        /// No block ever holds more than its terrain leaves room for.
        #[test]
        fn no_block_holds_more_than_it_has_room_for(
            features in prop::collection::vec(feature(), 1..24),
            ticks in 1u64..=40,
        ) {
            let mut scene = Scene::sealed(SPAN + 1);
            let mut solver = Solver::new();

            for feature in &features {
                match *feature {
                    Feature::Solid(x, y, z) => scene.make_solid(x, y, z),
                    Feature::Absorbent(x, y, z, rate) => scene.make_absorbent(x, y, z, rate),
                    Feature::Pour(x, y, z, volume) => {
                        let at = BlockPos::new(x, y, z);
                        scene.pour(at, volume);
                        solver.touch(at);
                    }
                }
            }

            settle_seeded(&mut scene, &mut solver, Tuning::DEFAULT, ticks, 0x9E37_79B9);

            for (at, value) in scene.contents() {
                let pos = BlockPos::new(at.0, at.1, at.2);
                let room = scene
                    .occupancy(pos)
                    .map_or(0, |occupancy| capacity(occupancy, Tuning::DEFAULT.waterlogs_at));
                prop_assert!(
                    value.volume() <= room,
                    "{:?} holds {} cells in room for {}",
                    at,
                    value.volume(),
                    room
                );
            }
        }
    }
}
