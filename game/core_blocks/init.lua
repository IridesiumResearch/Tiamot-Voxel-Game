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
}

game.log("registered core:white")
