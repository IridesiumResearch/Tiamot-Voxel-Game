-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Reference block registrations.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It exists
-- to prove that block registration works through the public mod API, and to
-- give the worldgen reference mod something to place.

game.register_block{
    id = "white",
    name = "White",
    description = "A featureless solid block.",
    -- Paths are relative to this mod's own directory, and the file is pushed
    -- to clients through the content pipeline rather than being something the
    -- engine knows about. `all` is the only key for now; per-face textures are
    -- an additive change when something needs them.
    --
    -- The image is white with a faint border on purpose. A wall of pure white
    -- blocks is one undifferentiated mass, and it is impossible to tell from
    -- looking at it whether meshing is working at all.
    textures = { all = "textures/white.png" },
}

game.log("registered core:white")

-- A lamp, which exists to prove `light_emit` works through the public API.
--
-- Task 10 gave `register_block` a `light_emit` field and, until this, nothing
-- in the reference mods used it — a mechanism with no reference implementation
-- is a mechanism nobody has checked from the outside. Delete this block and
-- the world keeps its daylight and loses every artificial light in it.
--
-- Warm rather than white: a lamp the same colour as the sun is indistinguishable
-- from a hole in the roof, which makes the coloured-light path impossible to
-- see working. The numbers are 0..15 per channel, the range the engine stores.
game.register_block{
    id = "lamp",
    name = "Lamp",
    description = "A block that glows warm.",
    textures = { all = "textures/white.png" },
    light_emit = { r = 15, g = 11, b = 6 },
}

game.log("registered core:lamp")
