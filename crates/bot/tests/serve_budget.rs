// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Terrain a mod paid for: the tick keeps its budget whatever a chunk costs.
//!
//! # The world this is about
//!
//! Reported from the window on 2026-09-09, a release build at view distance
//! 24 with the player's own generator: `serving` alone taking 50–110 ms of a
//! 50 ms tick, sustained for as long as the world was filling, and the
//! simulation dropping about a fifth of its ticks underneath. What a player
//! feels is not the terrain arriving slowly — it is every mob and every other
//! player moving in jerks, because the tick they are stepped on is being
//! skipped.
//!
//! The arithmetic was never in doubt once the phase breakdown named it.
//! `CHUNKS_PER_TICK` is **22**, sized when a chunk cost ~2.3 ms in the
//! reference world (a flat fill and its lighting). A chunk generated from a
//! sampled density field costs 3.98 ms to generate and 1.44 ms to light — the
//! numbers are in `api/stubs/game.lua`, beside the feature — so twenty-two of
//! them is **119 ms**, and the count knew nothing about it.
//!
//! **A chunk's cost belongs to the mod, so the engine cannot put it in a
//! constant.** What it can promise is the share of the tick it will spend:
//! `SERVE_TIME_BUDGET`, checked before each request, with whatever does not fit
//! staying queued for the next tick.
//!
//! # What this test asserts, and what it deliberately does not
//!
//! Not a wall-clock cost — a shared CI runner cannot promise one, and a gate
//! that fires on a neighbour's build gets muted. It asserts the MECHANISM:
//! with a generator expensive enough that a full `CHUNKS_PER_TICK` cannot fit
//! in a tick, the server still holds its budget, does not drop ticks, and
//! still delivers the terrain. Before the clock existed this test fails on the
//! first two while passing the third, which is exactly the bug.
//!
//! # Why this is `#[ignore]` by default
//!
//! It generates real terrain for half a minute. Nightly runs it the way the
//! other load tests are run:
//!
//! ```console
//! cargo test -p bot --test serve_budget --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::tick::TICK_DURATION;
use tiamot_server::{ServerHandle, Settings};

/// How long the world runs while a player streams it.
const SECONDS: u64 = 25;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-serve-budget").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that generates terrain the expensive way, on purpose.
///
/// Sampled density with caves — the shape of a real world generator, and the
/// documented worst case at 3.98 ms a chunk. The point is not that this mod is
/// badly written; it is that it is a perfectly reasonable mod, and the engine
/// must stay inside its budget while serving it.
fn write_generator(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("relief");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"relief\"\nname = \"Relief\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        "local stone = game.register_block{ id = \"stone\" }\n\
         local field = game.density{\n\
         \x20   op = \"min\",\n\
         \x20   a = {\n\
         \x20       op = \"sub\",\n\
         \x20       a = { op = \"noise\", stream = \"terrain\", frequency = 0.03, octaves = 3 },\n\
         \x20       b = { op = \"mul\", a = { op = \"y\" }, b = { op = \"const\", value = 0.06 } },\n\
         \x20   },\n\
         \x20   b = {\n\
         \x20       op = \"sub\",\n\
         \x20       a = { op = \"const\", value = 0.35 },\n\
         \x20       b = { op = \"abs\", a = { op = \"noise\", stream = \"caves\", frequency = 0.05 } },\n\
         \x20   },\n\
         }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_density(field, stone, { detail = \"sampled\" })\n\
         end)\n",
    )
    .expect("script");
    root
}

fn start(name: &str, mods: PathBuf, view: ViewDistance) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        identity_path: None,
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(11),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

#[test]
#[ignore = "load test: generates terrain for half a minute"]
fn expensive_terrain_does_not_cost_the_tick_its_budget() {
    let server = start(
        "expensive",
        write_generator("expensive-mods"),
        ViewDistance::DEFAULT,
    );
    let control = server.control().clone();
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let started = control.tick();
            let mut alice =
                Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
                    .await
                    .expect("connect");
            alice.join("Alice").await.expect("join");

            // Stream for a fixed span of world time rather than a fixed number
            // of chunks: the thing under test is what the tick does while it is
            // busy, and a chunk count would end the test as soon as the server
            // got good at its job.
            let deadline = std::time::Instant::now() + Duration::from_secs(SECONDS);
            let mut chunks = 0;
            while std::time::Instant::now() < deadline {
                chunks += alice
                    .collect_chunks(1, Duration::from_secs(5))
                    .await
                    .map_or(0, |arrived| arrived.len());
            }

            let ran = control.tick().saturating_sub(started);
            let dropped = control.dropped();
            let over_budget = control.over_budget_ticks();
            let slowest = Duration::from_micros(control.slowest_tick_micros());
            let share = slowest.as_secs_f64() / TICK_DURATION.as_secs_f64() * 100.0;
            println!(
                "serve budget: {chunks} chunks over {ran} ticks, slowest {slowest:?} \
                 ({share:.1}% of the {TICK_DURATION:?} budget), over_budget={over_budget} \
                 dropped={dropped}"
            );
            if let Some(phases) = control.slowest_phases() {
                println!("  worst tick: {phases}");
            }

            // **Non-vacuous first.** A server that served nothing would hold
            // every budget below perfectly, and the whole point of a time
            // budget is that the terrain still arrives.
            assert!(
                chunks > 0,
                "no terrain arrived at all, so the budget assertions below mean nothing"
            );

            // The reported symptom, and the one a player feels: a dropped tick
            // is a tick nothing was stepped on. Some tolerance, because the
            // join burst itself is real work and a shared runner is a shared
            // runner — but a fifth of the run, which is what was reported, is
            // not a tolerance.
            assert!(
                dropped * 20 < ran.max(1),
                "{dropped} of {ran} ticks were dropped while generating terrain. A tick that \
                 is skipped is a tick every mob and every other player stands still for."
            );

            // And the cause: the same limit the rest of the tick lives under.
            // A tenth of a run over budget is a server that cannot keep up.
            assert!(
                over_budget * 10 < ran.max(1),
                "{over_budget} of {ran} ticks ran over the {TICK_DURATION:?} budget serving \
                 terrain — serving is bounded by SERVE_TIME_BUDGET, so this is the bound \
                 failing rather than the terrain being expensive."
            );
        });
}
