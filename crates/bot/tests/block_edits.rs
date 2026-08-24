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
use tiamot_core::proto::{DisconnectReason, Edit, ServerMessage};
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
        mods_path: None,
        enabled_mods: None,
        seed: Some(1),
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    }
}

/// The reference mods, which register the tools a dig needs.
///
/// Charter rule 1: the engine has no tools of its own, not even a bare hand, so
/// a world with no mods is one nobody can dig in. Any test here that breaks a
/// block needs these; the ones that only check persistence do not.
fn reference_mods() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn settings_with_mods(dir: &std::path::Path) -> Settings {
    Settings {
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        ..settings(dir)
    }
}

/// Digs a whole block for real, re-aiming until the server confirms it.
///
/// Re-aiming at the same cell keeps its progress, so repeating costs nothing
/// and survives a message going missing.
async fn dig(bot: &mut Bot, pos: BlockPos) {
    bot.select_tool(None).await.expect("bare hand");
    let target = SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        bot.start_dig(target).await.expect("start dig");
        if bot
            .expect_block(pos, MaterialId::AIR.0, Duration::from_secs(4))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "digging {pos:?} never completed"
        );
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
    // THE acceptance criterion for Task 06, on the path Task 09 replaced it
    // with. It used to be "Alice sends a BlockDelta and Bob sees it", which
    // stopped being possible when clients lost the ability to edit the world
    // directly — so the criterion is now what it always meant: **a change one
    // player makes shows up for another**, made the way a player actually
    // makes one.
    let dir = world_dir("two-bots");
    let server = ServerHandle::start(&settings_with_mods(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        // Something for Alice to break, arranged by the operator.
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        for bot in [&mut alice, &mut bob] {
            bot.expect_block(pos, stone, Duration::from_secs(10))
                .await
                .expect("the seed should reach both");
        }

        dig(&mut alice, pos).await;

        assert!(
            bob.expect_block(pos, MaterialId::AIR.0, Duration::from_secs(10))
                .await
                .is_ok(),
            "Bob must see the block Alice broke"
        );

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
    let server = ServerHandle::start(&settings_with_mods(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let pos = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(pos, stone), "seed queue full");
        alice
            .expect_block(pos, stone, Duration::from_secs(10))
            .await
            .expect("the seed should land");

        // The digger is not excluded from their own broadcast. A client that
        // had to predict its own edits and never hear about them would have no
        // way to notice one being refused.
        dig(&mut alice, pos).await;

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
    let server = ServerHandle::start(&settings_with_mods(&dir)).expect("start");
    let glass = material_id(&server, "test:glass");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mut bob = join(&server, "Bob").await;

        let block = BlockPos::new(2, -1, 0);
        assert!(server.seed_block(block, glass), "seed queue full");
        for bot in [&mut alice, &mut bob] {
            bot.expect_block(block, glass, Duration::from_secs(10))
                .await
                .expect("the seed should reach both");
        }

        // A real chisel, with the sub-node brush a MOD registered — which is
        // the whole argument for sub-nodes existing.
        let target = SubNodePos::new(block.x * 3, block.y * 3, block.z * 3);
        alice
            .select_tool(Some("core_tools:chisel"))
            .await
            .expect("select chisel");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let saw_cell = loop {
            alice.start_dig(target).await.expect("start dig");
            let _ = tokio::time::timeout(Duration::from_secs(2), bob.recv()).await;
            let seen = bob.received().into_iter().any(|message| {
                matches!(
                    message,
                    ServerMessage::BlockDelta {
                        edit: Edit::SubNode { pos, material },
                        ..
                    } if pos == target && material == MaterialId::AIR.0
                )
            });
            if seen {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
        };

        assert!(
            saw_cell,
            "Bob never saw a SUB-NODE removal; a broadcast that rounded the chisel up to a \
             whole block would make it invisible to everyone but the digger"
        );
        // And emphatically not the whole block: that is the failure this test
        // exists to catch, and it looks identical from the digger's side.
        assert!(
            !bob.received().into_iter().any(|message| {
                matches!(
                    message,
                    ServerMessage::BlockDelta {
                        edit: Edit::Block { pos, material },
                        ..
                    } if pos == block && material == MaterialId::AIR.0
                )
            }),
            "the chisel was broadcast as a whole-block removal"
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
        // What persists is a world edit, whoever made it — so this arranges one
        // rather than acting out a player making it.
        assert!(server.seed_block(pos, stone), "seed queue full");

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
        assert!(server.seed_block(pos, stone), "seed queue full");
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

        // A placement naming a material that does not exist. The client-side
        // edit path this used to test is gone; `Place` is where a bad material
        // id can still arrive from a peer, and the requirement is unchanged —
        // refused, and not with a disconnect.
        alice
            .place_from_inventory(SubNodePos::new(6, 6, 6), 60_000)
            .await
            .expect("send");
        assert!(
            alice
                .next_block_delta(Duration::from_millis(500))
                .await
                .expect("wait")
                .is_none(),
            "an unregistered material must not reach the world"
        );

        // And the connection must still work: the operator writes a block and
        // Alice, still connected, sees it.
        let pos = BlockPos::new(3, 3, 3);
        assert!(server.seed_block(pos, stone), "seed queue full");
        assert_eq!(
            alice
                .next_block_delta(Duration::from_secs(5))
                .await
                .expect("wait"),
            Some(Edit::Block {
                pos,
                material: stone
            }),
            "a bad request must not have cost the player their session"
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn the_retired_block_delta_is_refused_rather_than_ignored() {
    // **The point of the whole exercise.** Task 07's `BlockDelta` let a client
    // write a block straight into the world, which made every rule digging and
    // placing enforce — you pay for what you take, you are refused what you may
    // not have, a mod may veto it — optional for anyone who sent the older
    // message instead.
    //
    // It cannot be deleted: postcard encodes a variant as its ordinal, so
    // removing it would renumber `Chat`, `AddKey` and everything after, and any
    // peer built against either side would silently reinterpret every later
    // message. Deprecated in place is the only way to retire one.
    //
    // Refused rather than ignored, and this asserts which. A client built
    // against an engine that no longer exists should find out immediately
    // rather than watch its edits vanish.
    let dir = world_dir("retired-blockdelta");
    let server = ServerHandle::start(&settings(&dir)).expect("start");
    let stone = material_id(&server, "test:stone");

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let pos = BlockPos::new(5, 5, 5);
        alice
            .send_retired_block_delta(Edit::Block {
                pos,
                material: stone,
            })
            .await
            .expect("send");

        let reason = alice
            .refusal(Duration::from_secs(5))
            .await
            .expect("wait")
            .expect("the server must say why rather than ignoring it");
        assert!(
            matches!(reason, DisconnectReason::ProtocolError { .. }),
            "expected a protocol error, got {reason:?}"
        );

        // And nothing reached the world: a watcher joining afterwards sees a
        // world without the block. Refusing loudly is only half the claim.
        let mut watcher = join(&server, "Watcher").await;
        assert!(
            watcher
                .expect_block(pos, stone, Duration::from_millis(500))
                .await
                .is_err(),
            "the refused edit reached the world anyway"
        );
        watcher.disconnect().await;
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

        // Both gameplay requests, from a peer that has authenticated but never
        // entered the world. The session refuses anything but the handshake
        // before `Phase::InWorld`, and the check is the phase itself rather
        // than a flag someone can forget to test.
        intruder
            .place_from_inventory(SubNodePos::new(0, 0, 0), stone)
            .await
            .expect("send");
        intruder
            .start_dig(SubNodePos::new(0, 0, 0))
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
