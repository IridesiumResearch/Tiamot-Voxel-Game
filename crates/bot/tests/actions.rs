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
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(repo().join("game")),
        enabled_mods: None,
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
///
/// **Counted rather than read off one message.** A block brush no longer emits
/// a single `Edit::Block` — a block comes apart one sub-node at a time, so the
/// only difference between the two brushes on the wire is how MANY cells go.
/// A chisel takes one and stops; a bare hand takes all twenty-seven.
#[derive(Debug, PartialEq, Eq)]
enum Took {
    Block,
    Cell,
    Nothing,
}

/// How many distinct sub-nodes of `at` have been removed so far.
fn cells_taken(bot: &Bot, at: BlockPos) -> usize {
    let mut gone = std::collections::BTreeSet::new();
    for message in bot.received() {
        if let ServerMessage::BlockDelta { edit, .. } = message {
            match edit {
                // Still possible from a mod's `set_block`, and still a whole
                // block when it happens.
                tiamot_core::proto::Edit::Block { pos, material }
                    if pos == at && material == tiamot_core::MaterialId::AIR.0 =>
                {
                    return tiamot_core::block::SUBNODES_PER_BLOCK;
                }
                tiamot_core::proto::Edit::SubNode { pos, material }
                    if pos.block() == at && material == tiamot_core::MaterialId::AIR.0 =>
                {
                    gone.insert((pos.x, pos.y, pos.z));
                }
                _ => {}
            }
        }
    }
    gone.len()
}

/// Digs the middle of a block WITHOUT choosing a tool first.
///
/// Every `Bot::dig_*` helper picks a tool by brush before it digs, which is
/// right for their callers and would defeat this test entirely: the whole
/// question is which tool the MOD put in the player's hand. So this drives the
/// dig by hand and lets whatever is held decide what comes out.
async fn dig_with_whatever_is_held(bot: &mut Bot, at: BlockPos) -> Took {
    let centre = SubNodePos::new(at.x * 3 + 1, at.y * 3 + 1, at.z * 3 + 1);

    // **Re-sent while waiting, the way `Bot::dig_block` does it.** A dig
    // re-aimed at the same cell is the same dig and costs nothing to repeat,
    // and a single `StartDig` can be refused — for reach, most often — leaving
    // this to wait out its whole deadline for a block that was never coming.
    //
    // That is exactly how it failed on the macOS runner: `move_to` reports an
    // INTENT, so on a machine where the walk had not finished the dig was out
    // of reach, refused once, and the test read `Nothing` and blamed the tool.
    // **Waited out rather than answered on the first cell.** A block brush
    // takes twenty-seven bites over the dig's whole duration, so the first
    // `SubNode` edit says only that digging has started — reading the outcome
    // there would call every dig a chisel. A chisel settles at one cell and
    // stays there, so the wait is what tells them apart.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut next_send = tokio::time::Instant::now();
    let mut settled_at: Option<(usize, tokio::time::Instant)> = None;
    while tokio::time::Instant::now() < deadline {
        let taken = cells_taken(bot, at);
        if taken >= tiamot_core::block::SUBNODES_PER_BLOCK {
            return Took::Block;
        }
        // A count that has not moved for a beat is a dig that has finished.
        match settled_at {
            Some((count, since)) if count == taken => {
                if taken > 0 && since.elapsed() > Duration::from_millis(1500) {
                    return Took::Cell;
                }
            }
            _ => settled_at = Some((taken, tokio::time::Instant::now())),
        }
        if tokio::time::Instant::now() >= next_send {
            bot.send(&ClientMessage::StartDig { target: centre })
                .await
                .expect("the dig should reach the server");
            next_send = tokio::time::Instant::now() + Duration::from_millis(500);
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.recv()).await;
    }
    // Nothing happened in twenty seconds. Whatever the server said about it is
    // far more useful than the bare `Nothing` this would otherwise report.
    let notices = bot.notices();
    assert!(
        notices.is_empty(),
        "the dig never landed and the server said: {notices:?}"
    );
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

#[test]
fn a_bot_script_drives_the_action_the_same_way_a_keyboard_does() {
    // **Criterion 5's convergence, asserted rather than asserted-about.** The
    // bot presses the action by NAME, exactly as the client does after it turns
    // a key into an id — no parallel path, so a scenario cannot keep passing
    // while the path a player uses rots.
    //
    // Charter rule 11 is what makes this possible: the id is the thing that
    // travels, and no part of the server knows which key produced it. A script
    // written against a key would break the moment somebody rebound it.
    let server = start("bot-script-action");
    block_on(async {
        let mut bot = join(&server, "Scripted").await;
        let solid = solid_id(&bot).await;
        let at = BlockPos::new(2, 5, 2);
        assert!(server.seed_block(at, solid), "the world should seed");
        bot.move_to(2.0, 0.0, 4.0).await.expect("walk into reach");
        bot.sleep_ticks(4).await;

        // The bot's own API, which is what a recorded scenario replays.
        bot.action("core_tools:chisel_mode", true)
            .await
            .expect("press");
        bot.sleep_ticks(2).await;

        assert_eq!(
            dig_with_whatever_is_held(&mut bot, at).await,
            Took::Cell,
            "a bot pressing the action by id did not get the chisel, so the \
             bot's path and a player's have come apart"
        );

        bot.action("core_tools:chisel_mode", false)
            .await
            .expect("release");
        bot.disconnect().await;
    });
    server.stop();
}
