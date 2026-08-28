// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Task 14: the reference HUD is a mod's, and it runs on the real `game/`.
//!
//! `crates/client/tests/connection.rs` proves the criterion from the client's
//! end — the script arrives, draws a hotbar, and goes when the mod goes. This
//! is the SERVER's end: that `game/core_ui` is a mod like any other, that
//! declaring a HUD script publishes exactly one Lua file and no others, and
//! that its inventory screen opens on the action it registered.
//!
//! The two halves of criterion 1 are two different tiers. The hotbar is a
//! script, because its content is a function of what a player carries. The
//! inventory screen is a widget tree, because it is not.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-hud-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
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

#[test]
fn the_reference_mods_push_exactly_one_hud_script() {
    // Charter rule 1 for the HUD: the engine draws a crosshair and the rest is
    // a mod's. `core_ui` is the only reference mod that asks for a script, and
    // the table names it and nothing else.
    let server = start("push");
    block_on(async {
        let bot = join(&server, "watcher").await;
        let scripts: Vec<tiamot_core::proto::HudScriptDef> = bot
            .received()
            .into_iter()
            .filter_map(|message| match message {
                tiamot_core::proto::ServerMessage::HudScripts { scripts } => Some(scripts),
                _ => None,
            })
            .next()
            .expect("a HudScripts message on join");

        assert_eq!(scripts.len(), 1, "got {scripts:?}");
        assert_eq!(scripts[0].mod_id, "core_ui");
        assert!(
            scripts[0].file.is_some(),
            "the script's file should have been indexed; a `None` here means the server could \
             not find `hud.lua` in the mod directory"
        );
    });
    assert!(server.stop());
}

#[test]
fn declaring_a_hud_script_publishes_that_file_and_no_other_lua() {
    // **The rule that made this work at all.** `.lua` is not distributable by
    // extension, because server mod code holds whatever a server operator put
    // in it. A mod publishes exactly the file it named — so `core_ui/hud.lua`
    // is fetchable and `core_ui/init.lua` is not, even though they sit in the
    // same directory.
    let server = start("publish");
    block_on(async {
        let mut bot = join(&server, "fetcher").await;
        let scripts = bot
            .received()
            .into_iter()
            .find_map(|message| match message {
                tiamot_core::proto::ServerMessage::HudScripts { scripts } => Some(scripts),
                _ => None,
            })
            .expect("a HudScripts message");
        let hash = scripts[0].file.expect("a hash");

        bot.request_content(vec![hash]).await.expect("ask");
        let got = bot.collect_content(1, PATIENCE).await.expect("collect");
        assert_eq!(got.len(), 1, "the declared script should be fetchable");
        let source = String::from_utf8(got[0].1.clone()).expect("Lua is text");
        assert!(
            source.contains("hud.on_draw"),
            "that is not core_ui's HUD script: {source:.120}"
        );
        assert!(
            !source.contains("register_hud_script"),
            "`init.lua` was published instead of `hud.lua`"
        );

        // And the file beside it is not reachable. Content is addressed by
        // hash, so the check is that the server never indexed it: ask for the
        // hash of the real `init.lua` and get nothing back.
        let init = std::fs::read(reference_mods().join("core_ui/init.lua")).expect("init.lua");
        let init_hash: [u8; 32] = *blake3::hash(&init).as_bytes();
        bot.request_content(vec![init_hash]).await.expect("ask");
        let got = bot
            .collect_content(1, Duration::from_secs(2))
            .await
            .expect("collect");
        assert!(
            got.is_empty(),
            "a mod's server-side Lua must not be fetchable"
        );
    });
    assert!(server.stop());
}

#[test]
fn what_a_player_digs_shows_up_in_the_slots_core_uis_screen_draws() {
    // **Reported from the window: "items do not seem to display in my
    // inventory yet".** Two faults, one symptom. `player:main` started with
    // zero slots and grew, and the screen drew a grid over slots 10..36 of it —
    // so a player who had dug one block owned slot 1 of a one-slot view and was
    // shown twenty-seven boxes that were not it.
    //
    // This asserts the whole path: dig, the material lands in the first slot of
    // `player:main`, and the tree the mod sends is a grid over that view
    // starting at that slot.
    let server = start("dug");
    block_on(async {
        let mut bot = join(&server, "digger").await;

        // `y = -1` is the surface: `fill_below_heightmap(0)` fills everything
        // BELOW zero, so the topmost solid block is under the player's feet.
        bot.dig_block(tiamot_core::BlockPos::new(0, -1, 0))
            .await
            .expect("dig");
        let slots = bot
            .until_view("player:main", |slots| {
                slots.first().is_some_and(Option::is_some)
            })
            .await
            .expect("the dug block never reached a slot");
        let units = slots[0].as_ref().expect("a stack in the first slot").units;
        assert!(units > 0);
        assert!(
            slots.len() >= 27,
            "a player needs room before the screen can show any, got {} slots",
            slots.len()
        );

        // And the screen the mod draws is over that view, from that slot.
        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let tree = loop {
            if let Some((_, tree)) = bot
                .dialogs()
                .into_iter()
                .find(|(form, _)| form == "core_ui:inventory")
            {
                break tree;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the inventory never opened"
            );
            bot.recv().await.expect("recv");
        };

        let grids: Vec<(&str, u16, u16)> = tree
            .nodes
            .iter()
            .filter_map(|node| match &node.widget {
                tiamot_core::ui::Widget::ItemGrid {
                    view, first, count, ..
                } => Some((view.as_str(), *first, *count)),
                _ => None,
            })
            .collect();
        assert!(
            grids.contains(&("player:main", 0, 27)),
            "the main grid should start at the first slot a player owns, got {grids:?}"
        );
        // **One grid, because there is one inventory.** The hotbar used to be
        // `player:hotbar`, a second view of nine slots beside the twenty-seven,
        // and a player had to shuffle stacks between two grids to put anything
        // where a number key could reach it. It is now a BAND: the first nine
        // slots of this grid, which is what its label says.
        assert_eq!(
            grids.len(),
            1,
            "a second grid is a second place a player has to move things to: {grids:?}"
        );
    });
    assert!(server.stop());
}

#[test]
fn core_uis_inventory_screen_opens_and_closes_on_the_action_it_registered() {
    // Criterion 1's other half, and criterion 2 on the real reference mods
    // rather than a fixture: the inventory screen is a widget TREE, opened by
    // an action the mod registered and the engine bound.
    let server = start("inventory");
    block_on(async {
        let mut bot = join(&server, "opener").await;

        // The action exists because the mod asked for it — the engine has no
        // inventory key of its own (charter rule 11).
        let actions = bot
            .received()
            .into_iter()
            .find_map(|message| match message {
                tiamot_core::proto::ServerMessage::ActionTable { actions } => Some(actions),
                _ => None,
            })
            .expect("an action table");
        let inventory = actions
            .iter()
            .find(|action| action.id == "core_ui:inventory")
            .expect("core_ui should register an inventory action");
        assert_eq!(inventory.mod_id, "core_ui");
        assert_eq!(
            inventory.default_key, "KeyE",
            "the mod suggests a default and the engine owns the binding"
        );

        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let opened = loop {
            if let Some(found) = bot
                .dialogs()
                .into_iter()
                .find(|(form, _)| form == "core_ui:inventory")
            {
                break found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the inventory never opened"
            );
            bot.recv().await.expect("recv");
        };
        let (_, tree) = opened;
        assert!(
            tree.nodes.iter().any(|node| matches!(
                &node.widget,
                tiamot_core::ui::Widget::ItemGrid { view, .. } if view == "player:main"
            )),
            "the screen should show the player's own slots"
        );

        // Pressing again closes it. A key that only opened would be a key that
        // trapped a player behind their own inventory.
        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot
                .closed_dialogs()
                .iter()
                .any(|form| form == "core_ui:inventory")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the inventory never closed"
            );
            bot.recv().await.expect("recv");
        }
    });
    assert!(server.stop());
}

#[test]
fn a_screen_closed_from_the_client_can_be_opened_again() {
    // **What Escape now does, and the trap under it.** The client raises
    // `Closed` for the topmost dialog rather than opening the pause menu behind
    // it, and that event goes to the mod as well as to the engine: the engine
    // closes the form whatever the mod does, but the MOD is the thing holding
    // "this player has the screen open". A mod that never heard the close would
    // still believe it was open, so the next press of the key would toggle it
    // shut and the inventory would look dead.
    //
    // The keystroke itself is the window's and is not tested here — this is the
    // half that crosses the wire.
    let server = start("closed");
    block_on(async {
        let mut bot = join(&server, "closer").await;

        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot
                .dialogs()
                .into_iter()
                .any(|(form, _)| form == "core_ui:inventory")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the inventory never opened"
            );
            bot.recv().await.expect("recv");
        }

        // What Escape sends.
        bot.dialog_event("core_ui:inventory", tiamot_core::proto::DialogEvent::Closed)
            .await
            .expect("send");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot
                .closed_dialogs()
                .iter()
                .any(|form| form == "core_ui:inventory")
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "closing it from the client did not close it"
            );
            bot.recv().await.expect("recv");
        }

        // And it opens again on the same key, which is what says the mod heard.
        let before = bot
            .dialogs()
            .into_iter()
            .filter(|(form, _)| form == "core_ui:inventory")
            .count();
        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let now = bot
                .dialogs()
                .into_iter()
                .filter(|(form, _)| form == "core_ui:inventory")
                .count();
            if now > before {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the key that used to open the inventory stopped working after a close"
            );
            bot.recv().await.expect("recv");
        }

        bot.disconnect().await;
    });
    assert!(server.stop());
}

/// Reads the tree of `core_ui`'s screen, waiting for one that satisfies `ready`.
async fn screen(
    bot: &mut Bot,
    ready: impl Fn(&tiamot_core::ui::Tree) -> bool,
) -> tiamot_core::ui::Tree {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        // **The LAST tree, not the first.** `Bot::dialogs` is a log of what
        // arrived, so a screen the mod has redrawn appears twice — and taking
        // the first hands back the version from before the button was pressed.
        if let Some((_, tree)) = bot
            .dialogs()
            .into_iter()
            .rfind(|(form, _)| form == "core_ui:inventory")
            && ready(&tree)
        {
            return tree;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the screen never reached the state this was waiting for; open: {:?}",
            bot.dialogs()
                .into_iter()
                .map(|(form, tree)| (
                    form,
                    tree.nodes
                        .iter()
                        .map(|node| format!("{:?}/{}", node.widget, node.name))
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
        );
        bot.recv().await.expect("recv");
    }
}

/// The name of every button in a tree.
fn buttons(tree: &tiamot_core::ui::Tree) -> Vec<String> {
    tree.nodes
        .iter()
        .filter(|node| matches!(node.widget, tiamot_core::ui::Widget::Button { .. }))
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn core_uis_shape_tab_cuts_a_block_and_the_cut_comes_back() {
    // **The reference implementation of crafting, end to end.** The engine
    // draws twenty-seven cells and reports a mask; that one item of a cut costs
    // one unit per cell is written in `game/core_ui/init.lua` and nowhere else
    // (charter rule 1). This drives the shipped mod rather than a fixture, so
    // it is the recipe a player actually gets.
    const CARVED: u32 = 0b1000_1111;

    let server = start("shapes");
    block_on(async {
        let mut bot = join(&server, "mason").await;

        // Something loose to cut. One block is 27 units and a five-cell stair
        // costs five, so one dig is more than enough.
        bot.dig_block(tiamot_core::BlockPos::new(0, -1, 0))
            .await
            .expect("dig");
        bot.until_view("player:main", |slots| {
            slots.first().is_some_and(Option::is_some)
        })
        .await
        .expect("the dug block never reached a slot");
        let before: u32 = bot.inventory().iter().map(|stack| stack.units).sum();

        bot.action("core_ui:inventory", true).await.expect("press");
        let tree = screen(&mut bot, |tree| !buttons(tree).is_empty()).await;
        assert!(
            buttons(&tree).iter().any(|name| name == "tab_shapes"),
            "the shape tab is not on the screen: {:?}",
            buttons(&tree)
        );

        // Switch to it, and the editor appears with a whole block in it.
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Pressed {
                name: "tab_shapes".to_owned(),
            },
        )
        .await
        .expect("send");
        let tree = screen(&mut bot, |tree| {
            tree.nodes
                .iter()
                .any(|node| matches!(node.widget, tiamot_core::ui::Widget::ShapeEditor { .. }))
        })
        .await;
        let opened = tree
            .nodes
            .iter()
            .find_map(|node| match node.widget {
                tiamot_core::ui::Widget::ShapeEditor { shape, .. } => Some(shape),
                _ => None,
            })
            .expect("checked above");
        assert_eq!(
            opened,
            tiamot_core::inventory::Shape::ALL,
            "chiselling is subtraction, so it starts from a whole block"
        );

        // Carve, then make one.
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Chiselled {
                name: "cut".to_owned(),
                shape: CARVED,
            },
        )
        .await
        .expect("send");
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Pressed {
                name: "make".to_owned(),
            },
        )
        .await
        .expect("send");

        // Driven to a CONDITION with a deadline: how many updates arrive before
        // the one carrying the cut is the machine's business.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let made = loop {
            let stacks = bot.inventory();
            if stacks.iter().any(|stack| stack.shape == CARVED) {
                break stacks;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the cut never came back: {:?}",
                bot.inventory()
            );
            bot.recv().await.expect("recv");
        };
        let cut = made
            .iter()
            .find(|stack| stack.shape == CARVED)
            .expect("checked above");
        assert_eq!(
            cut.units,
            CARVED.count_ones(),
            "one item of a cut costs one unit per cell"
        );
        let after: u32 = made.iter().map(|stack| stack.units).sum();
        assert_eq!(after, before, "crafting invented or destroyed units");

        bot.disconnect().await;
    });
    assert!(server.stop());
}

#[test]
fn the_offhand_key_swaps_and_swapping_twice_puts_it_back() {
    // **A place in the player's own inventory, not a second one.** The off-hand
    // is slot 28 of `player:main` — so `game.inventory` sees it, conservation
    // counts it, and there is no separate grid to shuffle between. The swap is
    // the server's: the client says which slot it was holding and nothing else.
    let server = start("offhand");
    block_on(async {
        let mut bot = join(&server, "swapper").await;

        bot.dig_block(tiamot_core::BlockPos::new(0, -1, 0))
            .await
            .expect("dig");
        let slots = bot
            .until_view("player:main", |slots| {
                slots.first().is_some_and(Option::is_some)
            })
            .await
            .expect("the dug block never reached a slot");
        let held = slots[0].clone().expect("a stack in the first slot");
        assert!(
            slots.len() > tiamot_core::inventory::PLAYER_OFFHAND_SLOT,
            "there is nowhere for the off-hand to be: {} slots",
            slots.len()
        );
        assert!(
            slots[tiamot_core::inventory::PLAYER_OFFHAND_SLOT].is_none(),
            "the off-hand starts empty"
        );
        let before: u32 = bot.inventory().iter().map(|stack| stack.units).sum();

        bot.send(&tiamot_core::proto::ClientMessage::SwapOffhand { slot: 0 })
            .await
            .expect("send");
        let swapped = bot
            .until_view("player:main", |slots| {
                slots
                    .get(tiamot_core::inventory::PLAYER_OFFHAND_SLOT)
                    .is_some_and(Option::is_some)
            })
            .await
            .expect("nothing ever reached the off-hand");
        assert_eq!(
            swapped[tiamot_core::inventory::PLAYER_OFFHAND_SLOT].as_ref(),
            Some(&held),
            "what was in the hand is not what is in the off-hand"
        );
        assert!(swapped[0].is_none(), "the hand should be empty now");

        // And back, because a gesture you cannot undo without looking is one
        // nobody presses.
        bot.send(&tiamot_core::proto::ClientMessage::SwapOffhand { slot: 0 })
            .await
            .expect("send");
        let back = bot
            .until_view("player:main", |slots| {
                slots.first().is_some_and(Option::is_some)
            })
            .await
            .expect("it never came back");
        assert_eq!(
            back[0].as_ref(),
            Some(&held),
            "two presses did not put it back"
        );
        assert!(back[tiamot_core::inventory::PLAYER_OFFHAND_SLOT].is_none());

        let after: u32 = bot.inventory().iter().map(|stack| stack.units).sum();
        assert_eq!(after, before, "swapping invented or destroyed units");

        bot.disconnect().await;
    });
    assert!(server.stop());
}

#[test]
fn making_a_stack_makes_as_many_as_the_player_can_pay_for() {
    // **Asked for from the window**: a "make stack" button in the shape
    // crafter, because ninety of a cut was ninety clicks.
    //
    // Driven against the REAL `core_ui`, through the real widget tree, because
    // the failure this rules out is the one that keeps happening: a button a
    // mod draws whose name nothing handles looks exactly like a button that
    // works until somebody presses it.
    let server = start("make-stack");
    block_on(async {
        let mut bot = join(&server, "Maker").await;

        // Something to cut. Digging one block is twenty-seven units.
        bot.dig_block(tiamot_core::BlockPos::new(0, -1, 0))
            .await
            .expect("the block should break");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let loose = loop {
            let held: u32 = bot.inventory().iter().map(|stack| stack.units).sum();
            if held >= 27 {
                break held;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "digging a block credited {held} units"
            );
            bot.recv().await.expect("recv");
        };

        bot.action("core_ui:inventory", true).await.expect("press");
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while !bot
            .dialogs()
            .iter()
            .any(|(form, _)| form == "core_ui:inventory")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the inventory never opened"
            );
            bot.recv().await.expect("recv");
        }

        // The shapes page, then a cut. `chiselled` is what the editor reports,
        // and the mod keeps it — so this is the same path a player's clicks
        // take, minus the clicking.
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Pressed {
                name: "tab_shapes".to_owned(),
            },
        )
        .await
        .expect("send");
        // A four-cell cut, so a block of twenty-seven pays for six of them.
        let cut = 0b1111u32;
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Chiselled {
                name: "cut".to_owned(),
                shape: cut,
            },
        )
        .await
        .expect("send");
        bot.dialog_event(
            "core_ui:inventory",
            tiamot_core::proto::DialogEvent::Pressed {
                name: "make_stack".to_owned(),
            },
        )
        .await
        .expect("send");

        let cost = cut.count_ones();
        let expected = loose / cost;
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let made: u32 = bot
                .inventory()
                .iter()
                .filter(|stack| stack.shape == cut)
                .map(|stack| stack.units / cost)
                .sum();
            if made == expected {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "one press should have made {expected} of the cut, and made {made}: {:?}",
                bot.inventory()
            );
            bot.recv().await.expect("recv");
        }

        // Conservation, which is the claim underneath: a stack was made out of
        // what was paid for and nothing else.
        let held: u32 = bot.inventory().iter().map(|stack| stack.units).sum();
        assert_eq!(held, loose, "units went missing making a stack");

        bot.disconnect().await;
    });
    server.stop();
}
