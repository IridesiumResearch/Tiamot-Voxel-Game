// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The identity suite, over a real server.
//!
//! Charter rule 13: identity is cryptographic **and recoverable**. A key you
//! can only lose once is a design defect, so this covers the ways back as
//! carefully as the ways in — recovery phrase, key sets, pre-committed
//! rotation, and the admin escape hatch.
//!
//! Every test drives a real loopback server. The unit tests in
//! `tiamot_core::identity` prove the crypto; these prove the *protocol* carries
//! it, which is a different claim and the one a player actually depends on.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::keyset::commit_to;
use tiamot_core::identity::{Allowlist, Identity, RecoveryPhrase};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::DisconnectReason;
use tiamot_server::{ServerHandle, Settings};

fn world_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tiamot-identity-suite")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn settings(dir: &std::path::Path) -> Settings {
    Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir.to_path_buf(),
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: None,
        enabled_mods: None,
        seed: Some(11),
        rcon: None,
        materials: Vec::new(),
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

async fn join_as(server: &ServerHandle, identity: Identity, name: &str) -> Bot {
    let mut bot = Bot::connect(server.local_addr(), identity, server.cert_fingerprint())
        .await
        .expect("connect");
    bot.join(name).await.expect("join");
    bot
}

/// Another handle on the same identity, through the seed.
fn same_identity(identity: &Identity) -> Identity {
    Identity::from_seed(&identity.seed())
}

/// Waits until the server has flushed identity changes to the database.
async fn wait_for_flush(server: &ServerHandle) {
    for _ in 0..200 {
        if !server.shared().identities.lock().await.is_dirty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn a_recovery_phrase_restores_an_identity_on_a_new_machine() {
    // THE recovery guarantee, end to end: write down 24 words, lose the key
    // file, type the words somewhere else, and you are the same player with the
    // same name. Without this the whole identity model is one disk failure away
    // from losing an account permanently.
    let dir = world_dir("phrase-restore");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let original = Identity::generate().expect("identity");
    let uuid = original.uuid_as_root();
    let phrase = original.recovery_phrase().expect("phrase").to_words();

    block_on(async {
        let bot = join_as(&server, original, "Alice").await;
        bot.disconnect().await;
        wait_for_flush(&server).await;

        // The key file is gone. All that survives is 24 words on paper.
        let restored = Identity::from_seed(
            &RecoveryPhrase::parse(&phrase)
                .expect("parse the written-down phrase")
                .seed()
                .expect("seed"),
        );
        assert_eq!(
            restored.uuid_as_root(),
            uuid,
            "the phrase must reconstruct the same identity"
        );

        let mut rejoined = Bot::connect(server.local_addr(), restored, server.cert_fingerprint())
            .await
            .expect("connect");
        rejoined
            .join("Alice")
            .await
            .expect("a restored identity must reclaim its own name");
        rejoined.disconnect().await;

        let identities = server.shared().identities.lock().await;
        assert_eq!(
            identities.name_holder("Alice"),
            Some(uuid),
            "and still hold it afterwards"
        );
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn a_phrase_restore_survives_a_server_restart_too() {
    // The realistic disaster: the player's machine dies, and by the time they
    // rebuild it the server has been restarted several times.
    let dir = world_dir("phrase-restart");
    let original = Identity::generate().expect("identity");
    let uuid = original.uuid_as_root();
    let phrase = original.recovery_phrase().expect("phrase").to_words();

    let server = ServerHandle::start(&settings(&dir)).expect("start");
    block_on(async {
        let bot = join_as(&server, original, "Alice").await;
        bot.disconnect().await;
        wait_for_flush(&server).await;
    });
    assert!(server.stop(), "clean shutdown");

    let server = ServerHandle::start(&settings(&dir)).expect("restart");
    block_on(async {
        let restored = Identity::from_seed(
            &RecoveryPhrase::parse(&phrase)
                .expect("parse")
                .seed()
                .expect("seed"),
        );
        let mut bot = Bot::connect(server.local_addr(), restored, server.cert_fingerprint())
            .await
            .expect("connect");
        bot.join("Alice").await.expect("restore after a restart");

        let identities = server.shared().identities.lock().await;
        assert_eq!(identities.name_holder("Alice"), Some(uuid));
    });
    server.stop();
}

#[test]
fn a_second_device_added_over_the_wire_joins_as_the_same_identity() {
    // The passkey model: many credentials, one account. Losing one device must
    // not lose the identity.
    let dir = world_dir("add-key");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let alice = Identity::generate().expect("identity");
    let uuid = alice.uuid_as_root();
    let laptop = Identity::generate().expect("identity");

    block_on(async {
        let mut phone = join_as(&server, same_identity(&alice), "Alice").await;
        phone
            .add_key(&laptop.public_key(), None)
            .await
            .expect("send AddKey");

        // Success is silent, so wait for the registry to show the new key
        // rather than for a message that will not come.
        let mut authorised = false;
        for _ in 0..200 {
            if server
                .shared()
                .identities
                .lock()
                .await
                .identity_of_key(&laptop.public_key())
                == Some(uuid)
            {
                authorised = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(authorised, "the laptop key should have been authorised");
        phone.disconnect().await;

        // The laptop now joins as Alice, using its own key.
        let mut laptop_bot = Bot::connect(server.local_addr(), laptop, server.cert_fingerprint())
            .await
            .expect("connect");
        laptop_bot
            .join("Alice")
            .await
            .expect("a second device must join as the same identity");
        laptop_bot.disconnect().await;

        let identities = server.shared().identities.lock().await;
        assert_eq!(identities.name_holder("Alice"), Some(uuid));
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn a_peer_cannot_add_a_key_to_someone_elses_identity() {
    // Note what this proves and what it does not. The session scopes AddKey to
    // the CALLER's own identity — the target UUID in the signed payload is not
    // consulted — so Mallory sending a well-formed request aimed at Alice
    // modifies his own key set at most.
    //
    // That is the stronger design, but it means this test would still pass with
    // `KeySet::check_signer` removed entirely. The authorisation check itself is
    // covered by `keyset::tests::an_add_key_signed_by_an_unauthorised_key_is_rejected`,
    // and over the wire by the test below. Discovered by planting exactly that
    // bug and watching this test not care.
    let dir = world_dir("add-key-unauthorised");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let alice = Identity::generate().expect("identity");
    let alice_uuid = alice.uuid_as_root();
    let mallory = Identity::generate().expect("identity");
    let mallorys_new_key = Identity::generate().expect("identity");

    block_on(async {
        // Alice joins so her identity exists.
        let alice_bot = join_as(&server, alice, "Alice").await;
        alice_bot.disconnect().await;

        // Mallory joins as himself and tries to add his key to Alice's set.
        let mut mallory_bot = join_as(&server, same_identity(&mallory), "Mallory").await;
        mallory_bot
            .add_key_signed_by(&mallory, &alice_uuid, &mallorys_new_key.public_key())
            .await
            .expect("send");

        tokio::time::sleep(Duration::from_millis(300)).await;

        let identities = server.shared().identities.lock().await;
        assert_eq!(
            identities.identity_of_key(&mallorys_new_key.public_key()),
            None,
            "an addition signed by an unauthorised key must be refused"
        );
        assert!(
            identities
                .key_set(&alice_uuid)
                .is_some_and(|set| set.all_keys().len() == 1),
            "Alice's key set must be untouched"
        );
    });

    server.stop();
}

#[test]
fn an_add_key_signed_by_a_key_outside_the_set_is_refused_over_the_wire() {
    // The authorisation check itself, reached through the protocol: the caller
    // is authenticated, and names a signer that is not in their own key set.
    // Without `check_signer` this would succeed, because the signature IS valid
    // for the key that made it — it just is not a key that may speak for this
    // identity.
    let dir = world_dir("add-key-outside-set");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let alice = Identity::generate().expect("identity");
    let uuid = alice.uuid_as_root();
    let outsider = Identity::generate().expect("identity");
    let target = Identity::generate().expect("identity");

    block_on(async {
        let mut bot = join_as(&server, same_identity(&alice), "Alice").await;

        // Signed correctly by `outsider`, over Alice's own UUID. Everything
        // verifies except the one thing that matters.
        bot.add_key_signed_by(&outsider, &uuid, &target.public_key())
            .await
            .expect("send");

        let reason = bot.refusal(Duration::from_secs(2)).await.expect("read");
        assert!(
            matches!(reason, Some(DisconnectReason::AuthFailed { .. })),
            "an addition signed by a key outside the set must be refused, got {reason:?}"
        );

        let identities = server.shared().identities.lock().await;
        assert_eq!(
            identities.identity_of_key(&target.public_key()),
            None,
            "the key must not have been authorised"
        );
        assert!(
            identities
                .key_set(&uuid)
                .is_some_and(|set| set.all_keys().len() == 1),
            "Alice's key set must be untouched"
        );
    });

    server.stop();
}

#[test]
fn a_rotation_matching_the_commitment_succeeds_and_retires_the_old_key() {
    // Pre-committed rotation, over the wire.
    let dir = world_dir("rotate-ok");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let successor = Identity::generate().expect("identity");
    let commitment = commit_to(&successor.public_key());

    // The root key must have registered the commitment before it can rotate to
    // it, so the identity is seeded directly rather than by a first join.
    let alice = Identity::generate().expect("identity");
    let uuid = alice.uuid_as_root();
    block_on(async {
        let mut identities = server.shared().identities.lock().await;
        identities.insert(tiamot_core::identity::KeySet::new(
            alice.public_key(),
            Some(commitment),
            0,
        ));
    });

    block_on(async {
        let mut bot = join_as(&server, same_identity(&alice), "Alice").await;
        bot.rotate_key(&successor.public_key(), None)
            .await
            .expect("send RotateKey");

        let mut rotated = false;
        for _ in 0..200 {
            if server
                .shared()
                .identities
                .lock()
                .await
                .identity_of_key(&successor.public_key())
                == Some(uuid)
            {
                rotated = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(rotated, "the pre-committed successor should be authorised");
        bot.disconnect().await;

        let identities = server.shared().identities.lock().await;
        assert_eq!(
            identities.identity_of_key(&alice.public_key()),
            None,
            "the rotated-away key must stop working"
        );
    });

    // And the successor can actually join.
    block_on(async {
        let mut heir = Bot::connect(server.local_addr(), successor, server.cert_fingerprint())
            .await
            .expect("connect");
        heir.join("Alice")
            .await
            .expect("the successor must join as the same identity");
        heir.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn a_rotation_to_the_wrong_key_is_refused_even_when_correctly_signed() {
    // The property pre-rotation exists for. A thief who steals the current key
    // holds a validly-signing credential — and still cannot rotate the identity
    // away, because they do not have the successor it was committed to.
    let dir = world_dir("rotate-wrong");
    let server = ServerHandle::start(&settings(&dir)).expect("start");

    let alice = Identity::generate().expect("identity");
    let uuid = alice.uuid_as_root();
    let designated = Identity::generate().expect("identity");
    let thiefs_key = Identity::generate().expect("identity");

    block_on(async {
        let mut identities = server.shared().identities.lock().await;
        identities.insert(tiamot_core::identity::KeySet::new(
            alice.public_key(),
            Some(commit_to(&designated.public_key())),
            0,
        ));
    });

    block_on(async {
        // The signature below is valid — made by the real current key. Only the
        // commitment does not match.
        let mut thief = join_as(&server, same_identity(&alice), "Alice").await;
        thief
            .rotate_key(&thiefs_key.public_key(), None)
            .await
            .expect("send");

        let reason = thief.refusal(Duration::from_secs(2)).await.expect("read");
        assert!(
            matches!(reason, Some(DisconnectReason::AuthFailed { .. })),
            "a rotation to an uncommitted key must be refused, got {reason:?}"
        );

        let identities = server.shared().identities.lock().await;
        assert_eq!(
            identities.identity_of_key(&thiefs_key.public_key()),
            None,
            "the thief's key must NOT be authorised"
        );
        assert_eq!(
            identities.identity_of_key(&alice.public_key()),
            Some(uuid),
            "and the real key must still work"
        );
    });

    server.stop();
}

#[test]
fn key_set_changes_survive_a_server_restart() {
    // A device added today must still work tomorrow.
    let dir = world_dir("keyset-restart");
    let alice = Identity::generate().expect("identity");
    let uuid = alice.uuid_as_root();
    let laptop = Identity::generate().expect("identity");

    let server = ServerHandle::start(&settings(&dir)).expect("start");
    block_on(async {
        let mut phone = join_as(&server, same_identity(&alice), "Alice").await;
        phone
            .add_key(&laptop.public_key(), None)
            .await
            .expect("send");
        for _ in 0..200 {
            if server
                .shared()
                .identities
                .lock()
                .await
                .identity_of_key(&laptop.public_key())
                == Some(uuid)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        phone.disconnect().await;
        wait_for_flush(&server).await;
    });
    assert!(server.stop(), "clean shutdown");

    let server = ServerHandle::start(&settings(&dir)).expect("restart");
    block_on(async {
        let mut laptop_bot = Bot::connect(server.local_addr(), laptop, server.cert_fingerprint())
            .await
            .expect("connect");
        laptop_bot
            .join("Alice")
            .await
            .expect("an added key must survive a restart");
        laptop_bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn the_embedded_server_runs_the_same_flow_in_process() {
    // Charter rule 2: singleplayer is this server on loopback, through the same
    // entry point. A second startup path would be a second set of bugs that
    // only appear in one mode.
    let dir = world_dir("embedded");
    let server = ServerHandle::start_embedded(&dir, 4).expect("start embedded");

    assert!(
        server.local_addr().ip().is_loopback(),
        "an embedded server must not be reachable from outside the machine"
    );

    block_on(async {
        let mut bot = join_as(&server, Identity::generate().expect("identity"), "Player").await;
        let chunks = bot
            .collect_chunks(1, Duration::from_secs(20))
            .await
            .expect("collect");
        assert!(!chunks.is_empty(), "singleplayer must stream a world too");
        bot.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}
