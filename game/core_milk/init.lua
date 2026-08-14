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

game.register_fluid{
    id = "milk",
    material = "milk",
    -- The full seven. Milk spreads as far as the engine can express, because a
    -- reference implementation should exercise the ceiling rather than sit
    -- comfortably below it — a flow_range of 3 would never catch an off-by-one
    -- at the limit.
    flow_range = 7,
    -- Every third fluid tick, so a spring runs at about 3 Hz rather than 10.
    -- Reported from the window as spreading "about 3x the speed I would hope
    -- for", which is a mod's opinion to hold and this is where it belongs —
    -- the engine's rate is 10 Hz and stays that way for anything that wants it.
    tick_rate = 3,
    -- **Sources on all but one side make a source.** Without this a source is a
    -- thing that exists exactly once: scoop one out of the middle of a lake and
    -- the hole fills with flow, which drains the moment its parent goes, and
    -- the lake is permanently one block of source poorer. Do that along a
    -- shoreline and the whole body of water is flow hanging off a shrinking
    -- core — an ocean that collapses because somebody filled a bucket.
    --
    -- Three rather than the two Minecraft asks for, deliberately. At two, any
    -- 2x2 pool is an infinite well and a bucket is a way to MAKE ocean; at
    -- three the rule only heals water that was already there.
    --
    -- The engine defaults this to zero and does not have an opinion — creating
    -- matter out of nothing is game design, and charter rule 1 puts that here.
    renews_from = 3,
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

game.register_on_place(function(event)
    if event.material ~= MILK then
        -- Somebody placing an ordinary block. Nothing to do with us; returning
        -- nothing allows it, and returning `false` here would make this mod a
        -- veto on every placement in the world.
        return
    end

    local at = { x = event.x, y = event.y, z = event.z }
    local there = game.get_fluid(at)
    if there.empty then
        game.set_fluid(at, { fluid = "core_milk:milk", source = true })
        game.log("poured milk at " .. at.x .. "," .. at.y .. "," .. at.z)
    else
        -- Clearing the source is what makes the rest drain: every flow block
        -- downstream loses its parent and empties a level per fluid tick, which
        -- is the behaviour most worth being able to watch happen.
        game.set_fluid(at, { level = 0 })
        game.log("scooped milk at " .. at.x .. "," .. at.y .. "," .. at.z)
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
