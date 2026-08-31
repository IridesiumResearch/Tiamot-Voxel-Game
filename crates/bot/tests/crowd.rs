// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Bodies make room for each other.
//!
//! **Reported from the window**: "players should very subtly collide with mobs
//! and each other, just like in Minecraft."
//!
//! Two players join the same world and stand still on the same spot. What the
//! test asserts is that they end up apart, that they got there gently, and that
//! neither was pushed vertically — a crowd that can push up is a crowd that can
//! lift somebody through a ceiling.
//!
//! **The mob half is not here.** A mimic walks under its own steam, so a bot
//! test that watched one move could not tell a push from a stride; the entity
//! side is `Population::a_body_in_the_crowd_is_pushed_out_of_a_players_space`
//! in `server::ent`, where the mob can be put somewhere and told to stay.

use std::path::{Path, PathBuf};

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-crowd").join(name);
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
        bind_addr: "127.0.0.1:0".parse().expect("an address"),
        world_path: scratch(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
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

/// Stands still for `ticks` and reports where that left the body.
///
/// Standing still is the point: a push that only showed up while walking would
/// be indistinguishable from ordinary movement.
async fn stand(bot: &mut Bot, ticks: u64) -> [f64; 3] {
    let at = bot
        .walk([0.0, 0.0, 0.0], 0, ticks)
        .await
        .expect("the server should keep reporting where the body is");
    let axis = |index: usize| {
        f64::from(match index {
            0 => at.chunk.x,
            1 => at.chunk.y,
            _ => at.chunk.z,
        }) * f64::from(tiamot_core::CHUNK_SUBNODES)
            + f64::from(at.local[index])
    };
    [axis(0), axis(1), axis(2)]
}

fn flat_distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    (dx * dx + dz * dz).sqrt()
}

#[test]
fn two_players_on_one_spot_lean_apart() {
    let server = start("apart");
    block_on(async {
        let mut first = join(&server, "First").await;
        let mut second = join(&server, "Second").await;

        // Both spawn at the same place, so the first reading is taken with
        // them already inside each other. A handful of ticks first, so that
        // both bodies exist on the server before either measurement.
        let start_a = stand(&mut first, 4).await;
        let start_b = stand(&mut second, 4).await;
        let at_first = flat_distance(start_a, start_b);

        for _ in 0..6 {
            let _ = stand(&mut first, 5).await;
            let _ = stand(&mut second, 5).await;
        }
        let end_a = stand(&mut first, 2).await;
        let end_b = stand(&mut second, 2).await;
        let at_last = flat_distance(end_a, end_b);

        // **Not "they gained 0.2 since the first reading".** That was a bet on
        // how fast the machine is. The push starts the moment the second body
        // exists, which is before the first measurement can be taken, so on a
        // slow runner most of the separation has already happened by then —
        // macOS CI read 1.808 then 1.993 and failed a 0.2 threshold having done
        // exactly the right thing.
        //
        // What is true on every machine is where they END UP. Two bodies that
        // spawned on one spot come to rest a snug body-width apart, which is
        // the distance `phys::crowd::separate` pushes to and then stops at.
        let resting =
            f64::from(tiamot_core::phys::PLAYER_WIDTH * tiamot_core::phys::crowd::SNUGNESS);
        assert!(
            at_last >= resting - 0.05,
            "two players standing on one spot did not make room for each other: \
             {at_first:.3} cells apart, then {at_last:.3}, which is inside the \
             resting distance of {resting:.3}"
        );
        assert!(
            at_last >= at_first - 0.05,
            "the pair drifted back together: {at_first:.3} cells apart, then {at_last:.3}"
        );

        // **Subtle, which was the requirement.** Sixty ticks is three seconds;
        // if the pair had been flung, they would be a long way apart rather
        // than a body's width.
        assert!(
            at_last < 12.0,
            "the push was not subtle: {at_last:.3} cells apart after three seconds"
        );

        // And nobody was lifted. The world is flat here, so a body that
        // changed height was pushed there.
        for (start, end, who) in [(start_a, end_a, "first"), (start_b, end_b, "second")] {
            assert!(
                (end[1] - start[1]).abs() < 1.0,
                "the {who} player's height changed by {:.3} cells while standing on flat ground",
                end[1] - start[1]
            );
        }

        first.disconnect().await;
        second.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_player_on_their_own_is_left_where_they_stand() {
    // The counter-example the test above needs: if a body drifted anyway, two
    // bodies drifting apart would prove nothing.
    let server = start("alone");
    block_on(async {
        let mut alone = join(&server, "Alone").await;
        let start = stand(&mut alone, 4).await;
        for _ in 0..6 {
            let _ = stand(&mut alone, 5).await;
        }
        let end = stand(&mut alone, 2).await;
        assert!(
            flat_distance(start, end) < 0.2,
            "a player standing alone wandered {:.3} cells",
            flat_distance(start, end)
        );
        alone.disconnect().await;
    });
    server.stop();
}
