// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The Mimic, against the real reference mods on a real server.
//!
//! # This is the acceptance test for the mod API, not for a mob
//!
//! `game/core_mimic/` is a fixture (see `game/README.md`), and its whole job is
//! to be built entirely out of the public surface: the join hook, `game.storage`,
//! entity spawning with a live-resolving nametag, `entities_in_radius` finding
//! players — who are entities now — `game.line_of_sight`, the per-entity step
//! callback, and `on_punch`. If any of that had needed engine support a
//! third-party mod could not reach, that would be a bug in the API rather than
//! something to work around in the mod (charter rule 1).
//!
//! The companion check is a grep, in `no_engine_special_cases`: the string
//! "mimic" appears nowhere outside `game/`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-mimic").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
}

fn reference_mods() -> PathBuf {
    repo().join("game")
}

/// A server whose world directory persists between calls, so a restart finds
/// the world it left.
fn start_in(world: &Path) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world.to_path_buf(),
        max_players: 8,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        seed: Some(4),
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

async fn join_as(server: &ServerHandle, identity: Identity, name: &str) -> Bot {
    let mut bot = Bot::connect(server.local_addr(), identity, server.cert_fingerprint())
        .await
        .expect("connect");
    bot.join(name).await.expect("join");
    bot
}

/// The mimic, as the bot sees it: the engine's humanoid, wearing somebody's
/// name, that is not the body of anyone connected.
fn is_mimic(entity: &tiamot_core::proto::EntityDef, wearing: &str) -> bool {
    entity.model.as_deref() == Some("engine:humanoid") && entity.nametag.as_deref() == Some(wearing)
}

#[test]
fn the_mimic_imprints_on_the_first_player_and_wears_their_name() {
    let world = scratch("imprint-world");
    let server = start_in(&world);
    block_on(async {
        let mut ada = join_as(&server, Identity::generate().expect("identity"), "Ada").await;

        // Two entities end up called "Ada": Ada's own body, which she is never
        // told about, and the mimic, which she is. So anything she can see
        // wearing her name IS the mimic.
        let mimic = ada
            .expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
            .await
            .expect("a mimic should appear, wearing the first player's name");

        assert!(
            mimic.collider.is_some(),
            "the mimic has no box, so nothing can walk into it"
        );
    });
}

#[test]
fn the_imprint_survives_a_restart_and_a_second_player_does_not_take_it() {
    let world = scratch("restart-world");
    // The same seed twice, because an identity is its seed (charter rule 13)
    // and Ada has to be the same person after the restart — which is the whole
    // thing under test.
    let ada_seed = Identity::generate().expect("identity").seed();
    let bob_key = Identity::generate().expect("identity");

    // First run: Ada arrives first, so the mimic is hers.
    {
        let server = start_in(&world);
        block_on(async {
            let mut ada = join_as(&server, Identity::from_seed(&ada_seed), "Ada").await;
            ada.expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
                .await
                .expect("the mimic should imprint on Ada");
        });
        drop(server);
    }

    // Second run, same world directory. Bob arrives FIRST this time, and the
    // mimic must still be Ada's: the imprint is a fact about the world, stored
    // against a UUID, and "whoever showed up first today" is not it.
    {
        let server = start_in(&world);
        block_on(async {
            let mut bob = join_as(&server, bob_key, "Bob").await;
            let mimic = bob
                .expect_entity(
                    |entity| entity.model.as_deref() == Some("engine:humanoid"),
                    PATIENCE,
                )
                .await
                .expect("the mimic should still be there after a restart");
            assert_eq!(
                mimic.nametag.as_deref(),
                Some("Ada"),
                "the imprint moved to whoever joined first after the restart"
            );

            // And Ada rejoining does not create a second one.
            let mut ada = join_as(&server, Identity::from_seed(&ada_seed), "Ada").await;
            ada.expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
                .await
                .expect("Ada should see the mimic too");
            let mimics = ada
                .entities()
                .values()
                .filter(|entity| entity.model.as_deref() == Some("engine:humanoid"))
                .count();
            // Ada sees the mimic and Bob's body, both humanoids, and never
            // herself.
            assert!(
                mimics <= 2,
                "the world grew a second mimic across the restart: {mimics} humanoids"
            );
        });
    }
}

#[test]
fn the_mimic_follows_the_player_it_imprinted_on() {
    let world = scratch("follow-world");
    let server = start_in(&world);
    block_on(async {
        let mut ada = join_as(&server, Identity::generate().expect("identity"), "Ada").await;
        let mimic = ada
            .expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
            .await
            .expect("a mimic should appear");

        let started = ada
            .entities()
            .get(&mimic.id)
            .map(|entity| (entity.chunk, entity.local))
            .expect("the mimic is known");

        // Walk for a few seconds. The mimic replays the path two seconds
        // behind, so it has to have moved by the end of it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut moved = false;
        while tokio::time::Instant::now() < deadline && !moved {
            ada.walk([1.0, 0.0, 0.0], 0, 8).await.expect("walk");
            if let Some(entity) = ada.entities().get(&mimic.id) {
                let same_chunk = entity.chunk == started.0;
                let drift = (entity.local[0] - started.1[0]).abs()
                    + (entity.local[1] - started.1[1]).abs()
                    + (entity.local[2] - started.1[2]).abs();
                moved = !same_chunk || drift > 1.0;
            }
        }
        assert!(
            moved,
            "the mimic never moved while the player it imprinted on walked away"
        );
    });
}

/// An entity's position in world blocks. Chunk times sixteen plus the cell
/// offset over three — the conversion the mod API does for a mod, done here
/// because a test reads the wire and the wire carries the chunk frame.
fn world_of(entity: &tiamot_core::proto::EntityDef) -> [f64; 3] {
    let span = f64::from(tiamot_core::CHUNK_SUBNODES);
    let per_block = f64::from(tiamot_core::SUBNODES_PER_AXIS);
    [
        (f64::from(entity.chunk.x) * span + f64::from(entity.local[0])) / per_block,
        (f64::from(entity.chunk.y) * span + f64::from(entity.local[1])) / per_block,
        (f64::from(entity.chunk.z) * span + f64::from(entity.local[2])) / per_block,
    ]
}

fn flat_distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (dx, dz) = (a[0] - b[0], a[2] - b[2]);
    (dx * dx + dz * dz).sqrt()
}

#[test]
fn the_mimic_ignores_a_player_it_did_not_imprint_on() {
    // The criterion says "ignores bot B", and it is the part of an imprint that
    // is easy to get wrong: a mob that follows whoever is nearest looks
    // identical to one that follows the right person, right up until a second
    // player walks past.
    let world = scratch("ignores-world");
    let server = start_in(&world);
    block_on(async {
        let mut ada = join_as(&server, Identity::generate().expect("identity"), "Ada").await;
        ada.expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
            .await
            .expect("a mimic should appear for Ada");

        let mut bob = join_as(&server, Identity::generate().expect("identity"), "Bob").await;

        // Bob walks away; Ada stands where she is. Not too far: past the view
        // distance he stops being replicated to Ada at all, and a test that
        // cannot see him cannot say the mimic ignored him.
        for _ in 0..10 {
            bob.walk([1.0, 0.0, 0.0], 0, 8).await.expect("walk");
        }
        let here = ada.walk([0.0; 3], 0, 4).await.expect("stand");
        let ada_at = [
            (f64::from(here.chunk.x) * 48.0 + f64::from(here.local[0])) / 3.0,
            (f64::from(here.chunk.y) * 48.0 + f64::from(here.local[1])) / 3.0,
            (f64::from(here.chunk.z) * 48.0 + f64::from(here.local[2])) / 3.0,
        ];

        let seen = ada.entities();
        let mimic = seen
            .values()
            .find(|entity| is_mimic(entity, "Ada"))
            .expect("the mimic is still there");
        let bob_body = seen
            .values()
            .find(|entity| entity.nametag.as_deref() == Some("Bob"))
            .expect("Bob is still there");

        let to_ada = flat_distance(world_of(mimic), ada_at);
        let to_bob = flat_distance(world_of(mimic), world_of(bob_body));
        assert!(
            to_ada < to_bob,
            "the mimic ended up nearer the player it did not imprint on: \
             {to_ada:.1} blocks from Ada, {to_bob:.1} from Bob"
        );
    });
}

#[test]
fn the_mimic_flees_when_it_is_punched() {
    let world = scratch("flee-world");
    let server = start_in(&world);
    block_on(async {
        let mut ada = join_as(&server, Identity::generate().expect("identity"), "Ada").await;
        let mimic = ada
            .expect_entity(|entity| is_mimic(entity, "Ada"), PATIENCE)
            .await
            .expect("a mimic should appear");

        // Let it settle beside her first, so the distance below is the one it
        // chose rather than the one it spawned at.
        for _ in 0..20 {
            ada.walk([0.0; 3], 0, 4).await.expect("stand");
        }
        let here = ada.walk([0.0; 3], 0, 1).await.expect("stand");
        let ada_at = [
            (f64::from(here.chunk.x) * 48.0 + f64::from(here.local[0])) / 3.0,
            (f64::from(here.chunk.y) * 48.0 + f64::from(here.local[1])) / 3.0,
            (f64::from(here.chunk.z) * 48.0 + f64::from(here.local[2])) / 3.0,
        ];
        let before = ada
            .entities()
            .get(&mimic.id)
            .map(|entity| flat_distance(world_of(entity), ada_at))
            .expect("the mimic is known");

        ada.punch(mimic.id).await.expect("punch");

        // It runs for three seconds. The furthest it gets in that time is the
        // measurement: asserting on the distance at one particular moment would
        // be asserting on when the reply happened to arrive.
        let mut furthest = before;
        for _ in 0..12 {
            ada.walk([0.0; 3], 0, 4).await.expect("stand");
            if let Some(entity) = ada.entities().get(&mimic.id) {
                furthest = furthest.max(flat_distance(world_of(entity), ada_at));
            }
        }
        assert!(
            furthest > before + 2.0,
            "the mimic did not flee: it was {before:.1} blocks away and got no \
             further than {furthest:.1}"
        );
    });
}

#[test]
fn no_engine_special_cases() {
    // The criterion, as a grep. `core_mimic` is content and the engine must not
    // know it exists — if any of this mob needed a hook nobody else could have,
    // the API is what should have grown, not core.
    //
    // **Engine SOURCE**, which is what the criterion is about. Test files are
    // deliberately outside it: `crates/core/tests/mods.rs` names every mod in
    // `game/` on purpose, because enumerating the shipped fixture set is how it
    // notices one being added or removed. That is a test knowing what ships,
    // not the engine knowing what a mimic is.
    let repo = repo();
    let mut offenders = Vec::new();
    for dir in ["docs", "fuzz", "spikes"] {
        walk(&repo.join(dir), &mut offenders);
    }
    let crates = std::fs::read_dir(repo.join("crates")).expect("the crates directory");
    for entry in crates.flatten() {
        walk(&entry.path().join("src"), &mut offenders);
    }
    assert!(
        offenders.is_empty(),
        "the engine mentions the mimic, which means it was special-cased: {offenders:?}"
    );
}

/// Every file under `dir` whose text mentions the mob.
fn walk(dir: &Path, offenders: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            walk(&path, offenders);
        } else if let Ok(text) = std::fs::read_to_string(&path)
            && text.to_lowercase().contains("mimic")
        {
            offenders.push(path.display().to_string());
        }
    }
}
