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
--
-- `grade` is the optional one, and lighting mode 3 is the only mode that applies
-- it: how the finished picture is graded, rather than how the world is lit. The
-- values below are deliberately restrained — a reference implementation exists to
-- prove the mechanism reaches the screen, and a mod that wants a look can push
-- every one of these much further. Noon is left as an exact identity on purpose,
-- so there is one hour in the day where mode 3 grades nothing and any difference
-- from mode 2 must be the shadows, bloom and fog rather than the grade.
game.register_sky{
    day_length_ticks = DAY_LENGTH_TICKS,
    -- Where a fresh world's clock starts. Mid-morning, because the alternative
    -- is what a counter starting at zero gives you: midnight, with no sun, no
    -- shadows, and nothing on screen to tell one graphics setting from another.
    -- The engine defaults to this same hour when a sky says nothing, but saying
    -- it here is the point — which hour a world opens on is this mod's call.
    start_time = 0.35,
    keyframes = {
        -- Midnight. Not black: a night nobody can see anything in is a night
        -- players spend indoors, and the moon is doing something.
        --
        -- The grade is what makes moonlight read as moonlight: the eye loses
        -- colour in the dark, so night is desaturated and cool, and opened up a
        -- little so the shapes are still legible at an intensity of 0.08.
        { time = 0.00, sky = {0.02, 0.03, 0.08}, sun = {0.35, 0.45, 0.80}, intensity = 0.08,
          grade = { exposure = 1.15, saturation = 0.55, tint = {0.92, 0.96, 1.12}, contrast = 0.95 } },
        -- The hour before dawn, still blue.
        { time = 0.20, sky = {0.05, 0.07, 0.15}, sun = {0.40, 0.45, 0.75}, intensity = 0.10,
          grade = { exposure = 1.12, saturation = 0.60, tint = {0.94, 0.97, 1.10}, contrast = 0.96 } },
        -- Sunrise, warm and low.
        { time = 0.27, sky = {0.85, 0.50, 0.35}, sun = {1.00, 0.65, 0.40}, intensity = 0.55,
          grade = { saturation = 1.10, tint = {1.05, 1.00, 0.95}, contrast = 1.05 } },
        -- Full morning.
        { time = 0.35, sky = {0.55, 0.72, 0.95}, sun = {1.00, 0.96, 0.90}, intensity = 0.95,
          grade = { saturation = 1.03, contrast = 1.02 } },
        -- Noon, the brightest and the flattest. Ungraded, deliberately — see the
        -- note above the list.
        { time = 0.50, sky = {0.50, 0.70, 1.00}, sun = {1.00, 1.00, 1.00}, intensity = 1.00 },
        -- Afternoon, on the way down.
        { time = 0.65, sky = {0.55, 0.70, 0.95}, sun = {1.00, 0.95, 0.88}, intensity = 0.95,
          grade = { saturation = 1.04, contrast = 1.02 } },
        -- Sunset, warmer than sunrise because it reads better against terrain.
        { time = 0.73, sky = {0.90, 0.45, 0.28}, sun = {1.00, 0.55, 0.30}, intensity = 0.50,
          grade = { saturation = 1.15, tint = {1.07, 1.00, 0.94}, contrast = 1.06 } },
        -- Dusk falling.
        { time = 0.80, sky = {0.15, 0.12, 0.25}, sun = {0.55, 0.45, 0.70}, intensity = 0.18,
          grade = { exposure = 1.08, saturation = 0.75, tint = {0.97, 0.98, 1.08}, contrast = 0.98 } },
        -- And back to midnight. **The last keyframe must restate the first's
        -- colours** — and its grade, for the same reason — or the day ends on a
        -- hard cut back to 0.00 at the moment the clock wraps.
        { time = 1.00, sky = {0.02, 0.03, 0.08}, sun = {0.35, 0.45, 0.80}, intensity = 0.08,
          grade = { exposure = 1.15, saturation = 0.55, tint = {0.92, 0.96, 1.12}, contrast = 0.95 } },
    },
}

game.log("registered a " .. DAY_LENGTH_TICKS .. "-tick day")
