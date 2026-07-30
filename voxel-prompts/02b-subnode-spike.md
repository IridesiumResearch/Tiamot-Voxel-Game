# TASK 02b — Sub-node risk spike: prove or kill the 3×3×3 system EARLY

Depends on: 02. Throwaway-quality code allowed in `spikes/subnode/` (a workspace member
excluded from release builds); the Sub-Node Contract doc is a permanent deliverable.

## Why this task exists
The sub-node system is the project's signature feature AND its biggest load-bearing risk.
Under a naive plan its true costs surface in Task 08 (meshing) and Task 09 (collision) —
after six tasks have built on the data model. This spike pulls those costs forward to a
decision NOW, before networking, persistence budgets, and rendering assume an answer.
Do not polish; measure.

## The algorithm is not a free variable
Use **binary greedy meshing** — occupancy as bitmasks in `u64` words, face culling by
shifting and ANDing whole columns, quad merging by bit tricks. Do not implement a classic
per-voxel greedy mesher and then measure it; that would measure the wrong thing and could
kill a viable design. Published implementations mesh a 64³-ish chunk in roughly 50–200 µs
single-threaded, around 7× faster than conventional Rust greedy meshers in the ~4.5 ms
range. Our 48³ sub-node grid is *smaller* than that reference volume (110k cells vs 262k).

**This is why chunks are 16³ blocks** (charter rule 6): a 48-cell column plus the 2 bits of
padding needed for neighbour-face culling is 50 bits — one `u64`. A 32³ block chunk would
need 96 bits per column and lose the entire technique. Record this rationale in the contract
doc; it is the single most important consequence of the chunk-size choice.

## Deliverable 1 — The Sub-Node Contract (`docs/subnode-contract.md`, permanent)
One page, authoritative forever, defining how EVERY system treats Uniform / Partial / Mixed:
- Collision: solid at sub-node resolution (a half-mined block is climbable/enterable).
- Lighting: block resolution; light passes a face iff that face's 3×3 sub-node layer is not
  fully occupied (codify Task 10's rule here).
- Fluid: blocks accept fluid iff occupancy is empty; Partial/Mixed are fluid-solid (Task 11
  is block-resolution and does not otherwise interact with sub-nodes).
- Worldgen: generators write at BLOCK resolution by default; sub-node detail is opt-in per
  generator (this caps the 27× generation cost to mods that ask for it — Task 04's
  ChunkBuffer implements the lazy expansion that makes this real).
- Pathfinding (Task 12): navigates at block resolution; Partial blocks are obstacles unless
  the bottom sub-node layer is empty (walkable-through rule kept dead simple).
- Placement/support: no structural simulation; any occupancy configuration is legal.
- Rendering: sub-node resolution meshing via binary greedy meshing; the `u64`-column
  invariant is stated here.
- Inventory: 27-unit arithmetic (Task 02), no exceptions.
- Networking: what a sub-node edit costs on the wire (fill in from Deliverable 5).
Every future task PR touching these systems cites the contract line it implements.

## Deliverable 2 — Meshing worst-case prototype
- Binary greedy mesher over the 48³ grid (correctness per Task 02 shapes; perf-focused).
- Scenes: (a) flat uniform terrain slab; (b) "chiselled city" — every surface block Partial
  with random 13/27 occupancy; (c) 3D-checkerboard Mixed worst case; (d) realistic mix
  (95% uniform, 4% partial, 1% mixed).
- Measure per scene: mesh time per chunk, vertex/index counts, **measured** VRAM (allocate
  the real buffers and read back the allocation — do not project), remesh time after a
  single sub-node edit.

## Deliverable 3 — Collision prototype
- Swept-AABB vs sub-node grid (throwaway version of Task 09's core): player-sized AABB doing
  random walks through scenes (b) and (d); measure per-tick collision cost at 20 tps for 100
  simulated players; verify step-up at 1/3 yard works and FEELS right in a minimal
  visualisation (a debug orbit camera is enough — no real renderer).

## Deliverable 4 — Lighting probe
The 3×3-mask face test sits inside the light BFS inner loop, so sub-node cost leaks into
Task 10 whether or not lighting stores sub-node data. Measure full-chunk light BFS over
scenes (b) and (d) with the mask test wired in, against a baseline that treats every Partial
as fully solid. Report the delta. This number decides whether Task 10 needs a per-block
"is any face permeable" cached bit.

## Deliverable 5 — Storage and bandwidth probe
The two budgets that silently constrain Tasks 03 and 06:
- Serialized + zstd-compressed chunk size for each scene. Task 03's "uniform chunk ≤ 100
  bytes" target has no equivalent for chiselled chunks yet — produce it here.
- BlockDelta bytes for a 60-second single-player chiselling session (record the edit stream
  from the collision prototype's random walk, or script one). This sets whether sub-node
  edits need their own compact delta encoding in Task 06 or can ride the block path.

## Deliverable 6 — Verdict memo (in the PR description + `docs/subnode-verdict.md`)
Three outcomes, not two. Compare measurements against these gates on dev hardware:

**KEEP** — all of:
- Scene (d) mesh time < 1 ms/chunk single-threaded; scene (b) < 4 ms; remesh-after-edit
  < 2 ms. (Calibrated against the published binary-greedy figures above: a 10× miss means
  the *implementation* is wrong, not the design. Investigate before declaring a verdict.)
- Scene (b) vertex count < 8× scene (a) equivalent surface; measured VRAM at view distance
  12 with 10% chiselled surfaces < 1.5 GB.
- Collision for 100 players in scene (d) < 1.5 ms/tick total.
- Light BFS delta from the mask test < 25% over the solid-Partial baseline.
- Scene (b) compressed chunk < 8 KB; chiselling delta stream < 32 KB/min/player.
- Scene (c) degrades gracefully (slow is fine; crash/explode is not).

**KEEP-WITH-LIMITS** — everything passes except the Mixed-heavy scene (c), or scene (b)
exceeds a bound while (d) is comfortable. Then cap the pathology rather than abandon the
feature: a hard per-chunk Mixed-slot budget, with blocks past the cap degrading to
`Partial` of the dominant material. Record the cap, add it to the contract, and add a
proptest that the degradation is deterministic and conserves unit accounting.

**FALLBACK** — the realistic scene (d) misses its gates and no fix is apparent within the
spike. Adopt: full-block collision + full-block meshing, KEEPING the 27-unit inventory
arithmetic and the 27-bit occupancy mask used only for (i) chisel drop accounting and (ii) a
visual damage/carve overlay on the block face. This preserves the game's identity (chisel,
spares, coalescing) at a fraction of the risk. If fallback is chosen, this task's output
also includes:
- edited versions of prompts **08 and 09** removing sub-node meshing and sub-node collision
  (Task 11 is already block-resolution and needs no edit; Task 15b's LOD0 definition follows
  automatically);
- a **rewritten** (not deleted) Sub-Node Contract stating the reduced semantics, so later
  tasks still have one authoritative place to cite.

## Acceptance criteria
- [A] Prototypes run headless with a single command each and print the measurement table.
- [A] All six deliverables produced; every gate above has a recorded number next to it, not
      a projection or an estimate.
- [A] Contract doc exists, covers every listed system, states the `u64`-column rationale, and
      CLAUDE.md references it (charter rule 12).
- [H] You read the verdict memo and explicitly decide KEEP / KEEP-WITH-LIMITS / FALLBACK
      before any Task 08 work. The decision is recorded in the memo with a date.
      (Tasks 03–07 are sub-node-agnostic either way and may proceed in parallel; 08 and 09
      are not — do not start 08 without the decision.)
