// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deciding whether material may be placed, and what geometry it makes.
//!
//! The sibling of [`crate::dig`]. Digging turns geometry into units; this turns
//! units back into geometry, and the two are held to the same conservation law
//! by [`crate::inventory`].
//!
//! # Everything here is a refusal, not a correction
//!
//! A placement that cannot happen is refused and the player is told why. None
//! of these cases silently place something smaller, somewhere else, or made of
//! a different material — a build that quietly did something other than what
//! was asked is worse than one that did nothing, because the player finds out
//! later and somewhere else.
//!
//! # The brush decides, exactly as it does for digging
//!
//! [`crate::dig::Brush`] says what a tool addresses, and placement honours the
//! same answer digging does. **This module implements Sub-Node Contract §7.1**,
//! which is where the rules below are authoritative.
//!
//! - [`Brush::Block`] writes a whole block, filled from the bottom up by
//!   [`crate::inventory::placement_mask`]. A player holding fewer than 27 units
//!   gets a `Partial` rather than a refusal — that is what "spare-node
//!   placement" means, and it is what the chisel scenario ends with. The cell
//!   the client names selects the *block* and not which of its cells get
//!   filled: the fill order is fixed so that it cannot depend on where a player
//!   happened to be looking.
//! - [`Brush::SubNode`] writes exactly the cell under the crosshair, one unit.
//!
//! # Why a sub-node brush places into the aimed cell
//!
//! Because otherwise this engine can carve detail and never build it back. A
//! chisel takes one cell out of a block; if putting one back always dropped it
//! to the bottom of the containing block, sculpting would be a one-way
//! operation and the sub-node resolution the whole engine is built around would
//! exist only for removal.
//!
//! This is a deliberate reversal of the original rule, which fixed the fill
//! order for *every* placement so that identical actions produced identical
//! geometry. That property is worth keeping where it costs nothing, so it still
//! holds for [`Brush::Block`] — where a player is asking for "a block here" and
//! not for a shape. A sub-node brush is a request for a *particular cell*, and
//! answering it with a different cell is the surprise, not the precision.

use crate::block::subnode_offset;
use crate::coords::{BlockPos, ChunkPos, SubNodePos};
use crate::detgen::floor_to_i64;
use crate::dig::Brush;
use crate::phys::Aabb;
use crate::{CHUNK_SUBNODES, SUBNODES_PER_AXIS, UNITS_PER_BLOCK};

/// Why a placement did not happen.
///
/// Carried back to the player. Charter rule 2 puts the decision on the server,
/// which means the client cannot work out the reason for itself and has to be
/// told one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The player holds none of that material.
    #[error("you are not carrying any of that")]
    NothingHeld,

    /// Something already fills a cell the placement would write.
    ///
    /// Judged per *cell*, not per block: a chiselled block has empty cells and
    /// a sub-node brush is allowed to fill them, which is the other half of
    /// making sculpting reversible. A block brush aimed at a block with
    /// anything in it still lands here, because the cells it fills overlap.
    #[error("there is already something there")]
    Occupied,

    /// The geometry would be inside a player.
    #[error("someone is standing there")]
    InsideAPlayer,
}

/// What a placement would write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// The block to write.
    pub block: BlockPos,
    /// Which of its cells get filled.
    pub occupancy: u32,
    /// Units taken from the player, which is `occupancy.count_ones()`.
    pub units: u32,
}

/// Works out what placing into `target` with `brush` would write, given what is
/// held.
///
/// Does not consult the world — every cell in [`Placement::occupancy`] still
/// has to be checked empty, and the geometry still has to be checked against
/// every body. Split that way because the arithmetic is worth testing on its
/// own, and because the two world checks need a world and a body list that this
/// crate's tests would otherwise have to fabricate to test 27 lines of bit
/// twiddling.
///
/// # Errors
///
/// [`Refusal::NothingHeld`] if `held` is zero.
pub fn plan(
    target: SubNodePos,
    held: u32,
    shape: Option<crate::inventory::Shape>,
    brush: Brush,
) -> Result<Placement, Refusal> {
    if held == 0 {
        return Err(Refusal::NothingHeld);
    }
    // **A shaped stack places its shape, whatever the tool.** The cut IS the
    // thing being placed, so a chisel does not get to take one cell out of a
    // stair — that would spend a whole stair's worth of somebody's work to put
    // down one cell of stone. A tool changes what you take OUT of the world;
    // what you put back is what you are holding.
    if let Some(shape) = shape {
        if held < shape.cells() {
            return Err(Refusal::NothingHeld);
        }
        return Ok(Placement {
            block: target.block(),
            occupancy: shape.occupancy(),
            units: shape.cells(),
        });
    }
    match brush {
        Brush::SubNode => Ok(Placement {
            block: target.block(),
            occupancy: 1 << cell_index(target),
            units: 1,
        }),
        Brush::Block => {
            let units = held.min(UNITS_PER_BLOCK);
            Ok(Placement {
                block: target.block(),
                occupancy: crate::inventory::placement_mask(units),
                units,
            })
        }
    }
}

/// Which of its block's 27 cells a world cell is.
///
/// `rem_euclid` rather than `%`: at negative coordinates the remainder is
/// negative and indexes the wrong cell — the same trap [`SubNodePos::block`]
/// documents, and it only ever shows up west or below the origin.
fn cell_index(target: SubNodePos) -> usize {
    let axis = |v: i32| v.rem_euclid(SUBNODES_PER_AXIS as i32) as u32;
    crate::block::subnode_index(axis(target.x), axis(target.y), axis(target.z))
}

/// The world cells a placement would fill.
///
/// World cells rather than block-local ones because the only thing that reads
/// this compares them against bodies, and bodies are somewhere else in the
/// world.
pub fn occupied_cells(placement: &Placement) -> impl Iterator<Item = [i64; 3]> + '_ {
    let base = [
        i64::from(placement.block.x) * i64::from(SUBNODES_PER_AXIS),
        i64::from(placement.block.y) * i64::from(SUBNODES_PER_AXIS),
        i64::from(placement.block.z) * i64::from(SUBNODES_PER_AXIS),
    ];
    (0..UNITS_PER_BLOCK as usize)
        .filter(move |index| placement.occupancy & (1 << index) != 0)
        .map(move |index| {
            let (x, y, z) = subnode_offset(index);
            [
                base[0] + i64::from(x),
                base[1] + i64::from(y),
                base[2] + i64::from(z),
            ]
        })
}

/// Whether a placement would put geometry inside a body.
///
/// # Why this is in `i64` world cells
///
/// The two things being compared live in different frames: a block position is
/// absolute, and a body is a chunk origin plus an offset inside it (charter
/// rule 7). Something has to bring them together, and doing it in `f32` world
/// space is exactly what rule 7 forbids — at the edge of a 120,000-block world
/// the cell coordinates run to 360,000, where an `f32` ulp is about 1/20 of a
/// cell. A placement check that was 5 cm out at the edge of the world and exact
/// at the origin would be a bug that only ever reproduced for people who had
/// travelled.
///
/// So the block's cells are exact `i64`, and each body's span is widened to the
/// cells it touches before the comparison. `f64` holds a chunk origin times 48
/// exactly to well beyond the world's size, and `floor` is in charter rule 4's
/// allowed subset — this runs inside the tick and has to be deterministic.
#[must_use]
pub fn blocks_a_body(placement: &Placement, bodies: &[(ChunkPos, Aabb)]) -> bool {
    let spans: Vec<([i64; 3], [i64; 3])> = bodies
        .iter()
        .map(|(origin, aabb)| {
            let span = |axis: usize| {
                let offset = f64::from(match axis {
                    0 => origin.x,
                    1 => origin.y,
                    _ => origin.z,
                }) * f64::from(CHUNK_SUBNODES);
                let low = offset + f64::from(aabb.min[axis]);
                let high = offset + f64::from(aabb.max[axis]);
                // The high end is exclusive: a body whose face lies exactly on a
                // cell boundary is resting against that cell, not inside it, and
                // treating it as inside would make it impossible to place a block
                // against the ground a player is standing on.
                (floor_to_i64(low), last_cell_touched(high))
            };
            let (x0, x1) = span(0);
            let (y0, y1) = span(1);
            let (z0, z1) = span(2);
            ([x0, y0, z0], [x1, y1, z1])
        })
        .collect();

    occupied_cells(placement).any(|cell| {
        spans
            .iter()
            .any(|(min, max)| (0..3).all(|axis| cell[axis] >= min[axis] && cell[axis] <= max[axis]))
    })
}

/// How much further than [`crate::phys::REACH`] the server allows.
///
/// **The server cannot use the client's own number.** The client raycasts from
/// where IT thinks the player is, which is up to `INPUT_LEAD` ticks ahead of
/// where the server has them — that is the whole design of prediction. Checking
/// the exact reach against the server's older position would refuse legitimate
/// actions whenever the player was moving, and the faster they moved the more
/// often it would happen.
///
/// Four ticks of sprinting is about 3.4 cells, and the body is 1.8 across. Five
/// cells covers both with room to spare. This is a bound on absurdity — no
/// mining across the map — rather than a precise gate, and erring generous is
/// correct: refusing something a player legitimately did is a bug they will
/// report, while allowing a dig half a metre further than intended is not.
pub const REACH_TOLERANCE: f32 = 5.0;

/// Whether `target` is close enough to a player's eye to act on.
///
/// Measured in world cells with the same `i64`/`f64` care as
/// [`blocks_a_body`], and for the same reason: the eye is a chunk origin plus a
/// local offset (charter rule 7) while the target is absolute, so bringing them
/// together in `f32` world space would be exact at the origin and metres out at
/// the edge of the world.
///
/// Compares squared distances, so nothing needs a square root — `sqrt` is in
/// charter rule 4's allowed subset, but not needing it is cheaper and exact.
#[must_use]
pub fn within_reach(origin: ChunkPos, eye: [f32; 3], target: SubNodePos) -> bool {
    let span = f64::from(CHUNK_SUBNODES);
    let eye_world = [
        f64::from(origin.x) * span + f64::from(eye[0]),
        f64::from(origin.y) * span + f64::from(eye[1]),
        f64::from(origin.z) * span + f64::from(eye[2]),
    ];
    // The cell's centre, not its corner: a player looking at the far face of a
    // cell they are just touching should not be refused by half a cell.
    let target_world = [
        f64::from(target.x) + 0.5,
        f64::from(target.y) + 0.5,
        f64::from(target.z) + 0.5,
    ];

    let mut squared = 0.0;
    for axis in 0..3 {
        let delta = target_world[axis] - eye_world[axis];
        squared += delta * delta;
    }
    let limit = f64::from(crate::phys::REACH + REACH_TOLERANCE);
    squared <= limit * limit
}

/// The last cell index a span reaching `high` touches.
///
/// One less than the floor when `high` sits exactly on a boundary, which is the
/// "resting against, not inside" case.
#[expect(
    clippy::float_cmp,
    reason = "exact equality IS the question: a body's face landing precisely on a cell \
              boundary is the case being distinguished, and a tolerance would make it \
              arbitrary how close counts as touching — which would then differ between a \
              body at the origin and one at the edge of the world"
)]
fn last_cell_touched(high: f64) -> i64 {
    let floored = floor_to_i64(high);
    if (floored as f64) == high {
        floored - 1
    } else {
        floored
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_shaped_stack_places_its_shape_and_costs_its_cells() {
        use crate::inventory::Shape;

        let shape = Shape::new(0b1_0101).expect("three cells");
        let placed = plan(cell(0, 0, 0), 100, Some(shape), Brush::Block).expect("held plenty");
        assert_eq!(
            placed.occupancy,
            shape.occupancy(),
            "the block should be written in the shape that was held"
        );
        assert_eq!(
            placed.units,
            shape.cells(),
            "a shape costs its cells and nothing else"
        );

        // **A chisel does not take one cell out of a stair.** The cut IS the
        // thing being placed, so a tool changes what you take out of the world
        // and not what you put back — otherwise a whole stair's worth of
        // somebody's work buys one cell of stone.
        let chiselled = plan(cell(1, 1, 1), 100, Some(shape), Brush::SubNode).expect("held");
        assert_eq!(chiselled.occupancy, shape.occupancy());
        assert_eq!(chiselled.units, shape.cells());
    }

    #[test]
    fn a_shape_needs_all_of_itself_to_be_placed() {
        use crate::inventory::Shape;

        // Half a stair is not a stair. Placing what is left of one would have
        // to decide which half, and there is no answer to that.
        let shape = Shape::new(0b1_1111).expect("five cells");
        assert!(matches!(
            plan(cell(0, 0, 0), shape.cells() - 1, Some(shape), Brush::Block),
            Err(Refusal::NothingHeld)
        ));
        assert!(plan(cell(0, 0, 0), shape.cells(), Some(shape), Brush::Block).is_ok());
    }

    use super::*;
    use crate::phys::PLAYER_HEIGHT;

    fn cell(x: i32, y: i32, z: i32) -> SubNodePos {
        SubNodePos::new(x, y, z)
    }

    #[test]
    fn a_full_block_of_units_places_a_solid_block() {
        let plan =
            plan(cell(0, 0, 0), UNITS_PER_BLOCK, None, Brush::Block).expect("27 units is a block");
        assert_eq!(plan.units, UNITS_PER_BLOCK);
        assert_eq!(plan.occupancy.count_ones(), UNITS_PER_BLOCK);
    }

    #[test]
    fn more_than_a_block_still_places_exactly_one() {
        // The surplus stays in the inventory. Placing two blocks' worth into
        // one block would destroy 27 units.
        let plan =
            plan(cell(0, 0, 0), UNITS_PER_BLOCK * 3 + 4, None, Brush::Block).expect("plenty held");
        assert_eq!(
            plan.units, UNITS_PER_BLOCK,
            "a placement consumed more than the block it filled"
        );
    }

    #[test]
    fn spare_nodes_place_a_partial_rather_than_being_refused() {
        // The behaviour the chisel scenario depends on: 13 units is not a
        // block, and the answer is 13 cells rather than "you cannot".
        let plan =
            plan(cell(0, 0, 0), 13, None, Brush::Block).expect("13 units is a partial block");
        assert_eq!(plan.units, 13);
        assert_eq!(plan.occupancy.count_ones(), 13);
        assert_ne!(
            plan.occupancy,
            (1 << UNITS_PER_BLOCK) - 1,
            "a partial placement filled the whole block"
        );
    }

    #[test]
    fn holding_nothing_is_refused() {
        assert_eq!(
            plan(cell(0, 0, 0), 0, None, Brush::Block),
            Err(Refusal::NothingHeld)
        );
    }

    #[test]
    fn a_subnode_brush_fills_the_cell_that_was_aimed_at() {
        // The reason sub-nodes survive being put back. Each of the 27 cells
        // plans a different placement, and each one is the cell named.
        for x in 0..3 {
            for y in 0..3 {
                for z in 0..3 {
                    let plan = plan(cell(x, y, z), 27, None, Brush::SubNode).expect("held");
                    assert_eq!(plan.units, 1, "a sub-node placement spent more than a unit");
                    assert_eq!(
                        plan.occupancy.count_ones(),
                        1,
                        "a sub-node placement filled more than a cell"
                    );
                    let filled: Vec<[i64; 3]> = occupied_cells(&plan).collect();
                    assert_eq!(
                        filled,
                        vec![[i64::from(x), i64::from(y), i64::from(z)]],
                        "cell {x},{y},{z} was placed somewhere else"
                    );
                }
            }
        }
    }

    #[test]
    fn a_subnode_brush_aims_correctly_west_of_the_origin() {
        // `%` on a negative coordinate is negative, which indexes the wrong
        // cell — or shifts by a negative amount and panics. The same trap
        // `Hit::block` documents, and it only reproduces west of the origin.
        let plan = plan(cell(-1, -1, -1), 27, None, Brush::SubNode).expect("held");
        assert_eq!(plan.block, BlockPos::new(-1, -1, -1));
        let filled: Vec<[i64; 3]> = occupied_cells(&plan).collect();
        assert_eq!(
            filled,
            vec![[-1, -1, -1]],
            "the aimed cell was not the cell filled"
        );
    }

    #[test]
    fn a_subnode_brush_places_one_unit_however_much_is_held() {
        // A chisel is precision, not throughput: holding a hundred units does
        // not fill a hundred cells, and holding one is enough.
        let many = plan(cell(1, 1, 1), 500, None, Brush::SubNode).expect("held");
        let one = plan(cell(1, 1, 1), 1, None, Brush::SubNode).expect("held");
        assert_eq!(many, one, "how much was held changed a sub-node placement");
        assert_eq!(one.units, 1);
    }

    #[test]
    fn a_block_brush_lets_the_cell_select_the_block_and_not_the_fill() {
        // Every cell of a block plans the same placement. The fill order is
        // fixed (bottom-up) so that it cannot depend on where the player was
        // looking, which would make the same action produce different geometry.
        // Deliberately NOT true of a sub-node brush — see the test above.
        let first = plan(cell(0, 0, 0), 5, None, Brush::Block).expect("held");
        for x in 0..3 {
            for y in 0..3 {
                for z in 0..3 {
                    let other = plan(cell(x, y, z), 5, None, Brush::Block).expect("held");
                    assert_eq!(
                        other, first,
                        "cell {x},{y},{z} planned differently from cell 0,0,0"
                    );
                }
            }
        }
    }

    #[test]
    fn something_underfoot_is_within_reach_and_the_horizon_is_not() {
        let origin = ChunkPos::new(0, 0, 0);
        let eye = [24.0, 10.0, 24.0];
        assert!(
            within_reach(origin, eye, cell(24, 5, 24)),
            "the cell a player is standing on must be reachable"
        );
        assert!(
            !within_reach(origin, eye, cell(24, 5, 240)),
            "a cell 200 cells away must not be"
        );
    }

    #[test]
    fn the_reach_check_crosses_chunk_origins() {
        // The eye is a chunk origin plus a local offset (charter rule 7) and
        // the target is absolute. A check that ignored the origin would pass
        // for everyone at spawn and refuse everything for anyone who walked —
        // the same trap `blocks_a_body` has, and worth its own test because the
        // two compute it separately.
        let far = ChunkPos::new(10, 0, 10);
        let eye = [24.0, 10.0, 24.0];
        let underfoot = cell(
            10 * CHUNK_SUBNODES as i32 + 24,
            5,
            10 * CHUNK_SUBNODES as i32 + 24,
        );
        assert!(
            within_reach(far, eye, underfoot),
            "a player in chunk 10,0,10 cannot reach the block under their own feet"
        );
        // And the same cell is far out of reach from the origin chunk.
        assert!(!within_reach(ChunkPos::new(0, 0, 0), eye, underfoot));
    }

    #[test]
    fn the_tolerance_is_generous_rather_than_exact() {
        // The server checks against an older position than the client raycast
        // from — that is what prediction IS — so an exact bound would refuse
        // legitimate actions whenever the player was moving. This pins that the
        // slack exists and is on the generous side.
        let origin = ChunkPos::new(0, 0, 0);
        let eye = [0.0, 0.0, 0.0];
        let just_past = crate::phys::REACH + 1.0;
        assert!(
            within_reach(origin, eye, cell(just_past as i32, 0, 0)),
            "a cell just past the client's own reach must still be allowed"
        );
        let far_past = crate::phys::REACH + REACH_TOLERANCE + 4.0;
        assert!(
            !within_reach(origin, eye, cell(far_past as i32, 0, 0)),
            "the tolerance must still be a bound, not an absence of one"
        );
    }

    #[test]
    fn a_placement_inside_a_standing_player_is_refused() {
        // The rule that stops a player being sealed into a block, and stops one
        // player entombing another.
        let plan = plan(cell(3, 0, 3), UNITS_PER_BLOCK, None, Brush::Block).expect("held");
        let feet = Aabb::player_at([4.0, 1.0, 4.0]);
        assert!(
            blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), feet)]),
            "a block placed around a player's feet was allowed"
        );
    }

    #[test]
    fn a_placement_beside_a_player_is_allowed() {
        // The counter-example that makes the test above mean something: if the
        // check were "always true" it would pass just as well.
        let plan = plan(cell(30, 0, 30), UNITS_PER_BLOCK, None, Brush::Block).expect("held");
        let feet = Aabb::player_at([4.0, 1.0, 4.0]);
        assert!(
            !blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), feet)]),
            "a block well away from the player was refused"
        );
    }

    #[test]
    fn a_block_under_the_feet_of_a_player_standing_on_it_is_allowed() {
        // A body resting on a surface has its minimum exactly on the boundary.
        // Counting that as "inside" would make it impossible to place a block
        // against the ground anyone is standing on — which is most placements.
        let plan = plan(cell(0, 0, 0), UNITS_PER_BLOCK, None, Brush::Block).expect("held");
        // Feet exactly on the top face of the block filling cells 0..3.
        let standing = Aabb::player_at([1.5, 3.0, 1.5]);
        assert!(
            !blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), standing)]),
            "a player standing on top of a block was judged to be inside it"
        );
    }

    #[test]
    fn the_check_reaches_across_a_chunk_origin() {
        // Bodies are stored as a chunk origin plus a local offset (charter rule
        // 7), so a body in chunk 1 and a block in chunk 1 must still line up.
        // With the origin ignored this passes for chunk 0 and silently fails
        // everywhere else — the kind of bug that only reproduces after someone
        // has walked for a while.
        let block_cell = CHUNK_SUBNODES as i32 + 3;
        let plan = plan(
            cell(block_cell, 0, block_cell),
            UNITS_PER_BLOCK,
            None,
            Brush::Block,
        )
        .expect("held");
        let body = Aabb::player_at([4.0, 1.0, 4.0]);
        assert!(
            blocks_a_body(&plan, &[(ChunkPos::new(1, 0, 1), body)]),
            "the body's chunk origin was not applied, so the check only works at the origin"
        );
        assert!(
            !blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), body)]),
            "a body in a different chunk collided with it anyway"
        );
    }

    #[test]
    fn a_partial_placement_only_blocks_where_it_actually_fills() {
        // Bottom-up fill means a few spare nodes occupy the bottom layer only,
        // so a player whose feet are above that layer is not in the way. A
        // check that used the whole block's extent would refuse this.
        let plan = plan(cell(0, 0, 0), 3, None, Brush::Block).expect("held");
        assert_eq!(plan.occupancy.count_ones(), 3);
        let above = Aabb::player_at([1.5, 1.0, 1.5]);
        assert!(
            !blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), above)]),
            "three spare nodes in the bottom layer blocked a body standing a layer above them"
        );
        // And the same body one layer down IS in the way.
        let inside = Aabb::player_at([1.5, 0.0, 1.5]);
        assert!(
            blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), inside)]),
            "the spare nodes did not block a body standing in them"
        );
    }

    #[test]
    fn a_tall_player_is_checked_over_their_whole_height() {
        // Head height, not just feet: sealing someone's head in is as bad as
        // sealing their feet, and the body is 5.4 cells tall.
        let head_cell = PLAYER_HEIGHT as i32 - 1;
        let plan = plan(cell(0, head_cell, 0), UNITS_PER_BLOCK, None, Brush::Block).expect("held");
        let body = Aabb::player_at([1.5, 0.0, 1.5]);
        assert!(
            blocks_a_body(&plan, &[(ChunkPos::new(0, 0, 0), body)]),
            "a block at head height was allowed on top of a standing player"
        );
    }
}
