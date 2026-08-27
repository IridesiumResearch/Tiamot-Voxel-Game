// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Task 14: chat is the engine's, and a mod may still refuse a line.
//!
//! Chat lives in the engine because moderation and RCON depend on it — an
//! operator must be able to read and stop what is said without every server
//! having installed the same mod. What MAY be said is policy, and policy is a
//! mod's (charter rule 1), so `register_on_chat` is a veto.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-chat-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that refuses any line holding "quiet", and allows everything else.
fn write_warden(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("warden");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"warden\"\nname = \"Warden\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
game.register_on_chat(function(event)
    if event.text:find("quiet") then
        return "not here"
    end
end)
"#,
    )
    .expect("script");
    root
}

fn start(name: &str, mods: Option<PathBuf>) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: mods,
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

/// Reads until a chat line containing `needle` arrives, or the patience runs out.
async fn heard(bot: &mut Bot, needle: &str) -> bool {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if bot.received().iter().any(
            |message| matches!(message, ServerMessage::Chat { text, .. } if text.contains(needle)),
        ) {
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
fn chat_works_with_no_mods_loaded_at_all() {
    // **The engine-native half.** Criterion 1 says a world with the reference
    // UI mod deleted still has chat; this is the strongest version of that —
    // no mods whatsoever.
    let server = start("bare", None);
    block_on(async {
        let mut speaker = join(&server, "Speaker").await;
        let mut listener = join(&server, "Listener").await;

        speaker.chat("hello from nowhere").await.expect("send");
        assert!(
            heard(&mut listener, "hello from nowhere").await,
            "chat did not reach another player on a server with no mods"
        );

        speaker.disconnect().await;
        listener.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_mod_can_refuse_a_line_and_tell_the_speaker_why() {
    let server = start("warden", Some(write_warden("warden")));
    block_on(async {
        let mut speaker = join(&server, "Speaker").await;
        let mut listener = join(&server, "Listener").await;

        // The refused line. The speaker is told; the listener never sees it.
        speaker.chat("please be quiet").await.expect("send");
        assert!(
            heard(&mut speaker, "not here").await,
            "the speaker was not told their line was refused"
        );

        // An allowed line AFTER it, so the listener has demonstrably been
        // pumped past the refused one — otherwise "did not arrive" passes
        // whenever the server is merely slow.
        speaker.chat("anything else").await.expect("send");
        assert!(
            heard(&mut listener, "anything else").await,
            "an allowed line did not get through"
        );
        assert!(
            !listener.received().iter().any(|message| {
                matches!(message, ServerMessage::Chat { text, .. } if text.contains("quiet"))
            }),
            "a refused line reached another player"
        );

        speaker.disconnect().await;
        listener.disconnect().await;
    });
    server.stop();
}
