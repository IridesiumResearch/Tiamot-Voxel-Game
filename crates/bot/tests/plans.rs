// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! `game.plans`, over a real server.
//!
//! # Why this test exists at all
//!
//! `set_plan_access` is the seventh `set_*_access` on the VM, and the sixth one
//! taught this codebase the lesson: a seam installed only by its own tests is
//! not installed. `game.set_block` had a queue type, a setter, stubs and unit
//! tests, and nothing ever called the setter on a running server — so it did
//! nothing, silently, for three tasks, because an empty slot is exactly what a
//! mod gets during worldgen and is not an error there. See `mod_edits.rs`.
//!
//! Plans have MORE places to be dead than that did: a capture goes through the
//! lease, a save goes through the world's database, and a stamp goes through a
//! queue the tick has to remember to pump. Each of those is a slot that answers
//! "nothing happened" when it is not wired, so each is exercised here through a
//! server nobody stubbed.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::BlockPos;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

/// How long to wait for something the server has to tick before it is true.
///
/// Thirty seconds for the reason `mod_edits.rs` gives: nothing here is slower
/// for it, because every wait ends on the condition rather than on the clock,
/// and ten seconds is a bet on how fast the machine is under a full parallel
/// workspace run.
const PATIENCE: Duration = Duration::from_secs(30);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-plans").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that builds two blocks, captures them, and stamps the plan elsewhere.
///
/// Every step reports itself by placing a marker block, which is how a mod's
/// answer reaches a test: there is no "what did your capture say" message on
/// the wire and there should not be one.
///
/// **Each marker is latched on its own.** Chunks arrive over several ticks, so
/// a mod that did all of this inside one `if` would lose whichever step's
/// terrain was not loaded on the tick the first one succeeded — the mistake
/// `mod_edits.rs` records having made.
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
        r#"
local ground = game.register_block{ id = "ground" }
local brick = game.register_block{ id = "brick" }
game.register_block{ id = "saw_capture" }
game.register_block{ id = "saw_list" }
game.register_block{ id = "saw_forget" }

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

local seen = {}
local function once(marker, at)
    if seen[marker] then
        return false
    end
    seen[marker] = true
    game.set_block(at, marker)
    return true
end

game.register_on_tick(function()
    -- The build to capture: two bricks in the air, where nothing generated
    -- could be mistaken for them.
    game.set_block({ x = 2, y = 9, z = 2 }, "builder:brick")
    game.set_block({ x = 3, y = 9, z = 2 }, "builder:brick")

    local placed = game.get_block({ x = 3, y = 9, z = 2 })
    if placed == nil or placed.material ~= brick then
        return
    end

    if not seen["captured"] then
        local made = game.plans.capture("hut", { x = 2, y = 9, z = 2 }, { x = 3, y = 9, z = 2 })
        if made == nil then
            return
        end
        seen["captured"] = true
        -- Two whole blocks of brick: 54 units, charter rule 5.
        if made.blocks == 2 and made.materials["builder:brick"] == 54
            and made.size.x == 2 and made.size.y == 1 and made.size.z == 1 then
            once("builder:saw_capture", { x = 5, y = 9, z = 2 })
        end
        local names = game.plans.list()
        if #names == 1 and names[1] == "hut" then
            once("builder:saw_list", { x = 6, y = 9, z = 2 })
        end
        -- And put it somewhere else, which is the whole point of having one.
        game.plans.stamp("hut", { x = 20, y = 9, z = 2 })
    end
end)

-- Forgetting is a separate hook so it cannot race the stamp above: a plan the
-- mod deleted before the tick pumped it would still stamp, and a test that
-- happened to pass either way would say nothing.
game.register_on_chat(function(event)
    if event.text == "forget" then
        if game.plans.forget("hut") and #game.plans.list() == 0
            and game.plans.info("hut") == nil then
            game.set_block({ x = 7, y = 9, z = 2 }, "builder:saw_forget")
        end
        return false
    end
end)
"#,
    )
    .expect("script");
    root
}

/// A mod that stamps a plan it did not capture, and never builds anything.
///
/// The second half of the persistence test: it shares the `builder` id, so the
/// plans the first run saved are its own, and it registers the same blocks in
/// the same order so the world's material ids still mean what they meant.
fn write_stamper(name: &str) -> PathBuf {
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
        r#"
local ground = game.register_block{ id = "ground" }
game.register_block{ id = "brick" }
game.register_block{ id = "saw_capture" }
game.register_block{ id = "saw_list" }
game.register_block{ id = "saw_forget" }

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

local done = false
game.register_on_tick(function()
    if done then
        return
    end
    -- A fresh VM, so this local is per RUN: the plan is the only thing that
    -- crossed the restart.
    if game.plans.stamp("hut", { x = 30, y = 9, z = 2 }) then
        done = true
    end
end)
"#,
    )
    .expect("script");
    root
}

fn start(mods: PathBuf, world: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        identity_path: None,
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

async fn watcher(server: &ServerHandle, name: &str) -> Bot {
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

fn material(bot: &Bot, name: &str) -> u16 {
    bot.material_table()
        .expect("the server sends a material table on join")
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.id)
        .unwrap_or_else(|| panic!("the mod registers {name}"))
}

#[test]
fn a_mod_captures_a_build_and_stamps_it_somewhere_else() {
    let world = scratch("stamp-world");
    let server = start(write_builder("stamp"), world);
    block_on(async {
        let mut bot = watcher(&server, "Bystander").await;
        let brick = material(&bot, "builder:brick");

        // What the capture SAID, first: a mod that is handed a plan whose size
        // or tally is wrong cannot tell until it builds the thing.
        for (at, marker) in [
            (BlockPos::new(5, 9, 2), "builder:saw_capture"),
            (BlockPos::new(6, 9, 2), "builder:saw_list"),
        ] {
            let id = material(&bot, marker);
            bot.expect_block(at, id, PATIENCE)
                .await
                .unwrap_or_else(|err| panic!("the mod never reported {marker}: {err}"));
        }

        // And what it BUILT: both blocks of the plan, at the offset it was
        // stamped to, which is the whole path — capture, save, load, queue,
        // apply, broadcast.
        for at in [BlockPos::new(20, 9, 2), BlockPos::new(21, 9, 2)] {
            bot.expect_block(at, brick, PATIENCE)
                .await
                .unwrap_or_else(|err| {
                    panic!("a stamped plan did not reach the world at {at:?}: {err}")
                });
        }

        // Then take it away again, on a hook of its own so it cannot race the
        // stamp above.
        bot.chat("forget").await.expect("chat");
        let forgotten = material(&bot, "builder:saw_forget");
        bot.expect_block(BlockPos::new(7, 9, 2), forgotten, PATIENCE)
            .await
            .expect("forgetting a plan should remove it from the store");
    });
}

#[test]
fn a_plan_outlives_the_world_that_captured_it() {
    // A plan is kept in the world's own database rather than in memory, which
    // is a claim only a restart can check. The second run's mod cannot capture
    // anything — it has no build to capture from and never calls capture — so
    // the only way the blocks appear is the saved plan being read back.
    let world = scratch("restart-world");
    let first = start(write_builder("restart-first"), world.clone());
    block_on(async {
        let mut bot = watcher(&first, "Builder").await;
        let brick = material(&bot, "builder:brick");
        bot.expect_block(BlockPos::new(20, 9, 2), brick, PATIENCE)
            .await
            .expect("the first run should capture and stamp");
    });
    assert!(first.stop(), "the world did not flush cleanly");

    let second = start(write_stamper("restart-second"), world);
    block_on(async {
        let mut bot = watcher(&second, "Visitor").await;
        let brick = material(&bot, "builder:brick");
        for at in [BlockPos::new(30, 9, 2), BlockPos::new(31, 9, 2)] {
            bot.expect_block(at, brick, PATIENCE)
                .await
                .unwrap_or_else(|err| {
                    panic!("a plan saved by an earlier run did not stamp at {at:?}: {err}")
                });
        }
    });
}
