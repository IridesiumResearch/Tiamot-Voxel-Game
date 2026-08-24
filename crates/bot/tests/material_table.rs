// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The material table, end to end: a mod registers a block with a texture, and
//! a client learns which number in a chunk blob means it and which bytes to
//! draw it with.
//!
//! This is the seam Task 08's renderer sits on. A chunk blob carries **world**
//! material ids and nothing else (charter rule 8), so without this table a
//! client can tell two materials apart and name neither. The alternative — a
//! client deriving the table by running the server's mods itself — is a second
//! implementation of something the server already decided, and a reason to
//! execute mod code the client otherwise never needs to run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("tiamot-material-table")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The repository's reference mods, which is what a default server runs.
fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn write_mod(root: &Path, id: &str, init: &str, assets: &[(&str, &[u8])]) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n"
        ),
    )
    .expect("manifest");
    std::fs::write(dir.join("init.lua"), init).expect("script");
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
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(11),
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

/// Connects and authenticates, stopping short of the world.
///
/// The table arrives with the manifest, in the authenticated phase, so a
/// renderer has it before the first chunk lands.
async fn authenticate(server: &ServerHandle) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    let nonce = bot.hello("Painter").await.expect("challenge");
    bot.authenticate(&nonce).await.expect("auth");
    bot.recv_until(|m| matches!(m, ServerMessage::MaterialTable { .. }))
        .await
        .expect("material table");
    bot
}

#[test]
fn the_reference_block_arrives_with_the_texture_its_mod_ships() {
    // The whole path in one test: `register_block{ textures = { all = ... } }`
    // in Lua, through the content index, onto the wire, and back out as bytes
    // that decode to the image the mod ships.
    let server = start("reference", reference_mods());

    block_on(async {
        let mut bot = authenticate(&server).await;
        let table = bot.material_table().expect("the table arrives on join");

        let white = table
            .iter()
            .find(|entry| entry.name == "core:white")
            .expect("the reference mod registers core:white");
        let hash = white
            .texture
            .expect("and declares a texture, which the server must resolve to a hash");

        bot.request_content(vec![hash]).await.expect("request");
        let items = bot
            .collect_content(1, Duration::from_secs(5))
            .await
            .expect("transfer");
        assert_eq!(items.len(), 1, "the texture must actually transfer");

        let on_disk = std::fs::read(reference_mods().join("core_blocks/textures/white.png"))
            .expect("the shipped PNG");
        assert_eq!(
            items[0].1, on_disk,
            "the bytes a client receives must be the file the mod ships"
        );

        bot.disconnect().await;
    });

    assert!(server.stop());
}

#[test]
fn the_reserved_materials_are_present_and_untextured() {
    // `engine:air` and `engine:unknown` are reserved (charter rule 8) and must
    // be in the table: a client that did not know 0 was air would draw the sky
    // as a missing texture. Neither has an image, and "no texture" is an absent
    // hash rather than a sentinel that could collide with a real one.
    let server = start("reserved", reference_mods());

    block_on(async {
        let bot = authenticate(&server).await;
        let table = bot.material_table().expect("table");

        for name in ["engine:air", "engine:unknown"] {
            let entry = table
                .iter()
                .find(|entry| entry.name == name)
                .unwrap_or_else(|| panic!("{name} must be in the table"));
            assert!(
                entry.texture.is_none(),
                "{name} should carry no texture, got {:?}",
                entry.texture
            );
        }

        assert_eq!(
            table
                .iter()
                .find(|entry| entry.name == "engine:air")
                .map(|entry| entry.id),
            Some(0),
            "air is id 0 in both id spaces, always"
        );

        bot.disconnect().await;
    });

    assert!(server.stop());
}

#[test]
fn the_table_is_sorted_by_id_and_has_no_duplicates() {
    // The client indexes an atlas by these ids. A duplicate would silently give
    // one material two tiles and leave the second unreachable; an unsorted
    // table would work today and break whatever assumes the order tomorrow.
    let server = start("sorted", reference_mods());

    block_on(async {
        let bot = authenticate(&server).await;
        let table = bot.material_table().expect("table");
        assert!(table.len() >= 3, "air, unknown, and at least one block");

        let ids: Vec<u16> = table.iter().map(|entry| entry.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids, sorted, "the table must be sorted and unique: {ids:?}");

        bot.disconnect().await;
    });

    assert!(server.stop());
}

#[test]
fn a_declared_but_missing_texture_leaves_the_material_untextured_rather_than_stopping_the_server() {
    // One mislaid PNG must not take a server down. The client draws its
    // missing-texture placeholder, the operator gets a log line naming the mod
    // and the path, and everyone else keeps playing.
    let mods = scratch("missing-mods");
    write_mod(
        &mods,
        "forgetful",
        "game.register_block{ id = 'ghost', textures = { all = 'textures/absent.png' } }",
        &[],
    );
    let server = start("missing", mods);

    block_on(async {
        let bot = authenticate(&server).await;
        let table = bot.material_table().expect("table");

        let ghost = table
            .iter()
            .find(|entry| entry.name == "forgetful:ghost")
            .expect("the block still registers");
        assert!(
            ghost.texture.is_none(),
            "a texture the mod did not ship must not be advertised: {:?}",
            ghost.texture
        );

        bot.disconnect().await;
    });

    assert!(server.stop());
}

#[test]
fn the_ids_in_the_table_are_the_ids_in_a_chunk_blob() {
    // The property the table exists for, and the one that would be silently
    // wrong if the server built the table from RUNTIME ids: a chunk decoded
    // with a passthrough map — which is all a client has — must yield the
    // numbers this table names.
    let server = start("blob-ids", reference_mods());

    block_on(async {
        let mut bot = authenticate(&server).await;
        let table = bot.material_table().expect("table");
        bot.send(&tiamot_core::proto::ClientMessage::JoinWorld)
            .await
            .expect("join");
        bot.recv_until(|m| matches!(m, ServerMessage::JoinWorld { .. }))
            .await
            .expect("in world");

        let chunks = bot
            .collect_chunks(1, Duration::from_secs(10))
            .await
            .expect("chunks");
        let pos = *chunks.first().expect("at least one chunk streams");

        // A passthrough map: world ids ARE the ids, because a client has no
        // `id_map` table to reconcile against.
        let chunk = bot
            .decode_chunk(
                pos,
                &tiamot_core::persist::idmap::MaterialMap::passthrough(),
            )
            .expect("a client can decode what the server sent it");

        let named: std::collections::BTreeSet<u16> = table.iter().map(|entry| entry.id).collect();
        for (local, view) in chunk.blocks() {
            for cell in 0..27 {
                let material = view.subnode(cell).get();
                assert!(
                    named.contains(&material),
                    "block {local:?} cell {cell} is material {material}, which the table does \
                     not name — the table and the blob disagree about which id space they are in"
                );
            }
        }

        bot.disconnect().await;
    });

    assert!(server.stop());
}
