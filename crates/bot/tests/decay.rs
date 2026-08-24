// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A block comes apart as you dig it, and what came off is yours.
//!
//! **The headline, and it is a change in what mining IS.** A dig used to be a
//! timer with a bar over it: nothing happened for a second and a half, then a
//! whole block vanished at once, and a player who stopped halfway got nothing
//! at all for the time they spent.
//!
//! Now the same total time is divided by the sub-nodes in the block and one
//! comes off at each step, credited as it goes. Stop halfway and you are
//! holding half a block's material, standing in front of half a block.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::{ClientMessage, ServerMessage};
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(15);

/// The block dug in every test here. `y = -1` is the surface under
/// `fill_below_heightmap(0)`, which is the trap every test in this repo hits
/// once.
const TARGET: tiamot_core::BlockPos = tiamot_core::BlockPos::new(0, -1, 0);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-decay-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write_mod(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("quarry");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"quarry\"\nname = \"Quarry\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
-- Slow enough that a half-dug block is a state a test can catch rather than
-- something that happens between two polls.
local stone = game.register_block{ id = "stone", hardness = 3.0 }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), stone)
end)
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }
"#,
    )
    .expect("script");
    root
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(write_mod(name)),
        seed: Some(21),
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

/// How many distinct sub-nodes of `at` the server has removed.
fn cells_gone(bot: &Bot, at: tiamot_core::BlockPos) -> usize {
    let mut gone = std::collections::BTreeSet::new();
    for message in bot.received() {
        if let ServerMessage::BlockDelta { edit, .. } = message
            && let tiamot_core::proto::Edit::SubNode { pos, material } = edit
            && pos.block() == at
            && material == tiamot_core::MaterialId::AIR.0
        {
            gone.insert((pos.x, pos.y, pos.z));
        }
    }
    gone.len()
}

/// Total units the server says the player is carrying.
fn carried(bot: &Bot) -> u32 {
    bot.received()
        .into_iter()
        .filter_map(|message| match message {
            ServerMessage::InventoryUpdate { stacks } => {
                Some(stacks.iter().map(|stack| stack.units).sum())
            }
            _ => None,
        })
        .next_back()
        .unwrap_or(0)
}

#[test]
fn stopping_halfway_leaves_half_a_block_and_keeps_half_its_material() {
    let server = start("halfway");
    block_on(async {
        let mut bot = join(&server, "miner").await;
        let centre =
            tiamot_core::SubNodePos::new(TARGET.x * 3 + 1, TARGET.y * 3 + 1, TARGET.z * 3 + 1);

        // Dig until some of the block is gone but not all of it.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let mut next_send = tokio::time::Instant::now();
        loop {
            let gone = cells_gone(&bot, TARGET);
            if (4..20).contains(&gone) {
                break;
            }
            assert!(
                gone < 27,
                "the whole block went before the test could stop it — is the \
                 hardness high enough?"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "the block never started coming apart"
            );
            if tokio::time::Instant::now() >= next_send {
                bot.send(&ClientMessage::StartDig { target: centre })
                    .await
                    .expect("dig");
                next_send = tokio::time::Instant::now() + Duration::from_millis(300);
            }
            let _ = tokio::time::timeout(Duration::from_millis(20), bot.recv()).await;
        }

        // **Stop.** This is the moment the old behaviour gave you nothing.
        bot.send(&ClientMessage::CancelDig).await.expect("cancel");
        for _ in 0..20 {
            let _ = tokio::time::timeout(Duration::from_millis(20), bot.recv()).await;
        }

        let gone = cells_gone(&bot, TARGET);
        let held = carried(&bot);
        assert!(
            (4..27).contains(&gone),
            "the block is {gone} sub-nodes down, which is not half dug"
        );
        // Every sub-node that came off is a unit in the inventory: 27 units is
        // one block (charter rule 5), so this is the same number twice.
        assert_eq!(
            held, gone as u32,
            "{gone} sub-nodes came out of the world and {held} units arrived in \
             the inventory — a dig must conserve what it removes"
        );

        // And the block is still THERE. Half a block standing is the thing a
        // player sees, and the thing the old timer could not express.
        assert!(
            cells_gone(&bot, TARGET) < 27,
            "the block finished breaking after the dig was cancelled"
        );
    });
    assert!(server.stop());
}

#[test]
fn a_whole_dig_still_yields_exactly_one_block() {
    // Conservation across the change: coming apart in pieces must not create or
    // destroy material compared with the block that was there.
    let server = start("whole");
    block_on(async {
        let mut bot = join(&server, "miner").await;
        bot.dig_block(TARGET).await.expect("dig");

        let deadline = tokio::time::Instant::now() + PATIENCE;
        while cells_gone(&bot, TARGET) < 27 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} sub-nodes came off",
                cells_gone(&bot, TARGET)
            );
            let _ = tokio::time::timeout(Duration::from_millis(20), bot.recv()).await;
        }

        // Exactly one block's worth, no more.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let _ = tokio::time::timeout(Duration::from_millis(20), bot.recv()).await;
        }
        assert_eq!(
            carried(&bot),
            tiamot_core::UNITS_PER_BLOCK,
            "a whole block came apart into {} units, not {}",
            carried(&bot),
            tiamot_core::UNITS_PER_BLOCK
        );
    });
    assert!(server.stop());
}

#[test]
fn a_mod_hears_one_break_per_block_rather_than_one_per_sub_node() {
    // **Reported from the window: breaking a block sounded like twenty-seven
    // things breaking.** It did. `on_dig_complete` is one event about one
    // block, and a mod playing a sound from it is the obvious thing to write —
    // but the veto was being asked once per BITE, so the hook fired once per
    // sub-node and the mod made a noise each time.
    //
    // Counted by having the mod mark the world once per event, at a height
    // that says how many it has heard.
    let root = scratch("counting");
    let dir = root.join("counter");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"counter\"\nname = \"Counter\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local stone = game.register_block{ id = "stone", hardness = 1.0 }
local mark = game.register_block{ id = "mark" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), stone)
end)
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }

-- One block per event, stacked upward. Two events would build a tower.
local heard = 0
game.register_on_dig_complete(function(event)
    heard = heard + 1
    game.set_block({ x = 0, y = 10 + heard, z = 0 }, "counter:mark")
end)
"#,
    )
    .expect("script");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch("counting-world"),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(root),
        seed: Some(41),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start");

    block_on(async {
        let mut bot = join(&server, "miner").await;
        let mark = bot
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "counter:mark")
            .map(|entry| entry.id)
            .expect("the mod registers a mark");

        bot.dig_block(TARGET).await.expect("dig");

        // The first mark says the hook fired at all. Without it the rest of
        // this proves nothing.
        bot.expect_block(tiamot_core::BlockPos::new(0, 11, 0), mark, PATIENCE)
            .await
            .expect("the mod never heard the dig complete");

        // Settle, then check nothing built a tower.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let _ = tokio::time::timeout(Duration::from_millis(20), bot.recv()).await;
        }
        assert!(
            !bot.saw_block(tiamot_core::BlockPos::new(0, 12, 0), mark),
            "the mod was told twice about one block — a break sound would have \
             played twenty-seven times"
        );
    });
    assert!(server.stop());
}
