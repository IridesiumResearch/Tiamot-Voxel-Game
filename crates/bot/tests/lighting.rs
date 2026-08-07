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
