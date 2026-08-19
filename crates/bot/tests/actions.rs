// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A mod-registered action, end to end, through the real `game/core_tools`.
//!
//! Charter rule 11 is the thing under test: a mod registers a NAME, the engine
//! owns the key, and the mod is told what was done rather than which key did
//! it. `core_tools:chisel_mode` is the reference implementation — a control
//! that swaps the tool while held — and it is built entirely out of the mod
//! API, so it is testable only from outside.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::{ClientMessage, ServerMessage};
use tiamot_core::{BlockPos, SubNodePos};
use tiamot_server::{ServerHandle, Settings};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-actions").join(name);
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
        seed: Some(5),
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

/// A solid block from the reference set, as the server numbered it.
async fn solid_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server should have sent a material table")
        .into_iter()
        .find(|entry| entry.name.ends_with(":white"))
        .map(|entry| entry.id)
        .expect("the reference mods should register a plain solid block")
}

/// What a dig took out: a whole block, one cell, or nothing yet.
#[derive(Debug, PartialEq, Eq)]
enum Took {
    Block,
    Cell,
    Nothing,
}

fn took(bot: &Bot, at: BlockPos) -> Took {
    for message in bot.received() {
        if let ServerMessage::BlockDelta { edit, .. } = message {
            match edit {
                tiamot_core::proto::Edit::Block { pos, material }
                    if pos == at && material == tiamot_core::MaterialId::AIR.0 =>
                {
                    return Took::Block;
                }
                tiamot_core::proto::Edit::SubNode { pos, material }
                    if pos.block() == at && material == tiamot_core::MaterialId::AIR.0 =>
                {
                    return Took::Cell;
                }
                _ => {}
            }
        }
    }
    Took::Nothing
}

/// Digs the middle of a block WITHOUT choosing a tool first.
///
/// Every `Bot::dig_*` helper picks a tool by brush before it digs, which is
/// right for their callers and would defeat this test entirely: the whole
/// question is which tool the MOD put in the player's hand. So this drives the
/// dig by hand and lets whatever is held decide what comes out.
async fn dig_with_whatever_is_held(bot: &mut Bot, at: BlockPos) -> Took {
    let centre = SubNodePos::new(at.x * 3 + 1, at.y * 3 + 1, at.z * 3 + 1);
    bot.send(&ClientMessage::StartDig { target: centre })
        .await
        .expect("the dig should reach the server");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        let outcome = took(bot, at);
        if outcome != Took::Nothing {
            return outcome;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.recv()).await;
    }
    Took::Nothing
}

async fn press(bot: &mut Bot, pressed: bool) {
    bot.send(&ClientMessage::Action {
        id: "core_tools:chisel_mode".to_owned(),
        pressed,
    })
    .await
    .expect("the action should reach the server");
    // The mods are told inside the tick, so give the server one.
    bot.sleep_ticks(2).await;
}

#[test]
fn holding_a_mods_action_swaps_the_tool_and_releasing_puts_it_back() {
    // **The end-to-end criterion.** A mod registers an action, the client sends
    // it by NAME, the server hands it to the mod, and the mod changes what the
    // player is holding — which shows up as the dig taking one cell instead of
    // a whole block.
    //
    // Asserted on the EFFECT rather than on the tool, because a client is never
    // told which tool it holds and should not be: what a tool means is a mod's
    // business (charter rule 1) and the server is the authority on what a dig
    // removes (rule 2).
    let server = start("chisel-mode");
    block_on(async {
        let mut bot = join(&server, "Chiseller").await;
        let solid = solid_id(&bot).await;

        // Three blocks in a row to dig, one per phase, so no phase is digging
        // what the last one left behind.
        let blocks = [
            BlockPos::new(2, 5, 2),
            BlockPos::new(3, 5, 2),
            BlockPos::new(4, 5, 2),
        ];
        for at in blocks {
            assert!(server.seed_block(at, solid), "the world should seed");
        }
        bot.move_to(3.0, 0.0, 4.0).await.expect("walk into reach");
        bot.sleep_ticks(4).await;

        // **Before the press**: the default tool is a bare hand, whose brush is
        // a whole block. If this comes out as a cell the test proves nothing
        // about the action, so it is asserted rather than assumed.
        assert_eq!(
            dig_with_whatever_is_held(&mut bot, blocks[0]).await,
            Took::Block,
            "a bare hand did not take a whole block, so the phases below cannot \
             be told apart"
        );

        // **Held**: the mod swaps in the chisel, whose brush is one cell.
        press(&mut bot, true).await;
        assert_eq!(
            dig_with_whatever_is_held(&mut bot, blocks[1]).await,
            Took::Cell,
            "holding core_tools:chisel_mode did not put the chisel in hand — \
             the action did not reach the mod, or the mod could not set the tool"
        );

        // **Released**: and back to what was there, which the mod remembered
        // rather than assumed.
        press(&mut bot, false).await;
        assert_eq!(
            dig_with_whatever_is_held(&mut bot, blocks[2]).await,
            Took::Block,
            "releasing the action did not put the original tool back"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_server_refuses_an_action_it_never_registered() {
    // Charter rule 14: a client is not trusted. An id the server does not have
    // must not reach the Lua dispatcher at all — every unknown id would
    // otherwise cost a hook run and a table, which is a cheap thing for a
    // hostile peer to spend the server's tick on.
    //
    // Asserted by the server still being there and still working afterwards:
    // there is nothing for an invented id to DO, so what is being checked is
    // that it is absorbed rather than acted on or fatal.
    let server = start("invented-action");
    block_on(async {
        let mut bot = join(&server, "Inventor").await;
        for id in ["not_a_mod:not_an_action", "engine:jump", ""] {
            bot.send(&ClientMessage::Action {
                id: id.to_owned(),
                pressed: true,
            })
            .await
            .expect("the message should be accepted by the transport");
        }
        bot.sleep_ticks(4).await;

        // Still connected, still ticking: the invented ids went nowhere.
        let solid = solid_id(&bot).await;
        let at = BlockPos::new(2, 5, 2);
        assert!(server.seed_block(at, solid), "the world should still seed");
        bot.move_to(2.0, 0.0, 4.0).await.expect("walk into reach");
        bot.sleep_ticks(4).await;
        assert_eq!(
            dig_with_whatever_is_held(&mut bot, at).await,
            Took::Block,
            "the session stopped working after an invented action id"
        );

        bot.disconnect().await;
    });
    server.stop();
}
