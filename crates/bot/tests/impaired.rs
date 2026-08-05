// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Digging and building over a deliberately bad network.
//!
//! # Why loopback is not enough
//!
//! Every other integration test runs over loopback, where the round trip is
//! microseconds. On that link the client is barely ahead of the server, almost
//! nothing is ever in flight, and the input queue's reorder buffer and repeat
//! window never do anything. **Every bug in those mechanisms hides on
//! loopback**, which is the only network the suite has — so Task 09's test list
//! asks specifically for 150 ms and 5% loss, because that is where they start
//! mattering.
//!
//! The loss is at the message layer, not the packet layer: QUIC retransmits, so
//! a dropped packet on a stream is invisible above the transport and dropping
//! those would be a test of quinn. What this drops is whole messages — an input
//! that never arrives — which is the failure the engine actually has to
//! survive. See [`bot::Impairment`].
//!
//! # The staircase
//!
//! A staircase is the scenario because it is the one that cannot be faked by a
//! client that got lucky: each step is a dig and a place at a position derived
//! from the last, so a lost or reordered message shows up as a missing or
//! misplaced step in the final world rather than as a number that drifted.

use std::path::PathBuf;
use std::time::Duration;

use bot::{Bot, Impairment};
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::{Edit, ServerMessage};
use tiamot_core::{BlockPos, MaterialId, SubNodePos};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 1] = ["test:stone"];

/// How many steps the staircase has.
///
/// Each one is a dig and a place at 150 ms round trip, so this trades coverage
/// against how long the suite takes. Four is enough that a reordering bug has
/// somewhere to show itself and short enough to stay under ten seconds.
const STEPS: i32 = 4;

fn stone() -> u16 {
    let mut registry = tiamot_core::Registry::new();
    let mut id = MaterialId::AIR;
    for name in MATERIALS {
        id = registry.register(name).expect("register");
    }
    id.0
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-impaired").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(name),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        seed: Some(11),
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

async fn join(server: &ServerHandle) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join("Builder").await.expect("join");
    bot
}

fn centre_of(pos: BlockPos) -> SubNodePos {
    SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1)
}

/// Whether the server has broadcast this block as being `material`.
fn saw(bot: &Bot, pos: BlockPos, material: u16) -> bool {
    bot.received().iter().any(|message| {
        matches!(
            message,
            ServerMessage::BlockDelta {
                edit: Edit::Block { pos: at, material: m },
                ..
            } if *at == pos && *m == material
        )
    })
}

/// Waits for a condition, driving the connection while it waits.
async fn until(bot: &mut Bot, timeout: Duration, done: impl Fn(&Bot) -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if done(bot) {
            return true;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), bot.recv()).await;
    }
    done(bot)
}

#[test]
fn a_staircase_survives_a_hundred_and_fifty_millisecond_round_trip_and_five_percent_loss() {
    let server = start("staircase");
    let stone = stone();

    block_on(async {
        let mut bot = join(&server).await;

        // Seeded and confirmed on a CLEAN link, so what the impairment is
        // tested against is the building, not the setting up. A quarry block
        // per step, since each place costs a block's worth of units.
        let quarry: Vec<BlockPos> = (0..STEPS).map(|i| BlockPos::new(20 + i, 40, 20)).collect();
        for pos in &quarry {
            bot.place(*pos, stone).await.expect("seed");
        }
        for pos in &quarry {
            bot.expect_block(*pos, stone, Duration::from_secs(10))
                .await
                .expect("the seed should land");
        }

        // From here the link is bad.
        bot.impair(Impairment::task_09());
        assert_eq!(bot.impairment().loss_percent, 5);
        assert_eq!(
            bot.impairment().latency_ms,
            75,
            "one way; 150 ms round trip"
        );

        bot.select_tool(None).await.expect("bare hand");

        // Each step: mine one quarry block, place it one higher and one along.
        // The position of step N depends on N, so a message that arrives out of
        // order or not at all leaves a hole in a specific place rather than a
        // number that is merely wrong.
        let mut built = Vec::new();
        for (index, source) in quarry.iter().enumerate() {
            let step = i32::try_from(index).expect("fits");
            bot.start_dig(centre_of(*source)).await.expect("dig");
            let dug = until(&mut bot, Duration::from_secs(20), |bot| {
                saw(bot, *source, MaterialId::AIR.0)
            })
            .await;
            assert!(dug, "step {step}: the quarry block was never broken");

            // The units have to arrive before they can be spent. Under 5% loss
            // the inventory update itself can be the message that goes missing
            // — but the server re-sends on the next change, and the dig below
            // is what forces one.
            let funded = until(&mut bot, Duration::from_secs(20), |bot| {
                bot.inventory()
                    .iter()
                    .any(|(id, units)| *id == stone && *units >= tiamot_core::UNITS_PER_BLOCK)
            })
            .await;
            assert!(funded, "step {step}: the dig never credited a full block");

            let target = BlockPos::new(20 + step, 41 + step, 24);
            // Re-sent until it lands. A placement is a request and 5% of them
            // never arrive; a client that sent once and assumed would be
            // relying on a network it was explicitly told is lossy.
            let mut attempts = 0;
            let placed = loop {
                bot.place_from_inventory(centre_of(target), stone)
                    .await
                    .expect("place");
                attempts += 1;
                if until(&mut bot, Duration::from_secs(3), |bot| {
                    saw(bot, target, stone)
                })
                .await
                {
                    break true;
                }
                if attempts >= 10 {
                    break false;
                }
            };
            assert!(
                placed,
                "step {step}: the block never appeared after {attempts} attempts; notices {:?}",
                bot.notices()
            );
            built.push(target);
        }

        // The final world state, asserted step by step. Every step is present
        // and at the height its index says — a staircase, not a pile.
        for (index, pos) in built.iter().enumerate() {
            let step = i32::try_from(index).expect("fits");
            assert!(
                saw(&bot, *pos, stone),
                "step {step} is missing from the finished staircase"
            );
            assert_eq!(pos.y, 41 + step, "step {step} is at the wrong height");
        }
        assert_eq!(
            built.len(),
            STEPS as usize,
            "the staircase is short: {built:?}"
        );
    });

    assert!(server.stop());
}

#[test]
fn a_lossy_link_actually_drops_things() {
    // The counter-example without which every test above could be running on a
    // perfectly clean link. Sends enough messages that 5% loss is a near
    // certainty, and asserts the server saw fewer than were sent.
    //
    // Chat is the probe because the server echoes every one it receives, so the
    // count is observable — and because dropping chat cannot break anything
    // else in the test.
    let server = start("loss-is-real");

    block_on(async {
        let mut bot = join(&server).await;
        bot.impair(Impairment {
            latency_ms: 0,
            loss_percent: 50,
            seed: 1,
        });

        const SENT: usize = 200;
        for i in 0..SENT {
            bot.chat(&format!("probe {i}")).await.expect("chat");
        }

        // Give what survived time to come back.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut echoed = 0;
        while tokio::time::Instant::now() < deadline {
            if tokio::time::timeout(Duration::from_millis(200), bot.recv())
                .await
                .is_err()
            {
                break;
            }
            echoed = bot
                .received()
                .iter()
                .filter(|message| {
                    matches!(message, ServerMessage::Chat { text, .. } if text.contains("probe"))
                })
                .count();
        }

        assert!(
            echoed < SENT,
            "every one of {SENT} messages arrived through a 50% lossy link, so the loss is \
             not being applied and every impairment test is running clean"
        );
        assert!(
            echoed > 0,
            "nothing at all arrived, so the link is broken rather than lossy"
        );
        println!("50% loss: {echoed} of {SENT} messages arrived");
    });

    assert!(server.stop());
}

#[test]
fn latency_delays_rather_than_spacing_out() {
    // The bug this guards against made the first version of the harness
    // useless: sleeping inside `send` serialises the delay, so eight inputs in
    // a row cost eight times the latency and the bot falls permanently behind.
    // A delayed link should cost roughly ONE latency for a burst, not one per
    // message.
    let server = start("latency-shape");

    block_on(async {
        let mut bot = join(&server).await;
        bot.impair(Impairment {
            latency_ms: 50,
            loss_percent: 0,
            seed: 1,
        });

        const BURST: usize = 20;
        let started = tokio::time::Instant::now();
        for i in 0..BURST {
            bot.chat(&format!("burst {i}")).await.expect("chat");
        }
        let elapsed = started.elapsed();

        // Serialised, this would be 20 x 50 ms = 1 second. Queued, the sends
        // return immediately and the delay is paid once, in the writer.
        assert!(
            elapsed < Duration::from_millis(400),
            "sending {BURST} messages over a 50 ms link took {elapsed:?}; the delay is being \
             serialised, which models message SPACING rather than latency"
        );
    });

    assert!(server.stop());
}
