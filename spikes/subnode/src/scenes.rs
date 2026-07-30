// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The four measurement scenes.
//!
//! Every scene is built through the public Task 02 API — no privileged access
//! to chunk internals. That is deliberate: if a scene cannot be expressed
//! through the mod-facing surface, that is itself a finding.
//!
//! Scene choice is the whole experiment. (a) is the floor, (d) is what players
//! will actually generate, (b) is a heavily-built world, and (c) is a shape
//! nothing produces naturally but which the storage layer must survive.

use tiamot_core::block::{EMPTY_CELLS, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BLOCKS_PER_CHUNK, BlockValue, CHUNK_BLOCKS, ChunkPos, MaterialId};

pub const STONE: MaterialId = MaterialId(2);
pub const DIRT: MaterialId = MaterialId(3);
pub const GRASS: MaterialId = MaterialId(4);
pub const WOOD: MaterialId = MaterialId(5);

/// Deterministic PRNG, so every run of the spike produces the same scenes and
/// therefore comparable numbers.
///
/// `SplitMix64`: three shifts and two multiplies, no dependency, and no
/// randomly-seeded hasher anywhere near a measurement.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound`.
    pub fn below(&mut self, bound: u32) -> u32 {
        (self.next_u64() % u64::from(bound)) as u32
    }

    /// True with probability `percent`/100.
    pub fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }
}

/// Which scene to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scene {
    /// (a) Flat uniform terrain slab. The floor: what the design costs when
    /// nobody has touched anything.
    Flat,
    /// (b) "Chiselled city" — every surface block Partial with a random 13/27
    /// occupancy. A heavily built-in world, not a pathological one.
    Chiselled,
    /// (c) 3D-checkerboard Mixed. The worst case the storage layer must
    /// survive; nothing generates this naturally.
    Checkerboard,
    /// (d) Realistic mix: 95% uniform, 4% partial, 1% mixed. The scene the
    /// KEEP gates are actually about.
    Realistic,
}

impl Scene {
    pub const ALL: [Self; 4] = [
        Self::Flat,
        Self::Chiselled,
        Self::Checkerboard,
        Self::Realistic,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Flat => "(a) flat",
            Self::Chiselled => "(b) chiselled",
            Self::Checkerboard => "(c) checkerboard",
            Self::Realistic => "(d) realistic",
        }
    }

    /// Builds the scene into a chunk at the origin.
    pub fn build(self, seed: u64) -> Chunk {
        let pos = ChunkPos::new(0, 0, 0);
        let mut rng = Rng::new(seed);
        match self {
            Self::Flat => flat(pos),
            Self::Chiselled => chiselled(pos, &mut rng),
            Self::Checkerboard => checkerboard(pos),
            Self::Realistic => realistic(pos, &mut rng),
        }
    }
}

/// Height of the terrain surface within a chunk, in blocks. Half-full is the
/// shape that produces the most surface area, which is what meshing costs
/// scale with.
const SURFACE_Y: u32 = 8;

/// (a) Solid below the surface, air above. One palette entry per material.
fn flat(pos: ChunkPos) -> Chunk {
    let mut chunk = Chunk::air(pos);
    for z in 0..CHUNK_BLOCKS {
        for y in 0..SURFACE_Y {
            for x in 0..CHUNK_BLOCKS {
                let material = if y == SURFACE_Y - 1 { GRASS } else { STONE };
                chunk.set_block_local(LocalBlock::new(x, y, z), BlockValue::Uniform(material));
            }
        }
    }
    chunk
}

/// (b) As (a), but every block on the surface layer is chiselled to a random
/// 13-of-27 occupancy.
///
/// 13/27 is chosen because it is the mask density that maximises face count:
/// roughly half occupied means roughly every occupied cell has an empty
/// neighbour, so almost every sub-node contributes faces. A denser or sparser
/// mask meshes to less.
fn chiselled(pos: ChunkPos, rng: &mut Rng) -> Chunk {
    let mut chunk = flat(pos);
    for z in 0..CHUNK_BLOCKS {
        for x in 0..CHUNK_BLOCKS {
            let occupancy = random_mask(rng, 13);
            chunk.set_block_local(
                LocalBlock::new(x, SURFACE_Y - 1, z),
                BlockValue::Partial {
                    material: GRASS,
                    occupancy,
                },
            );
        }
    }
    chunk
}

/// (c) Every block Mixed, alternating in a 3D checkerboard so no two
/// neighbours share contents and interning cannot collapse anything.
fn checkerboard(pos: ChunkPos) -> Chunk {
    let mut chunk = Chunk::air(pos);
    for index in 0..BLOCKS_PER_CHUNK {
        let local = LocalBlock::from_index(index);
        let parity = (local.x + local.y + local.z) % 2;
        let (primary, secondary) = if parity == 0 {
            (STONE, DIRT)
        } else {
            (DIRT, WOOD)
        };

        // Alternate materials cell by cell inside the block too, so every
        // sub-node face is a material boundary and nothing can merge.
        let mut cells = EMPTY_CELLS;
        for (cell_index, cell) in cells.iter_mut().enumerate() {
            let (cx, cy, cz) = tiamot_core::block::subnode_offset(cell_index);
            *cell = if (cx + cy + cz) % 2 == 0 {
                primary
            } else {
                secondary
            };
        }
        chunk.set_block_local(local, BlockValue::Cells(cells));
    }
    chunk
}

/// (d) 95% uniform, 4% partial, 1% mixed, applied to the surface region where
/// players actually build.
fn realistic(pos: ChunkPos, rng: &mut Rng) -> Chunk {
    let mut chunk = flat(pos);

    // Only blocks at or near the surface are candidates — nobody chisels
    // bedrock they cannot see.
    for z in 0..CHUNK_BLOCKS {
        for x in 0..CHUNK_BLOCKS {
            for y in (SURFACE_Y - 2)..SURFACE_Y {
                let roll = rng.below(100);
                let local = LocalBlock::new(x, y, z);
                if roll < 95 {
                    // Left uniform.
                } else if roll < 99 {
                    let bits = 13 + rng.below(10);
                    chunk.set_block_local(
                        local,
                        BlockValue::Partial {
                            material: GRASS,
                            occupancy: random_mask(rng, bits),
                        },
                    );
                } else {
                    let mut cells = [STONE; SUBNODES_PER_BLOCK];
                    for cell in &mut cells {
                        if rng.chance(30) {
                            *cell = DIRT;
                        } else if rng.chance(20) {
                            *cell = MaterialId::AIR;
                        }
                    }
                    chunk.set_block_local(local, BlockValue::Cells(cells));
                }
            }
        }
    }
    chunk
}

/// A random occupancy mask with exactly `bits` of the 27 sub-nodes set.
fn random_mask(rng: &mut Rng, bits: u32) -> u32 {
    let bits = bits.min(SUBNODES_PER_BLOCK as u32);
    let mut mask = 0u32;
    let mut set = 0;
    while set < bits {
        let bit = 1u32 << rng.below(SUBNODES_PER_BLOCK as u32);
        if mask & bit == 0 {
            mask |= bit;
            set += 1;
        }
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenes_are_deterministic() {
        // Every measurement in this spike depends on this.
        for scene in Scene::ALL {
            assert_eq!(
                scene.build(0xDEAD_BEEF),
                scene.build(0xDEAD_BEEF),
                "{} is not reproducible",
                scene.label()
            );
        }
    }

    #[test]
    fn scenes_have_the_shapes_they_claim() {
        let flat = Scene::Flat.build(1);
        assert_eq!(flat.mixed_len(), 0, "flat should need no mixed slots");

        let chiselled = Scene::Chiselled.build(1);
        assert_eq!(chiselled.mixed_len(), 0, "partial-only, no mixed");
        assert!(chiselled.palette_len() > flat.palette_len());

        let checkerboard = Scene::Checkerboard.build(1);
        assert!(
            checkerboard.mixed_len() > 1,
            "checkerboard should be mixed-heavy"
        );

        let realistic = Scene::Realistic.build(1);
        assert!(realistic.palette_len() > 2);
    }

    #[test]
    fn random_mask_sets_exactly_the_requested_bits() {
        let mut rng = Rng::new(7);
        for bits in 0..=27 {
            assert_eq!(random_mask(&mut rng, bits).count_ones(), bits);
        }
    }
}
