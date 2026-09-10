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

/// The fixture mods a developer is invited to copy into `game/` by hand.
///
/// `docs/fixtures/README.md` says to `cp -r docs/fixtures/relief game/relief`
/// and delete it afterwards, so a checkout with one sitting in `game/` is a
/// DOCUMENTED state, not a mistake — and before this existed it turned
/// `the_reference_mods_load_in_dependency_order` red, which reads exactly like
/// a regression in mod loading.
///
/// Read from the directory rather than hard-coded so the list cannot drift
/// from what is actually offered. Subtracting only these names is what keeps
/// the exhaustive assertion honest: a genuinely new reference mod in `game/`
/// is not in `docs/fixtures/`, so it still fails the test.
fn known_fixtures() -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/fixtures");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().join("mod.toml").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
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
    // Every mod in `game/`, in load order, minus any fixture copied in by hand
    // — see `known_fixtures`. Listed exhaustively rather than spot-checked:
    // this is the test that notices a reference mod being added or removed,
    // which is exactly the change most likely to be made without thinking
    // about load order.
    // **`core_*` is what a reference mod IS**, and `.gitignore` is where that is
    // decided: everything in `game/` is ignored except `README.md` and
    // `core_*/`, so a mod under any other name cannot be committed. The
    // exhaustive assertion loses nothing by scoping to them and stops being red
    // for every developer who keeps their own mods where the guide tells them
    // to — including the fixtures `known_fixtures` was written for, which are
    // not `core_*` either.
    //
    // A new reference mod still fails this, which is the point: it has to be
    // `core_*` to be committed at all.
    let fixtures = known_fixtures();
    let loaded: Vec<&str> = host
        .resolved()
        .ids()
        .into_iter()
        .filter(|id| *id == "core" || id.starts_with("core_"))
        .filter(|id| !fixtures.iter().any(|fixture| fixture == id))
        .collect();
    assert_eq!(
        loaded,
        vec![
            "core",
            "core_gear",
            "core_milk",
            "core_mimic",
            "core_sky",
            "core_tools",
            "core_ui",
            "core_worldgen"
        ],
        "core_worldgen depends on core and must load after it; core_milk depends on nothing \
         and registers its own block, so it sorts by name like the rest"
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
    // above — and the top block of that solid is `core:ground`, which drinks.
    // Milk poured on a world of nothing but `core:white` pools for ever,
    // because there is nowhere for it to go.
    let mut host = host_for(&game_dir());
    host.freeze().expect("freeze");
    assert_eq!(host.phase(), Phase::Frozen);

    let id_of = |name: &str| {
        host.vm()
            .block_ids()
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("the reference mods should register {name}"))
    };
    let white = id_of("core:white");
    let ground = id_of("core:ground");

    // Chunk (0, -1, 0) covers world y in -16..0 — entirely below the surface,
    // so it is NOT uniform any more: the top layer of it is the ground.
    let below = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            0,
            ChunkPos::new(0, -1, 0),
            MaterialId::AIR,
        )
        .expect("generate");
    assert_eq!(
        below.is_uniform(),
        None,
        "the chunk holding the surface is a layer of ground over white, not one material"
    );
    assert_eq!(
        below
            .get_block(tiamot_core::BlockPos::new(0, -1, 0))
            .expect("in chunk")
            .subnode(0),
        ground,
        "the top block of the world should be the absorbent layer"
    );
    assert_eq!(
        below
            .get_block(tiamot_core::BlockPos::new(0, -2, 0))
            .expect("in chunk")
            .subnode(0),
        white,
        "everything under the surface layer should be core:white"
    );

    // Chunk (0, 0, 0) covers world y in 0..16 — entirely above it.
    let above = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            0,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
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
        .generate_chunk(tiamot_core::domain::OVERWORLD, 42, pos, MaterialId::AIR)
        .expect("generate");
    let second = host
        .generate_chunk(tiamot_core::domain::OVERWORLD, 42, pos, MaterialId::AIR)
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
///    there are only THREE distinct values across any seed or position: the
///    chunk holding the surface layer, a chunk entirely under it, and a chunk
///    entirely above. That is correct for what it is, and it is a weak gate on
///    its own.
/// 2. **A noise-driven fixture**, below. That is where float determinism could
///    actually diverge, so that is what the gate has to cover.
///
/// If this fails, read `tests/determinism.rs`'s header first: the same rules
/// apply, and **updating the constant to match is not one of them.**
#[test]
fn the_reference_generator_matches_its_golden_hashes() {
    // Only three distinct values exist, by construction — see above.
    //
    // **Rebaselined 2026-08-26**, when the generator grew a one-block layer of
    // `core:ground` on top so that milk poured on the reference world has
    // somewhere to soak. A deliberate change to what the generator PRODUCES is
    // the sanctioned reason to move these; a difference with no such change is
    // a float divergence and the constants stay put.
    // Only SURFACE moved: a chunk entirely under the new layer, and one
    // entirely above it, hash exactly what they did before. That is the shape
    // a one-block change should have, and it is worth noticing that it does.
    const SURFACE: u64 = 0x299b_1a1c_ebd4_b325;
    const DEEP: u64 = 0xf7f8_857e_48f9_2325;
    const ABOVE: u64 = 0x4564_5dd3_5575_a325;

    const GOLDEN: [(u64, i32, i32, i32, u64); 6] = [
        (0, 0, -1, 0, SURFACE),
        (0, 0, 0, 0, ABOVE),
        (42, 3, -1, -7, SURFACE),
        (42, -100, -5, 100, DEEP),
        (7, 0, 1, 0, ABOVE),
        (u64::MAX, 12, -2, -12, DEEP),
    ];

    tiamot_core::detgen::assert_ieee_mode();
    let mut host = host_for(&game_dir());
    host.freeze().expect("freeze");

    assert_golden(&mut host, &GOLDEN, "reference mods");

    for (a, b) in [(SURFACE, DEEP), (SURFACE, ABOVE), (DEEP, ABOVE)] {
        assert_ne!(a, b, "the three shapes must differ, or this proves nothing");
    }
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
            .generate_chunk(
                tiamot_core::domain::OVERWORLD,
                seed,
                ChunkPos::new(x, y, z),
                MaterialId::AIR,
            )
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
    let first = host.generate_chunk(
        tiamot_core::domain::OVERWORLD,
        1,
        ChunkPos::new(0, 0, 0),
        MaterialId::AIR,
    );
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
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
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
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
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

#[test]
fn a_mod_cannot_register_one_hook_twice() {
    // **It used to overwrite and double-count.** The callback was stored under
    // one key per mod per hook, so a second `register_on_player_join` replaced
    // the first — and the mod's id went onto the caller list again, so the
    // surviving callback ran TWICE. A mod author writing the obvious thing —
    // two unrelated jobs on join, one `register_` call each — lost half their
    // code and had the other half run twice, with nothing said about either.
    //
    // Found while writing the cue tests, by writing exactly that mod.
    let root = scratch("double-hook");
    write_mod(
        &root,
        "twice",
        "",
        "game.register_on_player_join(function() end)\n\
         game.register_on_player_join(function() end)\n",
    );
    let host = host_for(&root);

    // Disabled rather than killing the load: a mod fault is that mod's problem
    // (charter rule 10).
    let failed = host.failed();
    assert_eq!(failed.len(), 1, "the mod should have been disabled");
    let reason = format!("{failed:?}");
    assert!(
        reason.contains("already registered"),
        "the message must say what happened: {reason}"
    );
    assert!(
        reason.contains("on_player_join"),
        "and WHICH hook, or an author has to guess: {reason}"
    );

    // The honest half: one registration is still fine.
    let root = scratch("single-hook");
    write_mod(
        &root,
        "once",
        "",
        "game.register_on_player_join(function() end)\n",
    );
    assert!(
        host_for(&root).failed().is_empty(),
        "one registration must still work, or this test proves nothing"
    );
}

#[test]
fn a_mod_can_generate_three_dimensional_terrain_with_caves() {
    // **The gap a heightmap cannot cross.** A height per column cannot describe
    // an overhang, an arch or a cave: those need a value at a point in space,
    // and a mod may not compute one itself (charter rule 4). So the mod
    // describes the field and the engine evaluates it.
    //
    // Asserted as a SHAPE rather than against a golden hash — the hash gate
    // covers reproducibility and this covers the thing a hash cannot say: that
    // the field is genuinely three-dimensional. Solid with air above it is what
    // a heightmap already did; what proves the mechanism is solid ABOVE air in
    // the same column somewhere in the chunk.
    let root = scratch("density");
    write_mod(
        &root,
        "caves",
        "",
        r#"
local stone = game.register_block{ id = "stone" }

-- Terrain that falls off with height, with tunnels cut out of it:
--   min(noise - y*0.06, 0.35 - abs(cave))
local field = game.density{
    op = "min",
    a = {
        op = "sub",
        a = { op = "noise", stream = "terrain", frequency = 0.03, octaves = 3 },
        b = { op = "mul", a = { op = "y" }, b = { op = "const", value = 0.06 } },
    },
    b = {
        op = "sub",
        a = { op = "const", value = 0.35 },
        b = { op = "abs", a = { op = "noise", stream = "caves", frequency = 0.05 } },
    },
}

game.register_on_generate(function(buf, pos)
    buf:fill_density(field, stone)
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");
    let stone = *host
        .vm()
        .block_ids()
        .get("caves:stone")
        .expect("the mod registers caves:stone");

    // Somewhere in the region there must be a column with solid ABOVE air —
    // an overhang or a cave roof. A heightmap cannot produce one.
    let mut overhangs = 0;
    for cx in -2..=2 {
        for cz in -2..=2 {
            let chunk = host
                .generate_chunk(
                    tiamot_core::domain::OVERWORLD,
                    4242,
                    ChunkPos::new(cx, -1, cz),
                    MaterialId::AIR,
                )
                .expect("generate");
            for x in 0..BLOCKS_PER_CHUNK.min(16) {
                for z in 0..16 {
                    let mut seen_air_above_solid = false;
                    let mut solid_below = false;
                    for y in 0..16 {
                        let solid = chunk
                            .get_block_local(LocalBlock::new(x as u32, y, z))
                            .subnode(0)
                            == stone;
                        if solid_below && !solid {
                            seen_air_above_solid = true;
                        }
                        if seen_air_above_solid && solid {
                            overhangs += 1;
                        }
                        solid_below = solid;
                    }
                }
            }
        }
    }
    assert!(
        overhangs > 0,
        "no column anywhere held solid above air, so the field is not three-dimensional \
         and a heightmap would have done"
    );
}

#[test]
fn a_mod_registers_a_font_and_a_style_may_name_it() {
    // **Charter rule 1 for the lettering.** A mod that can choose its blocks,
    // its sounds and its dialogs and not its typeface is a mod whose screens
    // all look like the engine's.
    //
    // Both halves in one mod, because a font nothing can name is not a
    // feature: the registration, and a style that asks for it. The style is
    // the half that would fail silently — `STYLE_FIELDS` refuses an unknown key
    // and takes the whole dialog down with it, which is what shipped twice for
    // block fields.
    let root = scratch("fonts");
    write_mod(
        &root,
        "lettering",
        "",
        r#"
game.register_font{ id = "display", file = "fonts/display.ttf" }
game.register_on_player_join(function(event)
    game.show_dialog{
        player = event.player,
        form = "titled",
        tree = {
            type = "container", direction = "column",
            children = {
                { type = "label", text = "Chapter One", font = "lettering:display", text_size = 24 },
            },
        },
    }
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");

    let fonts = host.vm().registered_fonts();
    assert_eq!(fonts.len(), 1, "the font did not register");
    assert_eq!(fonts[0].id, "lettering:display", "the id was not qualified");
    assert_eq!(fonts[0].mod_id, "lettering");
    assert_eq!(fonts[0].file, "fonts/display.ttf");
}

#[test]
fn a_mod_may_not_push_more_fonts_than_a_client_will_hold() {
    // The cap is about the client's glyph atlas as much as about parsing bytes
    // a server pushed — coverage is what costs, not the file. Refused where the
    // mod asked, rather than quietly dropped by the server later: a mod should
    // hear about its ninth font at the line that registers it.
    let root = scratch("too-many-fonts");
    let mut source = String::new();
    for index in 0..=tiamot_core::font::MAX_FONTS {
        source.push_str(&format!(
            "game.register_font{{ id = \"f{index}\", file = \"fonts/f{index}.ttf\" }}\n"
        ));
    }
    write_mod(&root, "greedy", "", &source);

    let host = host_for(&root);
    let failure = format!("{:?}", host.failed());
    assert!(
        failure.contains("at most"),
        "a mod registering {} fonts should be refused: {failure}",
        tiamot_core::font::MAX_FONTS + 1
    );
}

#[test]
fn a_block_may_declare_every_field_the_engine_documents() {
    // **The field allowlist is the last gate a mod passes, and it has been
    // forgotten twice.** `register_block` refuses any key it does not know, so
    // a field that exists everywhere else — read at registration, carried on
    // the wire, honoured by the client — is still unusable if its NAME is not
    // in `BLOCK_FIELDS`. The mod does not get a subtly wrong block; it fails to
    // load, with "unknown field".
    //
    // The engine's own tests all reach past this: they build a `MaterialDef`
    // or a `BlockRules` directly, which is the producing side. Only a mod
    // calling `register_block` meets the gate, so only a mod can prove it open.
    let root = scratch("block-fields");
    write_mod(
        &root,
        "everything",
        "",
        r#"
game.register_block{
    id = "window",
    name = "Window",
    transparent = true,
    tint = { strength = 0.2, scale = 24, low = {0.9, 1.0, 0.9}, high = {1.0, 1.0, 1.0} },
    hardness = 2.0,
    light_emit = { r = 0, g = 0, b = 0 },
    tags = { "glass" },
}
game.register_on_tick(function() end)
"#,
    );

    let host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "a block declaring documented fields should load: {:?}",
        host.failed()
    );
    assert!(
        host.vm().block_ids().contains_key("everything:window"),
        "the block never registered"
    );
}

#[test]
fn a_generator_can_ask_for_sub_node_terrain_without_writing_cells_by_hand() {
    // **Asked for by a mod author writing a worldgen mod**, whose complaint was
    // that sub-node worldgen has to be done a cell at a time and is therefore
    // too expensive to use. It was: `set_subnode` was the only sub-node write,
    // and covering a chunk with it is 110,592 calls into Lua.
    //
    // Now the resolution is an argument to the fill the mod was already doing,
    // and the whole 27x sample cost happens in Rust — and only for the blocks
    // the surface actually crosses, which is why it is 1.5x or 5.5x rather than
    // 27x. Sub-Node Contract §5: block resolution by default, sub-node opt-in.
    let root = scratch("subnode-density");
    write_mod(
        &root,
        "hills",
        "",
        r#"
local stone = game.register_block{ id = "stone" }

-- A surface that slopes, so it crosses blocks at an angle. At block resolution
-- this is a staircase; the whole point of sub-node detail is that it is not.
local field = {
    op = "sub",
    a = { op = "noise", stream = "terrain", frequency = 0.02, octaves = 3 },
    b = { op = "mul", a = { op = "y" }, b = { op = "const", value = 0.08 } },
}

game.register_on_generate(function(buf, pos)
    buf:fill_density(game.density(field), stone, { detail = "smooth" })
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");
    let stone = *host
        .vm()
        .block_ids()
        .get("hills:stone")
        .expect("the mod registers hills:stone");

    // Somewhere in the region there must be a PARTIAL block: one the surface
    // crosses, holding some of its 27 cells and not others. At block resolution
    // every block is all or nothing, and that is the staircase.
    let mut partial = 0;
    let mut solid = 0;
    for cy in -2..=0 {
        let chunk = host
            .generate_chunk(
                tiamot_core::domain::OVERWORLD,
                99,
                ChunkPos::new(0, cy, 0),
                MaterialId::AIR,
            )
            .expect("generate");
        for x in 0..16u32 {
            for y in 0..16u32 {
                for z in 0..16u32 {
                    let block = chunk.get_block_local(LocalBlock::new(x, y, z));
                    let filled = (0..27).filter(|cell| block.subnode(*cell) == stone).count();
                    if filled == 27 {
                        solid += 1;
                    } else if filled > 0 {
                        partial += 1;
                    }
                }
            }
        }
    }
    assert!(solid > 0, "the generator produced no terrain at all");
    assert!(
        partial > 0,
        "every block was all-or-nothing, so the fill was still at block resolution"
    );
}

#[test]
fn a_generator_can_fill_a_sea_around_its_terrain() {
    // **The only place an ocean can come from.** The conserved solver moves
    // what exists and creates nothing (Sub-Node Contract §4), so a sea is not
    // something a mod can pour — it has to be placed while the chunk is being
    // generated, or the world has no standing water in it at all. Asked for by
    // a mod author's assistant: "there is no fill_fluid yet".
    //
    // Fluid is a LAYER over the same blocks, not a material, so what this
    // asserts is that the sea went into the room the terrain left: nothing in
    // the solid part, and a full block's worth in the empty part.
    let root = scratch("sea");
    write_mod(
        &root,
        "ocean",
        "",
        r#"
local stone = game.register_block{ id = "stone" }
game.register_fluid{ id = "water", material = "ocean:stone" }

game.register_on_generate(function(buf, pos)
    -- Ground at y = 4, sea level at y = 12: eight blocks of water over it.
    buf:fill_below_heightmap(game.flat_heightmap(4), stone)
    buf:fill_fluid_below(12, "ocean:water")
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");
    // The ids the server would have assigned. Generation cannot resolve a name
    // without them, which is the seam this test also covers.
    host.vm_mut()
        .set_fluid_ids(&[("ocean:water".to_owned(), tiamot_core::fluid::FluidId(1))]);

    let (chunk, fluid) = host
        .generate_chunk_with_fluid(
            tiamot_core::domain::OVERWORLD,
            9,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
        .expect("generate");
    assert!(!chunk.is_uniform().is_some_and(|m| m == MaterialId::AIR));

    // Under the ground: solid, and no room for water.
    let below = fluid.get(tiamot_core::coords::LocalBlock::new(8, 2, 8));
    assert_eq!(
        below.volume(),
        0,
        "water was put inside the ground, which is room the terrain does not have"
    );

    // Between the ground and the sea level: a full block of it.
    let sea = fluid.get(tiamot_core::coords::LocalBlock::new(8, 8, 8));
    assert_eq!(
        sea.volume(),
        tiamot_core::fluid::MAX_VOLUME,
        "the sea did not fill the empty space below its level"
    );

    // Above the sea level: dry.
    let air = fluid.get(tiamot_core::coords::LocalBlock::new(8, 14, 8));
    assert_eq!(air.volume(), 0, "water above the level it was filled to");
}

#[test]
fn a_density_compiled_twice_from_an_edited_map_sees_the_edit() {
    // `{ op = "map" }` copies the field, and the copy is cached so a generator
    // that compiles its program per chunk — which is what the shortest correct
    // code does — does not copy four megabytes per chunk. That cache is only
    // sound if an edit to the map is visible to the next compile.
    //
    // **The mod does the asserting**, because a fault in `on_world_init` is
    // reported and a `game.log` line is not: the check has to be able to fail.
    let root = scratch("snapshot");
    write_mod(
        &root,
        "snap",
        "",
        r#"
game.register_block{ id = "stone" }

local function field()
    return game.map{ name = "f", side = 4, scale = 16 }
end

local function reading()
    return game.density{ op = "map", map = field() }:bounds({ x = 0, y = 0, z = 0 }).high
end

game.register_on_world_init(function()
    local map = field()
    map:offset(5.0)
    local first = reading()
    -- The same map, untouched: served from the cached copy, same answer.
    local again = reading()
    if math.abs(first - again) > 0.001 then
        error(string.format("two compiles of an untouched map disagreed: %f vs %f", first, again))
    end
    -- Edited: the copy is stale and has to be retaken.
    map:offset(10.0)
    local after = reading()
    if after < again + 5.0 then
        error(string.format("a compile after an edit saw %f, still the old %f", after, again))
    end
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");
    let faults = host.vm_mut().world_init().expect("pre-pass");
    assert!(
        faults.is_empty(),
        "the map snapshot is wrong — the mod's own check failed: {faults:?}"
    );
}

#[test]
fn a_map_and_a_density_can_feed_each_other_so_erosion_is_expressible() {
    // **The loop erosion needs, and the half that was missing.** A map could
    // only ever leave through `heightmap`, which feeds `fill_below_heightmap`
    // and nothing else — so a world whose surface is a DENSITY (a dome,
    // overhangs, caves) could compute an eroded field and had no way to read
    // it. Both directions exist now: `map:fill(density)` takes a surface into
    // a field, and `{ op = "map" }` reads a field back into a surface.
    //
    // The mod below is a small erosion: take the surface, smooth it, keep the
    // lower of the two — a valley cut rather than a hill averaged away — and
    // then build terrain from the result.
    let root = scratch("erosion");
    write_mod(
        &root,
        "erode",
        "",
        r#"
local stone = game.register_block{ id = "stone" }

-- The surface, as a density: this is the shape a heightmap cannot describe.
local SURFACE = {
    op = "sub",
    a = { op = "noise", stream = "terrain", frequency = 0.01, octaves = 4, amplitude = 40.0 },
    b = { op = "y" },
}

local function field()
    return game.map{ name = "land", side = 64, scale = 16 }
end

game.register_on_world_init(function()
    local land = field()
    -- Read the density's own surface into the map. `y = 0` is where a field
    -- of the form `noise - y` changes sign, so this is its height.
    land:fill(game.density(SURFACE), { y = 0.0, seed = 99 })

    local smoothed = game.map{ name = "smoothed", side = 64, scale = 16 }
    smoothed:fill(game.density(SURFACE), { y = 0.0, seed = 99 })
    smoothed:blur(3)
    land:combine(smoothed, "min")
end)

game.register_on_generate(function(buf, pos)
    -- And back out again: solid below the eroded field.
    local eroded = game.density{
        op = "sub",
        a = { op = "map", map = field() },
        b = { op = "y" },
    }
    -- What the engine can say about this chunk without evaluating it. A mod
    -- may skip its own work on the strength of it; the engine skips its own
    -- either way.
    local bounds = eroded:bounds(pos)
    if bounds.all_empty then
        return
    end
    buf:fill_density(eroded, stone)
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");
    let faults = host.vm_mut().world_init().expect("pre-pass");
    assert!(faults.is_empty(), "the pre-pass faulted: {faults:?}");

    // Under the surface: solid. Well above it: air. The field runs to about
    // +/-40 blocks around zero, so a chunk at y = -64 is under all of it and
    // one at y = 64 is over all of it.
    let deep = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, -4, 0),
            MaterialId::AIR,
        )
        .expect("generate");
    let sky = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, 4, 0),
            MaterialId::AIR,
        )
        .expect("generate");

    // `is_uniform` is how the other generation tests read a chunk back: it
    // answers with the one material a chunk is made of, or `None` when it
    // holds more than one.
    assert_ne!(
        deep.is_uniform(),
        Some(MaterialId::AIR),
        "the ground under an eroded field should not be entirely air"
    );
    assert_eq!(
        sky.is_uniform(),
        Some(MaterialId::AIR),
        "the sky over an eroded field should be empty"
    );
}

#[test]
fn a_world_pre_pass_computes_a_map_that_generation_then_reads() {
    // **The shape a river needs and a density field cannot have.** Where water
    // goes depends on where the land is everywhere else, so it cannot be a
    // function of one position — it has to be a whole field, computed once,
    // in passes that see all of it. The mod says which passes; the engine
    // holds the array and does the arithmetic, because charter rule 4 forbids
    // a script doing it per sample.
    let root = scratch("prepass");
    write_mod(
        &root,
        "land",
        "",
        r#"
local stone = game.register_block{ id = "stone" }

local function field()
    return game.map{ name = "height", side = 64, scale = 16 }
end

game.register_on_world_init(function()
    local map = field()
    map:noise{ seed = 99, frequency = 0.01, octaves = 4, amplitude = 40.0 }
    map:offset(20.0)
    -- Erosion, such as it is: smooth it and keep the lower of the two, which
    -- is a valley being cut rather than a hill being averaged away.
    local smooth = game.map{ name = "smoothed", side = 64, scale = 16 }
    smooth:noise{ seed = 99, frequency = 0.01, octaves = 4, amplitude = 40.0 }
    smooth:offset(20.0)
    smooth:blur(3)
    map:combine(smooth, "min")
end)

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(field():heightmap(pos), stone)
end)
"#,
    );

    let mut host = host_for(&root);
    assert!(
        host.failed().is_empty(),
        "the mod should load: {:?}",
        host.failed()
    );
    host.freeze().expect("freeze");

    // Nothing has run the pre-pass yet, so the map is flat and the world is
    // one height everywhere. This is the control: without it, a test that saw
    // terrain could not tell the pre-pass from the noise.
    let flat = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
        .expect("generate");

    let faults = host.vm_mut().world_init().expect("pre-pass");
    assert!(faults.is_empty(), "the pre-pass faulted: {faults:?}");

    let shaped = host
        .generate_chunk(
            tiamot_core::domain::OVERWORLD,
            1,
            ChunkPos::new(0, 0, 0),
            MaterialId::AIR,
        )
        .expect("generate");
    assert_ne!(
        flat.is_uniform(),
        None,
        "the control should be uniform before the pre-pass ran"
    );
    assert_ne!(
        shaped.is_uniform(),
        flat.is_uniform(),
        "generation produced the same chunk before and after the pre-pass, so it is \
         not reading the map"
    );

    // And the map came out for the caller that stores it, under the mod and
    // name the script asked for.
    let maps = host.vm_mut().take_maps();
    let names: Vec<&str> = maps.iter().map(|(_, name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"height") && names.contains(&"smoothed"),
        "the pre-pass built maps the server cannot save: {names:?}"
    );
    assert!(
        maps.iter().all(|(mod_id, _, _)| mod_id == "land"),
        "a map came back under the wrong mod"
    );
}
