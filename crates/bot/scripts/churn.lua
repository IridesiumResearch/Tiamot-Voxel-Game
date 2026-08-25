-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Dig/rebuild loops, for load.
--
-- Digs blocks out and puts them straight back. Self-cleaning on purpose: a load
-- script that only dug would eat the world, and one that only built would grow
-- it without bound and end up measuring the disk rather than the server.
--
-- **Dig FIRST, build second.** It used to place and then dig, which needed the
-- player to be carrying something before they had mined anything. A client
-- cannot conjure material any more, so the loop runs the way a player's does:
-- take it out, put it back.
--
-- Run: bot run crates/bot/scripts/churn.lua --server 127.0.0.1:47811

local ROUNDS = 20
local WIDTH = 4
-- Beside the player, never under them. Digging the block you are standing on
-- drops you into the hole, and putting it back is then refused for being
-- inside a player -- which is the rule working and the scenario staging it
-- wrong. Everything stays within `phys::REACH`, which the server enforces.
-- The top solid layer. The worldgen fills BELOW its heightmap, so a surface of
-- 0 puts the highest real block at y = -1; digging at 0 would find air and the
-- dig would never complete.
local Y = -1

bot.join("churner")

-- What the ground is made of, learned rather than assumed: one dig, then read
-- the inventory. A hard-coded id would be a scenario coupled to one mod set.
--
-- **What GREW, not what is there.** Picking any id with units in it was only
-- right while a player arrived carrying nothing, and a mod that hands something
-- out on join made it pick that instead — at random, because a Lua table's pair
-- order is unspecified. The failure was a placement refused as "not something
-- you can build with", which is the engine correctly refusing an ITEM.
local held = {}
for id, units in pairs(bot.inventory()) do
    held[id] = units
end
bot.dig_block(2, Y, 0)
local material = nil
for id, units in pairs(bot.inventory()) do
    if units > (held[id] or 0) then
        material = id
    end
end
bot.assert(material ~= nil, "the first dig credited nothing to build with")
bot.place(2, Y, 0, material)

for round = 1, ROUNDS do
    local z = (round % 3) - 1
    for i = 0, WIDTH - 1 do
        bot.dig_block(i + 1, Y, z)
    end
    for i = 0, WIDTH - 1 do
        bot.place(i + 1, Y, z, material)
    end
    bot.sleep_ticks(1)
end

-- The world must end where it started: everything dug was put back.
for i = 0, WIDTH - 1 do
    bot.expect_block(i + 1, Y, (ROUNDS % 3) - 1, material, 10000)
end

bot.disconnect()
