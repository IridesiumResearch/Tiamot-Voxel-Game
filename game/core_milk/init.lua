-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Reference fluid registration.
--
-- This is a TEST FIXTURE, not the shipped game (see game/README.md). It exists
-- to prove that a fluid can be registered, placed, and picked back up entirely
-- through the public mod API — charter rule 1. If anything here needed engine
-- support that a third-party mod could not reach, that would be a bug in the
-- API rather than something to work around here.

-- The block milk is DRAWN as. A fluid has no material of its own in the block
-- store — a block holds terrain and fluid independently — so a fluid names a
-- registered block and the mesher looks that up.
--
-- It is never placed as terrain by anything. Registering it as an ordinary
-- block is what lets it go through the same texture pipeline as everything
-- else, rather than the engine growing a second, nearly identical path for
-- fluid textures.
game.register_block{
    id = "milk",
    name = "Milk",
    description = "Drawn wherever milk is. Not something you place.",
    textures = { all = "textures/milk.png" },
    -- Milk glows very faintly. Not for the look: it is the cheapest way to see
    -- from across a room whether a pond arrived on a client, which is exactly
    -- what the reference mods are for.
    light_emit = { 2, 2, 2 },
}

-- **The saturation chain: ground that drinks, in three steps.**
--
-- Sub-Node Contract §4.3. The engine's mechanism is "this block takes `rate`
-- cells per fluid tick and then becomes `becomes`"; everything else is here.
-- Saturation is registered MATERIALS rather than state bits on a block, which
-- is what lets this mod own the darker texture and lets another mod give
-- saturated sand different behaviour without the engine learning what porosity
-- is.
--
-- The chain terminates by the last link simply not naming a successor. Soaked
-- ground still drinks — a puddle standing on it keeps draining — but it has
-- nothing left to turn into. A mod that wanted saturated ground to stop
-- absorbing would leave `absorbs` off it entirely, and that is the difference
-- between ground that is full and ground that is a drain.
--
-- Nine cells is a third of a block per fluid tick, chosen so the effect is
-- visible in a few seconds rather than being something you have to wait out.
-- **It is a mod's number**: pour one bucket into a hole in dry ground and most
-- of it soaks away, which is realistic and can read as "the bucket did not
-- work". Tune it here, not in the engine.
game.register_block{
    id = "ground",
    name = "Ground",
    description = "Dry. It drinks what is poured on it.",
    textures = { all = "textures/ground.png" },
    absorbs = { rate = 9, becomes = "damp" },
}

game.register_block{
    id = "damp",
    name = "Damp ground",
    description = "It has had some, and it will take more.",
    textures = { all = "textures/damp.png" },
    absorbs = { rate = 9, becomes = "soaked" },
}

game.register_block{
    id = "soaked",
    name = "Soaked ground",
    description = "As wet as it gets. Still a drain, but it turns into nothing.",
    textures = { all = "textures/soaked.png" },
    absorbs = { rate = 9 },
}

game.register_fluid{
    id = "milk",
    material = "milk",
    -- Every third fluid tick, so a spring runs at about 3 Hz rather than 10.
    -- Reported from the window as spreading "about 3x the speed I would hope
    -- for", which is a mod's opinion to hold and this is where it belongs —
    -- the engine's rate is 10 Hz and stays that way for anything that wants it.
    tick_rate = 3,
    -- **Milk does not evaporate.** A declared sink is the engine's mechanism
    -- and whether to use it is this mod's opinion (charter rule 1): a puddle
    -- that dries out is right for water in the sun and wrong for milk on a
    -- floor, and a reference fixture that quietly destroyed what a test poured
    -- would make every conservation assertion flaky rather than wrong.
    --
    -- A mod that wants a drying world sets this to how many fluid ticks a cell
    -- lasts on average, counted only for blocks open to the air.
    evaporates = 0,
    -- What the world looks like from inside it. Milk, so a warm white rather
    -- than a pure one — pure white reads as fog or as a broken frame, and the
    -- point of being under milk is that you can tell.
    --
    -- The engine has no opinion about this (charter rule 1); it is the mod that
    -- knows the fluid is milk.
    color = { r = 245, g = 243, b = 232 },
}

game.log("registered core_milk:milk")

-- Pouring and scooping, through the placement hook.
--
-- Deliberately NOT a new tool and NOT a new item. Placing the `core_milk:milk`
-- block is intercepted here: the terrain write is cancelled and a milk SOURCE
-- is poured in its place. Select it with the block picker the client already
-- has and every block you place is a spring.
--
-- That is the whole "creative source block" the task asks for, built out of
-- machinery that already existed. A mod that had to add an item, an inventory
-- slot and a UI to place a fluid would be evidence that the fluid API is too
-- small; this is evidence that it is not.
--
-- Placing onto milk that is already there SCOOPS instead, which gives both
-- actions one control. Odd, and it is a MOD's control scheme rather than the
-- engine's, which is the point of it living here.
local MILK = game.get_block_id("core_milk:milk")

-- Waterlogging: a block that changes when milk presses on it.
--
-- **The design decision this implements, and it is not the obvious one.** The
-- tempting model is to let one block hold terrain AND fluid at once. That is
-- refused: Sub-Node Contract §4 says a block above the threshold holds no
-- fluid, full stop, and a block holding both would need every system that
-- reads occupancy — collision, meshing, lighting, the solver — to learn a
-- second rule.
--
-- Instead a MOD swaps the block for a different one. `core_milk:sponge` is dry and
-- `core_milk:waterlogged` is what it becomes; both are ordinary solid blocks, so
-- nothing in the engine needs to know that one is wetter than the other. A
-- crafting recipe wringing a bucket back out of it is a later mod's business.
--
-- This exists to prove `on_fluid_flow` reaches far enough to express the
-- feature, which is the only thing a reference mod is for.
game.register_block{
    id = "sponge",
    name = "Sponge",
    description = "Dry. Milk pressing against it will not stay that way.",
    textures = { all = "textures/milk.png" },
    hardness = 0.3,
}

game.register_block{
    id = "waterlogged",
    name = "Waterlogged sponge",
    description = "What a sponge becomes when milk reaches it.",
    textures = { all = "textures/milk.png" },
    hardness = 0.3,
    -- Heavier to shift than the dry one, and pulling whatever it is mixed with
    -- along with it — which is `dominance` doing the job it exists for.
    dominance = 2.0,
}

game.register_on_fluid_flow(function(event)
    -- Only a sponge, and only where the milk actually is milk. A mod that
    -- reacted to every blocked flow would be a mod that reacted to every wall
    -- on every shoreline in the world.
    if event.block ~= "core_milk:sponge" or event.fluid ~= "core_milk:milk" then
        return
    end

    -- A trickle does not soak a sponge. `volume` is what the milk is pressing
    -- with, in cells of 27, so this is the mod's own threshold and not the
    -- engine's — a third of a block.
    if event.volume < 9 then
        return
    end

    game.set_block(event.into, "core_milk:waterlogged")
    game.log(
        "a sponge soaked at "
            .. event.into.x .. "," .. event.into.y .. "," .. event.into.z
    )
end)

-- Milk has a voice too. Louder and further than a block, because a pour is
-- what a player is looking for when they are trying to find the source.
game.register_sound{ id = "pour", file = "sounds/pour.wav", gain = 0.8, pitch_variance = 0.15 }

-- **The known wart, and it is content rather than a mechanism.**
--
-- This mod has no bucket ITEM: the milk material is both the thing carried and
-- the thing poured, so the hook only fires while the player is holding some. A
-- player who pours their very last drop is left with an empty hand and no way
-- to click the puddle back up.
--
-- A real game mod fixes this with a container — an item that persists whether
-- it is full or empty, the way a bucket does — and that is a design decision
-- about a game rather than a gap in the engine (charter rule 1). A reference
-- fixture proving the mechanism does not need one, and inventing one here would
-- be this file quietly becoming a game.
--
-- How much a bucket carries, in cells of 27 — one whole block's worth.
--
-- Charter rule 5's own unit, so a bucket is not a second quantity: it is
-- twenty-seven units of the milk material, held in the inventory exactly like
-- twenty-seven units of stone.
local BUCKET = 27

game.register_on_place(function(event)
    if event.material ~= MILK then
        -- Somebody placing an ordinary block. Nothing to do with us; returning
        -- nothing allows it, and returning `false` here would make this mod a
        -- veto on every placement in the world.
        return
    end

    local at = { x = event.x, y = event.y, z = event.z }
    local there = game.get_fluid(at)

    -- **Partial buckets, and they cost nothing to have.**
    --
    -- Under a conserved fluid a bucket is a MEASUREMENT rather than a switch,
    -- so scooping half a puddle has to give back half a bucket. Charter rule 5
    -- already makes that expressible without a new concept: milk in the
    -- inventory is units of a material like anything else, so "a bucket" is
    -- just 27 of them and "half a bucket" is 13.
    --
    -- The alternative the design considered was Minecraft's — top the bucket up
    -- out of neighbouring blocks until it is full — and it was rejected because
    -- it makes scooping one block drain water the player never pointed at.
    if there.volume > 0 then
        -- Take exactly what is in THIS block. Nothing is pulled out of its
        -- neighbours, so what the player scooped is what they were looking at.
        game.give(event.player, { material = "core_milk:milk", units = there.volume })
        game.set_fluid(at, { volume = 0 })
        game.log("scooped " .. there.volume .. " cells at " .. at.x .. "," .. at.y .. "," .. at.z)
    else
        -- **Charged here, by this mod, and that is not an oversight.** A hook
        -- that cancels a placement is not charged by the engine — see the
        -- `verdict.allowed` path in the server's placement loop — so the mod
        -- that decided what the click meant is the one that pays for it.
        --
        -- `game.take` reports what it actually got, which is how a player
        -- carrying thirteen units pours thirteen cells rather than being
        -- refused for not having a full bucket.
        local got = game.take(event.player, { material = "core_milk:milk", units = BUCKET })
        if got > 0 then
            game.set_fluid(at, { fluid = "core_milk:milk", volume = got })
            game.play_sound{
                sound = "pour",
                pos = { x = at.x + 0.5, y = at.y + 0.5, z = at.z + 0.5 },
                radius = 28,
            }
            game.log("poured " .. got .. " cells at " .. at.x .. "," .. at.y .. "," .. at.z)
        end
    end

    -- Cancel the terrain write. The block was only ever a way of naming what to
    -- pour — leaving it behind would seal the milk inside a solid block, which
    -- Sub-Node Contract §4 says cannot hold any.
    --
    -- **The empty string, not `false`.** Both cancel the placement; `false`
    -- means "the player may not build here" and the engine tells them so, which
    -- is the wrong thing to say to somebody whose milk poured perfectly. This
    -- pour SUCCEEDED — the mod handled it — so the player hears nothing.
    return ""
end)
