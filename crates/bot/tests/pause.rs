// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Pausing stops the world, and resuming does not make up for lost time.
//!
//! **Singleplayer's pause**, which only an embedded server ever gets: a hosted
//! one has other people in it, and one of them opening a menu must not stop the
//! world for everybody.

use std::path::PathBuf;
use std::time::Duration;

use tiamot_core::identity::Allowlist;
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-pause-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 1,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: None,
        enabled_mods: None,
        seed: Some(9),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

#[test]
fn a_paused_world_does_not_tick_and_does_not_catch_up_afterwards() {
    // Two properties: the world stops, and resuming does not lurch.
    //
    // **The second one is currently guaranteed by a different mechanism** — the
    // accumulator drops ticks rather than chasing them, so removing the pause
    // loop's clock read does not make this fail. It is asserted anyway, because
    // it is an end-to-end property a player would notice and it should not
    // matter which layer keeps the promise. What the clock read buys is that a
    // pause is not logged as the simulation falling behind; see `sim::run`.
    let server = start("held");

    // Establish that it ticks at all first, or the assertions below pass on a
    // server that was never running.
    std::thread::sleep(Duration::from_millis(300));
    let running = server.control().tick();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        server.control().tick() > running,
        "the world was not ticking to begin with, so this test proves nothing"
    );

    server.set_paused(true);
    assert!(server.paused());
    // A tick already in flight may still land, so settle before reading.
    std::thread::sleep(Duration::from_millis(200));
    let held = server.control().tick();
    std::thread::sleep(Duration::from_millis(600));
    let during = server.control().tick();
    assert_eq!(
        during,
        held,
        "the world advanced {} ticks while paused",
        during - held
    );

    // Six hundred milliseconds is twelve ticks at 20 Hz; a banked accumulator
    // would run them all in the first instant after resuming.
    server.set_paused(false);
    // **Polled to a condition, not slept for a fixed span.** A hundred and
    // fifty milliseconds is three ticks on a quiet machine and can be none at
    // all on a loaded runner, which is how this failed on macOS alone while
    // ubuntu and windows passed. The anti-banking check below is unaffected:
    // it reads the FIRST tick after the world restarts, and a banked
    // accumulator would already have fired all twelve by then.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut after = held;
    while std::time::Instant::now() < deadline {
        after = server.control().tick();
        if after > held {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(after > held, "the world did not restart after unpausing");
    // **Bounded by the accumulator's own cap, and that is the honest bound.**
    //
    // This used to allow ten and claim it caught a banked accumulator. It could
    // not: `MAX_CATCH_UP_TICKS` is five, so twelve ticks of pause can never all
    // arrive however the pause is implemented, and removing the clock drain
    // from the paused loop leaves this test green. Checked by doing exactly
    // that.
    //
    // So the claim is right-sized. Two mechanisms enforce it — the paused loop
    // throws the clock reading away so there is nothing to catch up, and the
    // accumulator would cap and DISCARD the excess anyway — and this fails only
    // if both go. That is still worth pinning: it is the property a player
    // notices, and the bound moves with the constant rather than being a
    // number somebody chose.
    assert!(
        after - held <= u64::from(tiamot_core::tick::MAX_CATCH_UP_TICKS),
        "resuming fired {} ticks at once, over the {} the accumulator caps at, so the pause \
         was banked AND the cap is gone",
        after - held,
        tiamot_core::tick::MAX_CATCH_UP_TICKS
    );

    assert!(server.stop());
}
