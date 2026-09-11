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
- **A material may declare itself `passable`, and then it stops nothing.** Grass,
  a fern, a hanging vine: a body walks through it as though it were air. The
  cell is still OCCUPIED for every other purpose — it meshes, it is lit, it
  holds fluid out, and a ray still stops at it, which is what lets a player aim
  at a tuft and break it. Only the body sweep asks the question differently.

  **Movement and aim are separate questions and this is where they separate.**
  A `passable` material answering "not solid" everywhere would be unbreakable:
  the dig ray and the reach check use the same solidity test the sweep does, so
  a tuft nothing collided with would also be a tuft nothing could target.

  Without this, every plant is a lip. Collision is at sub-node resolution and a
  two-cell fern is two thirds of a yard to climb — so foliage had to grow in
  clumps with gaps and tufts had to stay one cell tall, which is a mod shaping
  its content around an engine limit rather than around what it wants.
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

**Transparent materials are the one exception, and they do not touch this
cache** — see §8.1. A `Uniform(transparent)` block is permeable on all six faces,
decided by a material lookup where the cached answer is read rather than where it
is computed, so `Chunk` still knows nothing about a material registry.

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

### 5.1 Ground cover — a run of cells, standing on the surface, inside one block

A generator placing grass, ferns or moss needs three things a density field
cannot express, and `ChunkBuffer::fill_cover` is where they are defined.

**The surface is a fact about cells, not about samples.** It is wherever an
occupied cell has an empty cell directly above it. A field knows nothing of
this: it has one sample per block at block resolution, and a run two cells tall
in a block whose surface sits at an arbitrary cell contains that sample in only
a third of columns — so the surface-shell test §5's detail fill is built from
misses the other two thirds, in stripes that follow the contours.

**A run never crosses a block boundary.** The run starts at the lowest empty
cell in the block that stands on an occupied one, and stops at the block's top
however many cells were asked for. This is what keeps a tuft from being two
stacked blocks that highlight separately, dig separately and can be left
half-standing — which §7.1's "the acting tool's brush decides" would otherwise
permit and which reads as a bug.

**Cover is not ground for more cover.** Every base is found before any cell is
written, so a run never stands on a run. Without this rule a two-cell run would
seed a further run above it on the next block, and grass would climb.

Two consequences worth stating because they are visible:

- A block holding two surfaces at once — a one-cell shelf — grows cover on the
  lower one only. The upper face is bare. Rare shape, deliberate limit.
- A surface exactly on a chunk's bottom cell row gets no cover from that chunk:
  the block below is in the neighbouring chunk, which is not available at
  generation. One row in forty-eight.

Cover written this way is ordinary sub-node terrain — §2 collides with it, §3
lights through it, §8.4 draws it as a billboard when its material asks. Nothing
about it is a special case downstream.

**Implemented by** `ChunkBuffer::fill_cover`, exposed as `buf:fill_cover`.

### 5.2 Structures across a chunk edge — the neighbourhood, not a deferred write

A structure is rooted in one place and reaches out from it, and nothing makes
that reach stop at a chunk boundary. **The engine does not hold writes aimed at
a chunk that does not exist yet, and will not**: chunks are generated in
whatever order players walk towards them, so a chunk made before its neighbour
would lack what that neighbour contributes and a chunk made after it would have
it. Same seed, different world, differing only for players who approached from
one side. Charter rule 4 is not only about floats.

The order-independent shape is the other way round. A generator runs its
structure pass for every chunk within reach — each chunk's structures being a
pure function of its own position and the seed, through `rng_stream` — writes
all of them in **world** coordinates, and the buffer keeps the ones that land in
it. Every neighbour does the same work and keeps a different slice. Nothing is
stored between chunks, so nothing can be stored in the wrong order.

Two calls carry it, and both drop silently by design: a generator placing a
structure whose root is two chunks away should not have to know which of its
blocks fall inside, and making every mod check would be making every mod get the
edges right.

**Implemented by** `ChunkBuffer::set_block_world` and
`ChunkBuffer::set_subnode_world`, exposed as `buf:set_world` and
`buf:set_subnode_world`; `Density::sample` (`density:at`) supplies the ground
height under a root, because a buffer is write-only and a neighbouring chunk's
buffer is not the generator's to read.

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
- **A block brush tops up whatever it is aimed at, whosever material is
  already in there.** Filling the gaps in a carved block is what makes carving
  reversible, and it does not matter whether the carving was yours: if you hold
  a material and a block in reach has room, the material goes in the room.
  Nothing steps aside and nothing is refused for a material mismatch.

  **This reverses the rule that stood from Task 09 to 2026-09-04**, which was
  that a block brush topped up its own material only and a different one landed
  in the next block along the face (`place::landing`). That came from the
  window and was reverted from the window: mixing is what a player expects to
  be able to do, the step-off surprised more often than it protected, and a
  block nobody could have asked for turns out to be a block plenty of people
  ask for. Anything relying on the old behaviour — including the guarantee that
  a block-brush placement lands in the block that was aimed at only when the
  materials agree — no longer holds.

  What has NOT changed is that a placement only ever writes into **air**
  (§7.2). Material already in a cell is never displaced, overwritten or
  destroyed: that would be a conservation hole, and charter rule 5 does not
  have an exception for convenience. "On top of" means into the gaps, not
  instead of.

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

### 7.3 Stamping a plan — the named blocks replace, the absent ones are untouched

**A stamp is a mod edit, not a placement.** Nothing is paid for out of an
inventory, so §7.1 and §7.2's conservation rules do not apply to it — the same
way they do not apply to `game.set_block`, which has replaced whole blocks
since Task 09. What a mod may not do is destroy material a player is holding;
what it may obviously do is change the world, which is what a mod is for.

A plan (`crates/core/src/plan.rs`) is **sparse**: it records only the blocks
that hold something, so "this block holds nothing" and "this block was not
captured" are one state and cannot be told apart. That fixes both halves of
what a stamp does:

- **A block the plan does not name is not touched at all.** Stamping adds a
  building to a hillside rather than cutting its bounding box out of the hill
  first. A mod that wants the box clears it itself; a mod that wanted the
  additive behaviour could not have undone a clearing one.
- **A block the plan does name is replaced by what the plan says it is**,
  including its empty cells. The plan is a description of that block, not of
  the cells to add to it, so a half-slab in the plan lands as a half-slab.

**A mixed block is stamped as a replace followed by cells**, which is §7.2's
second shape used for the same reason: one `Edit::Partial` for the first
material, then one `Edit::SubNode` per filled cell for each material after it.
Sending an `Edit::Partial` per material would leave the block holding whichever
one was captured last, and a plan that quietly dropped the two-material parts
of somebody's build would be a loss in exactly the detail charter rule 19 says
is the point of the engine.

Implemented by `crates/server/src/plans.rs` — `capture` for the read and
`block_edits` for the write, with
`a_mixed_block_is_captured_as_layers_and_stamped_back_as_layers` as the test.

---

### 7.4 A mod's merge write — the cells it names, and nothing else

**`game.set_block` replaces, and that stays the default.** §7.3 says so and the
reason holds: a mod that says what a block is has said what the whole block is,
including which of its cells are empty, and a mod wanting to add to a block
could not have undone a replacing one.

But a mod growing something INTO terrain wants the other shape. A rock in turf
or a root through soil occupies a few cells of a block the world has already
filled, and a masked `set_block` there replaces the turf with air in the
twenty-odd cells the rock does not claim — the rock ends up standing in a
footprint of its own bounding block, which is the opposite of embedding it.

The merge write is **§7.2's rule made available to a mod**, and it is the same
rule for the same reason, so it produces the same two shapes of edit:

- The block is empty, or everything in it is already the material going in: one
  `Edit::Partial` carrying the UNION of what is there and what is being added,
  canonicalising to `Edit::Block` when that union is full.
- The block holds a different material: one `Edit::SubNode` per named cell,
  each preserving what it does not name. The result is a `Mixed` block — the
  storage form §0 exists for.

**A named cell is taken, whatever was in it.** Merging is about the cells the
write does NOT name, not about yielding to what is already there: a rock cell
landing where turf was becomes rock. A mod wanting to fill only what is empty
asks what the block holds first.

**Nothing on the wire is new.** The union and the per-cell forms are the edits
§7.2 already sends, so a merge write costs what a player placing the same cells
costs — up to twenty-seven edits for a block that holds a different material,
which §10 prices. A mod merging a large shape into varied terrain pays for it,
and the alternative is a variant that means "and keep the rest", which every
peer would have to be taught for a saving of a few bytes on a path no player
action reaches.

Conservation (charter rule 5) is not at stake either way: nothing is paid out
of an inventory, exactly as §7.3 describes for stamps.

Implemented by `WorldEdit::merge_partial`, resolved where the seeds are drained
because that is where the world can be read; `place::writes` is the shared
implementation of both shapes, so this section and §7.2 cannot drift apart.

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

### 8.1 Transparent materials — glass

A material may declare itself **transparent**. Glass is the case; a mod's window,
ice, or a coloured pane are the same case. One flag, not an alpha value: what a
transparent block looks like is its texture's own alpha, and a second opacity
number beside it would be two sources of truth for one appearance.

Transparency changes three rules and deliberately leaves the rest alone.

**Culling (§8).** A face is culled against the neighbouring cell *unless* exactly
one side of it is transparent. Two consequences, both required:

- A glass block against air draws, as any block does.
- A glass block against STONE draws the stone's face, which a fully-occupied
  neighbour would normally cull. Without this a wall behind a window is a hole
  straight through the world, which is the same fault fluid had against terrain
  (§4) and is fixed the same way.
- Two glass blocks against each other draw NEITHER interior face. Drawing them
  would stack two blended surfaces per pane and darken a window in proportion to
  its thickness.

**Drawing.** Transparent quads are a separate list, drawn in a blended pass after
the opaque world, like fluid. They are NOT sorted against each other: sorting per
quad is per-frame work proportional to the geometry, and the artefact it removes
— two panes at an angle blending in the wrong order — is far cheaper to accept
than to pay for every frame. **Stated so it is a known limit rather than a bug
report.**

**Lighting (§3).** A block whose content is `Uniform(transparent)` is permeable
on all six faces: light passes through glass. Otherwise a glass roof makes a dark
room, which is the first thing anybody builds with it.

The **permeability cache is not changed**, and charter rule 19's requirement that
lighting never recompute the 3×3 face test still holds. The transparency test is
a lookup on the material, done where the cached answer is READ —
`propagate::Neighbourhood::faces` — and not where it is computed. That is what
keeps `Chunk` free of any knowledge of a material registry, which it must be:
`permeability` is a pure function of block content and there are ninety-four
places that build a chunk.

Only `Uniform` is transparent to light. A `Partial` or `Mixed` block holding
glass falls back to the ordinary cell rule, because "how much light does a block
that is half glass and half stone pass" is a question with no obviously right
answer and no caller yet. Recorded as a limit rather than guessed at.

**Collision (§2) is unchanged. Glass is solid.** You cannot walk through a
window. Transparency is about light and sight, and nothing about it belongs in
the collision rule — a body collides with occupancy, which glass has.

**Fluid (§4) is unchanged.** Glass holds milk in, being solid.

---

### 8.2 Cutout materials — foliage

A material may instead declare itself **cutout**. Leaves are the case; a fern, a
grate, a chain-link fence are the same case. It is a different declaration from
§8.1 and not a variant of it, because **the two want opposite culling** and a
mod that picks the wrong one gets an artefact rather than a preference.

A texture that is transparent in PLACES is not the same thing as a texture that
is see-through EVERYWHERE, and the rule that is right for a window is wrong for
a canopy:

**Culling (§8). A cutout face is never culled, and never culls its
neighbour.** Not "unless one side is transparent" — never, including against
another cell of the same material. §8.1's rule exists so two panes do not stack
two blended surfaces and darken a window by its thickness; a canopy has the
opposite requirement, because the faces inside it are the leaves you see through
the gaps in the leaves in front. Culled, a mass of foliage becomes a hollow
shell whose alpha holes look straight through the world — reported from the
window as seeing the sky through the leaves, which is exactly what it was:
the frame's clear colour, with nothing drawn over it.

A cutout cell is therefore in NO occupancy set for culling purposes. Stone
behind leaves keeps its face for §8.1's reason, and the leaves keep all of
theirs.

**Drawing.** Cutout quads are their own list, drawn with an **alpha-tested
pipeline in the opaque phase**: the fragment is discarded below the alpha
threshold, and what survives writes depth like any solid surface. So the
sorting limit §8.1 records **does not apply here** — foliage occludes itself
correctly at every angle, for free, because depth does the work that sorting
would have to.

The threshold is a constant and not a per-material number, for §8.1's reason:
the texture already carries the alpha.

**The cost is real and is the trade.** Every interior face of a mass of foliage
is drawn, where a solid material would have culled it — a dense canopy is
several times the quads of the same volume of stone. That is what makes it look
like foliage rather than like a painted box, and a mod that wants the cheap
version leaves the flag off.

**Lighting (§3) is §8.1's rule, unchanged**: a block whose content is
`Uniform(cutout)` is permeable, so light passes through leaves. Dappled shade —
foliage that passes SOME light — is not expressible: permeability is a yes or
no, and inventing a third state for one material would put a number in the
lighting hot path that every other block would pay to read. Recorded as a limit.

**Collision (§2) is unchanged.** Leaves are solid, like glass. Whether a player
can walk through foliage is a mod's opinion about its own blocks, and the engine
has no view.

**A material is one or the other, never both.** `register_block` refuses a block
declaring `transparent` and `cutout` together rather than picking one, because
the two answer the same question differently and a silent winner is a bug
somebody debugs from the wrong end.

---

### 8.3 Swaying materials — fake wind

A material may declare itself **swaying**, and then the TOP of it moves.

**Presentation only, and it moves nothing real.** The world does not know the
grass is bending: collision, lighting, meshing and the server's idea of where
anything is are all untouched, and two clients with different frame rates
disagree about where a leaf is at any instant without disagreeing about
anything that matters. Charter rule 4 does not reach it — rendering is exempt.

**Which vertex moves is geometry; whether it moves at all is the material.**
The two are decided in different places on purpose:

- The mesher marks the vertices on the **top edge** of every quad, from the
  quad's own corners: for a face pointing up, all four; for a side face, the
  two with the greater height; for a face pointing down, none. It asks nothing
  about the material, so the mark costs a comparison per corner and no lookup.
- The shader displaces a marked vertex only if its material declares a sway
  amplitude. A world whose mods declare none does not read the table at all.

Marking the top edge rather than the whole quad is what makes it **bend rather
than slide**. Greedy meshing merges a plant's side into one quad spanning its
whole height, so displacing the top corners and leaving the bottom ones gives a
linear bend from base to tip for free — and the base stays planted, which a
rigid offset would not.

The motion is the engine's own smooth noise sampled at the vertex's world
position and the animation clock, so neighbouring plants move together in
gusts rather than each buzzing independently, and a plant keeps its phase as
the camera moves.

**A cell that is not the top of anything still gets marked** if it is the top
of its own quad. That is not a defect: the alternative is asking the world what
is above every cell at mesh time, and the artefact it would fix — a fern's
midriff moving because it happens to be the top of one merged run — is smaller
than a per-cell lookup across the whole chunk.

---

### 8.4 Billboard materials — grass

A material may declare itself a **billboard**, and then its cells are not drawn
as geometry at all. Each run of them becomes one camera-facing sprite.

**Why §8.2 is not enough, and this is not a preference.** A cutout cell is still
a cube: six faces, each showing what the texture does over that cell. Two things
follow and both are fatal for grass.

- **A texture repeats once per BLOCK** (§8, and `world.wgsl` says so at the
  top), so a face one cell across shows a NINTH of the tile — a crop, not a
  sprite. A grass texture drawn on single cells is nine pieces of grass, none of
  them whole.
- **A cube seen from any angle is a cube.** A tuft one cell across reads as a
  small floating box, and a plane of them reads as a fence. Neither reads as a
  plant, which is what "sprite cards are not really a thing here" means and it
  is correct.

So a billboard is its own kind of drawing:

- **One instance per RUN, not per cell.** Contiguous billboard cells in a column
  are one sprite standing on the lowest of them, as tall as the run and as wide.
  A run of one is a third of a yard; a run of three is a yard. Drawn per cell
  instead, a plant three cells tall would be three copies of its own texture
  stacked, which is worse than the cube it replaced.
- **It faces the camera about the vertical axis only.** Yaw, never pitch: grass
  that tilted to meet a player looking down would lie over like a fallen sign,
  and the ground is the one direction a sprite must keep its footing on.
- **No geometry buffer.** The quad is built in the vertex stage from the vertex
  index, exactly as a blob shadow is. A cell costs one instance rather than four
  vertices and six indices, which is why a field of grass is cheaper this way
  than as the cubes it replaces.
- **Alpha-tested and depth-writing**, like §8.2 and for the same reason: it
  needs no sorting, and it occludes and is occluded correctly at every angle.
- **It is culled against nothing and culls nothing.** A billboard cell is in no
  occupancy set, so the ground under it keeps its face and the sprite is drawn
  wherever the mod put it.

**Everything else about the cell is unchanged.** It is still there for collision
(unless it also declares `passable`), still lit, still holds fluid out, and a
ray still stops at it — so a player can aim at a tuft and break it. What changes
is only how it is drawn.

**Sway (§8.3) applies to the top of a sprite**, which is where its own vertices
are, so grass bends without any of §8.3's marking: the shader knows which two
corners are the top because it built them.

---

### 8.5 Biome colour — per chunk column, blended, never stored

A mod gives each chunk a colour through `register_chunk_tint`, and every
material in it that **declares a tint** is drawn through it. Declaring a tint is
what opts a material into varying with its surroundings; it is therefore also
what opts it into varying with the place, and a material that declares nothing
is the same colour everywhere.

**Per chunk COLUMN, not per chunk.** A biome is a fact about a place on the map
and not about a height. Keyed per chunk the colour would band vertically, with a
seam at eye level, and a mod would have to remember to answer the same thing for
every `y` to avoid it.

**Carried at the corners, not at the centre.** One flat colour per chunk draws
the world as 16-block squares — it trades a biome line nobody notices at ground
level for a grid nobody can stop noticing from a hill. Each of a chunk's four
x/z corners takes the mean of the four columns meeting there, so two chunks side
by side compute the same value for the corners they share and the field runs
across the boundary with nothing to see. A hard step between two biomes becomes
a gradient about a chunk wide.

**On the instance, not in the vertices.** A per-vertex colour would be four more
bytes on every terrain vertex, against the absolute VRAM bound charter rule 19
put in place of the geometry-inflation gate — for a value that is constant over
sixteen blocks. Sixteen floats per chunk costs nothing, and it means a colour
that changes needs no remesh: the next frame's instance simply carries different
numbers.

**Asked for when a chunk is served, and never stored.** A colour on disk would
freeze a mod's palette into every world it ever generated, so that changing a
biome's colour left the colour it used to be in the ground behind the player.
It costs one script call per chunk served, against the milliseconds generating
one costs.

Summaries carry no colour: a horizon silhouette holds no tinted material for it
to land on.

**Implemented by** `ScriptVm::chunk_tint` and `ChunkSource::tint`, sent as
`ServerMessage::ChunkData::tint`, held by `Renderer::set_chunk_tint` and blended
by `biome_at` in `world.wgsl`.

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
