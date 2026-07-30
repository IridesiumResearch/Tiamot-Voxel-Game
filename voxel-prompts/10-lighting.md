# TASK 10 — Light propagation + lighting modes 2 and 3, day/night sky

Depends on: 08, 09. Propagation in `crates/core::light` (server + client shared);
render modes in `crates/client`.

## Objective
One lighting data model (simulation-authoritative), three client presentation modes.
Modes are settings, not forks.

## Design
### Data + propagation (core)
- Per BLOCK (not sub-node): 4 bits sunlight + 4 bits per R,G,B block light = u16, stored as a
  per-chunk light layer (palette/RLE-compressed like block data; usually near-uniform).
- BFS flood fill: sunlight column seeding (full-strength downward propagation, attenuated
  lateral), colored block light from emissive blocks (`register_block` gains
  `light_emit = {r,g,b}` 0–15). Incremental add/remove on block change (standard two-queue
  removal algorithm). Partial blocks: light passes if the face's 3×3 sub-node layer is not
  fully occupied (cheap mask test; document — this is the Sub-Node Contract's lighting line).
  Task 02b measured this test's cost inside the BFS; if it recommended a cached per-block
  "any face permeable" bit, implement that here rather than re-deriving the mask per visit.
- Runs server-side (authoritative — expose `game.get_light(pos)` for mods: spawning rules
  etc.) AND client-side on received deltas so remeshing never waits on the network.
  Same code, both places.
- proptest invariants: relight(chunk) from scratch equals incremental result after random
  edit sequences; light values never exceed source bounds; removal leaves no orphan light.
### Mode 2 — Classic (client)
- Smooth lighting: per-vertex light = average of the 4 adjacent block light values
  (sampled at sub-node mesh vertices from block-resolution data, interpolated).
- Voxel AO: classic 3-neighbor corner darkening baked into vertex color at mesh time.
- Colored light: RGB channels through the same interpolation; sunlight channel multiplied by
  time-of-day sun color.
- Distance fog colored by sky color.
### Mode 3 — Beautiful (client, stylized not PBR)
- Cascaded shadow maps (3 cascades) from the sun with PCF soft edges; blend shadow with the
  stored sunlight channel (stored light still gates caves — shadow maps alone must not).
- Bloom (threshold + separable blur) driven by emissive blocks.
- Depth-aware fog with sun-tinted scattering approximation; simple color grading LUT per
  time-of-day keyframes.
- All post passes behind a small render-graph abstraction so Task 11 SSR can slot in.
### Day/night (mod-driven)
- `game/core_sky/`: registers a time-of-day cycle (`game.set_time_of_day`, tick hook
  advancing it; length in config), sun/moon direction, sky gradient colors, star field
  (static procedural stars from world seed — rendered as a skybox point layer; these may
  become real destinations in Task 15c, so derive them via `detgen` from the seed NOW with a
  stable catalog function `star_catalog(seed) -> Vec<StarRecord>` placed in core — one shared
  source means the sky and any future map cannot desync).
- Server owns time of day (synced in EntityStateDelta stream or a dedicated message);
  sunlight re-seeding NOT recomputed per tick — sunlight layer stores full-daylight values,
  and the client scales by sun intensity (standard trick; document).
### Settings
- Render mode selectable live in the settings UI (1/2/3) + client.toml; mode 1 must remain
  exactly Task 08's cost profile (no shadow/post allocations when in mode 1).

## Tests
- Core light unit + proptest suites above; golden light fingerprints in the determinism gate
  (light layer hash for fixed scenes, cross-platform).
- Mesh-light integration: vertex light values for a known scene (single lamp in a room)
  match hand-computed values in a table-driven test.
- Screenshot smoke goldens per mode (extend Task 08 harness to 3 scenes × 3 modes; coarse
  hash tolerance).
- Bench: full-chunk relight; incremental relight after single edit; CI budget documented.
  Load test: swarm bots placing/removing lamps — no tick over budget from light updates
  (batch relights per tick with a cap; spillover deferred).

## Acceptance criteria
- [A] Light VALUES correct: caves dark, lamps colored, sunlight scaling — asserted via core
      light tests and screenshot-hash scenes.
- [H] It looks GOOD in all 3 modes — mode 3 reads as 'lit diorama', not noise (your eye).
- [A] Mode switch at runtime with no restart; mode 1 perf unchanged vs Task 08 baseline.
- [A] Light propagation deterministic cross-platform (CI gate green).
- [A] Sky content (cycle length, colors, stars) demonstrably lives in `game/core_sky` Lua.
