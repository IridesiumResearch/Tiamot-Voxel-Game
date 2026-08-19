-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Reference tool registrations.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It exists
-- to prove two things through the public mod API:
--
--   1. that digging rules live in a mod at all — delete this directory and
--      nothing in the world can be broken, because the engine has no bare hand
--      of its own;
--   2. that a mod can reach sub-node resolution, which is the entire argument
--      for sub-nodes existing.

-- What a player digs with holding nothing.
--
-- `default = true` is what makes it that. The engine does not know the phrase
-- "bare hand"; it knows that some registered tool is the one to use when the
-- player has chosen none, and a mod says which.
game.register_tool{
    id = "hand",
    name = "Bare Hand",
    -- The whole block containing the targeted cell. What you expect from
    -- punching something.
    brush = "block",
    speed_multiplier = 1.0,
    default = true,
}

-- The reason sub-nodes exist, expressed as a mod.
--
-- `brush = "subnode"` removes exactly the cell under the crosshair — one of
-- the 27 in a block — so a player can carve a shape rather than delete a cube.
-- Nothing in the engine is special-cased for this: the chisel is a mod using
-- an API any other mod can use, which is what charter rule 1 asks for.
--
-- Slower than a bare hand on purpose. Precision costs time, and it also keeps
-- the tool from being strictly better than punching at everything.
game.register_tool{
    id = "chisel",
    name = "Chisel",
    brush = "subnode",
    speed_multiplier = 0.5,
}

game.log("registered core_tools:hand and core_tools:chisel")

-- **A mod-registered control, which is the whole of what charter rule 11 gives
-- a mod.** The mod names an action and suggests a key; the engine owns the
-- binding and a player may move it anywhere. There is deliberately no way to
-- ask which key it ended up on — a mod that could ask would branch on it, and
-- then rebinding would change behaviour rather than just controls.
game.register_action{
    id = "chisel_mode",
    default_key = "KeyC",
    description = "Hold to chisel: swaps to the chisel while held, and back on release",
}

-- What the action DOES, which is a mod's business and not the engine's.
--
-- Held rather than toggled, so both edges are used and a player cannot get
-- stuck in a mode they did not notice entering. What they were holding is
-- remembered rather than assumed: putting back "a bare hand" would quietly
-- take the chisel off somebody who had chosen it with the tool key.
game.register_on_action(function(event)
    if event.id ~= "core_tools:chisel_mode" then
        -- Somebody else's action. Every mod is told about every action so that
        -- one mod can react to another's control, which is worth more than the
        -- filtering it costs each of them.
        return
    end

    local key = "held_before:" .. event.player
    if event.pressed then
        game.storage.set(key, game.get_tool(event.player) or "core_tools:hand")
        game.set_tool(event.player, "core_tools:chisel")
    else
        game.set_tool(event.player, game.storage.get(key) or "core_tools:hand")
        game.storage.set(key, nil)
    end
end)

game.log("registered the core_tools:chisel_mode action")

-- **The noises digging makes.** Charter rule 1: the engine has no sounds of its
-- own, so a world where nobody registers these is a silent world — correct
-- rather than broken.
--
-- Both are the same recipe today. They are fixtures: `game/` is reference
-- implementations and test fixtures, and a real game ships recordings.
game.register_sound{ id = "break", file = "sounds/break.wav", gain = 0.9, pitch_variance = 0.12 }
game.register_sound{ id = "place", file = "sounds/place.wav", gain = 0.7, pitch_variance = 0.08 }

-- Played where the block was, not where the player is: a sound made at the
-- listener is a sound with no direction, and the whole point of the engine
-- knowing a position is that somebody else standing nearby hears it from the
-- right side.
--
-- Cell coordinates come in and world blocks go out — three cells to a block on
-- each axis (charter rule 5), and the half puts the sound in the middle of the
-- block rather than on its corner.
local function centre_of(event)
    return {
        x = (event.x + 0.5) / 3,
        y = (event.y + 0.5) / 3,
        z = (event.z + 0.5) / 3,
    }
end

game.register_on_dig_complete(function(event)
    game.play_sound{ sound = "break", pos = centre_of(event), radius = 24 }
    -- Returning nothing allows the dig. A sound is an observation, and a mod
    -- that vetoed a dig because it made a noise would be a strange mod.
end)

game.register_on_place(function(event)
    game.play_sound{ sound = "place", pos = centre_of(event), radius = 20 }
end)
