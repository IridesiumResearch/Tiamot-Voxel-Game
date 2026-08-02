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
