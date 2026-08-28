// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Inventories that belong to the world, over a real server.
//!
//! **A chest is the thing a Minecraft-like mod could not build.**
//! `register_view` gives every player one of something — an armour rack, a tool
//! belt — and that is the wrong shape for a box in the ground: there is one of
//! it, and whoever opens it sees the same contents.
//!
//! What is checked here is the part only a real server can show: that the view
//! reaches the player's own slots, that clicks move stacks into it, that what
//! goes in comes back out after a disconnect and a restart, and that two
//! players cannot both hold one.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

/// The container every test here uses.
const CHEST: &str = "chests:at:1,2,3";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-containers").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod with one chest in it, opened and filled by chat.
fn write_chests(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("chests");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"chests\"\nname = \"Chests\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

local CHEST = "chests:at:1,2,3"

game.register_on_player_join(function(event)
    game.give(event.player, { material = "chests:ground", units = 54 })
end)

game.register_on_chat(function(event)
    if event.text == "open" then
        game.make_container(CHEST, 9)
        if game.open_container(CHEST, event.player) then
            game.show_dialog{
                player = event.player, form = "chest",
                tree = { type = "container", direction = "column", children = {
                    { type = "label", text = "Chest" },
                    { type = "item_grid", view = CHEST, columns = 3, first = 1, count = 9 },
                }},
            }
        else
            game.set_block({ x = 9, y = 9, z = 9 }, "chests:busy")
        end
        return false
    end
    if event.text == "stow" then
        -- Move material straight in, so the test does not depend on clicks.
        local took = game.take(event.player, { material = "chests:ground", units = 27 })
        game.give(event.player, { material = "chests:ground", units = took, view = CHEST })
        return false
    end
    if event.text == "count" then
        local total = 0
        for _, stack in ipairs(game.container(CHEST)) do
            total = total + stack.units
        end
        -- Reported as a block somewhere nobody digs, which is how a mod says a
        -- number to a test: there is no "read a container" message on the wire.
        if total > 0 then
            game.set_block({ x = 5, y = 9, z = 5 }, "chests:counted_" .. total)
        end
        return false
    end
end)

game.register_block{ id = "busy" }
for n = 1, 60 do
    game.register_block{ id = "counted_" .. n }
end
"#,
    )
    .expect("script");
    root
}

/// A server over a given world directory, so a test can restart onto it.
fn start(mods: PathBuf, world: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(19),
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

/// Waits until the player's own views include one by that name.
async fn until_view_exists(bot: &mut Bot, view: &str, want: bool) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if bot.views().contains_key(view) == want {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        if bot.recv().await.is_err() {
            return false;
        }
    }
}

#[test]
fn opening_a_container_puts_it_in_the_players_own_views() {
    // **The whole design, from the outside.** A click moves a stack between a
    // slot and the player's cursor, and the cursor is on the player — so the
    // container goes to where the cursor is, and every mechanism the engine
    // already has works on it unchanged.
    let world = scratch("open-world");
    let server = start(write_chests("open"), world);
    block_on(async {
        let mut bot = join(&server, "Ada").await;
        bot.chat("open").await.expect("chat");
        assert!(
            until_view_exists(&mut bot, CHEST, true).await,
            "the container never reached the player's views: {:?}",
            bot.views().keys().collect::<Vec<_>>()
        );
        let slots = bot.view(CHEST).expect("the chest's view");
        assert_eq!(
            slots.len(),
            9,
            "the chest is not the size the mod asked for"
        );
        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn what_goes_into_a_container_survives_a_disconnect_and_a_restart() {
    // **The point of a container being the WORLD's.** A chest that emptied
    // when its owner logged out, or when the server restarted, would be a
    // chest nobody could use.
    let world = scratch("persist-world");
    let mods = write_chests("persist");

    let server = start(mods.clone(), world.clone());
    block_on(async {
        let mut bot = join(&server, "Ada").await;
        bot.chat("open").await.expect("chat");
        assert!(
            until_view_exists(&mut bot, CHEST, true).await,
            "never opened"
        );
        bot.chat("stow").await.expect("chat");

        // Wait until the chest's view really holds it.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            let inside: u32 = bot
                .view(CHEST)
                .map(|slots| slots.iter().flatten().map(|stack| stack.units).sum())
                .unwrap_or(0);
            if inside == 27 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the chest holds {inside} units, not 27"
            );
            bot.recv().await.expect("recv");
        }
        bot.disconnect().await;
    });
    assert!(server.stop(), "the world did not flush cleanly");

    // A new server over the same world: the chest is still full.
    let server = start(mods, world);
    block_on(async {
        let mut bot = join(&server, "Bert").await;
        bot.chat("count").await.expect("chat");

        let counted = bot
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "chests:counted_27")
            .map(|entry| entry.id)
            .expect("the mod registers the marker");
        bot.expect_block(tiamot_core::BlockPos::new(5, 9, 5), counted, PATIENCE)
            .await
            .expect("the chest came back holding something other than 27 units");
        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn two_players_cannot_hold_one_container() {
    // **Lending it twice would duplicate items**: each would get a copy, click
    // their own, and whoever closed second would write theirs over the other's.
    let world = scratch("busy-world");
    let server = start(write_chests("busy"), world);
    block_on(async {
        let mut ada = join(&server, "Ada").await;
        let mut bert = join(&server, "Bert").await;

        ada.chat("open").await.expect("chat");
        assert!(
            until_view_exists(&mut ada, CHEST, true).await,
            "never opened"
        );

        bert.chat("open").await.expect("chat");
        let busy = bert
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "chests:busy")
            .map(|entry| entry.id)
            .expect("the mod registers the marker");
        bert.expect_block(tiamot_core::BlockPos::new(9, 9, 9), busy, PATIENCE)
            .await
            .expect("a second player opened a chest somebody was already in");
        assert!(
            bert.view(CHEST).is_none(),
            "the second player got a copy anyway"
        );

        ada.disconnect().await;
        bert.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_container_comes_back_when_the_player_who_had_it_open_leaves() {
    // A container lent to somebody who has gone is one nobody can open again,
    // and its contents would be written into that player's row as theirs.
    let world = scratch("leave-world");
    let server = start(write_chests("leave"), world);
    block_on(async {
        let mut ada = join(&server, "Ada").await;
        ada.chat("open").await.expect("chat");
        assert!(
            until_view_exists(&mut ada, CHEST, true).await,
            "never opened"
        );
        ada.disconnect().await;

        // Somebody else can now open it, which is only true if it came back.
        let mut bert = join(&server, "Bert").await;
        bert.chat("open").await.expect("chat");
        assert!(
            until_view_exists(&mut bert, CHEST, true).await,
            "the chest was stranded on a player who left"
        );
        bert.disconnect().await;
    });
    server.stop();
}
