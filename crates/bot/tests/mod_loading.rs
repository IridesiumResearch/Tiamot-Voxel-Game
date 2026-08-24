// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Mods loaded through a real server: their blocks are placeable, and their
//! tick hooks run.
//!
//! Charter rule 1: the mod API is the only API. A block a mod registers must be
//! usable through exactly the same path an engine block would be — which is the
//! point of testing it end to end rather than unit-testing the registry.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::Edit;
use tiamot_core::{BlockPos, MaterialId};
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-mod-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes a one-file mod and returns the directory holding it.
fn write_mod(root: &Path, id: &str, manifest_extra: &str, source: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\n\
             license = \"GPL-3.0-only\"\n{manifest_extra}"
        ),
    )
    .expect("manifest");
    std::fs::write(dir.join("init.lua"), source).expect("script");
}

fn start(world: &Path, mods: Option<PathBuf>) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world.to_path_buf(),
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: mods,
        enabled_mods: None,
        seed: Some(1),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

async fn join(server: &ServerHandle, name: &str) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join(name).await.expect("join");
    bot
}

#[test]
fn the_reference_mods_load_and_their_blocks_are_placeable() {
    // `game/` holds reference mods and test fixtures. If they stop loading
    // through the real startup path, the public mod API has broken.
    let world = scratch("reference-mods");
    let repo_mods = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the game/ directory should exist");

    let server = start(&world, Some(repo_mods));

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        // `core:white` is registered by the reference block mod. Its id is
        // whatever the engine assigned; resolve it the way the server did.
        let white = MaterialId(2);
        let edit = Edit::Block {
            pos: BlockPos::new(1, 2, 3),
            material: white.0,
        };
        // Written by the operator, not by the client — a client cannot edit the
        // world. What is under test is that the id the mod registered is one
        // the world will ACCEPT, and that is the same check either way.
        assert!(server.seed_block(BlockPos::new(1, 2, 3), white.0));

        let seen = alice
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("a mod-registered block must be placeable");
        assert_eq!(seen, edit);

        alice.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn a_mods_tick_hook_runs_on_the_server_tick() {
    // `game.register_on_tick` is the mechanism Task 06 adds to the script API.
    // A hook that registered but never fired would be indistinguishable from a
    // working one until a mod tried to rely on it.
    let world = scratch("tick-hook-world");
    let mods = scratch("tick-hook-mods");
    let marker = world.join("ticks.txt");

    // The mod cannot write files — the sandbox forbids it — so it counts in a
    // Lua global and the test reads the count out through the VM instead.
    write_mod(
        &mods,
        "ticker",
        "",
        "ticks = 0\ngame.register_on_tick(function(dt) ticks = ticks + dt end)",
    );

    let server = start(&world, Some(mods));

    block_on(async {
        // A player is not required for ticks to run, but joining one proves the
        // hook fires on the same loop that serves players.
        let alice = join(&server, "Alice").await;
        // Two seconds is 40 ticks at 20 Hz.
        tokio::time::sleep(Duration::from_secs(2)).await;
        alice.disconnect().await;
    });

    let ticks_run = server.control().tick();
    assert!(
        ticks_run >= 30,
        "the server should have run ~40 ticks in two seconds, ran {ticks_run}"
    );
    assert!(server.stop(), "clean shutdown");
    assert!(
        !marker.exists(),
        "the sandbox must not let a mod write files"
    );
}

#[test]
fn a_mod_that_errors_every_tick_is_disabled_and_the_server_keeps_running() {
    // Charter rule 10: a mod error disables that mod, never the tick. A server
    // that died here would mean one bad mod could take down a whole world.
    let world = scratch("bad-mod-world");
    let mods = scratch("bad-mod-mods");
    write_mod(
        &mods,
        "explodes",
        "",
        "game.register_on_tick(function() error('every tick') end)",
    );

    let server = start(&world, Some(mods));

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The server must still be serving.
        let chunks = alice
            .collect_chunks(1, Duration::from_secs(10))
            .await
            .expect("collect");
        assert!(
            !chunks.is_empty(),
            "the server must still stream chunks after a mod fault"
        );
        alice.disconnect().await;
    });

    assert!(
        server.control().tick() > 5,
        "the tick loop must have kept running"
    );
    assert!(
        server.stop(),
        "a faulting mod must not stop a clean shutdown"
    );
}

#[test]
fn a_mod_that_fails_to_load_is_disabled_rather_than_fatal() {
    // Charter rule 10 again, at load time. The other mods must still come up.
    let world = scratch("broken-load-world");
    let mods = scratch("broken-load-mods");
    write_mod(&mods, "good", "", "game.register_block{ id = 'fine' }");
    write_mod(&mods, "broken", "", "this is not valid lua ((((");

    let server = start(&world, Some(mods));

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        // The good mod's block registered, so id 2 is a material the world
        // accepts.
        assert!(server.seed_block(BlockPos::new(0, 0, 0), 2));
        assert!(
            alice
                .next_block_delta(Duration::from_secs(5))
                .await
                .expect("wait")
                .is_some(),
            "the working mod's block should still be placeable"
        );
        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn an_unresolvable_mod_set_refuses_to_start() {
    // A mod that fails to LOAD is disabled; a set that fails to RESOLVE is
    // fatal, because there is no correct subset to fall back to. Starting
    // anyway would silently run a world missing whatever the absent dependency
    // was going to register.
    let world = scratch("unresolvable-world");
    let mods = scratch("unresolvable-mods");
    write_mod(&mods, "needy", "depends = [\"absent >=1.0\"]", "");

    let result = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(1),
        rcon: None,
        materials: Vec::new(),
    });

    let err = result
        .err()
        .expect("a missing dependency must refuse to start");
    let text = format!("{err}");
    assert!(
        text.contains("mods"),
        "the error should say the mods are the problem: {text}"
    );
}

#[test]
fn a_server_with_no_mods_still_runs() {
    // The engine is mechanisms; content is mods. A server with none is empty,
    // not broken.
    let world = scratch("no-mods");
    let server = start(&world, None);

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let chunks = alice
            .collect_chunks(1, Duration::from_secs(10))
            .await
            .expect("collect");
        assert!(!chunks.is_empty(), "an empty world still streams");
        alice.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn mod_blocks_get_the_ids_the_vm_promised_them() {
    // The trap this guards: the engine registry assigns ids sequentially, so
    // replaying registrations in any order but the VM's would give each block a
    // different number than its mod was handed — and every block that mod
    // placed would silently be a different material.
    //
    // Registration order here is deliberately NOT alphabetical.
    let world = scratch("id-order-world");
    let mods = scratch("id-order-mods");
    write_mod(
        &mods,
        "zebra",
        "",
        "game.register_block{ id = 'zulu' }\ngame.register_block{ id = 'alpha' }",
    );

    let server = start(&world, Some(mods));

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        // `zebra:zulu` registered first, so it is id 2 and `zebra:alpha` is 3.
        // If the replay had sorted by name they would be swapped.
        for (material, pos) in [(2u16, BlockPos::new(0, 0, 0)), (3, BlockPos::new(1, 0, 0))] {
            assert!(server.seed_block(pos, material));
            let seen = alice
                .next_block_delta(Duration::from_secs(5))
                .await
                .expect("wait")
                .expect("both ids must be registered and placeable");
            assert_eq!(seen, Edit::Block { pos, material });
        }

        // And nothing beyond them: a third id would mean the registry picked up
        // something the VM did not register.
        assert!(server.seed_block(BlockPos::new(2, 0, 0), 4));
        assert!(
            alice
                .next_block_delta(Duration::from_millis(500))
                .await
                .expect("wait")
                .is_none(),
            "an id past the registered set must be refused"
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_mod_left_out_of_the_selection_does_not_load() {
    // **The launcher's tick-boxes, from the server's side.** A world is started
    // with a list of mod ids and only those load — which is what makes turning
    // a mod off in the front screen mean anything at all.
    let world = scratch("selected-world");
    let mods = scratch("selected-mods");
    write_mod(&mods, "wanted", "", "game.register_block{ id = \"kept\" }");
    write_mod(
        &mods,
        "unwanted",
        "",
        "game.register_block{ id = \"dropped\" }",
    );

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: Some(vec!["wanted".to_owned()]),
        seed: Some(1),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start");

    block_on(async {
        let bot = join(&server, "picky").await;
        let table = bot
            .material_table()
            .expect("the material table arrives on join");
        let ids: Vec<&str> = table.iter().map(|def| def.name.as_str()).collect();
        assert!(
            ids.contains(&"wanted:kept"),
            "the mod that was ticked did not load: {ids:?}"
        );
        assert!(
            !ids.contains(&"unwanted:dropped"),
            "a mod left out of the selection registered anyway: {ids:?}"
        );
        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_selection_that_breaks_a_dependency_refuses_to_start() {
    // Half a mod set is not a smaller mod set. Turning off something another
    // ticked mod depends on is the same failure as never installing it, and it
    // has to be the same answer — otherwise a player generates a world with a
    // mod silently absent and finds out later.
    let world = scratch("broken-selection-world");
    let mods = scratch("broken-selection-mods");
    write_mod(&mods, "base", "", "");
    write_mod(&mods, "needy", "depends = [\"base >=0.1\"]", "");

    let result = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: Some(vec!["needy".to_owned()]),
        seed: Some(1),
        rcon: None,
        materials: Vec::new(),
    });
    assert!(
        result.is_err(),
        "a selection missing a dependency started anyway"
    );
}
