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
- **Step-down is the same height, and is not optional.** A body that began its
  tick on the ground, is not rising, and would end it airborne looks one
  sub-node below its feet; if there is ground there it is placed on it and stays
  on the ground. A drop of more than one sub-node is a fall and is left alone.
- **A body strides over a RUT narrower than its own footprint — never over a
  hole.** A rut has its floor within one sub-node of the feet; anything deeper is
  a hole and is fallen into. Only once the drop is known to be shallow does the
  body look one footprint ahead along the way it is moving, and stay at its
  current height if there is ground there.

  **Depth is the test, and width cannot be.** A one-block hole is three cells
  across and so is the gap between two rubble lips; the first version of this
  rule looked only for ground ahead, and since it probes with a footprint of its
  own it could see support 2.7 cells past the body's centre. A player who dug
  straight down two blocks then walked over the top of the hole.

  Without this, a body crossing chiselled ground **fell a whole sub-node and
  climbed straight back out on the very next tick** — a 30 cm spike lasting 50
  ms, once per gap, which is what a floor of scattered sub-node lips is made of.
  Measured over random rubble: five such spikes in forty ticks. A foot 1.8 cells
  wide does not fall into a crack narrower than itself, and this is the rule that
  says so.

  It cannot make a body hover off a ledge: the look is **ahead**, in the
  direction of travel, so at a real edge there is nothing to stride to and the
  body falls exactly as before. Only a gap with ground on its far side is
  bridged.

  This is the mirror of step-up and the two only make sense together. Without
  it, sub-node terrain is not walkable in practice: a body skims the tops of
  raised cells, drops through the gaps between them, and while airborne it both
  loses ground acceleration — a fifteenth of the grounded figure — and gets
  stopped by the side of the next cell it meets, which it cannot step over until
  it lands. Measured before this rule existed, walking over cells raised every
  third cell: **forward motion froze for three ticks at a time and the body
  bobbed a full sub-node**, once per gap. Reported from the window as "when
  walking over single subnodes I also glitch; when walking around full blocks on
  the surface I am fine" — full blocks are three sub-nodes and are never stepped
  at all, which is why they behaved.
- Movement resolves one axis at a time (X, then Y, then Z), which is what makes
  a body slide along a wall rather than stick to it.
- **Movement must never put a body inside geometry.** This is the invariant that
  outranks every performance concern in Task 09. It is a rule about what the
  *sweeps* may leave behind, and it is unchanged.
- **A body that BEGINS a tick inside geometry stays there.** *(Amended
  2026-08-16, superseding "is eased out of it".)* The rule above is about what
  movement may leave behind; this is about what the world may do to a body that
  was standing still — a block placed where it stands, a chunk arriving around
  it, a mod rewriting the ground under it.

  The engine used to ease such a body out along the shortest axis, one sub-node
  per tick. That is withdrawn, for two reasons that are about the rule rather
  than its implementation.

  **It could not be made to answer.** Every case where an escape would have
  helped is a case where the cells cannot say which way out is real. A body at a
  chunk boundary reaches into the chunk across it and an absent chunk reads as
  solid, so a boundary whose neighbour has not arrived is indistinguishable from
  a wall — and pushing on that guess cost players their position, reported from
  the window as chunk boundaries having their own collision. A body inside an
  unloaded chunk reads as buried in every direction. A body genuinely entombed
  has no shortest way out that the cells it touches can supply. The pass ended up
  refusing all three, which is most of what a player actually meets.

  **And being stuck is supposed to be bad.** A body squeezed into geometry should
  suffer for it and eventually die, which is what every game with this problem
  does, rather than being quietly relocated by the engine. The damage rule is not
  written yet — health is an entity property and entities are Task 12 — so what
  §2 guarantees for now is the *mechanism* it will read: a body whose volume
  overlaps solid cells is left overlapping them, visibly, every tick.

  **A stuck body can still walk out.** The sweep tests only the cells a leading
  face would ENTER, never the ones a body already occupies, so a step toward open
  air is unobstructed and a step deeper is refused. That is what makes this
  survivable rather than a soft-lock, and it is a property of the sweep rather
  than a special case: no code anywhere asks whether the body started inside.

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

**As implemented (Task 10), the cache lives on the palette entry rather than on
the block.** Permeability is a pure function of block content, and a chunk
already stores each distinct content exactly once, so a uniform chunk caches one
value instead of 4,096 — and lighting pays no extra indirection, because
resolving a block to its palette entry is how it reads the block at all.
`Chunk::faces` is the accessor; `light::permeability` is the rule it caches.

The trade is stated rather than hidden: four bytes per palette entry after
alignment, so a saturated 4,096-entry palette costs 16 KiB where a per-block
byte would have cost 4 KiB. Every realistic chunk is far cheaper and the
saturated case is not a shape terrain produces. A stale cache would leak light
through solid rock hours from the edit that caused it, so
`the_cached_permeability_survives_an_arbitrary_edit_sequence` checks every block
against the uncached function after edits and after a repack.

**Two rules the face test alone does not decide, settled in Task 10:**

- **An emitter's light ignores its own faces and respects its neighbours'.** A
  lamp is usually a full block, which the rule above makes opaque on all six
  faces, so its own glow would be sealed inside it and `light_emit` would only
  work on blocks somebody had chiselled. A block glows on its surface rather
  than in its middle. A lamp walled in on every side still lights nothing.
- **Removal walks out of a block regardless of that block's new faces.** The
  commonest edit is a block becoming solid, and testing its new state finds
  every face shut — the light it used to pass would stay where it was, leaving
  a shaft lit under a roof that was just placed.

Both live in `crates/core/src/light/propagate.rs` and are shared by the full and
incremental paths, so the property test holding those two equal covers them.

---

## 4. Fluid — block resolution, volume in cells of 27

Fluid is block-resolution: **one number per block**, never a per-cell mask. That
number is a **volume in cells of 27** — the same unit as everything else in the
engine (charter rule 5) — and it is **conserved**.

- A block's **capacity** is `27 − occupancy`. A block one third full of stone
  holds one third less fluid, and a block at or above the registering fluid's
  `waterlogs_at` threshold has no usable capacity at all: it is **fluid-solid**,
  neither holding nor passing fluid. A mod may swap a fluid-solid block for a
  different one through `on_fluid_flow` if it wants waterlogging.
- Volume moves between blocks. It is never created. It leaves the world only
  through a **declared sink**, and every sink is counted (§4.3).
- Sub-node occupancy is read for exactly two purposes: computing capacity, and
  deciding whether a block is floor. Nothing else about the lattice is
  consulted.

### 4.1 Why cells of 27, and what it retires

This section used to run levels `1..=7`, Minecraft's number, and carried two
apologies for it. Both are now gone.

**The volume lie is retired.** The old text said, on purpose, that "a block that
is a third full of stone still holds a whole level of fluid… this is wrong and
is deliberately not corrected." That was defensible while fluid was
unconserved — nothing could measure the discrepancy. **Conservation makes it
observable**: buckets measure volume, so a player can pour a bucket into
chiselled ground and get more back out. Capacity of `27 − occupancy` is the
correction, and it costs nothing, because occupancy is already computed for the
floor test on the same block in the same visit.

**The `24/7` conversion is retired.** Levels had to be converted into
twenty-sevenths for the mesher's surface height and the physics' submerged
fraction, both of which speak in cells. Volume in cells *is* that number, so the
conversion is an identity and the bridging method is gone.

**This does not make fluid sub-node resolution.** There is still exactly one
volume per block and nothing writes a partial cell mask for fluid. The unit
changed; the resolution did not. The 27× state cost the scope decision ruled out
is not reopened by this section.

### 4.2 The update rule

Conserved, and applied to one block at a time in a fixed order. Stop early when
the block empties.

1. **Down first.** Move as much volume as the block below will accept.
2. **Sideways.** For each horizontal neighbour that can accept fluid and holds
   less than this block currently holds, lowest-holding first,
   `transfer = (mine − theirs) / 2` in integer arithmetic, recomputing `mine`
   after each transfer so a block can never give away more than it has. A
   difference of one produces a transfer of zero, so **water settles without a
   separate stability test** — that is the property that makes this terminate.
3. **Stuck droplets.** A block holding one or two cells cannot split, so on a
   slope it would leave permanent streaks. If a horizontal neighbour is empty
   and the block beneath *that* neighbour is not full, move the whole volume
   there.
4. **Absorption** (§4.3).

**Direction order is derived from the block's own coordinates, never from the
tick counter.** A tick-derived rotation has to be persisted or a reloaded world
diverges from a fresh one, and it makes every block in the world favour the same
side on the same tick, which reads as a pulse across a large pond. Coordinates
are stateless, survive reload, and decorrelate neighbours.

**Unloaded neighbours are solid.** `Neighbourhood::occupancy` returns `None` for
anything not loaded, and `None` is not zero: a flood must not run off the edge
of the loaded world and a pond must not drain into a chunk that has not arrived.

There are **no source blocks**. An infinite spring is a conservation violation
by definition, so `flow_range`, `renews_from` and the source flag are gone with
the model that needed them. Standing bodies of water large enough that draining
them matters are a future mechanism, deliberately deferred — see §4.5.

### 4.3 Declared sinks, and why they are counted

Conservation with no sinks is a world that only ever gets wetter. Two sinks are
allowed, and **the solver reports how much each one destroyed** rather than
silently discarding it:

- **Absorption.** Fluid touching a block a mod has declared absorbent loses
  volume to it. What "absorbent" means, how much is lost, and what the block
  turns into are the mod's (charter rule 1) — saturation is expressed as
  **registered materials**, `dirt` → `damp_dirt` → `saturated_dirt`, not as
  engine state bits. Chunks are palette-compressed so three materials are very
  nearly free, the mod owns the darker texture, and a mod may give saturated
  sand different behaviour from saturated dirt without the engine knowing what
  porosity is.
- **Evaporation.** A block with air above it may lose volume on a random tick.

Both randomness sources are engine-provided seeded streams
(`world_seed + chunk_coords + stream_name`, charter rule 4). A process RNG here
fails the cross-platform hash gate — or worse, does not, and two servers drift.

The conservation invariant charter rule 15 requires is therefore
**`volume in = volume still present + absorbed + evaporated`**, which is only
expressible because the sinks are counted. A solver that destroyed volume
without reporting it would make the proptest unwritable.

### 4.4 Surface height, and the block above

Rendered surface height and the physics' submerged fraction are both
`volume / 27` directly.

**A block with fluid above it renders full, with no surface.** Only the topmost
block of a body of fluid has a surface, which is what a body of water looks
like. The old rule capped a full block at 24 cells of 27 so that a brim-full
block still showed a surface below the block above it — a hack for a waterfall
reading as a solid column. Conservation removes the need for it: falling fluid
genuinely holds little volume per block, so a waterfall is thin because it is
thin, not because the renderer was told to lie about it.

### 4.5 What is deferred, and why it is safe to defer

Large standing bodies of water — oceans — want a mask and a global sea level
rather than physical blocks, because simulating an ocean block by block is
ruinous and because a conserved ocean drains into the first cave anybody digs
under it. That mechanism is **deliberately not built yet**.

It is safe to defer because **no reference generator produces standing water**:
every drop in a world comes out of a player's bucket. The day worldgen grows an
ocean is the day this section needs its other half, and that is a mechanism
task, not a tuning pass.

### Implemented by

`crates/core/src/fluid/` — `Neighbourhood::occupancy` reports how full a block
is, the fluid's own `waterlogs_at` decides what that means for floor, and
`Fluid::capacity` turns it into how much will fit. The world reports a fact; the
policy lives with the fluid, so two fluids in one world may disagree about what
counts as floor.

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
- **A block brush fills the GAPS in a block that has been partly mined**, not
  the first N cells from the bottom. Those are the same mask in an empty block
  and different masks in a carved one, where the bottom-up run overlaps what is
  left and the placement is refused — reported from the window as placing
  against a half-mined block doing nothing and saying something was already
  there. The gaps are taken in `placement_mask`'s bottom-up order, so a partial
  payment still fills deterministically.
- **A block brush tops up its OWN material and steps around anything else.**
  Filling the gaps in a carved block is what makes carving reversible; it is
  not an invitation to mix. A block brush aimed at a block holding a different
  material lands in the next block along the face instead, leaving the gaps
  empty — reported from the window, and `place::landing` is the rule. With no
  face to step along it is refused rather than guessed at.
  **Mixing is still possible and still deliberate**: a sub-node brush places
  one cell of anything into any block with room, which is how a block of
  twenty-two stone and five gold gets made. The distinction is the point — a
  brush that addresses whole blocks should not produce a block nobody could
  have asked for.

**A stack cut to a shape places its cut, whatever brush is held.** The cut *is*
the thing being carried: a chisel does not get to spend a whole crafted stair to
put down one of its cells, and a block brush does not get to flatten it into a
bottom-up run of the same number of cells. A tool decides what comes OUT of the
world; what goes back in is whatever is in the player's hand. Only loose
material — a stack with no cut — is subject to the brush at all.

**Occupancy is judged per cell, never per block.** A placement is refused if any
cell it would fill is already occupied — so a chiselled block's empty cells can
be filled, and a whole-block placement into a block with no room left is refused
because its cells overlap.

Together these are what make carving **reversible**: a cell taken out of a block
can be put back into the same cell of the same block. Without either half — a
fill anchored to the block's bottom, or a refusal that looked at the whole block
— sub-node resolution would exist only for removal.

### 7.2 Writing a placement — the plan's cells, and never a count

**The edit a placement produces carries the planned cells themselves.** It is
never re-derived downstream from how MANY units were paid: `placement_mask(n)`
is the answer to "what does loose material look like", and applying it to a
plan that already chose its cells silently replaces a crafted shape, or a set
of gaps, with a bottom-up run of the same size. That is one defect with two
faces — a crafted stair placing as a lump, and a gap-fill landing in the wrong
cells.

**A write into a block that already holds something must MERGE.**
`Edit::Partial` sets the whole block, so sending one that names only the new
cells deletes everything else in it — material destroyed on a path no player
caused and charter rule 5 forbids. Two shapes of write follow:

- The block is empty, or everything in it is the material being placed: one
  `Edit::Partial` carrying the UNION of what was there and what is being added
  (canonicalising to `Edit::Block` when that union is full).
- The block holds a different material: one `Edit::SubNode` per added cell, in
  `placement_mask`'s order. Each preserves what it does not name, and the
  result is a `Mixed` block — the storage form §0 exists for.

Task 09 implements §7.1; `crates/core/src/place.rs` is the implementation and
`a_chiselled_cell_goes_back_into_the_cell_it_came_out_of` is the test. §7.2 is
`place::trim` and `place::writes`, and
`placing_a_cut_stack_puts_that_cut_in_the_world` is the test that would have
caught its absence.

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
