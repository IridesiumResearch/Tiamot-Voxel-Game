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
             game.register_domain{{ id = \"attic\", generator = function(buf, pos)\n\
             \x20   buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
             end }}\n\
             game.register_domain{{ id = \"void\" }}\n\
             game.register_domain{{ id = \"ship\", instanced = true,\n\
             \x20   generator = function(buf, pos)\n\
             \x20       buf:fill_below_heightmap(game.flat_heightmap(0), ground)\n\
             \x20   end }}\n\
             game.register_domain{{ id = \"space\", kind = \"sparse\", scale = 1000.0 }}\n\
             {extra}\n"
        ),
    )
    .expect("script");
    root
}

/// A server on a world that is NOT wiped first.
///
/// For the tests that restart one: `scratch` deletes what it finds, which is
/// right for a fresh fixture and fatal for a round trip.
fn restart_at(world: PathBuf, mods: PathBuf) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
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

/// Whether a stored chunk has anything solid in it.
///
/// Sampled over the chunk rather than asked of a summary: a chunk is a palette
/// and a bit array, and "is it empty" is a question about what the blocks say.
fn holds_anything(chunk: &tiamot_core::Chunk, at: tiamot_core::ChunkPos) -> bool {
    let corner = tiamot_core::BlockPos::from_chunk_corner(at);
    (0..16).any(|x| {
        (0..16).any(|y| {
            (0..16).any(|z| {
                chunk
                    .get_block(tiamot_core::BlockPos::new(
                        corner.x + x,
                        corner.y + y,
                        corner.z + z,
                    ))
                    .is_some_and(|view| view.filled_cells() > 0)
            })
        })
    })
}

/// Whether this bot has been told about any entity at all.
fn saw_a_spawn(bot: &Bot) -> bool {
    bot.received().into_iter().any(|message| {
        matches!(
            message,
            tiamot_core::proto::ServerMessage::EntitySpawn { .. }
        )
    })
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

#[test]
fn a_player_who_moves_stops_seeing_the_bodies_they_left_behind() {
    // **Criterion A3's other half.** Chunk interest cannot tell a body in this
    // domain from one standing at the same coordinates in another, because the
    // coordinates are identical — so a player who moved would watch the world
    // they left walking around inside the one they are in.
    let server = start(
        "bodies",
        write_mod(
            "bodies",
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
        let mut stayer = join(&server, "Stayer").await;
        settle_for(&mut traveller, 50).await;
        settle_for(&mut stayer, 50).await;

        // Non-vacuous first: while they share a domain, the traveller is told
        // about the other body. Without this the test passes on a server that
        // replicates nothing at all.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !saw_a_spawn(&traveller) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the two players never saw each other, so this cannot tell \
                 domain scoping from replication being broken outright"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), traveller.recv()).await;
        }

        traveller.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&traveller).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the traveller never moved"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), traveller.recv()).await;
        }

        // From here on, the one who stayed is in another space. Everything the
        // traveller hears about them from now is a leak.
        let before = traveller.received().len();
        for _ in 0..40 {
            let _ = stayer.walk([1.0, 0.0, 0.0], 0, 1).await;
        }
        settle_for(&mut traveller, 60).await;

        let leaked: Vec<_> = traveller
            .received()
            .into_iter()
            .skip(before)
            .filter(|message| {
                matches!(
                    message,
                    tiamot_core::proto::ServerMessage::EntitySpawn { .. }
                        | tiamot_core::proto::ServerMessage::EntityState { .. }
                )
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "a player in another domain was told about {} entity message(s) \
             from the one they left",
            leaked.len()
        );

        traveller.disconnect().await;
        stayer.disconnect().await;
    });
    server.stop();
}

#[test]
fn an_edit_in_another_domain_survives_a_restart_and_the_overworld_is_untouched() {
    // **Criterion: persistence.** A dig inside a second domain has to come back
    // under that domain's own key — and the overworld at the same coordinates
    // has to be exactly as it was, because a row is `(domain, position)` and
    // getting that wrong writes one space's edits into another's.
    let world = scratch("persist-world");
    let mods = write_mod(
        "persist",
        "game.register_on_chat(function(event)\n\
         \x20   if event.text == 'attic' then\n\
         \x20       local body = game.player_entity(event.player)\n\
         \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
         \x20       return false\n\
         \x20   end\n\
         end)",
    );

    // The block a player digs after being moved into the attic, decided by
    // where they land rather than assumed: the transfer puts them at (8, 4, 8),
    // and they fall to the floor the shared generator makes.
    let dug = std::sync::Arc::new(std::sync::Mutex::new(None));

    let first = restart_at(world.clone(), mods);
    let target = std::sync::Arc::clone(&dug);
    block_on(async {
        let mut bot = join(&first, "Builder").await;
        settle_for(&mut bot, 40).await;
        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "never moved");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 40).await;

        let here = bot.settle().await.expect("settle").block();
        let block = tiamot_core::BlockPos::new(here.x + 1, here.y - 1, here.z);
        bot.dig_block(block).await.expect("dig in the attic");
        *target.lock().expect("lock") = Some(block);

        bot.disconnect().await;
    });
    // Stopping is what writes it: a round trip that never closed the world is a
    // test of the cache.
    assert!(first.stop(), "the world should close cleanly");

    let block = dug.lock().expect("lock").expect("a block was dug");

    // **Asked of the world file, not of a client's memory.** A second session
    // has been told nothing about what happened in the first, so a delta-based
    // check would pass on a world where nothing was written at all.
    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen the world");

    let attic = db
        .load_chunk_in("places:attic", block.chunk())
        .expect("read")
        .expect("the attic chunk was written when the player dug in it");
    assert_eq!(
        attic.get_block(block).map(|view| view.filled_cells()),
        Some(0),
        "the hole dug in the attic did not survive the restart"
    );

    let overworld = db
        .load_chunk(block.chunk())
        .expect("read")
        .expect("the overworld chunk was written when the player stood on it");
    assert!(
        overworld
            .get_block(block)
            .is_some_and(|view| view.filled_cells() > 0),
        "the overworld at {block:?} was emptied by a dig in another domain, so \
         one space's edits are landing in another's rows"
    );
}

#[test]
fn a_mob_spawned_in_another_domain_comes_back_after_a_restart() {
    // **The arrival housekeeping, per domain.** Entities are stored under
    // `(domain, chunk)`, so a chunk arriving in a ship has to be asked about
    // the ship's mobs — a shared arrival list would load the overworld's and
    // leave the ship's on disk for ever, which reads as the ship being empty
    // every time somebody comes back to it.
    let world = scratch("mob-persist-world");
    let mods = write_mod(
        "mobpersist",
        "game.register_on_chat(function(event)\n\
         \x20   if event.text == 'attic' then\n\
         \x20       local body = game.player_entity(event.player)\n\
         \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
         \x20       return false\n\
         \x20   elseif event.text == 'spawn' then\n\
         \x20       local body = game.player_entity(event.player)\n\
         \x20       local me = game.entity(body)\n\
         \x20       local mob = game.spawn_entity{\n\
         \x20           pos = { x = me.pos.x + 1, y = me.pos.y, z = me.pos.z },\n\
         \x20           model = 'places:ground',\n\
         \x20       }\n\
         \x20       if mob then\n\
         \x20           game.transfer_entity(mob, 'places:attic',\n\
         \x20               { x = me.pos.x + 1, y = me.pos.y, z = me.pos.z })\n\
         \x20       end\n\
         \x20       return false\n\
         \x20   end\n\
         end)",
    );

    let first = restart_at(world.clone(), mods);
    block_on(async {
        let mut bot = join(&first, "Keeper").await;
        settle_for(&mut bot, 40).await;
        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "never moved");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 40).await;
        bot.chat("spawn").await.expect("chat");
        settle_for(&mut bot, 60).await;
        bot.disconnect().await;
    });
    assert!(first.stop(), "the world should close cleanly");

    // Asked of the world file: a mob in the attic must be written under the
    // ATTIC's key and not the overworld's, and there must be one.
    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen the world");

    // **The entities table, not just "something was stored".** Chunk rows alone
    // would satisfy a check on `stored_domains`, and the chunks are written
    // because the player stood there — which would make this pass with the mob
    // still in the overworld or nowhere at all.
    let home = tiamot_core::ChunkPos::new(0, 0, 0);
    let in_the_attic = db
        .load_chunk_entities_in("places:attic", home)
        .expect("read the attic's entities");
    assert_eq!(
        in_the_attic.len(),
        1,
        "the attic holds {} entities at {home:?}, so a mob moved into it was \
         not written under its domain",
        in_the_attic.len()
    );

    let in_the_overworld = db
        .load_chunk_entities(home)
        .expect("read the overworld's entities");
    assert!(
        in_the_overworld.is_empty(),
        "the overworld still holds {} entities at {home:?}: a mob that moved \
         out was left behind as a copy, which is how a world fills up with \
         everything that ever travelled",
        in_the_overworld.len()
    );
    drop(db);

    // **And it comes back.** Written correctly is half of it; the other half is
    // the arrival housekeeping asking the ATTIC about the attic's chunks. A
    // shared arrival list loads the overworld's mobs and leaves the ship's on
    // disk for ever, which reads as the ship being empty every time somebody
    // returns to it.
    //
    // Seen through replication, which is domain-scoped — so an entity reaching
    // a player standing in the attic is an entity that is in the attic.
    let second = restart_at(
        world,
        write_mod(
            "mobpersist-again",
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
        let mut bot = join(&second, "Returner").await;
        settle_for(&mut bot, 40).await;
        assert!(
            !saw_a_spawn(&bot),
            "somebody was already visible in the overworld, so seeing a body in \
             the attic would prove nothing"
        );

        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !saw_a_spawn(&bot) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the mob stored in the attic never came back when somebody \
                 returned to it"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }

        bot.disconnect().await;
    });
    second.stop();
}

#[test]
fn a_domain_nobody_visits_costs_nothing_and_a_sparse_one_never_holds_voxels() {
    // **Two criteria that are one observation about storage.** A registered
    // domain is a name until somebody goes there — lazily instantiated, so it
    // costs nothing on disk — and a `sparse` one has no voxels to store even
    // then. `places:attic` and `places:space` are both registered by the
    // fixture and neither is visited here.
    let world = scratch("zero-cost-world");
    let server = restart_at(world.clone(), write_mod("zerocost", ""));
    block_on(async {
        let mut bot = join(&server, "Homebody").await;
        settle_for(&mut bot, 60).await;
        bot.disconnect().await;
    });
    assert!(server.stop(), "the world should close cleanly");

    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen the world");

    let stored = db.stored_domains().expect("read");
    assert!(
        stored.iter().any(|domain| domain == "overworld"),
        "the overworld stored nothing, so this cannot tell a domain that costs \
         nothing from a world that was never written (domains: {stored:?})"
    );
    for empty in ["places:attic", "places:space", "places:ship", "places:void"] {
        assert!(
            !stored.iter().any(|domain| domain == empty),
            "`{empty}` has rows in a world nobody ever went to it in \
             (domains: {stored:?})"
        );
    }
}

#[test]
fn a_transfer_that_cannot_land_leaves_the_body_where_it_was() {
    // **Failure atomicity.** A transfer that moved the body first and then
    // failed would strand it in a space with no ground under it that nothing
    // will ever stream. So the destination is reached BEFORE anything moves,
    // and a destination that cannot be reached abandons the whole move.
    //
    // The reachable injection point is a position outside the world: the world
    // is finite (charter rule 6), and `Transform::from_world` on a coordinate
    // beyond it gives a chunk that is not in it.
    let server = start(
        "atomic",
        write_mod(
            "atomic",
            "game.register_on_chat(function(event)\n\
             \x20   if event.text == 'nowhere' then\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, 'places:attic',\n\
             \x20           { x = 1e18, y = 1e18, z = 1e18 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Stayer").await;
        settle_for(&mut bot, 40).await;
        let before = bot.settle().await.expect("settle").block();

        bot.chat("nowhere").await.expect("chat");
        settle_for(&mut bot, 80).await;

        assert_eq!(
            switched_to(&bot),
            None,
            "a transfer to a place outside the world moved the player anyway"
        );
        let after = bot.settle().await.expect("settle").block();
        assert_eq!(
            (after.x, after.z),
            (before.x, before.z),
            "the player was moved by a transfer that could not land"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_sparse_domain_takes_a_body_and_never_stores_a_voxel() {
    // **Criterion: sparse domains.** `kind = "sparse"` is entities and nothing
    // else — the shape a space-like domain would use, which the engine does not
    // know or care is space. So a body can be moved into it, and no chunk is
    // ever stored for it however long somebody stands there.
    //
    // A body over terrain that is not loaded does not move, which is what a
    // space with no terrain at all looks like from the physics: they float
    // rather than falling for ever.
    let world = scratch("sparse-world");
    let server = restart_at(
        world.clone(),
        write_mod(
            "sparse",
            "game.register_on_chat(function(event)\n\
             \x20   if event.text == 'space' then\n\
             \x20       local body = game.player_entity(event.player)\n\
             \x20       game.transfer_entity(body, 'places:space', { x = 0, y = 40, z = 0 })\n\
             \x20       return false\n\
             \x20   end\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Astronaut").await;
        settle_for(&mut bot, 40).await;

        bot.chat("space").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "a sparse domain would not take a body, so it is not a domain"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        assert_eq!(switched_to(&bot).as_deref(), Some("places:space"));

        // Long enough that a domain which DID store chunks would have stored
        // some: the stream asks for everything in range as soon as it arrives.
        settle_for(&mut bot, 120).await;
        bot.disconnect().await;
    });
    assert!(server.stop(), "the world should close cleanly");

    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen the world");
    let stored = db.stored_domains().expect("read");
    assert!(
        stored.iter().any(|domain| domain == "overworld"),
        "nothing was stored at all, so this cannot tell a sparse domain from a \
         world that was never written (domains: {stored:?})"
    );
    assert!(
        !stored.iter().any(|domain| domain == "places:space"),
        "a sparse domain stored something; it takes entities and nothing else \
         (domains: {stored:?})"
    );
}

#[test]
fn a_domain_whose_mod_was_removed_keeps_its_contents_and_gives_them_back() {
    // **Charter rule 8's rule for materials, applied to spaces.** Data a mod
    // cannot currently name is still that player's data. A world holding a
    // domain nothing registers any more must open, keep its rows, and hand them
    // back unchanged when the mod returns — the same promise
    // `a_pond_whose_mod_was_removed_survives_and_comes_back` makes for fluid.
    let world = scratch("forgotten-world");
    let with_attic = write_mod(
        "forgotten",
        "game.register_on_chat(function(event)\n\
         \x20   if event.text == 'attic' then\n\
         \x20       local body = game.player_entity(event.player)\n\
         \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
         \x20       return false\n\
         \x20   end\n\
         end)",
    );

    let first = restart_at(world.clone(), with_attic.clone());
    block_on(async {
        let mut bot = join(&first, "Builder").await;
        settle_for(&mut bot, 40).await;
        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "never moved");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 60).await;
        bot.disconnect().await;
    });
    assert!(first.stop(), "the world should close cleanly");

    let stored_before = {
        let mut registry = tiamot_core::Registry::new();
        let db = tiamot_core::persist::WorldDb::open(
            world.join(tiamot_server::handle::WORLD_FILE),
            &mut registry,
        )
        .expect("reopen");
        db.stored_domains().expect("read")
    };
    assert!(
        stored_before.iter().any(|domain| domain == "places:attic"),
        "the attic stored nothing, so there is nothing for a removed mod to \
         lose (domains: {stored_before:?})"
    );

    // **Now open the same world with a mod set that knows nothing about it.**
    // A different mod id, so `places:attic` is a domain no registration names.
    let stranger = {
        let root = scratch("forgotten-stranger");
        let dir = root.join("stranger");
        std::fs::create_dir_all(&dir).expect("mod dir");
        std::fs::write(
            dir.join("mod.toml"),
            "id = \"stranger\"\nname = \"Stranger\"\nversion = \"0.1.0\"\n\
             license = \"GPL-3.0-only\"\n",
        )
        .expect("manifest");
        std::fs::write(dir.join("init.lua"), "-- knows nothing of the attic\n").expect("script");
        root
    };
    let second = restart_at(world.clone(), stranger);
    block_on(async {
        let mut bot = join(&second, "Stranger").await;
        settle_for(&mut bot, 40).await;
        bot.disconnect().await;
    });
    assert!(
        second.stop(),
        "a world holding a domain nothing registers would not close"
    );

    let stored_after = {
        let mut registry = tiamot_core::Registry::new();
        let db = tiamot_core::persist::WorldDb::open(
            world.join(tiamot_server::handle::WORLD_FILE),
            &mut registry,
        )
        .expect("a world with an unregistered domain must still open");
        db.stored_domains().expect("read")
    };
    assert!(
        stored_after.iter().any(|domain| domain == "places:attic"),
        "opening the world without the mod that made the attic dropped it \
         (domains: {stored_after:?})"
    );

    // And it comes back: the mod returns, and the space is enterable again.
    let third = restart_at(world, with_attic);
    block_on(async {
        let mut bot = join(&third, "Returner").await;
        settle_for(&mut bot, 40).await;
        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the attic did not come back when its mod did"
            );
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        bot.disconnect().await;
    });
    third.stop();
}

#[test]
fn a_ship_with_somebody_in_it_is_not_scuttled() {
    // **The same defect as breaking a container somebody has open**, which is
    // the comparison the engine's own refusal is written against. A player is
    // moved into a ship and the mod then tries to destroy it; the ship has to
    // survive, and the player has to still be in it.
    //
    // Then they leave, it is destroyed for real, and its rows go with it.
    let world = scratch("scuttle-world");
    let server = restart_at(
        world.clone(),
        write_mod(
            "scuttle",
            "local id = nil\n\
             game.register_on_chat(function(event)\n\
             \x20   local body = game.player_entity(event.player)\n\
             \x20   if event.text == 'aboard' then\n\
             \x20       id = game.create_domain('places:ship', '17')\n\
             \x20       game.transfer_entity(body, id, { x = 8, y = 4, z = 8 })\n\
             \x20   elseif event.text == 'scuttle' then\n\
             \x20       game.destroy_domain(id)\n\
             \x20   elseif event.text == 'ashore' then\n\
             \x20       game.transfer_entity(body, 'overworld', { x = 8, y = 4, z = 8 })\n\
             \x20   end\n\
             \x20   return false\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Captain").await;
        settle_for(&mut bot, 40).await;

        bot.chat("aboard").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).as_deref() != Some("places:ship/17") {
            assert!(tokio::time::Instant::now() < deadline, "never boarded");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 60).await;

        // Scuttled with the captain aboard: refused, and they are still there.
        bot.chat("scuttle").await.expect("chat");
        settle_for(&mut bot, 60).await;
        assert_eq!(
            switched_to(&bot).as_deref(),
            Some("places:ship/17"),
            "destroying an occupied domain moved the player out of it"
        );

        // Ashore, and then it really goes.
        bot.chat("ashore").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).as_deref() != Some("overworld") {
            assert!(tokio::time::Instant::now() < deadline, "never went ashore");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 40).await;
        bot.chat("scuttle").await.expect("chat");
        settle_for(&mut bot, 60).await;

        bot.disconnect().await;
    });
    assert!(server.stop(), "the world should close cleanly");

    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen");
    let stored = db.stored_domains().expect("read");
    assert!(
        stored.iter().any(|domain| domain == "overworld"),
        "nothing was stored at all (domains: {stored:?})"
    );
    assert!(
        !stored.iter().any(|domain| domain == "places:ship/17"),
        "an empty ship was destroyed and its rows are still there \
         (domains: {stored:?})"
    );
}

#[test]
fn what_a_player_carries_and_who_they_are_survive_the_move() {
    // **Criterion: identity and inventory survive a transfer.** Moving between
    // spaces is a handoff of a body, not a new session — so what they were
    // carrying is still theirs on the far side. The engine has no idea what an
    // inventory means (charter rule 1); what it must not do is drop one.
    let server = start(
        "carry",
        write_mod(
            "carry",
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
        let mut bot = join(&server, "Carrier").await;
        settle_for(&mut bot, 40).await;

        // Dig something, so there is something to carry. Non-vacuous by
        // construction: an empty inventory survives every possible bug.
        let here = bot.settle().await.expect("settle").block();
        let block = tiamot_core::BlockPos::new(here.x + 1, here.y - 1, here.z);
        bot.dig_block(block).await.expect("dig");
        let carried: u32 = bot
            .await_inventory(Duration::from_secs(5))
            .await
            .expect("inventory")
            .iter()
            .map(|stack| stack.units)
            .sum();
        assert!(carried > 0, "nothing was picked up, so nothing can be lost");

        bot.chat("attic").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "never moved");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 60).await;

        let still: u32 = bot
            .await_inventory(Duration::from_secs(5))
            .await
            .expect("inventory")
            .iter()
            .map(|stack| stack.units)
            .sum();
        assert_eq!(
            still, carried,
            "a player carrying {carried} units arrived in another domain with \
             {still}"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_ship_keeps_what_was_built_in_it_across_a_restart() {
    // **Criterion: a runtime instance is indistinguishable from a registered
    // domain downstream.** `places:attic` is registered in the window and
    // `places:ship/17` is made while the world runs, and persistence must not
    // be able to tell them apart — the instance's edits belong under the
    // instance's own key.
    let world = scratch("ship-persist-world");
    let mods = write_mod(
        "shippersist",
        "game.register_on_chat(function(event)\n\
         \x20   if event.text == 'aboard' then\n\
         \x20       local body = game.player_entity(event.player)\n\
         \x20       local id = game.create_domain('places:ship', '17')\n\
         \x20       game.transfer_entity(body, id, { x = 8, y = 4, z = 8 })\n\
         \x20       return false\n\
         \x20   end\n\
         end)",
    );
    let dug = std::sync::Arc::new(std::sync::Mutex::new(None));

    let server = restart_at(world.clone(), mods);
    let target = std::sync::Arc::clone(&dug);
    block_on(async {
        let mut bot = join(&server, "Shipwright").await;
        settle_for(&mut bot, 40).await;
        bot.chat("aboard").await.expect("chat");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while switched_to(&bot).as_deref() != Some("places:ship/17") {
            assert!(tokio::time::Instant::now() < deadline, "never boarded");
            let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
        }
        settle_for(&mut bot, 40).await;

        let here = bot.settle().await.expect("settle").block();
        let block = tiamot_core::BlockPos::new(here.x + 1, here.y - 1, here.z);
        bot.dig_block(block).await.expect("dig aboard");
        *target.lock().expect("lock") = Some(block);
        bot.disconnect().await;
    });
    assert!(server.stop(), "the world should close cleanly");

    let block = dug.lock().expect("lock").expect("a block was dug");
    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen");

    let aboard = db
        .load_chunk_in("places:ship/17", block.chunk())
        .expect("read")
        .expect("the ship's chunk was written when somebody stood in it");
    assert_eq!(
        aboard.get_block(block).map(|view| view.filled_cells()),
        Some(0),
        "what was built in a ship made at runtime did not survive the restart"
    );

    let overworld = db
        .load_chunk(block.chunk())
        .expect("read")
        .expect("the overworld chunk exists");
    assert!(
        overworld
            .get_block(block)
            .is_some_and(|view| view.filled_cells() > 0),
        "a dig aboard a runtime instance landed in the overworld's rows"
    );
}

#[test]
fn a_domain_is_filled_by_its_own_generator_and_not_by_the_overworlds() {
    // **A domain fills itself, or it is air.** Running every mod's
    // `on_generate` in every space would fill anything anybody ever made with
    // the overworld's ground — a hill through the middle of somebody's hull.
    // So the overworld gets its generators, a domain that named one gets that,
    // and a domain that named none gets nothing.
    //
    // `places:void` names no generator and must be empty. `places:loft` names
    // one, and must have what it made. `places:attic` names one too, which is
    // what the other tests in this file stand on — a domain with no generator
    // is a domain you fall through for ever, which is correct and is why the
    // fixture gives the rooms people walk about in some ground.
    let world = scratch("generators-world");
    let server = restart_at(
        world.clone(),
        write_mod(
            "generators",
            "local slab = game.register_block{ id = 'slab' }\n\
             game.register_domain{ id = 'loft', generator = function(buf, pos)\n\
             \x20   buf:fill_below_heightmap(game.flat_heightmap(-4), slab)\n\
             end }\n\
             game.register_on_chat(function(event)\n\
             \x20   local body = game.player_entity(event.player)\n\
             \x20   if event.text == 'attic' then\n\
             \x20       game.transfer_entity(body, 'places:attic', { x = 8, y = 4, z = 8 })\n\
             \x20   elseif event.text == 'loft' then\n\
             \x20       game.transfer_entity(body, 'places:loft', { x = 8, y = 4, z = 8 })\n\
             \x20   end\n\
             \x20   return false\n\
             end)",
        ),
    );
    block_on(async {
        let mut bot = join(&server, "Surveyor").await;
        settle_for(&mut bot, 40).await;
        for room in ["attic", "loft"] {
            bot.chat(room).await.expect("chat");
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !switched_to(&bot).is_some_and(|domain| domain.ends_with(room)) {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "never reached `{room}`"
                );
                let _ = tokio::time::timeout(Duration::from_millis(60), bot.recv()).await;
            }
            settle_for(&mut bot, 80).await;
        }
        bot.disconnect().await;
    });
    assert!(server.stop(), "the world should close cleanly");

    let mut registry = tiamot_core::Registry::new();
    let db = tiamot_core::persist::WorldDb::open(
        world.join(tiamot_server::handle::WORLD_FILE),
        &mut registry,
    )
    .expect("reopen");

    // The overworld is full, which is what says the fixture's generator ran at
    // all — without this, "the attic is empty" is also true of a broken world.
    let ground = db
        .load_chunk(tiamot_core::ChunkPos::new(0, -1, 0))
        .expect("read")
        .expect("the overworld under the player was written");
    assert!(
        holds_anything(&ground, tiamot_core::ChunkPos::new(0, -1, 0)),
        "the overworld generated nothing, so this cannot tell an empty domain \
         from an empty world"
    );

    let void = db
        .load_chunk_in("places:void", tiamot_core::ChunkPos::new(0, -1, 0))
        .expect("read");
    assert!(
        void.is_none_or(|chunk| !holds_anything(&chunk, tiamot_core::ChunkPos::new(0, -1, 0))),
        "a domain that named no generator was filled by the overworld's"
    );

    let loft = db
        .load_chunk_in("places:loft", tiamot_core::ChunkPos::new(0, -1, 0))
        .expect("read")
        .expect("a domain with its own generator was written");
    assert!(
        holds_anything(&loft, tiamot_core::ChunkPos::new(0, -1, 0)),
        "a domain's own generator did not fill it"
    );
}
