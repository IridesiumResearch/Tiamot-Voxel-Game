// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A pond reaching a second client over a bad link.
//!
//! # The criterion, and what it is really about
//!
//! Task 11: "bot places a source, second bot's keyframe-recovery path exercised
//! under forced loss; final client fluid layer hash matches server."
//!
//! **Every `ChunkFluid` is a keyframe rather than a delta, and that decision is
//! what this tests.** A spreading front changes tens of blocks a tick, so the
//! RLE'd whole layer beats a delta stream exactly where it matters — and loss
//! recovery becomes the normal path instead of rare code that never runs in a
//! test. The claim is therefore that a client which missed messages is corrected
//! by the next one rather than drifting, and the way to check it is to make a
//! client miss messages and then ask whether it agrees with the server.
//!
//! Loss here is at the message layer, not the packet layer: QUIC retransmits, so
//! dropping packets would test quinn. Whole dropped messages are the failure the
//! engine has to survive. See [`bot::Impairment`].
//!
//! # Why the milk is poured by a mod rather than by a bot
//!
//! Pouring through `core_milk` means placing its block, which means holding the
//! units to place — a player's inventory, a tool, and a dig to fill it. All of
//! that is tested elsewhere and none of it is what this is about. A mod pouring
//! on a tick puts a source in the world with the same `game.set_fluid` call the
//! reference mod uses, and leaves this test measuring the wire.

use std::path::PathBuf;
use std::time::Duration;

use bot::{Bot, Impairment};
use tiamot_core::fluid::FluidLayer;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::{BlockPos, ChunkPos};
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 1] = ["test:stone"];

/// Where the mod pours. Near spawn, so it is inside the first chunks a joining
/// bot is interested in, and above the terrain so it stays where it was put.
const POND: BlockPos = BlockPos::new(2, 4, 2);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-fluid-mp").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that registers a fluid, lays a floor, and pours one source onto it.
///
/// The floor is not decoration: it gives the source something to rest on, so
/// what the clients receive is a stable pond rather than a column mid-fall that
/// looks different in every message.
fn write_pourer(root: &std::path::Path) -> PathBuf {
    let mods = root.join("mods");
    let dir = mods.join("pourer");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"pourer\"\nname = \"Pourer\"\nversion = \"0.1.0\"\n\
         license = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            r#"
game.register_block{{ id = "milk", name = "Milk" }}
game.register_block{{ id = "rock", name = "Rock" }}
game.register_fluid{{ id = "milk", material = "milk", flow_range = 7 }}

local elapsed = 0
local floored = false
local poured = false
-- The callback is handed a COUNT OF STEPS, not a tick number.
game.register_on_tick(function(dt_ticks)
    elapsed = elapsed + dt_ticks

    -- The floor first. `set_block` is queued on the seed queue rather than
    -- applied inline, so the chunk is not resident the instant this returns.
    if not floored and elapsed >= 10 then
        floored = true
        game.set_block({{ x = {x}, y = {floor}, z = {z} }}, "pourer:rock")
        return
    end

    if poured or elapsed < 30 then
        return
    end
    poured = true
    game.set_fluid({{ x = {x}, y = {y}, z = {z} }}, {{ fluid = "pourer:milk", source = true }})
    game.log("poured")
end)
"#,
            x = POND.x,
            y = POND.y,
            z = POND.z,
            floor = POND.y - 1,
        ),
    )
    .expect("init.lua");
    mods
}

fn start(name: &str) -> (ServerHandle, PathBuf) {
    let root = scratch(name);
    let mods = write_pourer(&root);
    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: root.clone(),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(11),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start");
    (server, root)
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

/// Waits for a condition, driving the connection while it waits.
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

/// A layer's contents as one number, for comparing two peers' views.
///
/// Folded in [`tiamot_core::coords::LocalBlock`] index order, which is what the
/// layer iterates in, so two equal layers hash equally whatever order their
/// blocks were written in.
fn fingerprint(layer: &FluidLayer) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for value in layer.blocks() {
        hasher.update(&[value.0]);
    }
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 output is 32 bytes"),
    )
}

#[test]
fn a_second_client_recovers_the_pond_over_a_lossy_link() {
    let (server, _root) = start("keyframe-recovery");
    let chunk: ChunkPos = POND.chunk();

    block_on(async {
        // **The first bot is on a clean link and is the control.** Without it,
        // "the second bot saw a pond" is satisfied by a server that never
        // poured one and a test that asserted nothing.
        let mut clean = join(&server, "Clean").await;
        assert!(
            until(&mut clean, Duration::from_secs(15), |bot| {
                bot.fluid_at(POND).is_source()
            })
            .await,
            "the pond never reached a client on a clean link, so there is nothing to recover"
        );

        // **The second bot loses one message in five before it has even
        // joined**, which is far worse than the 5% Task 09 asks for and is the
        // point: a keyframe scheme should not merely survive a bad link, it
        // should be indifferent to one, because every message it drops is
        // replaced rather than missed.
        let mut lossy = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        lossy.impair(Impairment {
            latency_ms: 75,
            loss_percent: 20,
            seed: 0x6d69_6c6b,
        });
        lossy.join("Lossy").await.expect("join");

        assert!(
            until(&mut lossy, Duration::from_secs(20), |bot| {
                bot.fluid_at(POND).is_source()
            })
            .await,
            "a client on a lossy link never recovered the pond — the keyframe that \
             should have replaced what it dropped either never came or did not carry \
             the whole layer"
        );

        // **The layers, not just the one block.** A client that got the source
        // and missed the flow around it would pass the assertion above and be
        // wrong everywhere else.
        let their = lossy.fluid_layer(chunk).expect("a layer arrived");
        let ours = clean.fluid_layer(chunk).expect("a layer arrived");
        assert_eq!(
            fingerprint(&their),
            fingerprint(&ours),
            "the two clients disagree about the chunk's fluid: clean {:?} against lossy {:?}",
            ours.filled(),
            their.filled()
        );

        clean.disconnect().await;
        lossy.disconnect().await;
    });

    server.stop();
}

#[test]
fn a_client_that_joins_after_the_pour_is_told_about_it() {
    // Late joiners are the other half of what keyframes are for. A delta stream
    // would have sent the pour to whoever was connected at the time and left
    // anyone arriving afterwards with a dry chunk — which is a bug nobody sees
    // until a second person joins a server that has been running a while.
    let (server, _root) = start("late-joiner");

    block_on(async {
        let mut first = join(&server, "First").await;
        assert!(
            until(&mut first, Duration::from_secs(15), |bot| {
                bot.fluid_at(POND).is_source()
            })
            .await,
            "the pond never poured"
        );
        first.disconnect().await;

        // Nobody was connected when this one arrived, and the pour happened
        // long before it did.
        let mut late = join(&server, "Late").await;
        assert!(
            until(&mut late, Duration::from_secs(15), |bot| {
                bot.fluid_at(POND).is_source()
            })
            .await,
            "a client joining after the pour was never told about the pond"
        );
        late.disconnect().await;
    });

    server.stop();
}
