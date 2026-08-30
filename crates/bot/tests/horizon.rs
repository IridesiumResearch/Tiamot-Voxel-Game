// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The horizon, over a real server: chunks too far away to send in full.
//!
//! Task 15b's streaming half. The unit tests pin the ring maths and the cache;
//! these pin the thing a player experiences — that land past the detail radius
//! arrives at all, that it arrives as a summary rather than a chunk, and that
//! the two are never both held for one position.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_core::{BlockPos, ChunkPos};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];

fn world_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-horizon-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The reference mods, whose generator fills everything below y = 0.
fn reference_mods() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn start(name: &str, view: ViewDistance) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world_dir(name),
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: view,
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(1),
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

/// Everything the horizon produced for one connection.
#[derive(Default)]
struct Seen {
    /// Positions sent in full.
    chunks: Vec<ChunkPos>,
    /// Positions sent as a summary, and the level each came at.
    summaries: Vec<(ChunkPos, u8)>,
}

/// Reads until `enough` summaries have arrived, or `patience` runs out.
///
/// The patience is the failure path, not the normal one: an assertion that
/// waited for an exact count would be an assertion about how fast the machine
/// is, and one that always waited the full timeout would put twenty seconds on
/// every CI run for no information.
async fn watch(bot: &mut Bot, patience: Duration, enough: usize) -> Seen {
    let mut seen = Seen::default();
    let deadline = tokio::time::Instant::now() + patience;
    while tokio::time::Instant::now() < deadline && seen.summaries.len() < enough {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, bot.recv()).await {
            Ok(Ok(ServerMessage::ChunkData { pos, .. })) => seen.chunks.push(pos),
            Ok(Ok(ServerMessage::ChunkSummary { pos, blob })) => {
                let summary = tiamot_core::lod::codec::decode(&blob)
                    .expect("the server sent a horizon that would not decode");
                seen.summaries.push((pos, summary.level()));
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    seen
}

#[test]
fn land_past_the_detail_radius_arrives_as_a_summary() {
    // **The claim the whole task is about.** Before this, the world ended at
    // the view distance; now it carries on, at a resolution that costs a few
    // dozen bytes a chunk instead of a few thousand.
    let view = ViewDistance::MINIMUM;
    let server = start("horizon-arrives", view);
    let spawn = BlockPos::new(0, 1, 0).chunk();

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let seen = watch(&mut alice, Duration::from_secs(20), 32).await;

        assert!(
            !seen.summaries.is_empty(),
            "nothing past the detail radius was ever sent"
        );

        // Every summary is outside the detail radius, and every chunk is
        // inside it. A position in both sets would be a client drawing the
        // same chunk twice, once coarse and once fine.
        for (pos, level) in &seen.summaries {
            let distance = pos
                .x
                .abs_diff(spawn.x)
                .max(pos.y.abs_diff(spawn.y))
                .max(pos.z.abs_diff(spawn.z));
            assert!(
                distance > u32::from(view.horizontal) || distance > u32::from(view.vertical),
                "{pos:?} was summarised but is inside the detail radius"
            );
            assert!(
                (tiamot_core::lod::FINEST..=tiamot_core::lod::COARSEST).contains(level),
                "{pos:?} came at level {level}, which is not a level"
            );
            assert!(
                !seen.chunks.contains(pos),
                "{pos:?} was sent both in full and as a summary"
            );
        }
    });
}

#[test]
fn a_horizon_costs_a_fraction_of_what_the_terrain_costs() {
    // Not a benchmark — a shape check. The point of a summary is that it is
    // small; one that is not is a horizon that costs more bandwidth than the
    // world in front of it, and there would be no reason to send it.
    let view = ViewDistance::MINIMUM;
    let server = start("horizon-is-small", view);

    block_on(async {
        let mut alice = join(&server, "Alice").await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut summary_bytes = 0usize;
        let mut summaries = 0usize;
        while tokio::time::Instant::now() < deadline && summaries < 32 {
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, alice.recv()).await {
                Ok(Ok(ServerMessage::ChunkSummary { blob, .. })) => {
                    summary_bytes += blob.len();
                    summaries += 1;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(summaries > 0, "no horizon arrived");
        let average = summary_bytes / summaries;
        assert!(
            average < 1024,
            "a summary averaged {average} bytes over {summaries} of them; the horizon \
             is meant to be cheap, and a level-1 summary uncompressed is 8 KiB"
        );
    });
}
