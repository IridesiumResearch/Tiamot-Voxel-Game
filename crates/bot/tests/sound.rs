// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Sound events reach the players who can hear them, and nobody else.
//!
//! Task 13's delivery criterion, and the whole of what a bot can assert about
//! audio: a bot has no speakers, so whether a noise came out is the [H] half.
//! What is testable — and what actually decides whether the feature works — is
//! that the server tells the right players and not the wrong ones.
//!
//! Charter rule 2 is why the server decides at all: a client told about every
//! sound in the world could hear through walls and across a continent, and
//! would pay for the messages either way.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

/// How far the mod's sound carries. Comfortably inside a chunk, so the two
/// bots below differ by earshot rather than by whether they are loaded.
const RADIUS: f32 = 8.0;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-sound").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that registers a sound and plays it wherever it is told.
///
/// Its own fixture rather than a `game/` mod: the reference mods will make
/// noises when the audio backend lands, and a test that asserted on those would
/// break every time somebody tuned one.
fn write_noisemaker(root: &std::path::Path) -> PathBuf {
    let mods = root.join("mods");
    let dir = mods.join("noise");
    std::fs::create_dir_all(dir.join("sounds")).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"noise\"\nname = \"Noise\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    // A real file, so the content pipeline has something to hash. Not valid
    // audio — nothing in this test decodes it, and a client that tried would be
    // exercising the decoder rather than the delivery.
    std::fs::write(dir.join("sounds/thud.ogg"), b"not really an ogg").expect("sound file");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "game.register_sound{{ id = \"near\", file = \"sounds/thud.ogg\", gain = 0.5 }}\n\
             game.register_sound{{ id = \"far\", file = \"sounds/thud.ogg\" }}\n\
             -- **Two sounds, and the DISTANCE is the mod's to decide.**\n\
             -- An earlier version walked one bot away and played one sound.\n\
             -- It passed with the radius check deleted, because `move_to` is\n\
             -- an intent rather than a teleport and the bot was still standing\n\
             -- next to the other one: nobody was out of earshot and the far\n\
             -- assertion was vacuous.\n\
             --\n\
             -- So neither bot moves. One sound is played where any spawn can\n\
             -- hear it, the other a hundred thousand blocks away, and a client\n\
             -- that hears the second is not applying the radius at all.\n\
             --\n\
             -- Every tick, because firing once races the joins.\n\
             game.register_on_tick(function()\n\
               game.play_sound{{ sound = \"near\", pos = {{ x = 0, y = 0, z = 0 }}, radius = 512 }}\n\
               game.play_sound{{ sound = \"far\", pos = {{ x = 100000, y = 0, z = 100000 }}, radius = {RADIUS} }}\n\
             end)\n"
        ),
    )
    .expect("init.lua");
    mods
}

fn start(name: &str) -> ServerHandle {
    let root = scratch(name);
    let mods = write_noisemaker(&root);
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: root,
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(3),
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

async fn join_at(server: &ServerHandle, name: &str, x: f32, z: f32) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join(name).await.expect("join");
    bot.move_to(x, 0.0, z).await.expect("walk");
    bot.sleep_ticks(4).await;
    bot
}

async fn pump(bot: &mut Bot, ticks: u32) {
    for _ in 0..ticks {
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.recv()).await;
    }
}

#[test]
fn a_sound_reaches_the_player_in_earshot_and_not_the_one_outside_it() {
    let server = start("earshot");
    block_on(async {
        let mut bot = join_at(&server, "Listener", 1.0, 1.0).await;
        pump(&mut bot, 60).await;

        let heard = bot.sounds_heard();
        assert!(
            heard.iter().any(|(sound, _)| sound == "noise:near"),
            "a sound played within 512 blocks of spawn was not delivered: {heard:?}"
        );
        assert!(
            !heard.iter().any(|(sound, _)| sound == "noise:far"),
            "a sound played a hundred thousand blocks away, with an eight-block \
             radius, was delivered anyway: {heard:?} — the radius is not being \
             applied, so every client hears everything in the world"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_registered_sound_reaches_the_client_with_its_file_and_its_gain() {
    // The table half: a client cannot play what it was never told about, and
    // the file travels by hash through the same content pipeline a texture
    // does. Charter rule 1 — the engine has no sounds of its own.
    let server = start("sound-table");
    block_on(async {
        let bot = join_at(&server, "Listener", 1.0, 1.0).await;
        let table = bot
            .sound_table()
            .expect("the server should have sent a sound table");

        let near = table
            .iter()
            .find(|sound| sound.id == "noise:near")
            .expect("the mod's sound should be in the table");
        assert!(
            near.file.is_some(),
            "the sound's file was not hashed into the content pipeline, so a \
             client has nothing to fetch"
        );
        assert!(
            (near.gain - 0.5).abs() < f32::EPSILON,
            "the mod's gain did not survive: {}",
            near.gain
        );

        bot.disconnect().await;
    });
    server.stop();
}
