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
    start_at(scratch(name))
}

/// Starts a server on a world directory that may already exist.
///
/// **Separate from [`start`] because that one WIPES.** A restart test needs the
/// second server to find what the first one left, and a helper that clears the
/// directory would quietly make it a test of two fresh worlds.
fn start_at(world_path: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path,
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
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
    join_as(server, name, Identity::generate().expect("identity")).await
}

/// The same, under an identity the caller keeps.
///
/// **Needed the moment a test reconnects to the same world.** A display name is
/// a per-server claim bound to a UUID on first join (charter rule 13), so a
/// second session under a fresh key is a different person asking for a name
/// that is taken — which is the rule working, not a bug to route around.
async fn join_as(server: &ServerHandle, name: &str, identity: Identity) -> Bot {
    let mut bot = Bot::connect(server.local_addr(), identity, server.cert_fingerprint())
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
                |entity| {
                    entity
                        .item
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                },
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
                .find(|entity| {
                    entity
                        .item
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                })
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

#[test]
fn a_dropped_stack_can_still_be_picked_up_after_the_world_is_reopened() {
    // **Reported from the window**: "dropped stacks laying on the ground look
    // good but I am not able to pick them back up upon closing and reopening a
    // world."
    //
    // The entity survived — entities are persisted — and the MOD's memory of it
    // did not. `core_gear` kept the settling window in a Lua table, and its
    // tick walked that table, so after a restart nothing ever looked at the
    // item again: it lay there, drawn correctly, inert.
    //
    // The fix is to search outward from PLAYERS rather than from what the mod
    // remembers, which is also the only way the mod can meet an item it did not
    // throw.
    let world = scratch("reload-pickup");
    // **One identity across both sessions**, rebuilt from its seed, because the
    // player coming back is the same player. A fresh key would be somebody else
    // asking for a name that is already claimed — which is charter rule 13
    // working, not something to route around.
    let seed = Identity::generate().expect("identity").seed();
    let sword;

    // First session: get a sword, drop it, and leave it lying there.
    {
        let server = start_at(world.clone());
        sword = block_on(async {
            let mut bot = join_as(&server, "Dropper", Identity::from_seed(&seed)).await;
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

            bot.select_slot(0).await.expect("select");
            bot.action("core_gear:drop", true).await.expect("press");

            // On the ground, and out of the inventory, before the world closes.
            bot.expect_entity(
                |entity| {
                    entity
                        .item
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                },
                PATIENCE,
            )
            .await
            .expect("the dropped stack never appeared as an entity");
            let deadline = tokio::time::Instant::now() + PATIENCE;
            loop {
                if !bot.inventory().iter().any(|stack| stack.material == sword) {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "dropping it did not take it out of the inventory"
                );
                bot.recv().await.expect("recv");
            }

            // **Walk away before closing the world.** The settling window is
            // three seconds and the throw lands a couple of blocks off, so a
            // player who stands there long enough simply picks it back up —
            // which is the mod working, and leaves this test with nothing to
            // reopen. It failed exactly that way under a loaded test run,
            // where the waiting above takes longer than the window.
            for _ in 0..40 {
                let _ = bot.walk([0.0, 0.0, -1.0], 0, 4).await;
            }
            let still_dropped = bot.inventory().iter().all(|stack| stack.material != sword);
            assert!(
                still_dropped,
                "the sword was picked back up before the world closed, so the restart proves \
                 nothing"
            );

            bot.disconnect().await;
            sword
        });
        server.stop();
    }

    // Second session: the same world, a fresh server, a fresh mod VM.
    let server = start_at(world);
    block_on(async {
        let mut bot = join_as(&server, "Dropper", Identity::from_seed(&seed)).await;

        // It is still there.
        let dropped = bot
            .expect_entity(
                |entity| {
                    entity
                        .item
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                },
                PATIENCE,
            )
            .await
            .expect("the dropped stack did not survive the restart");

        // And it can be picked up, which is what was broken. Walk to where it
        // actually is rather than guessing.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.inventory().iter().any(|stack| stack.material == sword) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the stack survived the restart but could not be picked up: the mod never \
                 adopted an item it did not remember throwing"
            );
            let at = bot
                .entities()
                .into_values()
                .find(|entity| {
                    entity
                        .item
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                })
                .unwrap_or_else(|| dropped.clone());
            let cells = f32::from(u16::try_from(tiamot_core::SUBNODES_PER_AXIS).unwrap_or(3));
            let span = tiamot_core::CHUNK_BLOCKS as f32;
            let _ = bot
                .move_to(
                    at.chunk.x as f32 * span + at.local[0] / cells,
                    0.0,
                    at.chunk.z as f32 * span + at.local[2] / cells,
                )
                .await;
            bot.sleep_ticks(4).await;
        }

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn what_a_player_carries_survives_the_world_closing() {
    // **Measured before it was fixed**: a bot got a sword, disconnected, the
    // server restarted, and it rejoined with an empty inventory.
    //
    //     BEFORE: [StackDef { material: 9, units: 27, shape: 0 }]
    //     AFTER:  []
    //
    // `WorldDb::save_player` and `load_player` existed in the persistence layer
    // and nothing in the server called them — inventories lived in memory on
    // the endpoint and were rebuilt empty at join. In singleplayer, quitting to
    // the menu stops the server, so everything a player carried went with it
    // every session.
    let world = scratch("inventory-restart");
    let seed = Identity::generate().expect("identity").seed();
    let sword;
    let before;

    {
        let server = start_at(world.clone());
        (sword, before) = block_on(async {
            let mut bot = join_as(&server, "Keeper", Identity::from_seed(&seed)).await;
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
            let before = bot.units_of(sword);
            assert!(before > 0, "nothing to carry across the restart");

            bot.disconnect().await;
            (sword, before)
        });
        // **Stopped with the player still on it**, which is the case the
        // leave-diff cannot cover: nobody watches the tick see them go when the
        // server is the thing going away.
        server.stop();
    }

    let server = start_at(world);
    block_on(async {
        let mut bot = join_as(&server, "Keeper", Identity::from_seed(&seed)).await;
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.units_of(sword) == before {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "came back carrying {} units of {} rather than {before}",
                bot.units_of(sword),
                sword
            );
            let _ = bot.walk([0.0; 3], 0, 2).await;
        }
        bot.disconnect().await;
    });
    server.stop();
}
