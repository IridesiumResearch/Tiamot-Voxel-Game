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

-- Who has it open. A UUID, never a display name (charter rule 13) — a player
-- who changes their name on another server must not lose their open screen.
local open = {}

--- The screen itself.
---
--- **Two views, not two halves of one.** `player:main` is where dug material
--- lands; `player:hotbar` is the nine slots the number keys select between, and
--- shift-clicking moves a stack between them. An earlier version of this drew
--- both grids over `player:main` starting at slot 10 — which showed twenty-seven
--- empty boxes and nothing a player owned, because that is not where their
--- items are.
local function screen()
    return {
        type = "container", direction = "column", gap = 6, padding = 10,
        children = {
            { type = "label", text = "Inventory" },
            { type = "item_grid", view = "player:main", columns = 9, first = 1, count = 27 },
            { type = "spacer", size = 8 },
            { type = "label", text = "Hotbar" },
            { type = "item_grid", view = "player:hotbar", columns = 9, first = 1, count = 9 },
        },
    }
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
        game.show_dialog{ player = event.player, form = "inventory", tree = screen() }
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
