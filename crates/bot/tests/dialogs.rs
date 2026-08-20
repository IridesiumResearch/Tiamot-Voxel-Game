// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Task 14: a server mod shows a dialog to a client that pushed no code.
//!
//! **Criterion 2, end to end.** The bot is a vanilla client: it has no mod
//! loaded, runs nothing the server sent, and understands the widget schema and
//! nothing else. If a dialog reaches it and its events reach the mod, the tier
//! that says "untrusted server mods describe UI as data" works.
//!
//! **Criterion 3** lives here too, because the two are the same seam seen from
//! opposite ends: a forged event is an ordinary event that happens to be a lie,
//! and the server's answer is the same validation either way.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::{Click, DialogEvent};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["shop:ground", "shop:counter"];

/// Long enough for a join, several ticks and a broadcast on a loaded runner.
const PATIENCE: Duration = Duration::from_secs(10);

/// Where the mod marks the world, one height per kind of event.
const PRESSED: tiamot_core::BlockPos = tiamot_core::BlockPos::new(0, 4, 0);
const CLICKED: tiamot_core::BlockPos = tiamot_core::BlockPos::new(0, 6, 0);
const CLOSED: tiamot_core::BlockPos = tiamot_core::BlockPos::new(0, 8, 0);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-dialogs-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that opens a dialog when somebody joins, and records what comes back.
///
/// It reports what it heard by PLACING A BLOCK, which is the only channel a bot
/// can observe without a "read a mod's variable" message that should not exist.
/// `mod_edits.rs` and `perception.rs` established the pattern.
fn write_shop(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("shop");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"shop\"\nname = \"Shop\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
local counter = game.register_block{ id = "counter" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }

game.register_on_player_join(function(event)
    game.show_dialog{
        player = event.player,
        form = "till",
        tree = {
            type = "container", direction = "column", gap = 4, padding = 8,
            children = {
                { type = "label", text = "Welcome" },
                { type = "button", name = "buy", text = "Buy" },
                { type = "item_grid", view = "player:main", columns = 9, count = 9 },
            },
        },
    }
end)

-- Every event marks the world so a bot can see it happened. The block goes at
-- a height that depends on WHAT happened, so one position tells them apart.
game.register_on_dialog_event(function(event)
    local y = 0
    if event.kind == "pressed" then y = 4
    elseif event.kind == "clicked" then y = 6
    elseif event.kind == "closed" then y = 8
    end
    if y > 0 then
        game.set_block({ x = 0, y = y, z = 0 }, "shop:counter")
    end
end)
"#,
    )
    .expect("script");
    root
}

fn start(name: &str, mods: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(11),
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

#[test]
fn a_server_mod_shows_a_dialog_to_a_client_that_pushed_no_code() {
    // **Criterion 2.** Nothing the server sent is executed by this bot. It
    // decodes a widget tree and that is all it can do with one.
    let server = start("shows", write_shop("shows"));
    block_on(async {
        let mut bot = join(&server, "Shopper").await;
        bot.recv_until(|m| matches!(m, tiamot_core::proto::ServerMessage::ShowDialog { .. }))
            .await
            .expect("no dialog arrived");
        let dialogs = bot.dialogs();
        let (form, tree) = &dialogs[0];
        // Namespaced with the owning mod's id, so two mods may both use "till".
        assert_eq!(form, "shop:till");
        assert!(
            tiamot_core::ui::check(tree, tiamot_core::ui::Limits::default()).is_ok(),
            "the server sent a tree that fails its own checker"
        );

        // The tree says what the mod wrote: a column, a label, a button, a grid.
        let kinds: Vec<&str> = tree
            .nodes
            .iter()
            .map(|node| match &node.widget {
                tiamot_core::ui::Widget::Container { .. } => "container",
                tiamot_core::ui::Widget::Label { .. } => "label",
                tiamot_core::ui::Widget::Button { .. } => "button",
                tiamot_core::ui::Widget::ItemGrid { .. } => "grid",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["container", "label", "button", "grid"],
            "{kinds:?}"
        );
        // Children after parents — the invariant everything downstream relies on.
        for (index, node) in tree.nodes.iter().enumerate() {
            if node.children.count > 0 {
                assert!(node.children.first as usize > index, "child before parent");
            }
        }

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_button_press_reaches_the_mod_that_opened_the_dialog() {
    let server = start("press", write_shop("press"));
    block_on(async {
        let mut bot = join(&server, "Presser").await;
        bot.recv_until(|m| matches!(m, tiamot_core::proto::ServerMessage::ShowDialog { .. }))
            .await
            .expect("no dialog arrived");

        bot.dialog_event(
            "shop:till",
            DialogEvent::Pressed {
                name: "buy".to_owned(),
            },
        )
        .await
        .expect("send");

        // The mod marks the world at y=4 when it hears a press.
        let counter = bot
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "shop:counter")
            .map(|entry| entry.id)
            .expect("the mod registers a counter");
        bot.expect_block(PRESSED, counter, PATIENCE)
            .await
            .expect("the press never reached the mod");

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_forged_event_for_a_dialog_nobody_opened_changes_nothing() {
    // **Criterion 3, the forgery half.** A client can send any bytes it likes.
    // The server routes an event by the OWNER it recorded when the dialog
    // opened, so an event naming a form nobody owns has nowhere to go — and
    // one naming another mod's form still reaches only that mod, which never
    // opened it for this player.
    let server = start("forged", write_shop("forged"));
    block_on(async {
        let mut bot = join(&server, "Forger").await;
        bot.recv_until(|m| matches!(m, tiamot_core::proto::ServerMessage::ShowDialog { .. }))
            .await
            .expect("no dialog arrived");
        let counter = bot
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "shop:counter")
            .map(|entry| entry.id)
            .expect("the mod registers a counter");

        for (form, event) in [
            (
                "shop:nosuchform",
                DialogEvent::Pressed {
                    name: "buy".to_owned(),
                },
            ),
            (
                "otherMod:till",
                DialogEvent::Pressed {
                    name: "buy".to_owned(),
                },
            ),
            (
                "shop:nosuchform",
                DialogEvent::Clicked {
                    view: "player:main".to_owned(),
                    index: 9999,
                    click: Click::Right,
                },
            ),
        ] {
            bot.dialog_event(form, event).await.expect("send");
        }

        // A real press on the real form, sent LAST. When its mark arrives the
        // server has demonstrably processed everything before it — which is
        // what makes the absence of the other marks meaningful rather than a
        // race the test happened to win.
        bot.dialog_event(
            "shop:till",
            DialogEvent::Pressed {
                name: "buy".to_owned(),
            },
        )
        .await
        .expect("send");
        bot.expect_block(PRESSED, counter, PATIENCE)
            .await
            .expect("the honest press never landed, so the test proves nothing");

        // The marks the mod would leave if the forgeries had been delivered.
        for pos in [CLICKED, CLOSED] {
            assert!(
                !bot.saw_block(pos, counter),
                "a forged event for a form nobody owns reached the mod: {pos:?}"
            );
        }

        bot.disconnect().await;
    });
    server.stop();
}
