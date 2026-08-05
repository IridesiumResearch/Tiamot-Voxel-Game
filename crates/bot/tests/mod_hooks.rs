// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Cancellable mod hooks, over a real server.
//!
//! Charter rule 1: the mod API is the only API, and rules about what a player
//! may do belong in mods rather than in the engine. `on_dig_complete` and
//! `on_place` are how a mod says no — a protection plugin, a claim system, a
//! tutorial that will not let you break the wrong thing.
//!
//! # Every veto test is paired with a permissive twin
//!
//! "The block is still there" is satisfied by a server where digging is broken
//! for reasons that have nothing to do with the hook. So each refusal test has
//! a counterpart running the SAME scenario against a mod whose hook returns
//! nothing, and asserts the action goes through. A change that broke digging
//! outright would pass the first test and fail the second.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::{BlockPos, MaterialId, SubNodePos};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 1] = ["test:stone"];

fn stone() -> u16 {
    let mut registry = tiamot_core::Registry::new();
    let mut id = MaterialId::AIR;
    for name in MATERIALS {
        id = registry.register(name).expect("register");
    }
    id.0
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-mod-hooks").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes a mod directory holding a default tool and the given hook bodies.
///
/// The tool registration is not incidental: the engine has no bare hand of its
/// own (charter rule 1), so a mod set with no tools is one nobody can dig in —
/// and a veto test against a world where digging was impossible anyway would
/// prove nothing at all.
fn write_warden(name: &str, hooks: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("warden");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"warden\"\nname = \"Warden\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "game.register_tool{{\n\
             \x20   id = \"hand\",\n\
             \x20   brush = \"block\",\n\
             \x20   speed_multiplier = 1.0,\n\
             \x20   default = true,\n\
             }}\n\
             {hooks}\n"
        ),
    )
    .expect("script");
    root
}

fn start(name: &str, mods: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(3),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
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

async fn join(server: &ServerHandle) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join("Subject").await.expect("join");
    bot
}

fn centre_of(pos: BlockPos) -> SubNodePos {
    SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1)
}

/// Seeds a block, digs it, and reports whether the server removed it.
///
/// The Task 07 direct-edit path seeds it, because what is under test is the
/// dig, not the seeding.
async fn dig_and_see(bot: &mut Bot, pos: BlockPos, material: u16) -> bool {
    bot.place(pos, material).await.expect("seed");
    bot.expect_block(pos, material, Duration::from_secs(10))
        .await
        .expect("the seed should land");

    bot.select_tool(None).await.expect("bare hand");
    bot.start_dig(centre_of(pos)).await.expect("start dig");

    // A whole block at the default hardness is 15 ticks; three seconds is
    // several times that, so a timeout here means it is not going to happen
    // rather than that it has not happened yet.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(200), bot.recv()).await;
        let removed = bot.received().into_iter().any(|message| {
            matches!(
                message,
                tiamot_core::proto::ServerMessage::BlockDelta {
                    edit: tiamot_core::proto::Edit::Block { pos: at, material: m },
                    ..
                } if at == pos && m == MaterialId::AIR.0
            )
        });
        if removed {
            return true;
        }
    }
    false
}

#[test]
fn a_mod_can_refuse_a_dig() {
    let server = start(
        "dig-refused",
        write_warden(
            "dig-refused",
            "game.register_on_dig_complete(function() return false end)",
        ),
    );
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let removed = dig_and_see(&mut bot, BlockPos::new(8, 40, 8), stone).await;
        assert!(
            !removed,
            "the mod refused the dig and the block went anyway"
        );
        assert!(
            bot.inventory().is_empty(),
            "a refused dig still credited the player: {:?}",
            bot.inventory()
        );
    });

    assert!(server.stop());
}

#[test]
fn a_hook_that_does_not_refuse_lets_the_dig_through() {
    // The twin of the test above. Without this, "the block survived" would also
    // be satisfied by a server on which nothing can be dug at all.
    let server = start(
        "dig-allowed",
        write_warden(
            "dig-allowed",
            "seen = 0\ngame.register_on_dig_complete(function() seen = seen + 1 end)",
        ),
    );
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let removed = dig_and_see(&mut bot, BlockPos::new(8, 40, 8), stone).await;
        assert!(
            removed,
            "a hook that returned nothing cancelled the dig; only an explicit false should"
        );
    });

    assert!(server.stop());
}

#[test]
fn a_mod_that_throws_while_vetoing_does_not_stop_the_dig() {
    // Charter rule 10 through the whole stack: a crash disables that mod and
    // the world keeps working. If a fault counted as a refusal, one broken mod
    // would make the server unmineable for everybody.
    let server = start(
        "dig-throws",
        write_warden(
            "dig-throws",
            "game.register_on_dig_complete(function() error('boom') end)",
        ),
    );
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let removed = dig_and_see(&mut bot, BlockPos::new(8, 40, 8), stone).await;
        assert!(
            removed,
            "a mod's crash was treated as a veto, so one bad mod can stop the server digging"
        );
    });

    assert!(server.stop());
}

#[test]
fn a_mod_can_refuse_a_placement_and_the_player_keeps_their_material() {
    let server = start(
        "place-refused",
        write_warden(
            "place-refused",
            "game.register_on_place(function() return false end)",
        ),
    );
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;

        // Mine a block so there is something to place, which also proves the
        // veto is specific to placing rather than a mod that broke everything.
        let quarry = BlockPos::new(8, 40, 8);
        assert!(
            dig_and_see(&mut bot, quarry, stone).await,
            "digging should still work; only placing is refused here"
        );
        bot.await_inventory(Duration::from_secs(10))
            .await
            .expect("the dig should credit");
        let before = bot.inventory();
        assert!(!before.is_empty(), "nothing was credited to place with");

        let target = BlockPos::new(8, 40, 12);
        bot.place_from_inventory(centre_of(target), stone)
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), bot.recv()).await;

        assert!(
            bot.notices()
                .iter()
                .any(|text| text.contains("cannot build")),
            "the refusal was silent; notices {:?}",
            bot.notices()
        );
        assert_eq!(
            bot.inventory(),
            before,
            "a refused placement charged the player anyway"
        );
    });

    assert!(server.stop());
}

#[test]
fn a_hook_can_refuse_selectively_using_the_event_it_is_given() {
    // The useful case, and the one that proves the event payload is real: a
    // mod that protects one region and allows everywhere else. A hook that
    // could only say "no to everything" would not be worth having.
    let guarded = BlockPos::new(8, 40, 8);
    let server = start(
        "selective",
        write_warden(
            "selective",
            &format!(
                "game.register_on_dig_complete(function(e)\n\
                 \x20   if e.x >= {} and e.x <= {} then return false end\n\
                 end)",
                guarded.x * 3,
                guarded.x * 3 + 2,
            ),
        ),
    );
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        assert!(
            !dig_and_see(&mut bot, guarded, stone).await,
            "the guarded block was dug"
        );
        assert!(
            dig_and_see(&mut bot, BlockPos::new(20, 40, 8), stone).await,
            "a block outside the guarded range was refused too, so the hook is not reading \
             the event"
        );
    });

    assert!(server.stop());
}

/// Where the reference mods live, for the test below.
fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

#[test]
fn the_reference_mods_register_no_vetoes() {
    // `game/` is a set of test fixtures, not a game (charter scope discipline),
    // and nothing in it should be quietly refusing player actions. If this ever
    // fails, a fixture has grown an opinion — which would make every other
    // test in the suite depend on it.
    let server = start("reference", reference_mods());
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        assert!(
            dig_and_see(&mut bot, BlockPos::new(8, 40, 8), stone).await,
            "a reference mod is refusing digs"
        );
    });

    assert!(server.stop());
}
