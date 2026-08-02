// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The whole client, minus the window.
//!
//! # Why this test is the important one
//!
//! Task 08's headline acceptance criterion — "opens a window, starts an
//! embedded server, and you fly over a white half-space" — is a human gate, and
//! there is no honest way to assert it. What there IS an honest way to assert
//! is everything underneath it: a real server started in-process, a real QUIC
//! join, real chunks decoded and meshed and uploaded, and a real frame that has
//! a world in it.
//!
//! [`App`] is deliberately free of `winit` for exactly this reason. What the
//! window adds on top is event translation and a surface — worth eyes on, and
//! not worth pretending a test covers.
//!
//! # This is singleplayer
//!
//! `ServerHandle::start` here is the same call the standalone binary makes and
//! the same one `client.toml`'s `server = "embedded"` makes. Charter rule 2:
//! there is one simulation code path, and this is it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use client::app::{App, Input, Teleport};
use client::cache::ContentCache;
use client::config::{Config, RenderMode};
use client::net::Connection;
use client::render::offscreen::perceptual_hash;
use client::render::{Gpu, Offscreen, Renderer};
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// Long enough for a cold start plus worldgen on a loaded runner.
const PATIENCE: Duration = Duration::from_secs(30);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tiamot-client-session")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// See `tests/screenshot.rs` for why a missing adapter skips rather than fails.
fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            assert!(
                std::env::var("TIAMOT_REQUIRE_GPU").is_err(),
                "TIAMOT_REQUIRE_GPU is set and no adapter was available: {err}"
            );
            println!("SKIPPING: no graphics adapter on this machine ({err})");
            None
        }
    }
}

/// Starts an embedded server exactly as `server = "embedded"` does.
fn embedded(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 1,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        seed: Some(7),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("the embedded server must start")
}

/// Builds the client the way `main.rs` does, without the window.
fn client(name: &str, server: &ServerHandle, gpu: Gpu) -> App {
    let home = scratch(&format!("{name}-home"));
    let config = Config {
        display_name: format!("Viewer-{name}"),
        ..Config::default()
    };
    let connection = Connection::open(
        server.local_addr(),
        Identity::generate().expect("identity"),
        config.display_name.clone(),
        ContentCache::open(&home.join("content")).expect("cache"),
        &home.join("known-hosts"),
    )
    .expect("connect");

    let renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");
    App::new(config, connection, renderer)
}

/// Runs frames until `done`, or gives up.
///
/// Deliberately the same sequence `main.rs` runs — pump, remesh, advance — so
/// what this exercises is the real frame loop rather than a convenient subset.
fn run_frames(app: &mut App, done: impl Fn(&App) -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        assert!(
            app.pump_network(),
            "the connection ended: {:?}",
            app.warnings()
        );
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        if done(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    false
}

/// Whether a frame has anything other than sky in it.
fn shows_a_world(frame: &client::texture::Image) -> bool {
    // The sky is the clear colour and nothing else is that blue. A frame with
    // any pixel where red is close to blue has geometry in it.
    (0..frame.height).step_by(4).any(|y| {
        (0..frame.width).step_by(4).any(|x| {
            frame
                .pixel(x, y)
                .is_some_and(|pixel| i32::from(pixel[2]) - i32::from(pixel[0]) < 20)
        })
    })
}

#[test]
fn singleplayer_joins_its_own_server_and_draws_the_world() {
    // Everything Task 08 asks for except the window: an embedded server, a real
    // join over loopback, streamed chunks, meshes on the GPU, and a frame with
    // a world in it.
    let Some(gpu) = gpu() else { return };
    let server = embedded("singleplayer");
    let mut app = client("singleplayer", &server, gpu);

    assert!(
        run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4),
        "expected to join and mesh chunks; warnings: {:?}",
        app.warnings()
    );

    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let frame = target.capture(app.renderer(), &camera).expect("capture");

    assert!(
        shows_a_world(&frame),
        "the frame is entirely sky; the client joined but drew nothing"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_hud_reports_what_the_frame_actually_contains() {
    // A HUD that reported plausible numbers rather than real ones would be
    // worse than none: it is the first thing anyone reads when something looks
    // wrong.
    let Some(gpu) = gpu() else { return };
    let server = embedded("hud");
    let mut app = client("hud", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));

    // Draw once so the culler has run and `drawn` means something.
    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let _ = target.capture(app.renderer(), &camera).expect("capture");

    let hud = app.hud();
    assert!(hud.iter().any(|line| line.contains("fps")), "{hud:?}");
    assert!(hud.iter().any(|line| line.contains("chunk ")), "{hud:?}");
    assert!(
        hud.iter().any(|line| line.contains("materials")),
        "the material count is how you tell an empty atlas from a full one: {hud:?}"
    );
    // The adapter name, so a bug report says which driver drew the picture.
    assert!(
        hud.iter().any(|line| line.contains(app.adapter())),
        "{hud:?}"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn teleporting_fifty_thousand_blocks_leaves_the_geometry_where_it_was() {
    // The [A]-assertable half of "no visible jitter at ±50,000 blocks".
    //
    // The world deliberately does NOT follow the camera, so this is the exact
    // situation the debug action creates in the real client: geometry that has
    // not moved, viewed from a position fifty thousand blocks away. The frames
    // must differ — the camera moved — and the round trip home must land back
    // on the original picture, bit for bit at the hash's resolution.
    let Some(gpu) = gpu() else { return };
    let server = embedded("teleport");
    let mut app = client("teleport", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));

    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let before = target.capture(app.renderer(), &camera).expect("capture");

    app.teleport(Teleport::Far);
    let camera = *app.camera();
    let away = target.capture(app.renderer(), &camera).expect("capture");
    assert_ne!(
        perceptual_hash(&before),
        perceptual_hash(&away),
        "teleporting fifty thousand blocks away should not leave the world in front of you"
    );

    app.teleport(Teleport::Home);
    let camera = *app.camera();
    let back = target.capture(app.renderer(), &camera).expect("capture");
    assert_eq!(
        perceptual_hash(&before),
        perceptual_hash(&back),
        "coming home did not restore the picture; the round trip through 50,000 blocks lost \
         precision, which is exactly what floating origin exists to prevent"
    );

    app.shutdown();
    assert!(server.stop());
}
