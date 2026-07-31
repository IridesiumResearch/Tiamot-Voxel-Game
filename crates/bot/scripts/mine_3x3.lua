-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Mines a 3x3 slab and checks the unit arithmetic.
--
-- Charter rule 5: 1 block = 27 units. Nine blocks is 243 units, which must
-- display as exactly 9 blocks and 0 spare nodes. This script is the canonical
-- end-to-end proof of that design -- it goes through a real client, a real
-- protocol, and a real world.
--
-- Run: bot run crates/bot/scripts/mine_3x3.lua --server 127.0.0.1:47811

local STONE = 2
local Y = 6

bot.join("miner")

-- Build something to mine. Worldgen may or may not have put anything here, so
-- the scenario supplies its own material rather than assuming terrain.
for dx = 0, 2 do
    for dz = 0, 2 do
        bot.place(40 + dx, Y, 40 + dz, STONE)
    end
end
for dx = 0, 2 do
    for dz = 0, 2 do
        bot.expect_block(40 + dx, Y, 40 + dz, STONE, 10000)
    end
end

local before = bot.inventory()[STONE] or 0

for dx = 0, 2 do
    for dz = 0, 2 do
        bot.dig_block(40 + dx, Y, 40 + dz)
    end
end
for dx = 0, 2 do
    for dz = 0, 2 do
        bot.expect_block(40 + dx, Y, 40 + dz, bot.AIR, 10000)
    end
end

bot.sleep_ticks(4)
local gained = (bot.inventory()[STONE] or 0) - before

bot.assert(
    gained == 9 * bot.UNITS_PER_BLOCK,
    "nine blocks should be " .. (9 * bot.UNITS_PER_BLOCK) .. " units, got " .. gained
)

local blocks = gained // bot.UNITS_PER_BLOCK
local spares = gained % bot.UNITS_PER_BLOCK
bot.assert(blocks == 9, "expected 9 whole blocks, got " .. blocks)
bot.assert(spares == 0, "expected 0 spare nodes, got " .. spares)

bot.disconnect()
