// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Swarm load: twenty bots, sustained, with the server watched throughout.
//!
//! Task 07's stated test. The question is not raw speed — the macro benchmark
//! answers that — but whether a server under real concurrent load stays
//! *stable*: no tick blowing the budget by an order of magnitude, no memory
//! climbing without bound, and every bot finishing.
//!
//! # Why this is `#[ignore]` by default
//!
//! It takes a minute of wall clock, which does not belong in the `cargo test`
//! a developer runs twenty times an hour. CI runs it explicitly. A test that
//! makes the fast loop slow is a test people learn to skip.
//!
//! Run it with:
//!
//! ```console
//! cargo test -p bot --test swarm_load --release -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use tiamot_core::identity::Allowlist;
use tiamot_core::interest::ViewDistance;
use tiamot_core::tick::TICK_DURATION;
use tiamot_server::{ServerHandle, Settings};

/// Resident set size in kibibytes, on Linux.
///
/// Returns `None` elsewhere, and the memory assertion is skipped rather than
/// faked — a memory check that silently passes on macOS and Windows is worse
/// than one that says it did not run.
#[cfg(target_os = "linux")]
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn rss_kib() -> Option<u64> {
    None
}

/// The reference mods, which register the tools and generate the terrain.
fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

#[test]
#[ignore = "takes about a minute; CI runs it explicitly"]
fn twenty_bots_for_sixty_seconds_leave_the_server_healthy() {
    const BOTS: u32 = 20;
    const SECONDS: u64 = 60;

    let dir = std::env::temp_dir().join("tiamot-swarm-load");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 64,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        // The reference mods, because the swarm DIGS rather than writing blocks
        // into the world. Charter rule 1 leaves the engine with no tools of its
        // own, so a modless server is one where every bot fails at its first
        // dig — and `core_worldgen` is what puts ground under them to dig.
        //
        // This test ran modless until the light work landed, which was harmless
        // only for as long as `wander` could edit the world directly. Nightly
        // caught it; nothing in the fast CI loop could, because nothing else
        // runs this workload.
        mods_path: Some(reference_mods()),
        seed: Some(21),
        rcon: None,
        materials: vec!["load:stone".to_owned()],
    })
    .expect("start");

    let addr = server.local_addr();
    let control = server.control().clone();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    // Let startup settle, then take the baseline. Measuring from process start
    // would count the world opening and the first chunk generation as "growth".
    std::thread::sleep(Duration::from_secs(3));
    let _ = control.take_tick_samples();
    let rss_after_warmup = rss_kib();

    let results = runtime.block_on(async {
        let mut handles = Vec::new();
        for index in 0..BOTS {
            handles.push(tokio::spawn(bot::wander(
                addr,
                format!("load{index}"),
                index,
                Duration::from_secs(SECONDS),
                u64::from(index).wrapping_mul(0x9E37_79B9) | 1,
            )));
        }
        let mut healthy = 0;
        let mut failures = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(_)) => healthy += 1,
                Ok(Err(err)) => failures.push(err.to_string()),
                Err(err) => failures.push(format!("task panicked: {err}")),
            }
        }
        (healthy, failures)
    });

    let (healthy, failures) = results;
    let samples = control.take_tick_samples();
    let report = bot::bench::TickReport::from_samples(
        &samples,
        control.over_budget_ticks(),
        control.dropped(),
        BOTS,
        SECONDS,
    );
    let rss_after_load = rss_kib();

    println!("{}", report.to_table());
    println!("  bots healthy: {healthy}/{BOTS}");
    for failure in &failures {
        println!("  failure: {failure}");
    }
    if let (Some(before), Some(after)) = (rss_after_warmup, rss_after_load) {
        println!("  RSS: {before} KiB after warmup, {after} KiB after load");
    }

    assert_eq!(
        healthy, BOTS,
        "every bot should finish; failures: {failures:?}"
    );
    assert!(
        report.ticks > 0,
        "the server should have ticked; a run with no samples proves nothing"
    );

    // The hard gate. Nothing a shared runner does accounts for a tick five
    // times over budget; that is the server's own fault.
    let slowest = Duration::from_micros(report.max_us);
    assert!(
        slowest < TICK_DURATION * 5,
        "slowest tick was {slowest:?} ({} of the {TICK_DURATION:?} budget), over 5x",
        bot::bench::TickReport::budget_share(report.max_us)
    );

    // Memory must be bounded after warmup. A server whose RSS climbs with
    // uptime is one that dies overnight, and no throughput number matters then.
    match (rss_after_warmup, rss_after_load) {
        (Some(before), Some(after)) => {
            // Generous: twenty bots' worth of chunk cache and connection state
            // is real growth. What this catches is unbounded growth, not
            // working set.
            let limit = before.saturating_mul(3).max(before + 512 * 1024);
            assert!(
                after <= limit,
                "RSS grew from {before} KiB to {after} KiB under load, past the {limit} KiB \
                 bound. That is the shape of a leak rather than a working set."
            );
        }
        _ => println!("  (RSS unavailable on this platform; the memory check did not run)"),
    }

    assert!(server.stop(), "the server should shut down cleanly");
}
