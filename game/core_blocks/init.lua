-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Reference block registrations.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It exists
-- to prove that block registration works through the public mod API, and to
-- give the worldgen reference mod something to place.

-- What walking on it sounds like. The client plays its own footsteps from its
-- own movement — no round trip, because a player's own steps are the one sound
-- whose lateness they would notice.
game.register_sound{ id = "step", file = "sounds/step.wav", gain = 0.5, pitch_variance = 0.2 }

game.register_block{
    id = "white",
    sounds = { step = "step" },
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

-- Two blocks whose only purpose is to prove `dominance` works through the
-- public API, for the same reason the lamp above proves `light_emit` does.
--
-- A block is 27 sub-node cells, so a chiselled block can hold more than one
-- material and the engine has to decide what the mixture costs to break. It
-- averages mining RATES, which means the soft part of a block carries the rest
-- away on its own; `dominance` is how a material says it should count for more
-- than its share of the cells. See `api/stubs/game.lua`.
--
-- Chisel a few cells of `core:crumb` into a wall of `core:white` and the wall
-- comes apart noticeably faster. Do the same with `core:pitch` and it takes
-- markedly longer. Neither is a game design decision — they are the two
-- directions of one mechanism, made visible.
game.register_block{
    id = "crumb",
    name = "Crumb",
    description = "Soft, and it takes whatever it is mixed into with it.",
    textures = { all = "textures/white.png" },
    hardness = 0.2,
    dominance = 3.0,
}

game.register_block{
    id = "pitch",
    name = "Pitch",
    description = "Stubborn, and it makes whatever it is mixed into stubborn.",
    textures = { all = "textures/white.png" },
    hardness = 6.0,
    dominance = 6.0,
}

game.log("registered core:crumb and core:pitch")


-- The engine's movement cues, given a noise.
--
-- **The engine raises these; this mod decides what they sound like.** There is
-- no jump code a mod could reach and none it needs to: the client watches its
-- own body leave and meet the ground, and plays whatever is bound here without
-- waiting for the server. A sound of your own jump arriving a round trip late
-- reads as a worse sound rather than as latency.
game.register_sound{ id = "jump", file = "sounds/jump.wav", gain = 0.4, pitch_variance = 0.1 }
game.register_sound{ id = "land", file = "sounds/land.wav", gain = 0.6, pitch_variance = 0.1 }

game.bind_sound("engine:jump", "jump")
game.bind_sound("engine:land", "land")
