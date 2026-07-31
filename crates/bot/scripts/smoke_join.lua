-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The smallest useful scenario: connect, join, say something, leave.
--
-- Run: bot run crates/bot/scripts/smoke_join.lua --server 127.0.0.1:47811

bot.join("smoke")
bot.chat("hello from the smoke test")
bot.sleep_ticks(5)

-- An inventory read proves the server answered, not just that it accepted.
local inv = bot.inventory()
bot.assert(type(inv) == "table", "inventory should be a table")

bot.disconnect()
