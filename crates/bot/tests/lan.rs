// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A world opened to the network is reachable from it.
//!
//! **Reported from the window**: "I need a way to open a LAN server from the
//! menu before I join the game, so people can host LAN servers at home."
//!
//! The client's front screen ticks a box and the world it starts binds to every
//! interface instead of loopback. What this file checks is the half that can
//! silently be wrong: that binding is what actually decides reachability, and
//! that the default has not quietly become "open".

use std::path::{Path, PathBuf};

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-lan").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
        .join("game")
}

/// Where to dial a server that may be listening on everything.
///
/// **The unspecified address is somewhere to listen, not somewhere to connect**
/// — QUIC refuses it outright. The client hit exactly this the first time a
/// world was opened to the LAN: it could not join its own world. See
/// `own_address` in the client.
fn dial(listening: std::net::SocketAddr) -> std::net::SocketAddr {
    if listening.ip().is_unspecified() {
        std::net::SocketAddr::from(([127, 0, 0, 1], listening.port()))
    } else {
        listening
    }
}

/// A world bound the way the front screen binds one.
fn start(name: &str, bind: &str, max_players: u32) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: bind.parse().expect("an address"),
        world_path: scratch(name),
        max_players,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(3),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

#[test]
fn a_world_opened_to_the_network_takes_more_than_one_player() {
    // The "Open to LAN" tick box binds every interface and raises the player
    // cap: one is right for a world nobody else can reach and a baffling
    // refusal for one they can.
    let server = start("open", "0.0.0.0:0", 8);
    block_on(async {
        let mut first = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        first.join("First").await.expect("join");

        let mut second = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("a second machine should be able to connect");
        second
            .join("Second")
            .await
            .expect("a world open to the network takes a second player");

        first.disconnect().await;
        second.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_world_kept_to_this_machine_admits_one_player() {
    // The default. A second player is refused rather than crashing anything —
    // which is what makes the tick box mean something.
    let server = start("closed", "127.0.0.1:0", 1);
    block_on(async {
        let mut first = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        first.join("First").await.expect("join");

        let mut second = Bot::connect(
            dial(server.local_addr()),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        assert!(
            second.join("Second").await.is_err(),
            "a world kept to one machine let a second player in"
        );

        first.disconnect().await;
    });
    server.stop();
}
