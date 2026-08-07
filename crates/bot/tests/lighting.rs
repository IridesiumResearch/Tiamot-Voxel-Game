// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Light, over a real server: Task 10's criterion A1 end to end.
//!
//! The core suites prove propagation is correct against a fixture world. These
//! prove the *server* runs it on the world players are in, and that the answer
//! reaches a client over the wire — which is where the interesting failures are,
//! because the two halves are joined by a chunk cache, a tick loop, a cap, and a
//! run-length codec.
//!
//! # Why these assert on values and not on screenshots
//!
//! "Caves are dark and lamps are coloured" is a statement about numbers before
//! it is a statement about pixels. A screenshot test can only fail *after* a
//! renderer exists and can fail for a dozen reasons that have nothing to do with
//! light. These fail for exactly one.

use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::light::MAX_LEVEL;
use tiamot_core::{BlockPos, MaterialId};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];

fn stone() -> u16 {
    let mut registry = tiamot_core::Registry::new();
    let mut id = MaterialId::AIR;
    for (index, name) in MATERIALS.iter().enumerate() {
        let assigned = registry.register(name).expect("register");
        if index == 0 {
            id = assigned;
        }
    }
    id.0
}

fn reference_mods() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn start(name: &str) -> ServerHandle {
    let dir = std::env::temp_dir().join(format!("tiamot-light-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("world dir");

    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        seed: Some(7),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
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
fn the_open_sky_reaches_a_client_as_full_daylight() {
    // The baseline. If this fails nothing else in the file means anything,
    // because every other assertion is relative to daylight being present.
    let server = start("daylight");

    block_on(async {
        let mut bot = join(&server, "Sunbather").await;
        // Well above the reference generator's terrain, so nothing is in the
        // way of the sky.
        let open = BlockPos::new(4, 20, 4);

        let level = bot
            .expect_light(open, |light| light.sun() > 0, Duration::from_secs(20))
            .await
            .unwrap_or_else(|err| panic!("no daylight arrived: {err}"));

        assert_eq!(
            level.sun(),
            MAX_LEVEL,
            "open sky should be full daylight, got {level:?}"
        );
    });

    assert!(server.stop());
}

#[test]
fn ground_level_is_lit_and_the_rock_below_it_is_not() {
    // **Criterion A1's "caves are dark", in the smallest world that has one.**
    // `core_worldgen` fills below its heightmap, so a surface of 0 puts the
    // highest solid block at y = -1 and everything under that is enclosed rock.
    let server = start("underground");

    block_on(async {
        let mut bot = join(&server, "Miner").await;

        let above = BlockPos::new(4, 1, 4);
        bot.expect_light(above, |light| light.sun() > 0, Duration::from_secs(20))
            .await
            .expect("the surface should be lit");

        // Deep enough that no sideways leak from the surface can reach it: the
        // lateral gradient is one level per block, so sixteen blocks down is
        // beyond anything daylight can do.
        let deep = BlockPos::new(4, -16, 4);
        let level = bot
            .expect_light(deep, |_| true, Duration::from_secs(20))
            .await
            .expect("light for the chunk below should arrive");

        assert_eq!(
            level.sun(),
            0,
            "sunlight reached sixteen blocks inside solid rock: {level:?}"
        );
        assert!(
            level.is_dark(),
            "buried rock should be pitch black with no lamps in the world: {level:?}"
        );
    });

    assert!(server.stop());
}

#[test]
fn breaking_the_surface_lets_daylight_into_the_hole() {
    // Light following an edit, over the wire, with nobody asking for it: the
    // server relights, notices the chunk changed, and sends it. A client that
    // had to ask would show a black hole until it did.
    let server = start("dig-in");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Digger").await;

        // The top solid block of the reference generator's ground. Solid rock
        // holds no light, so this block is dark until it stops being rock —
        // and the block UNDER it stays dark either way, because digging one
        // block leaves the next one solid.
        let surface = BlockPos::new(2, -1, 0);

        bot.expect_light(surface, |_| true, Duration::from_secs(20))
            .await
            .expect("light should arrive for the ground");
        let before = bot.light_at(surface).expect("light");
        assert_eq!(
            before.sun(),
            0,
            "the surface block started lit while it was still solid rock, so this test proves \
             nothing"
        );

        bot.dig_block(surface).await.expect("dig the surface away");

        let after = bot
            .expect_light(surface, |light| light.sun() > 0, Duration::from_secs(20))
            .await
            .unwrap_or_else(|err| panic!("daylight never reached the hole: {err}"));
        assert_eq!(
            after.sun(),
            MAX_LEVEL,
            "a hole open straight to the sky should be full daylight, got {after:?}"
        );

        // And its neighbour, still solid, is still dark — which is what makes
        // the assertion above about the hole rather than about the chunk.
        assert_eq!(
            bot.light_at(BlockPos::new(2, -2, 0)).expect("light").sun(),
            0,
            "digging one block lit the solid rock beneath it"
        );
        let _ = stone;
    });

    assert!(server.stop());
}

#[test]
fn a_chunk_is_lit_once_however_many_players_ask_for_it() {
    // Players who join together ask for the same chunks, and relighting one
    // that already has light produces the answer it already had — at about
    // 1.4 ms a chunk, measured, which is what turned four bots joining at once
    // into 22 ms ticks in the macro benchmark.
    //
    // Counted rather than timed. A timing assertion on a shared CI runner is a
    // coin toss; "every chunk was lit exactly once" is a property of the code
    // and holds on any machine.
    let server = start("lit-once");
    let control = server.control().clone();

    block_on(async {
        let mut first = join(&server, "First").await;
        first
            .collect_chunks(24, Duration::from_secs(20))
            .await
            .expect("the first player's chunks");
        let after_one = control.full_relights();
        assert!(
            after_one > 0,
            "no chunk was lit at all, so this test is measuring nothing"
        );

        let mut second = join(&server, "Second").await;
        second
            .collect_chunks(24, Duration::from_secs(20))
            .await
            .expect("the second player's chunks");

        // Give the tick a moment to finish anything the second join started.
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            control.full_relights(),
            control.lit_chunks(),
            "chunks were relit that already had light: {} relights for {} lit chunks",
            control.full_relights(),
            control.lit_chunks()
        );
    });

    assert!(server.stop());
}

/// A mod that refuses to break anything the sun can see.
///
/// A rule expressed entirely in terms of `game.get_light`, and one that reads
/// like something a real mod would do — "you may only mine underground" is a
/// perfectly ordinary game rule. What makes it a good test is that the answer
/// is observable from outside: the block either goes or it does not.
fn write_light_reader(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join("tiamot-light-reader").join(name);
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("watcher");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"watcher\"\nname = \"Watcher\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        "local ground = game.register_block{ id = \"ground\" }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
         end)\n\
         game.register_tool{ id = \"hand\", brush = \"block\", speed_multiplier = 1.0, \
         default = true }\n\
         game.register_on_dig_complete(function(event)\n\
         \x20   -- A dig event is in CELL coordinates, three per block, and\n\
         \x20   -- `game.get_light` takes blocks. Getting this wrong reads the\n\
         \x20   -- light three times too far out and the rule silently inverts.\n\
         \x20   local bx, by, bz = event.x // 3, event.y // 3, event.z // 3\n\
         \x20   local above = game.get_light{ x = bx, y = by + 1, z = bz }\n\
         \x20   game.log(\"light above the dig: sun=\" .. above.sun)\n\
         \x20   if above.sun > 0 then\n\
         \x20       return false\n\
         \x20   end\n\
         end)\n",
    )
    .expect("script");
    root
}

#[test]
fn a_mod_can_read_the_light_where_something_happened() {
    // `game.get_light` over the real path: a mod callback, inside a tick, on a
    // world whose light the server computed this tick. A mod deciding whether
    // something may happen somewhere dark is the reason it exists.
    //
    // The assertion is the mod's RULE rather than the mod merely running: a
    // surface block has daylight above it and survives, a buried one does not
    // and goes. Both directions, because a `get_light` that returned darkness
    // for everything would pass a test that only dug underground.
    let mods = write_light_reader("dig");
    let dir = std::env::temp_dir().join("tiamot-light-reader-world");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("world dir");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(11),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start");

    block_on(async {
        let mut bot = join(&server, "Digger").await;
        // The surface, whose neighbour above is open sky.
        let lit = BlockPos::new(2, -1, 0);
        // Three blocks down, whose neighbour above is solid rock.
        let buried = BlockPos::new(2, -4, 0);

        bot.expect_light(lit, |_| true, Duration::from_secs(20))
            .await
            .expect("light should arrive for the ground");

        // Refused: the mod read full daylight above it. `dig_block` waits for
        // the block to become air and times out when it never does, which is
        // the veto working.
        let refused = bot.dig_block(lit).await;
        assert!(
            refused.is_err(),
            "the surface block broke, so the mod saw no daylight above it"
        );

        // Allowed: rock above it, so the mod read darkness.
        bot.dig_block(buried)
            .await
            .expect("a buried block should break, so the mod read darkness above it");
    });

    assert!(server.stop());
}
