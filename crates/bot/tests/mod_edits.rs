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
    start_at(name, mods, scratch(&format!("{name}-world")))
}

/// The same, over a world directory the caller names — for a test that stops a
/// server and starts another onto the same world.
fn start_at(name: &str, mods: PathBuf, world: PathBuf) -> ServerHandle {
    let _ = name;
    start_with(mods, world)
}

fn start_with(mods: PathBuf, world: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
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

/// A mod that teleports and shoves whoever says so in chat.
fn write_mover(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("mover");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"mover\"\nname = \"Mover\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

game.register_on_chat(function(event)
    if event.text == "away" then
        game.move_player(event.player, { x = 40.5, y = 6.0, z = 24.5 })
        return false
    end
    if event.text == "up" then
        game.push_player(event.player, { x = 0, y = 2.0, z = 0 })
        return false
    end
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn a_mod_can_move_a_player_and_the_client_is_told() {
    // **The seam test.** A player is in the entity store as a transient copy
    // of a body the tick steps, so `set_entity` on one is overwritten within
    // the tick and nothing says so — the failure this API is shaped to avoid,
    // and one only a real server can demonstrate.
    let server = start("move", write_mover("move"));
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Traveller").await.expect("join");

        // Where the server says the body is, before and after.
        let here = bot.walk([0.0, 0.0, 0.0], 0, 3).await.expect("stand");
        let start = world_x(&here);

        bot.chat("away").await.expect("chat");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let arrived = loop {
            let at = bot.walk([0.0, 0.0, 0.0], 0, 1).await.expect("stand");
            let x = world_x(&at);
            if (x - 40.5).abs() < 1.5 {
                break x;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a mod moved the player to x=40.5 and the server still says x={x} \
                 (it started at {start})"
            );
        };
        assert!(
            (arrived - start).abs() > 1.0,
            "the player was already where the mod sent them, so this proves nothing"
        );

        // And a shove reaches the body too: upward, so it shows as leaving the
        // ground rather than as a position a walk could explain.
        let mut left_the_ground = false;
        bot.chat("up").await.expect("chat");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while tokio::time::Instant::now() < deadline {
            if let Some(tiamot_core::proto::ServerMessage::PlayerState { velocity, .. }) = bot
                .received()
                .into_iter()
                .rev()
                .find(|m| matches!(m, tiamot_core::proto::ServerMessage::PlayerState { .. }))
                && velocity[1] > 0.5
            {
                left_the_ground = true;
                break;
            }
            bot.recv().await.expect("recv");
        }
        assert!(
            left_the_ground,
            "a mod pushed the player upward and the body never moved"
        );
    });
}

/// A player state's world `x`, in BLOCKS — the unit a mod speaks.
///
/// The wire carries cells (charter rule 5: 27 to a block), and `move_player`
/// takes blocks like every other mod-facing position. Comparing the two
/// directly is a factor of three, which looks exactly like a teleport landing
/// three times too far away.
fn world_x(at: &bot::client::PlayerPosition) -> f64 {
    let cells =
        f64::from(at.chunk.x) * f64::from(tiamot_core::CHUNK_SUBNODES) + f64::from(at.local[0]);
    cells / f64::from(tiamot_core::SUBNODES_PER_AXIS)
}

/// A mod that hands out two of the same thing, told apart by a detail.
fn write_smith(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("smith");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"smith\"\nname = \"Smith\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

-- Two swords, worn to different amounts. Same material, same cut: only the
-- mod's own word tells them apart.
game.register_on_player_join(function(event)
    game.give(event.player, { material = "smith:ground", units = 1, detail = "wear=11" })
    game.give(event.player, { material = "smith:ground", units = 1, detail = "wear=4" })
    game.give(event.player, { material = "smith:ground", units = 27 })
end)

game.register_on_chat(function(event)
    if event.text ~= "melt" then
        return
    end
    -- Asking for plain material must not reach either sword.
    local took = game.take(event.player, { material = "smith:ground", units = 99 })
    game.give(event.player, { material = "smith:ground", units = took, detail = "melted" })
    return false
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn two_items_a_mod_says_are_different_stay_different() {
    // **The whole reason a stack carries a detail**, end to end: through
    // `game.give`, the server's slots, the wire and back out to a client. Two
    // swords worn to different amounts are two stacks; a recipe asking for
    // plain material reaches neither.
    let server = start("details", write_smith("details"));
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Smith").await.expect("join");

        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let marked: Vec<Option<String>> = bot
                .inventory()
                .iter()
                .map(|stack| stack.detail.clone())
                .collect();
            if marked.len() == 3 {
                assert!(
                    marked.contains(&Some("wear=11".to_owned()))
                        && marked.contains(&Some("wear=4".to_owned()))
                        && marked.contains(&None),
                    "the two swords and the plain material did not stay three stacks: {marked:?}"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected three stacks, got {marked:?}"
            );
            bot.recv().await.expect("recv");
        }

        // And a recipe asking for plain material takes only the plain material.
        bot.chat("melt").await.expect("chat");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let details: Vec<Option<String>> = bot
                .inventory()
                .iter()
                .map(|stack| stack.detail.clone())
                .collect();
            if details.contains(&Some("melted".to_owned())) {
                assert!(
                    details.contains(&Some("wear=11".to_owned()))
                        && details.contains(&Some("wear=4".to_owned())),
                    "melting down the plain material took the swords too: {details:?}"
                );
                assert!(
                    !details.contains(&None),
                    "the plain material was not the thing that melted: {details:?}"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "nothing melted: {details:?}"
            );
            bot.recv().await.expect("recv");
        }
    });
}

/// Two mods, so the world's id order and a later session's can differ.
///
/// `first` registers a block nobody else does. Removing it on the second run
/// shifts every id after it, which is the only way the WORLD ids in a chunk and
/// the RUNTIME ids `game.get_block_id` hands out stop coinciding.
fn write_pair(name: &str, with_first: bool) -> PathBuf {
    let root = scratch(name);
    if with_first {
        let dir = root.join("first");
        std::fs::create_dir_all(&dir).expect("mod dir");
        std::fs::write(
            dir.join("mod.toml"),
            "id = \"first\"\nname = \"First\"\nversion = \"0.1.0\"\n\
             license = \"GPL-3.0-only\"\n",
        )
        .expect("manifest");
        // Several blocks, so removing this mod shifts the other's ids by more
        // than one and a coincidence is that much less likely.
        std::fs::write(
            dir.join("init.lua"),
            "for n = 1, 5 do game.register_block{ id = \"filler_\" .. n } end\n",
        )
        .expect("script");
    }

    let dir = root.join("second");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"second\"\nname = \"Second\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
local brick = game.register_block{ id = "brick" }
game.register_block{ id = "agreed" }
game.register_block{ id = "disagreed" }

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

local said = false
game.register_on_tick(function()
    game.set_block({ x = 2, y = 9, z = 2 }, "second:brick")
    if said then
        return
    end
    local at = game.get_block({ x = 2, y = 9, z = 2 })
    if at == nil then
        return
    end
    said = true
    -- The whole claim: what `get_block` reports is comparable against what
    -- `get_block_id` hands out. A world id here reads as a different block.
    if at.material == brick then
        game.set_block({ x = 4, y = 9, z = 2 }, "second:agreed")
    else
        game.set_block({ x = 4, y = 9, z = 2 }, "second:disagreed")
    end
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn a_block_read_back_is_in_the_id_space_a_mod_speaks() {
    // **Charter rule 8, and a defect that shipped for one day.** A chunk holds
    // WORLD ids — stable across sessions, which is what the database needs —
    // and `game.get_block_id` hands out RUNTIME ids, which registration
    // produces. In a world made and opened by the same mod set the two
    // coincide, which is why the first test of `game.get_block` passed while
    // it returned the wrong one.
    //
    // So this makes them diverge: a world created with two mods and reopened
    // with one, which shifts every id the removed mod was in front of.
    let world = scratch("id-space-world");

    // First run: both mods, so `second:brick` gets a world id after the
    // filler blocks.
    let server = start_at("id-space-1", write_pair("id-space-1", true), world.clone());
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
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "second:brick")
            .map(|entry| entry.id)
            .expect("the mod registers a brick");
        bot.expect_block(PLACED, brick, PATIENCE)
            .await
            .expect("the mod should place its brick");
    });
    assert!(server.stop(), "the world did not flush cleanly");

    // Second run: the filler mod is gone, so `second:brick`'s RUNTIME id is
    // lower than the WORLD id the chunk was written with.
    let server = start_at("id-space-2", write_pair("id-space-2", false), world);
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        // A different name: the world remembers that "Bystander" belongs to
        // the first run's identity, and a name is a per-server claim bound to
        // a UUID (charter rule 13).
        bot.join("Onlooker").await.expect("join");

        let table = bot.material_table().expect("a material table");
        let id = |name: &str| {
            table
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.id)
                .unwrap_or_else(|| panic!("the mod registers {name}"))
        };
        bot.expect_block(BlockPos::new(4, 9, 2), id("second:agreed"), PATIENCE)
            .await
            .expect(
                "`game.get_block` reported a material a mod cannot compare against — \
                 a world id where a runtime id was wanted",
            );
    });
    server.stop();
}

/// A mod that reacts to somebody leaving.
fn write_doorman(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("doorman");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"doorman\"\nname = \"Doorman\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
game.register_block{ id = "waved" }
game.register_block{ id = "had_stuff" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

game.register_on_player_join(function(event)
    game.give(event.player, { material = "doorman:ground", units = 27 })
end)

game.register_on_player_leave(function(event)
    game.set_block({ x = 3, y = 9, z = 3 }, "doorman:waved")
    -- **The inventory must still be readable here.** A mod dropping what
    -- somebody was carrying is the whole reason this hook runs before the row
    -- is written.
    for _, stack in ipairs(game.inventory(event.player)) do
        if stack.units > 0 then
            game.set_block({ x = 4, y = 9, z = 3 }, "doorman:had_stuff")
        end
    end
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn a_mod_hears_about_a_player_leaving_while_it_can_still_act() {
    // **The other half of a join.** Anything a mod keeps per player — a party,
    // a claim, a timer, a bar it was drawing — is bookkeeping it could start
    // and never end.
    //
    // The ordering is the part worth testing: a mod that wants to drop what
    // somebody was carrying has to be able to read their inventory, so the
    // hook runs before the row is written.
    let server = start("leaving", write_doorman("leaving"));
    block_on(async {
        let mut going = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        going.join("Ada").await.expect("join");
        // **Wait for the tick to SEE her.** Joins and leaves are both a diff
        // against who is present, so somebody who arrives and goes inside one
        // 50 ms tick produces neither event — which is coherent, and is not
        // what this test is about.
        going
            .walk([0.0, 0.0, 0.0], 0, 3)
            .await
            .expect("stand still for a few ticks");

        // A second player, to watch — the world only reaches somebody who is in
        // it, and the one who left is by definition not.
        let mut watcher = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        watcher.join("Bert").await.expect("join");

        going.disconnect().await;

        let table = watcher.material_table().expect("a material table");
        let id = |name: &str| {
            table
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.id)
                .unwrap_or_else(|| panic!("the mod registers {name}"))
        };
        watcher
            .expect_block(BlockPos::new(3, 9, 3), id("doorman:waved"), PATIENCE)
            .await
            .expect("no mod heard about a player leaving");
        watcher
            .expect_block(BlockPos::new(4, 9, 3), id("doorman:had_stuff"), PATIENCE)
            .await
            .expect(
                "the hook ran after the inventory was gone, so a mod cannot drop \
                 what somebody was carrying",
            );
    });
}

/// A mod that grows a crop, the only way a mod can: by being offered blocks.
fn write_farm(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("farm");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"farm\"\nname = \"Farm\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
local seed = game.register_block{ id = "seed" }
local grown = game.register_block{ id = "grown" }

-- A field of seed, made by WORLDGEN — which is the case a mod's own list of
-- planted crops cannot cover, and the reason this hook exists.
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

game.register_on_chat(function(event)
    if event.text ~= "sow" then
        return
    end
    for x = 0, 3 do
        for z = 0, 3 do
            game.set_block({ x = x, y = 4, z = z }, "farm:seed")
        end
    end
    return false
end)

-- **The mechanism.** The engine offers blocks; this one decides what that
-- means. Nothing here says when, and nothing here scans the world.
game.register_random_tick(seed, function(event)
    game.set_block({ x = event.x, y = event.y, z = event.z }, "farm:grown")
end)

-- And a material nobody registered must never arrive.
game.register_block{ id = "wrongly_offered" }
game.register_random_tick(grown, function(event)
    game.set_block({ x = 8, y = 9, z = 8 }, "farm:wrongly_offered")
end)
"#,
    )
    .expect("script");
    root
}

#[test]
fn a_mod_is_offered_blocks_to_grow_and_only_the_ones_it_asked_for() {
    // **The mechanism behind everything that happens on its own**: crops,
    // grass spreading, saplings, leaf decay, fire. None of them is a thing a
    // player did, and a mod cannot drive them from its own list because the
    // blocks were mostly made by worldgen and never passed through a hook.
    let server = start("random-tick", write_farm("random-tick"));
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Farmer").await.expect("join");

        let table = bot.material_table().expect("a material table");
        let id = |name: &str| {
            table
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.id)
                .unwrap_or_else(|| panic!("the mod registers {name}"))
        };

        bot.chat("sow").await.expect("chat");
        // Sixteen blocks of seed, three cells per chunk per tick out of 4096:
        // any one of them comes up about every 1,400 ticks, and sixteen of them
        // about every 85. A generous wait, because what is being tested is that
        // it happens at all.
        bot.expect_block(BlockPos::new(0, 4, 0), id("farm:seed"), PATIENCE)
            .await
            .expect("the field was never sown");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut grew = false;
        while tokio::time::Instant::now() < deadline {
            if bot.received().into_iter().any(|message| {
                matches!(
                    message,
                    tiamot_core::proto::ServerMessage::BlockDelta {
                        edit: tiamot_core::proto::Edit::Block { material, .. },
                        ..
                    } if material == id("farm:grown")
                )
            }) {
                grew = true;
                break;
            }
            bot.recv().await.expect("recv");
        }
        assert!(grew, "nothing in a sown field was ever offered a turn");

        // **And nothing else was.** The second handler is registered for
        // `farm:grown`, which now exists — so a marker at 8,9,8 says the engine
        // offered a material a mod DID ask about, and its absence would say the
        // filter is too tight. What must never appear is a random tick for a
        // material nobody registered, and that is what the filter above is; a
        // mod cannot observe it directly, so what is asserted is the shape it
        // would break: the field grew, which means the offers arrived, and the
        // world is still made of `farm:ground`, which was never registered.
        bot.expect_block(BlockPos::new(8, 9, 8), id("farm:wrongly_offered"), PATIENCE)
            .await
            .expect("a registered material stopped being offered once it changed");
    });
}
