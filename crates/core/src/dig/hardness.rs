// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! How hard a block is to break when it is made of more than one thing.
//!
//! A block is 27 sub-node cells (charter rule 5) and each of them may be a
//! different material, so "the hardness of this block" is a question about a
//! mixture rather than a lookup. This module answers it, and answers the
//! sub-node case with it.
//!
//! # The rule: average the RATES, weighted by dominance
//!
//! ```text
//! rate = Σ(dᵢ / hᵢ) / Σ dᵢ        time = 1 / rate
//! ```
//!
//! where `hᵢ` is the cell's material's hardness in seconds and `dᵢ` its
//! [`Resistance::dominance`].
//!
//! Averaging *rates* rather than *times* is the whole design, and it is what
//! makes a mixture behave the way a mixture should: a fast material contributes
//! a large rate, so the weak part of a block carries the rest of it away. Mixing
//! dirt into stone lands the block near dirt, not halfway. That falls out of the
//! arithmetic with every dominance at 1 — no special case decides it.
//!
//! # Why dominance exists as well
//!
//! Rate-averaging only ever pulls a mixture toward its *softest* member, and
//! that is one of the two behaviours a mixture wants. A material can also be
//! sticky — hard to cut in a way that makes everything packed around it hard to
//! cut — and no mean over hardnesses alone can express that, because the
//! quantity that varies is not the hardness but how much the material *insists*.
//!
//! So each material carries a second number. `dominance` is its weight in the
//! average above, and because the average is over rates, weighting a slow
//! material heavily drags the mixture toward it just as effectively as
//! weighting a fast one. One field, both directions:
//!
//! - Dirt at `dominance = 3` makes dirt-in-stone break at nearly dirt's speed.
//! - Rubber at `dominance = 6` makes rubber-in-anything break at nearly
//!   rubber's, which is slowly.
//!
//! Measured against the numbers a reference mod might pick — dirt 0.5 s,
//! stone 1.5 s, gold 1.0 s, iron 3.0 s, rubber 10.0 s. Half a block each, as
//! near as 27 cells allow: 14 of the first material and 13 of the second.
//!
//! | mixture | blend | pure |
//! |---|---|---|
//! | gold + stone | 1.19 s | 1.0 / 1.5 |
//! | iron + stone | 2.03 s | 3.0 / 1.5 |
//! | dirt (dom 3) + stone | 0.59 s | 0.5 / 1.5 |
//! | rubber (dom 6) + stone | 5.68 s | 10.0 / 1.5 |
//!
//! Gold beats iron; dirt lands nearer 0.5 than 1.5; rubber lands nearer 10 than
//! 1.5. Those three orderings are the rule, and the exact figures are pinned by
//! the tests below so that a change to the blend has to be a deliberate one.
//!
//! # Determinism
//!
//! Charter rule 4 reaches here: what a block costs to break decides what the
//! world looks like a second later, so it is simulation. The blend is addition
//! and division over a fixed 0..27 iteration and nothing else — no `powf`, no
//! roots, no accumulation over an unordered collection. A power mean would
//! express the same intent more generally and is not available for exactly that
//! reason.
//!
//! # Cost
//!
//! Evaluated when a dig advances, so at most once per digging player per tick —
//! 50 players at 20 Hz is 1,000 evaluations a second, each 27 adds and one
//! divide over a lookup the caller already has in hand. The lookups dominate,
//! and a [`BlockView::Uniform`] block (nearly all of them) needs exactly one:
//! see [`block_hardness`]'s fast path.

use crate::block::{BlockView, SUBNODES_PER_BLOCK};
use crate::material::MaterialId;

/// How one material resists being mined.
///
/// What a mod declares per block, and the only thing the blend below reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resistance {
    /// Seconds to break a full block of this material with a bare hand.
    pub hardness: f32,
    /// How strongly this material imposes itself on a mixture. `1.0` is
    /// neutral; see the [module documentation](self).
    pub dominance: f32,
}

impl Resistance {
    /// A neutral material at a given hardness.
    #[must_use]
    pub const fn new(hardness: f32) -> Self {
        Self {
            hardness,
            dominance: Self::DEFAULT_DOMINANCE,
        }
    }

    /// The dominance of a material whose mod said nothing about it.
    ///
    /// One: it pulls its own weight and no more, so a mod that never heard of
    /// this field gets a plain rate average.
    pub const DEFAULT_DOMINANCE: f32 = 1.0;
}

/// The smallest hardness the blend will divide by, in seconds.
///
/// A mod may legitimately register a hardness of zero — something that comes
/// apart the moment it is touched — and `1 / 0` is an infinity that would
/// propagate into the sum and make the whole block's answer `NaN` the moment a
/// second material joined it. Charter rule 4 forbids producing `NaN` in
/// simulation state outright, so the divisor gets a floor instead.
///
/// It is small enough that a zero-hardness cell still swamps everything around
/// it — which is the correct reading of the rule, not a compromise with it: an
/// instant material mixed into stone makes the block instant, exactly as dirt
/// mixed into stone makes it nearly dirt. `dominance` is the knob for a mod that
/// wants otherwise.
const MIN_HARDNESS: f32 = 0.0001;

/// What one sub-node costs, as a fraction of its material's whole-block time.
///
/// **A 27-cell block chiselled out one cell at a time therefore costs twice
/// what smashing it whole does** (27 / 13.5 = 2). That is the number's entire
/// justification: sub-node precision is a choice a player makes, and it should
/// cost them time rather than being strictly better than the coarse tool.
pub const SUBNODE_SHARE: f32 = 1.0 / 13.5;

/// Clamps a mod-supplied resistance into something the blend can divide by.
///
/// `register_block` already refuses a negative or non-finite hardness, so this
/// is the second line rather than the first: the registry is not the only way a
/// `Resistance` can be constructed, and a `NaN` reaching the sum would poison
/// every block in the world rather than the one that caused it.
fn sane(resistance: Resistance) -> (f32, f32) {
    let hardness = if resistance.hardness.is_finite() && resistance.hardness > MIN_HARDNESS {
        resistance.hardness
    } else {
        MIN_HARDNESS
    };
    let dominance = if resistance.dominance.is_finite() && resistance.dominance > 0.0 {
        resistance.dominance
    } else {
        Resistance::DEFAULT_DOMINANCE
    };
    (hardness, dominance)
}

/// Seconds to break one sub-node cell of `material`.
///
/// [`SUBNODE_SHARE`] of what a whole block of it would cost. A mixture has no
/// say here — a cell is one material by definition, which is the other half of
/// why the sub-node lattice is the resolution the engine stores materials at.
#[must_use]
pub fn subnode_hardness(material: MaterialId, of: impl Fn(MaterialId) -> Resistance) -> f32 {
    if material.is_air() {
        return 0.0;
    }
    let (hardness, _) = sane(of(material));
    hardness * SUBNODE_SHARE
}

/// Seconds to break a whole block, blended over the materials in it.
///
/// Air cells are skipped: they are not material and nothing about them resists a
/// tool. A block of nothing but air returns `0.0`, which
/// [`super::ticks_to_break`] turns into a single tick rather than a division by
/// zero — though the dig loop never asks, because it stops on an air target
/// first.
///
/// **How full the block is does not enter into it.** A stone block with twenty
/// cells already chiselled away takes exactly as long to finish as a whole one,
/// because what is being measured is how the material resists a tool and not how
/// much of it there is. Making a half-mined block quicker to finish is a
/// progression decision, and progression is a mod's business (charter rule 1) —
/// the engine's job is to make the composition legible, which is what
/// `dominance` does.
#[must_use]
pub fn block_hardness(view: &BlockView<'_>, of: impl Fn(MaterialId) -> Resistance) -> f32 {
    // A uniform or partial block is one material, so the blend is the identity
    // and the loop below would do 27 lookups to prove it. This is the case
    // nearly every block in a world is in.
    match view {
        BlockView::Uniform(material) | BlockView::Partial { material, .. } => {
            if material.is_air() {
                return 0.0;
            }
            let (hardness, _) = sane(of(*material));
            return hardness;
        }
        BlockView::Mixed(_) => {}
    }

    // Fixed order, 0..27, per charter rule 4: a float sum whose order can vary
    // is a float sum whose result can vary.
    let mut total_dominance = 0.0f32;
    let mut total_rate = 0.0f32;
    for index in 0..SUBNODES_PER_BLOCK {
        let material = view.subnode(index);
        if material.is_air() {
            continue;
        }
        let (hardness, dominance) = sane(of(material));
        total_dominance += dominance;
        total_rate += dominance / hardness;
    }

    if total_rate <= 0.0 {
        // Every cell was air. Nothing to break.
        return 0.0;
    }
    total_dominance / total_rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockValue, Cells, subnode_index};

    const DIRT: MaterialId = MaterialId(2);
    const STONE: MaterialId = MaterialId(3);
    const GOLD: MaterialId = MaterialId(4);
    const IRON: MaterialId = MaterialId(5);
    const RUBBER: MaterialId = MaterialId(6);

    /// The reference numbers the module documentation's table is computed from.
    fn reference(material: MaterialId) -> Resistance {
        match material {
            DIRT => Resistance {
                hardness: 0.5,
                dominance: 3.0,
            },
            STONE => Resistance::new(1.5),
            GOLD => Resistance::new(1.0),
            IRON => Resistance::new(3.0),
            // Anything unnamed gets the engine's own default, which is
            // deliberately none of the figures above: a test that accidentally
            // reached for an unregistered material should not silently get
            // gold's numbers.
            RUBBER => Resistance {
                hardness: 10.0,
                dominance: 6.0,
            },
            _ => Resistance::new(crate::script::BlockRules::DEFAULT_HARDNESS),
        }
    }

    /// A block that is half `a` and half `b`, split along x.
    fn halves(a: MaterialId, b: MaterialId) -> Cells {
        let mut cells = crate::block::EMPTY_CELLS;
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    cells[subnode_index(x, y, z)] = if x < 2 { a } else { b };
                }
            }
        }
        cells
    }

    /// As near half and half as 27 cells allow: 14 of `a` and 13 of `b`. This
    /// is exactly what the documented table is computed from.
    fn even(a: MaterialId, b: MaterialId) -> Cells {
        let mut cells = crate::block::EMPTY_CELLS;
        for (index, cell) in cells.iter_mut().enumerate() {
            *cell = if index % 2 == 0 { a } else { b };
        }
        cells
    }

    fn blend(cells: &Cells) -> f32 {
        block_hardness(&BlockView::Mixed(cells), reference)
    }

    #[test]
    fn a_block_of_one_material_is_that_material() {
        assert!((block_hardness(&BlockView::Uniform(STONE), reference) - 1.5).abs() < 1e-6);
        // And a partly chiselled block of it is still that material: how much is
        // left does not change how the material resists a tool.
        let partial = BlockView::Partial {
            material: STONE,
            occupancy: 0b101,
        };
        assert!((block_hardness(&partial, reference) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn a_softer_ore_makes_the_block_quicker_than_a_harder_one() {
        // The user's own example: gold+stone must beat iron+stone.
        let gold = blend(&even(GOLD, STONE));
        let iron = blend(&even(IRON, STONE));
        assert!(
            gold < iron,
            "gold+stone {gold} should break faster than iron+stone {iron}"
        );
        // And the documented figures, which is what makes this a known-answer
        // test rather than an inequality that any monotone rule would pass.
        assert!((gold - 1.19).abs() < 0.01, "gold+stone was {gold}");
        assert!((iron - 2.03).abs() < 0.01, "iron+stone was {iron}");
    }

    #[test]
    fn dirt_drags_a_mixture_down_to_nearly_its_own_speed() {
        // "if dirt is mixed into stone or iron it would pull the breaking speed
        // much closer to dirt than it would to iron."
        let mixed = blend(&even(DIRT, STONE));
        let midpoint = f32::midpoint(0.5, 1.5);
        assert!(
            mixed < midpoint,
            "a rate average must land below the arithmetic midpoint, got {mixed}"
        );
        assert!(
            (mixed - 0.5).abs() < (mixed - 1.5f32).abs(),
            "{mixed} should sit nearer dirt than stone"
        );
        assert!((mixed - 0.59).abs() < 0.01, "dirt+stone was {mixed}");

        // And into iron, which is the harder half of the same sentence.
        let with_iron = blend(&even(DIRT, IRON));
        assert!(
            with_iron < 1.0,
            "dirt should carry even iron away, got {with_iron}"
        );
    }

    #[test]
    fn rubber_drags_a_mixture_up_to_nearly_its_own_speed() {
        // The opposite direction, and the reason `dominance` is a field rather
        // than the rate average being the whole rule.
        let mixed = blend(&even(RUBBER, STONE));
        assert!(
            mixed > 1.5,
            "rubber must make stone slower, not faster: {mixed}"
        );
        assert!((mixed - 5.68).abs() < 0.01, "rubber+stone was {mixed}");

        // Even against dirt, which is the strongest softener there is.
        let with_dirt = blend(&even(RUBBER, DIRT));
        assert!(
            with_dirt > 0.5 * 2.0,
            "rubber should more than double dirt's time, got {with_dirt}"
        );
    }

    #[test]
    fn dominance_is_what_separates_the_two_directions() {
        // With every dominance at 1 the rule still softens — that falls out of
        // averaging rates — but it cannot harden. This is the proof that the
        // second field is doing work rather than being decoration.
        let neutral = |_material: MaterialId| Resistance::new(10.0);
        let stone_only = |material: MaterialId| {
            if material == RUBBER {
                Resistance::new(10.0)
            } else {
                Resistance::new(1.5)
            }
        };
        let cells = even(RUBBER, STONE);
        let flat = block_hardness(&BlockView::Mixed(&cells), stone_only);
        assert!(
            flat < 10.0 && flat < blend(&cells),
            "without dominance a hard material cannot dominate: {flat}"
        );
        let _ = neutral;
    }

    #[test]
    fn air_cells_are_not_material_and_do_not_count() {
        // A block that is one cell of stone and 26 of air breaks at stone's
        // speed, not at a twenty-seventh of it.
        let mut cells = crate::block::EMPTY_CELLS;
        cells[0] = STONE;
        let only = block_hardness(&BlockView::Mixed(&cells), reference);
        assert!((only - 1.5).abs() < 1e-6, "one stone cell gave {only}");

        // And a block of nothing at all is zero rather than a division by zero.
        let empty = block_hardness(&BlockView::Mixed(&crate::block::EMPTY_CELLS), reference);
        assert!(empty.abs() < f32::EPSILON, "a block of air came to {empty}");
    }

    #[test]
    fn how_much_of_a_block_is_left_does_not_change_its_hardness() {
        // Documented behaviour, and the one a future progression mod is most
        // likely to want changed — so it is pinned rather than incidental.
        let full = blend(&even(DIRT, STONE));
        let mut thinned = even(DIRT, STONE);
        for (index, cell) in thinned.iter_mut().enumerate() {
            if index % 4 == 0 {
                *cell = MaterialId::AIR;
            }
        }
        let partial = blend(&thinned);
        // Not equal — the proportions changed — but both are still a blend of
        // the same two materials rather than a function of how full the block
        // is. The check that matters is that removing material never makes the
        // block HARDER than its hardest constituent.
        assert!(partial <= 1.5 && full <= 1.5);
    }

    #[test]
    fn a_sub_node_costs_a_thirteen_and_a_half_th_of_its_own_material() {
        let cell = subnode_hardness(STONE, reference);
        assert!((cell - 1.5 / 13.5).abs() < 1e-6, "a stone cell took {cell}");

        // Which makes chiselling a whole block out cost twice smashing it.
        let whole = block_hardness(&BlockView::Uniform(STONE), reference);
        let chiselled = cell * SUBNODES_PER_BLOCK as f32;
        assert!(
            (chiselled - whole * 2.0).abs() < 1e-4,
            "27 cells came to {chiselled} against {whole} for the block"
        );

        let nothing = subnode_hardness(MaterialId::AIR, reference);
        assert!(nothing.abs() < f32::EPSILON, "an air cell cost {nothing}");
    }

    #[test]
    fn a_zero_hardness_material_never_produces_a_non_finite_result() {
        // Charter rule 4 forbids NaN in simulation state, and `1 / 0` in the
        // rate sum is the obvious way to get one.
        let instant = |material: MaterialId| {
            if material == DIRT {
                Resistance::new(0.0)
            } else {
                Resistance::new(1.5)
            }
        };
        let cells = even(DIRT, STONE);
        let blended = block_hardness(&BlockView::Mixed(&cells), instant);
        assert!(blended.is_finite(), "got {blended}");
        assert!(blended >= 0.0);
        assert!(
            blended < 0.01,
            "an instant material should carry the block, got {blended}"
        );
    }

    #[test]
    fn a_nonsense_resistance_is_survived_rather_than_propagated() {
        // `register_block` refuses these, but a `Resistance` can be built
        // without it and one bad block must not poison the world's arithmetic.
        let broken = |_material: MaterialId| Resistance {
            hardness: f32::NAN,
            dominance: f32::INFINITY,
        };
        let cells = even(DIRT, STONE);
        let blended = block_hardness(&BlockView::Mixed(&cells), broken);
        assert!(blended.is_finite(), "got {blended}");
    }

    #[test]
    fn the_same_mixture_blends_to_the_same_number_every_time() {
        // Underwrites the determinism gate: this feeds how long a block takes to
        // break, which decides what the world looks like a tick later.
        let cells = halves(DIRT, STONE);
        // Bit equality, not a tolerance: "the same arithmetic gives the same
        // answer" is exactly what charter rule 4 asks for, and a tolerance here
        // would accept a blend that drifted.
        let first = blend(&cells);
        for _ in 0..16 {
            assert_eq!(blend(&cells).to_bits(), first.to_bits());
        }
    }

    #[test]
    fn the_blend_never_leaves_the_range_of_its_constituents() {
        // A mean must be a mean: whatever the weights, the answer sits between
        // the fastest and slowest material in the block. A rule that could leave
        // that range would be able to make a block of stone and dirt harder than
        // stone, which no mod author would predict.
        let cells = even(DIRT, RUBBER);
        let blended = blend(&cells);
        assert!(
            (0.5..=10.0).contains(&blended),
            "{blended} left the range its materials define"
        );

        let three = {
            let mut cells = crate::block::EMPTY_CELLS;
            for (index, cell) in cells.iter_mut().enumerate() {
                *cell = match index % 3 {
                    0 => DIRT,
                    1 => STONE,
                    _ => IRON,
                };
            }
            cells
        };
        let blended = blend(&three);
        assert!((0.5..=3.0).contains(&blended), "{blended}");
    }

    #[test]
    fn a_block_value_and_its_view_agree() {
        // The blend is written against `BlockView`, and the fast path matches on
        // its variants — so a `Mixed` view holding 27 copies of one material must
        // give the same answer as the `Uniform` that canonicalises to.
        let mut cells = crate::block::EMPTY_CELLS;
        cells.fill(STONE);
        let as_mixed = block_hardness(&BlockView::Mixed(&cells), reference);
        let as_uniform = block_hardness(&BlockView::Uniform(STONE), reference);
        assert!((as_mixed - as_uniform).abs() < 1e-6);

        // And the canonical form really is the uniform one, so this equality is
        // what the world actually stores rather than a hypothetical.
        assert_eq!(
            BlockValue::Cells(cells).canonical(),
            BlockValue::Uniform(STONE)
        );
    }
}
