// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What other people are holding, and which way they are facing.
//!
//! **Two reports and one root cause between them.** Every other figure had
//! empty hands, because nothing on the wire said what anybody but the local
//! player was carrying; and a body faced the mirror image of its owner, twice,
//! because `Bot::walk` sent `look: [0.0, 0.0]` and yaw zero is the one value
//! where a camera's angle and a figure's agree.
//!
//! The second is why the first could go unnoticed: a suite in which nobody can
//! turn and nobody holds anything is a suite that cannot see either.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-hands").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root")
        .join("game")
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(5),
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

/// The numeric id of a material by its string id, which is per-session.
fn material_of(bot: &Bot, name: &str) -> u16 {
    bot.material_table()
        .expect("the server should have sent a material table")
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.id)
        .unwrap_or_else(|| panic!("the reference mods should register `{name}`"))
}

/// Drives the connection until a condition holds, or gives up.
async fn until(bot: &mut Bot, timeout: Duration, done: impl Fn(&Bot) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if done(bot) {
            return true;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), bot.recv()).await;
    }
    done(bot)
}

#[test]
fn one_player_sees_what_another_is_holding() {
    // **Reported from the window**: every other figure has empty hands. The
    // client draws what the LOCAL player holds from its own inventory, and
    // nothing on the wire said what anybody else had.
    let server = start("sees-hands");
    block_on(async {
        let mut ada = join(&server, "Ada").await;
        let mut bert = join(&server, "Bert").await;

        let sword = material_of(&ada, "core_gear:sword");

        // Ada finds Bert. Both are at spawn, so he is already in view.
        let bert_body = ada
            .expect_entity(|entity| entity.nametag.as_deref() == Some("Bert"), PATIENCE)
            .await
            .expect("Ada should see Bert");
        assert_eq!(
            bert_body.hands,
            [None, None],
            "Bert is not holding anything yet"
        );

        // Bert picks up a sword.
        bert.chat("gear").await.expect("ask");
        assert!(
            until(&mut bert, PATIENCE, |bot| bot
                .inventory()
                .iter()
                .any(|stack| stack.material == sword))
            .await,
            "Bert never got a sword"
        );
        bert.select_slot(0).await.expect("select");

        // **And Ada is told**, without Bert doing anything else. This is the
        // whole feature: it is not enough that the server knows.
        assert!(
            until(&mut ada, PATIENCE, |bot| {
                bot.entities().get(&bert_body.id).is_some_and(|entity| {
                    entity.hands[0]
                        .as_ref()
                        .is_some_and(|stack| stack.material == sword)
                })
            })
            .await,
            "Ada never saw the sword in Bert's hand: {:?}",
            ada.entities()
                .get(&bert_body.id)
                .map(|entity| entity.hands.clone())
        );

        ada.disconnect().await;
        bert.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_body_faces_the_way_its_owner_is_looking() {
    // **The bug that survived two fixes**, because nothing could turn a body:
    // `Bot::walk` sent `look: [0.0, 0.0]` and yaw zero is the one value where a
    // camera's angle and a figure's agree. A camera looks along
    // `(-sin yaw, cos yaw)` and a figure faces `(sin yaw, cos yaw)`, so the
    // conversion is a negation and leaving it out is invisible until somebody
    // turns.
    //
    // Quarter turns, so the answer is an axis and a sign error cannot hide in
    // the rounding of a byte.
    let server = start("facing");
    block_on(async {
        let mut ada = join(&server, "Ada").await;
        let mut bert = join(&server, "Bert").await;

        let bert_body = ada
            .expect_entity(|entity| entity.nametag.as_deref() == Some("Bert"), PATIENCE)
            .await
            .expect("Ada should see Bert");

        for (turns, name) in [(0.25_f32, "east"), (0.5, "south"), (0.75, "west")] {
            bert.look_at([turns, 0.0]);
            // A tick or two of standing still, so the look reaches the server.
            for _ in 0..8 {
                let _ = bert.walk([0.0; 3], 0, 2).await;
            }

            // What the SERVER says the body faces, quantised to a byte.
            let want = tiamot_core::ent::replicate::quantise_yaw(tiamot_core::ent::figure_yaw(
                turns * std::f32::consts::TAU,
            ));
            assert!(
                until(&mut ada, PATIENCE, |bot| {
                    bot.entities()
                        .get(&bert_body.id)
                        .is_some_and(|entity| entity.yaw.abs_diff(want) <= 2)
                })
                .await,
                "Bert is looking {name} and Ada sees his body at {:?} rather than {want}",
                ada.entities().get(&bert_body.id).map(|entity| entity.yaw)
            );
        }

        ada.disconnect().await;
        bert.disconnect().await;
    });
    server.stop();
}
