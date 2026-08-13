// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The binary mesher against a dumb oracle, over random chunks.
//!
//! Charter rule 15 asks for property tests on simulation invariants. Meshing is
//! presentation rather than simulation, but it earns the same treatment for a
//! different reason: it is bit-twiddling over a `u64` bit layout, which is
//! exactly the kind of code that passes every example you thought to write and
//! is wrong on the one you did not.
//!
//! The oracle is deliberately slow and obvious — one quad per exposed cell
//! face, no merging, written a completely different way. If the two disagree
//! about which faces exist, the fast one is wrong.

use std::collections::BTreeSet;

use client::mesher::{Absent, Neighbours, Quad, mesh_chunk, reference};
use proptest::prelude::*;
use tiamot_core::coords::SubNodePos;
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId};

/// Full daylight, so these properties are about geometry. Light is its own
/// merge key (see `client::shade`), and a varying field would be testing that
/// instead of the greedy mesher against its oracle.
const DAY: client::shade::Uniform = client::shade::Uniform(tiamot_core::light::Light::DAYLIGHT);

/// Every quad expanded back into the cell faces it covers.
///
/// A `BTreeSet` because it also catches a face emitted twice — two quads
/// overlapping is a bug the surface-area comparison alone would miss, since
/// the totals could still match.
fn faces(quads: &[Quad]) -> BTreeSet<(u8, bool, u8, u8, u8, u16)> {
    let mut out = BTreeSet::new();
    for quad in quads {
        for du in 0..quad.du {
            for dv in 0..quad.dv {
                let inserted = out.insert((
                    quad.axis,
                    quad.positive,
                    quad.u + du,
                    quad.v + dv,
                    quad.w,
                    quad.material,
                ));
                assert!(inserted, "a face was emitted twice by {quad:?}");
            }
        }
    }
    out
}

/// A chunk built from a list of whole-block and sub-node edits.
fn build(blocks: &[(u8, u8, u8, u16)], cells: &[(u8, u8, u8, u16)]) -> Chunk {
    let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
    for (x, y, z, material) in blocks {
        let _ = chunk.set_block(
            BlockPos::new(i32::from(*x), i32::from(*y), i32::from(*z)),
            BlockValue::Uniform(MaterialId(*material)),
        );
    }
    for (x, y, z, material) in cells {
        let _ = chunk.set_subnode(
            SubNodePos::new(i32::from(*x), i32::from(*y), i32::from(*z)),
            MaterialId(*material),
        );
    }
    chunk
}

/// Whole-block edits: coordinates inside a 16³ chunk, a small material set.
fn block_edits() -> impl Strategy<Value = Vec<(u8, u8, u8, u16)>> {
    // Materials drawn from a handful of ids INCLUDING air, so the generator
    // produces holes as well as fill — a mesher that only ever saw solid input
    // would never exercise its face culling.
    prop::collection::vec(
        (
            0u8..16,
            0u8..16,
            0u8..16,
            prop::sample::select(vec![0u16, 2, 3, 4]),
        ),
        0..40,
    )
}

/// Sub-node edits: coordinates inside the 48³ cell grid.
fn cell_edits() -> impl Strategy<Value = Vec<(u8, u8, u8, u16)>> {
    prop::collection::vec(
        (
            0u8..48,
            0u8..48,
            0u8..48,
            prop::sample::select(vec![0u16, 2, 3]),
        ),
        0..60,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The central property: the merged mesh and the reference describe the
    /// same surface, cell face for cell face.
    #[test]
    fn the_merged_mesh_matches_the_reference_surface(
        blocks in block_edits(),
        cells in cell_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );
        let naive = reference::mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);

        prop_assert_eq!(
            faces(&merged.quads),
            faces(&naive),
            "the merged mesh and the reference disagree about the surface"
        );
    }

    /// Merging must never invent or lose area. Stated separately from the
    /// set comparison because it is the property a human would check by eye,
    /// and a failure here localises faster.
    #[test]
    fn merging_preserves_total_surface_area(
        blocks in block_edits(),
        cells in cell_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );
        let naive = reference::mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);

        let merged_area: u32 = merged
            .quads
            .iter()
            .map(|quad| u32::from(quad.du) * u32::from(quad.dv))
            .sum();
        let naive_area = u32::try_from(naive.len()).unwrap_or(0);

        prop_assert_eq!(merged_area, naive_area);
    }

    /// Merging must actually merge. A mesher that emitted one quad per face
    /// would pass every correctness property above and be useless.
    #[test]
    fn merging_never_produces_more_quads_than_the_reference(
        blocks in block_edits(),
        cells in cell_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );
        let naive = reference::mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY);

        prop_assert!(
            merged.quads.len() <= naive.len(),
            "merging produced {} quads against the reference's {}",
            merged.quads.len(),
            naive.len()
        );
    }

    /// Every quad must lie inside the chunk. A quad at cell 48 would be one
    /// past the end and would read a neighbour's geometry.
    #[test]
    fn every_quad_stays_inside_the_chunk(
        blocks in block_edits(),
        cells in cell_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );

        for quad in &merged.quads {
            prop_assert!(quad.du >= 1 && quad.dv >= 1, "degenerate quad {:?}", quad);
            prop_assert!(usize::from(quad.u) + usize::from(quad.du) <= 48, "{:?}", quad);
            prop_assert!(usize::from(quad.v) + usize::from(quad.dv) <= 48, "{:?}", quad);
            prop_assert!(usize::from(quad.w) < 48, "{:?}", quad);
            prop_assert!(quad.material != 0, "air must never be meshed: {:?}", quad);
        }
    }

    /// Border culling must agree with the oracle too, not just interior faces.
    ///
    /// The neighbour is a different random chunk, so the shared plane has a
    /// mix of exposed and hidden cells rather than being uniformly one or the
    /// other — which is where an off-by-one in the padding bits shows up.
    #[test]
    fn border_culling_matches_the_reference(
        blocks in block_edits(),
        cells in cell_edits(),
        neighbour_blocks in block_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let neighbour = build(&neighbour_blocks, &[]);

        for side in 0..6 {
            let mut neighbours = Neighbours::none();
            neighbours.sides[side] = Some(&neighbour);

            let merged = mesh_chunk(&chunk, &neighbours, Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );
            let naive = reference::mesh_chunk(&chunk, &neighbours, Absent::Air, &DAY);

            prop_assert_eq!(
                faces(&merged.quads),
                faces(&naive),
                "border culling disagrees with the reference on side {}",
                side
            );
        }
    }

    /// Vertex packing must round-trip for every quad the mesher emits.
    #[test]
    fn every_emitted_vertex_round_trips(
        blocks in block_edits(),
        cells in cell_edits(),
    ) {
        let chunk = build(&blocks, &cells);
        let merged = mesh_chunk(&chunk, &Neighbours::open(), Absent::Air, &DAY,
        &client::mesher::NoFluid,
    );
        let (vertices, indices) = merged.to_buffers();

        for vertex in &vertices {
            let (x, y, z) = vertex.position();
            prop_assert!(x <= 48 && y <= 48 && z <= 48, "position out of range");
            prop_assert!(vertex.material_id() != 0, "air was meshed");
        }
        for index in &indices {
            prop_assert!((*index as usize) < vertices.len(), "index out of range");
        }
    }
}
