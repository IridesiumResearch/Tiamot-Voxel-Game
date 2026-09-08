-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
-- Inventory presentation and recipes belong to this mod. The engine owns
-- slot transfers, input bindings, shape interaction and authoritative stacks.
game.register_hud_script("hud.lua")
game.register_action{ id = "inventory", default_key = "KeyE", description = "Open inventory / shape crafter" }
game.register_sound{ id = "click", file = "sounds/click.wav", gain = 0.35 }
game.bind_sound("engine:ui_click", "click")
game.bind_sound("engine:ui_close", "click")

-- Engine-domain BLAKE3 of shipped PNGs (tiamot:content:v1 prefix); the content pipeline serves these exact files.
local PANEL_FRAME = { 44, 137, 124, 255, 55, 186, 20, 208, 14, 218, 11, 202, 110, 219, 109, 132, 31, 45, 190, 141, 54, 254, 2, 157, 134, 116, 166, 62, 187, 9, 146, 97 }
local SLOT_FRAME = { 134, 235, 239, 189, 84, 210, 89, 15, 160, 215, 152, 39, 112, 27, 106, 142, 78, 108, 18, 118, 79, 230, 52, 251, 72, 184, 206, 130, 18, 215, 205, 66 }
local FULL = game.OCCUPANCY_FULL
local state = {}
local C = {
    base = { 19, 22, 25, 248 }, panel = { 27, 31, 34, 255 },
    slot = { 14, 17, 20, 255 }, edge = { 89, 94, 94, 255 },
    brass = { 154, 132, 92, 255 }, ink = { 225, 215, 191, 255 },
    muted = { 157, 164, 162, 255 }, accent = { 121, 195, 184, 255 },
}
local function label(text, size, colour)
    return { type = "label", text = text, style = { text_size = size or 17, text_colour = colour or C.ink } }
end
local function box(direction, children, gap, padding, style)
    return { type = "container", direction = direction, children = children,
        gap = gap or 8, padding = padding or 0, align = "stretch", style = style }
end
local function button(name, text, active)
    local b = { type = "button", name = name, text = text, cross_size = 44,
        style = { background = active and { 58, 65, 59, 255 } or C.panel,
            border = active and C.brass or C.edge, text_colour = active and C.ink or C.muted, text_size = 19, nine_slice = SLOT_FRAME } }
    return b
end
local function tab_button(name, text, active)
    local b = button(name, text, active)
    b.size, b.cross_size = 44, nil
    local tab = box("column", { b, { type = "spacer", size = 4,
        style = { background = active and C.brass or C.edge } } }, 0)
    tab.cross_size = 48
    return tab
end
local function section(title, children)
    table.insert(children, 1, label(title, 18, C.brass))
    return box("column", children, 10, 36, { background = C.panel, border = C.edge, nine_slice = PANEL_FRAME })
end
local function session(player)
    if not state[player] then
        state[player] = { tab = "items", page = 1, mask = FULL, options = {}, message = "" }
    end
    return state[player]
end
local function cells(mask)
    local n = 0
    for bit = 0, 26 do
        if mask & (1 << bit) ~= 0 then n = n + 1 end
    end
    return n
end
-- Stable canonical selection: consuming the last of a material must never
-- shift a dropdown index onto a different material. Named stacks stay intact.
local function stock(player)
    local result = {}
    for _, entry in ipairs(game.inventory(player)) do
        local id = game.block_of(entry.material)
        if id and not entry.shape and not entry.detail and entry.units > 0 then
            result[#result + 1] = { id = id, material = entry.material, units = entry.units }
        end
    end
    table.sort(result, function(a, b) return a.id < b.id end)
    return result
end
local function friendly(id)
    return (id:match(":(.+)$") or id):gsub("_", " ")
end
local function slots(first, count)
    local rows = {}
    for row = 0, (count - 1) // 9 do
        local children = {}
        for col = 0, 8 do
            local index = first + row * 9 + col
            if index < first + count then
                children[#children + 1] = { type = "item_slot", view = "player:main", index = index,
                    size = 58, cross_size = 58,
                    style = { background = C.slot, border = C.edge, text_colour = C.ink, text_size = 15, nine_slice = SLOT_FRAME } }
            end
        end
        local r = box("row", children, 5)
        r.size = 58
        rows[#rows + 1] = r
    end
    local grid = box("column", rows, 5)
    grid.align = "center"
    return grid
end
local function items(s)
    local first = s.page == 1 and 10 or 29 + (s.page - 2) * 18
    local navigation = box("row", {
        button("previous", "< Previous"), label("Page " .. s.page, 17, C.muted),
        button("next", "Next >"),
    }, 12)
    return box("column", {
        section("QUICK ACCESS", { slots(1, 9) }),
        section("PACK", { slots(first, math.min(18, 65535 - first + 1)), navigation }),
        box("row", {
                { type = "item_slot", view = "player:main", index = 28, size = 58, cross_size = 58,
                    style = { background = C.slot, border = C.brass, text_colour = C.ink, text_size = 15, nine_slice = SLOT_FRAME } },
                label("Off-hand", 17, C.muted),
        }, 12),
        label("Left: move stack   /   Right: split or place one", 16, C.muted),
    }, 10)
end
-- Integer masks use the engine's x + 3*y + 9*z convention.
local function preset(kind)
    local mask = 0
    for z = 0, 2 do for y = 0, 2 do for x = 0, 2 do
        local keep = kind == "slab" and y == 0
            or kind == "pillar" and x == 1 and z == 1
            or kind == "stairs" and y <= z
        if keep then mask = mask | (1 << (x + 3*y + 9*z)) end
    end end end
    return mask
end
local function shapes(player, s)
    local list = stock(player)
    local names, selected, entry = {}, nil, nil
    s.options = {}
    for i, item in ipairs(list) do
        names[i] = friendly(item.id) .. "  /  " .. item.units .. " units"
        s.options[i] = item.id
        if item.id == s.material then selected, entry = i, item end
    end
    if not s.material and list[1] then
        selected, entry, s.material = 1, list[1], list[1].id
    end
    if not entry then
        -- If a chosen material ran out, require an explicit selection. Never
        -- silently craft the next material using a queued double-click.
        table.insert(names, 1, #list == 0 and "No loose material" or "Choose a material >")
        table.insert(s.options, 1, false)
        selected = 1
    end
    local children = {
        label("Choose material, then carve your shape.", 17, C.muted),
        { type = "dropdown", name = "material", options = names, selected = selected,
            size = 44, style = { background = C.slot, border = C.brass, text_colour = C.ink, text_size = 19, nine_slice = SLOT_FRAME } },
    }
    if #list == 0 then
        children[#children + 1] = section("MATERIAL NEEDED", {
            label("Dig some material to begin shaping.", 16),
            label("Existing shapes and named items are kept intact.", 12, C.muted),
        })
    elseif entry then
        children[#children + 1] = { type = "shape_editor", name = "cut", shape = s.mask,
            material = entry.material, size = 190,
            style = { background = C.slot, border = C.edge, nine_slice = SLOT_FRAME } }
        children[#children + 1] = box("row", {
            button("slab", "Slab"), button("stairs", "Stairs"),
            button("pillar", "Pillar"), button("reset", "Full block"),
        }, 6)
        children[#children + 1] = label("Left: carve   /   Right: restore   /   Arrows: turn", 16, C.muted)
        -- The editor owns its live mask. A delayed tree echo would overwrite
        -- newer clicks, so use a truthful fixed rule instead of a stale cost.
        children[#children + 1] = label("Each remaining cell costs 1 unit per shape.", 17, C.ink)
        children[#children + 1] = box("row", {
            button("make", "Craft one", true), button("make_stack", "Craft stack", true),
        }, 8)
    end
    if s.message ~= "" then children[#children + 1] = label(s.message, 17, C.accent) end
    return section("SHAPE CRAFTER", children)
end
local function screen(player)
    local s = session(player)
    return box("column", {
        label("T I A M O T", 18, C.brass),
        label("Inventory & Crafting", 30),
        box("row", { tab_button("items", "Inventory", s.tab == "items"),
            tab_button("shapes", "Shape crafter", s.tab == "shapes") }, 6),
        s.tab == "shapes" and shapes(player, s) or items(s),
    }, 10, 44, { background = C.base, border = C.brass, nine_slice = PANEL_FRAME })
end
local function redraw(player)
    game.update_dialog{ player = player, form = "inventory", tree = screen(player) }
end
local function craft(player, s, stack)
    local entry
    for _, item in ipairs(stock(player)) do
        if item.id == s.material then entry = item; break end
    end
    local cost = cells(s.mask)
    if cost == 0 or cost == 27 then
        s.message = cost == 0 and "Restore at least one cell first." or "Carve a cell or choose a preset first."
        return
    end
    if not entry then s.message = "Choose a material with loose units remaining."; return end
    local count = stack and math.min(game.ITEMS_PER_STACK, entry.units // cost) or 1
    if count < 1 or entry.units < cost * count then s.message = "Not enough material for this shape."; return end
    local price = count * cost
    local spent = game.take(player, { material = entry.material, units = price })
    if spent ~= price then
        if spent > 0 then game.give(player, { material = entry.material, units = spent }) end
        s.message = "Material changed. Nothing crafted; try again."
    elseif not game.give(player, { material = entry.material, shape = s.mask, count = count }) then
        game.give(player, { material = entry.material, units = spent })
        s.message = "Could not craft. Material returned."
    else
        s.message = "Crafted " .. count .. "  /  " .. price .. " units used"
    end
end

game.register_on_action(function(event)
    if event.id ~= "tiamot_inventory:inventory" or not event.pressed then return end
    local s = session(event.player)
    if s.open then
        game.close_dialog{ player = event.player, form = "inventory" }
        s.open = false
    else
        s.open = game.show_dialog{ player = event.player, form = "inventory", tree = screen(event.player) }
    end
end)
game.register_on_dialog_event(function(event)
    if event.form ~= "tiamot_inventory:inventory" then return end
    local s = state[event.player]
    if not s or not s.open then return end
    if event.kind == "closed" then s.open = false; return end
    if event.kind == "chiselled" and event.name == "cut" and s.tab == "shapes" then
        if math.type(event.shape) == "integer" and event.shape >= 0 and event.shape <= FULL then
            s.mask, s.message = event.shape, ""
        end
        return
    end
    if event.kind == "chose" and event.name == "material" then
        local id = s.options[event.index]
        if id then s.material, s.message = id, ""; redraw(event.player) end
        return
    end
    if event.kind ~= "pressed" then return end
    if event.name == "items" or event.name == "shapes" then
        s.tab, s.message = event.name, ""
    elseif s.tab == "items" and event.name == "previous" then s.page = math.max(1, s.page - 1)
    elseif s.tab == "items" and event.name == "next" then s.page = math.min(3641, s.page + 1)
    elseif s.tab == "shapes" and event.name == "reset" then s.mask, s.message = FULL, ""
    elseif s.tab == "shapes" and (event.name == "slab" or event.name == "stairs" or event.name == "pillar") then
        s.mask, s.message = preset(event.name), ""
    elseif s.tab == "shapes" and (event.name == "make" or event.name == "make_stack") then
        craft(event.player, s, event.name == "make_stack")
    else return end
    redraw(event.player)
end)
game.register_on_player_leave(function(event) state[event.player] = nil end)
