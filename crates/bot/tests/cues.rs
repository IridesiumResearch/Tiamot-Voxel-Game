// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Cues and loops: the standard way anything in this engine gets a noise.
//!
//! **Registering a sound and saying when it plays are two steps**, and that is
//! the whole design. A mod raises a named cue whether or not anybody has bound
//! a sound to it; another mod binds a sound to a cue it did not raise. Neither
//! has to know the other exists, which is what makes it a system rather than a
//! habit of calling `play_sound` from every hook.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-cues-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that raises a cue when somebody joins, and a second that binds it.
///
/// Two mods on purpose: the whole claim is that the raiser and the binder do
/// not have to be the same author.
fn write_mods(name: &str) -> PathBuf {
    let root = scratch(name);

    let raiser = root.join("bell");
    std::fs::create_dir_all(&raiser).expect("mod dir");
    std::fs::write(
        raiser.join("mod.toml"),
        "id = \"bell\"\nname = \"Bell\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        raiser.join("init.lua"),
        r#"
game.register_block{ id = "ground" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), game.get_block_id("bell:ground"))
end)
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }

-- Raised with nothing bound to it at load time. Whether anybody gives it a
-- noise is somebody else's business and may be nobody's.
--
-- One handler, because the engine allows one per hook per mod and says so.
game.register_on_player_join(function(event)
    game.cue{ cue = "arrival", pos = { x = 0, y = 0, z = 0 }, radius = 64 }
    -- And a loop, so the ambience path is exercised by a real server.
    game.play_loop{ id = "hum", sound = "chime:tone", everywhere = true, gain = 0.5 }
end)
"#,
    )
    .expect("script");

    let binder = root.join("chime");
    std::fs::create_dir_all(binder.join("sounds")).expect("mod dir");
    std::fs::write(
        binder.join("mod.toml"),
        "id = \"chime\"\nname = \"Chime\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    // A real WAV, so the sound table entry has a file the content index finds.
    // Copied from the reference mods rather than synthesised here: the bot does
    // not depend on the client, and a hand-rolled header would have to satisfy
    // the ingest guard's "exactly one `fmt ` chunk" rule to be worth anything.
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../game/core_ui/sounds/click.wav");
    std::fs::copy(&source, binder.join("sounds/tone.wav")).expect("a reference sound to copy");
    std::fs::write(
        binder.join("init.lua"),
        r#"
game.register_sound{ id = "tone", file = "sounds/tone.wav" }
-- Binding to ANOTHER mod's cue, which is the point of qualifying it.
game.bind_sound("bell:arrival", "tone")
-- And to one of the engine's, which only the client can raise.
game.bind_sound("engine:jump", "tone")
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
fn one_mod_raises_a_cue_and_another_decides_what_it_sounds_like() {
    // The claim, end to end on a real server: `bell` raises `bell:arrival` and
    // has no sounds at all; `chime` binds one to it and never mentions the
    // event. What reaches the client is an ordinary `PlaySound` for the sound
    // the BINDER chose.
    let server = start("cue", write_mods("cue"));
    block_on(async {
        let mut bot = join(&server, "listener").await;

        let bindings = bot
            .received()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::SoundBindings { bindings } => Some(bindings),
                _ => None,
            })
            .expect("a binding table on join");
        assert!(
            bindings
                .iter()
                .any(|b| b.cue == "bell:arrival" && b.sound == "chime:tone"),
            "the cross-mod binding did not reach the client: {bindings:?}"
        );
        assert!(
            bindings.iter().any(|b| b.cue == "engine:jump"),
            "an engine cue must be bindable, or a jump can never make a noise"
        );

        // And the raised cue arrives as the bound sound.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if bot.received().into_iter().any(|message| {
                matches!(message, ServerMessage::PlaySound { sound, .. } if sound == "chime:tone")
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the cue never played the sound bound to it"
            );
            bot.recv().await.expect("recv");
        }
    });
    assert!(server.stop());
}

#[test]
fn a_loop_starts_everywhere_and_is_not_a_one_shot() {
    // Ambience: no position, full gain, and a lifetime rather than an instant.
    let server = start("loop", write_mods("loop"));
    block_on(async {
        let mut bot = join(&server, "listener").await;

        let deadline = tokio::time::Instant::now() + PATIENCE;
        let started = loop {
            if let Some(found) = bot
                .received()
                .into_iter()
                .find_map(|message| match message {
                    ServerMessage::StartLoop {
                        id,
                        sound,
                        everywhere,
                        ..
                    } => Some((id, sound, everywhere)),
                    _ => None,
                })
            {
                break found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the loop never started"
            );
            bot.recv().await.expect("recv");
        };
        assert_eq!(started.0, "bell:hum", "the mod's own id, qualified");
        assert_eq!(started.1, "chime:tone");
        assert!(
            started.2,
            "ambience must be `everywhere`, or it attenuates as the player walks"
        );
    });
    assert!(server.stop());
}
