-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The chisel scenario, as a scenario: Task 09's [A] acceptance criterion
-- written the way a modder would read it.
--
-- Chisel 13 sub-nodes out of a block, check the inventory shows 0 blocks and 13
-- spare nodes, then place them back and check the geometry that results.
--
-- **This is the whole argument for sub-nodes, end to end.** A block is 3x3x3
-- cells and 27 units (charter rule 5). Take 13 of those cells and you hold 13
-- units -- not 13 blocks, and not one. Put them down and you get a block that
-- is 13 cells full, filled from the bottom up, not a cube and not nothing.
--
-- Nothing here is special-cased in the engine. `core_tools:chisel` is a mod
-- using `register_tool{ brush = "subnode" }`, and this script picks it by BRUSH
-- rather than by name -- the engine has no opinion about what a chisel is
-- called (charter rule 1), so naming one would couple the scenario to `game/`
-- instead of to the API.
--
-- Run: bot run crates/bot/scripts/chisel_sculpt.lua --server 127.0.0.1:47811

-- How many cells to take. Fewer than 27, so what comes back is a PARTIAL block
-- -- which is the case that only exists because sub-nodes do.
local CELLS = 13

-- Arm's reach of spawn: the server bounds digging and placing by `phys::REACH`,
-- so a scenario working across the map would be refused before it proved
-- anything. y = -1 is the top solid layer, because `core_worldgen` fills BELOW
-- its heightmap.
local BX, BY, BZ = 2, -1, 0
-- Empty air beside the player to build in. Placing into the cell you occupy is
-- refused, and rightly.
local TX, TY, TZ = -2, 0, 0

bot.join("sculptor")

local before = 0
for _, units in pairs(bot.inventory()) do
    before = before + units
end

-- Take 13 of the block's 27 cells, bottom layer first. A block spans three
-- cells per axis, so the cell coordinates are the block's times three; walking
-- one axis past 2 would land in the NEXT block rather than another of this
-- one's cells.
local sx, sy, sz = BX * 3, BY * 3, BZ * 3
local taken = 0
for y = 0, 2 do
    for z = 0, 2 do
        for x = 0, 2 do
            if taken < CELLS then
                bot.dig_subnode(sx + x, sy + y, sz + z)
                taken = taken + 1
            end
        end
    end
end

local gained = 0
local material = nil
for id, units in pairs(bot.inventory()) do
    gained = gained + units
    if units > 0 then
        material = id
    end
end
gained = gained - before

bot.assert(
    gained == CELLS,
    CELLS .. " chiselled cells should be " .. CELLS .. " units, got " .. gained
)
bot.assert(material ~= nil, "the chiselling credited nothing to build with")

-- The display arithmetic charter rule 5 specifies: units / 27 blocks plus
-- units % 27 spare nodes. Thirteen units is no whole blocks and thirteen
-- spares -- a yield that rounded, or that counted blocks instead of units,
-- fails right here.
local blocks = gained // bot.UNITS_PER_BLOCK
local spares = gained % bot.UNITS_PER_BLOCK
bot.assert(blocks == 0, CELLS .. " units is no whole blocks, got " .. blocks)
bot.assert(spares == CELLS, CELLS .. " units is " .. CELLS .. " spare nodes, got " .. spares)

-- And put them back. The chisel goes away first: the brush decides what a
-- placement WRITES as well as what a dig removes, so building with a chisel in
-- hand puts down one cell -- the cell under the crosshair, which is what makes
-- carving reversible and is exercised by `sculpt_back` below. This part of the
-- scenario is about the block that 13 spare units make, so it wants the
-- whole-block brush. `bot.place` selects it, again by brush and not by name.
--
-- Fewer than 27 units makes a PARTIAL block: the engine fills it from the
-- bottom up, which `inventory::placement_mask` documents because the order is
-- observable.
bot.place(TX, TY, TZ, material)

-- The geometry, server-side. `expect_partial` waits for the broadcast that says
-- how many cells were filled, so this asserts the SHAPE rather than merely that
-- something appeared.
bot.expect_partial(TX, TY, TZ, material, CELLS, 10000)

-- Everything chiselled went back into the world.
local after = 0
for _, units in pairs(bot.inventory()) do
    after = after + units
end
bot.assert(
    after == before,
    "placing " .. CELLS .. " spares should have spent all of them, " .. after .. " left"
)

-- Sculpting in both directions, which is the reason a sub-node brush places
-- into the cell it is aimed at.
--
-- Take one cell out of a block and put it straight back into the SAME cell.
-- Two engine rules have to hold for this to work, and the scenario fails
-- visibly without either: a sub-node placement fills the cell under the
-- crosshair rather than the bottom of the block, and the "is it already
-- occupied" check is per cell rather than per block, so a block that still
-- holds its other 26 cells can be built into.
--
-- The top far corner: cell 26 of 27, the one a bottom-up fill would be least
-- likely to reach by accident.
local cx, cy, cz = sx + 2, sy + 2, sz + 2
bot.dig_subnode(cx, cy, cz)
bot.expect_units(material, 1, 10000)

-- `place_subnode` holds a tool by BRUSH, the same way `dig_subnode` does, so
-- this stays a statement about the mod API and not about `core_tools`.
bot.place_subnode(cx, cy, cz, material)

local left = 0
for _, units in pairs(bot.inventory()) do
    left = left + units
end
bot.assert(
    left == before,
    "the chiselled cell should have gone back into the world, " .. left .. " units left over"
)

bot.disconnect()
