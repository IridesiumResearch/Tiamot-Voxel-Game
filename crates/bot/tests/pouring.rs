// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Pouring and scooping with the real `game/core_milk`, through the mod API.
//!
//! Separate from `fluid_multiplayer`, which pours with a synthetic mod on
//! purpose: that file is about the wire, and this one is about what the shipped
//! reference mod actually does when a player right-clicks. Charter rule 1 puts
//! the whole of that behaviour in `game/`, so it is testable only from outside.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::BlockPos;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-pouring").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(repo().join("game")),
        enabled_mods: None,
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

/// The milk block, as the server numbered it this session (charter rule 8).
async fn milk_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server should have sent a material table")
        .into_iter()
        .find(|entry| entry.name.ends_with(":milk"))
        .map(|entry| entry.id)
        .expect("the reference mods should register milk")
}

/// Drives the connection until a condition holds, or gives up.
async fn until(bot: &mut Bot, timeout: Duration, done: impl Fn(&Bot) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if done(bot) {
            return true;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.recv()).await;
    }
    done(bot)
}

/// Gives a bot milk to pour, the way every other test acquires material:
/// the world is seeded with blocks and the bot digs them out.
///
/// There is no "grant" in the engine and there should not be — a client that
/// could conjure material would be deciding something the server owns.
async fn stock_up(server: &ServerHandle, bot: &mut Bot, milk: u16, blocks: i32) {
    // Away from where the pouring happens, so a seeded block is never mistaken
    // for a poured one, and along z so the bot can stand beside each in turn.
    for i in 0..blocks {
        let at = BlockPos::new(8, 4, i);
        assert!(
            server.seed_block(at, milk),
            "the world should accept a seeded milk block"
        );
    }
    for i in 0..blocks {
        let at = BlockPos::new(8, 4, i);
        // Within reach, which the server bounds.
        bot.move_to(8.0, 0.0, i as f32 + 2.0)
            .await
            .expect("walk to the seeded block");
        bot.dig_block(at).await.expect("dig the seeded milk");
    }
    assert!(
        bot.units_of(milk) >= tiamot_core::UNITS_PER_BLOCK,
        "digging seeded milk credited none of it, so there is nothing to pour"
    );
}

/// Carves a known basin: a solid floor with air above it.
///
/// The world is generated, so the block a test names may be inside a hill or
/// over a hole — and milk that cannot spread measures the terrain rather than
/// the mod. Seeding both layers makes the fixture the test's own.
///
/// Air is material 0 (charter rule 8), which is how the roof comes off.
fn carve_basin(
    server: &ServerHandle,
    floor: u16,
    y: i32,
    xs: std::ops::RangeInclusive<i32>,
    z: i32,
) {
    for x in xs {
        for dz in -1..=1 {
            assert!(
                server.seed_block(BlockPos::new(x, y - 1, z + dz), floor),
                "the world should accept a seeded floor"
            );
            // Two blocks of headroom, so nothing overhead drips into the basin.
            for dy in 0..=1 {
                assert!(
                    server.seed_block(BlockPos::new(x, y + dy, z + dz), 0),
                    "the world should accept seeded air"
                );
            }
        }
    }
}

/// A material the world can stand on, as the server numbered it this session.
async fn floor_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server should have sent a material table")
        .into_iter()
        // `core_blocks:white` is the reference set's plain solid block; there
        // is no "stone" in `game/`, which is the point of the reference mods
        // being fixtures rather than a game.
        .find(|entry| entry.name.ends_with(":white"))
        .map(|entry| entry.id)
        .expect("the reference mods should register a plain solid block")
}

/// Pours at a block, the way a right-click does.
///
/// `Bot::place` is the wrong primitive here and the reason is the whole point
/// of this file: `core_milk` CANCELS the terrain write, because leaving the
/// block behind would seal the milk inside solid stone (Sub-Node Contract §4).
/// So no block ever appears and `place` waits out its patience for one. This
/// sends the click and lets the assertions watch the fluid layer instead.
async fn pour_at(bot: &mut Bot, pos: BlockPos, milk: u16) {
    let centre = tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
    bot.place_from_inventory(centre, milk)
        .await
        .expect("the pour should reach the server");
}

/// Walls the four sides of a block so conserved milk stays in it.
///
/// **Sub-Node Contract §4 is why this exists.** Milk is conserved and levels
/// itself, so twenty-seven cells poured onto an open floor are gone from the
/// block you poured into within a couple of fluid ticks — spread over its
/// neighbours, which is correct and useless to a test about one block.
fn wall_in(server: &ServerHandle, floor: u16, at: BlockPos) {
    for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        assert!(
            server.seed_block(BlockPos::new(at.x + dx, at.y, at.z + dz), floor),
            "the world should accept a seeded wall"
        );
    }
}

#[test]
fn a_pour_and_a_scoop_give_back_exactly_what_was_carried() {
    // **The conservation round trip, through the mod API and nothing else.**
    // Charter rule 15 wants conservation asserted on simulation invariants; this
    // is the same invariant seen from where a player stands, which is the only
    // place it can go wrong in a way somebody notices.
    //
    // Under the old model this test could not have been written: a bucket
    // created a source out of nothing and scooping destroyed one, so units in
    // and units out had no relationship at all.
    let server = start("pour-and-scoop");
    block_on(async {
        let mut bot = join(&server, "Pourer").await;
        let milk = milk_id(&bot).await;
        // **Two blocks' worth, and the second one is not slack.** `core_milk`
        // pours the material a player is HOLDING, so somebody who pours their
        // last drop has nothing left to click with and cannot scoop it back —
        // see the note in `game/core_milk/init.lua`. A test that carried one
        // bucket would be unable to exercise the second half of its own round
        // trip.
        stock_up(&server, &mut bot, milk, 2).await;
        let floor = floor_id(&bot).await;
        let ground = BlockPos::new(2, 4, 2);
        carve_basin(&server, floor, ground.y, 1..=4, ground.z);
        wall_in(&server, floor, ground);
        bot.sleep_ticks(4).await;

        let carried = bot.units_of(milk);
        assert_eq!(
            carried,
            2 * tiamot_core::UNITS_PER_BLOCK,
            "the fixture should start with exactly two blocks' worth"
        );

        bot.move_to(2.0, 0.0, 4.0).await.expect("walk to the pour");
        pour_at(&mut bot, ground, milk).await;

        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| {
                bot.fluid_at(ground).volume() == tiamot_core::UNITS_PER_BLOCK
            })
            .await,
            "a whole bucket poured into a walled block should hold all of it; it holds {}",
            bot.fluid_at(ground).volume()
        );
        // **Waited for, not asserted straight away.** The fluid layer and the
        // inventory are two messages, and the pour arriving says nothing about
        // whether the debit has. Asserting here read the units the client had
        // before it was told.
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot.units_of(milk)
                == carried - tiamot_core::UNITS_PER_BLOCK)
            .await,
            "the pour was not charged a bucket: {} units left of {carried}",
            bot.units_of(milk)
        );

        // And back out again.
        pour_at(&mut bot, ground, milk).await;
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot
                .fluid_at(ground)
                .is_empty())
            .await,
            "clicking a block that already holds milk should scoop it"
        );
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot.units_of(milk)
                == carried)
            .await,
            "what came back out is not what went in: {} units against {carried} — milk was \
             created or destroyed by a round trip that has no sink in it",
            bot.units_of(milk)
        );

        bot.disconnect().await;
    });
    server.stop();
}

/// Walls a two-block trough, so one bucket settles as two half-full blocks.
fn trough(server: &ServerHandle, floor: u16, left: BlockPos) {
    let right = BlockPos::new(left.x + 1, left.y, left.z);
    for at in [left, right] {
        for dz in [-1, 1] {
            assert!(
                server.seed_block(BlockPos::new(at.x, at.y, at.z + dz), floor),
                "the world should accept a seeded wall"
            );
        }
    }
    for x in [left.x - 1, right.x + 1] {
        assert!(
            server.seed_block(BlockPos::new(x, left.y, left.z), floor),
            "the world should accept a seeded end wall"
        );
    }
}

#[test]
fn scooping_a_shallow_puddle_gives_back_a_partial_bucket() {
    // **The decision this protects**: a bucket is a MEASUREMENT, not a switch.
    // Scooping half a puddle gives half a bucket back, and it costs no new
    // concept to say so — milk in an inventory is units of a material like
    // anything else (charter rule 5), so "half a bucket" is just fewer units.
    //
    // The rejected alternative was Minecraft's, where a bucket tops itself up
    // out of neighbouring blocks until it is full. That makes scooping one
    // block drain water the player never pointed at.
    let server = start("partial-bucket");
    block_on(async {
        let mut bot = join(&server, "Pourer").await;
        let milk = milk_id(&bot).await;
        stock_up(&server, &mut bot, milk, 2).await;
        let floor = floor_id(&bot).await;
        let ground = BlockPos::new(2, 4, 2);
        carve_basin(&server, floor, ground.y, 1..=4, ground.z);
        // **A sealed trough of exactly two blocks.** One bucket levels itself
        // across them and then STOPS, which is what makes the amount readable:
        // an open puddle is still moving when the test samples it, and the
        // first version of this test scooped a block that had lost a cell
        // between the reading and the click.
        trough(&server, floor, ground);
        bot.sleep_ticks(4).await;

        bot.move_to(2.0, 0.0, 4.0).await.expect("walk to the pour");
        pour_at(&mut bot, ground, milk).await;

        // Levelled and settled: half a bucket each, give or take the odd cell
        // that cannot be split.
        let neighbour = BlockPos::new(ground.x + 1, ground.y, ground.z);
        assert!(
            until(&mut bot, Duration::from_secs(20), |bot| {
                let here = bot.fluid_at(ground).volume();
                let there = bot.fluid_at(neighbour).volume();
                here + there == tiamot_core::UNITS_PER_BLOCK && here.abs_diff(there) <= 1
            })
            .await,
            "the bucket never levelled across the trough: {} and {}",
            bot.fluid_at(ground).volume(),
            bot.fluid_at(neighbour).volume()
        );
        let there = bot.fluid_at(ground).volume();
        assert!(
            there > 0 && there < tiamot_core::UNITS_PER_BLOCK,
            "the block holds {there} cells, which is not a partial bucket"
        );

        let before = bot.units_of(milk);
        pour_at(&mut bot, ground, milk).await;

        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot.units_of(milk)
                == before + there)
            .await,
            "a scoop of {there} cells credited {} units rather than {there} — a partial bucket \
             was either rounded up to a whole one or refused",
            bot.units_of(milk).saturating_sub(before)
        );

        bot.disconnect().await;
    });
    server.stop();
}
