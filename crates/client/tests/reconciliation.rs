// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Prediction and reconciliation over a deliberately bad network.
//!
//! # The half of the impairment test that needs a real client
//!
//! `crates/bot/tests/impaired.rs` proves the world ends up right when messages
//! are late and some never arrive. It cannot prove anything about
//! *reconciliation*, because a bot does not predict — it asks the server where
//! it is and believes the answer. Only the client runs `phys::step` locally,
//! replays what the server has not seen, and measures the difference.
//!
//! That difference is the number Task 09's test list asks to be logged and
//! bounded, and it is the one number that says whether prediction works at all.
//! **A correction that is never zero is prediction failing**, and it is nearly
//! impossible to notice by eye: no single frame looks wrong, the world is just
//! subtly not where it was left.
//!
//! # Why this needs a bad network to mean anything
//!
//! On loopback the client is a handful of milliseconds ahead of the server, so
//! there is almost nothing in flight and almost nothing to reconcile. A
//! reconciliation test on loopback passes on a client whose replay logic is
//! entirely broken. See `tiamot_server::transport::Impairment`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use client::app::{App, Input};
use client::cache::ContentCache;
use client::config::{Config, RenderMode};
use client::net::Connection;
use client::predict::SNAP_DISTANCE;
use client::render::{Gpu, Renderer};
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::transport::Impairment;
use tiamot_server::{ServerHandle, Settings};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tiamot-reconciliation")
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

fn client(name: &str, server: &ServerHandle, gpu: Gpu, impairment: Impairment) -> App {
    let home = scratch(&format!("{name}-home"));
    let config = Config {
        display_name: format!("Predictor-{name}"),
        ..Config::default()
    };
    let connection = Connection::open_impaired(
        server.local_addr(),
        Identity::generate().expect("identity"),
        config.display_name.clone(),
        ContentCache::open(&home.join("content")).expect("cache"),
        &home.join("known-hosts"),
        impairment,
    )
    .expect("connect");

    let renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");
    App::new(config, connection, renderer)
}

/// Runs the real frame loop until `done`, or gives up.
fn run_frames(app: &mut App, input: Input, seconds: f32, done: impl Fn(&App) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut last = Instant::now();
    while Instant::now() < deadline {
        assert!(
            app.pump_network(),
            "the connection ended: {:?}",
            app.warnings()
        );
        app.remesh();
        let dt = last.elapsed().as_secs_f32().min(0.1);
        last = Instant::now();
        app.advance(input, dt);
        if done(app) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    done(app)
}

#[test]
fn walking_over_a_bad_link_keeps_the_correction_small() {
    // The [A] criterion's "reconciliation error metrics logged and bounded".
    //
    // Bounded by `SNAP_DISTANCE`, which is where the client stops blending a
    // correction and teleports instead — the point at which the player sees it.
    // Staying under that is the actual claim: whatever the network does, the
    // prediction never becomes a visible jump.
    let Some(gpu) = gpu() else { return };
    let server = embedded("bad-link");
    let mut app = client("bad-link", &server, gpu, Impairment::default());

    // Joined on a CLEAN link, then impaired — see `net::Command::Impair`. The
    // handshake is sent once with no retries, so 5% loss during it is a 5%
    // chance of never joining at all, which is a property of this artificial
    // loss rather than of any real network.
    assert!(
        run_frames(&mut app, Input::default(), 30.0, |app| app.joined()
            && app.predicting()
            && app.meshed_chunks() >= 4),
        "expected to join; warnings: {:?}",
        app.warnings()
    );
    app.impair(Impairment::task_09());

    // Walk. Standing still would reconcile trivially — the server and the
    // client agree that nothing happened, whatever the latency.
    let forward = Input {
        forward: 1.0,
        ..Input::default()
    };
    // Settle first, and measure afterwards. **The first reconcile after a join
    // is a one-off**: the client has predicted from spawn with no server state
    // to compare against, and the correction that follows is a startup
    // transient rather than anything about the network. Including it made the
    // measurement identical to three decimal places whatever the client did,
    // which is how it was found.
    run_frames(&mut app, forward, 2.0, |_| false);

    let mut worst = 0.0f32;
    let mut samples = 0;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut last = Instant::now();
    while Instant::now() < deadline {
        assert!(app.pump_network(), "the connection ended");
        app.remesh();
        let dt = last.elapsed().as_secs_f32().min(0.1);
        last = Instant::now();
        app.advance(forward, dt);

        let correction = app.pacing().worst_correction_cells();
        if correction > 0.0 {
            samples += 1;
        }
        worst = worst.max(correction);
        std::thread::sleep(Duration::from_millis(8));
    }

    // Logged, which is half of what the criterion asks for. The HUD carries
    // the same number for a human; this is the machine-readable copy.
    let conditions = Impairment::task_09();
    println!(
        "reconciliation over {}ms one way / {}% loss: worst correction {worst:.3} cells \
         ({:.3} yards); {samples} of the sampled windows had any correction at all",
        conditions.latency_ms,
        conditions.loss_percent,
        worst / tiamot_core::SUBNODES_PER_AXIS as f32,
    );

    // Bounded ABOVE, which is the criterion: `SNAP_DISTANCE` is where the
    // client stops blending a correction and teleports instead, so staying
    // under it means the player never sees the reconciliation happen.
    assert!(
        worst < SNAP_DISTANCE,
        "the worst correction was {worst:.3} cells, at or past the {SNAP_DISTANCE}-cell snap \
         distance — the player would see the client teleport rather than blend"
    );

    // And bounded BELOW, which is what stops the assertion above being
    // satisfied by a client that never predicted anything. Instrumenting the
    // reconcile showed what actually happens here: the error is exactly 0.000
    // on almost every tick — prediction and server agree bit for bit, as
    // charter rule 4's determinism promises — with brief excursions to a couple
    // of cells at exactly the ticks where the 5% loss ate an input. Those
    // excursions ARE the thing being measured. Their absence would mean the
    // loss never reached the client, and every number here would be a loopback
    // measurement wearing a costume.
    assert!(
        worst > 0.1,
        "the worst correction was {worst:.3} cells — essentially nothing ever needed \
         correcting, so either the loss is not reaching the client or the client is not \
         predicting"
    );

    // And the client must still have been moving. A prediction that never
    // moves is never corrected, so a bound with no movement under it proves
    // nothing at all.
    let travelled = app.server_travelled();
    assert!(
        travelled > 2.0,
        "the player only travelled {travelled:.2} blocks, so the correction bound above is \
         measuring a client that never moved"
    );

    app.shutdown();
    assert!(server.stop());
}

#[test]
fn the_impairment_is_actually_applied_to_the_client() {
    // The counter-example. Everything above would pass identically on a clean
    // link, which is exactly the failure mode this whole file exists to avoid —
    // so prove the link really is slow by measuring how long the join takes
    // against an unimpaired one.
    let Some(clean_gpu) = gpu() else { return };

    let clean_server = embedded("clean");
    let mut clean = client("clean", &clean_server, clean_gpu, Impairment::default());
    let clean_started = Instant::now();
    assert!(run_frames(&mut clean, Input::default(), 30.0, |app| app.joined()));
    let clean_join = clean_started.elapsed();
    clean.shutdown();
    assert!(clean_server.stop());

    // A second adapter rather than a clone: `Gpu` owns a device and a queue and
    // is deliberately not `Clone`.
    let Some(slow_gpu) = gpu() else { return };
    let slow_server = embedded("slow");
    let mut slow = client(
        "slow",
        &slow_server,
        slow_gpu,
        Impairment {
            latency_ms: 200,
            loss_percent: 0,
            seed: 1,
        },
    );
    let slow_started = Instant::now();
    assert!(run_frames(&mut slow, Input::default(), 30.0, |app| app.joined()));
    let slow_join = slow_started.elapsed();
    slow.shutdown();
    assert!(slow_server.stop());

    println!("join: {clean_join:?} clean, {slow_join:?} at 200 ms one way");
    assert!(
        slow_join > clean_join,
        "joining over a 200 ms link ({slow_join:?}) was no slower than over loopback \
         ({clean_join:?}), so the impairment is not reaching the client and every \
         reconciliation number above was measured on a clean network"
    );
}
