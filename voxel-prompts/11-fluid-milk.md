# TASK 11 — Fluid: milk. Simple Minecraft-style flow, done well

Depends on: 09, 10. Solver in `crates/core::fluid`; registered from `game/core_milk/`;
smooth-surface rendering in `crates/client`.

## Scope decision (settled — read first, do not re-open)
We deliberately DO NOT build a conserved / pressure-equalizing fluid sim. Milk uses the
classic Minecraft model — source blocks + finite-distance flow decay, no conservation — with
the presentation and feel upgrades of the well-known "better water" mods: smooth surfaces,
real flow direction, good swimming. This keeps fluid at BLOCK resolution (not sub-node),
which removes fluid entirely from the sub-node risk surface. A fancier sim can be a future
mod once the WASM tier exists; the engine API below must not preclude that (levels are data,
the update rule is swappable).

## Design
### Simulation (core, block resolution)
- Fluid state per BLOCK: 4 bits = level 0–7 + source flag. Stored as a sparse per-chunk
  fluid layer (no milk ⇒ zero bytes; RLE within chunks).
- Update rule (fixed order, deterministic):
  - A source block sustains level 7 and spreads.
  - Flow-down: any fluid above a fluid-accepting block becomes falling flow (level 7 column).
  - Lateral: level `n` spreads to open orthogonal neighbors at `n-1`, min level 1; standard
    Minecraft shortest-path-to-drop preference (flows toward nearby holes within 4 blocks).
  - Decay: flow blocks with no valid parent (source or higher neighbor) drain by 1 level per
    fluid tick until gone. Two adjacent sources do NOT create new sources (no infinite-milk
    duplication; keep it simple, revisit as game design later in `game/`).
- Scheduling: fluid ticks at TICK_HZ/2; only blocks touched by an edit or an active flow are
  queued (active set); settled milk costs zero. Per-tick processing cap with carry-over.
- Partial blocks (sub-node) interaction rule — keep it to ONE sentence of semantics, added to
  the Sub-Node Contract (Task 02b): a block accepts fluid iff its occupancy is empty; Partial
  and Mixed blocks are fluid-solid. No fluid inside partially-mined blocks, period.
- Player interaction: swimming in `core::phys` — buoyancy proportional to submerged fraction
  of the AABB (computed from block levels), drag, jump = rise / sneak = sink, reduced
  walk speed in shallow milk. Fall damage cancelled by ≥2 deep milk (hook for mods).
- Mod API: `game.register_fluid{ id, color, flow_range, tick_rate, texture, sounds }`
  (engine supports N registered fluids; ship one), `game.set_fluid(pos, level, is_source)`,
  `game.get_fluid(pos)`, `on_fluid_flow(pos)` hook (budgeted). `game/core_milk/` registers
  milk + a creative source block + scoop/pour on the chisel for testing.
- Network: fluid deltas ride the existing BlockDelta path (fluid layer change = delta);
  periodic per-chunk fluid keyframes for late joiners / loss recovery.

### Rendering (client)
- Smooth surface: per-column height from level (level 7 ≈ 0.9 blocks), corner heights
  averaged with neighbors — no blocky stair-stepping. Flowing blocks get UV scroll along the
  computed flow direction vector; still milk gets a slow ripple. Milk renders opaque-white
  with slight tint (alpha sorting stays trivial); underwater overlay tint + fog when the
  camera is submerged.
- Mode 3: screen-space reflection on milk surfaces via the Task 10 render-graph slot
  (unchanged from the original plan).

## Tests
- [A] Determinism: fixed scenarios (spring on a slope, pool fill, drained channel) hashed
  after K fluid ticks, added to the cross-platform CI gate; chunk-update-order shuffle ⇒
  identical result (border correctness at block resolution).
- [A] Behavior known-answers: spread range exactly `flow_range`; hole-seeking chooses the
  correct direction table-driven; removing a source drains completely (assert no orphan flow
  blocks remain, and active set returns to empty).
- [A] proptest: after any edit sequence + settling, every flow block has a valid parent chain
  to a source (no floating milk), and fluid never occupies a non-empty-occupancy block.
- [A] Multiplayer: bot places a source, second bot's keyframe-recovery path exercised under
  forced loss; final client fluid layer hash matches server.
- [A] Perf: worst-case spring field (100 sources) — cells/sec recorded, tick budget held via
  the processing cap; settled world profiles at zero fluid cost (assert empty active set).
- [H] Manual feel checklist in PR: swimming (bob at surface, sink/rise response), waterfall
  look, flow direction reads correctly, underwater tint acceptable in all 3 render modes.

## Acceptance criteria
- [A] All simulation, determinism, and multiplayer tests green in CI.
- [A] Settled-world zero-cost assertion holds; processing cap prevents tick overrun in the
      spring-field scenario.
- [A] Fluid semantics for partial blocks enforced and covered by proptest.
- [H] Milk looks and feels like "Minecraft water but nicer" — smooth surface, directional
      flow, decent swimming — verified by you in-game.
