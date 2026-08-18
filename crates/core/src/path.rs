// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Getting from one place to another on foot.
//!
//! A* over blocks, with the walkability rule the Sub-Node Contract already
//! fixed. Like [`crate::sight`] this is a **native** helper rather than
//! something a mod builds out of per-cell reads: a search is thousands of
//! neighbour tests, and thousands of Lua calls per mob per repath is not a
//! thing the tick budget has room for.
//!
//! # Block resolution, and it is the contract's decision not this module's
//!
//! Sub-Node Contract §6: navigation is block resolution, and a block is an
//! obstacle **unless its bottom sub-node layer — the nine cells at local
//! `y == 0` — is empty**, in which case a body walks through it. That one rule
//! covers every storage form without a case for any of them: a `Uniform` solid
//! has a full bottom layer, air has an empty one, and a chiselled block is
//! whichever its own floor says it is.
//!
//! The contract also states the cost of that choice, so this module does not
//! have to re-argue it: a mob may fail to path through a gap a player can
//! squeeze into. That is accepted, because sub-node navigation multiplies the
//! search space by twenty-seven for marginal benefit.
//!
//! # Determinism
//!
//! Charter rule 4, and this module has an unusually easy time of it: **the
//! search is entirely integer**. Costs, the heuristic and the frontier ordering
//! are `u32` and `i32`, so there is no float to reason about at all. The
//! frontier breaks ties on position and the visited sets are [`BTreeMap`]s, so
//! two servers expanding the same graph expand it in the same order.
//!
//! # The budget is the point
//!
//! A search is unbounded work in the shape of a function call, and the caller is
//! a script. [`Options::budget`] caps expansions and [`Route::Exhausted`] is a
//! real answer rather than an error — a mob that cannot find a way in two
//! thousand nodes should do something else, not stall the tick while the engine
//! keeps looking.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use crate::block::BlockView;
use crate::coords::BlockPos;
use crate::phys::ChunkLookup;

/// Expansions a search will make when the caller does not say.
pub const DEFAULT_BUDGET: u32 = 2_000;

/// The most expansions a caller may ask for in one search.
///
/// A ceiling on what one Lua call can cost. Measured on the reference machine
/// (`benches/perception.rs`), ten thousand expansions is about **2.7 ms — five
/// per cent of a whole tick, for one call from one mob**. A mod that wants more
/// than that is a mod that should be searching less often instead.
pub const MAX_BUDGET: u32 = 10_000;

/// Expansions every search in one tick may spend between them.
///
/// **The one that actually protects the server.** [`MAX_BUDGET`] bounds a single
/// call, which does nothing about two hundred mobs each making one: at the
/// per-call ceiling that would be five hundred milliseconds of pathfinding in a
/// fifty millisecond tick. This is a pool the whole tick draws from, so the cost
/// of navigation is a property of the engine rather than of how carefully every
/// installed mod was written.
///
/// Eight thousand expansions is about 2.2 ms, or 4.4% of a tick — in the same
/// range as the caps lighting and chunk streaming already take. A tick that
/// exhausts it does not fail: later searches get [`Route::Exhausted`], which is
/// an answer a mob already has to handle, and the pool refills next tick.
pub const TICK_BUDGET: u32 = 8_000;

/// How far apart the ends may be before a search is refused outright.
///
/// Cheap insurance in front of the budget: a goal on the other side of the world
/// exhausts any budget you give it, and saying so immediately is kinder than
/// spending the whole allowance discovering it.
pub const MAX_SPAN_BLOCKS: i32 = 192;

/// What the mob walking the path can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Expansions before the search gives up. Clamped to [`MAX_BUDGET`].
    pub budget: u32,
    /// How many blocks of clear space the body needs above its feet.
    ///
    /// Two for anything humanoid. The engine does not read this off the
    /// entity's collider on purpose: a mod may want a mob to path only where it
    /// could also fit while crouching, or to reserve headroom it does not
    /// strictly need.
    pub height: i32,
    /// How far it can climb in one move, in blocks.
    pub step_up: i32,
    /// How far it will drop in one move, in blocks.
    ///
    /// Nothing here knows about fall damage — that is a mod's rule (charter
    /// rule 1) — so this is only how far the search is willing to route.
    pub max_drop: i32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            height: 2,
            step_up: 1,
            max_drop: 3,
        }
    }
}

impl Options {
    /// The budget this actually gets, whatever was asked for.
    #[must_use]
    pub const fn allowance(&self) -> u32 {
        if self.budget == 0 {
            DEFAULT_BUDGET
        } else if self.budget > MAX_BUDGET {
            MAX_BUDGET
        } else {
            self.budget
        }
    }
}

/// How a search ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Blocks to walk, start first and goal last.
    Found(Vec<BlockPos>),
    /// Everything reachable was searched and the goal was not in it — or the
    /// goal is not somewhere this body could stand, or the two ends are more
    /// than [`MAX_SPAN_BLOCKS`] apart.
    Unreachable,
    /// The budget ran out first. **Not the same as unreachable**: a bigger
    /// budget, or a nearer goal, might succeed. A mob told this should pick a
    /// closer target rather than conclude there is no way.
    Exhausted,
    /// There was no world to search. See [`crate::sight::Sighting::Unavailable`],
    /// which this mirrors exactly and for the same reason.
    Unavailable,
}

/// The seam a mod reaches pathfinding through.
///
/// Separate from [`crate::sight::Access`] because they are separate questions,
/// and implemented by the same handle on the server because they need the same
/// thing: the world, during the part of the tick that runs mod callbacks.
pub trait Access: Send + Sync {
    /// A route between two world points, in world blocks.
    fn find_path(&self, from: [f64; 3], to: [f64; 3], options: Options) -> Route;
}

/// Searches for a walkable route between two blocks.
///
/// The start does not have to be standable — a mob may be mid-fall or wedged in
/// geometry, and refusing to look would leave it stuck for ever. The **goal**
/// does: a route to somewhere the body could not stand is not a route.
#[must_use]
pub fn search(chunks: &impl ChunkLookup, from: BlockPos, to: BlockPos, options: &Options) -> Route {
    search_counted(chunks, from, to, options).0
}

/// The same search, and what it cost.
///
/// The count is expansions, which is the unit [`Options::budget`] is in and the
/// one a caller drawing on a shared pool has to subtract. Separate from
/// [`search`] because almost nobody needs it and a tuple at every call site
/// would be noise.
#[must_use]
pub fn search_counted(
    chunks: &impl ChunkLookup,
    from: BlockPos,
    to: BlockPos,
    options: &Options,
) -> (Route, u32) {
    if !from.in_world() || !to.in_world() {
        return (Route::Unreachable, 0);
    }
    if (from.x - to.x).abs() > MAX_SPAN_BLOCKS
        || (from.y - to.y).abs() > MAX_SPAN_BLOCKS
        || (from.z - to.z).abs() > MAX_SPAN_BLOCKS
    {
        return (Route::Unreachable, 0);
    }

    let world = Terrain {
        chunks,
        height: options.height.max(1),
        known: std::cell::RefCell::new(BTreeMap::new()),
        footing: std::cell::RefCell::new(BTreeMap::new()),
    };

    if from == to {
        return (Route::Found(vec![from]), 0);
    }
    if !world.standable(to) {
        return (Route::Unreachable, 0);
    }

    // `Reverse` so the heap pops the cheapest. The tuple's tail is the position
    // itself, which is what makes the order total: two nodes with the same
    // estimate must be expanded in an order that is a property of the graph
    // rather than of whichever the heap happened to hold (charter rule 4).
    let mut frontier: BinaryHeap<Reverse<(u32, u32, BlockPos)>> = BinaryHeap::new();
    let mut cost: BTreeMap<BlockPos, u32> = BTreeMap::new();
    let mut came_from: BTreeMap<BlockPos, BlockPos> = BTreeMap::new();

    frontier.push(Reverse((heuristic(from, to), 0, from)));
    cost.insert(from, 0);

    let allowance = options.allowance();
    let mut expanded = 0u32;

    // Built once. It is the same four-to-nine values for every expansion, and
    // rebuilding and re-sorting it inside the loop was measurably the most
    // expensive thing the search did.
    let mut heights: Vec<i32> = (-options.max_drop.max(0)..=options.step_up.max(0)).collect();
    heights.sort_by_key(|dy| (dy.abs(), -*dy));

    while let Some(Reverse((_, spent, here))) = frontier.pop() {
        if here == to {
            return (Route::Found(retrace(&came_from, from, to)), expanded);
        }
        // A stale heap entry: this node was reached more cheaply after it was
        // pushed. Skipping rather than removing is the usual way, and it costs
        // a comparison instead of a heap rebuild.
        if cost.get(&here).is_some_and(|best| *best < spent) {
            continue;
        }

        expanded += 1;
        if expanded > allowance {
            return (Route::Exhausted, expanded);
        }

        for (next, step) in world.steps(here, &heights).into_iter().flatten() {
            let so_far = spent + step;
            if cost.get(&next).is_some_and(|best| *best <= so_far) {
                continue;
            }
            cost.insert(next, so_far);
            came_from.insert(next, here);
            frontier.push(Reverse((so_far + heuristic(next, to), so_far, next)));
        }
    }

    (Route::Unreachable, expanded)
}

/// Blocks between two points, ignoring height.
///
/// Admissible because every move changes exactly one of x and z by exactly one
/// and costs at least one, so this can never overestimate — which is what makes
/// the first route A* finds the shortest one.
const fn heuristic(from: BlockPos, to: BlockPos) -> u32 {
    ((from.x - to.x).abs() + (from.z - to.z).abs()) as u32
}

/// Walks the `came_from` chain back and turns it the right way round.
fn retrace(
    came_from: &BTreeMap<BlockPos, BlockPos>,
    from: BlockPos,
    to: BlockPos,
) -> Vec<BlockPos> {
    let mut route = vec![to];
    let mut here = to;
    while here != from {
        let Some(&before) = came_from.get(&here) else {
            break;
        };
        route.push(before);
        here = before;
    }
    route.reverse();
    route
}

/// The nine sub-node cells on a block's floor, as a mask over [`BlockView`]'s
/// occupancy bits.
///
/// Sub-Node Contract §6 names exactly these: the cells at block-local `y == 0`.
/// Indices are `x + 3y + 9z`, so with `y` zero they are `x + 9z` — three runs of
/// three, nine apart. Checked against `subnode_index` in this module's tests
/// rather than trusted, because a mask written from the formula by hand is a
/// silent way to make a mob walk through walls.
const FLOOR_CELLS: u32 = 0b111 | (0b111 << 9) | (0b111 << 18);

/// The world, answering only the two questions a search asks of it.
///
/// # The memo is not an optimisation to take or leave
///
/// Passability is nine cell lookups, and a search asks it for the same block
/// again and again: once for each of the four neighbours that can reach it, once
/// more for every height they try, and again for the headroom of everything
/// standing under it. Measured without the memo, a search that spends its
/// default budget cost **3.7 ms — seven per cent of a whole tick, for one call
/// from one mob**. With it, the nine-cell test happens once per block for the
/// life of the search.
///
/// A `BTreeMap` and not a `HashMap`: nothing here iterates it, so the ordering
/// is not what charter rule 4 is worried about, but a random-seeded hasher in
/// simulation code is a habit worth not having.
struct Terrain<'a, S: ChunkLookup> {
    chunks: &'a S,
    height: i32,
    /// Blocks already tested for passability, and what they answered.
    known: std::cell::RefCell<BTreeMap<BlockPos, bool>>,
    /// The same for standability, which `steps` asks about twenty times per
    /// expansion and which neighbouring expansions ask about again.
    footing: std::cell::RefCell<BTreeMap<BlockPos, bool>>,
}

impl<S: ChunkLookup> Terrain<'_, S> {
    /// Contract §6: a block is walked through iff its bottom sub-node layer —
    /// the nine cells at local `y == 0` — is empty.
    ///
    /// The bottom layer and not the whole block, which is the part that is easy
    /// to get wrong: a block with a lamp hanging from its ceiling is somewhere a
    /// body walks, and one with a single cell of stone on its floor is not.
    fn passable(&self, block: BlockPos) -> bool {
        if let Some(&known) = self.known.borrow().get(&block) {
            return known;
        }
        let answer = self.test_passable(block);
        self.known.borrow_mut().insert(block, answer);
        answer
    }

    /// The floor test itself, run once per block per search.
    ///
    /// # One block read, not nine cell reads
    ///
    /// Asking [`crate::phys::Solid`] cell by cell is the obvious way and it was
    /// measurably the whole cost of a search: nine chunk lookups and nine
    /// palette decodes per block, for three blocks per standability test, for
    /// twenty tests per expansion. A block already knows its own shape as a
    /// 27-bit occupancy mask, so the answer is one lookup and one `&`.
    ///
    /// **An absent chunk is not passable**, which is the charter-wide rule that
    /// absence reads as solid — a mob does not plan a route through terrain the
    /// server has not generated.
    fn test_passable(&self, block: BlockPos) -> bool {
        let Some(chunk) = self.chunks.chunk(block.chunk()) else {
            return false;
        };
        match chunk.get_block(block) {
            None => false,
            Some(BlockView::Uniform(material)) => material.is_air(),
            Some(BlockView::Partial {
                material,
                occupancy,
            }) => material.is_air() || occupancy & FLOOR_CELLS == 0,
            // Two or more materials, so there is no single mask to test and the
            // cells have to be read. The rare form, and the only one that pays
            // for itself.
            Some(BlockView::Mixed(cells)) => (0..crate::SUBNODES_PER_AXIS)
                .flat_map(|z| (0..crate::SUBNODES_PER_AXIS).map(move |x| (x, z)))
                .all(|(x, z)| cells[crate::block::subnode_index(x, 0, z)].is_air()),
        }
    }

    /// Whether a body with its feet in this block would stand there.
    ///
    /// Headroom above and something underneath. An unloaded chunk reads as solid
    /// (charter-wide), so this answers `false` beyond what the server holds —
    /// a mob does not get to plan a route through terrain nobody has generated.
    fn standable(&self, block: BlockPos) -> bool {
        if let Some(&known) = self.footing.borrow().get(&block) {
            return known;
        }
        let answer = self.test_standable(block);
        self.footing.borrow_mut().insert(block, answer);
        answer
    }

    /// The headroom-and-floor test itself, run once per block per search.
    fn test_standable(&self, block: BlockPos) -> bool {
        if !block.in_world() {
            return false;
        }
        for up in 0..self.height {
            if !self.passable(BlockPos::new(block.x, block.y + up, block.z)) {
                return false;
            }
        }
        !self.passable(BlockPos::new(block.x, block.y - 1, block.z))
    }

    /// Where a body standing here could step next, and what each move costs.
    ///
    /// Four lateral directions, each tried at every reachable height in
    /// `heights` — which the search builds once, nearest-to-level first, so the
    /// `break` below takes the nearest. Iterating from the top down instead
    /// would make a mob climb a step it could have walked around, and only on
    /// ground where both were possible: the kind of thing that looks like a
    /// physics bug rather than a search one.
    ///
    /// A fixed array rather than a `Vec`, because this is the search's inner
    /// loop and there are never more than four answers. Allocating here cost
    /// more than the terrain tests did.
    fn steps(&self, from: BlockPos, heights: &[i32]) -> [Option<(BlockPos, u32)>; 4] {
        const LATERAL: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

        let mut out = [None; 4];
        for (slot, (dx, dz)) in out.iter_mut().zip(LATERAL) {
            for dy in heights.iter().copied() {
                let next = BlockPos::new(from.x + dx, from.y + dy, from.z + dz);
                if !self.standable(next) {
                    continue;
                }
                // Climbing needs the space above the block being left, or the
                // body would be walking through the ceiling it is stepping
                // under. Nothing checks this on the way down, because falling
                // through the floor you were standing on is not a thing.
                if dy > 0 && !self.passable(BlockPos::new(from.x, from.y + self.height, from.z)) {
                    continue;
                }
                // One per move, plus the climb or drop. Level ground is
                // therefore preferred over a route of the same length that goes
                // over a hill, which is what makes a path look walked rather
                // than solved.
                *slot = Some((next, 1 + dy.unsigned_abs()));
                // The nearest standable height in this direction is the only one
                // worth taking: the others are the same lateral move to a place
                // further away.
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::block::BlockValue;
    use crate::chunk::Chunk;
    use crate::coords::{ChunkPos, SubNodePos};
    use crate::material::MaterialId;

    const STONE: MaterialId = MaterialId(2);

    /// A world a test builds block by block. Chunks that were never touched are
    /// absent, and absence reads as solid — which is half of what is under test.
    #[derive(Default)]
    struct Loaded(BTreeMap<ChunkPos, Chunk>);

    impl Loaded {
        fn air(mut self, pos: ChunkPos) -> Self {
            self.0.insert(pos, Chunk::new(pos, MaterialId::AIR));
            self
        }

        fn solid(mut self, block: BlockPos) -> Self {
            let pos = block.chunk();
            let chunk = self
                .0
                .entry(pos)
                .or_insert_with(|| Chunk::new(pos, MaterialId::AIR));
            chunk
                .set_block(block, BlockValue::Uniform(STONE))
                .expect("the block is in the chunk it names");
            self
        }

        /// A flat floor across one block layer of the chunk at the origin.
        fn floor(mut self, y: i32, from: i32, to: i32) -> Self {
            for z in from..=to {
                for x in from..=to {
                    self = self.solid(BlockPos::new(x, y, z));
                }
            }
            self
        }

        /// One sub-node cell, for the partial-block cases §6 is about.
        fn cell(mut self, cell: SubNodePos) -> Self {
            let pos = cell.block().chunk();
            let chunk = self
                .0
                .entry(pos)
                .or_insert_with(|| Chunk::new(pos, MaterialId::AIR));
            chunk
                .set_subnode(cell, STONE)
                .expect("the cell is in range");
            self
        }
    }

    impl ChunkLookup for Loaded {
        fn chunk(&self, pos: ChunkPos) -> Option<&Chunk> {
            self.0.get(&pos)
        }
    }

    /// A floor at y=0 across the WHOLE origin chunk, so feet go at y=1.
    ///
    /// The whole chunk and not part of it, which took a run of confusing
    /// failures to learn: a floor stopping short leaves the chunk's edge open,
    /// and a body that walks off it stands on the roof of the absent chunk
    /// below — because absence reads as solid. Every route then escaped round
    /// the outside of the wall it was supposed to be stopped by. A full floor
    /// makes the chunk a sixteen-block room with solid walls on all sides.
    fn field() -> Loaded {
        Loaded::default()
            .air(ChunkPos::new(0, 0, 0))
            .floor(0, 0, 15)
    }

    fn found(route: Route) -> Vec<BlockPos> {
        match route {
            Route::Found(blocks) => blocks,
            other => panic!("expected a route, got {other:?}"),
        }
    }

    #[test]
    fn the_floor_mask_is_the_floor() {
        // The mask is written as a literal for speed, so it is checked against
        // the canonical index formula here. A mask one bit out would make some
        // chiselled blocks walkable that are not, and nothing else in the suite
        // would notice.
        let mut expected = 0u32;
        for z in 0..crate::SUBNODES_PER_AXIS {
            for x in 0..crate::SUBNODES_PER_AXIS {
                expected |= 1 << crate::block::subnode_index(x, 0, z);
            }
        }
        assert_eq!(FLOOR_CELLS, expected);
    }

    #[test]
    fn a_straight_walk_is_the_straight_walk() {
        let world = field();
        let route = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(5, 1, 1),
            &Options::default(),
        ));
        assert_eq!(route.first(), Some(&BlockPos::new(1, 1, 1)));
        assert_eq!(route.last(), Some(&BlockPos::new(5, 1, 1)));
        assert_eq!(route.len(), 5, "the shortest way is four steps: {route:?}");
    }

    #[test]
    fn a_wall_is_walked_around() {
        // A wall across z, with a gap at z=4, so the only way through is the
        // gap — and the route must be longer than the straight line.
        let mut world = field();
        for z in 0..=15 {
            if z == 4 {
                continue;
            }
            world = world
                .solid(BlockPos::new(3, 1, z))
                .solid(BlockPos::new(3, 2, z));
        }
        let route = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(5, 1, 1),
            &Options::default(),
        ));
        assert!(
            route.iter().any(|step| step.z >= 4),
            "the route did not use the only gap: {route:?}"
        );
        assert!(route.len() > 5, "the route walked through the wall");
    }

    #[test]
    fn a_wall_with_no_gap_is_unreachable() {
        let mut world = field();
        for z in 0..=15 {
            world = world
                .solid(BlockPos::new(3, 1, z))
                .solid(BlockPos::new(3, 2, z));
        }
        assert_eq!(
            search(
                &world,
                BlockPos::new(1, 1, 1),
                BlockPos::new(5, 1, 1),
                &Options::default()
            ),
            Route::Unreachable
        );
    }

    #[test]
    fn a_step_up_is_climbed_and_a_wall_is_not() {
        // One block high: walkable with the default step_up of 1.
        let world = field().solid(BlockPos::new(3, 1, 1));
        let route = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(5, 1, 1),
            &Options::default(),
        ));
        assert!(
            route.contains(&BlockPos::new(3, 2, 1)),
            "the step was not climbed: {route:?}"
        );

        // The same shape with the climb forbidden has to go round, and there is
        // nowhere to go round to if the step spans the field.
        let mut walled = field();
        for z in 0..=15 {
            walled = walled.solid(BlockPos::new(3, 1, z));
        }
        assert_eq!(
            search(
                &walled,
                BlockPos::new(1, 1, 1),
                BlockPos::new(5, 1, 1),
                &Options {
                    step_up: 0,
                    ..Options::default()
                }
            ),
            Route::Unreachable,
            "a body that cannot step up walked up anyway"
        );
    }

    #[test]
    fn a_drop_further_than_allowed_is_not_taken() {
        // A ledge at y=4 over the floor at y=0. Reaching the floor means
        // dropping three, which the default allows and a limit of one does not.
        let mut world = field();
        for z in 0..=15 {
            for x in 0..=2 {
                world = world.solid(BlockPos::new(x, 3, z));
            }
        }
        let from = BlockPos::new(1, 4, 1);
        let to = BlockPos::new(6, 1, 1);

        assert!(matches!(
            search(&world, from, to, &Options::default()),
            Route::Found(_)
        ));
        assert_eq!(
            search(
                &world,
                from,
                to,
                &Options {
                    max_drop: 1,
                    ..Options::default()
                }
            ),
            Route::Unreachable
        );
    }

    #[test]
    fn a_low_ceiling_stops_a_tall_body_and_not_a_short_one() {
        // A ceiling one block above the floor along the whole width, leaving a
        // single block of headroom.
        let mut world = field();
        for z in 0..=15 {
            world = world.solid(BlockPos::new(3, 2, z));
        }
        let from = BlockPos::new(1, 1, 1);
        let to = BlockPos::new(5, 1, 1);

        assert_eq!(
            search(&world, from, to, &Options::default()),
            Route::Unreachable,
            "a two-block body fitted through one block of headroom"
        );
        assert!(matches!(
            search(
                &world,
                from,
                to,
                &Options {
                    height: 1,
                    ..Options::default()
                }
            ),
            Route::Found(_)
        ));
    }

    #[test]
    fn a_block_whose_floor_is_empty_is_walked_through() {
        // Sub-Node Contract §6, the whole rule: the bottom sub-node layer is
        // what decides. A block with material only in its top cells is walked
        // through, and the same block with one cell on its floor is not.
        let mut world = field();
        for dz in 0..3 {
            for dx in 0..3 {
                world = world.cell(BlockPos::new(3, 1, 1).subnode(dx, 2, dz));
            }
        }
        let from = BlockPos::new(1, 1, 1);
        let to = BlockPos::new(5, 1, 1);
        assert_eq!(
            found(search(&world, from, to, &Options::default())).len(),
            5,
            "a block with a clear floor blocked the way"
        );

        let floored = world.cell(BlockPos::new(3, 1, 1).subnode(1, 0, 1));
        let route = found(search(&floored, from, to, &Options::default()));
        assert!(
            !route.contains(&BlockPos::new(3, 1, 1)),
            "one cell on the floor did not make the block an obstacle: {route:?}"
        );
    }

    #[test]
    fn a_budget_that_runs_out_says_so_rather_than_lying() {
        // Exhausted is not Unreachable, and a mod told the wrong one either
        // gives up on a place it could reach or keeps asking about one it
        // cannot. A wall with no gap, searched with almost no budget.
        let mut world = field();
        for z in 0..=15 {
            world = world
                .solid(BlockPos::new(3, 1, z))
                .solid(BlockPos::new(3, 2, z));
        }
        assert_eq!(
            search(
                &world,
                BlockPos::new(1, 1, 1),
                BlockPos::new(5, 1, 1),
                &Options {
                    budget: 3,
                    ..Options::default()
                }
            ),
            Route::Exhausted
        );
    }

    #[test]
    fn an_absurd_budget_is_clamped_rather_than_honoured() {
        assert_eq!(
            Options {
                budget: u32::MAX,
                ..Options::default()
            }
            .allowance(),
            MAX_BUDGET
        );
        assert_eq!(
            Options {
                budget: 0,
                ..Options::default()
            }
            .allowance(),
            DEFAULT_BUDGET
        );
    }

    #[test]
    fn a_goal_nobody_could_stand_in_is_unreachable() {
        // Inside the floor, and in mid-air. Neither is a route.
        let world = field();
        let from = BlockPos::new(1, 1, 1);
        assert_eq!(
            search(&world, from, BlockPos::new(5, 0, 1), &Options::default()),
            Route::Unreachable,
            "a route was found into solid ground"
        );
        assert_eq!(
            search(&world, from, BlockPos::new(5, 6, 1), &Options::default()),
            Route::Unreachable,
            "a route was found into thin air"
        );
    }

    #[test]
    fn a_route_does_not_leave_what_is_loaded() {
        // The room is one chunk wide and the next chunk is absent. Absence
        // reads as solid, so a goal beyond it is unreachable rather than a
        // route through terrain the server has not generated.
        let world = field();
        assert_eq!(
            search(
                &world,
                BlockPos::new(1, 1, 1),
                BlockPos::new(20, 1, 1),
                &Options::default()
            ),
            Route::Unreachable
        );
    }

    #[test]
    fn the_far_side_of_the_world_is_refused_before_it_is_searched() {
        let world = field();
        assert_eq!(
            search(
                &world,
                BlockPos::new(1, 1, 1),
                BlockPos::new(MAX_SPAN_BLOCKS + 2, 1, 1),
                &Options::default()
            ),
            Route::Unreachable
        );
    }

    #[test]
    fn the_same_graph_is_searched_the_same_way_twice() {
        // Charter rule 4. The search is all integers, so the risk is not
        // arithmetic — it is the frontier's tie-breaking, which is why the heap
        // orders on position and the maps are `BTreeMap`s. Two runs over an
        // open field, where ties are everywhere, must agree exactly.
        let world = field();
        let once = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(9, 1, 9),
            &Options::default(),
        ));
        let again = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(9, 1, 9),
            &Options::default(),
        ));
        assert_eq!(once, again);
    }

    #[test]
    fn every_step_of_a_route_is_somewhere_the_body_could_be() {
        // The property that matters to a mob walking it: no step teleports, and
        // every one is standable. A route that satisfied the goal test and
        // nothing else would still be a route the mimic falls off.
        let world = field().solid(BlockPos::new(4, 1, 4));
        let route = found(search(
            &world,
            BlockPos::new(1, 1, 1),
            BlockPos::new(8, 1, 8),
            &Options::default(),
        ));
        let options = Options::default();
        let terrain = Terrain {
            chunks: &world,
            height: options.height,
            known: std::cell::RefCell::new(BTreeMap::new()),
            footing: std::cell::RefCell::new(BTreeMap::new()),
        };
        for pair in route.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let lateral = (a.x - b.x).abs() + (a.z - b.z).abs();
            assert_eq!(lateral, 1, "a step moved {lateral} blocks sideways");
            assert!(
                (a.y - b.y).abs() <= options.step_up.max(options.max_drop),
                "a step changed height by more than allowed: {a:?} -> {b:?}"
            );
            assert!(terrain.standable(b), "a step landed somewhere unstandable");
        }
    }
}
