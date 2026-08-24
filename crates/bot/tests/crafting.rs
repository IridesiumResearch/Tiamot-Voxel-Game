// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Crafting is a mod, and this is the mechanism it needs from the engine.
//!
//! Charter rule 1: the engine holds mechanisms and content is Lua. A recipe is
//! content — "twenty-seven units of stone becomes five stairs" is a decision
//! nobody but a mod should be making. What the engine has to provide is the
//! ability to spend material and hand something back, and to conserve units
//! across the pair (charter rule 5).
//!
//! Until `game.give` and `game.take` existed only digging could credit an
//! inventory and only placing could debit one, so a SHAPED stack — which only
//! crafting produces — had no way to come into being at all. This drives the
//! whole path against a real server: a mod hands a fresh player loose stone,
//! turns some of it into a cut, and the client is told about both.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::{ServerMessage, StackDef};
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(10);

/// The cut the mod makes: five of a block's twenty-seven cells.
///
/// Cells 0, 1, 2, 3 and 4 — `index = x + 3*y + 9*z`, so the bottom-front row
/// and two above it. What it looks like does not matter here; that it survives
/// the round trip unchanged does.
const STAIR: u32 = 0b1_1111;

/// How many units one stair costs, which is how many cells it has.
const PER_STAIR: u32 = 5;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-crafting-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that gives a joining player stone and turns it into stairs on request.
///
/// **Deliberately arithmetic that does not come out even.** Twenty-seven units
/// buys five five-cell stairs with two units left over, and the mod hands those
/// back rather than pocketing them — so the test can assert the total is
/// unchanged, which is the property that matters.
fn write_bench(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("bench");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"bench\"\nname = \"Bench\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
game.register_block{ id = "stone" }
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), game.get_block_id("bench:stone"))
end)

local STAIR = 0x1F

-- A starting kit, handed to somebody who has never dug anything. Two whole
-- blocks, by `count`, which is the spelling a recipe is written in.
game.register_on_player_join(function(event)
    game.give(event.player, { material = "bench:stone", count = 2 })
end)

game.register_on_chat(function(event)
    if event.text ~= "craft" then
        return
    end
    local spent = game.take(event.player, { material = "bench:stone", units = 27 })
    if spent < 27 then
        -- Could not afford it: put back exactly what was taken.
        if spent > 0 then
            game.give(event.player, { material = "bench:stone", units = spent })
        end
        return false
    end
    game.give(event.player, { material = "bench:stone", shape = STAIR, count = 5 })
    game.give(event.player, { material = "bench:stone", units = 2 })
    return false
end)
"#,
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
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
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

/// Reads until an inventory the test is happy with arrives, and returns it.
///
/// Driven to a CONDITION rather than a frame count: what is being waited for is
/// a tick that has run the mod's hook, and how many messages that takes is the
/// machine's business.
async fn inventory_where(
    bot: &mut Bot,
    happy: impl Fn(&[StackDef]) -> bool,
) -> Option<Vec<StackDef>> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    let mut latest: Option<Vec<StackDef>> = None;
    loop {
        for message in bot.received() {
            if let ServerMessage::InventoryUpdate { stacks } = message {
                latest = Some(stacks);
            }
        }
        if let Some(stacks) = &latest
            && happy(stacks)
        {
            return latest;
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        if bot.recv().await.is_err() {
            return None;
        }
    }
}

/// Every unit a player is carrying, whatever it is cut into.
fn total(stacks: &[StackDef]) -> u32 {
    stacks.iter().map(|stack| stack.units).sum()
}

/// The units held as stairs, which is what a craft adds to.
fn stairs(stacks: &[StackDef]) -> u32 {
    stacks
        .iter()
        .filter(|stack| stack.shape == STAIR)
        .map(|stack| stack.units)
        .sum()
}

#[test]
fn a_mod_hands_a_fresh_player_material_it_never_dug() {
    // **The half that used to be impossible.** An inventory record is created
    // the first time a player is CREDITED, so somebody who has just joined has
    // none — and the first version of `give` refused them for it, which would
    // have made a starting kit unexpressible.
    let server = start("kit", write_bench("kit"));
    block_on(async {
        let mut bot = join(&server, "Newcomer").await;
        let stacks = inventory_where(&mut bot, |stacks| total(stacks) >= 54)
            .await
            .expect("the mod's starting kit never arrived");
        assert_eq!(total(&stacks), 54, "two blocks is fifty-four units");
        assert!(
            stacks.iter().all(|stack| stack.shape == 0),
            "a kit of loose material must not arrive cut: {stacks:?}"
        );
        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn spending_loose_material_on_a_cut_conserves_every_unit() {
    // The whole claim, end to end: a mod takes 27 units, gives back five
    // five-cell stairs and the two units that would not fit, and the player has
    // exactly as much as they started with — in two entries rather than one,
    // because a cut and the rubble it came from do not stack.
    let server = start("craft", write_bench("craft"));
    block_on(async {
        let mut bot = join(&server, "Mason").await;
        let before = inventory_where(&mut bot, |stacks| total(stacks) >= 54)
            .await
            .expect("the starting kit never arrived");

        bot.chat("craft").await.expect("send");

        let after = inventory_where(&mut bot, |stacks| {
            stacks.iter().any(|stack| stack.shape == STAIR)
        })
        .await
        .expect("the cut never reached the client");

        let stairs = after
            .iter()
            .find(|stack| stack.shape == STAIR)
            .expect("checked above");
        assert_eq!(
            stairs.units,
            5 * PER_STAIR,
            "five stairs of five cells is twenty-five units"
        );
        assert_eq!(
            total(&after),
            total(&before),
            "crafting invented or destroyed units: {before:?} -> {after:?}"
        );
        assert!(
            after.iter().any(|stack| stack.shape == 0),
            "the loose remainder was not handed back: {after:?}"
        );

        bot.disconnect().await;
    });
    server.stop();
}

#[test]
fn a_recipe_that_cannot_be_afforded_puts_back_exactly_what_it_took() {
    // `game.take` is partial by design — it reports how many units it got, so a
    // mod that cannot finish can undo itself rather than pocket the difference.
    //
    // The kit affords exactly two crafts: 54 units in, 27 spent each time and 2
    // handed back, leaving 4 loose. The THIRD is the one under test, and it is
    // the one that finds four units where it wanted twenty-seven.
    let server = start("short", write_bench("short"));
    block_on(async {
        let mut bot = join(&server, "Mason").await;
        inventory_where(&mut bot, |stacks| total(stacks) >= 54)
            .await
            .expect("the starting kit never arrived");

        for made in 1..=2u32 {
            bot.chat("craft").await.expect("send");
            let after = inventory_where(&mut bot, |stacks| stairs(stacks) == made * 5 * PER_STAIR)
                .await
                .unwrap_or_else(|| panic!("craft {made} never landed"));
            assert_eq!(total(&after), 54, "craft {made} did not conserve units");
        }

        // And the one that cannot be paid for. Nothing should change, so this
        // waits for an ABSENCE: drain the connection for a fixed window and
        // read whatever the inventory ended up as.
        bot.chat("craft").await.expect("send");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if bot.recv().await.is_err() {
                break;
            }
        }
        let mut after = Vec::new();
        for message in bot.received() {
            if let ServerMessage::InventoryUpdate { stacks } = message {
                after = stacks;
            }
        }
        if !after.is_empty() {
            assert_eq!(
                total(&after),
                54,
                "the refused recipe kept the four units it could not spend: {after:?}"
            );
            assert_eq!(
                stairs(&after),
                2 * 5 * PER_STAIR,
                "a recipe that could not be afforded made something anyway: {after:?}"
            );
        }

        bot.disconnect().await;
    });
    server.stop();
}
