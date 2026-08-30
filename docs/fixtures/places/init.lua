-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Four simulation spaces, for looking at what a domain switch feels like.
--
-- This is a TEST FIXTURE, not content (charter rule 1). It exists so Task 15a's
-- one [H] criterion can be walked through by hand. Copy it into `game/` and
-- delete it when you are done — see docs/fixtures/README.md.
--
-- Say any of these in chat: attic, ship, space, void, home

-- **Blocks from `core_blocks`, not blocks of our own.** A block registered with
-- no `textures` draws as the missing-texture chequer, and a fixture whose whole
-- job is "does this look right" must not be the thing that looks wrong. Each
-- space gets a DIFFERENT one, so arriving somewhere is unmistakable rather than
-- looking like a respawn.
local white = game.get_block_id("core:white")
local crumb = game.get_block_id("core:crumb")
local pitch = game.get_block_id("core:pitch")

-- The overworld is `core_worldgen`'s and is left alone.

-- A room with a floor you can tell apart from the overworld's.
game.register_domain{ id = "attic", generator = function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), white)
end }

-- A ship TEMPLATE. Every instance inherits this hull, so fifty ships are one
-- piece of worldgen rather than fifty.
game.register_domain{ id = "ship", instanced = true, generator = function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), crumb)
end }

-- Something to stand on that is neither of the above, for telling two
-- instances apart from each other.
game.register_domain{ id = "cellar", generator = function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), pitch)
end }

-- Entities only, no voxels at all. You float; there is no floor to fall to.
game.register_domain{ id = "space", kind = "sparse", scale = 1000.0 }

-- **The one to try on purpose.** A voxel domain that named no generator, so it
-- is genuinely empty and you fall through it. Nothing arrives, because there is
-- nothing to arrive — which is where "waiting" and "broken" look most alike,
-- and is what a mod author hits the first time they register a domain and
-- forget its generator.
game.register_domain{ id = "void" }

local WHERE = {
    attic = "places:attic",
    cellar = "places:cellar",
    space = "places:space",
    void = "places:void",
    home = "overworld",
}

game.register_on_chat(function(event)
    local body = game.player_entity(event.player)
    if body == nil then
        return
    end
    local to = WHERE[event.text]
    if event.text == "ship" then
        -- Made on demand, and the SAME ship every time: creating one twice
        -- returns the one already there rather than emptying it.
        to = game.create_domain("places:ship", "17")
    end
    if to == nil then
        return
    end
    if not game.transfer_entity(body, to, { x = 8, y = 4, z = 8 }) then
        game.log("nowhere called " .. tostring(to))
    end
    return false
end)

game.log("registered places:attic, places:cellar, places:ship, places:space and places:void")
