// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Tick stability under load.
//!
//! Four bots streaming inputs and edits while the server runs 200 ticks. The
//! question is not whether it keeps up on this machine — CI runners are shared
//! and noisy — but whether the tick loop degrades *gracefully*: no tick that
//! blows the budget by an order of magnitude, and no runaway catch-up.
//!
//! # Why the thresholds are asymmetric
//!
//! Over 2× budget is **logged, not failed**. A shared CI runner can lose a
//! whole scheduling quantum to a neighbour, and a test that failed on that
//! would be noise — and noise gets muted, which is how a real regression
//! eventually walks through unnoticed.
//!
//! Over 5× budget is a **hard failure**. Nothing the runner does to us
//! accounts for a 250 ms tick; that is the server's own fault and worth
//! stopping for.

use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::proto::{ClientMessage, Edit};
use tiamot_core::tick::TICK_DURATION;
use tiamot_core::{BlockPos, MaterialId};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];
const BOTS: usize = 4;
const TICKS: u64 = 200;

fn stone_id() -> u16 {
    let mut registry = tiamot_core::Registry::new();
    let mut id = MaterialId::AIR;
    for (index, name) in MATERIALS.iter().enumerate() {
        let assigned = registry.register(name).expect("register");
        if index == 0 {
            id = assigned;
        }
    }
    id.0
}

#[test]
fn two_hundred_ticks_under_four_bots_stays_within_budget() {
    let dir = std::env::temp_dir().join("tiamot-tick-stability");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("valid loopback address"),
        world_path: dir.clone(),
        max_players: 16,
        allowlist: Allowlist::open(),
        rcon: None,
        view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
        mods_path: None,
        seed: Some(1),
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start");

    let stone = stone_id();
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();
    let control = server.control().clone();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    runtime.block_on(async {
        let mut bots = Vec::new();
        for index in 0..BOTS {
            let mut bot = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
                .await
                .expect("connect");
            bot.join(&format!("Bot{index}")).await.expect("join");
            bots.push(bot);
        }

        let started = control.tick();
        let mut sent = 0u64;

        // Stream input every iteration and an edit every fourth, which is a
        // heavier edit rate than a human could produce and keeps the queue
        // genuinely occupied.
        for round in 0..TICKS {
            for (index, bot) in bots.iter_mut().enumerate() {
                bot.send(&ClientMessage::PlayerInput {
                    tick: control.tick(),
                    movement: [0.5, 0.0, -0.5],
                    look: [0.1, 0.2],
                    actions: 0,
                })
                .await
                .expect("send input");

                if round % 4 == 0 {
                    // Spread across chunks so the cache and the dirty set both
                    // do real work rather than hitting one hot chunk.
                    let offset = i32::try_from(round).expect("fits")
                        + i32::try_from(index).expect("fits") * 97;
                    // Load on the edit path, applied as the operator: what
                    // this measures is the cost of applying and broadcasting
                    // edits under a full server, not who asked for them.
                    assert!(server.seed_block(BlockPos::new(offset, 4, offset / 3), stone));
                    sent += 1;
                }
            }

            // Roughly one tick of wall time per round, so 200 rounds is 200
            // ticks of load rather than 200 rounds crammed into a moment.
            tokio::time::sleep(TICK_DURATION).await;
        }

        // Let the last edits drain.
        for _ in 0..40 {
            if control.tick() >= started + TICKS {
                break;
            }
            tokio::time::sleep(TICK_DURATION).await;
        }

        let ran = control.tick() - started;
        let slowest = Duration::from_micros(control.slowest_tick_micros());
        let over_budget = control.over_budget_ticks();
        let dropped = control.dropped();

        println!(
            "ticks={ran} edits_sent={sent} slowest={slowest:?} over_budget={over_budget} \
             dropped={dropped}"
        );

        assert!(
            ran >= TICKS,
            "the server should have run at least {TICKS} ticks, ran {ran}"
        );

        // Hard failure: nothing a shared runner does accounts for this.
        assert!(
            slowest < TICK_DURATION * 5,
            "a tick took {slowest:?}, over 5x the {TICK_DURATION:?} budget — that is the \
             server's own fault, not scheduling noise"
        );

        // Soft: logged, because CI runners lose quanta to their neighbours and
        // a test that failed on that would be muted rather than fixed.
        if slowest > TICK_DURATION * 2 {
            println!(
                "NOTE: slowest tick {slowest:?} exceeded 2x the {TICK_DURATION:?} budget; \
                 acceptable on a shared runner, worth watching if it persists locally"
            );
        }

        assert!(
            dropped < TICKS / 2,
            "the server dropped {dropped} of {ran} ticks — it is not keeping up at all"
        );

        for bot in bots {
            bot.disconnect().await;
        }
    });

    assert!(server.stop(), "the server should shut down cleanly");
}

#[test]
fn worldgen_under_a_joining_player_stays_inside_the_tick_budget() {
    // Chunk generation runs on the simulation thread and is now the most
    // expensive thing on it. A player joining at full view distance asks for
    // ~1800 chunks, which is the heaviest burst the server sees in normal play.
    //
    // Reports the numbers as a share of the 50 ms budget, per charter rule 18:
    // "0.4 ms" says nothing, "0.4 ms, 0.8% of a tick" says something.
    let dir = std::env::temp_dir().join("tiamot-worldgen-load");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let repo_mods = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("game/ should exist");

    let view = tiamot_core::interest::ViewDistance::DEFAULT;
    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 16,
        allowlist: Allowlist::open(),
        view_distance: view,
        mods_path: Some(repo_mods),
        seed: Some(7),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start");

    let control = server.control().clone();
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();
    let wanted =
        tiamot_core::interest::chunks_around(tiamot_core::BlockPos::new(0, 1, 0).chunk(), view)
            .len();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut alice =
                Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
                    .await
                    .expect("connect");
            alice.join("Alice").await.expect("join");

            // Take a decent bite of the interest set — enough to be a real
            // generation burst without making the test slow.
            let target = wanted.min(400);
            let received = alice
                .collect_chunks(target, Duration::from_secs(60))
                .await
                .expect("collect");

            let slowest = Duration::from_micros(control.slowest_tick_micros());
            let share = slowest.as_secs_f64() / TICK_DURATION.as_secs_f64() * 100.0;
            println!(
                "worldgen load: {} chunks streamed, slowest tick {slowest:?} \
                 ({share:.1}% of the {TICK_DURATION:?} budget), \
                 over_budget={} dropped={}",
                received.len(),
                control.over_budget_ticks(),
                control.dropped(),
            );

            assert!(
                received.len() >= target,
                "expected {target} chunks, got {}",
                received.len()
            );
            assert!(
                slowest < TICK_DURATION * 5,
                "a tick took {slowest:?} ({share:.1}% of budget) generating terrain — \
                 over 5x is the server's own fault, not scheduling noise"
            );
            if slowest > TICK_DURATION * 2 {
                println!("NOTE: slowest tick {slowest:?} exceeded 2x budget under generation load");
            }

            alice.disconnect().await;
        });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn four_bots_all_see_a_fourth_bots_edit() {
    // Broadcast fan-out, rather than the two-party case. A per-connection
    // subscription bug would show up here and not in the two-bot test.
    let dir = std::env::temp_dir().join("tiamot-fanout");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("valid loopback address"),
        world_path: dir,
        max_players: 16,
        allowlist: Allowlist::open(),
        rcon: None,
        view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
        mods_path: None,
        seed: Some(1),
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start");

    let stone = stone_id();
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut bots = Vec::new();
            for index in 0..BOTS {
                let mut bot =
                    Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
                        .await
                        .expect("connect");
                bot.join(&format!("Watcher{index}")).await.expect("join");
                bots.push(bot);
            }

            let edit = Edit::Block {
                pos: BlockPos::new(64, 65, 66),
                material: stone,
            };
            assert!(server.seed_block(BlockPos::new(64, 65, 66), stone));

            for (index, bot) in bots.iter_mut().enumerate() {
                let seen = bot
                    .next_block_delta(Duration::from_secs(5))
                    .await
                    .expect("wait")
                    .unwrap_or_else(|| panic!("bot {index} never saw the edit"));
                assert_eq!(seen, edit, "bot {index} saw the wrong edit");
            }

            for bot in bots {
                bot.disconnect().await;
            }
        });

    server.stop();
}
