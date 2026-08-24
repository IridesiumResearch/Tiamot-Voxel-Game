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

/// The reference mods, which define the tools and the ground.
fn reference_mods() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// A server running the reference mods, so digging is possible at all.
fn start_with_mods(name: &str) -> ServerHandle {
    let dir = std::env::temp_dir().join("tiamot-mining").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(5),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
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
        enabled_mods: None,
        seed: Some(5),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
}

/// Whether the server has broadcast a whole-block removal at `pos`.
///
/// Asserting that a block SURVIVED cannot use `expect_block`, and this is the
/// mistake it exists to prevent: that helper asks "have I ever seen an edit
/// setting this to that", so the bot's own earlier `place` satisfies it and the
/// assertion can never fail. Two tests here were written that way and passed
/// against a deliberately broken server; the absence of a removal is what
/// "still there" actually means.
fn saw_removal(bot: &Bot, pos: BlockPos) -> bool {
    bot.received().into_iter().any(|message| {
        matches!(
            message,
            tiamot_core::proto::ServerMessage::BlockDelta {
                edit: tiamot_core::proto::Edit::Block { pos: at, material },
                ..
            } if at == pos && material == tiamot_core::MaterialId::AIR.0
        )
    })
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
async fn build_slab(
    bot: &mut Bot,
    server: &ServerHandle,
    origin: BlockPos,
    material: u16,
) -> Vec<BlockPos> {
    let mut placed = Vec::new();
    for dx in 0..3 {
        for dz in 0..3 {
            let pos = BlockPos::new(origin.x + dx, origin.y, origin.z + dz);
            // The operator arranges the slab. A client cannot edit the world,
            // and mining is what these tests are about — the arranging is
            // scaffolding and should not be asserting a capability of its own.
            assert!(server.seed_block(pos, material), "seed queue full");
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

/// Digs a whole block for real and waits for the server to confirm it.
///
/// The Task 07 stand-in wrote air straight into the world; this counts ticks
/// with a mod-registered tool, which is the only way a block comes out now.
/// Re-aimed until it lands: re-aiming at the same cell keeps its progress, so
/// repeating is free and it survives a message going missing.
async fn dig_whole_block(bot: &mut Bot, pos: BlockPos) {
    bot.select_tool(None).await.expect("bare hand");
    let target = SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        bot.start_dig(target).await.expect("start dig");
        if bot
            .expect_block(pos, MaterialId::AIR.0, Duration::from_secs(4))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "digging {pos:?} never completed"
        );
    }
}

/// Chisels one cell for real, with a mod-registered sub-node brush.
async fn chisel_cell(bot: &mut Bot, target: SubNodePos, expect_units: u32, stone: u16) {
    bot.select_tool(Some("core_tools:chisel"))
        .await
        .expect("select chisel");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        bot.start_dig(target).await.expect("start dig");
        let _ = bot.await_inventory(Duration::from_secs(4)).await;
        if bot.units_of(stone) >= expect_units {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "chiselling {target:?} never yielded {expect_units} units"
        );
    }
}

#[test]
fn mining_a_three_by_three_yields_exactly_nine_blocks_and_no_spares() {
    // `mine_3x3.lua` as a Rust test: nine whole blocks is 243 units, which must
    // display as exactly 9 blocks and 0 spare nodes. A yield that rounded, or
    // that counted blocks instead of units, fails here.
    let server = start_with_mods("mine-3x3");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let origin = BlockPos::new(-1, -1, -1);
        let blocks = build_slab(&mut bot, &server, origin, stone).await;

        // Placing yielded whatever was there before (air, so nothing). Start
        // counting from here.
        let before = bot.units_of(stone);

        for pos in &blocks {
            dig_whole_block(&mut bot, *pos).await;
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
    let server = start_with_mods("subnode-mining");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let block = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(block, stone), "seed queue full");
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
        for (index, (dx, dy, dz)) in cells.into_iter().enumerate() {
            let want = before + u32::try_from(index).expect("fits") + 1;
            chisel_cell(
                &mut bot,
                SubNodePos::new(base.x + dx, base.y + dy, base.z + dz),
                want,
                stone,
            )
            .await;
        }
        let units = bot.units_of(stone).saturating_sub(before);

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
    let server = start_with_mods("chisel-equals-block");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;

        // Block A: mined whole.
        let whole = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(whole, stone), "seed queue full");
        bot.expect_block(whole, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        let before_whole = bot.units_of(stone);
        dig_whole_block(&mut bot, whole).await;
        let mut whole_yield = 0;
        for _ in 0..50 {
            bot.await_inventory(Duration::from_millis(200)).await.ok();
            whole_yield = bot.units_of(stone).saturating_sub(before_whole);
            if whole_yield >= UNITS_PER_BLOCK {
                break;
            }
        }

        // Block B: chiselled away, all 27 cells.
        let chiselled = BlockPos::new(-2, -1, 0);
        assert!(server.seed_block(chiselled, stone), "seed queue full");
        bot.expect_block(chiselled, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        let before_chisel = bot.units_of(stone);

        let base = SubNodePos::new(chiselled.x * 3, chiselled.y * 3, chiselled.z * 3);
        let mut taken = 0;
        for dx in 0..3 {
            for dy in 0..3 {
                for dz in 0..3 {
                    taken += 1;
                    chisel_cell(
                        &mut bot,
                        SubNodePos::new(base.x + dx, base.y + dy, base.z + dz),
                        before_chisel + taken,
                        stone,
                    )
                    .await;
                }
            }
        }
        let chisel_yield = bot.units_of(stone).saturating_sub(before_chisel);

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
fn nothing_can_be_dug_or_built_across_the_map() {
    // The client's raycast stops at `phys::REACH`, but **a bound only the
    // client enforces is not a bound**: nothing stopped a peer naming any cell
    // in the world and mining it from spawn. Charter rule 2 puts the decision
    // on the server, so the server checks it.
    //
    // Deliberately generous — see `place::REACH_TOLERANCE`. The server checks
    // against a position several ticks older than the one the client aimed
    // from, so an exact bound would refuse legitimate actions from anyone who
    // was moving. This asserts the bound exists, not where exactly it sits.
    let server = start_with_mods("out-of-reach");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;

        let far = BlockPos::new(400, -1, 400);
        assert!(server.seed_block(far, stone), "seed queue full");

        // Aim at it and hold. Nothing should ever come of it.
        let target = SubNodePos::new(far.x * 3 + 1, far.y * 3 + 1, far.z * 3 + 1);
        for _ in 0..40 {
            bot.start_dig(target).await.expect("start dig");
            let _ = bot.await_inventory(Duration::from_millis(50)).await;
        }
        assert!(
            !saw_removal(&bot, far),
            "a block 400 blocks away was mined from spawn"
        );
        assert_eq!(bot.units_of(stone), 0, "and nothing was credited for it");
        assert!(
            bot.notices().iter().any(|text| text.contains("too far")),
            "the refusal was silent; notices {:?}",
            bot.notices()
        );

        // The counter-example, without which this passes on a server where
        // digging is broken outright: the same dig, in arm's reach, works.
        let near = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(near, stone), "seed queue full");
        bot.expect_block(near, stone, Duration::from_secs(10))
            .await
            .expect("the near seed should land");
        dig_whole_block(&mut bot, near).await;
        // The removal broadcast and the inventory update are separate messages,
        // so the credit can still be in flight when the block has gone.
        for _ in 0..50 {
            if bot.units_of(stone) > 0 {
                break;
            }
            let _ = bot.await_inventory(Duration::from_millis(200)).await;
        }
        assert!(
            bot.units_of(stone) > 0,
            "the reachable dig yielded nothing either, so this proves nothing"
        );
    });

    server.stop();
}

#[test]
fn digging_air_yields_nothing() {
    // A stack of air would be a bug that propagated into every inventory that
    // touched it.
    let server = start_with_mods("dig-air");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let empty = BlockPos::new(2, 2, 0);

        // Aiming at air and holding the button. The dig never COMPLETES —
        // the server cancels one whose target is already gone — so there is no
        // confirmation to wait for, and the assertion is about what did not
        // happen: nothing credited, and above all no stack of air.
        //
        // The old version of this test wrote air into an air block and waited
        // for the broadcast, which the direct-edit path obligingly sent. There
        // is no such path now, and the claim is the same one.
        bot.select_tool(None).await.expect("bare hand");
        let target = SubNodePos::new(empty.x * 3 + 1, empty.y * 3 + 1, empty.z * 3 + 1);
        for _ in 0..40 {
            bot.start_dig(target).await.expect("start dig");
            let _ = bot.await_inventory(Duration::from_millis(50)).await;
        }

        assert_eq!(bot.units_of(stone), 0);
        assert!(
            bot.inventory()
                .iter()
                .all(|stack| stack.material != MaterialId::AIR.0),
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
    let server = start_with_mods("per-player");
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

        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        miner
            .expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("confirmed");
        dig_whole_block(&mut miner, pos).await;

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

#[test]
fn a_dig_takes_time_and_yields_the_block_it_broke() {
    // The whole server-authoritative dig loop, end to end: the client says
    // where, the server counts ticks against the block's hardness, applies the
    // removal, computes the drop and credits it.
    //
    // The timing assertion is the load-bearing one. A server that broke the
    // block on the first `StartDig` would pass every "is it gone" check, and
    // would also let any client break every block in the world instantly —
    // which is precisely what charter rule 2 puts the decision on the server
    // to prevent.
    let server = start_with_mods("timed-dig");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        bot.expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("the block should exist before it can be dug");

        // Aim at the middle cell of the block.
        let target = tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
        let started = std::time::Instant::now();
        bot.start_dig(target).await.expect("start dig");

        bot.expect_block(pos, tiamot_core::MaterialId::AIR.0, Duration::from_secs(20))
            .await
            .expect("the block should break on its own once the ticks are counted");
        let took = started.elapsed();

        assert!(
            took >= Duration::from_millis(200),
            "the block broke in {took:?}; a server that breaks on the first message is not \
             counting anything"
        );

        // Sub-Node Contract §9: a whole block of one material yields 27 units.
        let carried = bot
            .await_inventory(Duration::from_secs(10))
            .await
            .expect("the drop should be credited");
        assert!(
            carried
                .iter()
                .any(|stack| stack.material == stone && stack.units == 27),
            "expected 27 units of stone, got {carried:?}"
        );
    });

    assert!(server.stop());
}

#[test]
fn cancelling_a_dig_leaves_the_block_alone() {
    // The counter-example to the test above: without this, a server that
    // ignored `CancelDig` and broke everything eventually would still pass.
    let server = start_with_mods("cancelled-dig");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        bot.expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("place should land");

        let target = tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
        bot.start_dig(target).await.expect("start");
        tokio::time::sleep(Duration::from_millis(150)).await;
        bot.stop_dig().await.expect("cancel");

        // Long enough that an uncancelled dig would have finished several times
        // over — the default hardness is 0.75 s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !saw_removal(&bot, pos),
            "the block went away after the dig was cancelled"
        );
    });

    assert!(server.stop());
}

#[test]
fn deleting_the_tools_mod_makes_the_world_undiggable() {
    // Task 09's acceptance criterion, and charter rule 1 as an executable
    // claim: the rules for breaking things live in `game/`, not in the engine.
    //
    // The engine knows how to COUNT a dig — ticks against hardness — and
    // nothing about what a player digs with. `core_tools` says a bare hand
    // exists and what it does. With no mods there is no tool, so a `StartDig`
    // is accepted and simply never progresses.
    //
    // The pair is the test. Without the first half this passes on a server
    // that cannot dig for some unrelated reason; without the second it passes
    // on one that cannot dig at all.
    let stone = stone();
    let target_of =
        |pos: BlockPos| tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);

    // With the tools mod: the block goes.
    let server = start_with_mods("tools-present");
    block_on(async {
        let mut bot = join(&server).await;
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        bot.expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("place should land");

        bot.start_dig(target_of(pos)).await.expect("start dig");
        bot.expect_block(pos, tiamot_core::MaterialId::AIR.0, Duration::from_secs(20))
            .await
            .expect("with a tools mod loaded, digging must work");
    });
    assert!(server.stop());

    // Without it: the same request, and the block stays.
    let server = start("tools-absent");
    block_on(async {
        let mut bot = join(&server).await;
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        bot.expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("place should land");

        bot.start_dig(target_of(pos)).await.expect("start dig");
        // Far longer than the 0.75 s default hardness would need.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !saw_removal(&bot, pos),
            "the block broke with no tools mod loaded; the engine is deciding how digging works"
        );
    });
    assert!(server.stop());
}

#[test]
fn the_chisel_takes_one_cell_where_a_hand_takes_the_block() {
    // The claim sub-nodes exist to justify, proven through the mod API rather
    // than asserted: `core_tools:chisel` registers `brush = "subnode"`, and
    // nothing in the engine is special-cased for it.
    //
    // Contract §9 fixes the arithmetic on both sides: a whole block is 27
    // units, one cell is 1.
    let server = start_with_mods("chisel-vs-hand");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;
        let pos = BlockPos::new(2, -1, 0);
        let target = tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);

        assert!(server.seed_block(pos, stone), "seed queue full");
        bot.expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("place should land");

        bot.select_tool(Some("core_tools:chisel"))
            .await
            .expect("select chisel");
        bot.start_dig(target).await.expect("start dig");

        // One cell gone, not the block: `expect_block` reads the block's first
        // cell, which the chisel did not touch, so the block is still stone.
        let carried = bot
            .await_inventory(Duration::from_secs(20))
            .await
            .expect("the cell should be credited");
        assert!(
            carried
                .iter()
                .any(|stack| stack.material == stone && stack.units == 1),
            "a chiselled cell should yield exactly 1 unit, got {carried:?}"
        );
        // Sharper than reading the block back: the broadcast says which KIND
        // of edit happened. A sub-node edit at the target and no whole-block
        // removal is exactly "one cell, not the cube".
        let edits: Vec<_> = bot
            .received()
            .into_iter()
            .filter_map(|message| match message {
                tiamot_core::proto::ServerMessage::BlockDelta { edit, .. } => Some(edit),
                _ => None,
            })
            .collect();
        assert!(
            edits.iter().any(|edit| matches!(
                edit,
                tiamot_core::proto::Edit::SubNode { pos: at, material }
                    if *at == target && *material == tiamot_core::MaterialId::AIR.0
            )),
            "no sub-node removal at the chiselled cell: {edits:?}"
        );
        assert!(
            !edits.iter().any(|edit| matches!(
                edit,
                tiamot_core::proto::Edit::Block { pos: at, material }
                    if *at == pos && *material == tiamot_core::MaterialId::AIR.0
            )),
            "the chisel removed the whole block instead of one cell: {edits:?}"
        );
    });

    assert!(server.stop());
}
