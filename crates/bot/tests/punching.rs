// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Hitting things, over a real server.
//!
//! # What the engine does about a punch
//!
//! Nothing. There is no damage model in core and there will not be one: what a
//! hit *means* is a game decision and charter rule 1 puts those in mods. The
//! engine's whole job here is to say who hit what, having first established
//! that they could reach it — because charter rule 2 makes the client a viewer,
//! and a viewer that could assert a hit could assert every hit.
//!
//! So these tests assert on what a mod did about a punch, and on the punches
//! that never reached one.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::BlockPos;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

/// Where the mod records that it saw a punch, and what it saw.
const WAS_PUNCHED: BlockPos = BlockPos::new(1, 10, 1);
const KNEW_THE_OWNER: BlockPos = BlockPos::new(3, 10, 1);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-punching").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that writes a marker when it is told about a punch.
fn write_referee(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("referee");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"referee\"\nname = \"Referee\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        "local rock = game.register_block{ id = \"rock\" }\n\
         local mark = game.register_block{ id = \"mark\" }\n\
         game.register_on_generate(function(buf, pos)\n\
         \x20   buf:fill_below_heightmap(game.flat_heightmap(0), rock)\n\
         end)\n\
         game.register_on_punch(function(event)\n\
         \x20   game.set_block({ x = 1, y = 10, z = 1 }, \"referee:mark\")\n\
         \x20   -- The attacker and the owner are different people, which is the\n\
         \x20   -- whole point of carrying both: one field says who swung and the\n\
         \x20   -- other says whose body took it.\n\
         \x20   if event.owner ~= nil and event.owner ~= event.attacker then\n\
         \x20       game.set_block({ x = 3, y = 10, z = 1 }, \"referee:mark\")\n\
         \x20   end\n\
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
        mods_path: Some(write_referee(name)),
        enabled_mods: None,
        seed: Some(9),
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

fn mark_id(bot: &Bot) -> u16 {
    bot.material_table()
        .expect("the server sends a material table on join")
        .into_iter()
        .find(|entry| entry.name == "referee:mark")
        .map(|entry| entry.id)
        .expect("the mod registers a mark")
}

#[test]
fn punching_another_player_reaches_the_mod_with_both_parties() {
    let server = start("hit");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let _bob = join(&server, "Bob").await;
        let mark = mark_id(&alice);

        // Both spawn at the same point, so Bob is well within reach.
        let bob_body = alice
            .expect_entity(|entity| entity.nametag.as_deref() == Some("Bob"), PATIENCE)
            .await
            .expect("Alice should be told about Bob");

        alice.punch(bob_body.id).await.expect("send the punch");

        alice
            .expect_block(WAS_PUNCHED, mark, PATIENCE)
            .await
            .expect("the mod should have been told about the punch");
        alice
            .expect_block(KNEW_THE_OWNER, mark, PATIENCE)
            .await
            .expect("the mod should have been told whose body it was");
    });
}

#[test]
fn a_punch_at_an_entity_that_does_not_exist_reaches_nobody() {
    // A client naming an id the server never issued, which is the shape every
    // invented punch takes. Silence rather than an error: an entity that
    // despawned between the click and the tick is ordinary traffic, and telling
    // the mods about a punch at nothing would be inventing an event.
    let server = start("ghost");
    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let mark = mark_id(&alice);

        alice.punch(u64::MAX).await.expect("send the punch");

        // Give the server several ticks to have done the wrong thing.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
        while tokio::time::Instant::now() < deadline {
            let _ = tokio::time::timeout(Duration::from_millis(100), alice.recv()).await;
        }
        assert!(
            !alice.saw_subnode(
                tiamot_core::SubNodePos::new(
                    WAS_PUNCHED.x * 3 + 1,
                    WAS_PUNCHED.y * 3 + 1,
                    WAS_PUNCHED.z * 3 + 1
                ),
                mark
            ),
            "a punch at an entity that does not exist was reported as a punch"
        );
    });
}
