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

pub mod slots;

pub use slots::{
    Grab, MAX_VIEW_SLOTS, PLAYER_HOTBAR_SLOTS, PLAYER_MAIN, PLAYER_MAIN_SLOTS, PLAYER_OFFHAND_SLOT,
    Slots, View, ViewDef,
};

use crate::UNITS_PER_BLOCK;
use crate::block::{BlockView, SUBNODES_PER_BLOCK};
use crate::material::MaterialId;

/// A quantity of one material, measured in sub-node units.
///
/// Never holds air and never holds zero units: both are the absence of a stack
/// rather than a stack, and allowing them would mean every consumer had to
/// filter. [`Stack::new`] enforces this.
/// **Serialisable because an entity can BE a stack** — an item lying on the
/// ground persists with the chunk it is in. That path is the world's own
/// database and not a peer, so it is not hostile input (charter rule 14): what
/// comes off the wire is [`crate::proto::StackDef`], which the decoder checks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Stack {
    /// What this is made of.
    pub material: MaterialId,
    /// How much, in units. 27 units is one whole block.
    pub units: u32,
    /// The arrangement each item of this stack is cut to, if any.
    ///
    /// See [`Shape`]. `None` is loose material — what digging yields and what
    /// a full block places from.
    pub shape: Option<Shape>,
}

/// A sub-node arrangement a quantity of material has been cut to.
///
/// # Why a stack carries one at all
///
/// The engine's headline feature is that a block need not be a cube. Until now
/// that was only true of blocks in the WORLD: an inventory held loose material
/// and placing it made a cube, so a shape a player chiselled could not be kept,
/// carried, stacked or placed again.
///
/// A shape is a 27-bit occupancy mask over a block's sub-nodes — the same mask
/// [`crate::block::BlockContent::Partial`] stores, so placing one is writing
/// down what is already held rather than converting between two ideas of shape.
///
/// # What it costs, and why that keeps charter rule 5
///
/// One item of a shape costs [`Shape::cells`] units, so a stack's `units` is
/// still just units and conservation is still just addition. Ten items of a
/// five-cell shape is fifty units, and melting them back down gives fifty units
/// of loose material. **There is no second quantity and no exchange rate.**
///
/// # Stacking
///
/// Two stacks merge only if they are the same material AND the same shape.
/// That is the whole of "blocks crafted into the same shape stack": identical
/// things stack, and a stair and a slab of the same stone are not identical.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Shape(u32);

impl Shape {
    /// A shape from a 27-bit occupancy mask.
    ///
    /// Returns `None` for empty — nothing to hold — and for full, which is a
    /// whole block and is `None` by definition: a cube is loose material's own
    /// arrangement, and giving it a second spelling would mean a full block and
    /// twenty-seven units of the same stone did not stack.
    #[must_use]
    pub const fn new(occupancy: u32) -> Option<Self> {
        let mask = occupancy & Self::ALL;
        if mask == 0 || mask == Self::ALL {
            None
        } else {
            Some(Self(mask))
        }
    }

    /// Every sub-node of a block, as a mask.
    pub const ALL: u32 = (1 << UNITS_PER_BLOCK) - 1;

    /// The occupancy mask, for writing into a block.
    #[must_use]
    pub const fn occupancy(self) -> u32 {
        self.0
    }

    /// How many sub-nodes one item of this shape occupies, which is what one
    /// costs in units.
    #[must_use]
    pub const fn cells(self) -> u32 {
        self.0.count_ones()
    }
}

/// Why a stack operation could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StackError {
    /// Merging two stacks cut to different shapes.
    #[error("cannot merge a {left:?} with a {right:?}: different shapes")]
    ShapeMismatch {
        /// Shape of the stack being merged into.
        left: Option<Shape>,
        /// Shape of the stack being merged in.
        right: Option<Shape>,
    },

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
        (!material.is_air() && units > 0).then_some(Self {
            material,
            units,
            shape: None,
        })
    }

    /// A stack of `count` items cut to `shape`.
    ///
    /// The units follow from the shape: one item costs [`Shape::cells`] of
    /// them, so there is one quantity and no exchange rate.
    ///
    /// Returns `None` for air, zero, or an amount that overflows.
    #[must_use]
    pub fn shaped(material: MaterialId, shape: Shape, count: u32) -> Option<Self> {
        let units = count.checked_mul(shape.cells())?;
        Self::new(material, units).map(|stack| Self {
            shape: Some(shape),
            ..stack
        })
    }

    /// How many whole items this stack holds.
    ///
    /// For loose material that is whole blocks; for a shaped stack it is how
    /// many of that shape.
    #[must_use]
    pub const fn count(&self) -> u32 {
        let per = match self.shape {
            Some(shape) => shape.cells(),
            None => UNITS_PER_BLOCK,
        };
        // A shape with no cells is unrepresentable — `Shape::new` refuses an
        // empty mask — so this is a guard against a `Shape` that cannot exist
        // rather than a case anything reaches. `match` rather than
        // `checked_div().unwrap_or()` only because `unwrap_or` is not const.
        match self.units.checked_div(per) {
            Some(count) => count,
            None => 0,
        }
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
        // **Same material is not enough.** A stair and a slab of one stone are
        // not the same thing, and merging them would lose the shape of
        // whichever went second — the units would survive and the work would
        // not. This is the whole of "blocks crafted into the same shape stack".
        if self.shape != other.shape {
            return Err(StackError::ShapeMismatch {
                left: self.shape,
                right: other.shape,
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
            shape: self.shape,
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

    /// How many units of this a single slot holds.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        stack_capacity(self.shape)
    }
}

/// Splits a unit count into whole blocks and spare nodes.
///
/// How many items `units` of a material cut to `shape` is, or `None` for loose
/// material, where `shape` is the 27-bit mask the wire carries and `0` is loose.
///
/// **What a player counts is items, not units.** Charter rule 5's blocks and
/// spare nodes is the right answer for loose rubble and the wrong one for a
/// stack of stairs: thirteen units cut to a thirteen-cell shape is ONE stair,
/// and an interface that labelled it `+13` was telling a player they had
/// thirteen of something. Reported from the window, after making a cut and
/// finding nodes in the hotbar.
///
/// `None` rather than a count of one is the honest answer for loose material:
/// there is no item to count, which is exactly why the display differs.
#[must_use]
pub const fn items(units: u32, shape: u32) -> Option<u32> {
    if shape == 0 {
        return None;
    }
    // A shape with no cells cannot exist — `Shape::new` refuses an empty mask —
    // so the division is guarded against a value that never arrives rather than
    // a case anything reaches.
    units.checked_div(shape.count_ones())
}

/// Which way a shape's authored FRONT is pointing.
///
/// A cut is made in the shape editor, which shows the block from outside with
/// three faces labelled — front, top and side. Those labels are the only reason
/// a shape has an orientation at all: without them a stair placed with its step
/// facing a wall and one facing the room are the same object, and the player
/// has no way to ask for either.
///
/// **Front is `+z` and top is `+y`**, which is what the editor draws, and the
/// side it labels is `+x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facing {
    /// As authored.
    #[default]
    North,
    /// A quarter turn about the vertical axis: the front now points along `+x`.
    East,
    /// A half turn: the front points along `-z`.
    South,
    /// Three quarters: the front points along `-x`.
    West,
}

impl Facing {
    /// How many quarter turns from the authored orientation.
    #[must_use]
    pub const fn quarters(self) -> u32 {
        match self {
            Self::North => 0,
            Self::East => 1,
            Self::South => 2,
            Self::West => 3,
        }
    }

    /// The facing whose front points most nearly along `(dx, dz)`.
    ///
    /// Ties go to the `z` axis, which only matters on an exact diagonal and has
    /// to go somewhere; picking by a rule rather than by whichever comparison
    /// came first is what makes it the same answer on every machine.
    #[must_use]
    pub fn toward(dx: f32, dz: f32) -> Self {
        if dz.abs() >= dx.abs() {
            if dz >= 0.0 { Self::North } else { Self::South }
        } else if dx >= 0.0 {
            Self::East
        } else {
            Self::West
        }
    }
}

/// Turns an occupancy mask a quarter turn about the vertical axis, `quarters`
/// times.
///
/// A permutation of the twenty-seven cells and nothing more: no arithmetic, so
/// the cell count is preserved exactly and four turns are the identity. That is
/// what makes it safe in simulation — charter rule 4 has nothing to object to
/// in a table of bit moves.
#[must_use]
pub fn turned(mask: u32, quarters: u32) -> u32 {
    let mut mask = mask & Shape::ALL;
    for _ in 0..quarters % 4 {
        mask = permute(mask, |x, y, z| (z, y, 2 - x));
    }
    mask
}

/// Turns an occupancy mask a quarter turn about the `x` axis, `quarters` times.
///
/// One turn takes the front face to the top. Three take it to the bottom, which
/// is what a shape placed against a wall wants — see
/// [`crate::place::oriented`].
#[must_use]
pub fn tipped(mask: u32, quarters: u32) -> u32 {
    let mut mask = mask & Shape::ALL;
    for _ in 0..quarters % 4 {
        mask = permute(mask, |x, y, z| (x, z, 2 - y));
    }
    mask
}

/// Moves every set cell of `mask` to where `to` sends it.
fn permute(mask: u32, to: impl Fn(u32, u32, u32) -> (u32, u32, u32)) -> u32 {
    let mut out = 0;
    for index in 0..crate::UNITS_PER_BLOCK as usize {
        if mask & (1 << index) == 0 {
            continue;
        }
        let (x, y, z) = crate::block::subnode_offset(index);
        let (x, y, z) = to(x, y, z);
        out |= 1 << crate::block::subnode_index(x, y, z);
    }
    out
}

/// How many of a thing one slot holds.
///
/// **Counted in things, not units.** Ninety blocks of stone and ninety stairs
/// are both a full stack, which is what a player means by one — a cap written
/// in units would make a stack of stairs five times deeper than a stack of
/// slabs for no reason anyone could see.
///
/// A slot that is full does not refuse what will not fit: the rest goes into
/// the next slot, and the view grows if it has to. Nothing is ever lost to a
/// cap.
pub const ITEMS_PER_STACK: u32 = 90;

/// The unit cap for one slot holding material cut to `shape`.
///
/// `None` is loose material, where the thing being counted is a block.
#[must_use]
pub const fn stack_capacity(shape: Option<Shape>) -> u32 {
    let per_item = match shape {
        Some(shape) => shape.cells(),
        None => UNITS_PER_BLOCK,
    };
    ITEMS_PER_STACK * per_item
}

/// `(units / 27, units % 27)` — charter rule 5's display rule, in one place so
/// no caller open-codes the division.
#[must_use]
pub const fn display(units: u32) -> (u32, u32) {
    (units / UNITS_PER_BLOCK, units % UNITS_PER_BLOCK)
}

/// The occupancy mask for placing `units` sub-nodes of a material.
///
/// **Fills bottom-up: the whole bottom layer, then the next, then the top.**
/// Within a layer it goes in [`crate::block::subnode_index`] order, which is x
/// fastest then z. The order is documented because it is *observable* — a
/// player placing five spare nodes sees exactly which five cells appear, and a
/// mod computing the same shape has to be able to predict it.
///
/// Bottom-up rather than any other order because material placed against a
/// surface should rest on it. A fill that started at the top would leave spare
/// nodes floating with a gap underneath, which looks like a bug whatever the
/// documentation says.
///
/// `units` at or above [`UNITS_PER_BLOCK`] returns a full mask; the caller
/// turns that into a `Uniform` block rather than a `Partial` one.
#[must_use]
pub fn placement_mask(units: u32) -> u32 {
    if units >= UNITS_PER_BLOCK {
        return (1 << UNITS_PER_BLOCK) - 1;
    }

    let mut mask = 0;
    for index in fill_order().take(units as usize) {
        mask |= 1 << index;
    }
    mask
}

/// The order a block is filled in: bottom layer first, then the canonical index
/// order within each layer.
///
/// **Shared with `place::plan`**, which fills the GAPS in a partly-mined block
/// and has to use the same order or a half-placed block would look different
/// depending on how it got that way.
pub fn fill_order() -> impl Iterator<Item = usize> {
    (0..crate::SUBNODES_PER_AXIS).flat_map(|y| {
        (0..crate::SUBNODES_PER_AXIS).flat_map(move |z| {
            (0..crate::SUBNODES_PER_AXIS).map(move |x| crate::block::subnode_index(x, y, z))
        })
    })
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
                    // Loose, always: what comes out of a block is material, and
                    // the shape it happened to be arranged in belonged to the
                    // block rather than to the stone.
                    Err(insert_at) => stacks.insert(
                        insert_at,
                        Stack {
                            material,
                            units: 1,
                            shape: None,
                        },
                    ),
                }
            }
            stacks
        }
    }
}

/// What an edit removed from a block, as stacks.
///
/// Diffs the block's 27 cells before and after, and yields whatever went away.
/// General by construction rather than by case analysis: digging a whole block,
/// chiselling one sub-node, and replacing stone with wood are all the same
/// operation to this function, and none of them can be got wrong separately.
///
/// A cell whose material *changed* counts as removed, because the material that
/// was there is gone. Air is never yielded — there is nothing to pick up.
///
/// **Output order is ascending [`MaterialId`]**, for the same reason
/// [`break_block`] guarantees it: drop order is observable, so it must not
/// depend on anything that could differ between machines (charter rule 4).
#[must_use]
pub fn removed_units(before: BlockView<'_>, after: BlockView<'_>) -> Vec<Stack> {
    let mut stacks: Vec<Stack> = Vec::new();
    for index in 0..SUBNODES_PER_BLOCK {
        let was = before.subnode(index);
        if was.is_air() || was == after.subnode(index) {
            continue;
        }
        match stacks.binary_search_by_key(&was, |stack| stack.material) {
            Ok(found) => stacks[found].units += 1,
            Err(insert_at) => stacks.insert(
                insert_at,
                Stack {
                    material: was,
                    units: 1,
                    shape: None,
                },
            ),
        }
    }
    stacks
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
        // Keyed by material AND shape: two shapes of one stone are two stacks,
        // and sorting by the pair keeps the result ordered without a second
        // pass. Ascending material first, so drop order is unchanged for the
        // loose material that is almost all of it.
        let key = (stack.material, stack.shape);
        match merged.binary_search_by_key(&key, |existing| (existing.material, existing.shape)) {
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

/// Units of one material held across a set of stacks.
#[must_use]
pub fn units_of(stacks: &[Stack], material: MaterialId) -> u32 {
    stacks
        .iter()
        .filter(|stack| stack.material == material)
        .fold(0u32, |total, stack| total.saturating_add(stack.units))
}

/// Removes up to `units` of a material, returning how many were actually taken.
///
/// **Takes what is there rather than failing when it is short**, and the caller
/// acts on the returned count. That is what makes placing spare nodes work: a
/// player with 13 units who asks to place a block gets a `Partial` of 13 rather
/// than a refusal, which is the behaviour the task specifies. A caller that
/// needs all-or-nothing checks [`units_of`] first — the two are on the same
/// borrow, so nothing can change in between.
///
/// Empty stacks are dropped, so an inventory does not accumulate zero-unit
/// entries that display as materials the player does not have.
pub fn debit(stacks: &mut Vec<Stack>, material: MaterialId, units: u32) -> u32 {
    let mut remaining = units;
    for stack in stacks.iter_mut() {
        if remaining == 0 {
            break;
        }
        if stack.material != material {
            continue;
        }
        let take = stack.units.min(remaining);
        stack.units -= take;
        remaining -= take;
    }
    stacks.retain(|stack| !stack.is_empty());
    units - remaining
}

/// Where a mod reaches a player's inventory.
///
/// # Why this is a seam and not a field
///
/// Charter rule 1: the mod API is the only API, and crafting belongs in a mod.
/// A mod that turns twenty-seven units of stone into three stairs has to be
/// able to spend one and hand back the other — and inventories live on the
/// server's connection state, which `crates/core`'s script host has no business
/// reaching into. So the host holds a `dyn Access` the server fills in, the
/// same way [`crate::dig::Tools`] and [`crate::storage::Access`] work.
///
/// Every method takes the player's raw UUID bytes, because charter rule 13 is
/// that mod state keys on the UUID and never on the display name.
///
/// # It is the server's copy that changes
///
/// Nothing here goes near a client. A mod gives, the server's slots change, and
/// the player is told what they now have — the same direction digging and
/// placing already run in. An inventory a client could assert into is not an
/// inventory.
pub trait Access: Send + Sync {
    /// What a player holds in one view, consolidated: one stack per
    /// material-and-shape.
    ///
    /// Empty for a player who is not connected or a view that does not exist,
    /// which are the same answer to "what is in it" and neither is an error.
    fn contents(&self, player: [u8; 32], view: &str) -> Vec<Stack>;

    /// Puts a stack into a player's view.
    ///
    /// Returns whether it took. `false` means the player is not connected or
    /// the view does not exist — never that it did not fit, because
    /// [`crate::inventory::Slots::insert`] grows rather than refusing.
    fn give(&self, player: [u8; 32], view: &str, stack: Stack) -> bool;

    /// What a player is holding: the stack in the hotbar slot they selected.
    ///
    /// `None` for a player who is not connected, or an empty slot — which are
    /// the same answer to "what is in your hand" and neither is an error.
    ///
    /// **The slot is the client's own UI state**, sent when it changes (see
    /// `ClientMessage::SelectSlot`). Without it a mod could see what somebody
    /// OWNS and never what they are pointing with, which is the difference
    /// between an inventory and a weapon.
    fn held(&self, player: [u8; 32]) -> Option<Stack>;

    /// Takes up to `units` of one material and cut, returning how many it got.
    ///
    /// **Partial by design.** A crafting mod asking for more than the player
    /// has gets what there was and can decide whether to give it back; the
    /// alternative — all or nothing — hides the amount and makes the mod ask
    /// twice.
    fn take(
        &self,
        player: [u8; 32],
        view: &str,
        material: MaterialId,
        shape: Option<Shape>,
        units: u32,
    ) -> u32;
}

#[cfg(test)]
mod tests {

    #[test]
    fn four_quarter_turns_are_no_turn_at_all() {
        // The property that makes a rotation a permutation rather than a
        // transformation: nothing is lost and nothing is invented, so a shape
        // survives being turned round and round.
        for mask in [0b11111u32, 0b1010_1010_1010, 1 << 26, 0x7FF_FFFF, 1] {
            assert_eq!(turned(mask, 4), mask, "{mask:#029b} did not come home");
            assert_eq!(tipped(mask, 4), mask, "{mask:#029b} did not come home");
            for quarters in 0..4 {
                assert_eq!(
                    turned(mask, quarters).count_ones(),
                    mask.count_ones(),
                    "turning {mask:#029b} {quarters} times changed how much there is"
                );
                assert_eq!(
                    tipped(mask, quarters).count_ones(),
                    mask.count_ones(),
                    "tipping {mask:#029b} {quarters} times changed how much there is"
                );
            }
        }
    }

    #[test]
    fn a_turn_moves_the_front_to_the_side_and_back() {
        // One cell at the middle of the front face, followed round.
        let front = 1 << crate::block::subnode_index(1, 1, 2);
        let east = 1 << crate::block::subnode_index(2, 1, 1);
        let back = 1 << crate::block::subnode_index(1, 1, 0);
        let west = 1 << crate::block::subnode_index(0, 1, 1);
        assert_eq!(turned(front, 1), east);
        assert_eq!(turned(front, 2), back);
        assert_eq!(turned(front, 3), west);
    }

    #[test]
    fn three_tips_put_the_front_face_underneath() {
        // What a shape placed against a wall wants: the front pointing at the
        // player's feet.
        let front = 1 << crate::block::subnode_index(1, 1, 2);
        let below = 1 << crate::block::subnode_index(1, 0, 1);
        assert_eq!(tipped(front, 3), below);
        let above = 1 << crate::block::subnode_index(1, 2, 1);
        assert_eq!(tipped(front, 1), above, "one tip should go the other way");
    }

    #[test]
    fn a_facing_points_at_whoever_is_asking() {
        assert_eq!(Facing::toward(0.0, 4.0), Facing::North);
        assert_eq!(Facing::toward(0.0, -4.0), Facing::South);
        assert_eq!(Facing::toward(4.0, 0.0), Facing::East);
        assert_eq!(Facing::toward(-4.0, 0.0), Facing::West);
        // Mostly one way is that way.
        assert_eq!(Facing::toward(1.0, 3.0), Facing::North);
        assert_eq!(Facing::toward(-3.0, 1.0), Facing::West);
        // An exact diagonal has to go somewhere, and it goes to the same
        // somewhere every time.
        assert_eq!(Facing::toward(2.0, 2.0), Facing::North);
        assert_eq!(Facing::toward(2.0, 2.0), Facing::toward(2.0, 2.0));
    }

    #[test]
    fn a_turned_stair_is_a_stair_and_not_a_lump() {
        // A shape whose turns are all DIFFERENT, which a symmetrical one
        // cannot show: four distinct masks means the orientation is real and
        // not an expensive way of writing the same number.
        let stair = 0b111 | (0b111 << 9) | (0b111 << 12);
        let turns: std::collections::BTreeSet<u32> =
            (0..4).map(|quarters| turned(stair, quarters)).collect();
        assert_eq!(
            turns.len(),
            4,
            "a stair should look different from every side: {turns:?}"
        );
    }
    /// A stair-ish shape: the lower half plus one step. Five cells.
    fn stair() -> Shape {
        Shape::new(0b101).expect("two cells is a shape")
    }

    #[test]
    fn a_shape_is_neither_nothing_nor_a_whole_block() {
        // Empty has nothing to hold. Full is a cube, which is what loose
        // material already places — giving it a second spelling would mean a
        // full block and twenty-seven units of the same stone did not stack.
        assert!(Shape::new(0).is_none());
        assert!(Shape::new(Shape::ALL).is_none());
        assert!(Shape::new(0b111).is_some());

        // Bits past the block are not a shape's business.
        let clipped = Shape::new(0b1 | (1 << 30)).expect("the low bit survives");
        assert_eq!(clipped.occupancy(), 0b1);
    }

    #[test]
    fn one_item_of_a_shape_costs_its_cells_and_nothing_else() {
        // **Charter rule 5 with no exchange rate.** A stack's units are units,
        // whatever it is cut to, so conservation is still addition — and
        // melting a shaped stack down gives back exactly what went in.
        let shape = stair();
        let stack = Stack::shaped(MaterialId(3), shape, 10).expect("a shaped stack");
        assert_eq!(stack.units, 10 * shape.cells());
        assert_eq!(stack.count(), 10);

        // Loose material counts in whole blocks, which is the same rule read
        // with the block as the shape.
        let loose = Stack::from_blocks(MaterialId(3), 4).expect("four blocks");
        assert_eq!(loose.count(), 4);
        assert_eq!(loose.units, 4 * UNITS_PER_BLOCK);
    }

    #[test]
    fn the_same_shape_stacks_and_a_different_one_does_not() {
        // The whole of "blocks crafted into the same shape stack": identical
        // things stack, and a stair and a slab of one stone are not identical.
        let shape = stair();
        let other = Shape::new(0b11).expect("a different shape");

        let mut mine = Stack::shaped(MaterialId(3), shape, 2).expect("stack");
        assert!(
            mine.merge(Stack::shaped(MaterialId(3), shape, 3).expect("stack"))
                .is_ok()
        );
        assert_eq!(mine.count(), 5);

        // Same stone, different cut: refused, and the units of whichever went
        // second are NOT quietly folded in.
        let before = mine.units;
        assert!(matches!(
            mine.merge(Stack::shaped(MaterialId(3), other, 1).expect("stack")),
            Err(StackError::ShapeMismatch { .. })
        ));
        assert_eq!(mine.units, before, "a refused merge changed the stack");

        // And shaped never merges with loose, which is the case that would
        // otherwise silently turn somebody's work back into rubble.
        assert!(matches!(
            mine.merge(Stack::new(MaterialId(3), 27).expect("loose")),
            Err(StackError::ShapeMismatch { .. })
        ));
    }

    #[test]
    fn splitting_a_shaped_stack_keeps_the_shape() {
        let shape = stair();
        let mut stack = Stack::shaped(MaterialId(3), shape, 4).expect("stack");
        let taken = stack.split(shape.cells() * 2).expect("split");
        assert_eq!(taken.shape, Some(shape));
        assert_eq!(taken.count(), 2);
        assert_eq!(stack.count(), 2);
    }

    #[test]
    fn consolidating_keeps_shapes_apart() {
        // Two cuts of one stone are two stacks. Keyed by the PAIR, so this does
        // not depend on which order they arrive in.
        let shape = stair();
        let other = Shape::new(0b11).expect("a different shape");
        let merged = consolidate([
            Stack::shaped(MaterialId(3), shape, 1).expect("stack"),
            Stack::new(MaterialId(3), 27).expect("loose"),
            Stack::shaped(MaterialId(3), other, 1).expect("stack"),
            Stack::shaped(MaterialId(3), shape, 2).expect("stack"),
        ]);
        assert_eq!(merged.len(), 3, "got {merged:?}");
        let stairs = merged
            .iter()
            .find(|stack| stack.shape == Some(shape))
            .expect("the stairs");
        assert_eq!(stairs.count(), 3, "two arrivals of one shape should merge");
    }

    #[test]
    fn what_comes_out_of_a_block_is_loose_material() {
        // A shape belongs to the block, not to the stone. Digging a chiselled
        // block gives rubble, and the arrangement is somebody's work to redo —
        // which is what makes shape crafting worth doing at all.
        let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
        cells[0] = MaterialId(3);
        cells[4] = MaterialId(3);
        let dropped = break_block(BlockView::Mixed(&cells));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].shape, None);
        assert_eq!(dropped[0].units, 2);
    }

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
                units: 27,
                shape: None
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
                units: 3,
                shape: None
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
                    units: 2,
                    shape: None
                },
                Stack {
                    material: DIRT,
                    units: 1,
                    shape: None
                },
                Stack {
                    material: GRASS,
                    units: 1,
                    shape: None
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
            shape: None,
        };
        stack
            .merge(Stack {
                material: STONE,
                units: 5,
                shape: None,
            })
            .expect("merge");
        assert_eq!(stack.units, 15);
    }

    #[test]
    fn merge_refuses_different_materials_and_leaves_the_stack_alone() {
        let mut stack = Stack {
            material: STONE,
            units: 10,
            shape: None,
        };
        let err = stack
            .merge(Stack {
                material: DIRT,
                units: 5,
                shape: None,
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
            shape: None,
        };
        let err = stack
            .merge(Stack {
                material: STONE,
                units: 1,
                shape: None,
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
            shape: None,
        };
        let taken = stack.split(12).expect("split");
        assert_eq!(
            taken,
            Stack {
                material: STONE,
                units: 12,
                shape: None
            }
        );
        assert_eq!(stack.units, 18);
    }

    #[test]
    fn split_of_the_whole_stack_empties_it() {
        let mut stack = Stack {
            material: STONE,
            units: 30,
            shape: None,
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
            shape: None,
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
                shape: None,
            },
            Stack {
                material: STONE,
                units: 4,
                shape: None,
            },
            Stack {
                material: GRASS,
                units: 2,
                shape: None,
            },
            Stack {
                material: STONE,
                units: 1,
                shape: None,
            },
        ]);
        assert_eq!(
            stacks,
            vec![
                Stack {
                    material: STONE,
                    units: 5,
                    shape: None
                },
                Stack {
                    material: GRASS,
                    units: 5,
                    shape: None
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
                shape: None,
            },
            Stack {
                material: STONE,
                units: 100,
                shape: None,
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
                shape: None,
            },
            Stack {
                material: STONE,
                units: 0,
                shape: None,
            },
            Stack {
                material: STONE,
                units: 3,
                shape: None,
            },
        ]);
        assert_eq!(
            stacks,
            vec![Stack {
                material: STONE,
                units: 3,
                shape: None
            }]
        );
    }

    #[test]
    fn digging_a_whole_block_yields_exactly_one_block_of_units() {
        // Charter rule 5: 27 units is one block, and the display is
        // `units / 27` blocks + `units % 27` nodes. Nine of these is what
        // `mine_3x3.lua` asserts.
        let stone = MaterialId(2);
        let before = BlockView::Uniform(stone);
        let after = BlockView::Uniform(MaterialId::AIR);

        let yielded = removed_units(before, after);
        assert_eq!(yielded.len(), 1);
        assert_eq!(yielded[0].material, stone);
        assert_eq!(yielded[0].units, UNITS_PER_BLOCK);
        assert_eq!(yielded[0].display(), (1, 0), "one block, no spare nodes");
    }

    #[test]
    fn digging_one_subnode_yields_one_unit_not_one_block() {
        // The sub-node half of the 27-unit design. A yield that rounded up to a
        // whole block would let a player mine 27 blocks' worth of material by
        // chiselling 27 corners.
        let stone = MaterialId(2);
        let before_cells: Cells = [stone; SUBNODES_PER_BLOCK];
        let mut after_cells = before_cells;
        after_cells[13] = MaterialId::AIR;

        let yielded = removed_units(
            BlockView::Mixed(&before_cells),
            BlockView::Mixed(&after_cells),
        );
        assert_eq!(yielded.len(), 1);
        assert_eq!(yielded[0].units, 1, "one cell is one unit");
        assert_eq!(yielded[0].display(), (0, 1), "no blocks, one spare node");
    }

    #[test]
    fn twenty_seven_chiselled_nodes_make_exactly_one_block() {
        // The arithmetic that makes sub-node mining fair: chiselling a block
        // away one cell at a time must yield the same total as breaking it.
        let stone = MaterialId(2);
        let mut cells: Cells = [stone; SUBNODES_PER_BLOCK];
        let mut total = 0u32;

        for index in 0..SUBNODES_PER_BLOCK {
            let before_cells = cells;
            cells[index] = MaterialId::AIR;
            let yielded = removed_units(BlockView::Mixed(&before_cells), BlockView::Mixed(&cells));
            total += yielded.iter().map(|stack| stack.units).sum::<u32>();
        }

        assert_eq!(total, UNITS_PER_BLOCK);
        assert_eq!(
            display(total),
            (1, 0),
            "27 nodes is one block and no spares"
        );
    }

    #[test]
    fn replacing_a_material_yields_the_one_that_was_there() {
        // Not just digging: putting wood where stone was removes the stone.
        let stone = MaterialId(2);
        let wood = MaterialId(3);
        let yielded = removed_units(BlockView::Uniform(stone), BlockView::Uniform(wood));

        assert_eq!(yielded.len(), 1);
        assert_eq!(yielded[0].material, stone);
        assert_eq!(yielded[0].units, UNITS_PER_BLOCK);
    }

    #[test]
    fn an_unchanged_block_yields_nothing() {
        let stone = MaterialId(2);
        assert!(removed_units(BlockView::Uniform(stone), BlockView::Uniform(stone)).is_empty());
    }

    #[test]
    fn digging_air_yields_nothing() {
        // There is nothing to pick up, and a stack of air would be a bug that
        // propagated into every inventory that touched it.
        let air = BlockView::Uniform(MaterialId::AIR);
        assert!(removed_units(air, air).is_empty());
        assert!(removed_units(air, BlockView::Uniform(MaterialId(2))).is_empty());
    }

    #[test]
    fn a_mixed_block_yields_each_material_in_id_order() {
        // Drop order is observable — it decides which stack an almost-full
        // inventory keeps — so it must not depend on cell iteration order.
        let mut cells: Cells = EMPTY_CELLS;
        cells[0] = MaterialId(5);
        cells[1] = MaterialId(3);
        cells[2] = MaterialId(5);
        cells[3] = MaterialId(4);

        let yielded = removed_units(
            BlockView::Mixed(&cells),
            BlockView::Uniform(MaterialId::AIR),
        );
        let ids: Vec<u16> = yielded.iter().map(|stack| stack.material.0).collect();
        assert_eq!(ids, vec![3, 4, 5], "ascending material id");
        assert_eq!(yielded[2].units, 2, "two cells of material 5");
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    #[test]
    fn a_full_block_of_units_fills_every_cell() {
        assert_eq!(
            placement_mask(UNITS_PER_BLOCK).count_ones(),
            UNITS_PER_BLOCK
        );
        // And more than a block's worth does not overflow into nothing.
        assert_eq!(
            placement_mask(UNITS_PER_BLOCK + 5).count_ones(),
            UNITS_PER_BLOCK
        );
    }

    #[test]
    fn nothing_placed_is_an_empty_mask() {
        assert_eq!(placement_mask(0), 0);
    }

    #[test]
    fn spare_nodes_fill_the_bottom_layer_first() {
        // The documented order, and the one a player sees. Material placed
        // against a surface should rest on it — a fill that started at the top
        // would leave nodes floating with a gap under them.
        let mask = placement_mask(9);
        for z in 0..3 {
            for x in 0..3 {
                assert!(
                    mask & (1 << crate::block::subnode_index(x, 0, z)) != 0,
                    "cell ({x}, 0, {z}) should be filled"
                );
            }
        }
        assert_eq!(mask.count_ones(), 9, "exactly the bottom layer");

        // Nothing above it.
        for y in 1..3 {
            for z in 0..3 {
                for x in 0..3 {
                    assert!(
                        mask & (1 << crate::block::subnode_index(x, y, z)) == 0,
                        "cell ({x}, {y}, {z}) should be empty"
                    );
                }
            }
        }
    }

    #[test]
    fn a_partial_layer_fills_in_index_order_within_it() {
        // Five nodes: the whole bottom row of three, then two of the next.
        let mask = placement_mask(5);
        assert_eq!(mask.count_ones(), 5);
        for (x, z, expected) in [
            (0, 0, true),
            (1, 0, true),
            (2, 0, true),
            (0, 1, true),
            (1, 1, true),
            (2, 1, false),
        ] {
            let filled = mask & (1 << crate::block::subnode_index(x, 0, z)) != 0;
            assert_eq!(filled, expected, "cell ({x}, 0, {z})");
        }
    }

    #[test]
    fn every_count_places_exactly_that_many() {
        // The property that makes the 27-unit arithmetic hold: placing n units
        // consumes n and occupies n cells, with no rounding anywhere.
        for units in 0..=UNITS_PER_BLOCK {
            assert_eq!(
                placement_mask(units).count_ones(),
                units,
                "placing {units} units"
            );
        }
    }

    #[test]
    fn what_is_placed_is_what_breaking_it_gives_back() {
        // Conservation, which is the whole point of counting in units: place
        // n spare nodes, break the result, get n back. Contract §9 pairs with
        // the fill order here.
        for units in 1..UNITS_PER_BLOCK {
            let drops = break_block(BlockView::Partial {
                material: crate::MaterialId(7),
                occupancy: placement_mask(units),
            });
            let total: u32 = drops.iter().map(|stack| stack.units).sum();
            assert_eq!(total, units, "placing and breaking {units} units");
        }
    }
}
