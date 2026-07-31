// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Content push over a real server: manifest, request, transfer, verify.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::content::hash_bytes;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ContentHash;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-content-push").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Writes a mod with a manifest, a script, and whatever assets are given.
fn write_mod(root: &Path, id: &str, assets: &[(&str, &[u8])]) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n"
        ),
    )
    .expect("manifest");
    std::fs::write(dir.join("init.lua"), "-- server-only, never pushed\n").expect("script");
    for (relative, bytes) in assets {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, bytes).expect("asset");
    }
}

fn start(name: &str, mods: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
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

/// Connects and authenticates, stopping at `Authenticated` — content transfer
/// happens before entering the world, which is the whole point of the phase.
async fn authenticate(server: &ServerHandle, name: &str) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    let nonce = bot.hello(name).await.expect("challenge");
    bot.authenticate(&nonce).await.expect("auth");
    bot.recv_until(|m| matches!(m, tiamot_core::proto::ServerMessage::ModManifest { .. }))
        .await
        .expect("manifest");
    bot
}

#[test]
fn a_client_receives_a_requested_asset_intact() {
    let mods = scratch("basic-mods");
    let texture = b"a texture, pretending to be a PNG".to_vec();
    write_mod(&mods, "art", &[("stone.png", &texture)]);
    let server = start("basic", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;
        let hash = hash_bytes(&texture);

        bot.request_content(vec![hash]).await.expect("request");
        let received = bot
            .collect_content(1, Duration::from_secs(20))
            .await
            .expect("collect");

        assert_eq!(received.len(), 1);
        assert_eq!(received[0].0, hash);
        assert_eq!(received[0].1, texture, "the bytes must arrive intact");

        bot.disconnect().await;
    });

    assert!(server.stop(), "clean shutdown");
}

#[test]
fn a_large_asset_arrives_in_order_across_many_slices() {
    // Slicing is where an offset bug corrupts a file silently. The bot asserts
    // slices arrive contiguously and verifies the hash at the end, so a
    // mis-stepped offset fails here rather than on a player's screen.
    let mods = scratch("large-mods");
    let big: Vec<u8> = (0..900_000)
        .map(|i| u8::try_from(i % 251).unwrap_or(0))
        .collect();
    write_mod(&mods, "art", &[("big.png", &big)]);
    let server = start("large", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;
        let hash = hash_bytes(&big);

        bot.request_content(vec![hash]).await.expect("request");
        let received = bot
            .collect_content(1, Duration::from_secs(60))
            .await
            .expect("collect");

        assert_eq!(received.len(), 1);
        assert_eq!(received[0].1.len(), big.len());
        assert_eq!(received[0].1, big);

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn several_assets_transfer_in_one_request() {
    let mods = scratch("several-mods");
    let assets: Vec<(&str, Vec<u8>)> = vec![
        ("a.png", b"first asset".to_vec()),
        ("b.ogg", b"second asset".to_vec()),
        ("c.json", b"{\"third\": true}".to_vec()),
    ];
    let refs: Vec<(&str, &[u8])> = assets
        .iter()
        .map(|(name, bytes)| (*name, bytes.as_slice()))
        .collect();
    write_mod(&mods, "art", &refs);
    let server = start("several", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;
        let hashes: Vec<ContentHash> = assets.iter().map(|(_, bytes)| hash_bytes(bytes)).collect();

        bot.request_content(hashes.clone()).await.expect("request");
        let received = bot
            .collect_content(hashes.len(), Duration::from_secs(30))
            .await
            .expect("collect");

        assert_eq!(received.len(), hashes.len());
        for (hash, bytes) in &received {
            let expected = assets
                .iter()
                .find(|(_, candidate)| hash_bytes(candidate) == *hash)
                .expect("an asset we asked for");
            assert_eq!(bytes, &expected.1);
        }

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn server_lua_is_never_pushed_even_when_asked_for_by_hash() {
    // The security boundary. Server mod code can hold admin logic, allowlists,
    // or tokens; a client asking for it by hash must get nothing.
    let mods = scratch("no-lua-mods");
    write_mod(&mods, "art", &[("fine.png", b"public")]);
    // Overwrite the script with something identifiable.
    let secret = b"-- ADMIN TOKEN: hunter2\n";
    std::fs::write(mods.join("art").join("init.lua"), secret).expect("script");

    let server = start("no-lua", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;

        bot.request_content(vec![hash_bytes(secret)])
            .await
            .expect("request");
        let received = bot
            .collect_content(1, Duration::from_millis(800))
            .await
            .expect("collect");

        assert!(
            received.is_empty(),
            "server Lua must never be pushed, even to a client that knows its hash"
        );

        // And the connection still works for legitimate content.
        bot.request_content(vec![hash_bytes(b"public")])
            .await
            .expect("request");
        assert_eq!(
            bot.collect_content(1, Duration::from_secs(20))
                .await
                .expect("collect")
                .len(),
            1,
            "a refused request must not have cost the session"
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn an_unknown_hash_is_answered_with_silence_not_an_error() {
    // Answering "no such content" differently from "here it is" would let a
    // client probe what a private mod pack contains.
    let mods = scratch("unknown-mods");
    write_mod(&mods, "art", &[("known.png", b"present")]);
    let server = start("unknown", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;

        bot.request_content(vec![[0x5A; 32]])
            .await
            .expect("request");
        let received = bot
            .collect_content(1, Duration::from_millis(800))
            .await
            .expect("collect");
        assert!(received.is_empty(), "an unknown hash yields nothing");

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn the_manifest_carries_a_content_fingerprint_per_mod() {
    // A client compares fingerprints to decide whether it already has
    // everything, before asking for anything.
    let mods = scratch("manifest-mods");
    write_mod(&mods, "alpha", &[("a.png", b"alpha art")]);
    write_mod(&mods, "beta", &[("b.png", b"beta art")]);
    let server = start("manifest", mods);

    block_on(async {
        let bot = authenticate(&server, "Alice").await;
        let manifest = bot.manifest().expect("a manifest should have arrived");

        assert_eq!(manifest.len(), 2, "both mods should be listed");
        let ids: Vec<&str> = manifest.iter().map(|entry| entry.id.as_str()).collect();
        assert!(ids.contains(&"alpha") && ids.contains(&"beta"), "{ids:?}");

        for entry in &manifest {
            assert_ne!(
                entry.content_hash, [0u8; 32],
                "mod `{}` should have a real content fingerprint",
                entry.id
            );
            assert_eq!(entry.version, "0.1.0");
        }
        assert_ne!(
            manifest[0].content_hash, manifest[1].content_hash,
            "two mods with different content must fingerprint differently"
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn requesting_the_same_asset_twice_does_not_send_it_twice() {
    // A client looping its request list would otherwise be an amplifier: cheap
    // to send, expensive to serve.
    let mods = scratch("repeat-mods");
    let asset = b"only once".to_vec();
    write_mod(&mods, "art", &[("a.png", &asset)]);
    let server = start("repeat", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;
        let hash = hash_bytes(&asset);

        for _ in 0..5 {
            bot.request_content(vec![hash]).await.expect("request");
        }

        let first = bot
            .collect_content(1, Duration::from_secs(20))
            .await
            .expect("collect");
        assert_eq!(first.len(), 1);

        // Nothing more should arrive.
        let extra = bot
            .collect_content(1, Duration::from_millis(800))
            .await
            .expect("collect");
        assert!(
            extra.is_empty(),
            "a repeated request must not re-send the asset"
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn content_is_available_before_entering_the_world() {
    // The join flow's whole shape: manifest, then content, then JoinWorld. A
    // client that had to enter the world first would be rendering with assets
    // it does not have.
    let mods = scratch("before-world-mods");
    let asset = b"needed to render".to_vec();
    write_mod(&mods, "art", &[("a.png", &asset)]);
    let server = start("before-world", mods);

    block_on(async {
        let mut bot = authenticate(&server, "Alice").await;
        assert_eq!(
            bot.received()
                .iter()
                .filter(|m| matches!(m, tiamot_core::proto::ServerMessage::JoinWorld { .. }))
                .count(),
            0,
            "this bot has not joined the world yet"
        );

        bot.request_content(vec![hash_bytes(&asset)])
            .await
            .expect("request");
        assert_eq!(
            bot.collect_content(1, Duration::from_secs(20))
                .await
                .expect("collect")
                .len(),
            1,
            "content must transfer before the world does"
        );

        bot.disconnect().await;
    });

    server.stop();
}

#[test]
fn an_unauthenticated_peer_gets_no_content() {
    // The ordering constraint applied to assets. A private mod pack must not be
    // downloadable by anyone who can open a socket.
    let mods = scratch("unauth-mods");
    let asset = b"private art".to_vec();
    write_mod(&mods, "art", &[("a.png", &asset)]);
    let server = start("unauth", mods);

    block_on(async {
        let mut intruder = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");

        // Straight to a content request, with no handshake at all.
        intruder
            .request_content(vec![hash_bytes(&asset)])
            .await
            .expect("send");

        let received = intruder
            .collect_content(1, Duration::from_secs(2))
            .await
            .unwrap_or_default();
        assert!(
            received.is_empty(),
            "an unauthenticated peer must receive no content"
        );
    });

    server.stop();
}
