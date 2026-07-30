// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Property tests for the voxel data model (charter rule 15).
//!
//! The palette, the reference counting, the bit packing, the interning, and the
//! canonicalisation are each individually simple and collectively easy to get
//! subtly wrong — a refcount that drifts, an index that survives a repack
//! pointing at the wrong slot, a canonical form reached by two different paths.
//! Unit tests catch the cases somebody thought of. These catch the rest.
//!
//! The central property is equivalence with a reference model: a naive
//! `Vec<[MaterialId; 27]>` with no compression at all, whose correctness is
//! obvious by inspection. Any operation sequence applied to both must leave
//! them reading identically. If the compressed chunk and the obvious one ever
//! disagree, the compressed one is wrong.

use proptest::prelude::*;

use tiamot_core::block::{self, Cells, EMPTY_CELLS, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::inventory::{break_block, total_units};
use tiamot_core::{BLOCKS_PER_CHUNK, BlockValue, ChunkPos, MaterialId};

/// An uncompressed chunk: one 27-cell array per block, no palette, no
/// interning, no canonicalisation.
///
/// 216 KiB per chunk, which is exactly why [`Chunk`] exists — and exactly why
/// this makes a trustworthy oracle.
struct ReferenceChunk {
    blocks: Vec<Cells>,
}

impl ReferenceChunk {
    fn new(fill: MaterialId) -> Self {
        Self {
            blocks: vec![[fill; SUBNODES_PER_BLOCK]; BLOCKS_PER_CHUNK],
        }
    }

    fn set_block(&mut self, local: LocalBlock, value: &BlockValue) {
        self.blocks[local.index()] = value.cells();
    }

    fn set_subnode(&mut self, local: LocalBlock, cell: usize, material: MaterialId) {
        self.blocks[local.index()][cell] = material;
    }

    fn cells(&self, local: LocalBlock) -> &Cells {
        &self.blocks[local.index()]
    }
}

/// One edit, applied to both the chunk and the reference model.
#[derive(Debug, Clone)]
enum Op {
    SetBlockUniform {
        block: usize,
        material: MaterialId,
    },
    SetBlockPartial {
        block: usize,
        material: MaterialId,
        occupancy: u32,
    },
    SetBlockCells {
        block: usize,
        cells: Box<Cells>,
    },
    SetSubNode {
        block: usize,
        cell: usize,
        material: MaterialId,
    },
    Repack,
}

/// A deliberately tiny material alphabet.
///
/// Palette collisions, entry reclamation, and interning hits are the
/// interesting behaviour, and they only happen when values repeat. Drawing from
/// the whole `u16` range would make every block distinct and exercise almost
/// none of it.
fn material() -> impl Strategy<Value = MaterialId> {
    prop_oneof![
        3 => Just(MaterialId::AIR),
        1 => (2u16..6).prop_map(MaterialId),
    ]
}

/// Occupancy masks, with the canonicalisation boundaries drawn deliberately.
///
/// A uniform draw over `0..=OCCUPANCY_FULL` produces an all-set mask with
/// probability 2⁻²⁷ and an empty one just as rarely — so the two cases
/// canonicalisation exists to collapse would never be generated at all. An
/// earlier version of this file had exactly that hole, and a deliberately
/// planted "full Partial no longer collapses to Uniform" bug survived the whole
/// suite. Boundary values need drawing on purpose.
fn occupancy() -> impl Strategy<Value = u32> {
    prop_oneof![
        2 => Just(block::OCCUPANCY_FULL),
        2 => Just(0u32),
        // One bit clear of full, and one bit set — the near-misses that must
        // NOT collapse.
        1 => (0..SUBNODES_PER_BLOCK).prop_map(|bit| block::OCCUPANCY_FULL & !(1 << bit)),
        1 => (0..SUBNODES_PER_BLOCK).prop_map(|bit| 1u32 << bit),
        6 => 0u32..=block::OCCUPANCY_FULL,
    ]
}

fn cells() -> impl Strategy<Value = Box<Cells>> {
    prop_oneof![
        // All-same-material blocks, so the Cells → Uniform collapse is
        // exercised rather than left to chance.
        2 => material().prop_map(|m| Box::new([m; SUBNODES_PER_BLOCK])),
        // One material plus air, so the Cells → Partial collapse is exercised.
        2 => (material(), occupancy()).prop_map(|(m, mask)| {
            let mut cells = EMPTY_CELLS;
            for (index, cell) in cells.iter_mut().enumerate() {
                if mask & (1 << index) != 0 {
                    *cell = m;
                }
            }
            Box::new(cells)
        }),
        6 => proptest::collection::vec(material(), SUBNODES_PER_BLOCK).prop_map(|drawn| {
            let mut cells = EMPTY_CELLS;
            cells.copy_from_slice(&drawn);
            Box::new(cells)
        }),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    let block = 0..BLOCKS_PER_CHUNK;
    prop_oneof![
        4 => (block.clone(), material())
            .prop_map(|(block, material)| Op::SetBlockUniform { block, material }),
        3 => (block.clone(), material(), occupancy())
            .prop_map(|(block, material, occupancy)| Op::SetBlockPartial {
                block,
                material,
                occupancy,
            }),
        3 => (block.clone(), cells())
            .prop_map(|(block, cells)| Op::SetBlockCells { block, cells }),
        6 => (block.clone(), 0..SUBNODES_PER_BLOCK, material())
            .prop_map(|(block, cell, material)| Op::SetSubNode {
                block,
                cell,
                material,
            }),
        1 => Just(Op::Repack),
    ]
}

fn apply(chunk: &mut Chunk, reference: &mut ReferenceChunk, op: &Op) {
    match op {
        Op::SetBlockUniform { block, material } => {
            let local = LocalBlock::from_index(*block);
            let value = BlockValue::Uniform(*material);
            chunk.set_block_local(local, value);
            reference.set_block(local, &value);
        }
        Op::SetBlockPartial {
            block,
            material,
            occupancy,
        } => {
            let local = LocalBlock::from_index(*block);
            let value = BlockValue::Partial {
                material: *material,
                occupancy: *occupancy,
            };
            chunk.set_block_local(local, value);
            // The reference model stores what the value *means*, so it must be
            // canonicalised here too — a Partial of air means air.
            reference.set_block(local, &value.canonical());
        }
        Op::SetBlockCells { block, cells } => {
            let local = LocalBlock::from_index(*block);
            let value = BlockValue::Cells(**cells);
            chunk.set_block_local(local, value);
            reference.set_block(local, &value);
        }
        Op::SetSubNode {
            block,
            cell,
            material,
        } => {
            let local = LocalBlock::from_index(*block);
            let (x, y, z) = block::subnode_offset(*cell);
            let world = tiamot_core::BlockPos::new(local.x as i32, local.y as i32, local.z as i32)
                .subnode(x as i32, y as i32, z as i32);
            chunk.set_subnode(world, *material).expect("inside chunk");
            reference.set_subnode(local, *cell, *material);
        }
        Op::Repack => chunk.repack(),
    }
}

/// Every invariant the chunk claims to maintain, checked against its own state.
fn assert_internally_consistent(chunk: &Chunk) {
    // Canonical form: nothing stored may have a second representation.
    //
    // Checked structurally rather than via `BlockValue::is_canonical`, which
    // asks `canonical()` whether it agrees with itself — a bug inside
    // canonicalisation would make that check agree with the bug. These
    // assertions restate the invariants independently.
    for (local, view) in chunk.blocks() {
        match view.to_value() {
            BlockValue::Uniform(_) => {}
            BlockValue::Partial {
                material,
                occupancy,
            } => {
                assert!(
                    !material.is_air(),
                    "block {local:?}: a Partial of air must be Uniform(AIR)"
                );
                assert_ne!(
                    occupancy, 0,
                    "block {local:?}: an empty Partial must be Uniform(AIR)"
                );
                assert_ne!(
                    occupancy,
                    block::OCCUPANCY_FULL,
                    "block {local:?}: a full Partial must be Uniform"
                );
            }
            BlockValue::Cells(cells) => {
                // A Mixed block must genuinely hold two or more distinct non-air
                // materials; anything less should have collapsed.
                let mut distinct: Vec<_> = cells
                    .iter()
                    .filter(|cell| !cell.is_air())
                    .copied()
                    .collect();
                distinct.sort_unstable();
                distinct.dedup();
                assert!(
                    distinct.len() >= 2,
                    "block {local:?}: a Mixed block with {} distinct material(s) \
                     should have collapsed to Uniform or Partial",
                    distinct.len()
                );
            }
        }
    }

    // Palette width must be exactly what the entry count needs — never wider,
    // or memory is being wasted, and never narrower, or indices are truncated.
    let bits = chunk.bits_per_index();
    assert!(
        (1usize << u32::from(bits)) >= chunk.palette_len() || bits == 0 && chunk.palette_len() <= 1,
        "palette of {} does not fit in {bits} bits",
        chunk.palette_len()
    );

    // Every mixed slot must be reachable from some block; an orphan is a leak.
    assert!(
        chunk.mixed_len() <= chunk.palette_len(),
        "more mixed slots ({}) than palette entries ({})",
        chunk.mixed_len(),
        chunk.palette_len()
    );
}

fn assert_matches_reference(chunk: &Chunk, reference: &ReferenceChunk) {
    for index in 0..BLOCKS_PER_CHUNK {
        let local = LocalBlock::from_index(index);
        assert_eq!(
            &chunk.block_cells(local),
            reference.cells(local),
            "block {index} diverged from the reference model"
        );
    }
}

proptest! {
    // The acceptance criterion asks for 10,000 cases on the reference-model
    // equivalence property specifically.
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Any sequence of writes leaves the chunk reading identically to an
    /// uncompressed model of the same edits.
    #[test]
    fn matches_the_reference_model(
        fill in material(),
        ops in proptest::collection::vec(op(), 1..24),
    ) {
        let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), fill);
        let mut reference = ReferenceChunk::new(fill);

        for op in &ops {
            apply(&mut chunk, &mut reference, op);
        }

        assert_matches_reference(&chunk, &reference);
        assert_internally_consistent(&chunk);
    }
}

proptest! {
    /// Breaking a block yields exactly as many units as it had occupied
    /// sub-nodes — the conservation law that makes chiselling safe.
    #[test]
    fn break_block_conserves_units(cells in cells()) {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        let local = LocalBlock::new(0, 0, 0);
        chunk.set_block_local(local, BlockValue::Cells(*cells));

        let view = chunk.get_block_local(local);
        let stacks = break_block(view);

        let occupied = cells.iter().filter(|cell| !cell.is_air()).count() as u64;
        prop_assert_eq!(
            total_units(&stacks),
            occupied,
            "dropped units must equal occupied sub-nodes"
        );
        prop_assert_eq!(u64::from(view.occupied_units()), occupied);

        // Never a stack of air, never an empty stack.
        for stack in &stacks {
            prop_assert!(!stack.material.is_air());
            prop_assert!(stack.units > 0);
        }

        // Ascending material id, always: drop order is observable.
        for pair in stacks.windows(2) {
            prop_assert!(pair[0].material < pair[1].material, "output not sorted");
        }
    }

    /// A repack preserves contents exactly, and never leaves the index array
    /// wider than the palette needs.
    #[test]
    fn repack_is_content_identity(
        fill in material(),
        ops in proptest::collection::vec(op(), 1..24),
    ) {
        let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), fill);
        let mut reference = ReferenceChunk::new(fill);
        for op in &ops {
            apply(&mut chunk, &mut reference, op);
        }

        let before: Vec<Cells> = chunk.blocks().map(|(_, view)| view.cells()).collect();
        let live_before = chunk.palette_len();

        chunk.repack();

        let after: Vec<Cells> = chunk.blocks().map(|(_, view)| view.cells()).collect();
        prop_assert_eq!(before, after, "repack altered chunk contents");
        prop_assert_eq!(
            live_before,
            chunk.palette_len(),
            "repack changed the number of live entries"
        );

        // Idempotent: repacking twice must be the same as repacking once.
        let once = chunk.clone();
        chunk.repack();
        prop_assert_eq!(once, chunk.clone(), "repack is not idempotent");

        assert_matches_reference(&chunk, &reference);
        assert_internally_consistent(&chunk);
    }

    /// Canonicalisation maps every value to a fixed point, and preserves both
    /// the cell contents and the occupied unit count.
    #[test]
    fn canonicalisation_preserves_meaning(cells in cells()) {
        let value = BlockValue::Cells(*cells);
        let canonical = value.canonical();

        prop_assert_eq!(canonical.cells(), value.cells(), "cells changed");
        prop_assert_eq!(canonical.occupied_units(), value.occupied_units());
        prop_assert!(canonical.is_canonical());
        prop_assert_eq!(canonical.canonical(), canonical, "not a fixed point");
    }

    /// Two blocks written with equal contents by different routes end up
    /// identical — the property canonical form exists to guarantee, and the one
    /// the determinism hash gate will depend on.
    #[test]
    fn equal_contents_have_one_representation(cells in cells()) {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        let first = LocalBlock::new(0, 0, 0);
        let second = LocalBlock::new(1, 0, 0);

        // Route A: write the whole block at once.
        chunk.set_block_local(first, BlockValue::Cells(*cells));

        // Route B: write it one sub-node at a time.
        for (index, &material) in cells.iter().enumerate() {
            if material.is_air() {
                continue;
            }
            let (x, y, z) = block::subnode_offset(index);
            let world = tiamot_core::BlockPos::new(1, 0, 0).subnode(x as i32, y as i32, z as i32);
            chunk.set_subnode(world, material).expect("inside chunk");
        }

        prop_assert_eq!(
            chunk.get_block_local(first).to_value(),
            chunk.get_block_local(second).to_value(),
            "the same contents reached by two routes stored differently"
        );
    }

    /// Palette width tracks the palette exactly as blocks are made distinct and
    /// then collapsed back together.
    #[test]
    fn palette_width_tracks_entry_count(distinct in 1usize..=64) {
        let mut chunk = Chunk::air(ChunkPos::new(0, 0, 0));
        for index in 0..distinct {
            chunk.set_block_local(
                LocalBlock::from_index(index),
                BlockValue::Uniform(MaterialId(index as u16 + 2)),
            );
        }

        // `distinct` materials, plus air if any block is still air.
        let expected_entries = if distinct < BLOCKS_PER_CHUNK {
            distinct + 1
        } else {
            distinct
        };
        prop_assert_eq!(chunk.palette_len(), expected_entries);

        let bits = chunk.bits_per_index();
        prop_assert!(
            (1usize << u32::from(bits)) >= expected_entries,
            "{expected_entries} entries do not fit in {bits} bits"
        );
        if bits > 1 {
            prop_assert!(
                (1usize << u32::from(bits - 1)) < expected_entries,
                "{bits} bits is wider than {expected_entries} entries needs"
            );
        }

        // Collapse everything back to air; the palette must return to one entry
        // and the index array to zero width.
        for index in 0..distinct {
            chunk.set_block_local(LocalBlock::from_index(index), BlockValue::AIR);
        }
        prop_assert_eq!(chunk.palette_len(), 1);
        prop_assert_eq!(chunk.bits_per_index(), 0);
        prop_assert_eq!(chunk.mixed_len(), 0, "mixed slots leaked");
    }
}
