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
        operators: Vec::new(),
        view_distance: view,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
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
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
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
    // **And with no hand in it.** The viewmodel is always on screen in first
    // person, so a frame of nothing but sky is not a frame of nothing any more
    // — and the counter-example is about whether the WORLD drew. Caught by this
    // very assertion the day the hand landed, which is what it is for.
    app.renderer().set_hands(Vec::new());
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
        // **A whole block's worth, not merely something.** A block comes apart
        // a sub-node at a time and is credited as it goes, so the first credit
        // is now ONE unit — and the placement below wants twenty-seven, because
        // the brush is a block. Stopping at "carrying anything" left this
        // trying to build a block out of a single node.
        dug = app
            .carried()
            .first()
            .is_some_and(|stack| stack.units >= tiamot_core::UNITS_PER_BLOCK);
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        dug,
        "digging the ground never credited a whole block; warnings: {:?}",
        app.warnings()
    );
    app.stop_digging();

    let carried = app.carried()[0];
    assert!(
        carried.units >= tiamot_core::UNITS_PER_BLOCK,
        "short: {carried:?}"
    );

    // **Step back out of the hole first.**
    //
    // Collecting a whole block now means digging for long enough that the hole
    // is a real hole and the body has settled into it — and from in there every
    // face within reach is one the player is standing in, which the server
    // correctly refuses to build on and does so silently. A player would take a
    // step back without thinking about it; so does this.
    for _ in 0..40 {
        assert!(app.pump_network(), "the connection ended");
        app.advance(
            Input {
                forward: -1.0,
                ..Input::default()
            },
            1.0 / 60.0,
        );
        std::thread::sleep(Duration::from_millis(8));
    }

    // And build with it. Tried until it lands rather than aimed once: which
    // face is offered depends on exactly where the body came to rest.
    let mut placed_at = None;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && placed_at.is_none() {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        if let Some(at) = app.place_target() {
            app.place();
            for _ in 0..12 {
                assert!(app.pump_network(), "the connection ended");
                app.advance(Input::default(), 1.0 / 60.0);
                std::thread::sleep(Duration::from_millis(16));
            }
            if app
                .store()
                .get(at.chunk())
                .and_then(|chunk| chunk.get_subnode(at))
                .is_some_and(|material| !material.is_air())
            {
                placed_at = Some(at);
            }
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    assert!(
        placed_at.is_some(),
        "nothing could be built after digging {target:?}, carrying {carried:?}; \
         warnings: {:?}",
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
fn a_paused_world_leaves_the_client_exactly_where_it_was() {
    // **Reported from the window, twice.** First: after the pause menu, walking
    // put the player back where they started for ever, because a paused world
    // stops the SERVER's tick and not the client's and the server refuses an
    // input too far ahead. Snapping the tick back fixed that — and left the
    // second report, which is that a correction is still a correction: for
    // about a second and a half after closing the menu the body was being
    // pulled about while it was put right.
    //
    // So the client holds still too. Nothing to correct, because nothing was
    // predicted that the server did not also simulate.
    let Some(gpu) = gpu() else { return };
    let server = embedded("paused-world");
    let mut app = client("paused-world", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    run_real_time(&mut app, 1.0, Input::default());

    // Pause both sides, exactly as the window does.
    server.set_paused(true);
    app.set_world_paused(true);
    let (before, before_confirmed) = app.tick_pair();

    // Walk into the menu. A real player's movement keys are released, but the
    // point is that nothing the client is handed can move it while the world is
    // stopped — a held key at the moment of pausing must not accumulate.
    let walking = Input {
        forward: 1.0,
        ..Input::default()
    };
    run_real_time(&mut app, 1.0, walking);

    // **How far AHEAD of the server it got, not what its counter reads.**
    //
    // The client is allowed to follow the server while paused: an input still
    // in flight when the pause landed is processed, the server's tick moves,
    // and `resync_plan` catches the client up to keep its lead. That is the
    // client AGREEING with the server, and it predicts nothing — so it costs no
    // correction, which the assertion at the end of this test is what proves.
    //
    // Asserting the counter did not move at all forbade that, and went red on
    // macOS by exactly one tick. What must not happen is the client running
    // ahead on its own, because every one of those ticks is a correction
    // waiting to happen.
    let (during, during_confirmed) = app.tick_pair();
    let lead = |tick: u64, confirmed: u64| tick.saturating_sub(confirmed);
    assert!(
        lead(during, during_confirmed) <= lead(before, before_confirmed),
        "the client got {} ticks ahead of the server while the world was paused, up from {}",
        lead(during, during_confirmed),
        lead(before, before_confirmed)
    );

    // And it starts again on the other side.
    server.set_paused(false);
    app.set_world_paused(false);
    run_real_time(&mut app, 0.75, Input::default());
    let (after, _) = app.tick_pair();
    assert!(
        after > during,
        "the client did not restart after the world did: still at tick {after}"
    );

    // **The whole point**: no correction to smooth out on the way back. This is
    // the second report, as a number.
    let correction = app.pacing().worst_correction_cells();
    assert!(
        correction < 1.0,
        "unpausing left {correction} cells of correction to be pulled through"
    );
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
    let server = embedded("divergence");
    let mut app = client("divergence", &server, gpu);
    // Asked for on this client, not through the environment: these cases run on
    // parallel threads, so a `set_var` here and a `remove_var` in another test
    // raced and the trace came out empty on whichever machine lost.
    assert!(
        app.trace_physics_to(&trace_path),
        "could not open {}",
        trace_path.display()
    );

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

    let written = std::fs::read_to_string(&trace_path).expect("the trace file should exist");
    // A handful, not a rate. How many server messages land in a few seconds
    // depends on how loaded the machine is — macOS CI answered four where this
    // box answers seventy — and a test that encodes one machine's throughput is
    // a test that fails on somebody else's.
    let lines = written.lines().count();
    assert!(
        lines >= 3,
        "the trace wrote {lines} lines across a whole session, so it is not recording"
    );
    // The first line may be an `unmatched` one, which has its own shape, so this
    // checks a line that carries a comparison.
    let first = written
        .lines()
        .find(|line| line.contains("dist "))
        .unwrap_or_default();
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

#[test]
fn jumping_across_a_chunk_plane_does_not_replay_against_the_wrong_chunk() {
    // **The chunk-border glitch, at the seam it was actually felt on.** Walking
    // crosses the vertical planes; jumping crosses the horizontal one, and that
    // is the case the window reported — "jumping into a hole is still rough",
    // "climbing out of a hole ... jerking and sliding".
    //
    // A reconcile adopts a state from several ticks ago, so its origin is the
    // one the body held BEFORE the crossing, while the client has already
    // renormalised past it. The replay then ran against a view anchored a whole
    // chunk away: from the session log, the body fell 3.6 cells through solid
    // ground with `vy` at exactly five ticks of gravity.
    //
    // Two numbers say it did not happen. The divergence is the two simulations
    // measured against each other; the terrain conflict is the client's own
    // world asked whether it even has ground where the server is standing —
    // which is the half that tells a replay bug from a streaming one.
    //
    // **This is the broad invariant, not the regression test.** Measured on this
    // world it does NOT fail against the bug: hopping across a flat plain
    // crosses the vertical planes, and a view offset by a chunk on x still finds
    // flat ground under the body, so the disagreement stays small. What made the
    // reported glitch violent was terrain the player had dug. The case that
    // fails cleanly against the bug is
    // `predict::tests::a_replay_across_a_chunk_plane_still_has_ground_under_it`,
    // which stages the crossing exactly and watches the body fall through the
    // floor. Keep both: that one holds the fix in place, this one would notice a
    // whole class of ordinary-play disagreements neither of us has thought of.
    let Some(gpu) = gpu() else { return };
    let server = embedded_with_view("plane-jumps", ViewDistance::DEFAULT);
    let mut app = client("plane-jumps", &server, gpu);

    // Traced unconditionally, and from before the join, because the failure is a
    // claim about geometry and the count alone cannot be argued with.
    let trace = scratch("plane-jumps").join("physics.log");
    assert!(
        app.trace_physics_to(&trace),
        "could not open {}",
        trace.display()
    );

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    // **Two seconds, not one, and the second one is not padding.** `Pacing`
    // publishes the worst of each one-second window, so a reading taken at the
    // end of the first window still carries the join in it — the client holds no
    // terrain for a moment while the server is already falling to the ground,
    // which traced as `dv -0.4800, -0.7200, -0.9600`, gravity three ticks
    // running. Nothing about crossing a chunk plane, and enough to fail the
    // measurement below on its own.
    run_real_time(&mut app, 2.0, Input::default());

    // Held down: one hop per press is enforced on the tick, so this is a body
    // that jumps, lands, and jumps again for as long as the test runs — and a
    // jump is the only thing that crosses a horizontal plane on its own.
    let hopping = Input {
        forward: 1.0,
        jump: true,
        sprint: true,
        ..Input::default()
    };

    // **Sampled every frame, not read at the end.** `Pacing` publishes the worst
    // of each one-second window, so a single bad crossing is gone from the
    // readout a second later — and a crossing is over in three ticks. Reading it
    // once after the run is how the first version of this test passed against
    // the very bug it was written for.
    // **Measured from here, not from the client's first tick.** The very first
    // reconcile after a join lands before any chunk has arrived — traced as
    // `tick 1 ... server_ground true ... ground false` — and a client with no
    // world yet cannot be said to disagree with the server about one. What this
    // test is about starts once there is terrain to cross.
    let settled = app.pacing().terrain_conflicts_total();
    let start = app.camera().position.chunk;
    let mut crossed = start;
    let mut diverged: f32 = 0.0;
    let frame = Duration::from_millis(16);
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last = Instant::now();
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        let now = Instant::now();
        app.advance(hopping, now.duration_since(last).as_secs_f32());
        last = now;
        diverged = diverged.max(app.pacing().worst_divergence_cells());
        crossed = app.camera().position.chunk;
        std::thread::sleep(frame);
    }

    assert_ne!(
        (start.x, start.y, start.z),
        (crossed.x, crossed.y, crossed.z),
        "never crossed a chunk plane, so the test proved nothing: still at {start:?}"
    );
    assert_eq!(
        app.pacing().terrain_conflicts_total() - settled,
        0,
        "the client's world had no ground where the server was standing — the replay is \
         colliding against the wrong chunk. Trace: {}",
        trace.display()
    );
    assert!(
        diverged < 1.0,
        "the two simulations disagreed by {diverged} cells while jumping across chunk planes"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_frame_log_records_every_column_it_promises() {
    // **The log a scripted session will be read from.** It will be looked at
    // once, offline, by someone who cannot re-run it — so a missing column or a
    // row that never arrives is not an inconvenience, it is the whole session
    // wasted.
    //
    // So: the header names every field the writer writes, every row has as many
    // values as the header has names, and the numbers that should move during a
    // jump actually move.
    let Some(gpu) = gpu() else { return };
    let log_path = scratch("frame-log").join("frames.csv");
    let server = embedded("frame-log");
    let mut app = client("frame-log", &server, gpu);
    assert!(
        app.log_frames_to(&log_path),
        "could not open {}",
        log_path.display()
    );

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));

    // A headless client has no swapchain, so the frames are logged by hand here
    // exactly as `main` logs them — which is also what proves the columns line up
    // without a window.
    let phases = client::app::Phases::default();
    run_real_time(&mut app, 0.6, Input::default());
    for _ in 0..30 {
        app.pump_network();
        app.advance(Input::default(), 1.0 / 60.0);
        app.log_frame(&phases, true);
    }
    let jump = Input {
        jump: true,
        ..Input::default()
    };
    for tick in 0..90 {
        app.pump_network();
        app.advance(if tick < 4 { jump } else { Input::default() }, 1.0 / 60.0);
        app.log_frame(&phases, tick % 3 != 0);
    }

    app.shutdown();
    assert!(server.stop());

    let written = std::fs::read_to_string(&log_path).expect("the frame log should exist");
    let mut lines = written.lines();
    let header = lines.next().expect("a header");
    let columns = header.split(',').count();
    assert!(
        columns > 30,
        "the header names only {columns} columns, which is fewer than the log writes"
    );

    let rows: Vec<&str> = lines.collect();
    assert!(rows.len() >= 100, "only {} rows were written", rows.len());
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.split(',').count(),
            columns,
            "row {index} has a different number of values than the header names: {row}"
        );
    }

    // The body must be doing something across a jump, or the log is a column of
    // zeroes that would look like a perfectly still player.
    let column = |name: &str| -> usize {
        header
            .split(',')
            .position(|field| field == name)
            .unwrap_or_else(|| panic!("no `{name}` column in: {header}"))
    };
    let body_y = column("body_y");
    let heights: Vec<f32> = rows
        .iter()
        .filter_map(|row| row.split(',').nth(body_y)?.parse().ok())
        .collect();
    let lowest = heights.iter().copied().fold(f32::MAX, f32::min);
    let highest = heights.iter().copied().fold(f32::MIN, f32::max);
    assert!(
        highest - lowest > 1.0,
        "the body never moved across a jump ({lowest} to {highest}), so the log is recording \
         something other than the player"
    );

    // And the presented column has both answers in it, or it is not a fact.
    let presented = column("presented");
    let shown = rows
        .iter()
        .filter(|row| row.split(',').nth(presented) == Some("1"))
        .count();
    assert!(
        shown > 0 && shown < rows.len(),
        "{shown} of {} rows say presented, which cannot distinguish a dropped frame",
        rows.len()
    );
}

/// Every sound a server registers reaches the mixer, and walking makes noise.
///
/// # Why this test exists
///
/// Two bugs, both silent, both found only because somebody played the game and
/// said footsteps made no sound. Neither was in the footstep logic, which was
/// correct the whole time — `play_footsteps` chose the right material and paced
/// itself properly. The sounds simply were not there to play.
///
/// 1. **A race.** The decode was kicked off from the `ContentChunk` arm on a
///    spawned task, BEFORE `accept_slice` wrote the bytes into the cache, on the
///    reasoning that spawning deferred it far enough. It does not. When the task
///    won, the decode found nothing, returned quietly, and nothing retried.
/// 2. **A shared hash.** Content is addressed by content hash, and
///    `core_tools:break` and `core_tools:place` ship byte-identical files. The
///    dispatch used `find`, so the second sound was never decoded.
///
/// Both produced the same symptom and neither produced a warning. What makes
/// them catchable is asserting on the MIXER — the place a sound has to reach to
/// be audible — rather than on the tables, which were right in both cases and
/// which is all the bot tests could see.
#[test]
fn every_registered_sound_reaches_the_mixer_and_walking_makes_noise() {
    let Some(gpu) = gpu() else { return };
    let server = embedded("sounds-reach-the-mixer");
    let mut app = client("sounds-reach-the-mixer", &server, gpu);
    assert!(
        run_frames(&mut app, |app| app.joined() && app.meshed_chunks() >= 4),
        "the client never joined: {:?}",
        app.warnings()
    );

    // Sounds are fetched AFTER the join — textures gate it, sound does not — so
    // this waits for them rather than assuming they arrived with the world.
    let arrived = run_frames(&mut app, |app| {
        !app.sounds().is_empty()
            && app
                .sounds()
                .iter()
                .all(|sound| sound.file.is_none() || app.mixer().holds(&sound.id))
    });
    let table: Vec<(String, bool)> = app
        .sounds()
        .iter()
        .map(|sound| (sound.id.clone(), sound.file.is_some()))
        .collect();
    assert!(
        arrived,
        "not every sound reached the mixer: {:?}, mixer holds {}",
        table,
        app.mixer().len()
    );
    assert!(
        !table.is_empty(),
        "the reference mods register sounds, so an empty table is a delivery failure"
    );
    // The pair that shares a content hash, named so a future change that makes
    // them differ does not quietly retire the case they cover.
    for id in ["core_tools:break", "core_tools:place"] {
        assert!(
            app.mixer().holds(id),
            "{id} never reached the mixer; those two ship identical files and so share one hash"
        );
    }

    // And the footstep path end to end: walking on the reference worldgen's
    // terrain, which is `core:white`, and that block declares a step sound.
    assert!(
        app.mixer().holds("core:step"),
        "the step sound never reached the mixer, so nothing walked on can be heard"
    );
    let walk = Input {
        forward: 1.0,
        ..Input::default()
    };
    let mut fired = 0usize;
    for _ in 0..600 {
        assert!(
            app.pump_network(),
            "the connection ended: {:?}",
            app.warnings()
        );
        app.remesh();
        app.advance(walk, 1.0 / 60.0);
        if app.play_footsteps().is_some() {
            fired += 1;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    assert!(
        fired > 0,
        "walking across the world fired no footsteps at all"
    );

    // **And after a debug teleport**, which moves the frame the world is drawn
    // in out from under the player. Footsteps read a block position out of the
    // body's local coordinates, so anything that shifts a frame is worth one
    // cheap assertion — this does NOT currently discriminate between the two
    // origins that arithmetic could use (both were measured to work), it just
    // pins that walking still makes noise 50,000 blocks out.
    let jump = Input {
        teleport: Some(Teleport::Far),
        ..Input::default()
    };
    app.advance(jump, 1.0 / 60.0);
    assert!(
        run_frames(&mut app, |app| app.meshed_chunks() >= 4),
        "the world never came back after the teleport: {:?}",
        app.warnings()
    );
    let mut after = 0usize;
    for _ in 0..600 {
        assert!(
            app.pump_network(),
            "the connection ended: {:?}",
            app.warnings()
        );
        app.remesh();
        app.advance(walk, 1.0 / 60.0);
        if app.play_footsteps().is_some() {
            after += 1;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    assert!(after > 0, "no footsteps fired after a debug teleport");

    app.shutdown();
    server.stop();
}

#[test]
fn the_chat_box_asks_for_focus_once_and_a_line_reaches_the_server() {
    // **Reported from the window: "the chat does not seem to fully work".**
    //
    // Two things, one symptom. The box took the keys and never sent, because
    // egui reports a single-line field's Enter as `lost_focus` and the renderer
    // asked for focus back on EVERY frame — so it never lost it, and Enter
    // landed on a field that immediately grabbed focus again.
    //
    // The renderer's half needs a window. This is the half that does not: the
    // flag is raised exactly once per opening, which is what makes
    // `request_focus` a one-shot rather than a loop, and a typed line really
    // does leave for the server and come back.
    let Some(gpu) = gpu() else { return };
    let server = embedded("chat");
    let mut app = client("chat", &server, gpu);
    assert!(
        run_frames(&mut app, App::joined),
        "never joined: {:?}",
        app.warnings()
    );

    assert!(!app.chat_open());
    assert!(
        !app.take_chat_focus(),
        "a closed box does not want the keyboard"
    );

    app.set_chat_open(true);
    assert!(app.chat_open());
    assert!(app.take_chat_focus(), "the frame it opens, it asks");
    assert!(
        !app.take_chat_focus(),
        "and never again — asking every frame is what stopped Enter working"
    );

    // Re-opening an already-open box must not steal focus back, or a repeating
    // key would fight the caret.
    app.set_chat_open(true);
    assert!(!app.take_chat_focus());

    // A line goes out and comes back. `send_chat` closes the box, which is what
    // pressing Enter should do.
    app.chat_draft_mut().push_str("hello everybody");
    app.send_chat();
    assert!(!app.chat_open(), "sending closes the box");
    assert!(
        run_frames(&mut app, |app| app
            .chat()
            .any(|line| line.contains("hello everybody"))),
        "the line never came back from the server: {:?}",
        app.warnings()
    );

    // And closing discards a half-typed line rather than leaving it to
    // reappear the next time the box opens.
    app.set_chat_open(true);
    app.chat_draft_mut().push_str("never mind");
    app.set_chat_open(false);
    app.set_chat_open(true);
    assert!(app.chat_draft_mut().is_empty());

    app.shutdown();
    server.stop();
}

#[test]
fn the_menu_is_the_front_door_and_closing_it_closes_what_it_opened() {
    // **Reported from the window: the controls screen is janky and hard to
    // reach.** It was reachable only by one undocumented function key, because
    // Escape released the cursor and did nothing else.
    //
    // The state machine that fixes it is small and easy to get wrong in a way
    // that strands a player: a screen open over the world with no way back.
    // This is that machine, without a window.
    let Some(gpu) = gpu() else { return };
    let server = embedded("menu");
    let mut app = client("menu", &server, gpu);
    assert!(
        run_frames(&mut app, App::joined),
        "never joined: {:?}",
        app.warnings()
    );

    assert!(!app.menu_open());
    assert!(!app.settings_open());

    app.set_menu_open(true);
    assert!(app.menu_open());

    // Controls are a PAGE of the menu, not a screen beside it.
    app.open_settings();
    assert!(app.settings_open());
    assert!(app.menu_open(), "opening a page must not close the menu");

    // And closing the menu takes the page with it. Leaving the controls up
    // over the world after the menu had gone is the stranding case.
    app.set_menu_open(false);
    assert!(!app.settings_open());
    assert!(!app.menu_open());

    // The two switches the menu owns, both persisted through the same flag the
    // volume sliders use.
    assert!(app.hud_visible());
    app.set_hud_visible(false);
    assert!(!app.hud_visible());
    assert!(
        app.take_volumes_dirty(),
        "a changed setting must be written out, or it is forgotten on restart"
    );

    // The scale is clamped rather than obeyed: it is the one setting a player
    // cannot recover from if it goes wrong.
    let sane = app.ui_scale();
    app.set_ui_scale(f32::NAN);
    assert!(
        (app.ui_scale() - sane).abs() < f32::EPSILON,
        "a NaN scale must be ignored, not adopted"
    );
    app.set_ui_scale(1000.0);
    assert!(app.ui_scale() <= *client::config::UI_SCALE_RANGE.end());
    app.set_ui_scale(0.0);
    assert!(app.ui_scale() >= *client::config::UI_SCALE_RANGE.start());

    app.shutdown();
    server.stop();
}

#[test]
fn a_held_dig_finishes_its_block_before_looking_through_the_hole() {
    // **Reported from the window.** A block comes apart as you dig it, so
    // within half a second the raycast is looking THROUGH the hole it just made
    // and lands on whatever is behind. That retargeted the dig, threw away the
    // progress, and started chewing the next block while the first stood
    // half-eaten — so holding the button bored a ragged tunnel instead of
    // clearing one block at a time.
    //
    // The lock is the BUTTON, not the block: releasing frees the crosshair
    // mid-block, which is what makes a half-dug block something a player can
    // walk away from.
    let Some(gpu) = gpu() else { return };
    let server = embedded("dig-lock");
    let mut app = client("dig-lock", &server, gpu);
    assert!(
        run_frames(&mut app, |app| app.joined() && app.predicting()),
        "never joined: {:?}",
        app.warnings()
    );

    app.look_down_by(std::f32::consts::FRAC_PI_4);
    assert!(
        run_frames(&mut app, |app| app.dig_target().is_some()),
        "the crosshair found nothing to dig"
    );
    // Let the world settle before deciding what "the block it started on"
    // means: chunks are still arriving, and the first frame's raycast can land
    // on something that is not there a moment later.
    for _ in 0..30 {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }

    // The first press is what takes the lock, so the block it locked onto is
    // read from the selection AFTER it — not from the raycast before it.
    app.dig();
    let first = app
        .selection()
        .first()
        .expect("a selection once digging")
        .block();

    // Hold the button through a good part of one block's worth of digging —
    // long enough that sub-nodes have visibly gone and the raw raycast is
    // looking through the hole, and short enough that the block is not finished.
    // **Only while the block is still there.** Once it is empty the lock is
    // meant to release, so checking past that point would assert the opposite
    // of the intended behaviour — which is exactly what a first version of this
    // did, over a window slightly longer than one block takes to break.
    let cells_left = |app: &App| {
        let base = tiamot_core::SubNodePos::new(first.x * 3, first.y * 3, first.z * 3);
        app.store().get(first.chunk()).map_or(0, |chunk| {
            (0..3)
                .flat_map(|y| (0..3).flat_map(move |z| (0..3).map(move |x| (x, y, z))))
                .filter(|(x, y, z)| {
                    chunk
                        .get_subnode(tiamot_core::SubNodePos::new(
                            base.x + x,
                            base.y + y,
                            base.z + z,
                        ))
                        .is_some_and(|material| !material.is_air())
                })
                .count()
        })
    };
    let started_with = cells_left(&app);

    // **Driven to a condition, not for a fixed number of frames.** A count of
    // frames is a bet on how fast the machine is: sixty of them was enough here
    // and not on the macOS runner, where the block had not opened up by the end
    // of the window and the test failed saying so.
    let mut wandered = None;
    let mut ate = 0usize;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        if cells_left(&app) == 0 {
            break;
        }

        // The selection is what the dig is actually spending time on, and what
        // the player sees. It must not move.
        if let Some(shown) = app.selection().first()
            && shown.block() != first
        {
            wandered = Some(shown.block());
        }
        ate = ate.max(started_with.saturating_sub(cells_left(&app)));
        if wandered.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    assert!(
        wandered.is_none(),
        "the dig moved from {first:?} to {:?} while the button was still held",
        wandered.expect("checked")
    );
    // **Non-vacuity: the block really was being eaten while we watched.**
    //
    // An earlier version asserted that the raw raycast had pointed PAST the
    // block, on the reasoning that a hole is what it points through. That
    // failed on the macOS runner twice: whether the ray gets through depends on
    // when the client's own store catches up with the server's edits, which is
    // a property of the machine rather than of the lock. What is actually
    // observable is that sub-nodes went while the selection stayed put.
    assert!(
        ate > 0,
        "the block never lost a sub-node while the button was held, so nothing \
         here exercised the lock"
    );

    // And releasing frees it: the crosshair is the aim again.
    app.stop_digging();
    for _ in 0..10 {
        assert!(app.pump_network(), "the connection ended");
        app.advance(Input::default(), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        app.selection()
            .first()
            .is_none_or(|shown| shown.block() != first)
            || app.dig_target().is_some_and(|aim| aim.block() == first),
        "releasing the button did not give the crosshair back"
    );

    app.stop_digging();
    app.shutdown();
    assert!(server.stop());
}

/// The atlas reaches the interface, and is handed to egui exactly once.
///
/// # Why the count matters
///
/// `register_native_texture` allocates a bind group per call and `main.rs` has
/// no way to know when a new one is warranted other than being told. Asking
/// every frame would leak one per frame; asking once at startup would never
/// happen, because the material table arrives from the server long after the
/// window does. So the flag has to be true exactly once per atlas, and this
/// counts it across a whole session rather than trusting the shape of the code.
#[test]
fn the_atlas_reaches_the_interface_and_is_handed_over_exactly_once() {
    let Some(gpu) = gpu() else {
        return;
    };
    let server = embedded("atlas-bridge");
    let mut app = client("atlas-bridge", &server, gpu);

    let mut changes = 0;
    let mut arrived: Option<Instant> = None;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        if app.take_atlas_change() {
            changes += 1;
            arrived.get_or_insert_with(Instant::now);
        }
        // A second of frames AFTER the atlas lands, because what is being
        // tested is an absence: a flag left set would be counted again in
        // every one of them. Wall-clock rather than a frame count — the
        // window is how long the client is watched, not how much it did.
        if arrived.is_some_and(|at| at.elapsed() > Duration::from_secs(1)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    assert_eq!(
        changes, 1,
        "the atlas is handed to egui once per atlas, not once per frame"
    );

    // And the layout that went with it actually distinguishes materials — a
    // bridge that mapped everything to tile zero would draw one block for all.
    let tiles = app.tiles();
    let first = tiles.uv_of(1).expect("the material table has arrived");
    let second = tiles.uv_of(2).expect("the material table has arrived");
    assert_ne!(
        first, second,
        "two materials sharing a tile would draw as the same block"
    );

    app.shutdown();
    assert!(server.stop());
}

/// The hotbar is the player's own slots, in position.
///
/// # What this replaced
///
/// It used to be the CONSOLIDATED inventory — one entry per material, sorted by
/// id — so a player who dug a second thing watched the row rearrange itself
/// under their hands and the key that had been placing stone was suddenly
/// placing dirt. A hotbar is a place. This asserts the row IS the first slots
/// of `player:main`, which is the same grid the inventory screen's top row
/// draws, so the two can never disagree about what key three reaches.
#[test]
fn the_hotbar_is_the_first_slots_of_the_players_own_inventory() {
    let Some(gpu) = gpu() else {
        return;
    };
    let server = embedded("hotbar-slots");
    let mut app = client("hotbar-slots", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    app.look_down_by(std::f32::consts::FRAC_PI_4);
    assert!(
        run_frames(&mut app, |app| app.looking_at().is_some()),
        "the crosshair found nothing within reach"
    );

    // Dig, so there is anything at all to be in a slot.
    let mut filled = false;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && !filled {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        filled = app
            .views()
            .get("player:main")
            .is_some_and(|view| view.slots.iter().flatten().count() > 0);
        std::thread::sleep(Duration::from_millis(16));
    }
    app.stop_digging();
    assert!(filled, "nothing ever reached a slot: {:?}", app.warnings());

    let slots = app
        .views()
        .get("player:main")
        .expect("checked above")
        .slots
        .clone();
    let hotbar = app.hotbar().to_vec();
    assert_eq!(hotbar.len(), 9, "the number keys reach nine places");
    for (index, slot) in hotbar.iter().enumerate() {
        assert_eq!(
            *slot,
            slots.get(index).copied().flatten(),
            "hotbar slot {index} is not the inventory's slot {index}"
        );
    }
    assert!(
        hotbar.iter().any(Option::is_some),
        "the dug material never appeared in the hotbar"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_view_widens_as_you_move_and_eases_back_when_you_stop() {
    // **Reported from the window**: "when you start walking the camera zooms
    // out (fov) just by a tiny bit... sprinting should have an even more
    // extreme fov change. make the fov based on my speed."
    //
    // Driven by SPEED rather than by the gait, so wading and being shoved read
    // correctly without any of them being a special case — and so the number
    // this asserts is the one a player actually sees.
    let Some(gpu) = gpu() else { return };
    let server = embedded("speed-fov");
    let mut app = client("speed-fov", &server, gpu);

    assert!(run_frames(&mut app, |app| app.joined()
        && app.predicting()
        && app.meshed_chunks() >= 4));
    run_real_time(&mut app, 0.5, Input::default());
    let still = app.fov();

    let walking = Input {
        forward: 1.0,
        ..Input::default()
    };
    run_real_time(&mut app, 1.0, walking);
    let walked = app.fov();
    assert!(
        walked > still,
        "walking did not widen the view: {walked} against {still} standing"
    );

    // Sprinting is further out, because it is faster — not because it is a
    // different case.
    let sprinting = Input {
        forward: 1.0,
        sprint: true,
        ..Input::default()
    };
    run_real_time(&mut app, 1.5, sprinting);
    let sprinted = app.fov();
    assert!(
        sprinted > walked,
        "sprinting was no wider than walking: {sprinted} against {walked}"
    );

    // And it comes back. Given time, because it eases rather than snapping —
    // a field of view that stepped with the 20 Hz tick would strobe.
    run_real_time(&mut app, 2.0, Input::default());
    let stopped = app.fov();
    assert!(
        (stopped - still).abs() < 1e-3,
        "the view stayed at {stopped} after stopping, against {still} standing"
    );
}

#[test]
fn a_broken_block_does_not_hand_the_dig_straight_to_the_one_behind_it() {
    // **Asked for from the window**: "add an extremely minute pause after
    // deleting a given block that has another block behind it, so that people
    // can keep from breaking the back block behind the one they are focusing
    // on."
    //
    // Holding the button through a break otherwise starts on whatever the hole
    // reveals in the same frame, and a player who wanted one block has taken
    // two.
    let Some(gpu) = gpu() else { return };
    let server = embedded("break-pause");
    let mut app = client("break-pause", &server, gpu);
    assert!(
        run_frames(&mut app, |app| app.joined() && app.predicting()),
        "never joined: {:?}",
        app.warnings()
    );

    // Down into the ground, so there is certainly something behind the block
    // being dug — which is the only case the pause applies to.
    app.look_down_by(std::f32::consts::FRAC_PI_4);
    assert!(
        run_frames(&mut app, |app| app.dig_target().is_some()),
        "the crosshair found nothing to dig"
    );
    for _ in 0..30 {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(16));
    }

    // Hold the button until a block comes apart and the pause takes hold.
    let mut paused = false;
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        if app.dig_paused() {
            paused = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        paused,
        "a block was dug through with something behind it and the dig carried \
         straight on into it"
    );

    // And it is a pause, not a stop: it lets go on its own.
    let deadline = Instant::now() + PATIENCE;
    while app.dig_paused() {
        assert!(
            Instant::now() < deadline,
            "the pause after a break never ended, so the button is dead"
        );
        assert!(app.pump_network(), "the connection ended");
        app.advance(Input::default(), 1.0 / 60.0);
        app.dig();
        std::thread::sleep(Duration::from_millis(16));
    }

    app.stop_digging();
}
