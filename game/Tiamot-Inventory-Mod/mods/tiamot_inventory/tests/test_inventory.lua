-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
-- Run from repository root: lua5.4 mods/tiamot_inventory/tests/test_inventory.lua
local hooks, shown, inventory, takes, gives, fail_give, short_take = {}, nil, {}, {}, {}, false, false
local names = { [31] = "test:granite", [45] = "test:marble" }
game = { OCCUPANCY_FULL = 0x7FFFFFF, ITEMS_PER_STACK = 90 }
for _, name in ipairs({ "hud_script", "action", "sound" }) do game["register_" .. name] = function() end end
game.bind_sound = function() end
for _, name in ipairs({ "action", "dialog_event", "player_leave" }) do
    game["register_on_" .. name] = function(f) hooks[name] = f end
end
game.inventory = function() return inventory end
game.block_of = function(id) return names[id] end
game.show_dialog = function(spec) shown = spec.tree; return true end
game.update_dialog = game.show_dialog
game.close_dialog = function() return true end
game.take = function(_, spec)
    takes[#takes + 1] = spec
    local actual = short_take and spec.units - 1 or spec.units
    for _, s in ipairs(inventory) do if s.material == spec.material and not s.shape and not s.detail then s.units = s.units - actual; break end end
    return actual
end
game.give = function(_, spec)
    gives[#gives + 1] = spec
    if spec.shape and fail_give then return false end
    if not spec.shape then
        for _, s in ipairs(inventory) do if s.material == spec.material and not s.shape and not s.detail then s.units = s.units + spec.units; break end end
    end
    return true
end
dofile("mods/tiamot_inventory/init.lua")
local function open(player)
    hooks.action{ player = player or "alice", id = "tiamot_inventory:inventory", pressed = true }
end
local function event(kind, name, fields)
    local e = fields or {}; e.player = e.player or "alice"; e.form = "tiamot_inventory:inventory"; e.kind = kind; e.name = name
    hooks.dialog_event(e)
end
local function press(name) event("pressed", name) end
local function find(node, name)
    if node.name == name then return node end
    for _, child in ipairs(node.children or {}) do local n = find(child, name); if n then return n end end
end
local function indices(node, result)
    result = result or {}
    if node.type == "item_slot" then assert(not result[node.index], "duplicate slot"); result[node.index] = true end
    for _, child in ipairs(node.children or {}) do indices(child, result) end
    return result
end
local function reset(units)
    hooks.player_leave{ player = "alice" }
    inventory = { { material = 31, units = units or 90 } }; takes, gives = {}, {}; fail_give, short_take = false, false
    open(); press("shapes")
end
local tests = {
    inventory_layout_and_overflow = function()
        open(); local slots = indices(shown)
        for i = 1, 28 do assert(slots[i], "missing slot " .. i) end
        press("next"); slots = indices(shown)
        for i = 29, 46 do assert(slots[i]) end
        assert(slots[1] and slots[9] and slots[28] and not slots[10])
        press("previous"); assert(indices(shown)[10])
        event("closed"); open(); assert(indices(shown)[10])
    end,
    empty_crafter = function()
        reset(); inventory = {}; press("shapes"); assert(not find(shown, "cut"))
    end,
    full_and_empty_masks_do_not_spend = function()
        reset(); press("make"); assert(#takes == 0)
        event("chiselled", "cut", { shape = 0 }); press("make"); assert(#takes == 0)
    end,
    presets_conserve_units = function()
        for _, pair in ipairs({ { "slab", 9 }, { "stairs", 18 }, { "pillar", 3 } }) do
            reset(); press(pair[1]); press("make")
            assert(takes[1].units == pair[2]); assert(gives[1].count == 1)
            local n = 0; for b = 0, 26 do if gives[1].shape & (1 << b) ~= 0 then n = n + 1 end end
            assert(n * gives[1].count == takes[1].units)
        end
    end,
    stack_is_limited_by_material_and_stack_size = function()
        reset(100); press("slab"); press("make_stack"); assert(gives[1].count == 11 and takes[1].units == 99)
        reset(10000); press("pillar"); press("make_stack"); assert(gives[1].count == 90)
    end,
    insufficient_and_failed_transactions = function()
        reset(2); press("pillar"); press("make"); assert(#takes == 0)
        reset(); press("slab"); short_take = true; press("make"); assert(gives[1].units == 8 and inventory[1].units == 90)
        reset(); press("slab"); fail_give = true; press("make"); assert(gives[2].units == 9 and inventory[1].units == 90)
    end,
    depleted_selection_does_not_craft_another_material = function()
        reset(9); inventory[2] = { material = 45, units = 90 }
        press("shapes"); press("slab"); press("make"); press("make")
        assert(#takes == 1 and inventory[2].units == 90)
        event("chose", "material", { index = 2 }); press("make"); assert(takes[2].material == 45)
    end,
    named_and_shaped_stacks_are_not_consumed = function()
        reset(); inventory = { { material = 31, units = 100, detail = "named" }, { material = 45, units = 50, shape = 7 } }
        press("shapes"); assert(not find(shown, "cut")); press("make"); assert(#takes == 0)
    end,
    player_isolation_and_cleanup = function()
        reset(); press("slab"); open("bob"); assert(indices(shown)[28]); open("bob")
        event("chiselled", "cut", { shape = -1 }); press("make"); assert(takes[1].units == 9)
        hooks.player_leave{ player = "alice" }; open(); assert(indices(shown)[10])
        event("closed"); press("shapes"); assert(indices(shown)[10])
    end,
    carve_does_not_echo_tree = function()
        reset(); local before = shown; event("chiselled", "cut", { shape = 7 }); assert(shown == before)
        press("make"); assert(gives[1].shape == 7 and takes[1].units == 3)
    end,
}
local count = 0
for name, test in pairs(tests) do
    hooks.player_leave{ player = "alice" }; hooks.player_leave{ player = "bob" }
    inventory, takes, gives = {}, {}, {}; short_take, fail_give = false, false
    test(); count = count + 1; print("PASS " .. name)
end
-- Execute the real HUD with the same restricted globals as the engine.
local commands, draw, instructions = {}, nil, 0
local env = { math = math, string = string, table = table, tostring = tostring, ipairs = ipairs, pairs = pairs, type = type }
env.hud = { on_draw = function(f) draw = f end }
for _, kind in ipairs({ "rect", "text", "icon", "image" }) do env.hud[kind] = function(command)
    command.kind = kind; commands[#commands + 1] = command
    if kind == "text" then assert(#command.text <= 512); assert(utf8.len(command.text)) end
end end
assert(loadfile("mods/tiamot_inventory/hud.lua", "t", env))()
for selected = 1, 9 do
    commands, instructions = {}, 0
    debug.sethook(function() instructions = instructions + 100; assert(instructions < 200000) end, "", 100)
    draw{ selected = selected, carried = { [1] = { material = 31, blocks = 1, nodes = 13 },
        [9] = { material = 45, shape = 7, count = 90 } }, offhand = { material = 31, blocks = 2, nodes = 0 },
        tool = { name = string.rep("é", 100) }, looking_at = { name = "granite" } }
    debug.sethook()
    assert(#commands < 512)
    local icons = 0; for _, c in ipairs(commands) do if c.kind == "icon" then icons = icons + 1 end end
    assert(icons == 3, "holes must not hide slot nine or off-hand")
end
print("PASS HUD: sparse slots, off-hand, UTF-8, nine selections, sandbox and budget")
print("Passed " .. count .. " inventory scenarios and HUD checks")
