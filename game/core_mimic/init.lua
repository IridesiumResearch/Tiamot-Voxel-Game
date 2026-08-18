-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The Mimic: a blank-white copy of the first player ever to join this world,
-- which follows them at a distance and repeats what they did two seconds ago.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It is the
-- mod API's acceptance test: every mechanism Task 12 added is exercised here
-- through the public surface, and nothing in the engine knows this mod exists.
-- The check is a grep — the string "mimic" appears nowhere outside `game/`.
--
-- What it uses, and therefore what it proves works:
--
--   game.register_on_player_join   who arrived, by UUID
--   game.storage                   what this world has already seen
--   game.spawn_entity              with a live-resolving player nametag
--   game.entities_in_radius        finding players, who are entities now
--   game.entity                    reading one, including its owner
--   game.set_entity                driving one through the player's physics
--   game.line_of_sight             whether it can actually see you
--   game.register_on_entity_step   its own behaviour, per tick
--   game.register_on_punch         being hit

-- Twenty ticks a second (the engine's fixed rate), so these are seconds.
local DELAY_TICKS = 40 -- how far behind it walks: two seconds
local NOTICE_BLOCKS = 32 -- how far it can notice you, per the task
local KEEP_BLOCKS = 2.5 -- how close it will get before it stops
local FLEE_TICKS = 60 -- three seconds of running away after a punch
local WANDER_TICKS = 50 -- how often it picks a new idle direction
local WANDER_BLOCKS = 6 -- how far from home it will drift
local TRAIL_SLACK = 20 -- breadcrumbs kept past the delay, for jitter
local KNOCKBACK = 1.6 -- cells per tick a punch shoves it, about three blocks a second

-- Animation tags, as the engine numbers them. Named because `anim = 1` in the
-- middle of a state machine says nothing about what it means.
local ANIM_IDLE = 0
local ANIM_WALK = 1
local ANIM_RUN = 2

-- The four ways it paces while idle, in order.
local WANDER_FACES = { { 1, 0 }, { 0, 1 }, { -1, 0 }, { 0, -1 } }

-- Everything below is in-memory only. What has to survive a restart is in
-- `game.storage` (the imprint, and where the mimic belongs) or on the entity
-- itself (its position, which persists with its chunk) — a mod's Lua locals do
-- not, and pretending otherwise is how state quietly stops persisting.
local now = 0
local mimic = nil -- entity id, once we have found or made it
local imprint = nil -- the imprinted player's UUID, as hex
local trail = {} -- [tick] = { x, y, z, anim }, the player's own path
local flee_until = 0
local last_seen = nil -- where the imprint was, when it last was

--- Where the mimic belongs, from storage. Three numbers rather than a table,
--- because storage holds strings, numbers and flags and NOT tables — a table
--- would need a serialisation format baked into the mod API for ever.
local function home()
    local x = game.storage.get("home_x")
    if x == nil then
        return nil
    end
    return { x = x, y = game.storage.get("home_y"), z = game.storage.get("home_z") }
end

local function set_home(pos)
    game.storage.set("home_x", pos.x)
    game.storage.set("home_y", pos.y)
    game.storage.set("home_z", pos.z)
end

--- The horizontal distance between two world points, in blocks.
local function distance(a, b)
    local dx, dz = a.x - b.x, a.z - b.z
    return math.sqrt(dx * dx + dz * dz)
end

--- The imprinted player's entity, if they are online and near `centre`.
---
--- Keyed on the UUID and never the name (charter rule 13): a later player
--- claiming the imprinted player's name must not inherit the mimic, which on a
--- single server cannot happen and which a mod should still not depend on.
local function imprinted_near(centre, radius)
    for _, id in ipairs(game.entities_in_radius(centre, radius, "engine:player")) do
        local player = game.entity(id)
        if player ~= nil and player.owner == imprint then
            return id, player
        end
    end
    return nil, nil
end

--- Points the mimic at a world position, or stops it if it is close enough.
---
--- `drive` and not `pos`: `drive` is what the entity is TRYING to do and the
--- engine's own physics does the rest, so the mimic walks, collides, steps up a
--- lip and swims exactly as a player does. Setting `pos` would teleport it,
--- and a mob that slides through walls is not eerie, it is broken.
local function steer(id, self_pos, target, gait, anim)
    local dx, dz = target.x - self_pos.x, target.z - self_pos.z
    if distance(self_pos, target) < 0.35 then
        -- **Standing still is ANIM_IDLE**, and it has to be said explicitly:
        -- `anim or ANIM_IDLE` looks like it does this and does not. Lua's `or`
        -- falls through on `false` and `nil` only, and every animation tag —
        -- including zero — is truthy, so that expression is just `anim`. A
        -- mimic that stopped went on walking on the spot.
        game.set_entity(id, { drive = { walk = { x = 0, z = 0 } }, anim = ANIM_IDLE })
        return
    end
    -- Magnitude is ignored — the gait decides how fast anything moves — so this
    -- is a direction and nothing more.
    game.set_entity(id, {
        drive = { walk = { x = dx, z = dz }, gait = gait or "walk" },
        -- **Facing follows travel.** The engine has no opinion about where a
        -- mob looks — `Transform.yaw` is presentation and nothing in the
        -- physics reads it — so pointing the body is this mod's job, and a
        -- mimic that never turned faced north for ever.
        --
        -- `atan` is a transcendental, which charter rule 4 bans from the
        -- SIMULATION. This is not simulation: a heading changes nothing about
        -- where anything is, and the engine sends it quantised to a byte.
        yaw = math.atan(dx, dz),
        anim = anim or ANIM_WALK,
    })
end

-- **The imprint, and it is taken once for the life of the world.**
--
-- The engine does not say whether this is somebody's first-ever join, and it
-- should not: "first" is a rule this mod invents. `game.storage` is where the
-- answer lives, so the fact survives a restart and cannot be re-taken.
game.register_on_player_join(function(event)
    if game.storage.get("imprint") == nil then
        game.storage.set("imprint", event.player)
        game.log("the mimic has imprinted on " .. event.name)
    end
end)

-- **Being hit.** The engine has no damage model and this mob deals none; a
-- punch is only a reason to be somewhere else for a moment.
--
-- The shove is this mod's decision too, and it has to be: the engine reports
-- who hit what and stops (charter rule 1), so a mob that took a punch without
-- moving is a mob whose mod never told it to. Velocity rather than position —
-- `pos` teleports without sweeping and would put it through a wall.
game.register_on_punch(function(event)
    if mimic == nil or event.target ~= mimic then
        return
    end
    flee_until = now + FLEE_TICKS

    local self = game.entity(mimic)
    if self == nil then
        return
    end
    -- Away from whoever swung. Found by matching the attacker's UUID against
    -- the owner of a nearby player's body, because the event says who hit and
    -- the entity store says where they are.
    for _, id in ipairs(game.entities_in_radius(self.pos, 8, "engine:player")) do
        local attacker = game.entity(id)
        if attacker ~= nil and attacker.owner == event.attacker then
            local dx, dz = self.pos.x - attacker.pos.x, self.pos.z - attacker.pos.z
            local length = math.sqrt(dx * dx + dz * dz)
            if length > 0.001 then
                -- Cells per tick, which is what velocity is in. Sideways and a
                -- little up, so the shove reads as a hit rather than as a slide.
                game.set_entity(mimic, {
                    velocity = {
                        x = dx / length * KNOCKBACK,
                        y = KNOCKBACK * 0.6,
                        z = dz / length * KNOCKBACK,
                    },
                    anim = ANIM_RUN,
                })
            end
            return
        end
    end
end)

game.register_on_tick(function()
    now = now + 1
    imprint = imprint or game.storage.get("imprint")
    if imprint == nil then
        -- Nobody has ever joined this world. There is nothing to be a copy of.
        return
    end

    local where = home()

    -- Find the imprinted player. Around home if we have one, and otherwise
    -- anywhere at all — which happens exactly once, on the tick after the very
    -- first join, when there is no home yet because there is no mimic yet.
    local player_id, player = imprinted_near(where or { x = 0, y = 0, z = 0 }, where and 128 or 1e6)

    if player ~= nil then
        last_seen = player.pos
        -- The breadcrumb trail: where they were and what they were doing, one
        -- entry per tick. Old entries are dropped rather than accumulated —
        -- a table that only ever grows is a memory leak with a slow fuse.
        trail[now] = { x = player.pos.x, y = player.pos.y, z = player.pos.z, anim = player.anim }
        trail[now - DELAY_TICKS - TRAIL_SLACK] = nil
    end

    if mimic == nil and where ~= nil then
        -- A world that has been here before. The mimic persists with its chunk,
        -- so it is already out there — as long as that chunk is loaded.
        local found = game.entities_in_radius(where, 64, game.mod_id)
        mimic = found[1]
    end

    if mimic == nil and player ~= nil and game.storage.get("spawned") ~= true then
        -- The first mimic this world has ever had, made where the imprinted
        -- player is standing.
        mimic = game.spawn_entity({
            pos = { x = player.pos.x + 3, y = player.pos.y, z = player.pos.z },
            -- The engine's own rig. Untextured, which is matte white — there is
            -- no skin system and this mob does not want one.
            model = "engine:humanoid",
            -- The imprinted player's CURRENT name, resolved by the engine every
            -- time it is drawn. Storing the name instead would leave a stale
            -- copy over its head the moment they renamed themselves.
            nametag_player = imprint,
            collider = { width = 1.8, height = 5.4 },
        })
        if mimic ~= nil then
            set_home(player.pos)
            game.storage.set("spawned", true)
            game.log("the mimic is here")
        end
    end

    -- Keep the id we hold honest. An entity whose chunk unloaded is frozen, not
    -- gone, and `game.entity` answering nil is how a mod finds that out.
    if mimic ~= nil and game.entity(mimic) == nil then
        mimic = nil
    end
end)

game.register_on_entity_step(function(id)
    if id ~= mimic then
        return
    end
    local self = game.entity(id)
    if self == nil then
        return
    end

    local _, player = imprinted_near(self.pos, NOTICE_BLOCKS)

    -- **Punched.** Away from whoever is nearest, briefly, then back to normal.
    if now < flee_until then
        local from = (player and player.pos) or last_seen or self.pos
        steer(id, self.pos, {
            x = self.pos.x + (self.pos.x - from.x),
            y = self.pos.y,
            z = self.pos.z + (self.pos.z - from.z),
        }, "sprint", ANIM_RUN)
        return
    end

    -- **Following.** Near enough, and it can actually see you: a wall between
    -- the two is a wall, and a mob that tracked you through one would be a
    -- cheat rather than a mimic.
    if player ~= nil then
        local eye = { x = self.pos.x, y = self.pos.y + 1.5, z = self.pos.z }
        local theirs = { x = player.pos.x, y = player.pos.y + 1.5, z = player.pos.z }
        if game.line_of_sight(eye, theirs) then
            local past = trail[now - DELAY_TICKS]
            if past ~= nil then
                if distance(self.pos, player.pos) < KEEP_BLOCKS then
                    -- Close enough. It stops and copies what you were doing,
                    -- which is the part that reads as wrong.
                    game.set_entity(id, { drive = { walk = { x = 0, z = 0 } }, anim = past.anim })
                else
                    steer(id, self.pos, past, "walk", past.anim)
                end
                return
            end
        end
    end

    -- **Idling.** Near where they were last seen if it ever saw them, and near
    -- home otherwise. It picks a new direction every couple of seconds and
    -- walks that way, which is as much wandering as this needs.
    local anchor = last_seen or home() or self.pos

    -- Paced off the tick number rather than a random draw. Two servers running
    -- the same world must wander it the same way (charter rule 4), and Lua's
    -- own `math.random` is seeded per process — `game.rng_stream` is the
    -- deterministic one, but it is shaped for worldgen and wants a chunk. A
    -- counter is the honest answer for something this small.
    local pick = (now // WANDER_TICKS) % 4 + 1
    local face = WANDER_FACES[pick]
    local wander_to = {
        x = anchor.x + face[1] * WANDER_BLOCKS,
        y = anchor.y,
        z = anchor.z + face[2] * WANDER_BLOCKS,
    }
    steer(id, self.pos, wander_to, "walk", ANIM_WALK)
end)
