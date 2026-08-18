// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! `game.line_of_sight`, over a real server.
//!
//! # Why this cannot be a unit test
//!
//! The engine answers sight questions out of the world, and the world is the one
//! mod-facing thing that does not live behind a lock for the whole run: the tick
//! thread owns it and lends it to the mods for the part of each tick that runs
//! their callbacks (`server::sight`). Every piece of that is unit-tested on its
//! own — the traversal in `core::sight`, the lease in `server::sight`, the trip
//! through Lua in the VM's own tests — and all three can pass while the window
//! is wired into the wrong part of the tick, in which case a mod sees `nil`
//! for ever and every one of those tests still goes green.
//!
//! So this test runs a mod that asks, on a server that is really ticking, and
//! checks the answer came back at all.
//!
//! # How the answer gets out
//!
//! The mod turns each answer into a block edit at a known position, and the bot
//! waits for the broadcast. There is no "read a block" message and there should
//! not be one, so a marker block is how a mod tells a test anything.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::{BlockPos, SubNodePos};
use tiamot_server::{ServerHandle, Settings};

/// Long enough for the mod's twentieth tick (one second) plus the round trip,
/// short enough that a wedged server fails rather than hangs the suite.
const PATIENCE: Duration = Duration::from_secs(10);

/// Where the mod writes its answers. Well above the ground and inside the
/// spawn chunk, so a marker is never confused with terrain and is always
/// somewhere the joining bot is being sent chunks for.
const SAW_THROUGH_AIR: BlockPos = BlockPos::new(1, 8, 1);
const STOPPED_BY_GROUND: BlockPos = BlockPos::new(3, 8, 1);
const HAD_NO_WORLD: BlockPos = BlockPos::new(5, 8, 1);

/// The middle cell of a block, which is what a whole-block edit fills and the
/// only resolution the bot exposes for asking what it was told.
fn centre_of(pos: BlockPos) -> SubNodePos {
    SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-perception").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that looks twice and reports what it saw.
///
/// The two lines are chosen so that neither answer can be produced by accident:
/// one runs horizontally through open air well above the ground, and the other
/// runs straight down through the floor the player is standing on. A server
/// that answered `true` to everything would fail the second, and one that
/// answered `false` to everything would fail the first.
fn write_seer(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("seer");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"seer\"\nname = \"Seer\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        // The wait is not superstition: the sight test has to happen after the
        // chunks it looks through have been generated and loaded, which happens
        // when the bot joins and asks for them. Looking on tick 1 would report
        // unloaded terrain, which reads as blocked — a true answer to a
        // question this test is not asking.
        "local ground = game.register_block{ id = \"ground\" }\n\
         local marker = game.register_block{ id = \"marker\" }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
         end)\n\
         local turn = 0\n\
         game.register_on_tick(function()\n\
         \x20   turn = turn + 1\n\
         \x20   if turn < 20 then return end\n\
         \x20   local across = game.line_of_sight(\n\
         \x20       { x = 2.5, y = 4.5, z = 1.5 }, { x = 8.5, y = 4.5, z = 1.5 })\n\
         \x20   local down = game.line_of_sight(\n\
         \x20       { x = 2.5, y = 4.5, z = 1.5 }, { x = 2.5, y = -3.5, z = 1.5 })\n\
         \x20   if across == nil or down == nil then\n\
         \x20       game.set_block({ x = 5, y = 8, z = 1 }, \"seer:marker\")\n\
         \x20   else\n\
         \x20       if across then\n\
         \x20           game.set_block({ x = 1, y = 8, z = 1 }, \"seer:marker\")\n\
         \x20       end\n\
         \x20       if not down then\n\
         \x20           game.set_block({ x = 3, y = 8, z = 1 }, \"seer:marker\")\n\
         \x20       end\n\
         \x20   end\n\
         end)\n",
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
        seed: Some(7),
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

async fn join(server: &ServerHandle) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join("Watcher").await.expect("join");
    bot
}

/// The marker's numeric id, from the running server rather than assumed: ids
/// come from registration order, and a constant here would test that order.
fn marker_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server sends a material table on join")
        .into_iter()
        .find(|entry| entry.name == "seer:marker")
        .map(|entry| entry.id)
        .expect("the mod registers a marker")
}

#[test]
fn a_mod_can_see_through_air_and_not_through_the_ground() {
    let server = start("sight", write_seer("sight"));
    block_on(async {
        let mut bot = join(&server).await;
        let marker = marker_id(&bot);

        bot.expect_block(SAW_THROUGH_AIR, marker, PATIENCE)
            .await
            .expect("the mod should see across open air");

        bot.expect_block(STOPPED_BY_GROUND, marker, PATIENCE)
            .await
            .expect("the mod should not see down through the floor");
    });
}

#[test]
fn a_mod_asking_during_a_tick_is_never_told_there_is_no_world() {
    // The failure this exists for: every other test of this feature passes with
    // the lending window wired into the wrong part of the tick, because a mod
    // that always gets `nil` never writes a marker at all — and "no marker
    // arrived" is also what a broken broadcast looks like. This asserts the
    // shape of the answer directly.
    let server = start("window", write_seer("window"));
    block_on(async {
        let mut bot = join(&server).await;
        let marker = marker_id(&bot);

        // One real answer proves the tick reached the mod at all.
        bot.expect_block(SAW_THROUGH_AIR, marker, PATIENCE)
            .await
            .expect("the mod should have looked by now");

        // And the `nil` marker must not be among what arrived. Checked after a
        // real answer landed, so this is not merely a race the assertion won.
        assert!(
            !bot.saw_subnode(centre_of(HAD_NO_WORLD), marker),
            "the mod was told there was no world to look through, \
             which means the world is not lent where the callbacks run"
        );
    });
}
