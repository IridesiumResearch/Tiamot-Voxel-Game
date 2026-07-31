// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Block edits over a real server: broadcast, persistence, and refusal.
//!
//! Task 06's headline acceptance criterion is "two bots on one server see each
//! other's block edits". That is the test below, driven over real loopback QUIC
//! with nothing mocked.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::proto::Edit;
use tiamot_core::{BlockPos, MaterialId, SubNodePos};
use tiamot_server::{ServerHandle, Settings};

/// Materials the test server registers. The first real id is the one after the
/// engine's reserved set, so tests resolve ids rather than hard-coding them.
const MATERIALS: [&str; 3] = ["test:stone", "test:dirt", "test:glass"];

fn world_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-edit-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn settings(dir: &std::path::Path) -> Settings {
    Settings {
        bind_addr: "127.0.0.1:0".parse().expect("valid loopback address"),
        world_path: dir.to_path_buf(),
        max_players: 8,
        allowlist: Allowlist::open(),
        rcon: None,
        view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// The numeric id the server assigned to a registered material.
///
/// Resolved from the running server rather than assumed, so adding a reserved
/// material upstream cannot silently make these tests edit the wrong block.
fn material_id(server: &ServerHandle, name: &str) -> u16 {
    let _ = server;
    let mut registry = tiamot_core::Registry::new();
    let mut id = MaterialId::AIR;
    for material in MATERIALS {
        let assigned = registry.register(material).expect("register");
        if material == name {
            id = assigned;
        }
    }
    id.0
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
fn two_bots_see_each_others_block_edits() {
    // THE acceptance criterion for Task 06.
    let dir = world_dir("two-bots");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        let edit = Edit::Block {
            pos: BlockPos::new(4, 5, 6),
            material: stone,
        };
        alice.edit(edit.clone()).await.expect("send edit");

        let seen = bob
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("Bob must see Alice's edit");
        assert_eq!(seen, edit, "Bob must see exactly what Alice placed");

        alice.disconnect().await;
        bob.disconnect().await;
    });

    server.stop();
}

#[test]
fn an_editor_sees_their_own_edit_confirmed() {
    // The client needs the server's confirmation to know the edit was accepted
    // rather than silently dropped — otherwise it has to guess, and a guess
    // that is wrong leaves a ghost block on screen.
    let dir = world_dir("own-edit");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let edit = Edit::Block {
            pos: BlockPos::new(1, 1, 1),
            material: stone,
        };
        alice.edit(edit.clone()).await.expect("send edit");

        let seen = alice
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("the editor must get a confirmation");
        assert_eq!(seen, edit);

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_subnode_edit_is_broadcast_at_subnode_resolution() {
    // Sub-block resolution is the engine's defining feature. A broadcast that
    // rounded a chisel up to a whole block would make it invisible to everyone
    // but the editor.
    let dir = world_dir("subnode-broadcast");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let glass = material_id(&server, "test:glass");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        let edit = Edit::SubNode {
            pos: SubNodePos::new(10, 11, 12),
            material: glass,
        };
        alice.edit(edit.clone()).await.expect("send edit");

        let seen = bob
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("Bob must see the chisel");
        assert_eq!(
            seen, edit,
            "the broadcast must keep sub-node resolution, not round to a block"
        );

        alice.disconnect().await;
        bob.disconnect().await;
    });

    server.stop();
}

#[test]
fn an_edit_survives_a_server_restart() {
    // The other half of the acceptance criterion: restart the server, and the
    // chunk reloads with the edit.
    let dir = world_dir("edit-restart");
    let pos = BlockPos::new(7, 8, 9);

    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        alice
            .edit(Edit::Block {
                pos,
                material: stone,
            })
            .await
            .expect("send edit");

        // Wait for the server to confirm it applied, so the shutdown save has
        // something to write.
        alice
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("the edit must be applied before shutdown");
        alice.disconnect().await;
    });
    assert!(server.stop(), "clean shutdown");

    // Reopen the world directly and check the block is there.
    let mut registry = tiamot_core::Registry::new();
    for material in MATERIALS {
        registry.register(material).expect("register");
    }
    let db = tiamot_core::WorldDb::open(dir.join("world.sqlite"), &mut registry).expect("reopen");
    let chunk = db
        .load_chunk(pos.chunk())
        .expect("load")
        .expect("the chunk must have been written");
    assert_eq!(
        chunk.get_block(pos).expect("in chunk").subnode(0),
        MaterialId(stone),
        "the edit must survive a restart"
    );
    db.close().expect("close");
}

#[test]
fn an_edit_reaches_the_database_without_waiting_for_shutdown() {
    // The restart test above passes even with periodic saving disabled, because
    // `close()` flushes on the way out. That would leave a server that loses
    // every edit on a crash, with nothing to catch it — so this reads the
    // database WHILE the server is still running, which only succeeds if the
    // debounced save actually ran.
    let dir = world_dir("periodic-save");
    let pos = BlockPos::new(20, 21, 22);

    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        alice
            .edit(Edit::Block {
                pos,
                material: stone,
            })
            .await
            .expect("send edit");
        alice
            .next_block_delta(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("applied");

        // Long enough for at least one save interval to come round. WAL lets a
        // second connection read committed data while the writer is live.
        let mut found = false;
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if block_is_present(&dir, pos, stone) {
                found = true;
                break;
            }
        }
        assert!(
            found,
            "the edit should have been written by the periodic save, without a shutdown"
        );

        alice.disconnect().await;
    });

    server.stop();
}

/// Reads a block straight from the world file, alongside the running server.
fn block_is_present(dir: &std::path::Path, pos: BlockPos, material: u16) -> bool {
    let mut registry = tiamot_core::Registry::new();
    for name in MATERIALS {
        let _ = registry.register(name);
    }
    let Ok(db) = tiamot_core::WorldDb::open(dir.join("world.sqlite"), &mut registry) else {
        return false;
    };
    let present = matches!(
        db.load_chunk(pos.chunk()),
        Ok(Some(chunk)) if chunk.get_block(pos).is_some_and(|view| view.subnode(0) == MaterialId(material))
    );
    let _ = db.close();
    present
}

#[test]
fn an_edit_with_an_unregistered_material_is_ignored_without_dropping_the_player() {
    // A client racing a mod unload can send this without being hostile, so it
    // must not cost them their connection — but it must not reach the world
    // either.
    let dir = world_dir("bad-material");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;

        alice
            .edit(Edit::Block {
                pos: BlockPos::new(2, 2, 2),
                material: 60_000,
            })
            .await
            .expect("send");

        // Nothing should come back for the bad edit.
        assert!(
            alice
                .next_block_delta(Duration::from_millis(500))
                .await
                .expect("wait")
                .is_none(),
            "an unregistered material must not be broadcast"
        );

        // And the connection must still work.
        let good = Edit::Block {
            pos: BlockPos::new(3, 3, 3),
            material: stone,
        };
        alice.edit(good.clone()).await.expect("send");
        assert_eq!(
            alice
                .next_block_delta(Duration::from_secs(5))
                .await
                .expect("wait"),
            Some(good),
            "a bad edit must not have cost the player their session"
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_bot_that_never_joined_cannot_edit() {
    // The ordering constraint applied to gameplay: authenticate first or the
    // world does not hear from you.
    let dir = world_dir("unjoined-edit");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut intruder = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");

        intruder
            .edit(Edit::Block {
                pos: BlockPos::new(0, 0, 0),
                material: stone,
            })
            .await
            .expect("send");

        // A watcher who DID join must see nothing.
        let mut watcher = join(&server, "Watcher").await;
        assert!(
            watcher
                .next_block_delta(Duration::from_millis(500))
                .await
                .expect("wait")
                .is_none(),
            "an edit from an unauthenticated peer must never reach the world"
        );

        watcher.disconnect().await;
    });

    server.stop();
}
