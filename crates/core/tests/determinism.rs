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

/// The physics golden: a fixed scene, a fixed input log, and the hash of every
/// tick of the result.
///
/// **Regenerate only with the reasoning in the module docs.** This is the
/// number that says a client's prediction and a server's simulation are the
/// same computation — if they were not, reconciliation would correct every
/// tick and the player would never stop being dragged.
/// Regenerated once, deliberately, when a horizontal collision stopped zeroing
/// the velocity of a body that was rising — the fix for a stepped passage
/// costing a jump's worth of speed at every riser. Case 3 in the module docs:
/// the recipe changed, so the constant moves in the same commit that changed
/// it. The worldgen and light goldens are untouched, which is what says the
/// change was confined to `phys`.
///
/// Regenerated a second time, also case 3, when the sneak edge guard stopped
/// keeping the velocity it was suppressing. A body held at a brink accumulated
/// the speed it was not being allowed to use, and releasing sneak set it free —
/// a player standing on the lip of a hole with nothing pressed slid forward and
/// fell in. The script this hash covers walks, sprints AND sneaks, so its
/// trajectory legitimately moves. The worldgen and light goldens pass unchanged
/// again, which is the check that says the change stayed inside `phys`.
///
/// Regenerated a third time, case 3 again, when the ground friction went 0.6 →
/// 0.7 to give movement a little slide and the ground acceleration moved with
/// it to hold the top speed where it was. Reported from the window as wanting
/// starts and stops to feel soft rather than snapped. The script this hash
/// covers walks and sprints, so every trajectory in it legitimately moves; the
/// worldgen, light and fluid goldens all pass unchanged, which is what says the
/// change stayed inside `phys`.
const PHYSICS_GOLDEN: u64 = 445_538_206_318_146_463;

/// Runs the fixed physics scenario and hashes every tick of it.
///
/// Hashes EVERY tick rather than the final state: a run that diverged and
/// converged again — a body that clipped a corner differently and landed in the
/// same place — would pass a final-state check while being a different
/// simulation.
fn physics_fingerprint() -> u64 {
    use tiamot_core::phys::{Body, Gait, Intent, Solid, Tuning};

    /// A staircase with a wall, so the log exercises step-up, collision and
    /// falling rather than open ground.
    struct Scene;
    impl Solid for Scene {
        fn solid(&self, x: i32, y: i32, z: i32) -> bool {
            // A floor, a step at x = 4, and a wall at x = 9.
            y < 0 || (y < 1 && (4..9).contains(&x)) || (x == 9 && y < 6 && (-8..8).contains(&z))
        }
    }

    // A VARIED log. A constant intent would hold the body against one surface
    // and never exercise the transitions, which is where a divergence would
    // actually come from.
    let script: [(f32, f32, bool, u8); 8] = [
        (1.0, 0.0, false, 0),
        (1.0, 0.3, true, 1),
        (0.0, 1.0, false, 2),
        (-1.0, 0.2, true, 0),
        (0.5, -0.9, false, 1),
        (1.0, 1.0, true, 2),
        (-0.4, -0.4, false, 0),
        (0.0, 0.0, false, 1),
    ];

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:phys-golden:v1");
    let mut body = Body::at([1.5, 3.0, 0.5]);
    for tick in 0..240u32 {
        let (x, z, jump, gait) = script[(tick as usize) % script.len()];
        let intent = Intent {
            walk: [x, z],
            jump,
            gait: match gait {
                0 => Gait::Walk,
                1 => Gait::Sprint,
                _ => Gait::Sneak,
            },
        };
        body = tiamot_core::phys::step(&Scene, body, intent, &Tuning::DEFAULT);

        // Bit patterns, not values: two floats that compare equal can still be
        // different bits, and the whole point is that the bits agree.
        for value in body.position.iter().chain(body.velocity.iter()) {
            hasher.update(&value.to_bits().to_le_bytes());
        }
        hasher.update(&[u8::from(body.on_ground)]);
    }
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    )
}

#[test]
fn an_input_log_simulates_to_its_golden_hash() {
    // **Task 09's determinism criterion, and what underwrites prediction.**
    // The client replays inputs through `phys::step` and compares with the
    // server's answer; that comparison is only meaningful if the same inputs
    // give the same bits on every machine. The CI matrix runs this on Linux,
    // Windows and macOS.
    //
    // Charter rule 4 says this is achievable with plain `f32` because Rust
    // guarantees IEEE semantics and forbids fast-math, FMA contraction and
    // reassociation. This is the assertion that the guarantee holds in
    // practice, for the code players actually run.
    //
    // Sensitivity, measured rather than assumed: perturbing
    // `Tuning::ground_acceleration` from 0.43 to 0.4300001 — two parts in ten
    // million — changes this hash. Perturbing `walk_speed` does NOT, and that
    // is not a gap: it is a CAP, and the speed a body settles at comes from
    // acceleration and friction, so the cap never binds. `tuning`'s own
    // `friction_and_acceleration_settle_at_the_walk_speed` is what keeps the
    // two agreeing.
    assert_eq!(
        physics_fingerprint(),
        PHYSICS_GOLDEN,
        "the same input log simulated to a different result. Do NOT update the constant to \
         match — see the module docs. Either a banned operation reached `phys`, a thread is \
         in flush-to-zero mode, or the physics changed deliberately."
    );
}

#[test]
fn the_physics_golden_is_stable_across_repeated_calls() {
    // The counter-example to the golden being satisfied by chance: if the
    // scenario were not reproducible even in one process, the constant above
    // would be meaningless whatever it said.
    assert_eq!(physics_fingerprint(), physics_fingerprint());
}

/// The golden light fingerprint.
///
/// **Regenerate ONLY with a deliberate change to propagation**, exactly as with
/// the physics golden above. Light is integer arithmetic, so this is not
/// guarding against a float subset violation — it guards the other half of
/// charter rule 4, which is that the *order* of a computation must not vary
/// between platforms. A BFS whose queue order, face order, or container
/// iteration differed would reach a different fixed point on one of the three
/// CI targets and nowhere else.
const LIGHT_GOLDEN: u64 = 7_215_387_918_458_500_778;

/// Lights a fixed scene and hashes every block of it.
///
/// The scene is built so every rule has somewhere to show: open sky, a roof
/// with a shaft through it so there is a gradient under the lip, and a sealed
/// room with two lamps of different colours in it so the channels mix rather
/// than one drowning the others.
fn light_fingerprint() -> u64 {
    use std::collections::BTreeMap;
    use tiamot_core::coords::BlockPos;
    use tiamot_core::light::propagate::{Neighbourhood, Region};
    use tiamot_core::light::{Faces, Light, MAX_LEVEL, relight};

    struct Scene {
        region: Region,
        solid: BTreeMap<(i32, i32, i32), bool>,
        lamps: BTreeMap<(i32, i32, i32), Light>,
        light: BTreeMap<(i32, i32, i32), Light>,
    }

    impl Neighbourhood for Scene {
        fn faces(&self, pos: BlockPos) -> Option<Faces> {
            if !self.region.contains(pos) {
                return None;
            }
            let solid = self
                .solid
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(false);
            Some(if solid { Faces::OPAQUE } else { Faces::OPEN })
        }

        fn emission(&self, pos: BlockPos) -> Light {
            self.lamps
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Light::DARK)
        }

        fn light(&self, pos: BlockPos) -> Light {
            self.light
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Light::DARK)
        }

        fn set_light(&mut self, pos: BlockPos, level: Light) {
            if self.region.contains(pos) {
                self.light.insert((pos.x, pos.y, pos.z), level);
            }
        }
    }

    const SIZE: i32 = 24;
    let mut scene = Scene {
        region: Region {
            min: BlockPos::new(0, 0, 0),
            max: BlockPos::new(SIZE, SIZE, SIZE),
        },
        solid: BTreeMap::new(),
        lamps: BTreeMap::new(),
        light: BTreeMap::new(),
    };

    // A roof at y = 16 with a two-block shaft through it.
    for z in 0..=SIZE {
        for x in 0..=SIZE {
            scene.solid.insert((x, 16, z), true);
        }
    }
    for z in 10..12 {
        for x in 10..12 {
            scene.solid.insert((x, 16, z), false);
        }
    }
    // A sealed room in the corner, with two lamps inside it.
    for y in 2..8 {
        for z in 2..8 {
            for x in 2..8 {
                let wall = x == 2 || x == 7 || y == 2 || y == 7 || z == 2 || z == 7;
                scene.solid.insert((x, y, z), wall);
            }
        }
    }
    scene
        .lamps
        .insert((3, 3, 3), Light::new(0, MAX_LEVEL, 0, 4));
    scene
        .lamps
        .insert((6, 6, 6), Light::new(0, 2, MAX_LEVEL, 0));

    let region = scene.region;
    relight(&mut scene, region);

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:light-golden:v1");
    // A fixed traversal rather than the map's own order: the hash has to
    // describe the light, not the container it happens to be in.
    for y in 0..=SIZE {
        for z in 0..=SIZE {
            for x in 0..=SIZE {
                let level = scene.light(BlockPos::new(x, y, z));
                hasher.update(&level.0.to_le_bytes());
            }
        }
    }
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    )
}

#[test]
fn a_lit_scene_hashes_to_its_golden() {
    // **Task 10's cross-platform determinism criterion.** The CI matrix runs
    // this on Linux, Windows and macOS against the same constant.
    assert_eq!(
        light_fingerprint(),
        LIGHT_GOLDEN,
        "the same scene lit to a different result. Do NOT update the constant to match unless \
         propagation changed deliberately — a difference here means the BFS order, the face \
         order, or a container's iteration order varies between builds."
    );
}

#[test]
fn the_light_golden_is_stable_across_repeated_calls() {
    // The counter-example that makes the constant mean something: a scene not
    // even reproducible in one process could not be reproducible across three
    // platforms.
    assert_eq!(light_fingerprint(), light_fingerprint());
}

#[test]
fn the_lit_scene_is_not_trivially_uniform() {
    // A golden over an all-dark scene would pass against an implementation
    // that did nothing at all. This pins that the fixture exercises the rules
    // it was built for.
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:light-golden:v1");
    for _ in 0..(25 * 25 * 25) {
        hasher.update(&0u16.to_le_bytes());
    }
    let all_dark = u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    );
    assert_ne!(
        light_fingerprint(),
        all_dark,
        "the golden scene is entirely dark, so the hash proves nothing"
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

/// The golden fluid fingerprint.
///
/// **Regenerate ONLY with a deliberate change to the flow rule**, exactly as
/// with the two goldens above. Fluid is integer arithmetic throughout, so this
/// is not guarding a float subset violation — it guards the other half of
/// charter rule 4, that the *order* of a computation must not vary between
/// platforms. The solver's active set is a `BTreeSet` for precisely this
/// reason, and a `HashSet` swapped in would fail here on every platform and for
/// a different value on each run.
/// Rebaselined 2026-08-26 when the model became conserved: level meant
/// distance travelled from a source, volume means volume, and there are no
/// sources (Sub-Node Contract §4.1). A deliberate rule change is exactly the
/// case this constant's warning names.
const FLUID_GOLDEN: u64 = 10_233_241_359_803_942_171;
// Regenerated once, deliberately, when the hole preference was WIRED IN. It had
// been written, table-driven tested and never consulted by the solver, so milk
// spread evenly in all directions and reached a hole by covering the ground
// between. Steering it changes where milk ends up in the slope scenario, which
// is exactly what this hash is for — and is why the constant moved rather than
// the test being relaxed.
//
// Regenerated a second time, deliberately, when a FOURTH scenario was added for
// source renewal. The first three are untouched — renewal is off in
// `Tuning::DEFAULT` and cannot have changed them — so this move is the new
// scenario joining the hash and nothing else. Confirmed by the three original
// scenarios still settling to the values they always did before the fourth was
// appended.

/// Runs the three scenarios the task names and hashes what settled.
///
/// A **spring on a slope**, a **pool filling**, and a **channel draining** —
/// chosen because each exercises a different rule. The slope is the lateral
/// spread and the hole preference; the pool is falling and the level gradient
/// meeting a wall; the drained channel is decay, which is the only rule that
/// runs when nothing is being added.
///
/// All three are hashed together into one constant. Separate constants would
/// say which scenario diverged, which sounds useful and is not: a divergence is
/// an ordering bug in one shared solver, and the first thing anyone would do is
/// run all three anyway.
fn fluid_fingerprint() -> u64 {
    use std::collections::{BTreeMap, BTreeSet};
    use tiamot_core::coords::BlockPos;
    use tiamot_core::fluid::{Fluid, FluidId, MAX_VOLUME, Neighbourhood, Solver, Tuning};

    const MILK: FluidId = FluidId(1);

    #[derive(Default)]
    struct Scene {
        solid: BTreeSet<(i32, i32, i32)>,
        fluid: BTreeMap<(i32, i32, i32), Fluid>,
    }

    impl Neighbourhood for Scene {
        fn occupancy(&self, pos: BlockPos) -> Option<u32> {
            // Walled at the edges so nothing escapes the scenario. A fixture
            // that leaked would hash the leak rather than the rule.
            if pos.x.abs() > 12 || pos.z.abs() > 12 || pos.y < 0 || pos.y > 12 {
                return None;
            }
            Some(if self.solid.contains(&(pos.x, pos.y, pos.z)) {
                tiamot_core::UNITS_PER_BLOCK
            } else {
                0
            })
        }

        fn fluid(&self, pos: BlockPos) -> Fluid {
            self.fluid
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Fluid::EMPTY)
        }

        fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
            if value.is_empty() {
                self.fluid.remove(&(pos.x, pos.y, pos.z));
            } else {
                self.fluid.insert((pos.x, pos.y, pos.z), value);
            }
        }
    }

    /// Milk that evaporates, which is the seeded half of the rule.
    ///
    /// **The new determinism risk.** Flow is integer arithmetic and could only
    /// diverge through visit order; evaporation is randomness, and randomness
    /// that came from anywhere but the world seed and the block's position
    /// would differ between two servers running the same world. One in three,
    /// so the hash actually depends on it.
    const EVAPORATING: Tuning = Tuning {
        evaporates: 3,
        ..Tuning::DEFAULT
    };

    /// The world seed these scenes settle under.
    const SEED: u64 = 0x51E5_D0C7_A311_9F42;

    /// Settles a scene and folds every block of it into the hash.
    fn run(hasher: &mut blake3::Hasher, scene: Scene, solver: Solver, ticks: usize) {
        run_with(hasher, scene, solver, ticks, Tuning::DEFAULT);
    }

    /// The same, under a fluid tuned differently.
    fn run_with(
        hasher: &mut blake3::Hasher,
        mut scene: Scene,
        mut solver: Solver,
        ticks: usize,
        tuning: Tuning,
    ) {
        for tick in 0..ticks {
            solver.tick(&mut scene, tuning, usize::MAX, SEED, tick as u64);
        }
        // Sorted by construction — a `BTreeMap` — so the hash does not depend
        // on the order the blocks happened to be written in.
        for ((x, y, z), value) in &scene.fluid {
            hasher.update(&x.to_le_bytes());
            hasher.update(&y.to_le_bytes());
            hasher.update(&z.to_le_bytes());
            hasher.update(&value.0.to_le_bytes());
        }
        // And how much work is outstanding, so a scenario that settles on one
        // platform and not on another is a difference this notices.
        hasher.update(&(solver.active() as u64).to_le_bytes());
    }

    let mut hasher = blake3::Hasher::new();

    // 1. A spring on a slope: a staircase of solid blocks with a source at the
    //    top, so milk spreads, finds the drop, and falls to the next step.
    let mut scene = Scene::default();
    for step in 0..6 {
        for z in -6..=6 {
            for x in -6..=(step * 2 - 6) {
                scene.solid.insert((x, step, z));
            }
        }
    }
    let mut solver = Solver::new();
    scene.set_fluid(BlockPos::new(-6, 6, 0), Fluid::new(MILK, MAX_VOLUME));
    solver.touch(BlockPos::new(-6, 6, 0));
    run(&mut hasher, scene, solver, 120);

    // 2. A pool filling: a walled basin with a source above its middle, so the
    //    column falls, hits the floor, and spreads to the walls.
    let mut scene = Scene::default();
    for x in -4..=4 {
        for z in -4..=4 {
            scene.solid.insert((x, 0, z));
        }
    }
    for x in -4..=4 {
        for y in 1..=3 {
            scene.solid.insert((x, y, -4));
            scene.solid.insert((x, y, 4));
            scene.solid.insert((-4, y, x));
            scene.solid.insert((4, y, x));
        }
    }
    let mut solver = Solver::new();
    scene.set_fluid(BlockPos::new(0, 6, 0), Fluid::new(MILK, MAX_VOLUME));
    solver.touch(BlockPos::new(0, 6, 0));
    run(&mut hasher, scene, solver, 120);

    // 3. A drained channel: a trench filled from a source, which is then taken
    //    away, so what is hashed is the state decay left behind.
    let mut scene = Scene::default();
    for x in -8..=8 {
        for z in -1..=1 {
            scene.solid.insert((x, 0, z));
        }
        scene.solid.insert((x, 1, -1));
        scene.solid.insert((x, 1, 1));
    }
    let mut solver = Solver::new();
    scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::new(MILK, MAX_VOLUME));
    solver.touch(BlockPos::new(0, 1, 0));
    for _ in 0..60 {
        solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX, SEED, 0);
    }
    scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::EMPTY);
    solver.touch(BlockPos::new(0, 1, 0));
    // Deliberately stopped MID-DRAIN rather than after it finishes. A fully
    // drained channel hashes an empty map, which is the same value whatever the
    // decay rule did on the way there.
    run(&mut hasher, scene, solver, 3);

    // 4. An ocean healing: a 5x5 pool of sources with a bucket taken out of the
    //    middle, under a fluid that renews from three sides. The rule creates
    //    matter, which is the one thing in the solver that can run away — so it
    //    is in the gate rather than trusted to be small.
    let mut scene = Scene::default();
    for x in -3..=3 {
        for z in -3..=3 {
            scene.solid.insert((x, 0, z));
        }
    }
    let mut solver = Solver::new();
    for x in -2..=2 {
        for z in -2..=2 {
            scene.set_fluid(BlockPos::new(x, 1, z), Fluid::new(MILK, MAX_VOLUME));
            solver.touch(BlockPos::new(x, 1, z));
        }
    }
    for tick in 0..30u64 {
        solver.tick(&mut scene, EVAPORATING, usize::MAX, SEED, tick);
    }
    scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::EMPTY);
    solver.touch(BlockPos::new(0, 1, 0));
    run_with(&mut hasher, scene, solver, 30, EVAPORATING);

    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    )
}

#[test]
fn settled_milk_hashes_to_its_golden() {
    // **Task 11's cross-platform determinism criterion.** The CI matrix runs
    // this on Linux, Windows and macOS against the same constant.
    assert_eq!(
        fluid_fingerprint(),
        FLUID_GOLDEN,
        "the same scenarios settled to a different result. Do NOT update the constant to match \
         unless the flow rule changed deliberately — a difference here means the solver's visit \
         order varies between builds, which is the one thing the active set being ordered exists \
         to prevent."
    );
}

#[test]
fn the_fluid_golden_is_stable_across_repeated_calls() {
    // The counter-example that makes the constant mean something: scenarios not
    // reproducible in one process could not be reproducible across three
    // platforms.
    assert_eq!(fluid_fingerprint(), fluid_fingerprint());
}

#[test]
fn the_fluid_scenarios_actually_hold_milk() {
    // **The trap the light golden's sibling test exists for**, and it is worth
    // repeating here: a fingerprint over three empty scenes is perfectly stable
    // and perfectly meaningless. This asserts the scenarios did something.
    use std::collections::{BTreeMap, BTreeSet};
    use tiamot_core::coords::BlockPos;
    use tiamot_core::fluid::{Fluid, FluidId, MAX_VOLUME, Neighbourhood, Solver, Tuning};

    #[derive(Default)]
    struct Basin {
        solid: BTreeSet<(i32, i32, i32)>,
        fluid: BTreeMap<(i32, i32, i32), Fluid>,
    }
    impl Neighbourhood for Basin {
        fn occupancy(&self, pos: BlockPos) -> Option<u32> {
            if pos.x.abs() > 12 || pos.z.abs() > 12 || pos.y < 0 || pos.y > 12 {
                return None;
            }
            Some(if self.solid.contains(&(pos.x, pos.y, pos.z)) {
                tiamot_core::UNITS_PER_BLOCK
            } else {
                0
            })
        }
        fn fluid(&self, pos: BlockPos) -> Fluid {
            self.fluid
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Fluid::EMPTY)
        }
        fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
            if value.is_empty() {
                self.fluid.remove(&(pos.x, pos.y, pos.z));
            } else {
                self.fluid.insert((pos.x, pos.y, pos.z), value);
            }
        }
    }

    let mut scene = Basin::default();
    for x in -4..=4 {
        for z in -4..=4 {
            scene.solid.insert((x, 0, z));
        }
    }
    // The seed only matters to evaporation, which `Tuning::DEFAULT` has off.
    const SEED: u64 = 0x51E5_D0C7_A311_9F42;

    let mut solver = Solver::new();
    // **Several blocks' worth, not one.** A single block of 27 cells spread
    // over an 81-block basin is one cell everywhere, and a golden hashing a
    // flat sheet of ones would not notice the flow rule changing.
    let mut poured = 0;
    for y in 1..=4 {
        scene.set_fluid(BlockPos::new(0, y, 0), Fluid::new(FluidId(1), MAX_VOLUME));
        solver.touch(BlockPos::new(0, y, 0));
        poured += MAX_VOLUME;
    }
    for tick in 0..120u64 {
        solver.tick(&mut scene, Tuning::DEFAULT, usize::MAX, SEED, tick);
    }

    assert!(
        scene.fluid.len() > 20,
        "only {} blocks hold milk, so the golden is hashing almost nothing",
        scene.fluid.len()
    );
    // **Conservation, in the test that guards the golden.** Nothing here
    // absorbs and nothing evaporates, so every cell poured in is still
    // somewhere — and a golden over a scene that quietly lost half its milk
    // would be a stable hash of a broken rule.
    let held: u32 = scene.fluid.values().map(|value| value.volume()).sum();
    assert_eq!(
        held, poured,
        "milk went missing with no sink to account for it"
    );
    let volumes: BTreeSet<u32> = scene.fluid.values().map(|value| value.volume()).collect();
    assert!(
        volumes.len() > 1,
        "every block holds the same amount, so the gradient the golden should cover is absent"
    );
}
