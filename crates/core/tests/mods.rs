// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end mod lifecycle: the real `game/` directory, loaded and run.
//!
//! These tests use the **actual reference mods**, not fixtures. If
//! `game/core_worldgen/init.lua` stops working, this fails — which is the point.
//! A mod API that only its own test doubles exercise is not being tested.

use std::path::{Path, PathBuf};

use tiamot_core::coords::LocalBlock;
use tiamot_core::script::{EngineHost, ModHost, Phase, ScriptVm, VmLimits};
use tiamot_core::{BLOCKS_PER_CHUNK, ChunkPos, MaterialId};

/// The repository's `game/` directory.
fn game_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the game/ directory should exist at the repo root")
}

/// A scratch directory holding hand-written mods for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-mods-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes a mod into `root`.
fn write_mod(root: &Path, id: &str, manifest_extra: &str, source: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"1.0.0\"\n{manifest_extra}\n"),
    )
    .expect("manifest");
    std::fs::write(dir.join("init.lua"), source).expect("init.lua");
}

fn host_for(root: &Path) -> EngineHost {
    ModHost::load_from(root, VmLimits::default()).expect("load mods")
}

// ---------------------------------------------------------------------------
// The reference mods, end to end
// ---------------------------------------------------------------------------

#[test]
fn the_reference_mods_load_in_dependency_order() {
    let host = host_for(&game_dir());
    // Every mod in `game/`, in load order. Listed exhaustively rather than
    // spot-checked: this is the test that notices a reference mod being added
    // or removed, which is exactly the change most likely to be made without
    // thinking about load order.
    assert_eq!(
        host.resolved().ids(),
        vec!["core", "core_tools", "core_worldgen"],
        "core_worldgen depends on core and must load after it"
    );
    assert!(
        host.failed().is_empty(),
        "the shipped reference mods must load cleanly: {:?}",
        host.failed()
    );
}

#[test]
fn the_reference_generator_produces_the_half_white_world() {
    // The acceptance criterion, against the real mods: solid below y = 0, air
    // above.
    let mut host = host_for(&game_dir());
    host.freeze().expect("freeze");
    assert_eq!(host.phase(), Phase::Frozen);

    let white = host
        .vm()
        .block_ids()
        .get("core:white")
        .copied()
        .expect("the reference mod should register core:white");

    // Chunk (0, -1, 0) covers world y in -16..0 — entirely below the surface.
    let below = host
        .generate_chunk(0, ChunkPos::new(0, -1, 0), MaterialId::AIR)
        .expect("generate");
    assert_eq!(
        below.is_uniform(),
        Some(white),
        "everything below y=0 should be core:white"
    );

    // Chunk (0, 0, 0) covers world y in 0..16 — entirely above it.
    let above = host
        .generate_chunk(0, ChunkPos::new(0, 0, 0), MaterialId::AIR)
        .expect("generate");
    assert_eq!(
        above.is_uniform(),
        Some(MaterialId::AIR),
        "everything at or above y=0 should be air"
    );
}

#[test]
fn generation_is_reproducible_through_the_script_path() {
    let mut host = host_for(&game_dir());
    host.freeze().expect("freeze");

    let pos = ChunkPos::new(3, -1, -7);
    let first = host
        .generate_chunk(42, pos, MaterialId::AIR)
        .expect("generate");
    let second = host
        .generate_chunk(42, pos, MaterialId::AIR)
        .expect("generate");
    assert_eq!(
        first, second,
        "script-driven generation must be reproducible"
    );
}

/// **The script-driven half of the cross-platform determinism gate.**
///
/// Task 04's gate covers native generation. This covers generation driven
/// through the Lua callback — a different code path, with marshalling and a VM
/// in the middle, which could diverge across platforms independently.
///
/// Two fixtures, because they test different things:
///
/// 1. **The shipped reference mods.** Proves the actual `game/` directory
///    generates identically everywhere. Its generator is a CONSTANT surface, so
///    every chunk below y=0 hashes the same and every chunk above hashes the
///    same — only two distinct values across any seed or position. That is
///    correct for what it is, and it is a weak gate on its own.
/// 2. **A noise-driven fixture**, below. That is where float determinism could
///    actually diverge, so that is what the gate has to cover.
///
/// If this fails, read `tests/determinism.rs`'s header first: the same rules
/// apply, and **updating the constant to match is not one of them.**
#[test]
fn the_reference_generator_matches_its_golden_hashes() {
    // Only two distinct values exist, by construction — see above.
    const BELOW: u64 = 0xf7f8_857e_48f9_2325;
    const ABOVE: u64 = 0x4564_5dd3_5575_a325;

    const GOLDEN: [(u64, i32, i32, i32, u64); 6] = [
        (0, 0, -1, 0, BELOW),
        (0, 0, 0, 0, ABOVE),
        (42, 3, -1, -7, BELOW),
        (42, -100, -5, 100, BELOW),
        (7, 0, 1, 0, ABOVE),
        (u64::MAX, 12, -2, -12, BELOW),
    ];

    tiamot_core::detgen::assert_ieee_mode();
    let mut host = host_for(&game_dir());
    host.freeze().expect("freeze");

    assert_golden(&mut host, &GOLDEN, "reference mods");

    assert_ne!(
        BELOW, ABOVE,
        "the two halves must differ, or this proves nothing"
    );
}

/// The noise-driven half of the script gate.
///
/// A fixture generator that calls `game.noise_heightmap` — the path where a
/// platform difference in float behaviour would actually show up. Every case
/// here must produce a DISTINCT hash, which is asserted, because a gate whose
/// cases collide is weaker than it looks.
#[test]
fn script_driven_noise_worldgen_matches_its_golden_hashes() {
    const GOLDEN: [(u64, i32, i32, i32, u64); 6] = [
        (0, 0, 0, 0, 0x91b6_75ea_0ca4_2c55),
        (0, 1, 0, 0, 0x49a3_02ab_e052_4287),
        (0, 0, 0, 1, 0xc011_4a2a_1895_af17),
        (1, 0, 0, 0, 0x01c5_6344_13d5_2327),
        (42, -12, 0, 7, 0x5c20_d0b7_2247_5725),
        (u64::MAX, 100, 0, -100, 0xc821_b836_42f0_9877),
    ];

    tiamot_core::detgen::assert_ieee_mode();

    let root = scratch("noise-gate");
    write_mod(
        &root,
        "noisegen",
        "",
        r"
local solid = game.register_block{ id = 'solid' }

game.register_on_generate(function(buf, pos)
    local heights = game.noise_heightmap(pos, {
        octaves = 4,
        frequency = 0.03,
        amplitude = 6.0,
        base = 8,
    })
    buf:fill_below_heightmap(heights, solid)
end)
",
    );

    let mut host = host_for(&root);
    host.freeze().expect("freeze");

    assert_golden(&mut host, &GOLDEN, "noise fixture");

    // A gate whose cases collide could let a change move one onto another
    // unnoticed.
    let mut seen = std::collections::BTreeSet::new();
    for (seed, x, y, z, expected) in GOLDEN {
        assert!(
            seen.insert(expected),
            "seed {seed}, chunk ({x}, {y}, {z}) duplicates an earlier golden hash"
        );
    }
}

/// Shared golden-hash assertion, so both gates report failures the same way.
fn assert_golden(host: &mut EngineHost, golden: &[(u64, i32, i32, i32, u64)], what: &str) {
    let mut mismatches = Vec::new();
    for &(seed, x, y, z, expected) in golden {
        let chunk = host
            .generate_chunk(seed, ChunkPos::new(x, y, z), MaterialId::AIR)
            .expect("generate");
        let actual = hash_chunk(&chunk);
        if actual != expected {
            mismatches.push(format!(
                "  seed {seed}, chunk ({x}, {y}, {z}): expected {expected:#018x}, got {actual:#018x}"
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "SCRIPT-DRIVEN DETERMINISM GATE FAILED ({what}) on {} of {} cases:\n{}\n\n\
         Do NOT update these constants to match. See tests/determinism.rs.",
        mismatches.len(),
        golden.len(),
        mismatches.join("\n")
    );
}

/// FNV-1a over a chunk's materials. Same construction as `detgen::fingerprint`,
/// so the two gates are comparable.
fn hash_chunk(chunk: &tiamot_core::Chunk) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for index in 0..BLOCKS_PER_CHUNK {
        for cell in chunk.block_cells(LocalBlock::from_index(index)) {
            for byte in cell.get().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

// ---------------------------------------------------------------------------
// Crash isolation (charter rule 10)
// ---------------------------------------------------------------------------

#[test]
fn a_mod_that_fails_to_load_is_disabled_while_the_rest_keep_working() {
    let root = scratch("load-failure");
    write_mod(&root, "good", "", "game.register_block{ id = 'fine' }");
    write_mod(
        &root,
        "broken",
        "",
        "error('this mod is deliberately broken')",
    );

    let host = host_for(&root);

    assert_eq!(host.failed().len(), 1, "exactly one mod should have failed");
    assert_eq!(host.failed()[0].0, "broken");
    assert!(
        host.disabled().contains(&"broken".to_owned()),
        "the broken mod should be disabled: {:?}",
        host.disabled()
    );
    assert!(
        host.vm().block_ids().contains_key("good:fine"),
        "the healthy mod must still have registered: {:?}",
        host.vm().block_ids()
    );
}

#[test]
fn a_mod_that_faults_during_generation_is_disabled_and_the_world_keeps_working() {
    // The acceptance criterion. A mod that throws inside its generation
    // callback must not take the server with it.
    let root = scratch("runtime-fault");
    write_mod(
        &root,
        "good",
        "",
        r"
local id = game.register_block{ id = 'solid' }
game.register_on_generate(function(buf, pos)
    buf:fill_all(id)
end)
",
    );
    write_mod(
        &root,
        "zbroken",
        "",
        r"
game.register_on_generate(function(buf, pos)
    error('this generator is deliberately broken')
end)
",
    );

    let mut host = host_for(&root);
    host.freeze().expect("freeze");
    assert!(host.failed().is_empty(), "both mods should LOAD fine");

    // First generation hits the fault.
    let first = host.generate_chunk(1, ChunkPos::new(0, 0, 0), MaterialId::AIR);
    assert!(
        first.is_err(),
        "the faulting generator should report an error"
    );
    let err = first.expect_err("error");
    assert_eq!(
        err.mod_id(),
        Some("zbroken"),
        "the error must name the mod to blame: {err}"
    );
    assert!(
        host.disabled().contains(&"zbroken".to_owned()),
        "the faulting mod should now be disabled"
    );

    // Second generation skips it and the healthy generator still runs.
    let second = host
        .generate_chunk(1, ChunkPos::new(0, 0, 0), MaterialId::AIR)
        .expect("the world must keep generating once the bad mod is disabled");
    let solid = host
        .vm()
        .block_ids()
        .get("good:solid")
        .copied()
        .expect("registered");
    assert_eq!(
        second.is_uniform(),
        Some(solid),
        "the healthy generator's work must still be there"
    );
}

#[test]
fn an_infinite_loop_in_a_generator_is_stopped_rather_than_hanging_the_server() {
    let root = scratch("runaway");
    write_mod(
        &root,
        "runaway",
        "",
        r"
game.register_on_generate(function(buf, pos)
    while true do end
end)
",
    );

    let mut host: EngineHost = ModHost::load_from(
        &root,
        VmLimits {
            instructions_per_call: 200_000,
            ..VmLimits::default()
        },
    )
    .expect("load");
    host.freeze().expect("freeze");

    // Without the budget this never returns and the test suite hangs.
    let err = host
        .generate_chunk(1, ChunkPos::new(0, 0, 0), MaterialId::AIR)
        .expect_err("the budget must stop this");
    assert_eq!(err.mod_id(), Some("runaway"));
    assert!(
        host.disabled().contains(&"runaway".to_owned()),
        "a mod that burns its budget should be disabled"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn registration_after_freeze_is_refused() {
    let root = scratch("freeze");
    write_mod(&root, "late", "", "");

    let mut host = host_for(&root);
    assert_eq!(host.phase(), Phase::Registration);
    host.freeze().expect("freeze");
    assert_eq!(host.phase(), Phase::Frozen);

    let err = host
        .vm_mut()
        .eval_in("late", "game.register_block{ id = 'too_late' }")
        .expect_err("registration must be closed after freeze");
    assert!(
        err.to_string().contains("late"),
        "the error should attribute the mod: {err}"
    );
    assert!(
        !host.vm().block_ids().contains_key("late:too_late"),
        "nothing should have been registered"
    );
}

#[test]
fn a_mod_cannot_register_into_another_mods_namespace() {
    // What stops a third-party mod shadowing engine blocks.
    let root = scratch("namespace");
    write_mod(
        &root,
        "impostor",
        "",
        "game.register_block{ id = 'core:white' }",
    );

    let host = host_for(&root);
    assert_eq!(
        host.failed().len(),
        1,
        "registering into `core:` should have failed the mod"
    );
    assert!(
        !host.vm().block_ids().contains_key("core:white"),
        "the impostor must not have registered core:white"
    );
}

#[test]
fn the_engine_placeholder_material_is_not_registerable_by_mods() {
    // Charter rule 8: `engine:unknown` is the engine's, and content referencing
    // an absent mod must map to it. A mod claiming it would break that.
    let root = scratch("placeholder");
    write_mod(
        &root,
        "sneaky",
        "",
        "game.register_block{ id = 'engine:unknown' }",
    );

    let host = host_for(&root);
    assert_eq!(host.failed().len(), 1, "claiming engine: should fail");
    assert!(!host.vm().block_ids().contains_key("engine:unknown"));
}

#[test]
fn a_resolution_failure_is_fatal_rather_than_partial() {
    // A mod that fails to LOAD is disabled and the rest continue. A mod set
    // that fails to RESOLVE has no correct subset to fall back to — starting
    // anyway would mean starting a world the operator did not configure.
    let root = scratch("unresolvable");
    write_mod(&root, "needs_absent", "depends = [\"nowhere\"]", "");

    let result: Result<EngineHost, _> = ModHost::load_from(&root, VmLimits::default());
    let text = match result {
        Ok(_) => panic!("an unresolvable set must not start"),
        Err(err) => err.to_string(),
    };
    assert!(
        text.contains("resolve") || text.contains("dependenc"),
        "{text}"
    );
}

#[test]
fn the_resolved_set_has_a_stable_fingerprint() {
    let host = host_for(&game_dir());
    let again = host_for(&game_dir());
    assert_eq!(
        host.resolved().fingerprint(),
        again.resolved().fingerprint(),
        "the mod manifest fingerprint must be stable"
    );
}
