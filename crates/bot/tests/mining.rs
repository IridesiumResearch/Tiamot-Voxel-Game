// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Mining, over a real server: the end-to-end proof of the 27-unit design.
//!
//! Charter rule 5: 1 block = 27 units, and the display is `units / 27` blocks
//! plus `units % 27` nodes. Every other test of that arithmetic is a unit test
//! against a pure function; these drive it through a real client, a real
//! protocol, and a real world.
//!
//! # What is and is not covered here
//!
//! Digging yields units into a server-authoritative inventory. **Placing does
//! not yet consume from it** — Task 09 owns the rest of the player-interaction
//! loop. What exists is the half these tests need to be a real proof rather
//! than a vacuous one.

use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::inventory::display;
use tiamot_core::{BlockPos, MaterialId, SubNodePos, UNITS_PER_BLOCK};
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

fn start(name: &str) -> ServerHandle {
    let dir = std::env::temp_dir().join("tiamot-mining").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: None,
        seed: Some(5),
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
    bot.join("Miner").await.expect("join");
    bot
}

/// Fills a 3x3x1 slab so there is something to mine, and waits for it to land.
async fn build_slab(bot: &mut Bot, origin: BlockPos, material: u16) -> Vec<BlockPos> {
    let mut placed = Vec::new();
    for dx in 0..3 {
        for dz in 0..3 {
            let pos = BlockPos::new(origin.x + dx, origin.y, origin.z + dz);
            bot.place(pos, material).await.expect("place");
            placed.push(pos);
        }
    }
    for pos in &placed {
        bot.expect_block(*pos, material, Duration::from_secs(10))
            .await
            .expect("placement should be confirmed");
    }
    placed
}

#[test]
fn mining_a_three_by_three_yields_exactly_nine_blocks_and_no_spares() {
    // `mine_3x3.lua` as a Rust test: nine whole blocks is 243 units, which must
    // display as exactly 9 blocks and 0 spare nodes. A yield that rounded, or
    // that counted blocks instead of units, fails here.
    let server = start("mine-3x3");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let origin = BlockPos::new(4, 6, 4);
        let blocks = build_slab(&mut bot, origin, stone).await;

        // Placing yielded whatever was there before (air, so nothing). Start
        // counting from here.
        let before = bot.units_of(stone);

        for pos in &blocks {
            bot.dig_block(*pos).await.expect("dig");
        }
        for pos in &blocks {
            bot.expect_block(*pos, MaterialId::AIR.0, Duration::from_secs(10))
                .await
                .expect("the dig should be confirmed");
        }

        // Let the last inventory update arrive.
        let mut units = 0;
        for _ in 0..50 {
            bot.await_inventory(Duration::from_millis(200)).await.ok();
            units = bot.units_of(stone).saturating_sub(before);
            if units >= 9 * UNITS_PER_BLOCK {
                break;
            }
        }

        assert_eq!(
            units,
            9 * UNITS_PER_BLOCK,
            "nine blocks should be {} units, got {units}",
            9 * UNITS_PER_BLOCK
        );
        assert_eq!(
            display(units),
            (9, 0),
            "nine blocks must display as 9 blocks and 0 spare nodes"
        );

        bot.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn mining_single_subnodes_yields_one_unit_each_and_the_spares_add_up() {
    // `subnode_mining.lua` as a Rust test, and the half that makes the design
    // fair: chiselling must yield ONE unit per cell. If a sub-node dig yielded
    // a whole block, a player could mine 27 blocks' worth by taking 27 corners.
    let server = start("subnode-mining");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let block = BlockPos::new(8, 6, 8);
        bot.place(block, stone).await.expect("place");
        bot.expect_block(block, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");

        let before = bot.units_of(stone);

        // Chisel five cells out of the block's 27. Walking one axis would run
        // off the end after three: a block is 3x3x3 sub-nodes, so `base.x + 3`
        // is the NEXT block, which is air. An earlier version of this test did
        // exactly that and got 3 units instead of 5 — the engine was right and
        // the test was wrong, which is a good sign for the arithmetic.
        let base = SubNodePos::new(block.x * 3, block.y * 3, block.z * 3);
        let cells = [(0, 0, 0), (1, 0, 0), (2, 0, 0), (0, 1, 0), (0, 0, 1)];
        for (dx, dy, dz) in cells {
            bot.dig_subnode(SubNodePos::new(base.x + dx, base.y + dy, base.z + dz))
                .await
                .expect("chisel");
        }

        let mut units = 0;
        for _ in 0..50 {
            bot.await_inventory(Duration::from_millis(200)).await.ok();
            units = bot.units_of(stone).saturating_sub(before);
            if units >= 5 {
                break;
            }
        }

        assert_eq!(units, 5, "five cells is five units, got {units}");
        assert_eq!(
            display(units),
            (0, 5),
            "five units must display as 0 blocks and 5 spare nodes"
        );

        bot.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn twenty_seven_chiselled_nodes_equal_one_mined_block() {
    // The arithmetic that ties the two halves together, over the wire. Taking a
    // block apart one cell at a time must yield exactly what breaking it whole
    // does — otherwise one of the two paths is cheating.
    let server = start("chisel-equals-block");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;

        // Block A: mined whole.
        let whole = BlockPos::new(12, 6, 12);
        bot.place(whole, stone).await.expect("place");
        bot.expect_block(whole, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        let before_whole = bot.units_of(stone);
        bot.dig_block(whole).await.expect("dig");
        bot.expect_block(whole, MaterialId::AIR.0, Duration::from_secs(10))
            .await
            .expect("confirmed");

        let mut whole_yield = 0;
        for _ in 0..50 {
            bot.await_inventory(Duration::from_millis(200)).await.ok();
            whole_yield = bot.units_of(stone).saturating_sub(before_whole);
            if whole_yield >= UNITS_PER_BLOCK {
                break;
            }
        }

        // Block B: chiselled away, all 27 cells.
        let chiselled = BlockPos::new(14, 6, 14);
        bot.place(chiselled, stone).await.expect("place");
        bot.expect_block(chiselled, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        let before_chisel = bot.units_of(stone);

        let base = SubNodePos::new(chiselled.x * 3, chiselled.y * 3, chiselled.z * 3);
        for dx in 0..3 {
            for dy in 0..3 {
                for dz in 0..3 {
                    bot.dig_subnode(SubNodePos::new(base.x + dx, base.y + dy, base.z + dz))
                        .await
                        .expect("chisel");
                }
            }
        }

        let mut chisel_yield = 0;
        for _ in 0..80 {
            bot.await_inventory(Duration::from_millis(200)).await.ok();
            chisel_yield = bot.units_of(stone).saturating_sub(before_chisel);
            if chisel_yield >= UNITS_PER_BLOCK {
                break;
            }
        }

        assert_eq!(whole_yield, UNITS_PER_BLOCK, "a whole block is 27 units");
        assert_eq!(
            chisel_yield, whole_yield,
            "27 chiselled cells must yield exactly what breaking the block does"
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn digging_air_yields_nothing() {
    // A stack of air would be a bug that propagated into every inventory that
    // touched it.
    let server = start("dig-air");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let empty = BlockPos::new(20, 30, 20);

        bot.dig_block(empty).await.expect("dig");
        bot.expect_block(empty, MaterialId::AIR.0, Duration::from_secs(10))
            .await
            .expect("confirmed");
        bot.await_inventory(Duration::from_millis(500)).await.ok();

        assert_eq!(bot.units_of(stone), 0);
        assert!(
            bot.inventory()
                .iter()
                .all(|(id, _)| *id != MaterialId::AIR.0),
            "air must never appear in an inventory: {:?}",
            bot.inventory()
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn one_players_mining_does_not_credit_another() {
    // Inventories are per-identity. A yield credited to whoever happened to be
    // connected would be worse than no inventory at all.
    let server = start("per-player");
    let stone = stone();

    block_on(async {
        let mut miner = join(&server).await;
        let mut bystander = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bystander.join("Bystander").await.expect("join");

        let pos = BlockPos::new(30, 6, 30);
        miner.place(pos, stone).await.expect("place");
        miner
            .expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        miner.dig_block(pos).await.expect("dig");
        miner
            .expect_block(pos, MaterialId::AIR.0, Duration::from_secs(10))
            .await
            .expect("confirmed");

        for _ in 0..50 {
            miner.await_inventory(Duration::from_millis(200)).await.ok();
            if miner.units_of(stone) >= UNITS_PER_BLOCK {
                break;
            }
        }
        assert!(
            miner.units_of(stone) >= UNITS_PER_BLOCK,
            "the miner gets it"
        );

        bystander
            .await_inventory(Duration::from_millis(500))
            .await
            .ok();
        assert_eq!(
            bystander.units_of(stone),
            0,
            "a bystander must not be credited for someone else's mining"
        );

        miner.disconnect().await;
        bystander.disconnect().await;
    });

    server.stop();
}
