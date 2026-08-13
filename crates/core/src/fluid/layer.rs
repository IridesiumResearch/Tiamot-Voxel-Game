// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Per-chunk fluid storage.

use crate::BLOCKS_PER_CHUNK;
use crate::coords::LocalBlock;

use super::Fluid;

/// What one chunk's blocks hold, or nothing at all.
///
/// # Empty is free, and that is the whole design
///
/// Light is stored for every chunk because every chunk has some. Fluid is not:
/// the overwhelming majority of chunks in any world contain no fluid ever, and
/// the ones that do are usually a puddle in a corner. So the empty case carries
/// **no allocation at all**, and a layer that drains back to empty gives its
/// memory up again.
///
/// That is also what makes the settled-world cost assertion meaningful. A world
/// where nothing is flowing holds no active blocks and allocates no dense
/// arrays, so "fluid costs nothing when nothing is happening" is a fact about
/// the data structure rather than a claim about the scheduler.
///
/// There is no uniform-and-full case on purpose. A chunk uniformly full of milk
/// is 4,096 identical bytes, and it is rare enough that carrying a third variant
/// to catch it would cost more in branches on every read than it ever saves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FluidLayer {
    storage: Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Storage {
    /// Nothing anywhere in the chunk.
    #[default]
    Empty,
    /// A value per block, indexed by [`LocalBlock::index`].
    Dense {
        blocks: Box<[Fluid; BLOCKS_PER_CHUNK]>,
        /// How many blocks are non-empty, so emptiness is O(1) rather than a
        /// scan of 4,096 bytes after every drain.
        filled: u32,
    },
}

impl FluidLayer {
    /// A chunk with no fluid in it.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            storage: Storage::Empty,
        }
    }

    /// What a chunk-local block holds.
    #[must_use]
    pub fn get(&self, local: LocalBlock) -> Fluid {
        match &self.storage {
            Storage::Empty => Fluid::EMPTY,
            Storage::Dense { blocks, .. } => blocks[local.index()],
        }
    }

    /// Sets what a chunk-local block holds.
    ///
    /// Returns whether anything changed, which the solver uses to decide what to
    /// broadcast and what to re-queue. Allocates on the first non-empty write
    /// and gives the allocation back when the last drop drains away.
    pub fn set(&mut self, local: LocalBlock, value: Fluid) -> bool {
        match &mut self.storage {
            Storage::Empty => {
                if value.is_empty() {
                    return false;
                }
                let mut blocks = Box::new([Fluid::EMPTY; BLOCKS_PER_CHUNK]);
                blocks[local.index()] = value;
                self.storage = Storage::Dense { blocks, filled: 1 };
                true
            }
            Storage::Dense { blocks, filled } => {
                let slot = &mut blocks[local.index()];
                if *slot == value {
                    return false;
                }
                if slot.is_empty() {
                    *filled += 1;
                } else if value.is_empty() {
                    *filled -= 1;
                }
                *slot = value;
                if *filled == 0 {
                    // **Given back rather than kept warm.** A channel that
                    // drains completely is the case the tests assert on, and a
                    // layer that stayed dense after it would make "settled milk
                    // costs nothing" false by four kilobytes per chunk it had
                    // ever touched.
                    self.storage = Storage::Empty;
                }
                true
            }
        }
    }

    /// Whether the chunk holds no fluid at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self.storage, Storage::Empty)
    }

    /// How many blocks hold something.
    #[must_use]
    pub const fn filled(&self) -> u32 {
        match &self.storage {
            Storage::Empty => 0,
            Storage::Dense { filled, .. } => *filled,
        }
    }

    /// Every block in index order.
    ///
    /// Yields [`Fluid::EMPTY`] for an empty layer rather than nothing, because
    /// the encoder needs a full chunk's worth either way.
    pub fn blocks(&self) -> impl Iterator<Item = Fluid> + '_ {
        (0..BLOCKS_PER_CHUNK).map(move |index| match &self.storage {
            Storage::Empty => Fluid::EMPTY,
            Storage::Dense { blocks, .. } => blocks[index],
        })
    }

    /// Builds a layer from values in [`LocalBlock::index`] order.
    ///
    /// Used by the decoder. Collapses to the empty representation if nothing in
    /// the sequence held anything, so a peer that sends an explicitly-empty
    /// chunk costs the receiver nothing.
    pub fn from_blocks(values: impl IntoIterator<Item = Fluid>) -> Self {
        let mut blocks = Box::new([Fluid::EMPTY; BLOCKS_PER_CHUNK]);
        let mut filled = 0;
        for (slot, value) in blocks.iter_mut().zip(values) {
            if !value.is_empty() {
                filled += 1;
            }
            *slot = value;
        }
        if filled == 0 {
            return Self::empty();
        }
        Self {
            storage: Storage::Dense { blocks, filled },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FluidId, MAX_LEVEL};
    use super::*;

    fn local(x: u32, y: u32, z: u32) -> LocalBlock {
        LocalBlock::new(x, y, z)
    }

    #[test]
    fn an_untouched_layer_holds_nothing_and_allocates_nothing() {
        let layer = FluidLayer::empty();
        assert!(layer.is_empty());
        assert_eq!(layer.filled(), 0);
        assert_eq!(layer.get(local(3, 4, 5)), Fluid::EMPTY);
        assert_eq!(std::mem::size_of_val(&layer.storage), size_of::<Storage>());
    }

    #[test]
    fn writing_nothing_to_an_empty_layer_does_not_make_it_dense() {
        // Otherwise every block edit anywhere would allocate four kilobytes to
        // record that there is still no milk.
        let mut layer = FluidLayer::empty();
        assert!(!layer.set(local(0, 0, 0), Fluid::EMPTY));
        assert!(layer.is_empty());
    }

    #[test]
    fn a_layer_that_drains_gives_its_memory_back() {
        // **The settled-world assertion, at the storage level.**
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        assert!(layer.set(local(1, 1, 1), Fluid::flowing(milk, 4)));
        assert!(layer.set(local(2, 1, 1), Fluid::source(milk)));
        assert!(!layer.is_empty());
        assert_eq!(layer.filled(), 2);

        assert!(layer.set(local(1, 1, 1), Fluid::EMPTY));
        assert_eq!(layer.filled(), 1);
        assert!(!layer.is_empty());

        assert!(layer.set(local(2, 1, 1), Fluid::EMPTY));
        assert_eq!(layer.filled(), 0);
        assert!(layer.is_empty(), "a drained chunk is still holding storage");
    }

    #[test]
    fn setting_the_same_value_twice_reports_no_change() {
        // What the solver uses to decide whether to broadcast, so a false
        // positive here is a packet per settled block per tick.
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        assert!(layer.set(local(0, 0, 0), Fluid::flowing(milk, 3)));
        assert!(!layer.set(local(0, 0, 0), Fluid::flowing(milk, 3)));
        assert!(layer.set(local(0, 0, 0), Fluid::flowing(milk, 2)));
    }

    #[test]
    fn a_layer_round_trips_through_its_blocks() {
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        layer.set(local(0, 0, 0), Fluid::source(milk));
        layer.set(local(15, 15, 15), Fluid::flowing(milk, MAX_LEVEL));
        layer.set(local(7, 2, 9), Fluid::flowing(milk, 1));

        let copy = FluidLayer::from_blocks(layer.blocks());
        assert_eq!(copy, layer);
        assert_eq!(copy.filled(), 3);
    }

    #[test]
    fn an_all_empty_sequence_decodes_to_the_empty_representation() {
        let layer = FluidLayer::from_blocks(std::iter::repeat_n(Fluid::EMPTY, BLOCKS_PER_CHUNK));
        assert!(layer.is_empty());
    }
}
