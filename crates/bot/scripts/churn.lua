-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- Edit/undo loops, for load.
--
-- Places and removes the same blocks repeatedly. Self-cleaning on purpose: a
-- load script that only placed would grow the world without bound and end up
-- measuring the disk rather than the server.
--
-- Run: bot run crates/bot/scripts/churn.lua --server 127.0.0.1:47811

local STONE = 2
local ROUNDS = 20
local WIDTH = 4
local Y = 6

bot.join("churner")

for round = 1, ROUNDS do
    for i = 0, WIDTH - 1 do
        bot.place(60 + i, Y, 60 + (round % 8), STONE)
    end
    for i = 0, WIDTH - 1 do
        bot.dig_block(60 + i, Y, 60 + (round % 8))
    end
    bot.sleep_ticks(1)
end

-- The world must end where it started: every placed block was dug back out.
for i = 0, WIDTH - 1 do
    bot.expect_block(60 + i, Y, 60 + (ROUNDS % 8), bot.AIR, 10000)
end

bot.disconnect()
