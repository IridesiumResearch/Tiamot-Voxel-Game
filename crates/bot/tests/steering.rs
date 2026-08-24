// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A mob walks where it is told, and climbs what is in the way.
//!
//! **Reported from the window: the stalker cannot get out of a hole.** It could
//! not, and neither could anything else: `Intent.jump` had existed since Task
//! 09 and pathfinding had climbed a block since Task 12, but nothing joined
//! them. A mod could ask for a route and could ask a body to jump, and had no
//! way to know WHEN — that needs a look at the block in front of the mob's
//! feet, which is terrain, which a mod cannot read cheaply and should not have
//! to.
//!
//! `game.steer_entity` is the join, and this is it working through a real
//! server: a mob in a hole, told to come to the surface, arriving.

use std::path::PathBuf;
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(25);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-steer-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A mod that digs a hole, drops a mob in it, and steers it out.
///
/// The whole mob is fifteen lines, which is the other half of what was asked
/// for: adding one should not be a project.
fn write_mod(name: &str) -> PathBuf {
    let root = scratch(name);
    let dir = root.join("walker");
    std::fs::create_dir_all(&dir).expect("mod dir");
    std::fs::write(
        dir.join("mod.toml"),
        "id = \"walker\"\nname = \"Walker\"\nversion = \"0.1.0\"\nlicense = \"GPL-3.0-only\"\n",
    )
    .expect("manifest");
    std::fs::write(
        dir.join("init.lua"),
        r#"
local ground = game.register_block{ id = "ground" }
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)
game.register_tool{ id = "hand", brush = "block", speed_multiplier = 1.0, default = true }

-- Where the mob starts, and where it is sent. The pit is one block deep, which
-- is exactly the step a body can climb — and exactly what it could not before.
local PIT = { x = 6, y = -1, z = 0 }
local OUT = { x = 12, y = 0, z = 0 }

local mob = nil
local ticks = 0

game.register_on_tick(function()
    ticks = ticks + 1
    if mob == nil then
        -- Dig the pit, then stand something in it.
        game.set_block(PIT, "engine:air")
        mob = game.spawn_entity{
            pos = { x = PIT.x + 0.5, y = PIT.y, z = PIT.z + 0.5 },
            model = "engine:humanoid",
            collider = { width = 0.6, height = 1.8 },
        }
        return
    end
    -- One call a tick. This is the whole of driving a mob.
    game.steer_entity(mob, OUT)

    -- Report progress by marking the world, which is the only channel a bot
    -- can observe without a "read a mod's variable" message that should not
    -- exist. `mod_edits.rs` established the pattern.
    -- `fill_below_heightmap(0)` puts the surface block at y = -1, so standing
    -- on the surface means feet at y = 0 and standing in the pit means y = -1.
    -- Halfway between is the test for "it got out".
    local self = game.entity(mob)
    if self ~= nil and self.pos.y > -0.4 then
        game.set_block({ x = 0, y = 4, z = 0 }, "walker:ground")
    end
end)
"#,
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
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(write_mod(name)),
        enabled_mods: None,
        seed: Some(31),
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

#[test]
fn a_mob_climbs_out_of_a_one_block_pit_and_walks_to_where_it_was_sent() {
    let server = start("pit");
    block_on(async {
        let mut bot = Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("watcher").await.expect("join");

        let ground = bot
            .material_table()
            .expect("a material table")
            .into_iter()
            .find(|entry| entry.name == "walker:ground")
            .map(|entry| entry.id)
            .expect("the mod registers a ground block");

        // The mark the mod places once the mob is above the pit floor.
        bot.expect_block(tiamot_core::BlockPos::new(0, 4, 0), ground, PATIENCE)
            .await
            .expect("the mob never got out of the pit");

        // And it kept going: the entity ends up east of where it started,
        // which is what "walks to where it was sent" means. Read off the
        // entity stream rather than from the mod, so this is the SERVER's
        // opinion of where the body is.
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let mut furthest = f32::MIN;
        loop {
            for message in bot.received() {
                let entities = match message {
                    ServerMessage::EntitySpawn { entities } => entities
                        .into_iter()
                        .map(|entity| (entity.chunk, entity.local))
                        .collect::<Vec<_>>(),
                    ServerMessage::EntityState { entities, .. } => entities
                        .into_iter()
                        .map(|entity| (entity.chunk, entity.local))
                        .collect(),
                    _ => continue,
                };
                for (chunk, local) in entities {
                    let at = tiamot_core::ent::Transform::at(chunk, local).to_world();
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "a test comparing block-scale positions"
                    )]
                    let x = at[0] as f32;
                    furthest = furthest.max(x);
                }
            }
            if furthest > 8.0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the mob got as far as x = {furthest} and stopped; it was sent to 12"
            );
            bot.recv().await.expect("recv");
        }
    });
    assert!(server.stop());
}
