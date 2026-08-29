// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Domains, over a real server: more than one simulation space in one world.
//!
//! Task 15a. A world contains several named spaces with independent coordinate
//! frames and chunk stores; a body is in exactly one of them, and the engine
//! provides the mechanism for moving between them while mods decide when that
//! happens (charter rule 1).
//!
//! # What these drive
//!
//! A real mod, over a real server, through the real protocol. The unit tests in
//! `core::domain` and `server::domains` check each piece against itself; these
//! check the thing those pieces are for — that a player who moves stops seeing
//! the space they left, which is the criterion no unit test can reach.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 1] = ["test:stone"];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-domains").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that declares a second domain and a ship template, plus whatever else
/// a test needs.
///
/// Ground and a tool are not incidental: **an empty world is not a neutral
/// fixture.** With nothing to stand on a player free-falls, and every assertion
/// about where they are becomes an assertion about how fast the machine is.
fn write_mod(name: &str, extra: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("places");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"places\"\nname = \"Places\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "local ground = game.register_block{{ id = \"ground\" }}\n\
             game.register_on_generate(function(buf, pos)\n\
             \x20   buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
             end)\n\
             game.register_tool{{\n\
             \x20   id = \"hand\",\n\
             \x20   brush = \"block\",\n\
             \x20   speed_multiplier = 1.0,\n\
             \x20   default = true,\n\
             }}\n\
             game.register_domain{{ id = \"attic\" }}\n\
             game.register_domain{{ id = \"ship\", instanced = true }}\n\
             game.register_domain{{ id = \"space\", kind = \"sparse\", scale = 1000.0 }}\n\
             {extra}\n"
        ),
    )
    .expect("script");
    root
}

fn start(name: &str, mods: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        enabled_mods: None,
        seed: Some(3),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
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

/// Whether this bot has been told it changed domain, and to what.
fn switched_to(bot: &Bot) -> Option<String> {
    bot.received()
        .into_iter()
        .rev()
        .find_map(|message| match message {
            tiamot_core::proto::ServerMessage::DomainChanged { domain } => Some(domain),
            _ => None,
        })
}

/// How many chunks this bot has been sent since it last cleared its history.
fn chunks_seen(bot: &Bot) -> usize {
    bot.received()
        .into_iter()
        .filter(|message| matches!(message, tiamot_core::proto::ServerMessage::ChunkData { .. }))
        .count()
}

/// Pumps the connection for a while, so the server's messages arrive.
async fn settle_for(bot: &mut Bot, ticks: u64) {
    for _ in 0..ticks {
        let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
    }
}

#[test]
fn a_player_moved_to_another_domain_is_told_and_starts_again() {
    // The end-to-end shape of a transfer: a mod asks, the engine performs it,
    // and the client is told once that everything it holds is now wrong.
    let server = start(
        "transfer",
        write_mod(
            "transfer",
            "game.register_on_chat(function(event)\n\
             \x20   if event.text == 'attic' then\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Traveller").await;
        settle_for(&mut bot, 40).await;

        let before = chunks_seen(&bot);
        assert!(
            before > 0,
            "the overworld never streamed, so this cannot tell a domain change \
             from a server that sends nothing"
        );
        assert_eq!(
            switched_to(&bot),
            None,
            "a domain change before one was asked for"
        );

        bot.chat("attic").await.expect("chat");
        // Driven to a condition with a deadline rather than a fixed wait: the
        // transfer happens on a tick, and how many ticks it takes to reach us
        // is a fact about the machine.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a mod moved the player to another domain and the client was never told"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        assert_eq!(switched_to(&bot).as_deref(), Some("places:attic"));

        // And the new domain streams: the client threw everything away, so it
        // has to be filled again or the player is standing in nothing.
        let after_switch = chunks_seen(&bot);
        settle_for(&mut bot, 60).await;
        assert!(
            chunks_seen(&bot) > after_switch,
            "nothing was streamed after the move, so the player is in an empty world"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_mod_can_refuse_a_move_and_the_player_stays_put() {
    // **Paired with the test above**, which is the same scenario with no veto.
    // "The player did not move" is satisfied by a server where transfers are
    // broken for reasons that have nothing to do with the hook, so the pair is
    // what makes either of them mean anything.
    let server = start(
        "veto",
        write_mod(
            "veto",
            "game.register_on_domain_exit(function() return false end)\n\
             game.register_on_chat(function(event)\n\
             \x20   if event.text == 'attic' then\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Stayer").await;
        settle_for(&mut bot, 40).await;

        bot.chat("attic").await.expect("chat");
        settle_for(&mut bot, 80).await;

        assert_eq!(
            switched_to(&bot),
            None,
            "`on_domain_exit` returned false and the player moved anyway"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_player_in_another_domain_is_told_nothing_of_the_one_they_left() {
    // **Criterion A3, the cross-domain interest leak.** Two players, one of
    // whom moves; the one who stayed digs a hole. The traveller must not be
    // told about an edit in a space they are not in — the positions are
    // identical between domains, so a leak lands as terrain changing under
    // somebody who is nowhere near it.
    let server = start(
        "leak",
        write_mod(
            "leak",
            "game.register_on_chat(function(event)\n\
             \x20   if event.text == 'attic' then\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut traveller = join(&server, "Traveller").await;
        let mut digger = join(&server, "Digger").await;
        settle_for(&mut traveller, 40).await;
        settle_for(&mut digger, 40).await;

        traveller.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&traveller).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the traveller never moved, so nothing here is being tested"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), traveller.recv()).await;
        }

        // Everything the traveller has heard so far is history; what matters is
        // what arrives from here.
        let before = traveller.received().len();

        let here = digger.settle().await.expect("settle").block();
        let target = tiamot_core::BlockPos::new(here.x + 1, here.y - 1, here.z);
        digger.dig_block(target).await.expect("dig");
        settle_for(&mut traveller, 60).await;

        let leaked: Vec<_> = traveller
            .received()
            .into_iter()
            .skip(before)
            .filter(|message| {
                matches!(
                    message,
                    tiamot_core::proto::ServerMessage::BlockDelta { .. }
                )
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "a player in another domain was told about {} edit(s) made in the \
             one they left: {leaked:?}",
            leaked.len()
        );

        traveller.disconnect().await;
        digger.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_ship_is_made_at_runtime_and_is_a_domain_like_any_other() {
    // The case the registration window could not serve: an id no mod could
    // have named while the registry was open. What this asserts is that the
    // instance is indistinguishable from a registered domain downstream — the
    // player transfers into it and is told, exactly as for `places:attic`.
    let server = start(
        "instance",
        write_mod(
            "instance",
            "game.register_on_chat(function(event)\n\
             \x20   if event.text == 'ship' then\n\
             \x20       local id = game.create_domain('places:ship', '17')\n\
             \x20       local again = game.create_domain('places:ship', '17')\n\
             \x20       if id ~= again then\n\
             \x20           game.log('MISMATCH ' .. tostring(id) .. ' ' .. tostring(again))\n\
             \x20           return false\n\
             \x20       end\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, id, { x = 8, y = 4, z = 8 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Shipwright").await;
        settle_for(&mut bot, 40).await;

        bot.chat("ship").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a ship made at runtime was not a domain anybody could be moved into"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        assert_eq!(
            switched_to(&bot).as_deref(),
            Some("places:ship/17"),
            "the instance did not carry the id the engine spells it with"
        );

        bot.disconnect().await;
    });
    server.stop();
}
