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
--- Two grids over one view: the top three rows are the main inventory, the
--- bottom row is what the hotbar shows. Splitting them is presentation — the
--- server holds one list of slots and does not know this dialog exists.
local function screen()
    return {
        type = "container", direction = "column", gap = 6, padding = 10,
        children = {
            { type = "label", text = "Inventory" },
            { type = "item_grid", view = "player:main", columns = 9, first = 10, count = 27 },
            { type = "spacer", size = 6 },
            { type = "item_grid", view = "player:main", columns = 9, first = 1, count = 9 },
            { type = "button", name = "close", text = "Close" },
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
    if event.kind == "closed" or (event.kind == "pressed" and event.name == "close") then
        if event.kind == "pressed" then
            game.close_dialog{ player = event.player, form = "inventory" }
        end
        open[event.player] = nil
    end
end)
