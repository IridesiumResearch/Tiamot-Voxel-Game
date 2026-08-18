// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! `game.set_block`, over a real server.
//!
//! # The bug this exists for
//!
//! `game.set_block` had a queue type (`server::fluid::Edits`), a VM setter
//! (`ScriptVm::set_world_edit`), stubs promising it worked, and unit tests
//! proving it did. Nothing ever called the setter on a running server. So the
//! slot behind the function was empty for the whole life of every real world,
//! and an empty slot is not an error — it is exactly what a mod gets during
//! worldgen, where "there is no world yet" is the truth. A mod placing a block
//! therefore did nothing, silently, and every test in the tree stayed green.
//!
//! The lesson is narrow and worth keeping: **a seam installed only by its own
//! tests is not installed.** Every `set_*_access` on the VM needs one test that
//! reaches it through a server nobody stubbed.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::BlockPos;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

/// Where the mod builds. Inside the spawn chunk and well above the ground, so
/// nothing generated can be mistaken for what the mod put there.
const PLACED: BlockPos = BlockPos::new(2, 9, 2);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-mod-edits").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that places one block from its tick hook and then stops.
fn write_builder(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("builder");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"builder\"\nname = \"Builder\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        "local ground = game.register_block{ id = \"ground\" }\n\
         local brick = game.register_block{ id = \"brick\" }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
         end)\n\
         game.register_on_tick(function()\n\
         \x20   game.set_block({ x = 2, y = 9, z = 2 }, \"builder:brick\")\n\
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
        seed: Some(11),
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

#[test]
fn a_block_a_mod_places_reaches_the_world() {
    let server = start("place", write_builder("place"));
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Bystander").await.expect("join");

        let brick = bot
            .material_table()
            .expect("the server sends a material table on join")
            .into_iter()
            .find(|entry| entry.name == "builder:brick")
            .map(|entry| entry.id)
            .expect("the mod registers a brick");

        bot.expect_block(PLACED, brick, PATIENCE)
            .await
            .expect("a mod's set_block should land in the world and be broadcast");
    });
}
