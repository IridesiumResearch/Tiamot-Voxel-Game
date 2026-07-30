# TASK 09 — Player controller, voxel physics, punch/break/build, client prediction

Depends on: 08, and the Task 02b verdict. Physics in `crates/core::phys` (server-
authoritative, shared by client prediction). Interaction rules defined in Lua
(`game/core_tools/`).

If 02b returned FALLBACK, collision below is block-resolution and the chisel keeps only its
drop-accounting and carve-overlay roles; every other section stands unchanged. Carry 02b's
recorded collision cost forward as the regression baseline either way.

## Objective
Minecraft-grade first-person feel: smooth movement, swept-AABB collision, block targeting,
break/place through the mod API, with client-side prediction and server reconciliation.

## Design
- Physics (core, deterministic, fixed-tick): swept-AABB vs voxel grid at SUB-NODE resolution
  (a player collides correctly with a half-mined block). Player AABB 0.6×1.8×0.6 yards,
  eye height 1.62. Gravity, jump, step-up of one sub-node (1/3 yard), friction/air-control
  constants in one tuning module with doc comments. Walk/sprint/sneak (sneak = edge-safe:
  cannot walk off a block edge). No swimming yet (Task 11).
- Input pipeline: client samples input per render frame → quantized into per-tick
  `PlayerInput { tick, move_vec, look, buttons }` → sent unreliably with redundancy (last 3
  inputs per packet). Server applies inputs on their tick (small reorder buffer), simulates,
  sends authoritative `PlayerState { last_processed_input_tick, pos, vel, ... }`.
- Prediction & reconciliation (client): apply inputs locally immediately through THE SAME
  core physics code; keep an input history ring; on authoritative state, rewind to
  last_processed tick and replay pending inputs; smooth residual error over ~100 ms (snap if
  > 2 yards). Other players render from the interpolation buffer (Task 08 stub becomes real).
- Targeting: voxel raycast (Amanatides & Woo DDA) at sub-node resolution, reach 4.5 yards,
  returns block pos, sub-node pos, face normal. Client draws a selection wireframe honoring
  Partial occupancy (outline the actual occupied sub-node cells).
- Break/build through the mod API (this is the API acceptance test):
  - Extend `register_block` with `hardness` (seconds at bare hand) and `drops` override.
  - `game.register_tool{ id, brush, speed_multiplier, ... }` where `brush` defines removal
    shape: `"block"` (all sub-nodes) or `"subnode"` (the one hit) — extensible table format so
    mods can later define 3×3 columns etc.
  - Digging model: client sends start_dig(pos, face)/cancel; server tracks progress by
    hardness/tool; on completion applies removal, computes drops via Task 02
    `break_block`, inserts into player inventory (server-side inventory component:
    slots of `Stack`s; simple auto-merge), broadcasts. Progress ticks sent to the digger for
    the crack overlay.
  - Placement: place consumes 27 units for a full block (reject with feedback if < 27; spare-
    node placement of Partial blocks is allowed when holding < 27: fills sub-nodes bottom-up —
    document the fill order). Placement blocked if it intersects any player AABB.
  - Punch: left-click on entities → `game.register_on_punch(fn(attacker, target))` hook
    (used by Task 12).
  - Hooks: `on_dig_complete`, `on_place`, both cancellable by returning false.
- `game/core_tools/`: registers bare-hand defaults and a `core:chisel` tool with the
  `"subnode"` brush (this proves the brush API and gives subnode mining a face).
- Hotbar UI (egui for now): 9 slots, scroll/number selection, shows `blocks+spares` display
  format from core arithmetic. Server-authoritative inventory sync messages.
- Bot upgrade: `bot.move_to` now drives real inputs through pathless straight-line movement +
  jump heuristic; add `bot.start_dig/bot.stop_dig`. Update Task 07 scripts accordingly.

## Tests
- Physics unit: collision known-answer cases (land on floor exactly, slide along wall, step-up
  succeeds at 1 sub-node and fails at 2, sneak edge-guard). proptest: player never ends a tick
  intersecting solid geometry under random input sequences.
- Determinism: identical input logs ⇒ identical final state hash (this underwrites
  prediction correctness), cross-platform via CI.
- Prediction integration: bot with artificial 150 ms latency + 5% loss (add a network
  impairment option to the loopback harness) digs and builds a staircase; asserts final world
  state server-side; reconciliation error metrics logged and bounded.
- End-to-end: `mine_3x3.lua` now uses real digging with hardness timing; add
  `chisel_sculpt.lua` — chisel out 13 sub-nodes, assert inventory shows 0 blocks + 13 spares,
  place them back, assert the Partial geometry server-side.
- [H] Manual feel checklist (PR description): bunny-hop timing, sneak at edges, no camera
  stutter during reconciliation at 150 ms.

## Acceptance criteria
- [A] All physics/determinism/prediction tests green in CI.
- [A] Chisel scenario proves subnode brushes + spare-unit arithmetic end to end over the network.
- [A] Digging/placing rules demonstrably live in `game/` Lua (delete the mod dir ⇒ you can no
      longer dig — verify in a test).
- [H] Movement FEELS like Minecraft-grade first person (your judgment; the checklist above
      is the rubric).
