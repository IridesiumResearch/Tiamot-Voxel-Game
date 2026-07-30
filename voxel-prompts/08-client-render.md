# TASK 08 — Client foundation: window, wgpu, meshing, floating origin, lighting mode 1

Depends on: 06, 07, and the Task 02b verdict (do not start without it). Code in
`crates/client`.

## Objective
First visible build: connect to a server (embedded by default), stream chunks, render the
half-white world with a free-fly camera in lighting mode 1. Ugly-but-correct beats pretty.

## Design
- `winit` window + `wgpu` (vulkan/dx12/metal auto). Init handles adapter loss gracefully with
  a clear error. Config file for the client (`client.toml`): server addr or `embedded`,
  view distance, render mode, vsync.
- Client net task (tokio) implementing the Task 06 join flow including the content cache
  (BLAKE3-addressed dir under the platform data dir). Received chunks go into a client-side
  chunk store (reuse core types).
- Floating origin per charter rule 7: camera-relative rendering; all mesh vertex positions
  are chunk-local; per-draw transform = f64 (chunk_pos − camera_chunk) × 16 computed on CPU
  into f32 offsets. Add a debug action to teleport ±50,000 blocks and verify no jitter.
  (Rendering is exempt from the charter rule 4 float subset — this is presentation.)
- **Meshing: binary greedy meshing**, promoted from the Task 02b prototype, on a worker pool
  (rayon or tokio-blocking). Occupancy per axis as `u64` columns — 48 sub-node cells plus 2
  padding bits for neighbour-face culling fits one word, which is why chunks are 16³ blocks
  (charter rule 6). Face culling by shift-and-AND across whole columns; quad merging by bit
  operations. A Uniform block contributes a solid 3×3×3 cell group — build the column masks
  from the palette without ever expanding a trivial chunk to a dense array. Neighbour-chunk
  face culling across borders. Remesh on BlockDelta with a dirty queue and per-frame remesh
  budget. Vertex format: position (chunk-local f32), normal id, material id, light attribute.
  - If Task 02b returned FALLBACK, this is block-resolution meshing over 16³ columns instead;
    everything else in this section stands unchanged.
  - Carry the 02b measurements forward as the regression baseline: mesh time per chunk and
    remesh-after-edit must not regress against the spike's recorded numbers.
- Lighting mode 1: vertex-colored directional face shading (top 1.0 / bottom 0.5 / x-sides
  0.75 / z-sides 0.85) — no light propagation data yet (that's Task 10; leave the vertex
  attribute in place, fed with 1.0).
- Texturing: runtime atlas builder — collect block textures registered by mods (extend
  `register_block` with `textures = { all = "white.png" }` and mod-dir asset loading through
  the content pipeline; `core:white` gets an actual 16×16 white texture with a faint border so
  block edges read). Nearest-neighbour sampling, mipmap chain with padded tiles at atlas
  edges.
- PNG ingest is hostile input (charter rule 14): decode with `image-png` (the image-rs PNG
  decoder — no unsafe code, fuzzed on OSS-Fuzz, and Chromium's PNG decoder since M139;
  no C bindings anywhere in the client asset path). Set `Limits` — max dimensions (e.g.
  1024²) and max decoded bytes — BEFORE decode, not after. Decode on a worker with panic
  isolation: a poisoned texture becomes the magenta-checker fallback with a per-server
  warning toast, never a crash. Add `fuzz/texture_ingest` in this task covering the full path
  (bytes → validated → atlas slot), wired into the CI fuzz smoke job.
- Camera: free-fly (WASD + mouse look, raw mouse input, sensitivity in config) — the real
  character controller is Task 09. Frustum culling of chunk meshes. Basic HUD text via `egui`:
  fps, position, chunk count, facing.
- Frame pacing: render loop decoupled from network; interpolation buffer for entity state
  (used from Task 12; stub now).

## Tests
- [A] Meshing unit tests headless: quad counts for known shapes (single block = 6 quads at
  sub-node scale merged to 6; two adjacent blocks share no interior faces; a Partial block
  with one sub-node removed produces the expected face set).
- [A] Binary mesher proptest: output surface area equals a reference naive mesher's surface
  area over random chunks — the bit-twiddling implementation is exactly the kind of code that
  needs a dumb oracle to check it.
- [A] Border correctness: two adjacent chunks meshed independently produce no duplicated or
  missing faces at the shared plane.
- [A] Screenshot smoke test on CI ubuntu (llvmpipe/lavapipe software vulkan): render one
  frame of a fixed scene to a texture, hash it, compare golden per driver id (per-driver
  goldens tolerated; this gate is about "did rendering silently break", keep it coarse).
- [A] Mesh-perf regression check against the Task 02b baseline numbers.
- [H] Manual checklist in PR description: 60 fps at view distance 8 on the dev machine;
  teleport jitter inspection.

## Acceptance criteria
- [H] `cargo run -p client` with default config opens a window, starts an embedded server,
      and you fly over an infinite white half-space with visible block-edge texture.
- [A] Connecting to a separately-launched `server` binary works with the same build
      (verifiable headless via the screenshot harness against an external server).
- [A] Block edits made by a bot appear live in the client (remesh path proven — assert via
      screenshot-harness frame hash change after bot edit).
- [H] No visible jitter at ±50,000 blocks (floating origin — camera-delta magnitude is [A]
      assertable in a unit test; the visual check is yours).
- [A] Meshing tests + border test + screenshot smoke + texture fuzz smoke pass in CI.
- [A] Mesh timings within the Task 02b baseline.
