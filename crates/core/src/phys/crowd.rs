// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Bodies making room for each other.
//!
//! **Reported from the window**: "players should very subtly collide with mobs
//! and each other, just like in Minecraft."
//!
//! # Why this is a nudge and not a collision
//!
//! Terrain collision is a hard constraint: a body may not be inside a block, so
//! [`super::step`] resolves it absolutely, within the tick, by moving the body.
//! Bodies are not like that. Two players walking into the same doorway must not
//! be able to wedge each other, a mob must not be able to pin somebody against a
//! wall, and a crowd must not be able to squeeze one of its own into terrain —
//! any of which a hard constraint between bodies produces on its own.
//!
//! So this is a small *velocity* nudge, applied outward, capped, and never
//! applied vertically. It leans people apart over a few ticks instead of
//! forbidding an overlap, and terrain still gets the last word because the
//! nudge is added before [`super::step`] rather than after it.
//!
//! # Determinism
//!
//! Charter rule 4. The only unusual operation is a `sqrt` to normalise the
//! horizontal offset, which is in the allowed subset. Pairs are visited in a
//! fixed order and the result is accumulated per body in that same order, so
//! the sum does not depend on iteration order. Two bodies at exactly the same
//! place have no direction to be pushed apart along; the fallback is chosen
//! from their positions in the list, which is an order the caller controls,
//! rather than from anything random.

use crate::CHUNK_SUBNODES;
use crate::coords::ChunkPos;

/// How much of two bodies' combined width they are kept apart by.
///
/// A little under all of it, so that brushing past somebody does nothing and
/// standing in the same spot leans you apart. Not the whole width: bodies are
/// drawn as boxes and pushed as circles, and a separation big enough for the
/// corners would hold people a visible gap away from each other.
pub const SNUGNESS: f32 = 0.9;

/// The strongest nudge one pair can apply, in cells per tick.
///
/// **Subtle is the requirement.** At 20 Hz this is a slow lean, roughly a tenth
/// of a walking pace, so two players standing in one spot drift apart over a
/// second rather than being flung. A crowd can still add up to more than this,
/// which is what makes a body squeezed by several move faster than one leaned
/// on by one.
pub const MAX_PUSH: f32 = 0.05;

/// The most any one body can be pushed in a tick, however many are on it.
///
/// Without this, a body in the middle of a pile takes the sum of everything
/// around it and is fired out of the crowd. Terrain would still contain it, but
/// the movement reads as a bug.
pub const MAX_TOTAL: f32 = 0.12;

/// One body in the crowd.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Occupant {
    /// The chunk the position is relative to (charter rule 7).
    pub origin: ChunkPos,
    /// Feet-centre position within that chunk, in cells.
    pub position: [f32; 3],
    /// How tall it is, in cells. Vertical overlap is what makes two bodies
    /// share a space at all — somebody standing on your head is not in it.
    pub height: f32,
    /// How wide its footprint is, in cells. Two bodies are kept
    /// [`SNUGNESS`] of their combined half-widths apart, so a mob the size of
    /// a cart makes more room than a chicken.
    pub width: f32,
    /// Whether this body can be pushed.
    ///
    /// **Nothing sets this false yet.** It is here because "pushes and is not
    /// pushed" is a real thing for a mod to want — a cart, a fixture, a boss
    /// that shoves — and a separation pass that could not express it would have
    /// to be rewritten to add it rather than extended.
    pub movable: bool,
}

/// The nudge each occupant should have added to its velocity this tick.
///
/// Returns one entry per occupant, in the order given. Every vertical component
/// is zero: pushing bodies apart upwards is how a crowd lifts somebody through
/// a ceiling, and pushing down is how it presses them into the floor.
///
/// The caller adds these to velocities BEFORE stepping, so terrain has the last
/// word on where anybody ends up.
#[must_use]
pub fn separate(occupants: &[Occupant]) -> Vec<[f32; 3]> {
    let mut push = vec![[0.0f32; 3]; occupants.len()];
    // Every unordered pair once, in index order, so the accumulation below is
    // in a fixed sequence whatever the caller's collection is.
    for i in 0..occupants.len() {
        for j in (i + 1)..occupants.len() {
            let (a, b) = (occupants[i], occupants[j]);
            if !a.movable && !b.movable {
                continue;
            }
            let spacing = spacing(a, b);
            let Some((dx, dz, distance)) = offset(a, b, spacing) else {
                continue;
            };
            if distance >= spacing || !overlaps_vertically(a, b) {
                continue;
            }
            // Proportional to how far inside each other they are, so touching
            // does nothing and standing in the same place does the most.
            let overlap = (spacing - distance) / spacing;
            let strength = (overlap * MAX_PUSH).min(MAX_PUSH);
            // **Each is pushed the full amount, not half each.** Two players
            // in one spot should each step aside, which is what a player
            // expects from having walked into somebody; halving it makes a
            // pair separate at half the rate a player-and-a-post does, for no
            // reason a player could name.
            if a.movable {
                push[i][0] -= dx * strength;
                push[i][2] -= dz * strength;
            }
            if b.movable {
                push[j][0] += dx * strength;
                push[j][2] += dz * strength;
            }
        }
    }
    for nudge in &mut push {
        clamp_horizontal(nudge, MAX_TOTAL);
    }
    push
}

/// The unit horizontal direction from `a` to `b`, and how far apart they are.
///
/// `None` when the two are too far apart to matter, which is the common case
/// and worth answering before any arithmetic that costs something.
fn offset(a: Occupant, b: Occupant, spacing: f32) -> Option<(f32, f32, f32)> {
    let span = |axis: usize| {
        let chunks = match axis {
            0 => b.origin.x - a.origin.x,
            _ => b.origin.z - a.origin.z,
        };
        // In cells, through the chunk difference, so two bodies either side of
        // a chunk boundary are as close as they look. Never a world-space f32:
        // the difference is small even when the origins are not.
        chunks as f32 * CHUNK_SUBNODES as f32 + b.position[axis] - a.position[axis]
    };
    let dx = span(0);
    let dz = span(2);
    if dx.abs() >= spacing && dz.abs() >= spacing {
        return None;
    }
    let distance = (dx * dx + dz * dz).sqrt();
    if distance > f32::EPSILON {
        return Some((dx / distance, dz / distance, distance));
    }
    // **Exactly on top of each other.** There is no direction to separate
    // along, so one is picked from the order the caller passed them in — a
    // decision that is the same on every machine, which a random one would not
    // be (charter rule 4).
    Some((1.0, 0.0, 0.0))
}

/// How far apart two bodies' centres are kept.
fn spacing(a: Occupant, b: Occupant) -> f32 {
    (a.width + b.width) * 0.5 * SNUGNESS
}

/// Whether two bodies share any height, so that one is in the other's way.
///
/// Somebody standing on your head is above you, not inside you, and pushing
/// them sideways would slide them off a platform you are holding up.
fn overlaps_vertically(a: Occupant, b: Occupant) -> bool {
    let (a_low, a_high) = (a.position[1], a.position[1] + a.height);
    let (b_low, b_high) = (b.position[1], b.position[1] + b.height);
    a_low < b_high && b_low < a_high
}

/// Shortens a horizontal nudge to at most `limit`, keeping its direction.
fn clamp_horizontal(nudge: &mut [f32; 3], limit: f32) {
    let length = (nudge[0] * nudge[0] + nudge[2] * nudge[2]).sqrt();
    if length > limit && length > f32::EPSILON {
        let scale = limit / length;
        nudge[0] *= scale;
        nudge[2] *= scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a body was left completely alone.
    ///
    /// Bit-exact, and deliberately: the claim is not "nearly nothing" but "no
    /// arithmetic was applied to it at all", which is what makes a body out of
    /// range or standing on another one different from a body pushed very
    /// gently. Comparing bits also keeps `clippy::float_cmp` satisfied without
    /// weakening the assertion into a tolerance.
    fn still(push: [f32; 3]) -> bool {
        push.iter().all(|component| component.to_bits() == 0)
    }

    fn at(x: f32, z: f32) -> Occupant {
        Occupant {
            origin: ChunkPos::new(0, 0, 0),
            position: [x, 0.0, z],
            height: crate::phys::PLAYER_HEIGHT,
            width: crate::phys::PLAYER_WIDTH,
            movable: true,
        }
    }

    #[test]
    fn bodies_that_are_not_touching_are_left_alone() {
        let apart = crate::phys::PLAYER_WIDTH * SNUGNESS + 0.01;
        let pushes = separate(&[at(0.0, 0.0), at(apart, 0.0)]);
        assert!(pushes.iter().copied().all(still), "{pushes:?}");
    }

    #[test]
    fn two_bodies_in_one_place_lean_apart_rather_than_jumping() {
        let pushes = separate(&[at(0.0, 0.0), at(0.2, 0.0)]);
        assert!(pushes[0][0] < 0.0, "the first should go one way");
        assert!(pushes[1][0] > 0.0, "and the second the other");
        for push in &pushes {
            let speed = (push[0] * push[0] + push[2] * push[2]).sqrt();
            assert!(
                speed <= MAX_TOTAL,
                "a push of {speed} is not subtle; the cap is {MAX_TOTAL}"
            );
        }
    }

    #[test]
    fn nothing_is_ever_pushed_up_or_down() {
        // A crowd that could push vertically is a crowd that lifts one of its
        // own through a ceiling, and one that presses somebody into the floor.
        let crowd: Vec<Occupant> = (0..6)
            .map(|i| {
                let mut o = at(0.1 * i as f32, 0.05 * i as f32);
                o.position[1] = 0.02 * i as f32;
                o
            })
            .collect();
        for push in separate(&crowd) {
            assert_eq!(push[1].to_bits(), 0, "something was pushed vertically");
        }
    }

    #[test]
    fn a_body_standing_on_another_is_above_it_and_not_inside_it() {
        let lower = at(0.0, 0.0);
        let upper = Occupant {
            position: [0.0, crate::phys::PLAYER_HEIGHT + 0.1, 0.0],
            ..at(0.0, 0.0)
        };
        let pushes = separate(&[lower, upper]);
        assert!(
            pushes.iter().copied().all(still),
            "a body standing on another was shoved sideways off it: {pushes:?}"
        );
    }

    #[test]
    fn one_that_cannot_be_pushed_still_pushes() {
        let fixture = Occupant {
            movable: false,
            ..at(0.0, 0.0)
        };
        let pushes = separate(&[fixture, at(0.3, 0.0)]);
        assert!(still(pushes[0]), "an immovable body moved");
        assert!(pushes[1][0] > 0.0, "an immovable body did not push");
    }

    #[test]
    fn a_body_in_a_pile_is_leaned_on_and_not_fired_out_of_it() {
        // The reason for a total cap: without one, a body surrounded by five
        // others takes the sum of all five.
        let mut crowd = vec![at(0.0, 0.0)];
        for step in 0..5 {
            let angle = step as f32;
            crowd.push(at(0.2 + 0.05 * angle, 0.1 * angle));
        }
        let pushes = separate(&crowd);
        for push in &pushes {
            let speed = (push[0] * push[0] + push[2] * push[2]).sqrt();
            assert!(
                speed <= MAX_TOTAL + 1e-6,
                "a body in a pile was pushed at {speed}, over the {MAX_TOTAL} cap"
            );
        }
    }

    #[test]
    fn two_bodies_either_side_of_a_chunk_boundary_are_as_close_as_they_look() {
        // Charter rule 7: positions are (chunk, local), so a pair straddling a
        // boundary have small local coordinates that are far apart as numbers
        // and adjacent in the world. Comparing the locals alone would make two
        // people standing next to each other ignore one another entirely.
        let west = Occupant {
            origin: ChunkPos::new(0, 0, 0),
            position: [CHUNK_SUBNODES as f32 - 0.2, 0.0, 0.0],
            ..at(0.0, 0.0)
        };
        let east = Occupant {
            origin: ChunkPos::new(1, 0, 0),
            position: [0.2, 0.0, 0.0],
            ..at(0.0, 0.0)
        };
        let pushes = separate(&[west, east]);
        assert!(
            pushes[0][0] < 0.0 && pushes[1][0] > 0.0,
            "a pair across a chunk boundary did not push apart: {pushes:?}"
        );
    }

    #[test]
    fn the_answer_does_not_depend_on_who_is_asked_first() {
        // Not an ordering-independence claim — the fallback direction for two
        // bodies in exactly one place is deliberately order-dependent. This is
        // the weaker and more useful property: the same input in the same
        // order gives the same answer every time, which is what the hash gate
        // rests on.
        let crowd = vec![at(0.0, 0.0), at(0.4, 0.1), at(-0.3, 0.5)];
        assert_eq!(separate(&crowd), separate(&crowd));
    }
}
