// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Plans: a box of blocks, captured from the world and stamped back.
//!
//! # What a plan is for
//!
//! Building the same thing twice. A mod that wants villages, dungeons, ships or
//! a player's saved house needs to read a region once and write it somewhere
//! else, and doing that through `game.get_block` and `game.set_block` means a
//! mod holding tens of thousands of Lua tables and walking them by hand.
//!
//! # Names, not numbers
//!
//! A plan stores material NAMES (charter rule 8). Numeric ids are per session:
//! a plan captured today and stamped tomorrow, or shared between two people
//! whose mod sets load in a different order, would otherwise stamp a house made
//! of the wrong materials — and it would look like a corrupt file rather than
//! like an id table doing exactly what it was documented to do.
//!
//! The names live in a palette rather than on every cell, because a wall is
//! thousands of cells of one material and storing that name thousands of times
//! is the difference between a plan that fits in memory and one that does not.
//!
//! # Sparse, because most of a box is air
//!
//! A plan records only the cells that hold something. A hollow building is
//! mostly air, and a dense representation would spend its whole size on the
//! nothing inside. [`Plan::AIR_IS_ABSENT`] says what that means when stamping.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The largest a plan may be on any axis.
///
/// **A bound, not a preference.** A capture walks the box it is given, so an
/// unbounded one is an unbounded read inside a mod call inside the tick. 64 is
/// four chunks on a side — big enough for a building and small enough that the
/// worst case is bounded at a quarter of a million positions.
pub const MAX_SIDE: u16 = 64;

/// The most occupied cells a plan may hold.
///
/// Separate from [`MAX_SIDE`] because the two bound different things: the side
/// bounds how far a capture walks, and this bounds how much it can come back
/// with. A 64³ box of solid rock is 262,144 cells and would be a plan nothing
/// could stamp inside a tick budget.
pub const MAX_CELLS: usize = 65_536;

/// Why a plan could not be built or read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    /// A side is zero, or longer than [`MAX_SIDE`].
    #[error("a plan is 1..={MAX_SIDE} blocks on a side, not {side} on {axis}")]
    Side {
        /// Which axis, as `"x"`, `"y"` or `"z"`.
        axis: &'static str,
        /// What was asked for.
        side: u32,
    },
    /// More occupied cells than [`MAX_CELLS`].
    #[error("a plan holds at most {MAX_CELLS} blocks, and this region has more")]
    TooManyCells,
    /// A cell outside the plan's own box.
    #[error("({x}, {y}, {z}) is outside a plan of {size:?}")]
    OutOfBounds {
        /// The offending offset.
        x: u16,
        /// The offending offset.
        y: u16,
        /// The offending offset.
        z: u16,
        /// The plan's size.
        size: [u16; 3],
    },
    /// More distinct materials than a palette index can name.
    #[error("a plan holds at most {} distinct materials", u16::MAX)]
    PaletteFull,
}

/// One occupied block of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanCell {
    /// Offset from the plan's origin.
    pub at: [u16; 3],
    /// Index into [`Plan::palette`].
    pub material: u16,
    /// Which of the 27 sub-node cells are filled, indexed by
    /// [`crate::block::subnode_index`]. `0x07FF_FFFF` is a whole block.
    pub occupancy: u32,
}

/// A box of blocks, captured from the world and stampable back into it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    size: [u16; 3],
    palette: Vec<String>,
    cells: Vec<(u32, u16, u32)>,
}

impl Plan {
    /// What an absent cell means when a plan is stamped: **nothing is written**.
    ///
    /// A plan is sparse, so "this cell holds nothing" and "this cell was not
    /// captured" are the same state and cannot be told apart. Stamping
    /// therefore ADDS a building to a hillside rather than cutting a box out of
    /// it and putting the building in the hole.
    ///
    /// That is the more useful of the two and it is the one that can be built
    /// on: a mod that wants the hole can clear the box itself first, and a mod
    /// that wanted the additive behaviour could not have undone a clearing one.
    pub const AIR_IS_ABSENT: bool = true;

    /// An empty plan of this size.
    ///
    /// # Errors
    ///
    /// [`PlanError::Side`] if an axis is zero or over [`MAX_SIDE`].
    pub fn new(size: [u16; 3]) -> Result<Self, PlanError> {
        for (axis, side) in ["x", "y", "z"].into_iter().zip(size) {
            if side == 0 || side > MAX_SIDE {
                return Err(PlanError::Side {
                    axis,
                    side: u32::from(side),
                });
            }
        }
        Ok(Self {
            size,
            palette: Vec::new(),
            cells: Vec::new(),
        })
    }

    /// The plan's extent in blocks.
    #[must_use]
    pub const fn size(&self) -> [u16; 3] {
        self.size
    }

    /// How many occupied blocks it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether it holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The material names this plan uses, in palette order.
    #[must_use]
    pub fn palette(&self) -> &[String] {
        &self.palette
    }

    /// Records an occupied block.
    ///
    /// An occupancy of zero is a block holding nothing, which is recorded as
    /// absence — see [`Plan::AIR_IS_ABSENT`] — so writing one is not an error
    /// and not a cell.
    ///
    /// # Errors
    ///
    /// [`PlanError::OutOfBounds`], [`PlanError::TooManyCells`] or
    /// [`PlanError::PaletteFull`].
    pub fn set(&mut self, at: [u16; 3], material: &str, occupancy: u32) -> Result<(), PlanError> {
        let [x, y, z] = at;
        if x >= self.size[0] || y >= self.size[1] || z >= self.size[2] {
            return Err(PlanError::OutOfBounds {
                x,
                y,
                z,
                size: self.size,
            });
        }
        if occupancy == 0 {
            return Ok(());
        }
        if self.cells.len() >= MAX_CELLS {
            return Err(PlanError::TooManyCells);
        }
        let index = if let Some(index) = self.palette.iter().position(|name| name == material) {
            u16::try_from(index).map_err(|_| PlanError::PaletteFull)?
        } else {
            let index = u16::try_from(self.palette.len()).map_err(|_| PlanError::PaletteFull)?;
            self.palette.push(material.to_owned());
            index
        };
        self.cells.push((self.offset_of(at), index, occupancy));
        Ok(())
    }

    /// Every occupied block, in the order it was recorded.
    pub fn cells(&self) -> impl Iterator<Item = PlanCell> + '_ {
        self.cells.iter().map(|&(offset, material, occupancy)| {
            let x = offset % u32::from(self.size[0]);
            let y = (offset / u32::from(self.size[0])) % u32::from(self.size[1]);
            let z = offset / (u32::from(self.size[0]) * u32::from(self.size[1]));
            PlanCell {
                at: [x as u16, y as u16, z as u16],
                material,
                occupancy,
            }
        })
    }

    /// How many of each material it holds, by name.
    ///
    /// What a mod needs to say "you do not have enough stone for this" before
    /// stamping rather than after.
    #[must_use]
    pub fn tally(&self) -> BTreeMap<&str, usize> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for &(_, material, _) in &self.cells {
            if let Some(name) = self.palette.get(material as usize) {
                *counts.entry(name.as_str()).or_default() += 1;
            }
        }
        counts
    }

    fn offset_of(&self, [x, y, z]: [u16; 3]) -> u32 {
        u32::from(x)
            + u32::from(self.size[0]) * (u32::from(y) + u32::from(self.size[1]) * u32::from(z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_of_zero_or_more_than_the_maximum_is_refused() {
        // A capture walks the box it is given, so an unbounded side is an
        // unbounded read inside a mod call inside the tick.
        assert!(Plan::new([1, 1, 1]).is_ok());
        assert!(Plan::new([MAX_SIDE, MAX_SIDE, MAX_SIDE]).is_ok());
        assert_eq!(
            Plan::new([0, 4, 4]),
            Err(PlanError::Side { axis: "x", side: 0 })
        );
        assert_eq!(
            Plan::new([4, MAX_SIDE + 1, 4]),
            Err(PlanError::Side {
                axis: "y",
                side: u32::from(MAX_SIDE) + 1,
            })
        );
    }

    #[test]
    fn a_cell_round_trips_through_its_offset() {
        // The offset is packed and unpacked by two separate expressions, which
        // is exactly the shape that agrees for a cube and disagrees for
        // anything else. A deliberately unequal size is the case that catches
        // an axis order swapped between them.
        let mut plan = Plan::new([5, 3, 7]).expect("a valid size");
        for at in [[0, 0, 0], [4, 2, 6], [1, 2, 3], [4, 0, 0], [0, 2, 0]] {
            plan.set(at, "core:stone", 0x07FF_FFFF).expect("in bounds");
        }
        let read: Vec<[u16; 3]> = plan.cells().map(|cell| cell.at).collect();
        assert_eq!(
            read,
            vec![[0, 0, 0], [4, 2, 6], [1, 2, 3], [4, 0, 0], [0, 2, 0]]
        );
    }

    #[test]
    fn air_is_absence_rather_than_a_cell() {
        // A plan is sparse, so "holds nothing" and "was not captured" are one
        // state. Recording empty blocks would make a hollow building cost its
        // whole bounding box.
        let mut plan = Plan::new([4, 4, 4]).expect("a valid size");
        plan.set([1, 1, 1], "core:stone", 0).expect("not an error");
        assert!(plan.is_empty(), "an empty block was recorded as a cell");
        assert!(plan.palette().is_empty(), "and it claimed a palette entry");
    }

    #[test]
    fn the_palette_holds_each_name_once() {
        // A wall is thousands of cells of one material. Storing the name per
        // cell is the difference between a plan that fits in memory and one
        // that does not.
        let mut plan = Plan::new([8, 8, 8]).expect("a valid size");
        for x in 0..8u16 {
            plan.set([x, 0, 0], "core:stone", 1).expect("in bounds");
            plan.set([x, 1, 0], "core:wood", 1).expect("in bounds");
        }
        assert_eq!(plan.palette(), &["core:stone", "core:wood"]);
        assert_eq!(plan.len(), 16);
        assert_eq!(plan.tally().get("core:stone"), Some(&8));
        assert_eq!(plan.tally().get("core:wood"), Some(&8));
    }

    #[test]
    fn a_cell_outside_the_box_is_refused() {
        let mut plan = Plan::new([4, 4, 4]).expect("a valid size");
        assert!(plan.set([3, 3, 3], "core:stone", 1).is_ok());
        assert!(matches!(
            plan.set([4, 0, 0], "core:stone", 1),
            Err(PlanError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn a_plan_survives_being_written_down() {
        // Plans persist and are shared, so the encoding is part of the feature
        // rather than an implementation detail.
        let mut plan = Plan::new([3, 2, 4]).expect("a valid size");
        plan.set([2, 1, 3], "core:stone", 0x07FF_FFFF)
            .expect("in bounds");
        plan.set([0, 0, 0], "core:wood", 0b101).expect("in bounds");

        let bytes = postcard::to_allocvec(&plan).expect("encode");
        let back: Plan = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, plan);
        assert_eq!(back.size(), [3, 2, 4]);
        let cells: Vec<PlanCell> = back.cells().collect();
        assert_eq!(cells[0].at, [2, 1, 3]);
        assert_eq!(cells[1].occupancy, 0b101);
    }
}
