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
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
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

/// The reference mods, rather than the fixture above.
fn start_reference(name: &str) -> ServerHandle {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root");
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(repo.join("game")),
        enabled_mods: None,
        seed: Some(9),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

#[test]
fn breaking_a_block_makes_a_noise_where_the_block_was() {
    // **The end of the chain, through the shipped mods.** A player digs, the
    // mod plays a sound, the server decides who is close enough, and the client
    // is told — with the position of the BLOCK rather than of the player, which
    // is the whole reason the engine carries one.
    //
    // What it sounds like is the [H] half and this cannot ask about it.
    let server = start_reference("break-noise");
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Digger").await.expect("join");

        // A block of the reference set's plain solid, within reach.
        let solid = bot
            .material_table()
            .expect("material table")
            .into_iter()
            .find(|entry| entry.name.ends_with(":white"))
            .map(|entry| entry.id)
            .expect("the reference mods should register a solid block");
        let at = tiamot_core::BlockPos::new(2, 5, 2);
        assert!(server.seed_block(at, solid), "the world should seed");
        bot.move_to(2.0, 0.0, 4.0).await.expect("walk into reach");
        bot.sleep_ticks(4).await;

        bot.dig_block(at).await.expect("dig");
        pump(&mut bot, 20).await;

        let heard = bot.sounds_heard();
        let (sound, pos) = heard
            .iter()
            .find(|(sound, _)| sound == "core_tools:break")
            .expect("digging a block should have made a noise");
        assert_eq!(sound, "core_tools:break");

        // The middle of the block that was dug, not the player who dug it.
        // Somebody else standing nearby has to hear it from the right side.
        let expected = [
            f64::from(at.x) + 0.5,
            f64::from(at.y) + 0.5,
            f64::from(at.z) + 0.5,
        ];
        for axis in 0..3 {
            assert!(
                (pos[axis] - expected[axis]).abs() < 0.6,
                "the sound came from {pos:?} rather than the block at {expected:?}"
            );
        }

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_material_carries_the_sound_of_walking_on_it() {
    // **Criterion 1's second half: selection BY MATERIAL.** The client plays
    // its own footsteps from its own movement — no round trip, because a
    // player's own steps are the one sound whose lateness they would notice —
    // so the only way it can know what stone sounds like is the material table.
    //
    // Asserted on the table rather than on a noise, which is what a bot can
    // see. That the client then plays it is `App::play_footsteps`, and how it
    // SOUNDS is the [H] half.
    let server = start_reference("step-material");
    block_on(async {
        let bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        let mut bot = bot;
        bot.join("Walker").await.expect("join");

        let table = bot.material_table().expect("material table");
        let white = table
            .iter()
            .find(|entry| entry.name.ends_with(":white"))
            .expect("the reference mods should register a solid block");
        // `core:step`, not `core_blocks:step` — the DIRECTORY is `core_blocks`
        // and the mod's declared id is `core`. Worth pinning: a sound id built
        // from the wrong one resolves to nothing and fails silently, which is
        // exactly how this test failed the first time it was written.
        let step_id = white
            .step_sound
            .clone()
            .expect("the block's step sound did not reach the client, so nothing it walks on can make a noise");
        assert_eq!(step_id, "core:step");

        // And the sound it names is one the client was actually given, or the
        // lookup would find an id with no file behind it.
        let sounds = bot.sound_table().expect("sound table");
        let step = sounds
            .iter()
            .find(|sound| sound.id == step_id)
            .expect("the step sound should be registered");
        assert!(
            step.file.is_some(),
            "the step sound has no file, so there is nothing to decode"
        );

        // A material nobody gave a voice is silent rather than wrong — which is
        // every material until a mod says otherwise (charter rule 1).
        assert!(
            table.iter().any(|entry| entry.step_sound.is_none()),
            "every material has a step sound, so the silent case is untested"
        );

        bot.disconnect().await;
    });
    server.stop();
}
