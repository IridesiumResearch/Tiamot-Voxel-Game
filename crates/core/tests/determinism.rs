// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! **The cross-platform determinism gate.**
//!
//! This file is the reason charter rule 4 exists. The CI matrix runs it on
//! Linux, Windows, and macOS, and one differing bit on any of them fails the
//! build.
//!
//! # If a golden hash fails
//!
//! **Do not update the constant to match.** A mismatch means one of:
//!
//! 1. A banned operation reached a generation path. The `disallowed-methods`
//!    lint should have caught it — check whether something was `#[allow]`ed.
//! 2. A thread is in flush-to-zero mode. `detgen::assert_ieee_mode` says so.
//! 3. The generation recipe changed deliberately. Then, and only then,
//!    regenerate every golden in the same commit and say so in the message.
//!
//! Silently rewriting a golden converts a build failure into a bug report from
//! a player whose world generated differently from their friend's.

use proptest::prelude::*;

use tiamot_core::coords::LocalBlock;
use tiamot_core::detgen::{
    self, ChunkBuffer, Fractal, FractalParams, Region2d, Region3d, StreamRng, fill_2d, fill_3d,
    fractal_2d,
};
use tiamot_core::{BLOCKS_PER_CHUNK, CHUNK_BLOCKS, ChunkPos, MaterialId, fingerprint};

/// A fixture material. Numbered, not named — `detgen` contains no terrain
/// vocabulary and neither should its tests.
const FIXTURE: MaterialId = MaterialId(2);

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// `(world_seed, chunk x, y, z, expected fingerprint)`.
///
/// Chosen to cover the origin, all three axes independently, negative
/// coordinates, both extremes of the seed space, and positions near the world
/// bound — the places where a sign-extension or overflow bug would hide.
const GOLDEN: [(u64, i32, i32, i32, u64); 16] = [
    (0, 0, 0, 0, 0x5105_5d41_f2b6_7df3),
    (0, 1, 0, 0, 0xe449_dd73_53f3_60e3),
    (0, 0, 1, 0, 0x5191_6c8d_7ac9_b5b5),
    (0, 0, 0, 1, 0x5cce_3fd1_93c0_6203),
    (1, 0, 0, 0, 0xef0c_f1d0_ac99_fb4d),
    (1, -1, -1, -1, 0x271c_b2ee_61fa_49f6),
    (42, 12, 3, -7, 0x5c20_6cab_d26e_600f),
    (42, -12, -3, 7, 0x663b_b46a_c354_c84e),
    (0xDEAD_BEEF, 0, 0, 0, 0x411f_e117_b7e1_d772),
    (0xDEAD_BEEF, 100, 0, 100, 0x0644_ac22_50c4_1292),
    (u64::MAX, 0, 0, 0, 0x68fe_240d_8fc3_3607),
    (7, 3749, 0, -3750, 0x8c0a_b6c2_5106_4b62),
    (7, -3750, 5, 3749, 0x5b39_9f46_f638_b6c3),
    (123_456_789, 8, 8, 8, 0x0165_1916_4280_0ae0),
    (987_654_321, -64, 2, 64, 0x4548_5705_4a8c_2efd),
    (555, 1000, -1000, 0, 0x5a33_efa6_e3e5_4d51),
];

#[test]
fn golden_fingerprints_match() {
    // Guard first: a machine in flush-to-zero mode would fail below with a
    // confusing hash mismatch instead of the real diagnosis.
    detgen::assert_ieee_mode();

    let mut mismatches = Vec::new();
    for (seed, x, y, z, expected) in GOLDEN {
        let actual = fingerprint(seed, ChunkPos::new(x, y, z));
        if actual != expected {
            mismatches.push(format!(
                "  seed {seed}, chunk ({x}, {y}, {z}): expected {expected:#018x}, got {actual:#018x}"
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "CROSS-PLATFORM DETERMINISM GATE FAILED on {} of {} cases:\n{}\n\n\
         Do NOT update these constants to match. See this file's header for what \
         a mismatch actually means.",
        mismatches.len(),
        GOLDEN.len(),
        mismatches.join("\n")
    );
}

#[test]
fn every_golden_case_is_distinct() {
    // A gate whose cases collide is weaker than it looks: a change could move
    // one hash onto another and go unnoticed.
    let mut seen = std::collections::BTreeSet::new();
    for (seed, x, y, z, expected) in GOLDEN {
        assert!(
            seen.insert(expected),
            "seed {seed}, chunk ({x}, {y}, {z}) duplicates an earlier golden hash"
        );
    }
}

#[test]
fn the_fingerprint_is_stable_across_repeated_calls() {
    for (seed, x, y, z, _) in GOLDEN {
        let pos = ChunkPos::new(x, y, z);
        assert_eq!(fingerprint(seed, pos), fingerprint(seed, pos));
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn any_params() -> impl Strategy<Value = FractalParams> {
    (
        prop_oneof![
            Just(Fractal::Fbm),
            Just(Fractal::Ridged),
            Just(Fractal::Billow)
        ],
        0u32..20,
        0.0001f32..4.0,
        0.5f32..4.0,
        0.0f32..1.5,
    )
        .prop_map(
            |(fractal, octaves, frequency, lacunarity, gain)| FractalParams {
                fractal,
                octaves,
                frequency,
                lacunarity,
                gain,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// **No fill ever produces NaN or infinity, for any parameters.**
    ///
    /// Charter rule 4 bans NaN in simulation state because NaN *payloads* are
    /// not specified — two platforms can both produce "a NaN" with different
    /// bits, and the hash gate then fails for a reason nobody can reproduce.
    /// The parameter space is generated rather than sampled because a mod
    /// supplies these numbers and a mod can supply anything.
    #[test]
    fn no_fill_ever_yields_nan_or_infinity(
        params in any_params(),
        seed in any::<u64>(),
        origin_x in -100_000.0f32..100_000.0,
        origin_y in -100_000.0f32..100_000.0,
    ) {
        let region = Region2d {
            origin_x,
            origin_y,
            step_x: 1.0,
            step_y: 1.0,
            width: 8,
            height: 8,
        };
        let mut out = vec![0.0; region.len()];
        fill_2d(seed, &region, &params, &mut out).expect("sized correctly");
        for (index, value) in out.iter().enumerate() {
            prop_assert!(
                value.is_finite(),
                "sample {index} was {value} for {params:?} at ({origin_x}, {origin_y})"
            );
        }

        let region = Region3d {
            origin_x,
            origin_y,
            origin_z: 0.0,
            step: 1.0,
            width: 4,
            height: 4,
            depth: 4,
        };
        let mut out = vec![0.0; region.len()];
        fill_3d(seed, &region, &params, &mut out).expect("sized correctly");
        for value in &out {
            prop_assert!(value.is_finite(), "{value} for {params:?}");
        }
    }

    /// Point sampling and bulk filling agree **bit for bit**, not approximately.
    ///
    /// The bulk path exists for speed. If it ever diverged from the obvious one,
    /// worlds generated by a build that vectorised differently would differ.
    #[test]
    fn bulk_fill_agrees_with_point_sampling(
        params in any_params(),
        seed in any::<u64>(),
    ) {
        let region = Region2d {
            origin_x: -17.5,
            origin_y: 33.25,
            step_x: 0.5,
            step_y: 0.25,
            width: 12,
            height: 9,
        };
        let mut out = vec![0.0; region.len()];
        fill_2d(seed, &region, &params, &mut out).expect("sized correctly");

        for row in 0..region.height {
            for column in 0..region.width {
                let x = region.origin_x + column as f32 * region.step_x;
                let y = region.origin_y + row as f32 * region.step_y;
                prop_assert_eq!(
                    out[row * region.width + column].to_bits(),
                    fractal_2d(seed, x, y, &params).to_bits()
                );
            }
        }
    }

    /// `fill_below_heightmap` then `to_chunk` equals building the chunk the
    /// obvious way, block by block.
    #[test]
    fn heightmap_fill_matches_a_naive_reference(
        heights in proptest::collection::vec(-40i32..40, 256),
        chunk_y in -2i32..3,
    ) {
        let pos = ChunkPos::new(0, chunk_y, 0);

        let mut buffer = ChunkBuffer::air(pos);
        buffer.fill_below_heightmap(&heights, FIXTURE).expect("fill");
        let built = buffer.to_chunk();

        // The reference: decide each block independently, with no shared state
        // and nothing clever.
        let base_y = chunk_y * CHUNK_BLOCKS as i32;
        let mut reference = tiamot_core::Chunk::air(pos);
        for index in 0..BLOCKS_PER_CHUNK {
            let local = LocalBlock::from_index(index);
            let world_y = base_y + local.y as i32;
            let column = (local.x + CHUNK_BLOCKS * local.z) as usize;
            if world_y < heights[column] {
                reference.set_block_local(local, tiamot_core::BlockValue::Uniform(FIXTURE));
            }
        }

        // Compared by CONTENTS, not by `==`.
        //
        // `Chunk`'s `PartialEq` includes palette order, and the two paths build
        // their palettes in different orders: `to_chunk` seeds the palette from
        // the buffer's first block so a uniform buffer costs one entry and no
        // index storage, while the reference starts from air. Both are correct
        // and both are deterministic — palette order is an implementation
        // detail of the compression, not part of what the chunk means.
        //
        // This matters beyond the test: the determinism fingerprint hashes
        // materials read back through the block API, never the palette layout,
        // precisely so that a future change to palette construction cannot move
        // every world's hash.
        for index in 0..BLOCKS_PER_CHUNK {
            let local = LocalBlock::from_index(index);
            prop_assert_eq!(
                built.block_cells(local),
                reference.block_cells(local),
                "block {} differs", index
            );
        }
    }

    /// A buffer that only ever sees block operations never expands.
    ///
    /// Sub-Node Contract §5's cost guarantee, as a property rather than a
    /// promise: this is what stops every generator silently paying 27×.
    #[test]
    fn block_only_generation_never_expands(
        heights in proptest::collection::vec(-40i32..40, 256),
        extra in proptest::collection::vec((0usize..BLOCKS_PER_CHUNK, 2u16..8), 0..32),
    ) {
        let mut buffer = ChunkBuffer::air(ChunkPos::new(0, 0, 0));
        buffer.fill_below_heightmap(&heights, FIXTURE).expect("fill");
        for (index, material) in extra {
            buffer.set_block(LocalBlock::from_index(index), MaterialId(material));
        }
        prop_assert!(!buffer.is_expanded());
    }

    /// Streams are reproducible and independent for any inputs.
    #[test]
    fn streams_are_reproducible_and_independent(
        seed in any::<u64>(),
        x in -3750i32..3750,
        y in -3750i32..3750,
        z in -3750i32..3750,
    ) {
        let pos = ChunkPos::new(x, y, z);
        let mut first = StreamRng::new(seed, pos, "a");
        let mut second = StreamRng::new(seed, pos, "a");
        let mut other = StreamRng::new(seed, pos, "b");

        let mut collisions = 0;
        for _ in 0..16 {
            let value = first.next_u64();
            prop_assert_eq!(value, second.next_u64());
            if value == other.next_u64() {
                collisions += 1;
            }
        }
        prop_assert_eq!(collisions, 0, "differently named streams should not agree");
    }
}

// ---------------------------------------------------------------------------
// Zero terrain policy (acceptance criterion)
// ---------------------------------------------------------------------------

#[test]
fn detgen_contains_no_terrain_policy() {
    // `detgen` provides MECHANISMS. Worldgen POLICY is Lua mods (charter
    // rule 1), and a material name or a biome rule appearing here would be the
    // first crack in that. Enforced rather than trusted, because this is exactly
    // the kind of thing that arrives one convenient helper at a time.
    const FORBIDDEN: [&str; 10] = [
        "grass", "dirt", "stone", "sand", "water", "biome", "tree", "ore", "cave", "terrain",
    ];

    let module = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/detgen");
    let mut offences = Vec::new();

    for entry in std::fs::read_dir(&module).expect("detgen module directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read source");

        // Test modules are allowed fixtures and prose; production code is not.
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields one element");

        for (number, line) in production.lines().enumerate() {
            let lower = line.to_lowercase();
            // Comments may discuss what the module deliberately excludes.
            let trimmed = lower.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for word in FORBIDDEN {
                if lower.contains(word) {
                    offences.push(format!(
                        "{}:{}: contains `{word}`: {}",
                        path.file_name().expect("file name").to_string_lossy(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "detgen must contain mechanisms only, no terrain policy:\n{}",
        offences.join("\n")
    );
}
