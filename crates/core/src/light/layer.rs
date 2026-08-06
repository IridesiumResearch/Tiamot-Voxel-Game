// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Per-chunk light storage.

use crate::BLOCKS_PER_CHUNK;
use crate::coords::LocalBlock;

use super::Light;

/// The light levels of one chunk's blocks.
///
/// # Uniform until it is not
///
/// Almost every chunk is one level throughout: open sky is full daylight, and
/// everything below the surface is dark. Both are one word here rather than
/// 4,096. A chunk only allocates the dense array when a second distinct level
/// appears in it, and collapses back when it becomes uniform again.
///
/// That is the whole compression scheme, and it is deliberately not a palette:
/// the block palette exists because block content is an arbitrary
/// [`crate::block::BlockValue`] that is expensive to compare and store, whereas
/// a [`Light`] is a `u16`. A palette over `u16` keys costs an indirection to
/// save at most two bytes per distinct value, and the propagation loop writes
/// levels constantly — every write would have to intern, refcount, and possibly
/// free. The dense array is the right shape for a field that is written far
/// more often than it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightLayer {
    storage: Storage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Storage {
    /// Every block at this level.
    Uniform(Light),
    /// A level per block, indexed by [`LocalBlock::index`].
    Dense(Box<[Light; BLOCKS_PER_CHUNK]>),
}

impl LightLayer {
    /// A chunk at one level throughout.
    #[must_use]
    pub const fn uniform(level: Light) -> Self {
        Self {
            storage: Storage::Uniform(level),
        }
    }

    /// A chunk in the dark.
    #[must_use]
    pub const fn dark() -> Self {
        Self::uniform(Light::DARK)
    }

    /// The level at a chunk-local block.
    #[must_use]
    pub fn get(&self, local: LocalBlock) -> Light {
        match &self.storage {
            Storage::Uniform(level) => *level,
            Storage::Dense(levels) => levels[local.index()],
        }
    }

    /// Sets the level at a chunk-local block.
    ///
    /// Promotes to dense storage on the first write that differs from a uniform
    /// layer's level, and does nothing at all when the value is unchanged —
    /// which is most writes during propagation, and is what keeps a chunk of
    /// solid rock at one word through a full relight.
    pub fn set(&mut self, local: LocalBlock, level: Light) {
        match &mut self.storage {
            Storage::Uniform(current) => {
                if *current == level {
                    return;
                }
                let mut levels = Box::new([*current; BLOCKS_PER_CHUNK]);
                levels[local.index()] = level;
                self.storage = Storage::Dense(levels);
            }
            Storage::Dense(levels) => {
                levels[local.index()] = level;
            }
        }
    }

    /// The single level everywhere, if there is one.
    #[must_use]
    pub fn is_uniform(&self) -> Option<Light> {
        match &self.storage {
            Storage::Uniform(level) => Some(*level),
            Storage::Dense(levels) => {
                let first = levels[0];
                levels.iter().all(|level| *level == first).then_some(first)
            }
        }
    }

    /// Collapses dense storage back to uniform when every block agrees.
    ///
    /// **Not automatic on write.** Checking 4,096 entries after every `set`
    /// would turn a full relight from linear into quadratic; a relight calls
    /// this once when it finishes. The layer is correct either way — this is
    /// only about how much it costs to keep.
    pub fn compact(&mut self) {
        if let Some(level) = self.is_uniform() {
            self.storage = Storage::Uniform(level);
        }
    }

    /// Whether the layer is stored as a single level.
    ///
    /// For tests and for memory accounting; callers reading light should use
    /// [`LightLayer::get`] and not care.
    #[must_use]
    pub const fn is_compact(&self) -> bool {
        matches!(self.storage, Storage::Uniform(_))
    }

    /// Bytes this layer occupies beyond its own size.
    #[must_use]
    pub const fn memory_usage(&self) -> usize {
        match self.storage {
            Storage::Uniform(_) => 0,
            Storage::Dense(_) => BLOCKS_PER_CHUNK * size_of::<Light>(),
        }
    }

    /// Every level, in [`LocalBlock::index`] order.
    ///
    /// Materialises a uniform layer, so callers that only need one value should
    /// use [`LightLayer::get`]. Exists for hashing and serialisation, where the
    /// order has to be fixed and identical on every platform.
    #[must_use]
    pub fn levels(&self) -> Vec<Light> {
        match &self.storage {
            Storage::Uniform(level) => vec![*level; BLOCKS_PER_CHUNK],
            Storage::Dense(levels) => levels.to_vec(),
        }
    }

    /// Feeds the layer into a hasher in a fixed order.
    ///
    /// **Charter rule 4's territory.** The determinism gate hashes light, and a
    /// hash that depended on whether a layer happened to be stored uniform or
    /// dense would differ between a freshly generated chunk and the same chunk
    /// loaded from disk. So the uniform case is hashed as though it were dense.
    pub fn hash_into(&self, hasher: &mut blake3::Hasher) {
        match &self.storage {
            Storage::Uniform(level) => {
                for _ in 0..BLOCKS_PER_CHUNK {
                    hasher.update(&level.0.to_le_bytes());
                }
            }
            Storage::Dense(levels) => {
                for level in levels.iter() {
                    hasher.update(&level.0.to_le_bytes());
                }
            }
        }
    }
}

impl Default for LightLayer {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CHUNK_BLOCKS;
    use crate::light::MAX_LEVEL;

    fn at(x: u32, y: u32, z: u32) -> LocalBlock {
        LocalBlock::new(x, y, z)
    }

    #[test]
    fn a_fresh_layer_is_dark_everywhere_and_costs_nothing() {
        let layer = LightLayer::dark();
        assert!(layer.is_compact());
        assert_eq!(layer.memory_usage(), 0);
        assert_eq!(layer.get(at(0, 0, 0)), Light::DARK);
        assert_eq!(layer.get(at(15, 15, 15)), Light::DARK);
    }

    #[test]
    fn writing_the_level_it_already_holds_does_not_promote_it() {
        // The write that happens most during propagation: a block reached again
        // at a level it already has. Promoting on it would make a chunk of
        // solid rock allocate 8 KiB to hold the dark it started with.
        let mut layer = LightLayer::uniform(Light::DAYLIGHT);
        layer.set(at(4, 4, 4), Light::DAYLIGHT);
        assert!(
            layer.is_compact(),
            "an unchanged write allocated the dense array"
        );
    }

    #[test]
    fn a_differing_write_promotes_and_keeps_everything_else() {
        let mut layer = LightLayer::uniform(Light::DAYLIGHT);
        layer.set(at(1, 2, 3), Light::DARK);

        assert!(!layer.is_compact());
        assert_eq!(layer.get(at(1, 2, 3)), Light::DARK);
        assert_eq!(
            layer.get(at(1, 2, 4)),
            Light::DAYLIGHT,
            "promotion lost the level the rest of the chunk was at"
        );
        assert_eq!(layer.memory_usage(), BLOCKS_PER_CHUNK * 2);
    }

    #[test]
    fn compacting_is_only_possible_when_everything_agrees() {
        let mut layer = LightLayer::uniform(Light::DARK);
        layer.set(at(0, 0, 0), Light::DAYLIGHT);
        layer.compact();
        assert!(!layer.is_compact(), "compacted a layer holding two levels");

        // And back to one level: now it collapses, and the memory goes.
        layer.set(at(0, 0, 0), Light::DARK);
        layer.compact();
        assert!(layer.is_compact());
        assert_eq!(layer.memory_usage(), 0);
    }

    #[test]
    fn every_block_is_addressable_and_distinct() {
        // Catches an index formula that aliases two blocks — which as a
        // lighting bug looks like a lamp lighting a room somewhere else.
        let mut layer = LightLayer::dark();
        for x in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for z in 0..CHUNK_BLOCKS {
                    let level = Light::new((x % 16) as u8, (y % 16) as u8, (z % 16) as u8, 0);
                    layer.set(at(x, y, z), level);
                }
            }
        }
        for x in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for z in 0..CHUNK_BLOCKS {
                    assert_eq!(
                        layer.get(at(x, y, z)),
                        Light::new((x % 16) as u8, (y % 16) as u8, (z % 16) as u8, 0),
                        "block {x},{y},{z} read back as another block's level"
                    );
                }
            }
        }
    }

    #[test]
    fn a_uniform_layer_hashes_the_same_as_the_dense_layer_it_stands_for() {
        // Charter rule 4: the determinism gate hashes light, and the same world
        // must hash the same whether its layer was collapsed or not. A chunk
        // generated in memory and the same chunk loaded from disk can differ in
        // exactly this way.
        let uniform = LightLayer::uniform(Light::new(MAX_LEVEL, 0, 0, 0));

        let mut dense = LightLayer::uniform(Light::DARK);
        for index in 0..BLOCKS_PER_CHUNK {
            dense.set(
                LocalBlock::from_index(index),
                Light::new(MAX_LEVEL, 0, 0, 0),
            );
        }
        assert!(!dense.is_compact(), "the dense layer collapsed itself");

        let mut a = blake3::Hasher::new();
        uniform.hash_into(&mut a);
        let mut b = blake3::Hasher::new();
        dense.hash_into(&mut b);
        assert_eq!(
            a.finalize(),
            b.finalize(),
            "the same light hashed differently depending on how it was stored"
        );
    }
}
