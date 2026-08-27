// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A player's say in how far they see, and the server's veto.
//!
//! # Why this is a negotiation rather than a setting
//!
//! How far a player can see is a bargain between two machines. The client knows
//! what its GPU and its patience can take; the server knows what it can afford
//! to send fifty of. Neither can decide alone, so the client asks and the server
//! answers with what it is willing to send.
//!
//! **Asking for less is always granted, and that direction is the one that
//! matters.** A player on a modest machine, or on a bad link, needs a way to
//! make the world smaller — and before this the server's setting was everyone's
//! setting, with no way for a client to opt down.
//!
//! **Asking for more is capped**, which is the half that has to hold against a
//! peer rather than merely work for a cooperative one: a client that could name
//! its own radius could make a server generate, encode and send an arbitrarily
//! large neighbourhood by saying a number.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::{self, ViewDistance};
use tiamot_core::proto::{ClientMessage, ServerMessage};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 1] = ["test:stone"];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-view-distance").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// A server whose own limit is the DEFAULT radius, so there is room both to ask
/// for less and to ask for more than it will give.
fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::DEFAULT,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(4),
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

/// The most recent radius the server told this client it is streaming at.
fn granted(bot: &Bot) -> Option<(u8, u8)> {
    bot.received()
        .iter()
        .rev()
        .find_map(|message| match message {
            ServerMessage::ViewDistance {
                horizontal,
                vertical,
            } => Some((*horizontal, *vertical)),
            _ => None,
        })
}

/// Panics if the server has closed the connection.
///
/// **The assertion every one of these tests was missing, and it cost a broken
/// build.** The session state machine gates every message by phase, and
/// `ViewDistance` was served by the transport layer without ever being given an
/// accepting arm — so the server answered the request AND disconnected the
/// client for sending it. The grant arrived, these tests read it and passed, and
/// the real client died on join with "ViewDistance is not valid in phase
/// InWorld".
///
/// A test that reads one message and ignores the connection it arrived on is
/// only testing half of what happened.
fn assert_still_connected(bot: &Bot, what: &str) {
    if let Some(reason) = bot.received().iter().find_map(|message| match message {
        ServerMessage::Disconnect { reason } => Some(reason),
        _ => None,
    }) {
        panic!("the server disconnected the client after {what}: {reason:?}");
    }
}

/// Waits for a condition, driving the connection while it waits.
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

#[test]
fn the_server_states_its_view_distance_without_being_asked() {
    // A client that never asks still has to know, or it draws its fog for a
    // radius it invented and the world ends in clear air.
    let server = start("unprompted");

    block_on(async {
        let mut bot = join(&server, "Quiet").await;
        assert!(
            until(&mut bot, Duration::from_secs(10), |bot| granted(bot)
                .is_some())
            .await,
            "the server never said how far it streams"
        );
        assert_eq!(
            granted(&bot),
            Some((
                ViewDistance::DEFAULT.horizontal,
                ViewDistance::DEFAULT.vertical
            )),
            "the unprompted answer should be the server's own configured radius"
        );
        assert_still_connected(&bot, "joining");
        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn asking_for_less_is_granted_and_costs_the_server_less() {
    let server = start("asking-less");

    block_on(async {
        let mut bot = join(&server, "Modest").await;
        assert!(
            until(&mut bot, Duration::from_secs(10), |bot| granted(bot)
                .is_some())
            .await,
            "no opening answer"
        );

        // **Wait until there is something to give back.** Streaming is paced, so
        // a client that shrinks the instant it joins has been sent nothing
        // outside the smaller radius yet and correctly gets no unloads — which
        // would make this test pass or fail on timing rather than on behaviour.
        let spawn = bot
            .received()
            .iter()
            .find_map(|message| match message {
                ServerMessage::JoinWorld { spawn, .. } => Some(spawn.chunk()),
                _ => None,
            })
            .expect("a spawn");
        assert!(
            until(&mut bot, Duration::from_secs(20), |bot| {
                bot.chunks_received()
                    .iter()
                    .any(|pos| !interest::contains(spawn, ViewDistance::MINIMUM, *pos))
            })
            .await,
            "nothing outside the minimum radius ever arrived, so there is nothing \
             for shrinking to give back"
        );

        bot.send(&ClientMessage::ViewDistance {
            horizontal: ViewDistance::MINIMUM.horizontal,
            vertical: ViewDistance::MINIMUM.vertical,
        })
        .await
        .expect("ask");

        assert!(
            until(&mut bot, Duration::from_secs(10), |bot| granted(bot)
                == Some((
                    ViewDistance::MINIMUM.horizontal,
                    ViewDistance::MINIMUM.vertical
                )))
            .await,
            "asking for less was not granted, got {:?}",
            granted(&bot)
        );
        assert_still_connected(&bot, "asking for a smaller view distance");

        // **And it has to actually cost less.** A grant that changed a number
        // and streamed the same neighbourhood would be worse than no feature at
        // all: the player would be told their machine was doing less work while
        // it did exactly as much.
        let held: std::collections::BTreeSet<_> = bot.chunks_received().into_iter().collect();
        let unloaded: std::collections::BTreeSet<_> = bot
            .received()
            .iter()
            .filter_map(|message| match message {
                ServerMessage::ChunkUnload { pos } => Some(*pos),
                _ => None,
            })
            .collect();
        assert!(
            !unloaded.is_empty(),
            "shrinking the radius unloaded nothing, so nothing was actually given back"
        );
        for pos in &unloaded {
            assert!(
                held.contains(pos),
                "{pos:?} was unloaded but had never been sent"
            );
        }

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn asking_for_more_than_the_server_allows_is_capped() {
    // The half that has to hold against a peer rather than merely work for a
    // cooperative one. A client that could name its own radius could make a
    // server generate, encode and send an arbitrarily large neighbourhood by
    // saying a number — so the answer is the server's limit, and the client is
    // TOLD it is, rather than being left to infer it from chunks that never
    // arrive.
    let server = start("asking-more");

    block_on(async {
        let mut bot = join(&server, "Greedy").await;
        assert!(
            until(&mut bot, Duration::from_secs(10), |bot| granted(bot)
                .is_some())
            .await,
            "no opening answer"
        );

        bot.send(&ClientMessage::ViewDistance {
            horizontal: u8::MAX,
            vertical: u8::MAX,
        })
        .await
        .expect("ask");

        // Long enough that a server which was going to obey would have started.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tokio::time::timeout(Duration::from_millis(200), bot.recv()).await;

        assert_eq!(
            granted(&bot),
            Some((
                ViewDistance::DEFAULT.horizontal,
                ViewDistance::DEFAULT.vertical
            )),
            "a client asking for 255 chunks was not capped to the server's own radius"
        );
        assert_still_connected(&bot, "asking for an absurd view distance");

        // And nothing outside the server's radius was actually sent.
        let spawn = bot
            .received()
            .iter()
            .find_map(|message| match message {
                ServerMessage::JoinWorld { spawn, .. } => Some(spawn.chunk()),
                _ => None,
            })
            .expect("a spawn");
        for pos in bot.chunks_received() {
            assert!(
                interest::contains(spawn, ViewDistance::DEFAULT, pos),
                "{pos:?} is outside the server's own radius but was sent anyway"
            );
        }

        bot.disconnect().await;
    });

    server.stop();
}
