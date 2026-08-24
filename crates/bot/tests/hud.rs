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
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
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
        let units = slots[0].expect("a stack in the first slot").units;
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
        assert!(
            grids.iter().any(|(view, _, _)| *view == "player:hotbar"),
            "and the hotbar is its OWN view, not the tail of the main one: {grids:?}"
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
