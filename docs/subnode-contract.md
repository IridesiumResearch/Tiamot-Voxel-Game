<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The Sub-Node Contract

**Authoritative. Charter rule 12.** This document defines how every system in
the engine treats `Uniform`, `Partial`, and `Mixed` blocks. Any change to
sub-node semantics requires editing this document **first**, and any pull
request touching collision, lighting, fluid, meshing, worldgen, or pathfinding
cites the contract line it implements.

The point of having one authoritative page is that sub-node semantics are
cross-cutting: nine systems each make a decision about what "half a block"
means, and if those decisions are made independently they will not agree.
Disagreements between them are not cosmetic — they produce blocks you can see
through but not walk through, or mine for material that does not exist.

Created by Task 02b. **Verdict: KEEP** — full sub-node resolution, no cap, no
degradation path (Iridesium, 2026-07-30). Measurements and reasoning in
[`subnode-verdict.md`](subnode-verdict.md); performance budgets every system
here is held to in [`performance-targets.md`](performance-targets.md).

---

## 0. The three storage forms

Defined in `crates/core/src/block.rs`, which is also the single authoritative
statement of the sub-node index convention.

| Form | Meaning |
|---|---|
| `Uniform(material)` | All 27 sub-nodes are `material`. Includes a block of pure air. |
| `Partial { material, occupancy }` | One material occupying the sub-nodes set in a 27-bit mask; air elsewhere. |
| `Mixed(slot)` | Two or more distinct materials; the 27 cells live in a chunk-local table. |

**Canonical form is an invariant.** A `Partial` with a full mask is stored as
`Uniform`; with an empty mask, as `Uniform(AIR)`; a `Mixed` holding one distinct
material collapses to `Uniform` or `Partial`. Every write canonicalises. This is
not tidiness — without it, one world state would have several representations
and the cross-platform determinism hash would depend on the order blocks were
written.

**Sub-node index convention.** `index = x + 3*y + 9*z`, each of `x, y, z` in
`0..3`, index 0 at the `(0,0,0)` corner and 26 at `(2,2,2)`. Bit `index` of an
occupancy mask refers to the same cell. Stated once in
`block::subnode_index`; everything else references it.

**Occupancy means "not air".** There is no separate solidity flag. A sub-node is
occupied iff its material is not `MaterialId::AIR`.

---

## 1. The `u64`-column invariant

**This is the most important consequence of the 16³-block chunk size, and the
reason that number is not a free variable.**

A chunk is 16 blocks per axis, therefore **48 sub-node cells** per axis. Binary
greedy meshing represents a whole column of cells as a bitmask in one machine
word and culls faces with a shift and an AND across the entire column at once.
Face culling needs to know about the neighbouring cell just outside the chunk at
each end, so a column needs **48 + 2 padding = 50 bits — one `u64`.**

A 32³-block chunk would be 96 cells per axis and need 98 bits. Every column
operation would become a multi-word sequence with carries, and the technique
would be lost. The chunk size was chosen to make this fit.

Column bit layout: bit 0 is the neighbour at −1, bits 1..=48 are the chunk's own
cells, bit 49 is the neighbour at +48.

**Do not change `CHUNK_BLOCKS` without redesigning the mesher.** A compile-time
assertion in `crates/core/src/lib.rs` fails the build if this invariant is
broken, at the constant that explains why.

---

## 2. Collision — sub-node resolution

Collision is solid at sub-node resolution. A half-mined block is climbable and
enterable; the shape you see is the shape you collide with.

- A sub-node cell is solid iff occupied (not air). `Uniform`, `Partial`, and
  `Mixed` are all treated identically — only the per-cell occupancy matters, not
  which storage form holds it.
- **Step-up height is one sub-node (1/3 yard).** A body blocked horizontally
  retries the move one sub-node higher; if that is clear and it was on the
  ground, it steps. A two-sub-node lip stops it.
- Movement resolves one axis at a time (X, then Y, then Z), which is what makes
  a body slide along a wall rather than stick to it.
- **A body must never be left inside geometry.** This is the invariant that
  outranks every performance concern in Task 09.

Task 09 implements this. Task 02b's prototype measured 0.0136 ms/tick for 100
bodies.

---

## 3. Lighting — block resolution, sub-node permeability test

Light levels are stored **per block**, not per sub-node. Sub-nodes affect only
whether light crosses a face.

**The rule:** light passes a face iff that face's 3×3 sub-node layer is **not
fully occupied**.

- The test looks only at the 9 cells adjacent to that face, not the whole block.
  A block hollowed out in the middle but sealed on every side is correctly
  opaque.
- Light must be able to leave one block and enter its neighbour: both facing
  layers are tested.
- `Uniform(AIR)` is permeable on all six faces; any other `Uniform` is opaque on
  all six.

**Task 10 must cache a per-block permeability byte, computed on write.** This is
not a suggestion — Task 02b measured the uncached test at **≈50% overhead** on a
chiselled chunk against a treat-Partial-as-solid baseline, failing that gate.
The remedy is six bits per block, recomputed only when the block changes.

---

## 4. Fluid — block resolution

Fluid is block-resolution and does not otherwise interact with sub-nodes.

- A block accepts fluid iff its occupancy is **empty** — that is, iff it is
  `Uniform(AIR)`.
- `Partial` and `Mixed` blocks are **fluid-solid**: they neither hold nor pass
  fluid, however little of them is occupied.

This is deliberately cruder than collision. Modelling partial fluid volumes at
sub-node resolution would multiply fluid state by 27 for a visual effect nobody
asked for. Task 11 is block-resolution throughout and needs no sub-node
awareness beyond "is this block empty".

---

## 5. Worldgen — block resolution by default, sub-node opt-in

Generators write at **block resolution** by default. Sub-node detail is opt-in
per generator.

This caps the 27× generation cost to the mods that actually ask for it. A
generator that never opts in pays nothing for sub-nodes existing. Task 04's
`ChunkBuffer` implements the lazy expansion that makes this real: the buffer
stays block-resolution until a generator writes a sub-node, and only then
expands.

All worldgen randomness comes from engine-provided seeded noise and per-chunk RNG
streams (charter rule 4). Sub-node detail does not change that.

---

## 6. Pathfinding — block resolution

Navigation is **block resolution**. Deliberately dead simple:

- A `Partial` or `Mixed` block is an obstacle **unless its bottom sub-node layer
  (the 9 cells at `y == 0`) is empty**, in which case it is walkable-through.
- No sub-node pathfinding. No partial-cost traversal.

Entities may therefore fail to path through gaps a player can squeeze into. That
is an accepted limitation, not a bug: sub-node pathfinding would multiply the
search space by 27 for marginal navigational benefit. Task 12 implements this.

---

## 7. Placement and support — no structural simulation

**Any occupancy configuration is legal.** Floating sub-nodes are allowed.
Nothing falls, nothing collapses, nothing checks for support.

A mod that wants structural rules implements them through the mod API. The
engine has no opinion.

### 7.1 Placement resolution — the acting tool's brush decides

**A placement writes at the resolution its brush addresses, symmetrically with
digging (§2, and `dig::Brush`).** The engine has no placement resolution of its
own; the tool the player holds carries one, and a mod says which.

- **A sub-node brush fills the cell that was aimed at**, one unit, and nothing
  else. The client names a cell, and that cell is what is written.
- **A block brush fills the containing block from the bottom up**, up to 27
  units, per `inventory::placement_mask`. Here the named cell selects the
  *block* and not the fill: the order is fixed so that identical actions produce
  identical geometry regardless of where the player was looking.
- **A world with no tools still places, at block resolution.** Digging refuses
  without a mod-registered tool because the engine has no rule of its own for
  breaking things; placing has no such rule to be missing, and refusing it would
  let a mod set strand an inventory with no way to spend it.

**Occupancy is judged per cell, never per block.** A placement is refused if any
cell it would fill is already occupied — so a chiselled block's empty cells can
be filled, and a whole-block placement into a block with anything in it is
refused because its cells overlap.

Together these are what make carving **reversible**: a cell taken out of a block
can be put back into the same cell of the same block. Without either half — a
fill anchored to the block's bottom, or a refusal that looked at the whole block
— sub-node resolution would exist only for removal.

Task 09 implements this; `crates/core/src/place.rs` is the implementation and
`a_chiselled_cell_goes_back_into_the_cell_it_came_out_of` is the test.

---

## 8. Rendering — sub-node resolution, binary greedy meshing

Meshing is at sub-node resolution using **binary greedy meshing**, per §1.

- Quads must not merge across a material boundary. A merged quad spans one
  material only.
- Faces are culled against the neighbouring cell, including across chunk
  boundaries via the two padding bits.
- Positions quantise to 6 bits per axis (`0..=48`), giving an 8-byte vertex.

Task 08 implements this. Task 02b's prototype measured 0.110 ms/chunk on
realistic content and 0.128 ms on fully chiselled content.

---

## 9. Inventory — 27-unit arithmetic, no exceptions

Charter rule 5, implemented in Task 02. Quantities are stored in units as `u32`;
display is `units / 27` blocks plus `units % 27` nodes.

Breaking a block yields:

- `Uniform` of a solid material → 27 units of it
- `Uniform(AIR)` → nothing
- `Partial` → one unit per set occupancy bit
- `Mixed` → one stack per distinct non-air material, each with its cell count

**Output order is ascending `MaterialId`, always.** Drop order is observable — it
decides which stack an almost-full inventory keeps — so it must not depend on
cell iteration order or anything else that could differ between machines running
the same simulation.

---

## 10. Networking — what a sub-node edit costs on the wire

Measured in Task 02b, Deliverable 5. A minute of *continuous* chiselling
(1105 edits at 20 tps, one per tick with no pause):

| Encoding | Raw | zstd |
|---|---|---|
| Dedicated sub-node delta (block, cell, material — 5 bytes) | 5.40 KiB/min | **2.79 KiB/min** |
| Block path (resend the block's 27 cells — 56 bytes) | 60.43 KiB/min | **4.92 KiB/min** |

**Finding: a dedicated sub-node delta opcode is not required.** Raw, the compact
encoding is 11× smaller; compressed, the gap collapses to 1.8×, and both are far
inside the 32 KiB/min/player budget. Task 06 may ride the ordinary block path
without a separate sub-node opcode. The compact encoding remains available if
per-message overhead later proves to matter more than stream size.

Chunk transfer, zstd-compressed, measured at level 3:

| Scene | Compressed |
|---|---|
| Uniform | 157 B |
| Realistic (95/4/1) | 355 B |
| Fully chiselled surface | 1,797 B |
| Mixed checkerboard | 122 B |

These are the numbers Task 03's persistence budget should be set against. The
chiselled figure of 1.8 KiB is the one to design for; Task 03's "uniform chunk
≤ 100 bytes" target is not met by this spike's deliberately naive encoding
(157 B), which is expected — the real format will pack the palette properly.

---

## Cross-references

- Charter rules 4, 5, 6, 12 — `CLAUDE.md`
- Sub-node index convention — `crates/core/src/block.rs`
- `u64`-column compile-time assertion — `crates/core/src/lib.rs`
- Measurements and verdict — [`subnode-verdict.md`](subnode-verdict.md)
- Spike source — `spikes/subnode/`
