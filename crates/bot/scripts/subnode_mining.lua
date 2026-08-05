-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Chisels single sub-nodes out of real terrain and checks the spares
-- arithmetic.
--
-- The half of the 27-unit design that makes it fair: one cell must yield ONE
-- unit. If a sub-node dig yielded a whole block, a player could mine 27 blocks'
-- worth of material by taking 27 corners.
--
-- `bot.dig_subnode` holds a tool whose brush is `"subnode"`, chosen by BRUSH
-- rather than by name -- the engine has no opinion about what a chisel is
-- called (charter rule 1), so a scenario that named one would be coupled to
-- `game/` instead of to the API.
--
-- Run: bot run crates/bot/scripts/subnode_mining.lua --server 127.0.0.1:47811

-- The top solid layer: `core_worldgen` fills below its heightmap, so y = -1 is
-- the highest block that is actually there.
local BX, BY, BZ = 50, -1, 50

bot.join("chiseller")

local before = 0
for _, units in pairs(bot.inventory()) do
    before = before + units
end

-- A block is 3x3x3 sub-nodes, so the cell coordinates are the block's times
-- three. Walking one axis past 2 lands in the NEXT block, which would be a
-- different block rather than another of this one's cells.
local sx, sy, sz = BX * 3, BY * 3, BZ * 3
local cells = { {0,0,0}, {1,0,0}, {2,0,0}, {0,1,0}, {0,0,1} }
for _, cell in ipairs(cells) do
    bot.dig_subnode(sx + cell[1], sy + cell[2], sz + cell[3])
end

local gained = 0
for _, units in pairs(bot.inventory()) do
    gained = gained + units
end
gained = gained - before

bot.assert(gained == #cells, "five cells should be five units, got " .. gained)

local blocks = gained // bot.UNITS_PER_BLOCK
local spares = gained % bot.UNITS_PER_BLOCK
bot.assert(blocks == 0, "five units is no whole blocks, got " .. blocks)
bot.assert(spares == 5, "five units is five spare nodes, got " .. spares)

bot.disconnect()
