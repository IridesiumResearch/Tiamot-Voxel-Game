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
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
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

/// A mod that READS a block and reports what it found by writing a marker.
///
/// The reporting-by-marker trick is `perception.rs`'s: there is no "read a
/// block" message on the wire and there should not be one, so a mod's answer
/// reaches a test the only way anything reaches a client — as an edit.
fn write_reader(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("reader");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"reader\"\nname = \"Reader\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
local brick = game.register_block{ id = "brick" }
game.register_block{ id = "saw_ground" }
game.register_block{ id = "saw_brick" }
game.register_block{ id = "saw_air" }
game.register_block{ id = "saw_nothing" }

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

-- Put something down, then read the world back: the block it placed, a block
-- of the terrain under it, empty space beside it, and somewhere nobody has
-- loaded.
local reported = false
game.register_on_tick(function()
    game.set_block({ x = 2, y = 9, z = 2 }, "reader:brick")
    if reported then
        return
    end
    local placed = game.get_block({ x = 2, y = 9, z = 2 })
    if placed == nil or placed.material ~= brick then
        return
    end
    reported = true

    if placed.occupancy == game.OCCUPANCY_FULL then
        game.set_block({ x = 4, y = 9, z = 2 }, "reader:saw_brick")
    end

    local under = game.get_block({ x = 2, y = -1, z = 2 })
    if under ~= nil and under.material == ground then
        game.set_block({ x = 5, y = 9, z = 2 }, "reader:saw_ground")
    end

    -- **Air is an answer**, and this is the assertion that matters most:
    -- nothing there reads as air, not as nil.
    local beside = game.get_block({ x = 2, y = 9, z = 3 })
    if beside ~= nil and beside.material == game.AIR and beside.occupancy == 0 then
        game.set_block({ x = 6, y = 9, z = 2 }, "reader:saw_air")
    end

    -- And somewhere nobody is standing, which must NOT be generated to answer.
    if game.get_block({ x = 40000, y = 9, z = 40000 }) == nil then
        game.set_block({ x = 7, y = 9, z = 2 }, "reader:saw_nothing")
    end
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn a_mod_can_read_the_world_it_writes_to() {
    // **The seam test.** `game.get_block` reaches the world through the same
    // lease `line_of_sight` does, and a lease nobody installed answers
    // "unavailable" for ever without erroring — which is exactly how
    // `set_block` was dead on every real server for three tasks.
    //
    // Four markers, one per thing a mod has to be able to tell apart: its own
    // block, the terrain, empty space, and somewhere it cannot see.
    let server = start("read", write_reader("read"));
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Bystander").await.expect("join");

        let table = bot
            .material_table()
            .expect("the server sends a material table on join");
        let id = |name: &str| {
            table
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.id)
                .unwrap_or_else(|| panic!("the mod registers {name}"))
        };

        for (at, marker) in [
            (BlockPos::new(4, 9, 2), "reader:saw_brick"),
            (BlockPos::new(5, 9, 2), "reader:saw_ground"),
            (BlockPos::new(6, 9, 2), "reader:saw_air"),
            (BlockPos::new(7, 9, 2), "reader:saw_nothing"),
        ] {
            bot.expect_block(at, id(marker), PATIENCE)
                .await
                .unwrap_or_else(|err| panic!("the mod never reported {marker}: {err}"));
        }
    });
}
