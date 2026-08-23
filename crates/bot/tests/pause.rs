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
    std::thread::sleep(Duration::from_millis(150));
    let after = server.control().tick();
    assert!(after > held, "the world did not restart after unpausing");
    assert!(
        after < held + 10,
        "resuming fired {} ticks at once, so the pause was banked rather than \
         discarded",
        after - held
    );

    assert!(server.stop());
}
