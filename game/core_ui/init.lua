-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The reference HUD, and the proof that a HUD is a mod's.
--
-- Delete this directory and a client keeps a crosshair, chat and the settings
-- screen — and loses the hotbar, the dig readout, the target line and the
-- inventory screen entirely. That is Task 14's first acceptance criterion, and
-- `core_ui_owns_the_hotbar_and_taking_it_away_leaves_the_engine_alone` in
-- `crates/client/tests/connection.rs` is the test that will not let it quietly
-- stop being true.
--
-- The HUD is drawn by `hud.lua`, which the client runs in a sandbox. The
-- inventory screen is a widget tree, which nothing runs at all.

game.register_hud_script("hud.lua")

-- The inventory screen, and the other half of criterion 1.
--
-- **Tier 1, not tier 2.** A hotbar is a function of what you carry, so it is a
-- script; an inventory screen is a fixed arrangement of slots, so it is DATA and
-- goes over the wire as a widget tree that executes nothing. Which tier a piece
-- of UI belongs to is decided by whether it needs to compute, and this one does
-- not.
--
-- The engine owns the key (charter rule 11): this registers a NAME, suggests a
-- default, and never asks which key a player chose.
game.register_action{
    id = "inventory",
    default_key = "KeyE",
    description = "Open the inventory",
}

-- What each player has on screen: which tab, and what they have chiselled.
--
-- Keyed by UUID, never a display name (charter rule 13) — a player who changes
-- their name on another server must not lose their open screen.
local open = {}
local tab = {}
local cut = {}
local chosen = {}

--- Every material a player is carrying loose, as `{ id, material, units }`.
---
--- Loose only: a stack that is already cut is not something to cut again, and
--- offering it would suggest the shapes compose.
local function stock(player)
    local list = {}
    for _, entry in ipairs(game.inventory(player)) do
        if not entry.shape then
            list[#list + 1] = {
                id = game.block_of(entry.material) or ("material " .. entry.material),
                material = entry.material,
                units = entry.units,
            }
        end
    end
    table.sort(list, function(a, b) return a.id < b.id end)
    return list
end

--- How many cells a mask occupies, which is what one item of it costs.
local function cells(mask)
    local count = 0
    for bit = 0, 26 do
        if mask & (1 << bit) ~= 0 then
            count = count + 1
        end
    end
    return count
end

--- The two tabs, as a row of buttons.
---
--- **Buttons, not a tab widget.** The engine has no tabs and does not need
--- them: a tab is a button that changes what the rest of the tree is, and
--- `game.update_dialog` replaces a tree. Anything the engine added here would
--- be a fixed idea of what a tab looks like, imposed on every mod.
local function tabs(which)
    local function button(id, text)
        return {
            type = "button", name = id, text = text,
            style = which == id and { background = { 70, 90, 120 } } or nil,
        }
    end
    return {
        type = "container", direction = "row", gap = 6,
        children = { button("tab_items", "Items"), button("tab_shapes", "Shapes") },
    }
end

--- The items tab: one grid, and the top row is the hotbar.
---
--- **One view, not two.** `player:main` is where everything a player owns
--- lives, and its first nine slots are the ones the number keys select
--- between — so the hotbar is a PLACE in this grid rather than a second
--- inventory to shuffle things into.
local function items_tab()
    return {
        type = "container", direction = "column", gap = 6,
        children = {
            { type = "label", text = "Inventory — the top row is your hotbar" },
            { type = "item_grid", view = "player:main", columns = 9, first = 1, count = 27 },
            { type = "spacer", size = 6 },
            -- Slot 28: the off-hand, which the engine's `engine:offhand` key
            -- swaps into. Shown so a player can see what is in it and drag
            -- something else there; the key is for doing it without looking.
            {
                type = "container", direction = "row", gap = 6,
                children = {
                    { type = "label", text = "Off-hand" },
                    { type = "item_slot", view = "player:main", index = 28 },
                },
            },
        },
    }
end

--- The shape tab: chisel a block, then make as many of that cut as you can pay
--- for.
---
--- **The recipe is this mod's, and only this mod's.** The engine draws the
--- cells, reports the mask, and moves units when asked; that one item of a cut
--- costs one unit per cell is a decision written here, in Lua, and a mod that
--- wants chiselling to cost a tool or a bench replaces it without touching the
--- engine.
local function shapes_tab(player)
    local list = stock(player)
    local names = {}
    for index, entry in ipairs(list) do
        names[index] = entry.id .. "  (" .. entry.units .. "u)"
    end
    local pick = chosen[player] or 1
    local mask = cut[player] or 0x7FFFFFF
    local material = list[pick] and list[pick].material or 1
    local cost = cells(mask)

    local children = {
        { type = "label", text = "Shape crafting" },
    }
    if #names == 0 then
        children[#children + 1] = {
            type = "label", text = "Nothing loose to cut — dig something first.",
        }
        return { type = "container", direction = "column", gap = 6, children = children }
    end
    children[#children + 1] = {
        type = "dropdown", name = "material", options = names, selected = pick,
    }
    children[#children + 1] = {
        type = "shape_editor", name = "cut", shape = mask, material = material,
    }
    children[#children + 1] = {
        type = "label",
        text = cost == 0
            and "Chiselled away to nothing — right-click to start again."
            or ("One of these costs " .. cost .. " units."),
    }
    children[#children + 1] = {
        type = "container", direction = "row", gap = 6,
        children = {
            { type = "button", name = "make", text = "Make one" },
            { type = "button", name = "reset", text = "Reset" },
        },
    }
    return { type = "container", direction = "column", gap = 6, children = children }
end

--- The screen: the tabs, then whichever tab is showing.
local function screen(player)
    return {
        type = "container", direction = "column", gap = 6, padding = 10,
        children = {
            tabs(tab[player] or "tab_items"),
            (tab[player] == "tab_shapes") and shapes_tab(player) or items_tab(),
        },
    }
end

local function redraw(player)
    game.update_dialog{ player = player, form = "inventory", tree = screen(player) }
end

game.register_on_action(function(event)
    -- Presses only. Acting on the release as well would open the screen and
    -- close it again in the time it takes to let go of a key.
    if event.id ~= "core_ui:inventory" or not event.pressed then
        return
    end
    if open[event.player] then
        game.close_dialog{ player = event.player, form = "inventory" }
        open[event.player] = nil
    else
        game.show_dialog{ player = event.player, form = "inventory", tree = screen(event.player) }
        open[event.player] = true
    end
end)

game.register_on_dialog_event(function(event)
    if event.form ~= "core_ui:inventory" then
        return
    end
    -- A slot click is the SERVER's business and has already happened by the
    -- time this is called; there is nothing for a mod to do about it. What is
    -- tracked here is only whether the screen is on the player's display, so
    -- the key toggles rather than reopening something already open.
    if event.kind == "closed" then
        open[event.player] = nil
        return
    end

    if event.kind == "chiselled" then
        -- Not redrawn: the client is already showing this, and sending the
        -- mask straight back would fight the player's next click. What DOES
        -- need redrawing is the cost line, and that can wait for the next
        -- press rather than arriving mid-carve.
        cut[event.player] = event.shape
        return
    end

    if event.kind == "chose" and event.name == "material" then
        chosen[event.player] = event.index
        redraw(event.player)
        return
    end

    if event.kind ~= "pressed" then
        return
    end

    if event.name == "tab_items" or event.name == "tab_shapes" then
        tab[event.player] = event.name
        redraw(event.player)
    elseif event.name == "reset" then
        cut[event.player] = 0x7FFFFFF
        redraw(event.player)
    elseif event.name == "make" then
        local list = stock(event.player)
        local entry = list[chosen[event.player] or 1]
        local mask = cut[event.player] or 0x7FFFFFF
        local cost = cells(mask)
        -- A whole block is not a cut: it is what loose material already is, and
        -- letting it through would mean twenty-seven units and one "shaped"
        -- block of the same stone stopped stacking with each other.
        if not entry or cost == 0 or cost == 27 then
            redraw(event.player)
            return
        end
        local spent = game.take(event.player, { material = entry.material, units = cost })
        if spent < cost then
            -- Put back exactly what was taken. `game.take` reports the amount
            -- so a mod that cannot finish never has to guess.
            if spent > 0 then
                game.give(event.player, { material = entry.material, units = spent })
            end
        else
            game.give(event.player, { material = entry.material, shape = mask, count = 1 })
        end
        redraw(event.player)
    end
end)


-- The interface's own noise.
--
-- Bound rather than played: the client raises `engine:ui_click` when a widget
-- is actually pressed, because a click that waited for the server to agree it
-- had happened would arrive after the button had already moved.
game.register_sound{ id = "click", file = "sounds/click.wav", gain = 0.5 }

game.bind_sound("engine:ui_click", "click")
game.bind_sound("engine:ui_close", "click")
