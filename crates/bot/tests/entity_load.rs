// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Two hundred scripted mobs in one place: Task 12's stated load test.
//!
//! # What two hundred entities actually costs
//!
//! Three separate things, and only the first is what anyone expects:
//!
//! 1. **The physics.** Two hundred bodies swept against sub-node geometry.
//!    Measured in `spikes/ecs`, stepping the store itself is under a
//!    microsecond; the sweep is the real cost.
//! 2. **The Lua.** One `on_step` call per entity per tick, each with its own
//!    instruction budget — not a share of one, which is the arrangement that
//!    matters here: a mod with two hundred mobs must not give each a
//!    two-hundredth of a budget, and one runaway mob must not starve the other
//!    hundred and ninety-nine.
//! 3. **The replication.** Every one of them inside a viewer's cylinder is a
//!    delta a tick, per viewer.
//!
//! Charter rule 18: the budget is 50 ms for all simulation for all players, so
//! the assertion is the shape of the tick distribution rather than a time — a
//! shared CI runner cannot promise a time, and a gate that fires on a
//! neighbour's build gets muted.
//!
//! # Why this is `#[ignore]` by default
//!
//! It runs for half a minute. Nightly runs it the way the other load tests are
//! run:
//!
//! ```console
//! cargo test -p bot --test entity_load --release -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::tick::TICK_DURATION;
use tiamot_server::{ServerHandle, Settings};

/// The task's number.
const MOBS: u32 = 200;

/// How long the world runs with them in it.
const SECONDS: u64 = 30;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-entity-load").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that fills one area with wandering mobs.
///
/// Every one of them runs real script work every tick: reading its own entity,
/// deciding a direction, and writing a drive back. A mod whose `on_step` did
/// nothing would measure the dispatch and not the load.
fn write_swarm(name: &str, mobs: u32) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("swarm");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"swarm\"\nname = \"Swarm\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "local rock = game.register_block{{ id = \"rock\" }}\n\
             game.register_on_generate(function(buf, pos)\n\
             \x20   buf:fill_below_heightmap(game.flat_heightmap(0), rock)\n\
             end)\n\
             local made = false\n\
             local turn = 0\n\
             game.register_on_tick(function()\n\
             \x20   turn = turn + 1\n\
             \x20   if made or turn < 20 then return end\n\
             \x20   for i = 1, {mobs} do\n\
             \x20       local ring = i % 16\n\
             \x20       local id = game.spawn_entity({{\n\
             \x20           pos = {{ x = ring - 8, y = 1, z = (i // 16) - 6 }},\n\
             \x20           model = \"engine:humanoid\",\n\
             \x20           collider = {{ width = 1.8, height = 5.4 }},\n\
             \x20       }})\n\
             \x20       if id == nil then return end\n\
             \x20   end\n\
             \x20   made = true\n\
             \x20   game.log(\"the swarm is out\")\n\
             end)\n\
             -- One call per entity per tick, each doing enough to be worth\n\
             -- measuring: a read, a decision and a write.\n\
             game.register_on_entity_step(function(id)\n\
             \x20   local self = game.entity(id)\n\
             \x20   if self == nil then return end\n\
             \x20   local face = (id + turn // 40) % 4\n\
             \x20   local dx = (face == 0 and 1) or (face == 2 and -1) or 0\n\
             \x20   local dz = (face == 1 and 1) or (face == 3 and -1) or 0\n\
             \x20   game.set_entity(id, {{\n\
             \x20       drive = {{ walk = {{ x = dx, z = dz }}, gait = \"walk\" }},\n\
             \x20       anim = 1,\n\
             \x20   }})\n\
             end)\n"
        ),
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
        // The default rather than the minimum: the mobs have to be inside a
        // viewer's cylinder for replication to be part of what is measured, and
        // replication is a third of the cost.
        view_distance: ViewDistance::default(),
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(12),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

#[test]
#[ignore = "runs for half a minute; nightly runs it with --ignored"]
fn two_hundred_scripted_mobs_keep_the_tick_inside_its_budget() {
    let server = start("swarm", write_swarm("swarm", MOBS));
    let addr = server.local_addr();
    let control = server.control().clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    // A watcher, so the chunks the mobs are in stay loaded and every one of
    // them is replicated to somebody. A world with nobody in it is a world
    // whose chunks nobody asked for.
    let seen = runtime.block_on(async {
        let mut bot = Bot::connect_trusting(addr, Identity::generate().expect("identity"))
            .await
            .expect("connect");
        bot.join("Watcher").await.expect("join");

        // Let the world settle before anything is measured: the first ticks
        // include mod loading and first-visit chunk generation, which are real
        // and are not what this is watching.
        let settle = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < settle {
            let _ = tokio::time::timeout(Duration::from_millis(100), bot.recv()).await;
        }
        let _ = control.take_tick_samples();

        let until = tokio::time::Instant::now() + Duration::from_secs(SECONDS);
        while tokio::time::Instant::now() < until {
            let _ = tokio::time::timeout(Duration::from_millis(100), bot.recv()).await;
        }
        bot.entities().len()
    });

    let samples = control.take_tick_samples();
    let report = bot::bench::TickReport::from_samples(
        &samples,
        control.over_budget_ticks(),
        control.dropped(),
        1,
        SECONDS,
    );
    println!("{}", report.to_table());
    println!("  entities the watcher was told about: {seen}");

    assert!(
        seen >= MOBS as usize,
        "the watcher saw {seen} entities, not the {MOBS} the mod spawned — \
         so this measured a smaller world than it claims to"
    );
    assert!(
        report.ticks > 0,
        "the server should have ticked; a run with no samples proves nothing"
    );
    assert_eq!(
        report.over_budget, 0,
        "{} of {} ticks ran over the {TICK_DURATION:?} budget with {MOBS} mobs stepping",
        report.over_budget, report.ticks
    );
    assert_eq!(
        report.dropped, 0,
        "the server dropped {} ticks, so it could not keep up with {MOBS} mobs",
        report.dropped
    );

    assert!(server.stop());
}
