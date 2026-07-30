# TASK 12 — Entity system, skeletal animation, and the Mimic

Depends on: 09, 10. ECS in `crates/core::ent` (evaluate standalone `bevy_ecs`, `hecs`, and a
minimal hand-rolled store — pick one, justify in the PR, keep the mod-facing API identical
either way. Weigh deterministic iteration order and dependency weight in `core` at least as
heavily as raw query speed: entity counts here are small, and charter rule 4 forbids
order-dependent results in simulation).
The Mimic lives ENTIRELY in `game/core_mimic/` — it is the mod-API acceptance test: if it
needs an engine change, fix the API, not the mob.

## Objective
Server-authoritative entities with interpolated client rendering and a skeletal humanoid rig,
plus the one default mob: a blank-white mimic of the first player to join the world.

## Design
### Engine mechanisms (core + client)
- Entity = id + components. Engine-defined components: Transform (chunk-anchored, floating-
  origin safe), Velocity, AABB collider (reuses core::phys — entities collide with voxels and
  swim in milk for free), ModelRef, AnimState, Health, Nametag, Owner. Mods attach arbitrary
  Lua-table components (serialized with the entity; opaque to the engine).
- Persistence: entities save/load with their chunk (Task 03 table); unloaded-chunk entities
  are frozen. Spawn/despawn lifecycle hooks.
- Replication: interest-managed EntityStateDelta (pos/vel/anim/quantized look) on the
  unreliable channel + reliable spawn/despawn/component-change events. Client interpolation
  buffer (~100 ms) — makes Task 08's stub real for all entities including other players.
- Skeletal animation: glTF loading (mesh + skeleton + clips) through the content pipeline
  (mods ship .glb files, content-addressed like textures). glTF is the largest hostile-input
  surface in the project (charter rule 14): pure-Rust `gltf` crate; pre-decode caps enforced
  before allocation (file size, mesh/node/joint counts, vertex/index counts, animation
  channels/samples, buffer sizes vs declared); reject external buffer/image URIs entirely
  (embedded-only .glb); accessor bounds validated against buffers before any indexed read;
  parse on a worker with panic isolation — a poisoned model renders as a fallback capsule
  with a per-server warning, never a crash. Add `fuzz/gltf_ingest` in THIS task, seeded with
  the shipped humanoid, wired into the CI fuzz smoke job. Client-side skinning
  (GPU, 4 weights). Engine ships ONE built-in humanoid rig + clips (idle/walk/run/swing/swim)
  as `engine:humanoid` — used for players AND available to mods. Player rendering: humanoid
  rig, untextured default = matte white (skins later; do not build a skin system now).
- Server-side animation is state-tags only (walk/idle/…); clients map tags to clips —
  server never touches animation math.
- Mod API: `game.spawn_entity{ model, pos, components }`, `game.despawn`, component get/set,
  queries (`game.entities_in_radius(pos, r, filter)`), per-entity tick callbacks
  (`on_step(self, dt)` with instruction budget), `on_punch` wiring from Task 09,
  perception helpers implemented natively for cheapness: `game.line_of_sight(a, b)`,
  `game.find_path(a, b, opts)` (simple voxel A* with step/jump rules; hard node budget per
  call; async result).
### The Mimic (`game/core_mimic/`, pure Lua)
- On first-ever player join (persisted flag in a mod-storage API — add
  `game.storage` per-mod persistent KV, stored in the world DB), record that player's
  identity as "the imprint" — stored as the player UUID (charter rule 13), never the name.
  The nametag renders the CURRENT display name bound to that UUID. A later player claiming
  the same name (impossible on one server per Task 06, but defensively) must not steal the
  imprint.
- One mimic exists in the world at a time. Appearance: `engine:humanoid`, forced untextured
  white, nametag = the imprinted player's name.
- Behavior (state machine in Lua, all through public API): wanders near its spawn; when the
  imprinted player is within 32 yards and in line of sight, follows at a distance and
  MIRRORS: replays the player's actions on ~2 s delay (movement path breadcrumbs, swing
  animation when the player swings, sneak when they sneak) — implement breadcrumb recording
  via a per-tick hook on the player. If punched: flees briefly, then resumes. If the
  imprinted player is offline: idles near last-seen position. Persists across restarts.
- Keep it eerie-simple; no combat, no damage dealt.
### Bot + tests upgrade
- `bot.expect_entity(filter, timeout)`, entity observation in the bot API.

## Tests
- ECS unit: component CRUD, chunk-freeze/thaw round-trip through persistence, deterministic
  iteration order in simulation-relevant queries.
- Replication integration: entity spawned server-side appears on two bots; interpolation
  under 150 ms + loss stays within error bounds; despawn on chunk unload/reload cycles.
- Pathfinding known-answers + node-budget cutoff test; line-of-sight table cases.
- Mimic end-to-end (the acceptance test): fresh world → bot A joins (imprint persisted —
  restart server, still imprinted) → mimic spawns, mirrors bot A's recorded walk with delay
  (assert breadcrumb positions within tolerance), ignores bot B, flee-on-punch works.
- Identity durability: bot A rotates its key (Task 06 pre-rotation) and rejoins; the imprint
  still resolves to the same player and the nametag follows the current display name. This is
  the cheapest possible proof that mod state keyed on UUID survives key churn.
- Perf: 200 scripted wandering entities in one area — tick budget respected (per-entity Lua
  budget enforcement demonstrated).

## Acceptance criteria
- [A] `game/core_mimic` implements everything with zero engine special-cases (grep the diff:
      no "mimic" string outside `game/`).
- [A] Mimic scenario test green; persistence of imprint across restarts proven.
- [A] Interpolation error bounds met in tests.
- [H] Other players and the mimic LOOK smooth in motion; the mimic reads eerie, not broken.
- [A] 200-entity perf test within budget on CI.
