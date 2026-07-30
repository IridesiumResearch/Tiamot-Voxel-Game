-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The reference world generator: solid below y = 0, air above.
--
-- Deliberately the simplest generator that is still a real one. Its job is to
-- prove the whole path works — registration, freeze, the callback, the native
-- fill, and the cross-platform determinism gate — not to make interesting
-- terrain. Terrain is content, and content is a later phase.
--
-- Note what this does NOT do: it never computes a per-sample value in Lua. It
-- asks the engine for a heightmap and hands it to a fill. That is the only
-- ergonomic path by design — charter rule 4's float subset cannot be enforced
-- inside a script VM, so scripts orchestrate native work rather than doing it.

local white = game.get_block_id("core:white")

game.register_on_generate(function(buf, pos)
    -- A constant surface at y = 0. `flat_heightmap` is the native constant
    -- case; `game.noise_heightmap` is the same shape for real terrain.
    local surface = game.flat_heightmap(0)
    buf:fill_below_heightmap(surface, white)
end)

game.log("registered the reference generator")
