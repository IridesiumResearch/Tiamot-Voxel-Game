// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The star catalog: one source for the sky and for anything the stars become.
//!
//! # Why this is in core and not in the client
//!
//! Stars are drawn by the client, so on the face of it this belongs there. It
//! does not, and the reason is Task 15c: **a star may later be somewhere you can
//! go.** The moment a star is both a point in the sky and a destination on a
//! map, two independent derivations of "where the stars are" become two answers
//! to the same question, and the bug is a sky that does not match the map.
//!
//! Deriving them once, from the world seed, in the crate both ends already
//! depend on, makes that class of bug unavailable rather than unlikely. The
//! task asks for exactly this and asks for it now, before there is a second
//! consumer to disagree with.
//!
//! # Determinism
//!
//! Charter rule 4 applies in full: this feeds a persisted world's identity, not
//! just a picture. The catalog uses [`crate::detgen::StreamRng`] and the allowed
//! float subset — no trigonometry. A direction on the unit sphere therefore
//! comes from the rejection method rather than from `sin`/`cos` of two angles,
//! which is both deterministic and, as it happens, the standard way to sample a
//! sphere without clustering at the poles.

use crate::detgen::StreamRng;

/// The RNG stream the catalog draws from.
///
/// Named rather than derived from the seed alone so that adding another
/// world-level stream later cannot shift the stars — charter rule 4's
/// stream-per-purpose rule.
pub const STAR_STREAM: &str = "sky:stars";

/// How many stars a world has.
///
/// Fixed rather than configurable: the catalog is world identity, and a count
/// a server could change would mean two servers on the same seed disagreeing
/// about the sky. A mod that wants fewer visible draws fewer.
pub const STAR_COUNT: usize = 1024;

/// One star.
///
/// Positions are directions on the unit sphere, not points in space. A star is
/// infinitely far away for rendering purposes, and Task 15c can give it a
/// distance later without changing what the sky does with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StarRecord {
    /// Unit direction from the world's centre.
    pub direction: [f32; 3],
    /// Apparent brightness, `0.0..=1.0`.
    ///
    /// Skewed towards the dim end — a sky of equally bright stars reads as a
    /// texture rather than as a sky.
    pub magnitude: f32,
    /// Colour temperature as a hue mix, `0.0` coolest to `1.0` warmest.
    ///
    /// A hint rather than a physical temperature: the client decides what
    /// palette to run it through, and a mod may decide differently.
    pub warmth: f32,
}

/// Every star in a world, in a fixed order.
///
/// The same seed gives the same catalog on every platform and in every process.
/// Deriving it is cheap enough — a thousand rejection samples — that nothing
/// needs to cache it, and a cache would be one more thing that can disagree.
#[must_use]
pub fn star_catalog(world_seed: u64) -> Vec<StarRecord> {
    let mut rng = StreamRng::global(world_seed, STAR_STREAM);
    let mut stars = Vec::with_capacity(STAR_COUNT);

    while stars.len() < STAR_COUNT {
        let Some(direction) = sample_direction(&mut rng) else {
            continue;
        };
        // Squared, so most stars are faint and a few are bright. A uniform
        // magnitude gives a sky that looks printed on.
        let roll = rng.next_f32();
        stars.push(StarRecord {
            direction,
            magnitude: roll * roll,
            warmth: rng.next_f32(),
        });
    }
    stars
}

/// A uniformly distributed direction on the unit sphere, or `None` to retry.
///
/// **Rejection sampling, because charter rule 4 bans trigonometry in
/// simulation.** The textbook alternative — two angles through `sin` and `cos`
/// — calls platform libm and differs between operating systems, which is
/// exactly what the rule exists to prevent. Drawing a point in the unit cube
/// and rejecting it unless it lands inside the sphere costs a few extra draws
/// and uses only multiplication, comparison and `sqrt`, all of which are in the
/// allowed subset.
///
/// It is also better sampling than the naive angle method, which clusters
/// points at the poles.
fn sample_direction(rng: &mut StreamRng) -> Option<[f32; 3]> {
    let x = rng.next_f32_signed();
    let y = rng.next_f32_signed();
    let z = rng.next_f32_signed();
    let square = x * x + y * y + z * z;

    // Outside the sphere is rejected; so is a point at the very centre, which
    // has no direction and would divide by zero. The lower bound is not an
    // epsilon fudge — it is the one input for which the answer does not exist.
    if square > 1.0 || square <= 0.0 {
        return None;
    }
    let length = square.sqrt();
    Some([x / length, y / length, z / length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_sky() {
        // The property the whole module exists for: one derivation, so the sky
        // and any future map of it cannot disagree.
        let a = star_catalog(12_345);
        let b = star_catalog(12_345);
        assert_eq!(a.len(), STAR_COUNT);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_give_different_skies() {
        // The counter-example: a catalog that ignored its seed would satisfy
        // the test above perfectly.
        let a = star_catalog(1);
        let b = star_catalog(2);
        assert_ne!(a, b, "two worlds got identical skies");
    }

    #[test]
    fn every_direction_is_a_unit_vector() {
        // A direction that is not normalised puts a star at a distance, and
        // "distance" is what Task 15c may later mean by it — so this has to be
        // exactly right rather than nearly.
        for star in star_catalog(7) {
            let [x, y, z] = star.direction;
            let length_squared = x * x + y * y + z * z;
            assert!(
                (length_squared - 1.0).abs() < 1e-5,
                "star at {:?} has length squared {length_squared}",
                star.direction
            );
        }
    }

    #[test]
    fn the_stars_are_spread_over_the_whole_sphere() {
        // Rejection sampling is uniform; the angle method it replaces clusters
        // at the poles. This checks the sky is not a band or a cap — with 1,024
        // stars every octant should hold roughly an eighth of them.
        let mut octants = [0usize; 8];
        for star in star_catalog(99) {
            let [x, y, z] = star.direction;
            let index =
                usize::from(x > 0.0) | (usize::from(y > 0.0) << 1) | (usize::from(z > 0.0) << 2);
            octants[index] += 1;
        }
        let expected = STAR_COUNT / 8;
        for (index, count) in octants.into_iter().enumerate() {
            assert!(
                count > expected / 2 && count < expected * 2,
                "octant {index} holds {count} stars against about {expected}; the sky is not \
                 evenly covered"
            );
        }
    }

    #[test]
    fn most_stars_are_faint_and_a_few_are_bright() {
        // A uniform magnitude reads as a printed texture rather than a sky.
        // Squaring a uniform roll puts three quarters of the catalog below a
        // quarter brightness, and this pins that rather than leaving it to a
        // reader to infer from one multiplication.
        let stars = star_catalog(4);
        let faint = stars.iter().filter(|star| star.magnitude < 0.25).count();
        assert!(
            faint > stars.len() / 2,
            "only {faint} of {} stars are faint; the sky is evenly lit",
            stars.len()
        );
        assert!(
            stars.iter().any(|star| star.magnitude > 0.8),
            "no star is bright, so there is nothing to pick out"
        );
    }

    #[test]
    fn every_value_is_in_range() {
        for star in star_catalog(555) {
            assert!((0.0..=1.0).contains(&star.magnitude));
            assert!((0.0..=1.0).contains(&star.warmth));
            assert!(star.direction.iter().all(|value| value.is_finite()));
        }
    }

    #[test]
    fn the_catalog_uses_its_own_stream() {
        // Charter rule 4's stream-per-purpose rule. If the stars drew from a
        // shared stream, adding any other world-level randomness later would
        // silently rearrange the sky of every existing world.
        let named = StreamRng::global(3, STAR_STREAM).next_u64();
        let other = StreamRng::global(3, "sky:something-else").next_u64();
        assert_ne!(
            named, other,
            "two named streams gave the same sequence, so the name is not reaching the seed"
        );
    }
}
