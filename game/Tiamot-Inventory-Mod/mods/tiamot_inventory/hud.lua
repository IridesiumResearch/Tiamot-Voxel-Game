-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
-- Virtual-pixel geometry only. Counts, shapes and selection come from the
-- engine; holes in the hotbar never collapse or move another slot.
local SLOT, GAP, SLOTS = 72, 6, 9
local SLOT_FRAME = "86ebefbd54d2590fa0d79827701b6a8e4e6c12764fe634fb48b8ce8212d7cd42"
local PITCH = SLOT + GAP
local C = {
    shadow = { 0, 0, 0, 90 }, base = { 17, 20, 23, 235 },
    edge = { 84, 91, 93, 255 }, bevel = { 117, 120, 116, 255 },
    well = { 13, 16, 19, 245 }, brass = { 181, 155, 108, 255 },
    muted = { 160, 169, 168, 255 }, ink = { 235, 223, 196, 255 },
    accent = { 119, 202, 187, 255 },
}
local function rect(x, y, w, h, colour)
    hud.rect{ anchor = "bottom", x = x, y = y, w = w, h = h, colour = colour }
end
local function text(x, y, value, size, colour)
    hud.text{ anchor = "bottom", x = x, y = y, text = value, size = size, colour = colour }
end
-- Three rectangles create a small clipped corner without a raster dependency.
local function chamfer(x, y, w, h, c)
    rect(x + 2, y, w - 4, h, c)
    rect(x + 1, y - 1, 1, h - 2, c)
    rect(x + w - 2, y - 1, 1, h - 2, c)
end
local function quantity(slot)
    if slot.count then return tostring(slot.count) end
    if slot.nodes == 0 then return tostring(slot.blocks) end
    if slot.blocks == 0 then return "+" .. slot.nodes end
    return slot.blocks .. "+" .. slot.nodes
end
local function draw_slot(x, key, stack, selected)
    local y = SLOT + 24
    chamfer(x - 2, y + 2, SLOT + 4, SLOT + 4, C.shadow)
    chamfer(x, y, SLOT, SLOT, selected and C.brass or C.edge)
    hud.image{ anchor = "bottom", x = x + 2, y = y - 2, w = SLOT - 4, h = SLOT - 4, hash = SLOT_FRAME }
    rect(x + 4, y - 2, SLOT - 8, 1, selected and C.ink or C.bevel)
    if selected then
        rect(x + 13, 25, SLOT - 26, 2, C.accent)
    end
    if stack then
        hud.icon{ anchor = "bottom", x = x + 13, y = y - 13, size = SLOT - 26,
            material = stack.material, shape = stack.shape }
        -- A dark backing keeps quantities readable on pale material icons.
        local q = quantity(stack)
        local width = #q * 11 + 6
        rect(x + SLOT - width - 4, 43, width, 17, C.base)
        text(x + SLOT - width - 1, 43, q, 17, C.ink)
    end
    text(x + 5, y - 4, key, 14, selected and C.ink or C.muted)
end
hud.on_draw(function(state)
    local width = SLOTS * PITCH - GAP
    local left = -(width // 2)
    chamfer(left - 12, SLOT + 35, width + 24, SLOT + 28, C.shadow)
    chamfer(left - 8, SLOT + 32, width + 16, SLOT + 22, C.base)
    rect(left + 6, SLOT + 31, width - 12, 1, C.edge)
    for index = 1, SLOTS do
        draw_slot(left + (index - 1) * PITCH, tostring(index), state.carried[index], index == state.selected)
    end
    if state.offhand then
        draw_slot(left - PITCH - 12, "II", state.offhand, false)
    end
    local line = state.tool and state.tool.name or "Empty hand"
    if state.looking_at then line = line .. "  /  " .. state.looking_at.name end
    -- Registered names may be long. Keep this optional line within the HUD's
    -- text budget, trimming whole UTF-8 characters rather than splitting bytes.
    if #line > 64 then
        local last = 61
        while line:byte(last + 1) >= 128 and line:byte(last + 1) < 192 do last = last - 1 end
        line = line:sub(1, last) .. "..."
    end
    text(left + 6, SLOT + 58, line, 18, C.muted)
end)
