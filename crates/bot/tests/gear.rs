// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Items, dropping and picking up, against the real reference mods.
//!
//! # What is under test is the API, not the mod
//!
//! `game/core_gear/` is a fixture (see `game/README.md`). Its whole job is to
//! be built out of the public surface — `register_item`, `register_view`,
//! `register_action`, `game.held`, `game.player_entity`, `game.take`,
//! `spawn_entity{ item = }`, `entities_in_radius` and `game.give` — so that
//! "an item that is not a block" and "a thing lying on the floor" are things a
//! third-party mod can build. Anything here that needed engine support a mod
//! cannot reach would be a bug in the API rather than something to work around
//! in the mod (charter rule 1).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-gear").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
        .join("game")
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(4),
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

/// The numeric id of a material by its string id, which is per-session.
fn material_of(bot: &Bot, name: &str) -> u16 {
    bot.material_table()
        .expect("a material table")
        .into_iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("`{name}` is not in the material table"))
        .id
}

#[test]
fn an_item_reaches_a_client_marked_as_something_it_cannot_build_with() {
    // The whole of what makes an item an item. It is in the material table
    // like everything else a player can carry — same id space, same atlas, same
    // slots — and the one bit that differs says it may not be placed.
    let server = start("item-table");
    block_on(async {
        let bot = join(&server, "Carrier").await;

        let table = bot.material_table().expect("a material table");
        let sword = table
            .iter()
            .find(|entry| entry.name == "core_gear:sword")
            .expect("the reference item is in the material table");
        assert!(
            !sword.placeable,
            "an item reached the client as something you could build with"
        );
        // **And it has a picture.** An item with no texture reaches a client as
        // a material with no tile, which draws as the missing-texture chequer —
        // reported from the window as a pink and black cube, on the first
        // version of this mod, which registered no texture at all. The engine
        // cannot invent a picture of a sword.
        assert!(
            sword.texture.is_some(),
            "the item has no texture, so a player sees the missing-texture chequer"
        );

        // The counter-example, so the assertion above is not vacuous: a block
        // registered by another mod is in the same table and IS placeable.
        let stone = table
            .iter()
            .find(|entry| entry.name.starts_with("core:"))
            .expect("the reference blocks are in the material table");
        assert!(
            stone.placeable,
            "`{}` came back unplaceable, so `placeable` says nothing",
            stone.name
        );

        bot.disconnect().await;
    });
    assert!(server.stop());
}

#[test]
fn a_player_arrives_holding_an_item_and_cannot_place_it() {
    let server = start("item-place");
    block_on(async {
        let mut bot = join(&server, "Carrier").await;
        let sword = material_of(&bot, "core_gear:sword");

        // Asked for, not handed out on join: a fixture that changed every
        // player's starting inventory is a fixture other tests have to know
        // about, and three of them broke when it did.
        bot.chat("gear").await.expect("ask");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.inventory().iter().any(|stack| stack.material == sword) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the reference item never arrived: {:?}",
                bot.inventory()
            );
            bot.recv().await.expect("recv");
        }

        // **Refused, and the player is told.** The server owns the decision
        // (charter rule 2); the client refuses too, but a bot is not a client
        // and this is the half that matters.
        let before: u32 = bot.inventory().iter().map(|stack| stack.units).sum();
        let _ = bot.place(tiamot_core::BlockPos::new(0, 0, 0), sword).await;

        // Driven to a condition: what matters is that the units never move,
        // because a refusal that spent the item would be worse than one that
        // did nothing.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let now: u32 = bot.inventory().iter().map(|stack| stack.units).sum();
            assert_eq!(now, before, "placing an item spent it");
            let _ = tokio::time::timeout(Duration::from_millis(100), bot.recv()).await;
        }

        bot.disconnect().await;
    });
    assert!(server.stop());
}

#[test]
fn what_a_player_drops_lands_as_an_entity_and_comes_back() {
    // The round trip a dropped item is: out of the inventory, onto the floor
    // as an entity that IS a stack, and back into the inventory when the mod
    // decides somebody may have it. Every step of it is Lua; the engine's part
    // is that the stack survives each hop and that the thing on the floor can
    // be drawn at all.
    let server = start("drop-round-trip");
    block_on(async {
        let mut bot = join(&server, "Dropper").await;
        let sword = material_of(&bot, "core_gear:sword");

        bot.chat("gear").await.expect("ask");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.inventory().iter().any(|stack| stack.material == sword) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the reference item never arrived"
            );
            bot.recv().await.expect("recv");
        }

        // Slot zero is where a client starts, and the mod drops what is HELD —
        // so say so explicitly rather than relying on the default.
        bot.select_slot(0).await.expect("select");
        bot.action("core_gear:drop", true).await.expect("press");

        // **It LEAVES the inventory**, which is the half a first version of
        // this test did not check — and without it the whole thing passed with
        // `game.take` deleted from the mod, because a stack that was never
        // taken is still there to be found at the end.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if !bot.inventory().iter().any(|stack| stack.material == sword) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "dropping it did not take it out of the inventory: {:?}",
                bot.inventory()
            );
            bot.recv().await.expect("recv");
        }

        // And appears on the floor.
        let dropped = bot
            .expect_entity(
                |entity| entity.item.is_some_and(|stack| stack.material == sword),
                PATIENCE,
            )
            .await
            .expect("the dropped stack never appeared as an entity");
        assert!(
            dropped.model.is_none(),
            "a dropped stack is drawn as its stack, not as a rig"
        );
        assert!(
            dropped.collider.is_some(),
            "a dropped stack with no box would hang in the air"
        );

        // And the mod picks it back up — once its settling time is past and
        // somebody walks over it.
        //
        // **Walked TO, using the position on the wire, rather than oscillating
        // and hoping.** A thrown stack lands a couple of blocks away on
        // purpose, so a test that stood still would test a drop that failed to
        // go anywhere — but the first version of this walked blindly forward
        // and back along z, which only works while the item happens to land on
        // that line. It went red on the slowest CI runner and nowhere else,
        // which is what a test with an unstated assumption about timing looks
        // like: the bot was still settling onto the ground when it threw.
        //
        // The entity stream says where the thing IS. Re-read every leg, because
        // a stack that has not finished falling is still moving.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.inventory().iter().any(|stack| stack.material == sword) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the dropped stack was never picked back up"
            );
            let at = bot
                .entities()
                .into_values()
                .find(|entity| entity.item.is_some_and(|stack| stack.material == sword))
                .map(|entity| {
                    let cells =
                        f32::from(u16::try_from(tiamot_core::SUBNODES_PER_AXIS).unwrap_or(3));
                    let span = tiamot_core::CHUNK_BLOCKS as f32;
                    [
                        entity.chunk.x as f32 * span + entity.local[0] / cells,
                        entity.chunk.z as f32 * span + entity.local[2] / cells,
                    ]
                });
            match at {
                Some([x, z]) => {
                    let _ = bot.move_to(x, 0.0, z).await;
                    // Standing exactly on it is not walking over it: the mod
                    // picks up on proximity each tick, so give the server a
                    // few to notice.
                    bot.sleep_ticks(4).await;
                }
                // Not on screen this instant — it may not have spawned into
                // this client's view yet. Keep the connection turning.
                None => {
                    bot.recv().await.expect("recv");
                }
            }
        }

        bot.disconnect().await;
    });
    assert!(server.stop());
}

#[test]
fn a_registered_view_reaches_the_player_at_the_size_it_asked_for() {
    // Somewhere other than the backpack for a stack to sit. What the slots
    // MEAN is the mod's; that they exist and are four is the engine's.
    let server = start("worn-view");
    block_on(async {
        let mut bot = join(&server, "Wearer").await;

        let deadline = tokio::time::Instant::now() + PATIENCE;
        let worn = loop {
            if let Some(view) = bot.view("core_gear:worn") {
                break view;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the registered view never reached the client"
            );
            bot.recv().await.expect("recv");
        };
        assert_eq!(worn.len(), 4, "the view is not the size it registered");

        // And the mod can draw it: a grid over a view a MOD registered, on a
        // vanilla client, with no code pushed. That is the whole loop —
        // register a place, put a screen over it, and the engine moves stacks
        // between it and the backpack without knowing what "worn" means.
        bot.action("core_gear:gear", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let tree = loop {
            if let Some((_, tree)) = bot
                .dialogs()
                .into_iter()
                .rfind(|(form, _)| form == "core_gear:worn")
            {
                break tree;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the worn screen never opened"
            );
            bot.recv().await.expect("recv");
        };
        assert!(
            tree.nodes.iter().any(|node| matches!(
                &node.widget,
                tiamot_core::ui::Widget::ItemGrid { view, .. } if view == "core_gear:worn"
            )),
            "the screen does not show the view the mod registered"
        );

        bot.disconnect().await;
    });
    assert!(server.stop());
}
