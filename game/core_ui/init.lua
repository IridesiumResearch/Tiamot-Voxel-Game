-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: GPL-3.0-only
--
-- The reference HUD, and the proof that a HUD is a mod's.
--
-- Delete this directory and a client keeps a crosshair, chat and the settings
-- screen — and loses the hotbar, the dig readout and the target line entirely.
-- That is Task 14's first acceptance criterion, and `core_ui_owns_the_hud` in
-- `crates/bot/tests/hud.rs` is the test that will not let it quietly stop being
-- true.
--
-- Everything here is drawn by `hud.lua`, which the client runs in a sandbox.
-- This file's only job is to say so.

game.register_hud_script("hud.lua")
