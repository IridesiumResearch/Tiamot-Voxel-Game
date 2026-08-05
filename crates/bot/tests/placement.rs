// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Placing, over a real server: the other half of the 27-unit round trip.
//!
//! [`mining`](../mining.rs) proves that breaking geometry yields units. These
//! prove that units turn back into geometry, that the arithmetic is the same in
//! both directions, and that the server — not the client — decides whether any
//! of it happens.
//!
//! # The chisel scenario is the acceptance criterion
//!
//! Task 09's [A] criterion is that sub-node brushes and spare-unit arithmetic
//! work end to end over the network. That is
//! [`chiselled_spares_place_back_as_a_partial_block`]: a real chisel with a
//! mod-registered `"subnode"` brush takes cells one at a time, and the units it
//! yields go back into the world as a `Partial` block of exactly that many
//! cells. Nothing in the engine is special-cased for any of it.
//!
//! # Every refusal is checked for its reason, not just for its absence
//!
//! A placement that does not happen and a placement whose message was lost look
//! identical from the outside. Asserting only "the block is not there" would
//! pass on a server that silently dropped every request, so each refusal test
//! also asserts that the player was told why.

use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::inventory::display;
use tiamot_core::{BlockPos, MaterialId, SubNodePos, UNITS_PER_BLOCK};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];

/// How many cells the chisel scenario takes out, from the task's test list.
const CHISELLED_CELLS: u32 = 13;

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

fn reference_mods() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// A server running the reference mods, so there are tools to dig with.
fn start(name: &str) -> ServerHandle {
    let dir = std::env::temp_dir().join("tiamot-placement").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
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

/// The centre cell of a block.
fn centre_of(pos: BlockPos) -> SubNodePos {
    SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1)
}

/// Units of one material in the bot's last inventory update.
fn held(bot: &Bot, material: u16) -> u32 {
    bot.inventory()
        .iter()
        .find(|(id, _)| *id == material)
        .map_or(0, |(_, units)| *units)
}

/// Digs a whole block with the default tool and waits for the units to land.
async fn mine_a_block(bot: &mut Bot, server: &ServerHandle, pos: BlockPos, material: u16) {
    // Seeded by the operator. A client cannot edit the world directly, and a
    // test that arranged one that way would depend on a capability that is
    // deliberately not there.
    assert!(server.seed_block(pos, material), "seed queue full");
    bot.expect_block(pos, material, Duration::from_secs(10))
        .await
        .expect("the seeded block should land");

    bot.select_tool(None).await.expect("bare hand");
    bot.start_dig(centre_of(pos)).await.expect("start dig");
    bot.await_inventory(Duration::from_secs(20))
        .await
        .expect("the block should be credited");
}

#[test]
fn placing_a_whole_block_spends_exactly_twenty_seven_units() {
    // Charter rule 5 in both directions at once: what mining credits, placing
    // charges, and the two numbers are the same.
    let server = start("whole-block");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Builder").await;
        let quarry = BlockPos::new(8, 40, 8);
        mine_a_block(&mut bot, &server, quarry, stone).await;

        let before = held(&bot, stone);
        assert_eq!(
            before, UNITS_PER_BLOCK,
            "mining one block should yield 27 units, got {before}"
        );

        let target = BlockPos::new(8, 40, 12);
        bot.place_from_inventory(centre_of(target), stone)
            .await
            .expect("place");
        // A full placement is broadcast as a whole-block edit, not as a
        // `Partial` with all 27 bits set — the geometry is identical and one
        // form is enough.
        bot.expect_block(target, stone, Duration::from_secs(10))
            .await
            .unwrap_or_else(|err| panic!("the block should appear: {err}; {:?}", bot.notices()));

        bot.await_inventory(Duration::from_secs(10))
            .await
            .expect("the charge should be reported");
        assert_eq!(
            held(&bot, stone),
            0,
            "placing a whole block should have spent all 27 units"
        );
    });

    assert!(server.stop());
}

#[test]
fn chiselled_spares_place_back_as_a_partial_block() {
    // **The Task 09 [A] criterion.** A mod-registered `"subnode"` brush takes
    // 13 cells one at a time; the 13 units that yields go back into the world
    // as a block filled with exactly 13 cells. Sub-node brushes and spare-unit
    // arithmetic, end to end, over a real protocol.
    let server = start("chisel-sculpt");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Sculptor").await;
        let quarry = BlockPos::new(8, 40, 8);
        assert!(server.seed_block(quarry, stone), "seed queue full");
        bot.expect_block(quarry, stone, Duration::from_secs(10))
            .await
            .expect("the seeded block should land");

        bot.select_tool(Some("core_tools:chisel"))
            .await
            .expect("select the chisel");

        // 13 cells of the 27, taken one at a time. The block itself survives:
        // this is a chisel, not a pick.
        let base = SubNodePos::new(quarry.x * 3, quarry.y * 3, quarry.z * 3);
        let mut taken = 0;
        'chisel: for y in 0..3 {
            for z in 0..3 {
                for x in 0..3 {
                    if taken == CHISELLED_CELLS {
                        break 'chisel;
                    }
                    bot.start_dig(SubNodePos::new(base.x + x, base.y + y, base.z + z))
                        .await
                        .expect("start dig");
                    taken += 1;
                    // Wait for THIS cell before aiming at the next: re-aiming
                    // discards progress, so firing them all off at once would
                    // dig one cell thirteen times.
                    let deadline = Duration::from_secs(30);
                    let want = taken;
                    let started = tokio::time::Instant::now();
                    while held(&bot, stone) < want {
                        assert!(
                            started.elapsed() < deadline,
                            "cell {want} never yielded; held {} units",
                            held(&bot, stone)
                        );
                        bot.await_inventory(deadline).await.expect("credit");
                    }
                }
            }
        }

        let carried = held(&bot, stone);
        assert_eq!(
            carried, CHISELLED_CELLS,
            "thirteen chiselled cells should be thirteen units"
        );
        // The display arithmetic the criterion names: 0 blocks and 13 spares.
        let (blocks, spares) = display(carried);
        assert_eq!(
            (blocks, spares),
            (0, CHISELLED_CELLS),
            "13 units should display as 0 blocks and 13 spare nodes"
        );

        // Place them back, into empty space.
        let target = BlockPos::new(8, 40, 12);
        bot.place_from_inventory(centre_of(target), stone)
            .await
            .expect("place");
        bot.expect_partial(target, stone, CHISELLED_CELLS, Duration::from_secs(10))
            .await
            .unwrap_or_else(|err| {
                panic!(
                    "the partial should appear: {err}; notices {:?}",
                    bot.notices()
                )
            });

        bot.await_inventory(Duration::from_secs(10))
            .await
            .expect("the charge should be reported");
        assert_eq!(
            held(&bot, stone),
            0,
            "placing 13 spares should have spent all 13 units"
        );
    });

    assert!(server.stop());
}

#[test]
fn placing_with_nothing_in_hand_is_refused_and_says_so() {
    // The counter-example the other tests need: if placement were free, every
    // assertion about spending units above would pass without meaning anything.
    let server = start("empty-handed");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Pauper").await;
        let target = BlockPos::new(8, 40, 8);

        bot.place_from_inventory(centre_of(target), stone)
            .await
            .expect("send");
        // Nothing arrives, and a reason does.
        let refused = bot
            .expect_block(target, stone, Duration::from_secs(3))
            .await
            .is_err();
        assert!(refused, "a block was placed by a player carrying nothing");
        assert!(
            bot.notices().iter().any(|text| text.contains("carrying")),
            "the refusal was silent; notices were {:?}",
            bot.notices()
        );
    });

    assert!(server.stop());
}

#[test]
fn placing_into_something_solid_is_refused() {
    // Placing into occupied space would have to decide what happens to what was
    // already there, and every answer either destroys material or duplicates it
    // — a hole in charter rule 5's conservation either way.
    let server = start("occupied");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Overlapper").await;
        let quarry = BlockPos::new(8, 40, 8);
        mine_a_block(&mut bot, &server, quarry, stone).await;

        // Somewhere solid: seed a block with the free Task 07 edit path, then
        // try to place into it.
        let target = BlockPos::new(8, 40, 12);
        assert!(server.seed_block(target, stone), "seed queue full");
        bot.expect_block(target, stone, Duration::from_secs(10))
            .await
            .expect("the seed should land");

        let before = held(&bot, stone);
        bot.place_from_inventory(centre_of(target), stone)
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Drain whatever arrived so the notice is in `received`.
        let _ = tokio::time::timeout(Duration::from_millis(500), bot.recv()).await;

        assert!(
            bot.notices().iter().any(|text| text.contains("already")),
            "placing into a solid block was not refused; notices {:?}",
            bot.notices()
        );
        assert_eq!(
            held(&bot, stone),
            before,
            "a refused placement still charged the player"
        );
    });

    assert!(server.stop());
}

#[test]
fn placing_inside_a_player_is_refused() {
    // The rule that stops a player sealing themselves — or anyone else — inside
    // a block. The bot places into the cell it is standing in.
    let server = start("inside-a-player");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server, "Selfsealer").await;
        let quarry = BlockPos::new(8, 40, 8);
        mine_a_block(&mut bot, &server, quarry, stone).await;
        let before = held(&bot, stone);

        // Where the server says the body is, in cells, converted to the block
        // containing it. Asking the server rather than assuming spawn: the body
        // falls to the ground before this runs.
        let position = bot
            .walk([0.0, 0.0, 0.0], 0, 2)
            .await
            .expect("a still tick, to learn where the body is");
        // `detgen::floor_to_i32`, not `f32::floor`: the determinism lint bans
        // the latter everywhere, including here, because it lowers to a libm
        // call without SSE4.1 (charter rule 4). A test is not exempt — a test
        // that computed a different cell on a different machine would be a
        // flake nobody could reproduce.
        let cell = |axis: usize| tiamot_core::detgen::floor_to_i32(position.local[axis]);
        let feet = SubNodePos::new(
            position.chunk.x * tiamot_core::CHUNK_SUBNODES as i32 + cell(0),
            position.chunk.y * tiamot_core::CHUNK_SUBNODES as i32 + cell(1),
            position.chunk.z * tiamot_core::CHUNK_SUBNODES as i32 + cell(2),
        );

        bot.place_from_inventory(feet, stone).await.expect("send");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), bot.recv()).await;

        assert!(
            bot.notices().iter().any(|text| text.contains("standing")),
            "a block was placed inside the player; notices {:?}",
            bot.notices()
        );
        assert_eq!(
            held(&bot, stone),
            before,
            "a refused placement still charged the player"
        );
    });

    assert!(server.stop());
}
