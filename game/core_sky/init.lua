-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The reference sky: how long a day is, and what colour it goes.
--
-- **This file is Task 10's fourth acceptance criterion.** The criterion is that
-- sky content demonstrably lives in a mod, and the way to check it is to delete
-- this directory: the world keeps its light, its shadows and its lamps, and
-- simply stops having a day. Nothing in the engine knows what dawn is.
--
-- What the engine provides is narrow and deliberately so — it advances a number
-- from 0 to 1 over a period this file chooses, and interpolates between colours
-- this file lists. Everything a player would call "the sky" is here.
--
-- Note what this does NOT do: it never computes a colour per frame in Lua. It
-- hands over keyframes once, at registration, and the client interpolates
-- natively. A sky that ran script code every frame would put a mod's error
-- budget on the render thread, which charter rule 10 keeps it well away from.

-- Twenty minutes at the 20 Hz tick. This genre's usual figure, chosen HERE
-- rather than in the engine: a mod wanting a two-hour day changes this line and
-- nothing else.
local DAY_LENGTH_TICKS = 24000

-- Time runs 0 to 1, midnight to midnight, so 0.5 is noon. The keyframes need
-- not be sorted — the engine sorts them, because a list out of order would make
-- the sky jump backwards partway through the day — but they are written in
-- order here because that is how a person reads them.
--
-- `sky` is the colour the horizon goes, and it is also what distance fog fades
-- towards; `sun` tints the stored sunlight; `intensity` scales it. Setting
-- intensity to zero at midnight is what makes caves and the surface equally
-- dark at night while lamps keep working.
game.register_sky{
    day_length_ticks = DAY_LENGTH_TICKS,
    keyframes = {
        -- Midnight. Not black: a night nobody can see anything in is a night
        -- players spend indoors, and the moon is doing something.
        { time = 0.00, sky = {0.02, 0.03, 0.08}, sun = {0.35, 0.45, 0.80}, intensity = 0.08 },
        -- The hour before dawn, still blue.
        { time = 0.20, sky = {0.05, 0.07, 0.15}, sun = {0.40, 0.45, 0.75}, intensity = 0.10 },
        -- Sunrise, warm and low.
        { time = 0.27, sky = {0.85, 0.50, 0.35}, sun = {1.00, 0.65, 0.40}, intensity = 0.55 },
        -- Full morning.
        { time = 0.35, sky = {0.55, 0.72, 0.95}, sun = {1.00, 0.96, 0.90}, intensity = 0.95 },
        -- Noon, the brightest and the flattest.
        { time = 0.50, sky = {0.50, 0.70, 1.00}, sun = {1.00, 1.00, 1.00}, intensity = 1.00 },
        -- Afternoon, on the way down.
        { time = 0.65, sky = {0.55, 0.70, 0.95}, sun = {1.00, 0.95, 0.88}, intensity = 0.95 },
        -- Sunset, warmer than sunrise because it reads better against terrain.
        { time = 0.73, sky = {0.90, 0.45, 0.28}, sun = {1.00, 0.55, 0.30}, intensity = 0.50 },
        -- Dusk falling.
        { time = 0.80, sky = {0.15, 0.12, 0.25}, sun = {0.55, 0.45, 0.70}, intensity = 0.18 },
        -- And back to midnight. **The last keyframe must restate the first's
        -- colours**, or the day ends on a hard cut back to 0.00 at the moment
        -- the clock wraps.
        { time = 1.00, sky = {0.02, 0.03, 0.08}, sun = {0.35, 0.45, 0.80}, intensity = 0.08 },
    },
}

game.log("registered a " .. DAY_LENGTH_TICKS .. "-tick day")
