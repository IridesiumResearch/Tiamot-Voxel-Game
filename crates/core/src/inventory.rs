// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Unit arithmetic: what breaking a block yields and how quantities are held.
//!
//! Charter rule 5 in code. One block is 27 units, quantities are stored in
//! units as `u32`, and display splits them into whole blocks plus spare nodes.
//! There is no separate "partial block" quantity type and no special case for
//! fractional amounts — a third of a block is nine units, which is an ordinary
//! number.
//!
//! Storing units rather than blocks is what makes chiselling conserve material
//! for free: carve nine units out of a block and nine units is exactly what
//! drops. [`break_block`] is the conservation law, and the property test
//! `units_conserved` asserts it against arbitrary block contents.

use crate::UNITS_PER_BLOCK;
use crate::block::BlockView;
use crate::material::MaterialId;

/// A quantity of one material, measured in sub-node units.
///
/// Never holds air and never holds zero units: both are the absence of a stack
/// rather than a stack, and allowing them would mean every consumer had to
/// filter. [`Stack::new`] enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stack {
    /// What this is made of.
    pub material: MaterialId,
    /// How much, in units. 27 units is one whole block.
    pub units: u32,
}

/// Why a stack operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StackError {
    /// Merging two stacks of different materials.
    #[error("cannot merge {left:?} with {right:?}: different materials")]
    MaterialMismatch {
        /// Material of the stack being merged into.
        left: MaterialId,
        /// Material of the stack being merged in.
        right: MaterialId,
    },

    /// The combined amount would not fit in a `u32`.
    #[error("merging {left} and {right} units overflows u32")]
    Overflow {
        /// Units already held.
        left: u32,
        /// Units being added.
        right: u32,
    },

    /// Splitting off more than the stack holds.
    #[error("cannot take {requested} units from a stack of {available}")]
    Insufficient {
        /// Units asked for.
        requested: u32,
        /// Units actually present.
        available: u32,
    },
}

impl Stack {
    /// A stack of `units` of `material`.
    ///
    /// Returns `None` for air or for zero units — neither is a stack.
    #[must_use]
    pub fn new(material: MaterialId, units: u32) -> Option<Self> {
        (!material.is_air() && units > 0).then_some(Self { material, units })
    }

    /// A stack of `blocks` whole blocks.
    ///
    /// Returns `None` for air, zero, or an amount that overflows `u32`.
    #[must_use]
    pub fn from_blocks(material: MaterialId, blocks: u32) -> Option<Self> {
        Self::new(material, blocks.checked_mul(UNITS_PER_BLOCK)?)
    }

    /// Adds another stack's units into this one.
    ///
    /// # Errors
    ///
    /// [`StackError::MaterialMismatch`] if the materials differ, or
    /// [`StackError::Overflow`] if the total would not fit. On error this stack
    /// is left unchanged.
    pub fn merge(&mut self, other: Self) -> Result<(), StackError> {
        if self.material != other.material {
            return Err(StackError::MaterialMismatch {
                left: self.material,
                right: other.material,
            });
        }
        // Checked rather than saturating: silently capping would destroy
        // material, and this arithmetic is a conservation law.
        self.units = self
            .units
            .checked_add(other.units)
            .ok_or(StackError::Overflow {
                left: self.units,
                right: other.units,
            })?;
        Ok(())
    }

    /// Removes `units` from this stack and returns them as a new stack.
    ///
    /// Splitting off the entire stack is allowed and leaves this one at zero
    /// units; callers holding stacks in a container should drop an emptied one.
    ///
    /// # Errors
    ///
    /// [`StackError::Insufficient`] if the stack holds less than `units`. On
    /// error this stack is left unchanged.
    pub fn split(&mut self, units: u32) -> Result<Self, StackError> {
        if units > self.units {
            return Err(StackError::Insufficient {
                requested: units,
                available: self.units,
            });
        }
        self.units -= units;
        Ok(Self {
            material: self.material,
            units,
        })
    }

    /// Whether this stack has been emptied by [`Self::split`].
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.units == 0
    }

    /// This quantity as whole blocks and spare nodes.
    #[must_use]
    pub const fn display(&self) -> (u32, u32) {
        display(self.units)
    }
}

/// Splits a unit count into whole blocks and spare nodes.
///
/// `(units / 27, units % 27)` — charter rule 5's display rule, in one place so
/// no caller open-codes the division.
#[must_use]
pub const fn display(units: u32) -> (u32, u32) {
    (units / UNITS_PER_BLOCK, units % UNITS_PER_BLOCK)
}

/// What breaking a block yields.
///
/// - `Uniform` of a solid material yields 27 units of it.
/// - `Uniform` of air yields nothing.
/// - `Partial` yields one unit per set occupancy bit.
/// - `Mixed` yields one stack per distinct non-air material, each with its own
///   cell count.
///
/// **Output order is ascending [`MaterialId`]**, always. Drop order is
/// observable — it decides which stack an almost-full inventory keeps — so it
/// must not depend on cell iteration order, hash order, or anything else that
/// could differ between machines running the same simulation (charter rule 4).
#[must_use]
pub fn break_block(block: BlockView<'_>) -> Vec<Stack> {
    match block {
        BlockView::Uniform(material) => Stack::new(material, UNITS_PER_BLOCK).into_iter().collect(),

        BlockView::Partial {
            material,
            occupancy,
        } => Stack::new(material, occupancy.count_ones())
            .into_iter()
            .collect(),

        BlockView::Mixed(cells) => {
            // A block has 27 cells, so a sorted insert into a small Vec beats a
            // map: it allocates once and stays in cache.
            let mut stacks: Vec<Stack> = Vec::new();
            for &material in cells {
                if material.is_air() {
                    continue;
                }
                match stacks.binary_search_by_key(&material, |stack| stack.material) {
                    Ok(found) => stacks[found].units += 1,
                    Err(insert_at) => stacks.insert(insert_at, Stack { material, units: 1 }),
                }
            }
            stacks
        }
    }
}

/// Merges stacks of like materials, returning them in ascending
/// [`MaterialId`] order.
///
/// Amounts that would overflow `u32` are left as separate stacks rather than
/// being capped, so nothing is destroyed.
#[must_use]
pub fn consolidate(stacks: impl IntoIterator<Item = Stack>) -> Vec<Stack> {
    let mut merged: Vec<Stack> = Vec::new();
    for stack in stacks {
        if stack.is_empty() || stack.material.is_air() {
            continue;
        }
        match merged.binary_search_by_key(&stack.material, |existing| existing.material) {
            Ok(found) => {
                if merged[found].merge(stack).is_err() {
                    // Overflow: keep it separate rather than losing material.
                    merged.insert(found + 1, stack);
                }
            }
            Err(insert_at) => merged.insert(insert_at, stack),
        }
    }
    merged
}

/// Total units across a set of stacks, saturating rather than wrapping.
#[must_use]
pub fn total_units(stacks: &[Stack]) -> u64 {
    stacks.iter().map(|stack| u64::from(stack.units)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Cells, EMPTY_CELLS, SUBNODES_PER_BLOCK};

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);
    const GRASS: MaterialId = MaterialId(4);

    #[test]
    fn display_splits_units_into_blocks_and_nodes() {
        assert_eq!(display(0), (0, 0));
        assert_eq!(display(1), (0, 1));
        assert_eq!(display(26), (0, 26));
        assert_eq!(display(27), (1, 0));
        assert_eq!(display(28), (1, 1));
        assert_eq!(display(27 * 5 + 13), (5, 13));
    }

    #[test]
    fn a_stack_never_holds_air_or_nothing() {
        assert_eq!(Stack::new(MaterialId::AIR, 27), None);
        assert_eq!(Stack::new(STONE, 0), None);
        assert!(Stack::new(STONE, 1).is_some());
    }

    #[test]
    fn from_blocks_multiplies_by_twenty_seven() {
        assert_eq!(Stack::from_blocks(STONE, 3).expect("stack").units, 81);
        // And refuses to wrap.
        assert_eq!(Stack::from_blocks(STONE, u32::MAX), None);
    }

    #[test]
    fn breaking_a_solid_block_yields_twenty_seven_units() {
        let stacks = break_block(BlockView::Uniform(STONE));
        assert_eq!(
            stacks,
            vec![Stack {
                material: STONE,
                units: 27
            }]
        );
    }

    #[test]
    fn breaking_air_yields_nothing() {
        assert!(break_block(BlockView::Uniform(MaterialId::AIR)).is_empty());
    }

    #[test]
    fn breaking_a_partial_yields_its_popcount() {
        let stacks = break_block(BlockView::Partial {
            material: STONE,
            occupancy: 0b1011,
        });
        assert_eq!(
            stacks,
            vec![Stack {
                material: STONE,
                units: 3
            }]
        );
    }

    #[test]
    fn breaking_a_mixed_block_yields_one_stack_per_material() {
        let mut cells: Cells = EMPTY_CELLS;
        cells[0] = DIRT;
        cells[1] = STONE;
        cells[2] = STONE;
        cells[3] = GRASS;

        let stacks = break_block(BlockView::Mixed(&cells));
        assert_eq!(
            stacks,
            vec![
                Stack {
                    material: STONE,
                    units: 2
                },
                Stack {
                    material: DIRT,
                    units: 1
                },
                Stack {
                    material: GRASS,
                    units: 1
                },
            ]
        );
    }

    #[test]
    fn mixed_output_is_ordered_by_material_id_not_by_cell_position() {
        // The materials appear in descending id order in the cells; the output
        // must still be ascending. Drop order is observable, so it cannot
        // depend on where in the block a material happened to sit.
        let mut cells: Cells = EMPTY_CELLS;
        cells[0] = GRASS;
        cells[1] = DIRT;
        cells[2] = STONE;

        let stacks = break_block(BlockView::Mixed(&cells));
        let materials: Vec<_> = stacks.iter().map(|stack| stack.material).collect();
        assert_eq!(materials, vec![STONE, DIRT, GRASS]);
        assert!(materials.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_fully_mixed_block_still_totals_twenty_seven_units() {
        let mut cells: Cells = [STONE; SUBNODES_PER_BLOCK];
        cells[0] = DIRT;
        cells[26] = GRASS;
        let stacks = break_block(BlockView::Mixed(&cells));
        assert_eq!(total_units(&stacks), 27);
    }

    #[test]
    fn merge_adds_like_materials() {
        let mut stack = Stack {
            material: STONE,
            units: 10,
        };
        stack
            .merge(Stack {
                material: STONE,
                units: 5,
            })
            .expect("merge");
        assert_eq!(stack.units, 15);
    }

    #[test]
    fn merge_refuses_different_materials_and_leaves_the_stack_alone() {
        let mut stack = Stack {
            material: STONE,
            units: 10,
        };
        let err = stack
            .merge(Stack {
                material: DIRT,
                units: 5,
            })
            .expect_err("materials differ");
        assert!(matches!(err, StackError::MaterialMismatch { .. }));
        assert_eq!(stack.units, 10, "a failed merge must not alter the stack");
    }

    #[test]
    fn merge_refuses_to_overflow_rather_than_saturating() {
        // Saturating here would silently destroy material, which is exactly
        // what unit arithmetic exists to prevent.
        let mut stack = Stack {
            material: STONE,
            units: u32::MAX,
        };
        let err = stack
            .merge(Stack {
                material: STONE,
                units: 1,
            })
            .expect_err("should overflow");
        assert!(matches!(err, StackError::Overflow { .. }));
        assert_eq!(
            stack.units,
            u32::MAX,
            "a failed merge must not alter the stack"
        );
    }

    #[test]
    fn split_removes_units_and_returns_them() {
        let mut stack = Stack {
            material: STONE,
            units: 30,
        };
        let taken = stack.split(12).expect("split");
        assert_eq!(
            taken,
            Stack {
                material: STONE,
                units: 12
            }
        );
        assert_eq!(stack.units, 18);
    }

    #[test]
    fn split_of_the_whole_stack_empties_it() {
        let mut stack = Stack {
            material: STONE,
            units: 30,
        };
        let taken = stack.split(30).expect("split");
        assert_eq!(taken.units, 30);
        assert!(stack.is_empty());
    }

    #[test]
    fn split_refuses_to_take_more_than_is_there() {
        let mut stack = Stack {
            material: STONE,
            units: 5,
        };
        let err = stack.split(6).expect_err("insufficient");
        assert!(matches!(err, StackError::Insufficient { .. }));
        assert_eq!(stack.units, 5, "a failed split must not alter the stack");
    }

    #[test]
    fn consolidate_merges_and_orders() {
        let stacks = consolidate([
            Stack {
                material: GRASS,
                units: 3,
            },
            Stack {
                material: STONE,
                units: 4,
            },
            Stack {
                material: GRASS,
                units: 2,
            },
            Stack {
                material: STONE,
                units: 1,
            },
        ]);
        assert_eq!(
            stacks,
            vec![
                Stack {
                    material: STONE,
                    units: 5
                },
                Stack {
                    material: GRASS,
                    units: 5
                },
            ]
        );
    }

    #[test]
    fn consolidate_keeps_overflowing_amounts_separate_rather_than_losing_them() {
        let stacks = consolidate([
            Stack {
                material: STONE,
                units: u32::MAX,
            },
            Stack {
                material: STONE,
                units: 100,
            },
        ]);
        assert_eq!(stacks.len(), 2, "material must not be destroyed");
        assert_eq!(total_units(&stacks), u64::from(u32::MAX) + 100);
    }

    #[test]
    fn consolidate_drops_air_and_empty_stacks() {
        let stacks = consolidate([
            Stack {
                material: MaterialId::AIR,
                units: 27,
            },
            Stack {
                material: STONE,
                units: 0,
            },
            Stack {
                material: STONE,
                units: 3,
            },
        ]);
        assert_eq!(
            stacks,
            vec![Stack {
                material: STONE,
                units: 3
            }]
        );
    }
}
