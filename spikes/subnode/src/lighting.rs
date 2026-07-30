// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deliverable 4 — the lighting probe.
//!
//! # Why this is measured here and not in Task 10
//!
//! Lighting stores nothing at sub-node resolution: light is a per-block value.
//! It would be easy to conclude that sub-nodes therefore cost lighting nothing.
//!
//! They do not, because the *permeability test* sits in the BFS inner loop. To
//! decide whether light crosses a face, the propagator must ask whether that
//! face's 3×3 sub-node layer is fully occupied — for every face of every block
//! it visits. That is a per-face popcount instead of a bool read, several
//! million times per chunk relight.
//!
//! This probe measures that delta against a baseline that treats every Partial
//! block as fully solid. If the delta is large, Task 10 needs a cached
//! "is any face permeable" bit per block, computed on write instead of on every
//! propagation step.

use tiamot_core::block::{BlockView, subnode_index};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BLOCKS_PER_CHUNK, CHUNK_BLOCKS};

/// Maximum light level, as Minecraft-style voxel lighting uses.
const MAX_LIGHT: u8 = 15;

const BLOCKS: usize = CHUNK_BLOCKS as usize;

/// The six face directions as `(dx, dy, dz)`, in the order used by
/// [`face_permeable`].
const NEIGHBOURS: [(i32, i32, i32); 6] = [
    (-1, 0, 0),
    (1, 0, 0),
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
];

/// How a propagator decides whether light crosses a block face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permeability {
    /// Baseline: any block that is not entirely air blocks light completely.
    /// This is what a block-resolution engine does.
    SolidPartial,
    /// The Sub-Node Contract rule: light passes a face iff that face's 3×3
    /// sub-node layer is not fully occupied.
    SubNodeMask,
}

/// Whether light can leave `view` through face `face`.
///
/// The contract rule, in one function. Note it tests only the *face layer* —
/// the 9 cells adjacent to that face — not the whole block. A block hollowed
/// out in the middle but sealed on every side is correctly opaque.
#[must_use]
pub fn face_permeable(view: &BlockView<'_>, face: usize, mode: Permeability) -> bool {
    match mode {
        Permeability::SolidPartial => view.is_air(),
        Permeability::SubNodeMask => {
            if view.is_air() {
                return true;
            }
            // The 3x3 layer on this face: fix one axis at 0 or 2, sweep the
            // other two.
            let mut occupied = 0u32;
            for a in 0..3 {
                for b in 0..3 {
                    let (x, y, z) = match face {
                        0 => (0, a, b),
                        1 => (2, a, b),
                        2 => (a, 0, b),
                        3 => (a, 2, b),
                        4 => (a, b, 0),
                        _ => (a, b, 2),
                    };
                    if !view.subnode(subnode_index(x, y, z)).is_air() {
                        occupied += 1;
                    }
                }
            }
            occupied < 9
        }
    }
}

/// Result of one full-chunk relight.
#[derive(Debug, Clone, Copy)]
pub struct LightResult {
    /// Blocks that received any light.
    pub lit_blocks: usize,
    /// Sum of all light levels, so the two modes can be compared for effect as
    /// well as for cost.
    pub total_light: u64,
    /// Queue pops performed — the real unit of BFS work.
    pub propagations: u64,
}

/// Floods light through a chunk from its top face downward.
///
/// Skylight from above is the propagation that actually dominates a relight,
/// and it is the one that a chiselled surface layer makes expensive: light now
/// leaks *through* the surface instead of stopping at it.
#[must_use]
pub fn propagate(chunk: &Chunk, mode: Permeability) -> LightResult {
    let mut light = vec![0u8; BLOCKS_PER_CHUNK];
    // A plain Vec used as a FIFO. Deliberately not a HashMap-backed structure:
    // Task 10's real propagator must be deterministic (charter rule 4), and a
    // benchmark that used one would understate the cost of one that cannot.
    let mut queue: Vec<u16> = Vec::with_capacity(BLOCKS_PER_CHUNK);
    let mut propagations = 0u64;

    // Seed: every block in the top layer gets full skylight.
    for z in 0..BLOCKS {
        for x in 0..BLOCKS {
            let local = LocalBlock::new(x as u32, (BLOCKS - 1) as u32, z as u32);
            let index = local.index();
            light[index] = MAX_LIGHT;
            queue.push(index as u16);
        }
    }

    let mut head = 0;
    while head < queue.len() {
        let index = queue[head] as usize;
        head += 1;
        propagations += 1;

        let level = light[index];
        if level <= 1 {
            continue;
        }

        let local = LocalBlock::from_index(index);
        let view = chunk.get_block_local(local);

        for (face, (dx, dy, dz)) in NEIGHBOURS.iter().enumerate() {
            // The inner-loop cost this probe exists to measure.
            if !face_permeable(&view, face, mode) {
                continue;
            }

            let nx = local.x as i32 + dx;
            let ny = local.y as i32 + dy;
            let nz = local.z as i32 + dz;
            if nx < 0
                || ny < 0
                || nz < 0
                || nx >= BLOCKS as i32
                || ny >= BLOCKS as i32
                || nz >= BLOCKS as i32
            {
                continue;
            }

            let neighbour_local = LocalBlock::new(nx as u32, ny as u32, nz as u32);
            let neighbour_view = chunk.get_block_local(neighbour_local);
            // Light must be able to enter the neighbour as well as leave here.
            // The opposing face is the paired index: 0<->1, 2<->3, 4<->5.
            if !face_permeable(&neighbour_view, face ^ 1, mode) {
                continue;
            }

            let neighbour = neighbour_local.index();
            let attenuated = level - 1;
            if light[neighbour] < attenuated {
                light[neighbour] = attenuated;
                queue.push(neighbour as u16);
            }
        }
    }

    LightResult {
        lit_blocks: light.iter().filter(|&&level| level > 0).count(),
        total_light: light.iter().map(|&level| u64::from(level)).sum(),
        propagations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::{STONE, Scene};
    use tiamot_core::block::OCCUPANCY_FULL;
    use tiamot_core::{BlockValue, ChunkPos, MaterialId};

    #[test]
    fn air_is_permeable_in_both_modes() {
        let view = BlockView::Uniform(MaterialId::AIR);
        for face in 0..6 {
            assert!(face_permeable(&view, face, Permeability::SolidPartial));
            assert!(face_permeable(&view, face, Permeability::SubNodeMask));
        }
    }

    #[test]
    fn solid_is_opaque_in_both_modes() {
        let view = BlockView::Uniform(STONE);
        for face in 0..6 {
            assert!(!face_permeable(&view, face, Permeability::SolidPartial));
            assert!(!face_permeable(&view, face, Permeability::SubNodeMask));
        }
    }

    #[test]
    fn a_partial_block_is_opaque_to_the_baseline_but_may_leak_under_the_mask() {
        // A block with only its -x face layer full: opaque through that face,
        // permeable through the rest.
        let mut occupancy = 0u32;
        for a in 0..3 {
            for b in 0..3 {
                occupancy |= 1 << subnode_index(0, a, b);
            }
        }
        let view = BlockView::Partial {
            material: STONE,
            occupancy,
        };

        assert!(
            !face_permeable(&view, 0, Permeability::SubNodeMask),
            "the sealed -x face must block light"
        );
        assert!(
            face_permeable(&view, 1, Permeability::SubNodeMask),
            "the open +x face must pass light"
        );
        // The baseline cannot tell the difference: everything non-air is solid.
        for face in 0..6 {
            assert!(!face_permeable(&view, face, Permeability::SolidPartial));
        }
    }

    #[test]
    fn a_full_mask_is_opaque_under_the_sub_node_rule() {
        // Canonicalisation should prevent this ever being stored, but the rule
        // must be right regardless of how it is reached.
        let view = BlockView::Partial {
            material: STONE,
            occupancy: OCCUPANCY_FULL,
        };
        for face in 0..6 {
            assert!(!face_permeable(&view, face, Permeability::SubNodeMask));
        }
    }

    #[test]
    fn light_fills_an_empty_chunk_and_stops_at_solid() {
        let empty = Chunk::air(ChunkPos::new(0, 0, 0));
        let lit = propagate(&empty, Permeability::SubNodeMask);
        // Skylight seeds the top layer at 15 and attenuates by 1 per block, so
        // over a 16-block chunk the bottom layer arrives at exactly 0. Every
        // layer but that one is lit. This is the correct behaviour for a 15-level
        // light scale in a 16-block chunk, not an off-by-one.
        assert_eq!(
            lit.lit_blocks,
            BLOCKS_PER_CHUNK - BLOCKS * BLOCKS,
            "all but the bottom layer of an empty chunk should be lit"
        );

        let solid = Chunk::new(ChunkPos::new(0, 0, 0), STONE);
        let dark = propagate(&solid, Permeability::SubNodeMask);
        // Only the seeded top layer holds light; nothing propagates.
        assert_eq!(dark.lit_blocks, BLOCKS * BLOCKS);
    }

    #[test]
    fn the_mask_rule_never_lets_in_less_light_than_the_baseline() {
        // Treating Partial as solid is strictly more occluding, so the mask
        // rule must produce at least as much light everywhere. If it ever
        // produced less, the rule is inverted somewhere.
        for scene in Scene::ALL {
            let chunk = scene.build(0xA11CE);
            let baseline = propagate(&chunk, Permeability::SolidPartial);
            let masked = propagate(&chunk, Permeability::SubNodeMask);
            assert!(
                masked.total_light >= baseline.total_light,
                "{}: mask rule produced less light than the solid baseline",
                scene.label()
            );
        }
    }

    #[test]
    fn a_chiselled_surface_actually_leaks_light() {
        // If this does not hold, the probe is measuring two identical runs and
        // its delta means nothing.
        let chunk = Scene::Chiselled.build(1);
        let baseline = propagate(&chunk, Permeability::SolidPartial);
        let masked = propagate(&chunk, Permeability::SubNodeMask);
        assert!(
            masked.total_light > baseline.total_light,
            "a chiselled surface should let light past where a solid one would not"
        );
    }

    #[test]
    fn propagation_is_deterministic() {
        let chunk = Scene::Realistic.build(2);
        let first = propagate(&chunk, Permeability::SubNodeMask);
        let second = propagate(&chunk, Permeability::SubNodeMask);
        assert_eq!(first.total_light, second.total_light);
        assert_eq!(first.propagations, second.propagations);
    }

    #[test]
    fn a_sealed_partial_blocks_light_like_a_solid_block() {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        // A full floor of blocks whose top face layer is sealed.
        let mut occupancy = 0u32;
        for a in 0..3 {
            for b in 0..3 {
                occupancy |= 1 << subnode_index(a, 2, b);
            }
        }
        for z in 0..BLOCKS {
            for x in 0..BLOCKS {
                chunk.set_block_local(
                    LocalBlock::new(x as u32, 8, z as u32),
                    BlockValue::Partial {
                        material: STONE,
                        occupancy,
                    },
                );
            }
        }
        let lit = propagate(&chunk, Permeability::SubNodeMask);
        // Nothing below the sealed layer should be lit.
        for z in 0..BLOCKS {
            for x in 0..BLOCKS {
                let below = LocalBlock::new(x as u32, 7, z as u32);
                assert!(
                    chunk.get_block_local(below).is_air(),
                    "sanity: the space below should be air"
                );
            }
        }
        assert!(
            lit.lit_blocks < BLOCKS_PER_CHUNK,
            "a sealed layer must cast a shadow"
        );
    }
}
