// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests against a real loopback server.
//!
//! Every test here starts an actual [`ServerHandle`], binds a real UDP socket,
//! and drives it with a real QUIC client. Nothing is mocked. Charter rule 15:
//! the tested path is the shipped path, so a bug in framing, TLS, the
//! handshake, or the session state machine fails here rather than the first
//! time a human connects.
//!
//! # These run on OS-assigned ports
//!
//! Every server binds `127.0.0.1:0` and the test reads back the port it got.
//! Hard-coding one would make the suite fail when run twice at once, which is
//! exactly what `cargo test` does by default.

use std::path::PathBuf;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity, challenge_payload};
use tiamot_core::proto::{
    ClientMessage, DisconnectReason, PROTOCOL_VERSION, ServerMessage, WireSignature,
};
use tiamot_server::{ServerHandle, Settings};

/// Another handle on the same identity.
///
/// `Identity` is deliberately not `Clone` — it holds a secret key, and the
/// fewer ways there are to duplicate one the better. A test that needs two
/// handles goes through the seed explicitly, which is the same round trip a
/// recovery phrase makes.
fn same_identity(identity: &Identity) -> Identity {
    Identity::from_seed(&identity.seed())
}

/// A fresh world directory. Removed first so a previous run cannot leak state.
fn world_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-join-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn settings(dir: &std::path::Path, allowlist: Allowlist) -> Settings {
    Settings {
        bind_addr: "127.0.0.1:0".parse().expect("valid loopback address"),
        world_path: dir.to_path_buf(),
        max_players: 8,
        allowlist,
        materials: Vec::new(),
    }
}

fn start(name: &str) -> (ServerHandle, PathBuf) {
    let dir = world_dir(name);
    let handle = ServerHandle::start(&settings(&dir, Allowlist::open())).expect("start server");
    (handle, dir)
}

/// Runs an async body on a small runtime.
///
/// A current-thread runtime rather than the multi-threaded one: these tests are
/// a handful of connections, and a deterministic single-threaded executor makes
/// a hang a hang rather than an intermittent one.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

async fn connect(server: &ServerHandle, identity: Identity) -> Bot {
    Bot::connect(server.local_addr(), identity, server.cert_fingerprint())
        .await
        .expect("connect")
}

#[test]
fn a_bot_completes_the_whole_join_flow() {
    let (server, _dir) = start("full-join");

    block_on(async {
        let mut bot = connect(&server, Identity::generate().expect("identity")).await;
        bot.join("Alice").await.expect("join");

        let kinds: Vec<&str> = bot.received().iter().map(describe).collect();
        assert_eq!(
            kinds,
            vec!["HelloAck", "AuthChallenge", "ModManifest", "JoinWorld"],
            "the join flow must arrive in order, got {kinds:?}"
        );
        bot.disconnect().await;
    });

    assert!(server.stop(), "the server should shut down cleanly");
}

#[test]
fn a_first_join_registers_a_new_identity() {
    // Self-sovereign: an empty world must still be joinable. This is the case
    // that caught the original design, where an unknown key was refused and a
    // fresh server was one nobody could ever join.
    let (server, _dir) = start("first-join");
    let identity = Identity::generate().expect("identity");
    let uuid = identity.uuid_as_root();

    block_on(async {
        let mut bot = connect(&server, identity).await;
        bot.join("Newcomer")
            .await
            .expect("a stranger must be able to join an open server");
        bot.disconnect().await;

        let identities = server.shared().identities.lock().await;
        assert!(
            identities.contains(&uuid),
            "the identity should be registered"
        );
        assert_eq!(identities.name_holder("Newcomer"), Some(uuid));
    });

    server.stop();
}

#[test]
fn a_second_identity_cannot_take_a_bound_name() {
    let (server, _dir) = start("name-theft");

    block_on(async {
        let mut alice = connect(&server, Identity::generate().expect("identity")).await;
        alice.join("Alice").await.expect("Alice joins");

        let mut thief = connect(&server, Identity::generate().expect("identity")).await;
        let err = thief.join("Alice").await.expect_err("the name is taken");

        assert!(
            matches!(
                err,
                bot::BotError::Refused {
                    reason: DisconnectReason::NameTaken { .. }
                }
            ),
            "expected a NameTaken refusal, got {err}"
        );
        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_replayed_signature_is_rejected() {
    // The nonce is fresh per connection, so a signature captured from one
    // session must be useless in the next.
    let (server, _dir) = start("replay");
    let identity = Identity::generate().expect("identity");

    block_on(async {
        let mut first = connect(&server, same_identity(&identity)).await;
        let nonce = first.hello("Alice").await.expect("challenge");
        let captured = WireSignature(
            identity
                .sign(&challenge_payload(
                    &nonce,
                    &server.cert_fingerprint(),
                    PROTOCOL_VERSION,
                ))
                .to_bytes(),
        );

        // It is genuinely valid here — establish that, or the rejection below
        // could be hiding a malformed signature rather than a stale nonce.
        first
            .send(&ClientMessage::AuthResponse {
                signature: captured,
            })
            .await
            .expect("send");
        first
            .recv_until(|m| matches!(m, ServerMessage::ModManifest { .. }))
            .await
            .expect("the captured signature must be valid where it was made");

        // Now replay it on a fresh connection with a different nonce.
        let mut second = connect(&server, same_identity(&identity)).await;
        let _ = second.hello("Alice").await.expect("challenge");
        second
            .send(&ClientMessage::AuthResponse {
                signature: captured,
            })
            .await
            .expect("send");

        let err = second
            .recv_until(|m| matches!(m, ServerMessage::ModManifest { .. }))
            .await
            .expect_err("a replayed signature must be refused");
        assert!(
            matches!(
                err,
                bot::BotError::Refused {
                    reason: DisconnectReason::AuthFailed { .. }
                }
            ),
            "expected an auth failure, got {err}"
        );
    });

    server.stop();
}

#[test]
fn a_signature_bound_to_another_server_is_rejected() {
    // The MITM relay case: a signature captured on server A must not open a
    // session on server B. This is the whole reason the certificate
    // fingerprint is inside the signed payload.
    let (server, _dir) = start("wrong-server");

    block_on(async {
        let mut bot = connect(&server, Identity::generate().expect("identity")).await;
        let nonce = bot.hello("Alice").await.expect("challenge");

        bot.authenticate_with(&nonce, b"a-completely-different-server")
            .await
            .expect("send");

        let err = bot
            .recv_until(|m| matches!(m, ServerMessage::ModManifest { .. }))
            .await
            .expect_err("a signature for another server must be refused");
        assert!(
            matches!(
                err,
                bot::BotError::Refused {
                    reason: DisconnectReason::AuthFailed { .. }
                }
            ),
            "expected an auth failure, got {err}"
        );
    });

    server.stop();
}

#[test]
fn a_version_mismatch_is_refused_cleanly() {
    let (server, _dir) = start("version");

    block_on(async {
        let identity = Identity::generate().expect("identity");
        let mut bot = connect(&server, same_identity(&identity)).await;

        bot.send(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 99,
            public_key: *identity.public_key().as_bytes(),
            display_name: "Alice".to_owned(),
        })
        .await
        .expect("send");

        let message = bot.recv().await.expect("a reason, not a silent drop");
        assert!(
            matches!(
                message,
                ServerMessage::Disconnect {
                    reason: DisconnectReason::VersionMismatch { .. }
                }
            ),
            "expected a version mismatch, got {message:?}"
        );
    });

    server.stop();
}

#[test]
fn an_identity_survives_a_server_restart() {
    // The acceptance criterion: same key, same UUID, same name, after the
    // server has been fully stopped and started again.
    let dir = world_dir("restart");
    let identity = Identity::generate().expect("identity");
    let uuid = identity.uuid_as_root();

    let server = ServerHandle::start(&settings(&dir, Allowlist::open())).expect("start");
    let fingerprint = server.cert_fingerprint();
    block_on(async {
        let mut bot = Bot::connect(server.local_addr(), same_identity(&identity), fingerprint)
            .await
            .expect("connect");
        bot.join("Alice").await.expect("join");
        bot.disconnect().await;

        // The tick loop flushes the registry, so give it a tick or two to run.
        // Asserting `is_dirty()` here would be asserting the flush had NOT
        // happened yet, which is the opposite of what this test wants.
        for _ in 0..100 {
            if !server.shared().identities.lock().await.is_dirty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !server.shared().identities.lock().await.is_dirty(),
            "the join's bindings should have been flushed within a second"
        );
    });
    server.stop();

    // Second run, same directory.
    let server = ServerHandle::start(&settings(&dir, Allowlist::open())).expect("restart");
    assert_eq!(
        server.cert_fingerprint(),
        fingerprint,
        "the certificate must survive a restart, or every client has to re-pin"
    );

    block_on(async {
        let identities = server.shared().identities.lock().await;
        assert!(
            identities.contains(&uuid),
            "the identity must survive a restart"
        );
        assert_eq!(
            identities.name_holder("Alice"),
            Some(uuid),
            "and so must the name binding"
        );
    });

    server.stop();
}

#[test]
fn a_bot_pinning_the_wrong_fingerprint_cannot_connect() {
    // The TOFU guarantee, from the client side. If this passed, the bot's
    // verifier would be accepting anything and every other test in this file
    // would be proving less than it looks.
    let (server, _dir) = start("wrong-pin");

    block_on(async {
        let result = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            [0x00; 32],
        )
        .await;
        assert!(
            result.is_err(),
            "a client must refuse a server whose fingerprint it did not pin"
        );
    });

    server.stop();
}

#[test]
fn an_abrupt_disconnect_does_not_leak_the_player() {
    use std::sync::atomic::Ordering;

    let (server, _dir) = start("abrupt");

    block_on(async {
        let mut bot = connect(&server, Identity::generate().expect("identity")).await;
        bot.join("Alice").await.expect("join");

        // Wait for the server to register the join.
        for _ in 0..200 {
            if server.shared().players.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            server.shared().players.load(Ordering::Acquire),
            1,
            "the player should be counted while connected"
        );

        // No goodbye — the cable is pulled.
        bot.abandon();

        for _ in 0..500 {
            if server.shared().players.load(Ordering::Acquire) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            server.shared().players.load(Ordering::Acquire),
            0,
            "an abrupt disconnect must still release the player slot"
        );
    });

    server.stop();
}

#[test]
fn an_identity_off_a_restricted_allowlist_is_refused() {
    let dir = world_dir("allowlist");
    let permitted = Identity::generate().expect("identity");
    let server = ServerHandle::start(&settings(
        &dir,
        Allowlist::restricted([permitted.uuid_as_root()]),
    ))
    .expect("start");

    block_on(async {
        let mut allowed = Bot::connect(
            server.local_addr(),
            same_identity(&permitted),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        allowed
            .join("Permitted")
            .await
            .expect("an allowlisted identity may join");
        allowed.disconnect().await;

        let mut stranger = connect(&server, Identity::generate().expect("identity")).await;
        let err = stranger
            .join("Stranger")
            .await
            .expect_err("must be refused");
        assert!(
            matches!(
                err,
                bot::BotError::Refused {
                    reason: DisconnectReason::NotAllowlisted
                }
            ),
            "expected NotAllowlisted, got {err}"
        );
    });

    server.stop();
}

#[test]
fn no_world_state_flows_before_identity_is_proven() {
    // The ordering constraint, over a real socket. A peer that skips the
    // handshake and asks for the world must get nothing but a disconnect.
    let (server, _dir) = start("ordering");

    block_on(async {
        let mut bot = connect(&server, Identity::generate().expect("identity")).await;
        bot.send(&ClientMessage::JoinWorld).await.expect("send");

        let message = bot.recv().await.expect("a reason");
        assert!(
            matches!(message, ServerMessage::Disconnect { .. }),
            "expected a disconnect, got {message:?}"
        );
        assert!(
            !bot.received()
                .iter()
                .any(|m| matches!(m, ServerMessage::JoinWorld { .. })),
            "no world state may flow before identity is proven"
        );
    });

    server.stop();
}

fn describe(message: &ServerMessage) -> &'static str {
    match message {
        ServerMessage::HelloAck { .. } => "HelloAck",
        ServerMessage::AuthChallenge { .. } => "AuthChallenge",
        ServerMessage::ModManifest { .. } => "ModManifest",
        ServerMessage::JoinWorld { .. } => "JoinWorld",
        ServerMessage::Disconnect { .. } => "Disconnect",
        _ => "other",
    }
}
