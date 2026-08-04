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
    // This test used to assert the opposite — that the frame CHANGED, because
    // the world was left behind while the camera jumped. That is what the
    // client did, and it was wrong: the world ended up 50,000 blocks outside a
    // 1,000-block far plane, so "the picture changed" meant "the picture is now
    // empty sky", and the human gate it exists to support could only ever
    // report seeing nothing. The world now moves with the camera by the same
    // whole number of chunks, so the frame must be UNCHANGED — that is the
    // claim floating origin actually makes — and must still contain a world.
    let Some(gpu) = gpu() else { return };
    let server = embedded("teleport");
    let mut app = client("teleport", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));

    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let home_chunk = camera.position.chunk;
    let before = target.capture(app.renderer(), &camera).expect("capture");

    app.teleport(Teleport::Far);
    let camera = *app.camera();
    let away = target.capture(app.renderer(), &camera).expect("capture");

    assert_eq!(
        camera.position.chunk.x - home_chunk.x,
        client::app::TELEPORT_CHUNKS,
        "the camera should have moved 50,000 blocks east"
    );
    assert!(
        shows_a_world(&away),
        "the frame at 50,000 blocks out is empty sky, so this proves nothing about jitter — \
         the world did not come along"
    );
    assert_eq!(
        perceptual_hash(&before),
        perceptual_hash(&away),
        "the picture changed at the edge of the world; something in the render path is \
         accumulating a world-space f32"
    );

    // Idempotent: the displacement is absolute, so a second press is a no-op
    // rather than another 50,000 blocks.
    app.teleport(Teleport::Far);
    assert_eq!(
        app.camera().position.chunk,
        camera.position.chunk,
        "a second teleport moved again; the displacement is meant to be absolute"
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

#[test]
fn the_teleport_survives_the_frame_after_it() {
    // Its own test rather than another assertion on the one above, because
    // proving this needs a frame to be ADVANCED, and advancing one lets the
    // body settle — which moves the camera and breaks that test's
    // pixel-for-pixel comparisons for a reason that has nothing to do with what
    // it is checking.
    //
    // The regression: `advance` re-derives the camera from the predicted body
    // every frame, and did so without the debug teleport's displacement. The
    // world stayed 50,000 blocks out while the camera was dragged home, so one
    // frame after the jump the screen was empty sky — the exact failure mode
    // `e3594b1` fixed for Task 08, re-introduced by the walking controller that
    // replaced free-fly. Nothing caught it because the teleport test captures
    // its frames the instant it jumps and never runs another one.
    let Some(gpu) = gpu() else { return };
    let server = embedded("teleport-frame");
    let mut app = client("teleport-frame", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));
    assert!(
        app.predicting(),
        "there is no predicted body, so `advance` would not re-derive the camera at all and \
         this test could not see the bug it is about"
    );

    app.teleport(Teleport::Far);
    let displaced = app.camera().position.chunk;

    for _ in 0..5 {
        app.pump_network();
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        assert_eq!(
            app.camera().position.chunk.x,
            displaced.x,
            "the camera came back from {} to {} while the world stayed displaced",
            displaced.x,
            app.camera().position.chunk.x
        );
    }

    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let frame = target.capture(app.renderer(), &camera).expect("capture");
    assert!(
        shows_a_world(&frame),
        "five frames after the teleport the screen is empty sky"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn a_stall_does_not_leave_the_player_unable_to_move() {
    // The reported symptom — jitter with no movement — reproduced by the thing
    // a real client does and a headless test never does: STALL.
    //
    // The client's tick was a free-running counter, and `walk` discards the
    // backlog after a long frame so the count does not fast-forward the player.
    // That means a stall permanently LOSES ticks. The server refuses any input
    // whose tick it has already passed, so once the client falls behind, every
    // input it will ever send is refused — the player is frozen server-side
    // while the client predicts and is snapped back 20 times a second.
    //
    // A window has stalls constantly: GPU init, chunk uploads, a dragged
    // window, a missed vsync. This is one, made explicit.
    let Some(gpu) = gpu() else { return };
    let server = embedded("stall");
    let mut app = client("stall", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));

    let forward = Input {
        forward: 1.0,
        ..Input::default()
    };
    let walk_for = |app: &mut App, seconds: f32| {
        let deadline = Instant::now() + Duration::from_secs_f32(seconds);
        while Instant::now() < deadline {
            assert!(app.pump_network(), "connection ended");
            app.remesh();
            app.advance(forward, 1.0 / 60.0);
            std::thread::sleep(Duration::from_millis(16));
        }
    };

    walk_for(&mut app, 1.0);
    println!("before stall: client/server tick = {:?}", app.tick_pair());

    // The stall: two seconds in which no frame runs at all.
    std::thread::sleep(Duration::from_secs(2));

    let before = app.server_travelled();
    println!("after stall:  client/server tick = {:?}", app.tick_pair());
    walk_for(&mut app, 4.0);
    let after = app.server_travelled();
    println!("at end:       client/server tick = {:?}", app.tick_pair());

    assert!(
        after - before > 2.0,
        "after a two-second stall the player moved {:.2} blocks in four seconds of walking; \
         the client's tick fell behind the server's and every input is being refused",
        after - before
    );

    // The invariant underneath it. An input is refused outright once its tick
    // is one the server has already passed, so this is the thing that must
    // never stop being true — and it is measurable long before a player
    // notices anything wrong.
    let (client_tick, server_tick) = app.tick_pair();
    assert!(
        client_tick > server_tick,
        "the client is predicting tick {client_tick} while the server has reached \
         {server_tick}; every input it sends from here is refused"
    );
}

#[test]
fn holding_forward_actually_moves_the_player() {
    // Reported from the window: "I seem to be stuck in place. There is a lot of
    // jitter and jump when I try to move but I never leave the spawn coords."
    //
    // That is prediction fighting reconciliation. The client predicts forward,
    // the server says "still at spawn", and the correction drags it back every
    // 50 ms — which looks exactly like jitter around a fixed point.
    //
    // The cause is the tick number. The server refuses any input whose tick it
    // has already passed, and the client's tick was a free-running counter
    // seeded once at join: the join flow takes time, so it starts behind, and
    // `walk` discards the backlog after a stall so it never catches up. Every
    // input refused, forever.
    //
    // This asserts BOTH ends. Predicted movement alone would pass while the
    // server ignored every input, which is the bug.
    let Some(gpu) = gpu() else { return };
    let server = embedded("walking");
    let mut app = client("walking", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4));
    let start = app.camera().position.to_world();

    let forward = Input {
        forward: 1.0,
        ..Input::default()
    };
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        assert!(app.pump_network(), "connection ended: {:?}", app.warnings());
        app.remesh();
        app.advance(forward, 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }

    let ended = app.camera().position.to_world();
    let (dx, dz) = (ended.0 - start.0, ended.2 - start.2);
    let travelled = (dx * dx + dz * dz).sqrt();
    assert!(
        travelled > 2.0,
        "held forward for six seconds and moved {travelled:.2} blocks, from {start:?} to {ended:?}"
    );

    // And the SERVER agrees. Without this the test passes on a client that
    // predicts happily while every input it sends is refused.
    assert!(
        app.server_travelled() > 2.0,
        "the client moved but the server still has the player at spawn: {} blocks",
        app.server_travelled()
    );

    let (client_tick, server_tick) = app.tick_pair();
    assert!(
        client_tick > server_tick,
        "the client is predicting tick {client_tick} while the server has reached \
         {server_tick}; every input it sends from here is refused"
    );

    app.shutdown();
    assert!(server.stop());
}
