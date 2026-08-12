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
use client::config::{Config, LightingMode, RenderMode};
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

/// The same, with a view distance a player would actually use.
///
/// `embedded` uses `MINIMUM` so tests stream a handful of chunks instead of a
/// thousand. That is the right default for speed and the wrong one for anything
/// about RESIDENCY: at a radius of one chunk a player is always at the edge of
/// what has arrived, so "the client does not have that chunk" is true by
/// construction rather than by bug.
fn embedded_with_view(name: &str, view: ViewDistance) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 1,
        allowlist: Allowlist::open(),
        view_distance: view,
        mods_path: Some(reference_mods()),
        seed: Some(7),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("the embedded server must start")
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
///
/// # Why this is not a hue test any more
///
/// It used to ask whether any pixel had red close to blue, on the reasoning that
/// the sky is the clear colour and nothing else is that blue. **That reasoning is
/// wrong, and it cost a whole investigation.** Three tests in this file failed
/// intermittently with "the frame is entirely sky", about one run in nine, and
/// the diagnostic added to chase it printed `5 meshed, 1 pending, 3 drawn` —
/// geometry WAS drawn. The frame was never empty. Its surfaces were simply blue:
/// a surface with little stored sunlight falls to the shader's ambient floor,
/// and that floor carries the sky's own hue so that a cave stays legible rather
/// than going grey. Newly streamed chunks pass through exactly that state while
/// the server's flood settles across their neighbours, and in mode 3 a fully
/// shadowed floor takes the sky's colour permanently and by design (`8ff08a3`).
///
/// So: a frame that drew nothing is the clear colour EVERYWHERE, whatever the
/// lighting then does to it, and a frame that varies has geometry in it. The same
/// conclusion `screenshot.rs`'s mode matrix reached for the same reason on the
/// same day. This is stricter where it counts — a uniform frame fails even if it
/// happens to be the right hue — and no longer asks a question about colour that
/// the renderer is entitled to answer either way.
fn shows_a_world(frame: &client::texture::Image) -> bool {
    let mut lowest = [f32::MAX; 3];
    let mut highest = [f32::MIN; 3];
    for y in (0..frame.height).step_by(4) {
        for x in (0..frame.width).step_by(4) {
            let Some(pixel) = frame.pixel(x, y) else {
                continue;
            };
            for channel in 0..3 {
                let value = f32::from(pixel[channel]);
                lowest[channel] = lowest[channel].min(value);
                highest[channel] = highest[channel].max(value);
            }
        }
    }
    // Sixteen of 255, comfortably above a driver's dithering and far below the
    // difference between a lit surface and the sky it stands against.
    (0..3).any(|channel| highest[channel] - lowest[channel] > 16.0)
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

    // **The check checks itself first.** A "did the world draw" test that cannot
    // fail is worse than none, and the heuristic this file used before was
    // replaced precisely because it answered the wrong question — so the
    // replacement is made to say NO about a frame that genuinely has no world in
    // it, on this machine, this driver and this frame size, before it is trusted
    // to say yes about one that does.
    //
    // Straight up: nothing is above the player at spawn, so the frame is the
    // clear colour and nothing else. `look_down_by` takes a downward angle, so a
    // negative one aims at the sky.
    app.look_down_by(-1.5);
    let upward = *app.camera();
    let sky_only = target.capture(app.renderer(), &upward).expect("capture");
    assert!(
        !shows_a_world(&sky_only),
        "looking straight up at an empty sky still reads as a world, so this test cannot fail \
         and proves nothing"
    );
    app.look_down_by(0.0);

    assert!(
        shows_a_world(&frame),
        "the frame is entirely sky; the client joined but drew nothing. \
         camera {:?} pitch {:.2} yaw {:.2}, {} meshed, {} pending, {} drawn, predicting {}",
        camera.position,
        camera.pitch,
        camera.yaw,
        app.meshed_chunks(),
        app.pending_chunks(),
        app.renderer().drawn(),
        app.predicting(),
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
fn a_player_can_dig_and_then_build_with_what_they_dug() {
    // The whole loop through the CLIENT rather than a bot: aim with the real
    // raycast, dig what the crosshair is on, and place it back against a face.
    // Until this landed none of it was reachable from the window at all — left
    // click only grabbed the cursor, so digging had never once been done by a
    // person.
    //
    // The pairing is the point. Digging alone leaves a hole and proves nothing
    // about placement; placing alone cannot happen, because there is nothing to
    // place until something has been dug.
    let Some(gpu) = gpu() else { return };
    let server = embedded("dig-and-build");
    let mut app = client("dig-and-build", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // Aim down at a slant rather than straight down. Straight down finds the
    // cell the player is standing ON: digging it drops them into the hole, and
    // the placement that follows is then refused for being inside a player —
    // which is the rule working correctly and the test staging it wrong. A 45°
    // slant lands about five cells away, clear of a body 1.8 cells wide.
    app.look_down_by(std::f32::consts::FRAC_PI_4);
    assert!(
        run_frames(&mut app, |app| app.looking_at().is_some()),
        "the crosshair found nothing within reach even pointing straight down"
    );

    let target = app.dig_target().expect("something to dig");
    // Its own loop rather than `run_frames`, which hands out a shared borrow —
    // and digging is a thing the frame does, not a thing it observes. Re-aimed
    // every frame exactly as `main.rs` does while the button is held.
    let mut dug = false;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && !dug {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        dug = !app.carried().is_empty();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        dug,
        "digging the ground never credited anything; warnings: {:?}",
        app.warnings()
    );
    app.stop_digging();

    let carried = app.carried()[0];
    assert!(carried.1 > 0, "credited an empty stack: {carried:?}");

    // And build with it. Aim at a face rather than the hole just made — the
    // ground beside it will do — and place.
    assert!(
        run_frames(&mut app, |app| app.place_target().is_some()),
        "nothing to place against after digging"
    );
    let placed_at = app.place_target().expect("a face to build on");
    app.place();

    assert!(
        run_frames(&mut app, |app| {
            app.store()
                .get(placed_at.chunk())
                .and_then(|chunk| chunk.get_subnode(placed_at))
                .is_some_and(|material| !material.is_air())
        }),
        "the placed block never appeared at {placed_at:?}; dug {target:?}, carried \
         {carried:?}, warnings: {:?}",
        app.warnings()
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_selection_outlines_the_real_shape_of_a_chiselled_block() {
    // The task asks for an outline "honouring Partial occupancy — outline the
    // actual occupied sub-node cells". The easy version draws a cube whatever
    // is there, which is a lie precisely where the player is looking, and looks
    // right in every screenshot of un-chiselled terrain.
    //
    // So: outline a solid block, chisel a cell out of it, and assert the
    // outline lost exactly that cell.
    let Some(gpu) = gpu() else { return };
    let server = embedded("selection");
    let mut app = client("selection", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    app.look_down_by(std::f32::consts::FRAC_PI_4);
    assert!(
        run_frames(&mut app, |app| !app.selection().is_empty()),
        "nothing was outlined even with ground in view"
    );

    // A whole-block brush on solid ground outlines all 27 cells.
    let solid = app.selection();
    assert_eq!(
        solid.len(),
        27,
        "a solid block should outline all 27 of its cells, got {}",
        solid.len()
    );

    // Chisel one cell out of it and look again. The cell removed is the one
    // the crosshair is on, so it must be the one that leaves the outline.
    let removed = app.dig_target().expect("something to aim at");
    app.select_subnode_tool();
    let deadline = Instant::now() + PATIENCE;
    let mut gone = false;
    while Instant::now() < deadline && !gone {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        gone = app
            .store()
            .get(removed.chunk())
            .and_then(|chunk| chunk.get_subnode(removed))
            .is_some_and(tiamot_core::MaterialId::is_air);
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(gone, "the chisel never removed {removed:?}");
    app.stop_digging();

    // Back to a whole-block brush, and the outline must now be the real shape.
    app.select_block_tool();
    app.advance(Input::default(), 1.0 / 60.0);
    let chiselled = app.selection();
    assert!(
        !chiselled.contains(&removed),
        "the outline still includes {removed:?}, which was chiselled away — it is drawing \
         the cube the block used to be"
    );
    assert_eq!(
        chiselled.len(),
        26,
        "one cell was removed, so 26 should remain; got {}",
        chiselled.len()
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

#[test]
fn the_lighting_mode_switches_without_a_restart() {
    // Task 10's criterion, on the real client: no restart, no reconnection, no
    // pipeline rebuilt — and the world rebuilt, because light is baked into
    // vertices at mesh time and the geometry a mode shows was built for it.
    let Some(gpu) = gpu() else {
        eprintln!("no adapter; skipping");
        return;
    };
    let server = embedded("lighting-switch");
    let mut app = client("lighting-switch", &server, gpu);

    assert!(
        run_frames(&mut app, |app| {
            app.store().len() > 8 && app.pending_chunks() == 0
        }),
        "the world never finished meshing: {:?}",
        app.warnings()
    );

    assert_eq!(app.lighting_mode(), LightingMode::Classic, "the default");

    // Every mode in turn, and home again. Walking the whole cycle rather than
    // checking one switch is what catches a mode that was added to the enum and
    // not to everything that matches on it.
    for expected in [
        LightingMode::Beautiful,
        LightingMode::Simple,
        LightingMode::Classic,
    ] {
        app.cycle_lighting_mode();

        assert_eq!(app.lighting_mode(), expected, "the switch did not take");
        assert_eq!(
            app.renderer().lighting_mode(),
            expected,
            "the renderer is still drawing the old mode, so the switch is invisible"
        );
        // Against what is held NOW, not against a count taken before the
        // switch. Chunks keep arriving while this runs, and CI caught the
        // difference: 15 held against 10 counted a few frames earlier, which
        // is the network working rather than the switch failing.
        assert_eq!(
            app.pending_chunks(),
            app.store().len(),
            "switching to {expected:?} left part of the world meshed for the mode it is no \
             longer in"
        );

        // And it draws. A mode that switches cleanly and then renders nothing
        // is the failure this whole test would otherwise miss.
        assert!(
            run_frames(&mut app, |app| app.pending_chunks() == 0),
            "the world never re-meshed after switching to {expected:?}"
        );
        let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
        let camera = *app.camera();
        let frame = target
            .capture(app.renderer(), &camera)
            .expect("a frame in the new mode");
        assert!(
            shows_a_world(&frame),
            "{expected:?} drew an empty sky where the world used to be"
        );
    }

    assert!(server.stop());
}

#[test]
fn third_person_puts_a_body_in_the_frame_that_first_person_does_not() {
    // The debug view, end to end. There is no player model until Task 12, so
    // what third person shows is the collision box — and the reason it exists
    // is that a world of static terrain has no moving shadow to judge the
    // cascades by.
    let Some(gpu) = gpu() else {
        eprintln!("no adapter; skipping");
        return;
    };
    let server = embedded("third-person");
    let mut app = client("third-person", &server, gpu);

    assert!(
        run_frames(&mut app, |app| app.joined() && app.pending_chunks() == 0),
        "the world never finished meshing: {:?}",
        app.warnings()
    );
    // Look level, so the body is in front of the camera rather than under it.
    app.advance(Input::default(), 1.0 / 60.0);

    let target = Offscreen::new(app.renderer().gpu(), WIDTH, HEIGHT);
    let camera = *app.camera();
    let first = target.capture(app.renderer(), &camera).expect("capture");

    assert!(!app.is_third_person(), "first person is the default");
    app.toggle_third_person();
    assert!(app.is_third_person());
    // One frame for the camera to move and the body to be placed.
    app.advance(Input::default(), 1.0 / 60.0);

    let camera = *app.camera();
    let third = target.capture(app.renderer(), &camera).expect("capture");

    // Counted pixels rather than the perceptual hash: the hash averages a frame
    // into a 16x16 grid so that filtering differences cannot move it, and a
    // camera stepping back four blocks over a flat white world does not move it
    // either. The hash answers "did the world stop drawing"; this question is
    // "did anything change at all".
    let differing = (0..HEIGHT)
        .step_by(2)
        .flat_map(|y| (0..WIDTH).step_by(2).map(move |x| (x, y)))
        .filter(|(x, y)| first.pixel(*x, *y) != third.pixel(*x, *y))
        .count();
    assert!(
        differing > 8,
        "only {differing} sampled pixels changed, so neither the camera nor the body moved"
    );
    // Ground in the frame, not the horizon: a level camera over a flat plain
    // sees mostly distant terrain, and distant terrain is fogged to the sky's
    // own colour on purpose — so "is there a world here" cannot be asked of it.
    app.look_down_by(1.0);
    app.advance(Input::default(), 1.0 / 60.0);
    let camera = *app.camera();
    let looking_down = target.capture(app.renderer(), &camera).expect("capture");
    assert!(
        shows_a_world(&looking_down),
        "third person drew an empty sky with the camera pointed at the ground, so it went \
         somewhere the world is not"
    );

    // And back, because a view you cannot leave is a trap rather than a tool.
    app.toggle_third_person();
    app.advance(Input::default(), 1.0 / 60.0);
    assert!(!app.is_third_person());

    assert!(server.stop());
}

#[test]
fn a_scrubbed_clock_survives_the_servers_next_update() {
    // The one thing a local time override has to do. The server broadcasts the
    // time once a second, so an override that did not hold would be undone
    // between one look at the sky and the next — and the symptom would be "the
    // key does nothing", because a second is about how long it takes to notice
    // anything.
    let Some(gpu) = gpu() else {
        eprintln!("no adapter; skipping");
        return;
    };
    let server = embedded("clock");
    let mut app = client("clock", &server, gpu);

    assert!(
        run_frames(&mut app, |app| app.joined()),
        "never joined: {:?}",
        app.warnings()
    );

    app.nudge_time(0.5);
    assert!(app.time_is_local(), "scrubbing should take the clock over");
    let scrubbed = app.sky_time();

    // Long enough for at least one `TimeOfDay` from the server, which sends one
    // every twenty ticks.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.advance(Input::default(), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }

    // The local clock keeps RUNNING — it is a scrub, not a freeze — so what
    // must not have happened is a jump back to the server's time.
    let drift = (app.sky_time() - scrubbed).rem_euclid(1.0);
    assert!(
        drift < 0.1,
        "the clock jumped from {scrubbed} to {} , so the server's update overrode the scrub",
        app.sky_time()
    );

    app.resync_time();
    assert!(!app.time_is_local(), "resyncing should give the clock back");

    assert!(server.stop());
}

#[test]
fn the_debug_row_offers_one_of_every_material_and_nothing_when_aimed_at_sky() {
    // The singleplayer affordance for looking at blocks you have not mined —
    // lamps, most of all, which a player can otherwise only get by finding one.
    // Asserted through the list it produces rather than by writing it, because
    // what matters is that it names every material exactly once and never air.
    let Some(gpu) = gpu() else {
        eprintln!("no adapter; skipping");
        return;
    };
    let server = embedded("debug-row");
    let mut app = client("debug-row", &server, gpu);

    // Before the world arrives there is nothing to aim at, and the row is
    // empty rather than a guess. The same path covers looking at the sky.
    assert!(
        app.debug_material_row().is_empty(),
        "a row was laid out before the player had a world to stand in"
    );

    assert!(
        run_frames(&mut app, |app| app.joined() && app.pending_chunks() == 0),
        "the world never finished meshing: {:?}",
        app.warnings()
    );

    // Looking at the ground: one block per material, each at its own position,
    // and none of them air or the unknown placeholder.
    app.look_down_by(1.2);
    app.advance(Input::default(), 1.0 / 60.0);
    let row = app.debug_material_row();
    assert!(
        row.len() >= 2,
        "the reference mods register more than one block, so a row of {} is short",
        row.len()
    );

    let mut ids: Vec<u16> = row.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    let unique = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), unique, "the row repeats a material");
    assert!(
        !ids.contains(&tiamot_core::MaterialId::AIR.0),
        "the row includes air, which is a hole rather than a block"
    );
    assert!(
        !ids.contains(&tiamot_core::MaterialId::UNKNOWN.0),
        "the row includes the unknown-block placeholder, which is nothing a player wants a \
         sample of"
    );

    let mut positions: Vec<(i32, i32, i32)> =
        row.iter().map(|(pos, _)| (pos.x, pos.y, pos.z)).collect();
    let placed = positions.len();
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions.len(),
        placed,
        "two materials were laid out on the same block"
    );

    assert!(server.stop());
}

#[test]
fn holding_the_jump_key_gives_exactly_one_hop() {
    // **Requested from the window: "make it so only one hop per key press is
    // done."** Holding the key used to jump again the instant the body touched
    // down, which in a tunnel with a sub-node of headroom is a hop every three
    // ticks — felt as bouncing rather than as jumping.
    //
    // Driven through `advance` with the key HELD, so what is under test is the
    // edge detection rather than a caller politely sending one frame of jump.
    let Some(gpu) = gpu() else { return };
    let server = embedded("one-hop");
    let mut app = client("one-hop", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // Settled on the ground first, or the first jump is measured against a body
    // that is still falling to it.
    for _ in 0..40 {
        app.pump_network();
        app.advance(Input::default(), 1.0 / 60.0);
    }
    let resting = app.camera().position.to_world().1;

    let held = Input {
        jump: true,
        ..Input::default()
    };

    // Two seconds of holding it. One hop rises about 1.25 blocks and takes well
    // under a second, so a key that re-fires would show several separate rises.
    let mut peaks = 0;
    let mut airborne = false;
    for _ in 0..120 {
        app.pump_network();
        app.advance(held, 1.0 / 60.0);
        let height = app.camera().position.to_world().1 - resting;
        if height > 0.35 && !airborne {
            airborne = true;
            peaks += 1;
        } else if height < 0.1 {
            airborne = false;
        }
    }
    assert_eq!(
        peaks, 1,
        "holding jump produced {peaks} hops; one press is one hop"
    );

    // And releasing lets the next press through, or the key would work once per
    // session.
    for _ in 0..40 {
        app.pump_network();
        app.advance(Input::default(), 1.0 / 60.0);
    }
    let mut hopped_again = false;
    for _ in 0..60 {
        app.pump_network();
        app.advance(held, 1.0 / 60.0);
        if app.camera().position.to_world().1 - resting > 0.35 {
            hopped_again = true;
        }
    }
    assert!(
        hopped_again,
        "a fresh press after releasing did not jump, so the edge never re-armed"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn a_press_is_sent_for_more_than_one_tick_but_is_still_one_hop() {
    // **The fragility a single-tick edge has, and the ceiling on the cure.**
    //
    // `InputQueue::offer` refuses an input whose tick the server has already
    // passed, so one late packet lost a whole jump while the client had taken it
    // — reported as `worst correction 5.37 cells` at a landing, which is a jump's
    // arc. The press is now sent for `JUMP_EDGE_TICKS` ticks so losing one packet
    // costs nothing.
    //
    // The copies are only safe because a jump is honoured from the ground alone,
    // so this asserts the thing that would break if the window ever grew: one
    // press is still ONE hop. Held for a long time, too — the edge must not
    // re-arm itself.
    let Some(gpu) = gpu() else { return };
    let server = embedded("jump-edge");
    let mut app = client("jump-edge", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    for _ in 0..40 {
        app.pump_network();
        app.advance(Input::default(), 1.0 / 60.0);
    }
    let resting = app.camera().position.to_world().1;

    let held = Input {
        jump: true,
        ..Input::default()
    };
    let mut peaks = 0;
    let mut airborne = false;
    for _ in 0..140 {
        app.pump_network();
        app.advance(held, 1.0 / 60.0);
        let height = app.camera().position.to_world().1 - resting;
        if height > 0.35 && !airborne {
            airborne = true;
            peaks += 1;
        } else if height < 0.1 {
            airborne = false;
        }
    }
    assert_eq!(
        peaks, 1,
        "a press held for two seconds produced {peaks} hops; the redundancy window has grown \
         past the shortest airtime, or the edge is re-arming itself"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_hud_counts_footing_changes_and_a_still_player_has_none() {
    // **The instrument for "I jolt around".** A body walking over even ground is
    // on it every tick; a body falling is off it every tick; a body ALTERNATING
    // is one whose support is flickering, and no amount of camera smoothing makes
    // that feel right. The count distinguishes those, and it counts the
    // simulation's answer rather than anything about the picture.
    //
    // Tested from both ends, because a counter that only ever reads zero is
    // indistinguishable from one that is not wired up — which is the failure mode
    // every readout on this HUD has to be defended against.
    let Some(gpu) = gpu() else { return };
    let server = embedded("footing");
    let mut app = client("footing", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // Settle, then let a whole reporting window pass while standing still.
    let settle = |app: &mut App, seconds: f32| {
        let frames = (seconds * 60.0) as usize;
        for _ in 0..frames {
            app.pump_network();
            app.advance(Input::default(), 1.0 / 60.0);
        }
    };
    settle(&mut app, 1.5);
    settle(&mut app, 1.2);

    assert_eq!(
        app.pacing().footing_changes_last_second(),
        0,
        "a player standing still on flat ground changed footing; if that is real it is a bug, \
         and if it is not the counter is wrong"
    );

    // And a jump is a change of footing in each direction, so the count must
    // notice it. One press, held or not, is one hop.
    let jump = Input {
        jump: true,
        ..Input::default()
    };
    for _ in 0..3 {
        app.pump_network();
        app.advance(jump, 1.0 / 60.0);
    }
    settle(&mut app, 1.4);

    assert!(
        app.pacing().footing_changes_last_second() > 0,
        "a jump left the ground and landed again without the counter noticing, so it cannot \
         report jolting either"
    );

    app.shutdown();
    assert!(server.stop());
}

/// Runs frames in REAL time, so the client and the server advance together.
///
/// **The distinction this exists for.** `advance` takes the frame's `dt`, so a
/// loop calling it with 1/60 as fast as the CPU allows fast-forwards the client
/// through simulated time while the server's own thread ticks at 20 Hz on the
/// clock. Measured: a test doing that reached `tick 75` while the server had
/// confirmed `tick 3` — a lead of 72 against an `INPUT_LEAD` of 4, with every
/// input past `MAX_LOOKAHEAD` refused.
///
/// Anything about TIMING — corrections, tick alignment, input lateness — is
/// untestable that way, and reads as passing because the two sides are never in
/// the same conversation. Sleeping is what makes them contemporaries.
fn run_real_time(app: &mut App, seconds: f32, input: Input) {
    let frame = Duration::from_millis(16);
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut last = Instant::now();
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        let now = Instant::now();
        app.advance(input, now.duration_since(last).as_secs_f32());
        last = now;
        std::thread::sleep(frame);
    }
}

#[test]
fn jumping_in_real_time_leaves_no_correction_behind() {
    // **Reported from the window: `worst correction 5.79 cells (89% vertical)` at
    // a landing, intermittently, with the tick lead steady at 5.**
    //
    // Measured against the two things that could produce that magnitude: one tick
    // of jump offset is worth 1.34 cells (`phys::input`'s
    // `a_jump_applied_a_tick_late_costs_a_whole_arc_by_the_landing`), so five or
    // six cells is four or five ticks — which is the input lead, not a lost press.
    //
    // Run in real time, because the first version of this test ran a tight loop
    // and reached a lead of 72 against the server's 4: a client that far ahead has
    // every input refused and is not in the conversation this test is about.
    let Some(gpu) = gpu() else { return };
    let server = embedded("real-time-jump");
    let mut app = client("real-time-jump", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // Settle, and let the lead reach its steady state.
    run_real_time(&mut app, 1.5, Input::default());
    let (tick, confirmed) = app.tick_pair();
    let lead = tick as i64 - confirmed as i64;
    assert!(
        (1..=16).contains(&lead),
        "the lead is {lead} ticks ({tick} against {confirmed}), so this test is not measuring \
         what it claims — see `run_real_time`"
    );

    // Jump, then keep running while it rises, falls and lands.
    let jump = Input {
        jump: true,
        ..Input::default()
    };
    run_real_time(&mut app, 0.1, jump);
    run_real_time(&mut app, 2.5, Input::default());

    let correction = app.pacing().worst_correction_cells();
    assert!(
        correction < 1.0,
        "a jump in real time left a correction of {correction} cells ({}% vertical) with the \
         lead at {lead}",
        app.pacing().worst_correction_vertical_percent()
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_divergence_measure_and_its_trace_report_a_real_session() {
    // **The tool for a disagreement that will not reproduce.** A correction is
    // the gap between the newest prediction and a replay, so it grows with the
    // input lead and cannot say WHICH tick went wrong. This compares one tick's
    // two answers — the client's memory of what it predicted, against the
    // server's word about that same tick.
    //
    // Asserted from both ends, because an instrument that only ever reads zero is
    // indistinguishable from one that is not wired up: on a quiet loopback
    // session the two must agree, and the trace must still have written the lines
    // that say so.
    let Some(gpu) = gpu() else { return };
    let trace_path = scratch("divergence-trace").join("physics.log");
    // SAFETY: single-threaded test setup, before the client is built.
    unsafe { std::env::set_var("TIAMOT_TRACE_PHYSICS", &trace_path) };

    let server = embedded("divergence");
    let mut app = client("divergence", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // Real time, so the client and server are contemporaries — see
    // `run_real_time`. Walk and jump, so the trace has something to describe.
    run_real_time(&mut app, 1.5, Input::default());
    run_real_time(
        &mut app,
        0.1,
        Input {
            jump: true,
            ..Input::default()
        },
    );
    run_real_time(
        &mut app,
        2.0,
        Input {
            forward: 1.0,
            ..Input::default()
        },
    );

    // **What this may and may not assert.**
    //
    // Not that the two agree. The first version required under a cell and macOS
    // CI answered 1.11 — a real disagreement, on a loaded runner with irregular
    // frame timing, and about one tick of drift at the speed the body was going.
    // That is the very thing under investigation (the window reports five or six
    // cells, which is four or five ticks of the same), so asserting its absence
    // would be asserting a bug away and would leave the branch red for as long as
    // it took to fix.
    //
    // What it CAN assert is that the comparison compares like with like. A
    // measurement that had lost its frame — a stale origin, a mismatched tick —
    // reports a whole chunk, because that is what 48 cells of anchor error looks
    // like. Anything under a block means the two are describing the same place
    // and differing about it, which is a finding rather than a broken instrument.
    let diverged = app.pacing().worst_divergence_cells();
    println!("worst per-tick divergence: {diverged} cells");
    assert!(
        diverged < tiamot_core::SUBNODES_PER_AXIS as f32,
        "the two answers for one tick are {diverged} cells apart — more than a block, which is \
         an instrument comparing different frames rather than a simulation disagreeing"
    );

    app.shutdown();
    assert!(server.stop());

    // SAFETY: single-threaded teardown.
    unsafe { std::env::remove_var("TIAMOT_TRACE_PHYSICS") };

    let written = std::fs::read_to_string(&trace_path).expect("the trace file should exist");
    let lines = written.lines().count();
    assert!(
        lines > 10,
        "the trace wrote {lines} lines for three and a half seconds of play, so it is not \
         recording the ticks it claims to"
    );
    let first = written.lines().next().unwrap_or_default();
    for field in ["tick ", "dist ", "dv ", "footing_agreed "] {
        assert!(
            first.contains(field),
            "the trace line is missing `{field}`, so it cannot answer what it exists for: \
             {first}"
        );
    }
}

#[test]
fn walking_across_chunk_boundaries_never_touches_a_chunk_it_does_not_have() {
    // **Reported from the window with the border overlay on: "if I run into a
    // chunk corner, I often glitch ... if I am within a chunk I am completely
    // fine."**
    //
    // The core adapter is not the problem — `solid()` agrees with the world on
    // every cell across a corner where four chunks meet, when all four are
    // resident. So the question is residency, and this is the pair of numbers
    // that answers it: whether prediction ever consulted a chunk the client does
    // not have, and whether the two simulations then disagreed.
    //
    // Real time, because a client racing ahead of its server is not walking
    // across a boundary in any sense the server would recognise.
    let Some(gpu) = gpu() else { return };
    let server = embedded_with_view("boundaries", ViewDistance::DEFAULT);
    let mut app = client("boundaries", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    run_real_time(&mut app, 1.0, Input::default());

    let start = app.camera().position.chunk;
    // Diagonally, so corners are crossed rather than faces.
    let diagonally = Input {
        forward: 1.0,
        right: 1.0,
        sprint: true,
        ..Input::default()
    };
    run_real_time(&mut app, 6.0, diagonally);
    let finished = app.camera().position.chunk;

    assert_ne!(
        (start.x, start.z),
        (finished.x, finished.z),
        "never left the chunk it started in, so no boundary was crossed: {start:?}"
    );
    assert!(
        !app.pacing().predicted_into_unloaded(),
        "prediction collided against a chunk the client does not have while crossing a \
         boundary — an invisible wall at the seam, which is exactly the report"
    );
    let diverged = app.pacing().worst_divergence_cells();
    assert!(
        diverged < 1.0,
        "the two simulations disagreed by {diverged} cells while crossing a chunk boundary"
    );

    app.shutdown();
    assert!(server.stop());
}
