-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Mines a 3x3 slab of real terrain and checks the unit arithmetic.
--
-- Charter rule 5: 1 block = 27 units. Nine blocks is 243 units, which must
-- display as exactly 9 blocks and 0 spare nodes. This script is the canonical
-- end-to-end proof of that design -- it goes through a real client, a real
-- protocol, and a real world.
--
-- It used to build its own slab first, by sending block edits. A client cannot
-- edit the world any more, and it does not need to: `core_worldgen` puts a flat
-- surface, so there is terrain to mine wherever you stand.
--
-- Run: bot run crates/bot/scripts/mine_3x3.lua --server 127.0.0.1:47811

-- The top solid layer. `core_worldgen` fills BELOW its heightmap, so a surface
-- of 0 means blocks at y = -1 and down are solid and y = 0 is already air.
local Y = -1

bot.join("miner")

-- Everything within arm's reach of spawn. The server bounds digging by
-- `phys::REACH` (charter rule 2 — a bound only the client enforces is not one),
-- so a scenario mining at x = 40 would be refused before it proved anything.
--
-- The material is whatever the worldgen put there, so read it from the first
-- dig rather than assuming an id. A scenario that hard-coded one would break
-- the moment the mod set changed, which is exactly what mods are for.
local before = {}
for material, units in pairs(bot.inventory()) do
    before[material] = units
end

for dx = 0, 2 do
    for dz = 0, 2 do
        bot.dig_block(dx - 1, Y, dz - 1)
    end
end

-- Total everything gained, whatever it is made of. Nine blocks of one material
-- or nine of nine: the arithmetic is the same and it is the arithmetic under
-- test.
local gained = 0
for material, units in pairs(bot.inventory()) do
    gained = gained + units - (before[material] or 0)
end

bot.assert(
    gained == 9 * bot.UNITS_PER_BLOCK,
    "nine blocks should be " .. (9 * bot.UNITS_PER_BLOCK) .. " units, got " .. gained
)

local blocks = gained // bot.UNITS_PER_BLOCK
local spares = gained % bot.UNITS_PER_BLOCK
bot.assert(blocks == 9, "expected 9 whole blocks, got " .. blocks)
bot.assert(spares == 0, "expected 0 spare nodes, got " .. spares)

bot.disconnect()
