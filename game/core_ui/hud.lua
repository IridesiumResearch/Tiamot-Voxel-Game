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
local SLOTS = 9
local SLOT = 52
local GAP = 4
local PITCH = SLOT + GAP

local WHITE = { 255, 255, 255, 255 }
local DIM = { 190, 190, 190, 255 }
local PANEL = { 0, 0, 0, 130 }
local SELECTED = { 255, 255, 255, 220 }
local EMPTY = { 0, 0, 0, 90 }

--- Draws the hotbar: a fixed row of slots, filled from what you carry.
---
--- **A fixed row, not one box per stack.** It used to draw one slot per carried
--- material, so a player who had dug one thing saw one box and it moved as they
--- picked up a second. A hotbar is a place: the slots are always there and the
--- contents change.
---
--- `SLOTS` is this mod's choice and nothing else's. The engine registers
--- `engine:hotbar_1` to `_9` and clamps a selection to what is carried, so
--- changing this number changes what is drawn and nothing else breaks. Nine
--- matches the keys the engine offers, which is why it is nine.
local function hotbar(state)
    -- Centred as a group: the leftmost slot starts half the row's width to the
    -- left of the middle.
    local left = -((SLOTS * PITCH) - GAP) / 2

    hud.rect{
        anchor = "bottom", x = left - 6, y = SLOT + 18,
        w = (SLOTS * PITCH) - GAP + 12, h = SLOT + 12, colour = PANEL,
    }

    -- Above the target line rather than beside it: two hints on one row read
    -- as one sentence that does not parse.
    if #state.carried == 0 then
        hud.text{
            anchor = "bottom", x = -110, y = SLOT + 64,
            text = "carrying nothing — dig something", size = 20, colour = DIM,
        }
    end

    for index = 1, SLOTS do
        local slot = state.carried[index]
        local x = left + (index - 1) * PITCH

        -- The empty slot is drawn whether or not anything is in it: that is
        -- what makes it a place rather than a list.
        hud.rect{
            anchor = "bottom", x = x, y = SLOT + 6,
            w = SLOT, h = SLOT, colour = EMPTY,
        }

        -- The selection marker is a frame BEHIND the icon rather than a border
        -- around it: an outline drawn over the icon eats four pixels of a
        -- fifty-two pixel picture, which is a lot of a block to lose. Drawn for
        -- the selected slot even when it is empty, so the marker never
        -- disappears.
        if index == state.selected then
            hud.rect{
                anchor = "bottom", x = x - 3, y = SLOT + 9,
                w = SLOT + 6, h = SLOT + 6, colour = SELECTED,
            }
            hud.rect{
                anchor = "bottom", x = x, y = SLOT + 6,
                w = SLOT, h = SLOT, colour = EMPTY,
            }
        end

        if slot then
            hud.icon{
                anchor = "bottom", x = x, y = SLOT + 6, size = SLOT,
                material = slot.material,
            }

            -- Charter rule 5's display, as the engine worked it out. `1+13` is
            -- what forty units actually is; a raw `40` would be a number about
            -- nothing a player can hold.
            local label
            if slot.nodes == 0 then
                label = tostring(slot.blocks)
            elseif slot.blocks == 0 then
                label = "+" .. slot.nodes
            else
                label = slot.blocks .. "+" .. slot.nodes
            end
            hud.text{
                anchor = "bottom", x = x + 2, y = 20, text = label, size = 17, colour = WHITE,
            }
        end
    end
end

--- A bar that fills while a block is being broken.
---
--- **Deliberately not drawn any more.** A block now comes apart as you dig it —
--- sub-nodes fall away one at a time — so the block itself is the progress
--- indicator, and a bar beside it is a second, worse answer to the same
--- question. `state.dig` is still there for a mod that wants one; this one does
--- not.
---
--- Kept as a function rather than deleted so the decision is visible where
--- somebody would look for it.
local function digging(_state)
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
