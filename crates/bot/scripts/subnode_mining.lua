-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Chisels single sub-nodes and checks the spares arithmetic.
--
-- The half of the 27-unit design that makes it fair: one cell must yield ONE
-- unit. If a sub-node dig yielded a whole block, a player could mine 27 blocks'
-- worth of material by taking 27 corners.
--
-- Run: bot run crates/bot/scripts/subnode_mining.lua --server 127.0.0.1:47811

local STONE = 2
local BX, BY, BZ = 50, 6, 50

bot.join("chiseller")

bot.place(BX, BY, BZ, STONE)
bot.expect_block(BX, BY, BZ, STONE, 10000)

local before = bot.inventory()[STONE] or 0

-- A block is 3x3x3 sub-nodes, so the cell coordinates are the block's times
-- three. Walking one axis past 2 lands in the NEXT block, which is air.
local sx, sy, sz = BX * 3, BY * 3, BZ * 3
local cells = { {0,0,0}, {1,0,0}, {2,0,0}, {0,1,0}, {0,0,1} }
for _, cell in ipairs(cells) do
    bot.dig_subnode(sx + cell[1], sy + cell[2], sz + cell[3])
end

-- Wait for the yields rather than sleeping and hoping. A fixed sleep is a
-- guess about how fast the server is, and macOS CI proved the guess wrong:
-- five digs, sleep 200ms, and only one had landed.
bot.expect_units(STONE, before + #cells, 15000)
local gained = (bot.inventory()[STONE] or 0) - before

bot.assert(gained == #cells, "five cells should be five units, got " .. gained)

local blocks = gained // bot.UNITS_PER_BLOCK
local spares = gained % bot.UNITS_PER_BLOCK
bot.assert(blocks == 0, "five units is no whole blocks, got " .. blocks)
bot.assert(spares == 5, "five units is five spare nodes, got " .. spares)

bot.disconnect()
