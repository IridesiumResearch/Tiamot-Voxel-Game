-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Four simulation spaces, for looking at what a domain switch feels like.
-- This is a TEST FIXTURE, not content (charter rule 1) — it exists so the
-- Task 15b [H] gate can be walked through by hand. Delete it when you are done.
--
-- Say any of these in chat: attic, ship, space, void, home

local ground = game.register_block{ id = "ground" }

-- The overworld, so there is something to stand on before you leave it.
game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end)

-- A room with its own floor. The ordinary case: you arrive and there is ground.
game.register_domain{ id = "attic", generator = function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end }

-- A ship TEMPLATE. Every instance inherits this hull, so fifty ships are one
-- piece of worldgen rather than fifty.
game.register_domain{ id = "ship", instanced = true, generator = function(buf, pos)
    buf:fill_below_heightmap(game.flat_heightmap(0), ground)
end }

-- Entities only, no voxels at all. You float.
game.register_domain{ id = "space", kind = "sparse", scale = 1000.0 }

-- **The interesting one for the gate.** No generator, so it is empty and you
-- fall for ever — which means the loading state never clears. That is correct
-- (there is no terrain to arrive) and it is the case where "waiting" and
-- "broken" look most alike.
game.register_domain{ id = "void" }

game.register_on_chat(function(event)
    local body = game.player_entity(event.player)
    if body == nil then
        return
    end
    local to = nil
    if event.text == "attic" then
        to = "places:attic"
    elseif event.text == "ship" then
        to = game.create_domain("places:ship", "17")
    elseif event.text == "space" then
        to = "places:space"
    elseif event.text == "void" then
        to = "places:void"
    elseif event.text == "home" then
        to = "overworld"
    end
    if to == nil then
        return
    end
    game.transfer_entity(body, to, { x = 8, y = 4, z = 8 })
    return false
end)

game.log("registered places:attic, places:ship, places:space and places:void")
