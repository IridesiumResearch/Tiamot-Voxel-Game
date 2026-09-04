-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- A domain with hills in it, so Task 15b's horizon has something to show.
--
-- This is a TEST FIXTURE, not content (charter rule 1). `core_worldgen` is a
-- constant surface at y = 0 on purpose — terrain is content and content is a
-- later phase — but a flat world is exactly the one world in which a level-3
-- summary and a level-1 summary are indistinguishable, so the [H] gate that
-- asks "does the resolution change read as detail rather than a pop" cannot be
-- answered in it. Copy this into `game/` and delete it when you are done; see
-- docs/fixtures/README.md.
--
-- Say "hills" in chat to go, "home" to come back.
--
-- **A DOMAIN, not a replacement generator.** `core_worldgen` owns the
-- overworld's `register_on_generate`, and a fixture that fought it for that
-- callback would change what every other test and every other run sees. A
-- domain is additive: nothing else in the mod set can tell this is loaded, and
-- it composes with the `places` fixture rather than colliding with it. It also
-- happens to exercise the per-domain summary cache, which is the half of 15b
-- that a second domain is the only way to reach.

local white = game.get_block_id("core:white")
local ground = game.get_block_id("core:ground")

-- **Measured, not guessed.** `amplitude` is not the peak-to-trough height its
-- name suggests: the fractal it scales runs to roughly +/-0.42, so an amplitude
-- of 24 buys about 20 blocks of relief in total and reads as a plain with a
-- slight lean. That is what the first draft of this file shipped, and it is
-- exactly what the fixture exists to avoid. Probed against the real generator
-- over a 768-block window:
--
--     freq 0.004, amp  24  ->  20 blocks of spread   (reads as flat)
--     freq 0.020, amp  24  ->  21 blocks             (flat, busier)
--     freq 0.010, amp  60  ->  49 blocks
--     freq 0.008, amp 110  ->  91 blocks, -49 .. +42  <- these
--
-- `frequency` is inverse feature size, so 0.008 puts a hill about 125 blocks
-- across: four of them across the 512-block horizon, which is the scale that
-- makes a level change something you can watch happen rather than a pop.
--
-- The two fills are `core_worldgen`'s, for its reason: `core:ground` drinks and
-- `core:white` does not, so one absorbing layer on top stops milk pooling for
-- ever. `base` shifts the identical field down by one block, which is what
-- makes the second fill leave exactly one layer behind.
local SURFACE = { octaves = 5, frequency = 0.008, amplitude = 110.0, base = 0 }
local BELOW = { octaves = 5, frequency = 0.008, amplitude = 110.0, base = -1 }

game.register_domain{ id = "hills", generator = function(buf, pos)
    buf:fill_below_heightmap(game.noise_heightmap(pos, SURFACE), ground)
    buf:fill_below_heightmap(game.noise_heightmap(pos, BELOW), white)
end }

game.register_on_chat(function(event)
    local body = game.player_entity(event.player)
    if body == nil then
        return
    end
    local to
    if event.text == "hills" then
        to = "relief:hills"
    elseif event.text == "home" then
        to = "overworld"
    end
    if to == nil then
        return
    end
    -- **Well above the highest peak.** The probe above put the highest ground
    -- at y = +49, so 96 always drops into open air rather than into the inside
    -- of a hill. There is no fall damage in the engine — it is a mod's rule
    -- (see `crates/core/src/path.rs`) and no mod here implements one — so the
    -- fall costs nothing but the time.
    if not game.transfer_entity(body, to, { x = 8, y = 96, z = 8 }) then
        game.log("nowhere called " .. tostring(to))
    end
    return false
end)

game.log("registered relief:hills — say 'hills' in chat")
