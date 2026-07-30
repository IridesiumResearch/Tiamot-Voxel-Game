# TASK 02 — Voxel data model: blocks, sub-nodes, palettes, chunks, unit arithmetic

Depends on: 01. All code in `crates/core`.

## Objective
Implement the in-memory world representation and the 27-unit inventory arithmetic. This is the
foundation every other system reads and writes; correctness and compactness matter more than
features.

## Design (implement exactly)
- `MaterialId(u16)` runtime id. `0` reserved = air. A `Registry` stub maps `String ⇄ MaterialId`
  (full registry semantics arrive in Task 05; keep the trait surface minimal so it can be
  swapped in).
- Block content enum, storage-tagged:
  - `Uniform(MaterialId)` — whole block one material (includes air).
  - `Partial { material: MaterialId, occupancy: u32 }` — single material, 27-bit occupancy
    mask over sub-node positions. Canonical sub-node indexing: `idx = x + 3*y + 9*z`,
    x/y/z ∈ {0,1,2}, document it.
  - `Mixed(SlotIndex)` — index into a chunk-local side table of `[MaterialId; 27]`.
- Canonicalization invariants enforced on every write:
  - Partial with full mask → Uniform. Partial with empty mask → Uniform(air).
  - Mixed with all-same materials → collapses to Uniform/Partial. Unused Mixed slots reclaimed.
- `Chunk`: 16³ blocks, palette-compressed. Per-chunk palette of block-content entries,
  bit-packed indices sized to palette (1 entry ⇒ zero index storage; ≤2 ⇒ 1 bit; ≤4 ⇒ 2 bits; …).
  API: `get_block`, `set_block`, `get_subnode(world-relative subnode coords)`, `set_subnode`,
  `fill_region`, iteration, `is_uniform() -> Option<MaterialId>`, memory_usage().
- `ChunkPos(i32,i32,i32)`, `BlockPos`, `SubNodePos` newtypes with conversions and the
  world-size bound (±60_000 blocks from origin) checked.
- Inventory arithmetic module:
  - `Stack { material: MaterialId, units: u32 }`.
  - `break_block(content) -> Vec<Stack>`: Uniform ⇒ 27 units; Partial ⇒ popcount(mask) units;
    Mixed ⇒ one stack per distinct material with its count. Deterministic output order
    (ascending MaterialId).
  - `display(units) -> (blocks, spare_nodes)` = (units/27, units%27).
  - Stack merge/split helpers with overflow safety.

## Tests (required)
- Unit: canonicalization rules; palette grows/shrinks correctly; subnode get/set round-trip
  across all 27 positions; boundary coords.
- `proptest`: (a) any sequence of set_subnode/set_block ops, then read-back equals a naive
  27-array reference model; (b) break_block unit totals always equal occupied sub-node count;
  (c) palette repack is content-identity.
- Memory assertions: a uniform chunk ≤ 64 bytes of content storage; document measured sizes
  for representative mixed chunks in a comment.
- `criterion` benches: set_subnode hot loop, full-chunk fill, palette repack. Add
  `benches/` now; CI runs them in smoke mode (no regression gate yet).

## Acceptance criteria
- [A] All tests pass; proptest reference-model equivalence holds at 10k cases.
- [A] Uniform chunk memory bound met.
- [A] Public API documented; sub-node index convention documented in one place and referenced.
