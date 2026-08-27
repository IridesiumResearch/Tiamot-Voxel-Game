// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Players seeing each other, over a real server.
//!
//! # Why players are entities
//!
//! Charter rule 2 says there is exactly one simulation. There was not exactly
//! one *roster*: a player's body lived in `transport::Shared::bodies` and every
//! mob lived in the entity store, so anything asking "what is near me" — a mod,
//! a client's renderer, the replication tracker — had to ask twice and would
//! only ever get one of the two answers. Mirroring each body into the entity
//! store each tick makes the question have one answer.
//!
//! The mirror is deliberately thin: never saved, never stepped, never dirtying
//! a chunk. `server::ent::Population::transient` says what each of those would
//! otherwise cost.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-other-players").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The smallest mod set that makes a world worth standing in: ground, so a
/// player is not falling, and nothing else.
fn write_ground(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("ground");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"ground\"\nname = \"Ground\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        "local rock = game.register_block{ id = \"rock\" }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_below_heightmap(game.flat_heightmap(0), rock)\n\
         end)\n",
    )
    .expect("script");
    root
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(write_ground(name)),
        enabled_mods: None,
        seed: Some(5),
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

async fn join(server: &ServerHandle, name: &str) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join(name).await.expect("join");
    bot
}

#[test]
fn two_players_are_told_about_each_other_and_not_about_themselves() {
    let server = start("pair");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        // Each is told about the other, drawn as the engine's humanoid and
        // labelled with the name the OTHER one claimed — which is the whole
        // proof that nametags resolve against the roster rather than being
        // sent as a UUID or dropped.
        let seen_by_alice = alice
            .expect_entity(|entity| entity.nametag.as_deref() == Some("Bob"), PATIENCE)
            .await
            .expect("Alice should be told about Bob");
        assert_eq!(seen_by_alice.model.as_deref(), Some("engine:humanoid"));
        assert!(
            seen_by_alice.collider.is_some(),
            "a body with no box is a body a client cannot cull"
        );

        bob.expect_entity(
            |entity| entity.nametag.as_deref() == Some("Alice"),
            PATIENCE,
        )
        .await
        .expect("Bob should be told about Alice");

        // And neither is told about themselves. Checked after the positive
        // assertion, so this is not a race the test happened to win: by now
        // both have received a full pass of entity messages.
        assert!(
            !alice
                .entities()
                .values()
                .any(|entity| entity.nametag.as_deref() == Some("Alice")),
            "Alice was told where Alice is; a client drawing itself sees the \
             inside of its own head"
        );
        assert!(
            !bob.entities()
                .values()
                .any(|entity| entity.nametag.as_deref() == Some("Bob")),
            "Bob was told where Bob is"
        );
    });
}

#[test]
fn a_player_who_leaves_stops_being_drawn() {
    let server = start("leaver");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let bob = join(&server, "Bob").await;

        let seen = alice
            .expect_entity(|entity| entity.nametag.as_deref() == Some("Bob"), PATIENCE)
            .await
            .expect("Alice should be told about Bob");

        drop(bob);

        // The despawn has to arrive, or Bob stands in Alice's world for ever —
        // and a long-running server accumulates one of him per person who has
        // ever connected.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if !alice.entities().contains_key(&seen.id) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Bob disconnected and his body is still standing there"
            );
            let _ = tokio::time::timeout(Duration::from_millis(200), alice.recv()).await;
        }
    });
}
