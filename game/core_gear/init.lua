-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Reference items, dropping and worn slots.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It exists
-- to prove three engine mechanisms work through the public mod API, and it is
-- deliberately the smallest thing that does:
--
--   1. `game.register_item` — a thing a player can CARRY and cannot build
--      with. Everything else about it is a material, which is the point: it
--      stacks, it draws from the atlas, it sits in a slot, and the only
--      difference is that placing it is refused.
--   2. `game.spawn_entity{ item = ... }` — an entity that IS a stack, which is
--      what a dropped item is. The engine draws it and has no other opinion:
--      how long it lasts and who may pick it up are decided here, in Lua.
--   3. `game.register_view` — somewhere other than the backpack for a stack to
--      sit. What the slots MEAN is this mod's business and nothing the engine
--      knows about.

-- A thing you can hold and cannot place.
--
-- No hardness, no drops, no light: it is never in the world, so there is
-- nothing for any of them to mean, and the engine refuses them rather than
-- ignoring them.
game.register_item{
    id = "sword",
    name = "Wooden Sword",
    description = "Proof that a carryable thing need not be a block.",
    -- **Not optional in practice.** An item with no texture reaches a client
    -- as a material with no tile, and what a player sees is the
    -- missing-texture chequer — reported from the window as a pink and black
    -- cube. The engine cannot invent a picture of a sword, so a mod that
    -- registers one without a texture has registered something nobody can
    -- look at.
    texture = "textures/sword.png",
}

-- Somewhere to wear things.
--
-- Four slots, fixed. The engine gives it a name and a size and moves stacks
-- between it and anywhere else; that slot one is a helmet and slot four is
-- boots is a decision written here and nowhere else.
game.register_view{ id = "worn", slots = 4 }

-- The key that opens the worn slots.
--
-- **Checked against what the engine already binds**, which a mod cannot ask
-- about: `default_key` is a SUGGESTION and the engine owns bindings (charter
-- rule 11), so a mod suggesting one already taken produces two actions on one
-- key and a player wondering why their tool cycles when they open a screen.
-- The first draft of this suggested `KeyR`, which is `engine:next_tool`.
game.register_action{
    id = "gear",
    default_key = "KeyZ",
    description = "Open the worn slots",
}

-- The key that drops what you are holding.
--
-- The engine owns the binding and this mod owns the action (charter rule 11):
-- a suggested default, and a player who wants it elsewhere moves it on the
-- controls screen without this mod knowing.
game.register_action{
    id = "drop",
    default_key = "KeyQ",
    description = "Drop what you are holding",
}

--- How far above the feet a dropped stack appears, in blocks.
local LIFT = 1.2
--- How far in front of the body it starts, in blocks.
local AHEAD = 0.8
--- How hard it is thrown, in cells per tick.
---
--- Cells, not blocks: an entity's velocity is in the engine's own units, and
--- three cells is one block. A toss rather than a throw — far enough to clear
--- your own feet, close enough to pick up again without a walk.
local THROW = 0.5
--- The arc on top of the aim, in cells per tick.
---
--- A stack thrown dead level still wants to rise a little, or it reads as slid
--- rather than thrown.
local LOFT = 0.25
--- How close a player has to be to pick something up, in blocks.
local REACH = 1.5
--- How long a dropped stack ignores the player who dropped it, in ticks.
---
--- **Three seconds, not one.** At one second a thrown stack was picked straight
--- back up before it had finished landing — reported from the window as not
--- being able to get rid of it. Long enough to walk away from, short enough
--- that a stack dropped by accident is not a trip back across the map.
local SETTLE = 60

--- Dropped stacks this mod is looking after: entity id -> { owner, ticks }.
local dropped = {}

--- Where a player is, or nil if they are not connected.
local function body_of(uuid)
    local id = game.player_entity(uuid)
    if id == nil then
        return nil
    end
    return game.entity(id)
end

--- Throws a stack out in front of `body`.
---
--- **`body.facing` and not `math.sin(body.yaw)`**, and the difference is
--- charter rule 4. Turning an angle into a direction in Lua calls the
--- platform's libm, so two servers running this mod would put the same thrown
--- stack in slightly different places — and that difference is then persisted
--- world state. The engine works it out from a committed table instead
--- (`detgen::trig`), so the answer is the same everywhere and this mod never
--- has to know why.
---
--- The first version of this dropped straight down for want of that call, and
--- it was reported from the window as not really being thrown.
local function throw(body, stack)
    local id = game.spawn_entity{
        pos = {
            x = body.pos.x + body.facing.x * AHEAD,
            -- From the head rather than from the feet, and moved with the aim:
            -- a stack thrown while looking up should leave from above the eye,
            -- not from the ground.
            y = body.pos.y + LIFT + body.facing.y * AHEAD,
            z = body.pos.z + body.facing.z * AHEAD,
        },
        item = stack,
        -- A small box, so gravity puts it on the floor. An entity with no
        -- collider is a marker and would hang in the air.
        collider = { width = 0.5, height = 0.5 },
    }
    if id ~= nil then
        -- **Along the whole of `facing`, pitch included.** Look up and it goes
        -- further; look at your feet and it lands on them. Reported from the
        -- window as wanting it spat out rather than dropped, and the vertical
        -- part is what makes that read as a throw rather than a nudge.
        --
        -- `LOFT` is on top of the aim: a stack thrown dead level still wants to
        -- rise a little, or it reads as slid.
        game.set_entity(id, {
            velocity = {
                x = body.facing.x * THROW,
                y = body.facing.y * THROW + LOFT,
                z = body.facing.z * THROW,
            },
        })
    end
    return id
end

--- Whose screen is open, so the key toggles rather than reopening.
local open = {}

--- The worn slots, beside the backpack so a stack can be dragged between them.
---
--- **The engine has no idea what any of this means.** It draws four boxes and
--- moves stacks between them; that slot one is a helmet is a decision nothing
--- outside this file knows about, and a mod that wanted six slots or a hat and
--- a cape would say so in `register_view` and change nothing else.
local function screen()
    return {
        type = "container", direction = "column", gap = 6, padding = 10,
        children = {
            { type = "label", text = "Worn" },
            { type = "item_grid", view = "core_gear:worn", columns = 4, first = 1, count = 4 },
            { type = "spacer", size = 8 },
            { type = "label", text = "Carried" },
            { type = "item_grid", view = "player:main", columns = 9, first = 1, count = 27 },
        },
    }
end

game.register_on_dialog_event(function(event)
    if event.kind == "closed" then
        open[event.player] = nil
    end
end)

game.register_on_action(function(event)
    if event.id == "core_gear:gear" and event.pressed then
        if open[event.player] then
            game.close_dialog{ player = event.player, form = "worn" }
            open[event.player] = nil
        else
            game.show_dialog{ player = event.player, form = "worn", tree = screen() }
            open[event.player] = true
        end
        return
    end

    if event.id ~= "core_gear:drop" or not event.pressed then
        return
    end

    -- What they are POINTING with, not what they own. `game.inventory` would
    -- answer the second question, and dropping the wrong stack is exactly the
    -- kind of thing a player never forgives.
    local holding = game.held(event.player)
    if holding == nil then
        return
    end

    local body = body_of(event.player)
    if body == nil then
        return
    end

    -- **Taken before it is thrown, and only as much as was taken.** `game.take`
    -- reports what it actually got, so a stack that changed between the two
    -- calls cannot be duplicated: the thing that lands is the thing that left.
    local spec = { material = holding.material, shape = holding.shape, units = holding.units }
    local moved = game.take(event.player, spec)
    if moved <= 0 then
        return
    end
    spec.units = moved

    local id = throw(body, spec)
    if id == nil then
        -- No world to drop into. Put it back rather than losing it.
        game.give(event.player, spec)
        return
    end
    dropped[id] = { owner = event.player, ticks = 0 }
end)

game.register_on_tick(function()
    for id, watch in pairs(dropped) do
        local item = game.entity(id)
        if item == nil or item.item == nil then
            -- Gone, by some other hand than this one.
            dropped[id] = nil
        else
            watch.ticks = watch.ticks + 1
            -- Nearest first, so two players reaching at once resolve the same
            -- way on every server.
            for _, near in ipairs(game.entities_in_radius(item.pos, REACH, "engine:player")) do
                local person = game.entity(near)
                if person ~= nil and person.owner ~= nil then
                    local mine = person.owner == watch.owner
                    if not (mine and watch.ticks < SETTLE) then
                        game.give(person.owner, {
                            material = item.item.material,
                            shape = item.item.shape,
                            units = item.item.units,
                        })
                        game.despawn_entity(id)
                        dropped[id] = nil
                        break
                    end
                end
            end
        end
    end
end)

-- Say `gear` in chat and one appears.
--
-- **Asked for rather than handed out on join, and that is fixture hygiene.**
-- The first version gave everybody a sword when they arrived, which quietly
-- changed the starting inventory for every player on the server — and three
-- unrelated tests broke, each of them reasonably assuming a player begins
-- carrying what they dug and nothing else. A fixture that perturbs global
-- state is a fixture that other tests have to know about.
--
-- It is also the chat veto doing its job: returning `false` stops the line
-- being broadcast, so a command is not also a message everybody reads.
game.register_on_chat(function(event)
    if event.text ~= "gear" then
        return
    end
    game.give(event.player, { material = "core_gear:sword", count = 1 })
    return false
end)
