// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Lamp churn: Task 10's load test, and the one that watches lighting.
//!
//! # Why lamps rather than any other block
//!
//! Because lighting is not free and lamps are the expensive case. Benchmarked
//! on the reference machine, digging an ordinary block relights in 0.00012 ms
//! and **breaking a lamp costs 0.271 ms** — a flood that has to walk back
//! everything the lamp was lighting, and the only edit in the game whose cost
//! is set by how far its light reached rather than by how big it is.
//!
//! Fifty players is charter rule 18's number. Twenty bots doing nothing but
//! placing and removing lamps is a deliberately worse workload than fifty
//! players would produce, because no real player spends a minute doing only
//! this.
//!
//! # What this asserts, and what it deliberately does not
//!
//! It asserts the SHAPE of the tick distribution: no tick over the budget, no
//! dropped ticks, every bot finishing. It does not assert a time, because a
//! shared CI runner cannot promise one and a gate that fires on a neighbour's
//! build gets muted. The macro benchmark is where absolute numbers live.
//!
//! Run it the way nightly does:
//!
//! ```console
//! cargo test -p bot --test lamp_load --release -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::BlockPos;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::tick::TICK_DURATION;
use tiamot_server::{ServerHandle, Settings};

/// Bots, each with its own lamp.
const BOTS: u32 = 20;

/// How long they churn for.
const SECONDS: u64 = 30;

/// The block a bot stands on. The reference generator fills below its
/// heightmap, so the highest solid block is at y = -1.
const GROUND: i32 = -1;

/// How far apart the bots' lamps are, in blocks.
///
/// Three, which is deliberately close: lamps within reach of each other means
/// every relight has other lamps' light to walk through, which is the expensive
/// case rather than the tidy one. Not so close that two bots share a block —
/// see `bench::standard_session` for what that does to a dig.
const SPACING: i32 = 3;

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// The world id of `core:lamp`.
///
/// Looked up from the material table the server sends rather than assumed: ids
/// come from the mod set's registration order, and a constant here would be a
/// test of that order rather than of lighting.
async fn lamp_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server should have sent a material table")
        .into_iter()
        .find(|entry| entry.name.ends_with(":lamp"))
        .map(|entry| entry.id)
        .expect("the reference mods should register a lamp")
}

#[test]
#[ignore = "takes half a minute; nightly runs it explicitly"]
fn twenty_bots_churning_lamps_keep_the_tick_inside_its_budget() {
    let dir = std::env::temp_dir().join("tiamot-lamp-load");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 64,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(31),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start");

    let addr = server.local_addr();
    let control = server.control().clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    // Let the world settle before anything is measured: the first ticks include
    // mod loading and first-visit chunk generation, which are real and are not
    // what this is watching.
    std::thread::sleep(Duration::from_secs(3));

    let (healthy, failures, edits) = runtime.block_on(async {
        // One bot goes first to learn the lamp's id and to seed every bot's
        // lamp into the world. A bot can only build with what it has dug, so
        // there has to be a lamp there before anyone can churn one.
        let scout = Bot::connect_trusting(addr, Identity::generate().expect("identity"))
            .await
            .expect("connect");
        let mut scout = scout;
        scout.join("scout").await.expect("join");
        let lamp = lamp_id(&scout).await;
        for index in 0..BOTS {
            let x = i32::try_from(index).unwrap_or(0) * SPACING;
            assert!(
                server.seed_block(BlockPos::new(x, GROUND, 0), lamp),
                "the world should accept a seeded lamp"
            );
        }
        scout.disconnect().await;

        let mut handles = Vec::new();
        for index in 0..BOTS {
            handles.push(tokio::spawn(churn(addr, index, lamp)));
        }

        let mut healthy = 0;
        let mut failures = Vec::new();
        let mut edits = 0u64;
        for handle in handles {
            match handle.await {
                Ok(Ok(count)) => {
                    healthy += 1;
                    edits += count;
                }
                Ok(Err(err)) => failures.push(err),
                Err(err) => failures.push(format!("task panicked: {err}")),
            }
        }
        (healthy, failures, edits)
    });

    let samples = control.take_tick_samples();
    let report = bot::bench::TickReport::from_samples(
        &samples,
        control.over_budget_ticks(),
        control.dropped(),
        BOTS,
        SECONDS,
    );

    println!("{}", report.to_table());
    println!("  bots healthy: {healthy}/{BOTS}, lamp edits: {edits}");
    for failure in &failures {
        println!("  failure: {failure}");
    }

    assert_eq!(
        healthy, BOTS,
        "every bot should finish; failures: {failures:?}"
    );
    assert!(
        edits > BOTS.into(),
        "only {edits} lamp edits landed, which is not a churn test"
    );
    assert!(
        report.ticks > 0,
        "the server should have ticked; a run with no samples proves nothing"
    );
    assert_eq!(
        report.over_budget, 0,
        "{} of {} ticks ran over the {TICK_DURATION:?} budget while lamps churned",
        report.over_budget, report.ticks
    );
    assert_eq!(
        report.dropped, 0,
        "the server dropped {} ticks, so it could not keep up with the lighting",
        report.dropped
    );

    assert!(server.stop());
}

/// Walks until the bot can actually reach `pos`, or says it could not.
///
/// **`move_to` is best effort and does not promise arrival** — it gives up
/// after forty legs or five stalled ones and returns `Ok` from wherever it got
/// to, which is the right contract for a caller that just wants to move. A
/// caller that has to be within reach must check, or every edit it makes is
/// refused and the first dig reports "nothing broke at all" thirty seconds
/// later, naming a block that was solid the whole time.
///
/// Returns quickly when the bot is already there, so calling it every round
/// costs a settle.
async fn stand_beside(bot: &mut Bot, pos: BlockPos) -> Result<(), String> {
    /// Comfortably inside `phys::ray::REACH`, in blocks. The server measures
    /// from the eye to the target cell; this measures block centres, so the
    /// margin covers the difference rather than pretending to model it.
    const CLOSE: f64 = 3.0;

    for _ in 0..5 {
        let here = bot.settle().await.map_err(|err| err.to_string())?.block();
        let (dx, dy, dz) = (
            f64::from(here.x - pos.x),
            f64::from(here.y - pos.y),
            f64::from(here.z - pos.z),
        );
        if (dx * dx + dy * dy + dz * dz).sqrt() <= CLOSE {
            return Ok(());
        }
        bot.move_to(pos.x as f32, 0.0, (pos.z + 2) as f32)
            .await
            .map_err(|err| err.to_string())?;
    }
    let here = bot.settle().await.map_err(|err| err.to_string())?.block();
    Err(format!(
        "could not get within reach of {pos:?}; gave up at {here:?}"
    ))
}

/// One bot: dig its lamp out and put it straight back, for the duration.
///
/// Returns how many edits it landed, so the test can tell a churn from a bot
/// that connected and did nothing.
async fn churn(addr: std::net::SocketAddr, index: u32, lamp: u16) -> Result<u64, String> {
    let identity = Identity::generate().map_err(|err| err.to_string())?;
    let mut bot = Bot::connect_trusting(addr, identity)
        .await
        .map_err(|err| err.to_string())?;
    bot.join(&format!("lamp{index}"))
        .await
        .map_err(|err| err.to_string())?;

    let x = i32::try_from(index).unwrap_or(0) * SPACING;
    let pos = BlockPos::new(x, GROUND, 0);

    // Stand next to it, and CHECK. The server bounds digging and placing by
    // reach, and a bot that never walked would be refused every edit from spawn
    // onward — which is how this failed: `move_to` is best effort and says so,
    // giving up after forty legs or five stalled ones and returning `Ok` from
    // wherever it got to. `churn` called it once and then dug for thirty
    // seconds from a place that could not reach the lamp. It was always the
    // last bots to join that failed, which is exactly the ones whose walk
    // competes with nineteen others spawning.
    stand_beside(&mut bot, pos).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(SECONDS);
    let mut edits = 0;
    // Re-checked every round below, because nothing pins a body in place: the
    // ground it is standing on is the block it keeps digging out.
    while tokio::time::Instant::now() < deadline {
        // **The lamp goes back before the next dig, or there is nothing to
        // dig.** A dig at an empty block has nothing to broadcast, so the bot
        // waits out its whole patience for a delta that will never come and
        // fails thirty seconds later with "nothing broke at all". That is what
        // four consecutive red nightlies were: the dig credited a lamp, the
        // credit had not been RECEIVED when `units_of` was asked, the place was
        // skipped, and the next round dug a hole that was already a hole.
        stand_beside(&mut bot, pos).await?;
        if !bot.block_is_empty(pos) {
            bot.dig_block(pos).await.map_err(|err| err.to_string())?;
            edits += 1;
        }

        // Waited for rather than sampled. The reference lamp drops itself, so
        // the credit is coming; what varies under twenty bots is when. Asking
        // once and moving on is what left the hole.
        let held = bot
            .await_inventory(Duration::from_secs(2))
            .await
            .map_err(|err| err.to_string())?
            .into_iter()
            .filter(|stack| stack.material == lamp)
            .map(|stack| stack.units)
            .sum::<u32>();
        if held >= tiamot_core::UNITS_PER_BLOCK {
            bot.place(pos, lamp).await.map_err(|err| err.to_string())?;
            edits += 1;
        }
        bot.sleep_ticks(1).await;
    }

    bot.disconnect().await;
    Ok(edits)
}
