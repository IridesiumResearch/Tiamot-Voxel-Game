// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Chunk streaming to a joining player, over a real server.
//!
//! A client that completes the join flow and then receives nothing has a
//! working handshake and no game. These tests check the world actually arrives.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::{self, ViewDistance};
use tiamot_core::proto::ServerMessage;
use tiamot_core::{BlockPos, MaterialId};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];

fn world_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-stream-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The reference mods, whose generator fills everything below y = 0.
fn reference_mods() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

/// A server whose world has GROUND, so a joining player stands on it.
///
/// **This is the default, and running without mods is the special case.** A
/// world with no mods is pure air, and since Task 09 gave players physics a
/// player in one falls forever — correct behaviour, since there is nothing to
/// stand on, but it means their interest set drifts downwards for as long as
/// the test runs. Every assertion here about *which* chunks arrive is really
/// an assertion about a stationary observer, and a falling one fails it
/// eventually. It first showed up as one test failing only on the macOS
/// runner, which is slower, so the fall got further before the collection
/// finished.
fn start(name: &str, view: ViewDistance) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world_dir(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(1),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
}

/// A server with no mods, and therefore no ground.
///
/// Only for tests that do not care where the player ends up. See [`start`].
#[expect(
    dead_code,
    reason = "kept for tests that genuinely want an empty world"
)]
fn start_without_ground(name: &str, view: ViewDistance) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world_dir(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: None,
        enabled_mods: None,
        seed: Some(1),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
}

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
fn a_joining_player_receives_the_world_around_spawn() {
    let view = ViewDistance::MINIMUM;
    let server = start("receives-world", view);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
    let expected = interest::chunks_around(spawn_chunk, view);

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        let received = alice
            .collect_chunks(expected.len(), Duration::from_secs(20))
            .await
            .expect("collect");

        assert_eq!(
            received.len(),
            expected.len(),
            "the whole spawn neighbourhood should arrive, got {} of {}",
            received.len(),
            expected.len()
        );

        let received_set: std::collections::BTreeSet<_> = received.iter().copied().collect();
        let expected_set: std::collections::BTreeSet<_> = expected.iter().copied().collect();
        assert_eq!(
            received_set, expected_set,
            "the delivered set must match the interest set exactly"
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn the_chunk_under_the_player_arrives_first() {
    // A player dropping in wants the ground under their feet before the
    // horizon. Streaming in an arbitrary order means falling through the world
    // while the sky loads.
    let view = ViewDistance::MINIMUM;
    let server = start("nearest-first", view);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        let received = alice
            .collect_chunks(4, Duration::from_secs(20))
            .await
            .expect("collect");

        assert_eq!(
            received.first(),
            Some(&spawn_chunk),
            "the spawn chunk must arrive first, got {received:?}"
        );

        // And the rest must be weakly increasing in distance.
        let mut previous = 0;
        for pos in &received {
            let distance = interest::squared_distance(spawn_chunk, *pos);
            assert!(
                distance >= previous,
                "chunk {pos:?} at distance {distance} arrived after {previous}"
            );
            previous = distance;
        }

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn no_chunks_flow_before_the_player_joins_the_world() {
    // Streaming starts at InWorld, not at connect. A peer that has not
    // authenticated must not be able to read the map.
    let server = start("no-early-chunks", ViewDistance::MINIMUM);

    block_on(async {
        let mut intruder = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");

        // Complete only the handshake, never JoinWorld.
        let nonce = intruder.hello("Lurker").await.expect("challenge");
        intruder.authenticate(&nonce).await.expect("auth");
        intruder
            .recv_until(|m| matches!(m, ServerMessage::ModManifest { .. }))
            .await
            .expect("manifest");

        let chunks = intruder
            .collect_chunks(1, Duration::from_secs(2))
            .await
            .expect("collect");
        assert!(
            chunks.is_empty(),
            "a player who has not joined the world must receive no chunks, got {chunks:?}"
        );

        intruder.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_streamed_chunk_carries_the_edits_made_to_it() {
    // The blob a client receives is the same format the world stores. If the
    // streamed chunk did not reflect an applied edit, a player joining after a
    // build would see the world as it was before it.
    let view = ViewDistance::MINIMUM;
    let server = start("streamed-edits", view);
    let stone = stone_id();
    let pos = BlockPos::new(2, 3, 4);

    block_on(async {
        let mut builder = join(&server, "Builder").await;
        // The operator arranges it. What is under test is that a player joining
        // LATER receives the edited chunk, which does not care who made the
        // edit — only that the world has it.
        assert!(server.seed_block(pos, stone));
        builder
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("the edit must be applied");

        // A second player joins AFTER the edit and must see it in their chunk.
        let mut latecomer = join(&server, "Latecomer").await;
        let _ = latecomer
            .collect_chunks(
                interest::chunks_around(BlockPos::new(0, 1, 0).chunk(), view).len(),
                Duration::from_secs(20),
            )
            .await
            .expect("collect");

        let mut registry = tiamot_core::Registry::new();
        for name in MATERIALS {
            registry.register(name).expect("register");
        }
        let db = tiamot_core::WorldDb::open_in_memory(&mut registry).expect("id map");

        let chunk = latecomer
            .decode_chunk(pos.chunk(), db.materials())
            .expect("the streamed chunk should decode");
        assert_eq!(
            chunk.get_block(pos).expect("in chunk").subnode(0),
            MaterialId(stone),
            "a player joining after a build must see what was built"
        );

        builder.disconnect().await;
        latecomer.disconnect().await;
    });

    server.stop();
}

#[test]
fn streaming_respects_the_configured_view_distance() {
    // A bigger view must send more chunks, and the counts must match the
    // interest geometry rather than being whatever the loop happened to do.
    let small = ViewDistance {
        horizontal: 1,
        vertical: 1,
    };
    let large = ViewDistance {
        horizontal: 2,
        vertical: 1,
    };
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();

    let expected_small = interest::chunks_around(spawn_chunk, small).len();
    let expected_large = interest::chunks_around(spawn_chunk, large).len();
    assert!(expected_large > expected_small, "the test needs two sizes");

    for (name, view, expected) in [
        ("view-small", small, expected_small),
        ("view-large", large, expected_large),
    ] {
        let server = start(name, view);
        block_on(async {
            let mut alice = join(&server, "Alice").await;
            let received = alice
                .collect_chunks(expected, Duration::from_secs(30))
                .await
                .expect("collect");
            assert_eq!(
                received.len(),
                expected,
                "at view {view:?} the client should receive {expected} chunks"
            );

            // Nothing beyond the configured range.
            for pos in &received {
                assert!(
                    interest::contains(spawn_chunk, view, *pos),
                    "{pos:?} is outside the configured view distance"
                );
            }
            alice.disconnect().await;
        });
        server.stop();
    }
}

#[test]
fn each_chunk_is_sent_only_once() {
    // A streamer that forgot what it had sent would resend the whole interest
    // set every pass and saturate the link, while still passing every test that
    // only checks the set of chunks received.
    let view = ViewDistance::MINIMUM;
    let server = start("no-duplicates", view);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
    let expected = interest::chunks_around(spawn_chunk, view).len();

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let _ = alice
            .collect_chunks(expected, Duration::from_secs(20))
            .await
            .expect("collect");

        // Keep reading well past completion. Nothing more should arrive.
        let extra = alice
            .collect_chunks(1, Duration::from_secs(2))
            .await
            .expect("collect");
        assert!(
            extra.is_empty(),
            "a completed streamer must stop sending, got {extra:?}"
        );

        let all = alice.chunks_received();
        let unique: std::collections::BTreeSet<_> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "chunks were sent more than once: {} sends for {} chunks",
            all.len(),
            unique.len()
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn two_players_both_get_a_full_world() {
    // The shared per-tick budget must be shared, not monopolised: a second
    // player joining while the first is still streaming must also finish.
    let view = ViewDistance::MINIMUM;
    let server = start("two-players", view);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
    let expected = interest::chunks_around(spawn_chunk, view).len();

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        let alice_chunks = alice
            .collect_chunks(expected, Duration::from_secs(30))
            .await
            .expect("collect");
        let bob_chunks = bob
            .collect_chunks(expected, Duration::from_secs(30))
            .await
            .expect("collect");

        assert_eq!(alice_chunks.len(), expected, "Alice should be fully loaded");
        assert_eq!(bob_chunks.len(), expected, "Bob should be fully loaded");

        alice.disconnect().await;
        bob.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_streamed_chunk_carries_generated_terrain() {
    // The whole point of worldgen reaching the client. Before this was wired,
    // a joining player received chunks that decoded fine and contained nothing
    // — a working transport delivering an empty world.
    //
    // The reference generator is solid below y=0 and air above, so the chunk
    // under spawn must be solid and the one above it must not.
    let view = ViewDistance::MINIMUM;
    let dir = world_dir("generated-terrain");
    let repo_mods = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the game/ directory should exist");

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: Some(repo_mods),
        enabled_mods: None,
        seed: Some(7),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
        let expected = interest::chunks_around(spawn_chunk, view).len();
        let received = alice
            .collect_chunks(expected, Duration::from_secs(30))
            .await
            .expect("collect");
        assert_eq!(received.len(), expected, "the neighbourhood should arrive");

        // Every block the reference mods register, **in their registration
        // order**, because that order is what assigns world ids — a registry
        // missing one of them cannot translate the blob at all, and the error
        // says "could not translate material ids" rather than naming the block
        // that is absent.
        let mut registry = tiamot_core::Registry::new();
        let white = registry.register("core:white").expect("register");
        for name in ["core:lamp", "core:crumb", "core:pitch", "core:ground"] {
            registry.register(name).expect("register");
        }
        let db = tiamot_core::WorldDb::open_in_memory(&mut registry).expect("id map");

        // Below the surface: solid.
        let underground = BlockPos::new(0, -4, 0);
        let chunk = alice
            .decode_chunk(underground.chunk(), db.materials())
            .expect("the underground chunk should decode");
        assert_eq!(
            chunk.get_block(underground).expect("in chunk").subnode(0),
            white,
            "the generator fills everything below y=0; the client received air instead"
        );

        // Above the surface: air.
        let sky = BlockPos::new(0, 20, 0);
        let chunk = alice
            .decode_chunk(sky.chunk(), db.materials())
            .expect("the sky chunk should decode");
        assert_eq!(
            chunk.get_block(sky).expect("in chunk").subnode(0),
            MaterialId::AIR,
            "everything above y=0 should be air"
        );

        alice.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn generated_terrain_is_the_same_after_a_restart() {
    // Generated chunks are persisted rather than regenerated, so that a mod or
    // engine change cannot silently rewrite land a player has already built
    // next to. This is that guarantee, end to end.
    let view = ViewDistance::MINIMUM;
    let dir = world_dir("terrain-restart");
    let repo_mods = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("game/ should exist");

    let settings = |mods: Option<PathBuf>| Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir.clone(),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: mods,
        enabled_mods: None,
        seed: Some(7),
        rcon: None,
        materials: Vec::new(),
    };

    // First run: generate and store the neighbourhood.
    let server = ServerHandle::start(&settings(Some(repo_mods))).expect("start");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let expected = interest::chunks_around(BlockPos::new(0, 1, 0).chunk(), view).len();
        let received = alice
            .collect_chunks(expected, Duration::from_secs(30))
            .await
            .expect("collect");
        assert_eq!(received.len(), expected);
        alice.disconnect().await;
    });
    assert!(server.stop(), "clean shutdown");

    // Second run with NO mods at all. If the terrain were regenerated rather
    // than loaded, everything would come back as air.
    let server = ServerHandle::start(&settings(None)).expect("restart");
    block_on(async {
        let mut bob = join(&server, "Bob").await;
        let expected = interest::chunks_around(BlockPos::new(0, 1, 0).chunk(), view).len();
        let _ = bob
            .collect_chunks(expected, Duration::from_secs(30))
            .await
            .expect("collect");

        // As above: every reference block, in registration order.
        let mut registry = tiamot_core::Registry::new();
        let white = registry.register("core:white").expect("register");
        for name in ["core:lamp", "core:crumb", "core:pitch", "core:ground"] {
            registry.register(name).expect("register");
        }
        let db = tiamot_core::WorldDb::open_in_memory(&mut registry).expect("id map");

        let underground = BlockPos::new(0, -4, 0);
        let chunk = bob
            .decode_chunk(underground.chunk(), db.materials())
            .expect("decode");
        assert_eq!(
            chunk.get_block(underground).expect("in chunk").subnode(0),
            white,
            "stored terrain must survive a restart, even with the generator gone"
        );

        bob.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_player_who_leaves_mid_stream_does_not_wedge_the_server() {
    // Requests outlive the connection that made them. If the simulation
    // blocked on delivering to a departed client, one impatient player would
    // stall everyone.
    let server = start("leaves-mid-stream", ViewDistance::DEFAULT);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();

    block_on(async {
        // Join and immediately abandon, several times over, while the interest
        // set is far from drained.
        // Distinct names: these are distinct identities, and name binding is
        // first-come, so reusing one would be refused as name theft — which is
        // correct behaviour and would make this test about the wrong thing.
        for index in 0..5 {
            let bot = join(&server, &format!("Ghost{index}")).await;
            bot.abandon();
        }

        // The server must still serve a new player normally.
        let mut alice = join(&server, "Alice").await;
        let received = alice
            .collect_chunks(8, Duration::from_secs(20))
            .await
            .expect("collect");
        assert!(
            received.len() >= 8,
            "the server should still stream after clients left mid-stream, got {}",
            received.len()
        );
        assert_eq!(received[0], spawn_chunk);

        alice.disconnect().await;
    });

    assert!(server.stop(), "the server should still shut down cleanly");
}

#[test]
fn walking_far_enough_streams_new_chunks_and_unloads_the_ones_left_behind() {
    // The gap Task 08 shipped with, and the reason the client's world stopped
    // at the spawn neighbourhood however far you flew: the server pinned each
    // player's interest set to spawn because nothing told it where they were.
    // Now the simulation moves a body and the streamer follows it.
    //
    // Both halves are asserted. New chunks arriving proves the interest set
    // moved; unloads arriving proves it MOVED rather than merely grew, which
    // is the difference between streaming and a memory leak.
    let view = ViewDistance::MINIMUM;
    let server = start("walking-streams", view);

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        // Let the spawn neighbourhood finish arriving first, so what follows
        // is attributable to walking rather than to the join.
        let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
        let at_spawn = interest::chunks_around(spawn_chunk, view);
        alice
            .collect_chunks(at_spawn.len(), Duration::from_secs(30))
            .await
            .expect("the spawn neighbourhood should arrive");
        let before: std::collections::BTreeSet<_> = alice.chunks_received().into_iter().collect();

        // Two chunks' worth of walking east. A player walks 0.215 blocks a
        // tick, so 16 blocks takes about 75 ticks.
        let ended = alice
            .walk([1.0, 0.0, 0.0], 0, 200)
            .await
            .expect("walk east");

        assert!(
            ended.chunk.x > spawn_chunk.x,
            "the server never moved the player out of the spawn chunk: {ended:?}"
        );

        let after: std::collections::BTreeSet<_> = alice.chunks_received().into_iter().collect();
        let fresh: Vec<_> = after.difference(&before).copied().collect();
        assert!(
            !fresh.is_empty(),
            "walked {} chunks east and the world never followed",
            ended.chunk.x - spawn_chunk.x
        );
        assert!(
            fresh.iter().any(|pos| pos.x > spawn_chunk.x),
            "new chunks arrived but none of them were east of spawn: {fresh:?}"
        );

        let unloaded = alice
            .received()
            .into_iter()
            .filter(|message| matches!(message, ServerMessage::ChunkUnload { .. }))
            .count();
        assert!(
            unloaded > 0,
            "nothing was unloaded, so the interest set grew rather than moved"
        );
    });

    assert!(server.stop());
}

#[test]
fn a_client_that_talks_constantly_still_receives_its_world() {
    // A regression test for a starvation bug that made a client join, receive
    // its material table, and then never receive a single chunk.
    //
    // The streaming beat was a `sleep` constructed INSIDE the connection's
    // `tokio::select!`, so it was rebuilt from zero every time any other branch
    // won. A peer that said anything more often than the beat therefore starved
    // it forever — the timer never got 50 ms of quiet in which to elapse.
    //
    // Nothing sent that often until Task 09 gave the client an input per tick.
    // Note the shape this test needs: a BURST of inputs does not reproduce it,
    // because the quiet afterwards lets the timer fire. It has to be a steady
    // trickle, which is exactly what a player holding a key produces.
    let view = ViewDistance::MINIMUM;
    let server = start("constant-talker", view);
    let spawn_chunk = BlockPos::new(0, 1, 0).chunk();
    let expected = interest::chunks_around(spawn_chunk, view).len();

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut tick = 1;
        while std::time::Instant::now() < deadline {
            // Faster than the 50 ms beat, which is the whole point.
            alice
                .send(&tiamot_core::proto::ClientMessage::PlayerInput {
                    tick,
                    movement: [0.0, 0.0, 0.0],
                    look: [0.0, 0.0],
                    actions: 0,
                })
                .await
                .expect("send input");
            tick += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;

            if alice.chunks_received().len() >= expected {
                break;
            }
        }

        assert!(
            alice.chunks_received().len() >= expected,
            "a client sending an input every 20 ms received {} of {expected} chunks; the \
             streaming beat is being starved by its own traffic",
            alice.chunks_received().len()
        );
    });

    assert!(server.stop());
}
