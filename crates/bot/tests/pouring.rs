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

#[test]
fn a_pour_into_flowing_milk_makes_a_second_source_rather_than_scooping_it() {
    // **Reported from a running game**: "I do not seem to be able to place a
    // water source inside flowing water. I should be able to right click on the
    // block behind the flowing water and place another source right inside the
    // current puddle, next to the original water source."
    //
    // The engine was never the obstacle — its placement check is on TERRAIN
    // sub-nodes and fluid lives in its own layer, so the click reached the mod
    // every time. `core_milk` was the obstacle: it scooped whenever the block
    // held anything at all, so pouring into a spreading puddle emptied that one
    // block, which then refilled from the original source. The click looked
    // like it did nothing.
    //
    // Flow is milk that is only passing through. A bucket poured into it should
    // leave a second spring, which is what widening a pool requires.
    let server = start("pour-into-flow");
    block_on(async {
        let mut bot = join(&server, "Pourer").await;
        let milk = milk_id(&bot).await;
        stock_up(&server, &mut bot, milk, 2).await;
        let floor = floor_id(&bot).await;
        let ground = BlockPos::new(2, 4, 2);
        carve_basin(&server, floor, ground.y, 1..=4, ground.z);
        // Let the seeds land before pouring into them.
        bot.sleep_ticks(4).await;

        bot.move_to(2.0, 0.0, 4.0).await.expect("walk to the pour");
        pour_at(&mut bot, ground, milk).await;
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot
                .fluid_at(ground)
                .is_source())
            .await,
            "the first pour did not leave a source"
        );

        // Wait for it to spread, and take whichever neighbour it actually
        // reached rather than guessing one: the floor under a generated world
        // is not flat, and a test that names a block the milk had no reason to
        // enter measures the terrain instead of the mod.
        let neighbours = [
            BlockPos::new(ground.x + 1, ground.y, ground.z),
            BlockPos::new(ground.x - 1, ground.y, ground.z),
            BlockPos::new(ground.x, ground.y, ground.z + 1),
            BlockPos::new(ground.x, ground.y, ground.z - 1),
        ];
        let flowing = |bot: &Bot| {
            neighbours.into_iter().find(|at| {
                let there = bot.fluid_at(*at);
                !there.is_empty() && !there.is_source()
            })
        };
        assert!(
            until(&mut bot, Duration::from_secs(20), |bot| flowing(bot)
                .is_some())
            .await,
            "the milk never spread to any neighbour, so there is no flow to pour into"
        );
        let spread = flowing(&bot).expect("a flowing neighbour");

        // **The reported click.** Pour into the flow.
        pour_at(&mut bot, spread, milk).await;
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot
                .fluid_at(spread)
                .is_source())
            .await,
            "pouring into flowing milk did not leave a source — it was scooped \
             instead, which is the reported bug: the puddle refills from the \
             original source and the click appears to do nothing"
        );

        // And the original is untouched: a second spring beside the first, not
        // one moved.
        assert!(
            bot.fluid_at(ground).is_source(),
            "the second pour disturbed the first source"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_pour_onto_a_source_still_scoops_it() {
    // The other half, which must keep working: a source is what a bucket takes
    // back. Clearing it is what makes the rest drain, and being able to watch
    // that happen is the point of the mechanism.
    let server = start("scoop-a-source");
    block_on(async {
        let mut bot = join(&server, "Scooper").await;
        let milk = milk_id(&bot).await;
        stock_up(&server, &mut bot, milk, 2).await;
        let at = BlockPos::new(2, 4, 2);

        bot.move_to(2.0, 0.0, 4.0).await.expect("walk to the pour");
        pour_at(&mut bot, at, milk).await;
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot
                .fluid_at(at)
                .is_source())
            .await,
            "the pour did not leave a source"
        );

        pour_at(&mut bot, at, milk).await;
        assert!(
            until(&mut bot, Duration::from_secs(15), |bot| bot
                .fluid_at(at)
                .is_empty())
            .await,
            "pouring onto a source did not take it back"
        );

        bot.disconnect().await;
    });
    server.stop();
}
