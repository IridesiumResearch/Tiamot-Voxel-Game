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
