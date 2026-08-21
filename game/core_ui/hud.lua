-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The reference HUD: a hotbar, a dig readout, and what you are looking at.
--
-- This runs on the PLAYER's machine, once a frame, inside a sandbox with no
-- filesystem, no network and an instruction ceiling. Everything it may know
-- arrives in `state`; everything it may do is on `hud`.
--
-- **It draws and does not compute.** The engine has already divided the units
-- into blocks and spare nodes (charter rule 5), already worked out what is
-- under the crosshair, already clamped the dig progress. A HUD that recomputed
-- any of that would be a second answer nobody can reconcile with the first.

-- Virtual pixels. The canvas is 1080 tall and as wide as the window is; anchors
-- do the rest, so these numbers mean the same thing on every monitor.
local SLOT = 52
local GAP = 4
local PITCH = SLOT + GAP

local WHITE = { 255, 255, 255, 255 }
local DIM = { 190, 190, 190, 255 }
local PANEL = { 0, 0, 0, 130 }
local SELECTED = { 255, 255, 255, 220 }

--- Draws the carried stacks, centred along the bottom edge.
local function hotbar(state)
    local count = #state.carried
    if count == 0 then
        hud.text{
            anchor = "bottom", x = -110, y = 46,
            text = "carrying nothing — dig something", size = 20, colour = DIM,
        }
        return
    end

    -- Centred as a group: the leftmost slot starts half the row's width to the
    -- left of the middle, so a row of three and a row of nine are both centred.
    local left = -((count * PITCH) - GAP) / 2

    hud.rect{
        anchor = "bottom", x = left - 6, y = SLOT + 18,
        w = (count * PITCH) - GAP + 12, h = SLOT + 12, colour = PANEL,
    }

    for index, slot in ipairs(state.carried) do
        local x = left + (index - 1) * PITCH

        -- The selection marker is a frame BEHIND the icon rather than a border
        -- around it: an outline drawn over the icon eats four pixels of a
        -- fifty-two pixel picture, which is a lot of a block to lose.
        if index == state.selected then
            hud.rect{
                anchor = "bottom", x = x - 3, y = SLOT + 9,
                w = SLOT + 6, h = SLOT + 6, colour = SELECTED,
            }
        end

        hud.icon{ anchor = "bottom", x = x, y = SLOT + 6, size = SLOT, material = slot.material }

        -- Charter rule 5's display, as the engine worked it out. `1+13` is what
        -- forty units actually is; a raw `40` would be a number about nothing a
        -- player can hold.
        local label
        if slot.nodes == 0 then
            label = tostring(slot.blocks)
        elseif slot.blocks == 0 then
            label = "+" .. slot.nodes
        else
            label = slot.blocks .. "+" .. slot.nodes
        end
        hud.text{ anchor = "bottom", x = x + 2, y = 20, text = label, size = 17, colour = WHITE }
    end
end

--- A bar that fills while a block is being broken.
local function digging(state)
    if not state.dig then
        return
    end
    hud.bar{
        anchor = "centre", x = -90, y = 40,
        w = 180, h = 8, fill = state.dig,
        colour = WHITE, background = PANEL,
    }
end

--- What the crosshair is on, and what is in hand to do it with.
local function target(state)
    local line = state.tool and (state.tool.name .. " · " .. state.tool.brush .. " brush")
        or "no tool"
    if state.looking_at then
        line = line .. "   →   " .. state.looking_at.name
    end
    hud.text{ anchor = "bottom", x = -180, y = SLOT + 40, text = line, size = 18, colour = DIM }
end

hud.on_draw(function(state)
    hotbar(state)
    digging(state)
    target(state)
end)
