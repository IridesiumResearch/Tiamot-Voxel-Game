<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Sub-Node Spike — Verdict Memo

**Task 02b. The decision this memo records gates Task 08 and Task 09.**

| | |
|---|---|
| **Measured on** | AMD Ryzen 7 7800X3D, 16 threads, 30 GiB RAM, Linux (WSL2 container) |
| **GPU adapter** | llvmpipe (LLVM 21.1.8) via Vulkan — software rasteriser, no discrete GPU present |
| **Build** | `--release`, rustc 1.97.1, single-threaded throughout |
| **Reproduce** | `cargo run -p subnode-spike --release --features gpu -- all` |
| **Raw output** | [`../spikes/subnode/out/measurements.txt`](../spikes/subnode/out/measurements.txt) |

> **DECISION: _______________ (KEEP / KEEP-WITH-LIMITS / FALLBACK)**
> **Decided by: _______________  Date: _______________**
>
> This is an `[H]` criterion. It is not filled in, because it is not mine to
> fill in. **Task 08 must not start until it is.**

---

## 1. The gates, scored

Every row is a measured number. Nothing here is projected or estimated.

| # | Gate | Threshold | Measured | |
|---|---|---|---|---|
| 1 | Scene (d) mesh time | < 1 ms | **0.108 ms** | ✅ 9× margin |
| 2 | Scene (b) mesh time | < 4 ms | **0.143 ms** | ✅ 28× margin |
| 3 | Remesh after one sub-node edit | < 2 ms | **0.106 ms** (d), 0.135 ms (b) | ✅ 15× margin |
| 4 | Scene (b) vertices vs scene (a) | < 8× | **772×** | ❌ **FAIL** — see §2 |
| 5 | VRAM, view distance 12, 10% chiselled | < 1.5 GB | **19.9 MiB** | ✅ 77× margin |
| 6 | Collision, 100 players, scene (d) | < 1.5 ms/tick | **0.0140 ms/tick** | ✅ 107× margin |
| 7 | Light BFS mask-test delta, scene (d) | < 25% | **5–10%** | ✅ |
| 7b | Light BFS mask-test delta, scene (b) | < 25% | **44–51%** | ❌ **FAIL** — see §3 |
| 8 | Scene (b) compressed chunk | < 8 KiB | **1,797 B** | ✅ 4.7× margin |
| 9 | Chiselling delta stream | < 32 KiB/min | **2.79 KiB/min** | ✅ 11× margin |
| 10 | Scene (c) degrades gracefully | no crash | **0.337 ms, 122 B** | ✅ — see §4 |

**Eight of ten gates pass, most by one to two orders of magnitude. Two fail.**
Both failures are on scene (b), the fully-chiselled city; scene (d), the
realistic content mix, is comfortable everywhere.

Under the task's rubric that reads as **KEEP-WITH-LIMITS**. My analysis is that
the correct call is **KEEP**, because neither failure is what its gate was
written to catch. I set out the reasoning below so you can disagree with it.

---

## 2. Gate 4 — geometry inflation, 772× against a < 8× threshold

**The measurement is real. The metric is degenerate.**

Scene (a) is a perfectly flat slab. Greedy meshing collapses its entire 48×48
top surface into **one quad**, and the whole chunk into **10 quads / 40
vertices**. Scene (b) produces 30,888. Hence 772×.

The denominator is the problem. Any surface with detail — chiselled, natural
terrain, a staircase, a wall with a window — blows an 8× ratio against a flat
plane, because greedy meshing reduces the flat plane to almost nothing. The
threshold is unreachable by construction for anything except another flat plane.
It is a mis-specified metric, not a signal about sub-nodes.

What the gate was *protecting* is VRAM, and VRAM is measured directly by gate 5.
So I measured it at the worst case the gate contemplates and then past it:

| Surfaces chiselled | Quads | Allocated (measured) |
|---|---|---|
| 10% (the gate's assumption) | 372,429 | **19.9 MiB** |
| **100% (every surface block carved)** | 3,215,392 | **171.7 MiB** |

**Even if every surface block in view distance 12 is chiselled, the design
allocates 171.7 MiB against a 1,536 MiB budget — 9× headroom.** The concern
behind gate 4 does not materialise.

The discriminating comparison, for what it is worth, is scene (b) against scene
(d): **9.8×**. That is the honest statement of what chiselling costs in
geometry, and it is affordable.

**Recommendation:** replace gate 4 with an absolute VRAM bound, which gate 5
already is. Record the substitution rather than quietly dropping it.

---

## 3. Gate 7b — lighting, ≈50% against a < 25% threshold

**The measurement is real and the gate is meaningful. The failure is
cheaply fixable, and Deliverable 4 anticipated exactly this.**

Measured across repeated runs, so the noise is characterised rather than
assumed. These operations take tens of microseconds, and an early version of the
probe took too few samples — it reported deltas as wide as −10% to +61%,
including *negative* deltas for strictly more work, which is only possible if
the measurement is dominated by scheduler noise. Sample count is now 500 per
figure and the spread below is what remains:

| Scene | Baseline | With mask test | Delta |
|---|---|---|---|
| (b) chiselled | ≈0.056 ms | ≈0.084 ms | **44–51%** |
| (d) realistic | ≈0.055 ms | ≈0.059 ms | **5–10%** |

Two things follow.

First, the absolute cost is **≈30 µs per full-chunk relight**. Fifty percent of
a very small number is a smaller number. A server relighting 30 chunks a tick
would spend under a millisecond of a 50 ms budget on this.

Second — and this is the deliverable's actual purpose — **it decides that
Task 10 needs a cached per-block permeability byte.** The task text says this
number "decides whether Task 10 needs a per-block *is any face permeable* cached
bit". It does. Six bits per block, computed on write, removes the entire delta
from the propagation loop. That is now a measured requirement written into the
Sub-Node Contract §3, not a maybe.

**Recommendation:** accept the failure, treat it as a resolved design question,
and hold Task 10 to the cached bit.

---

## 4. Scene (c) — the Mixed pathology

The gate asks only that it degrade gracefully. It does, comfortably:

| | Scene (c) | vs scene (d) |
|---|---|---|
| Mesh time | 0.337 ms | 3.1× |
| Quads | 13,824 | 17.6× |
| Compressed chunk | 122 B | *smaller* |
| Light BFS | 0.014 ms | *faster* |

The checkerboard is the most expensive thing to mesh and among the cheapest to
store — it is perfectly regular, so zstd erases it. Lighting is fast because the
chunk is opaque and BFS terminates immediately.

**No Mixed-slot cap is needed.** This matters, because the rubric's prescribed
KEEP-WITH-LIMITS remedy is "a hard per-chunk Mixed-slot budget" — and that
remedy would not have helped: **scene (b), which fails two gates, contains no
Mixed blocks at all.** It is entirely `Partial`. Capping Mixed slots would have
addressed a pathology that is not the one measured.

---

## 5. What was not measured, and cannot be here

Stated plainly rather than glossed.

1. **Real-GPU VRAM.** The byte totals are exact — they are `Buffer::size()` read
   back from a real Vulkan device — and they are driver-independent. What a
   software rasteriser cannot show is real-hardware allocator overhead: page
   granularity, heap fragmentation, driver bookkeeping. Treat 19.9 MiB and
   171.7 MiB as **floors**. With 77× and 9× headroom respectively, the margin
   comfortably absorbs plausible overhead, but re-running on a real GPU is a
   human gate.

2. **Whether step-up at 1/3 yard feels right.** `[H]`, by definition. Artefacts
   are in [`../spikes/subnode/out/`](../spikes/subnode/out/) — see §6.

3. **Neighbour-aware face culling.** The mesher treats everything outside a
   chunk as air, so boundary faces are emitted that a real renderer would cull.
   Every geometry number here is therefore **pessimistic**, which is the right
   direction for a gate.

4. **Multi-threading.** All figures are single-threaded on one core, as the
   gates specify. A real client meshes on a pool.

5. **The VRAM world model.** 25×25 chunk columns × 4 vertical, of which the
   surface band and the band below it produce geometry. Terrain with caves,
   overhangs, or player structures would produce more. The 9× headroom at 100%
   chiselled is the margin available for that.

---

## 6. The human gates

Neither of these can be passed by a test, and I have not claimed either.

### [H] Step-up feel

Run `cargo run -p subnode-spike --release -- collision`, then open in any 3D
viewer:

| File | What it is |
|---|---|
| `staircase-scene.obj` | Three platforms 1, 2 and 3 sub-nodes tall on flat ground |
| `staircase-path.obj` | The body's path walking east into them |
| `chiselled-scene.obj` | Scene (b), a fully chiselled surface |
| `player-path.obj` | 30 s of wandering across it |

Units are yards. A player is 1.8 yards tall, so one sub-node is a sixth of body
height.

**Measured behaviour:** the body clears the 1-sub-node platform and stops at
x = 10.32 yards, immediately short of the 2-sub-node platform at 10.67. That is
the design intent — clear one sub-node, stop at two. **Whether one sub-node is
the right ceiling is your call.**

### [H] Real-hardware VRAM

Re-run `--features gpu -- vram --chiselled-percent 100` on a machine with a
discrete GPU and compare against 171.7 MiB.

---

## 7. Recommendation

**KEEP**, with three conditions recorded:

1. **Gate 4 is retired** and replaced by the absolute VRAM bound of gate 5. The
   ratio metric is degenerate against a flat-plane denominator; the 100%-chiselled
   measurement of 171.7 MiB is the meaningful answer.
2. **Task 10 must cache a per-block permeability byte.** Measured requirement,
   written into Sub-Node Contract §3.
3. **No Mixed-slot cap.** Scene (c) degrades gracefully and the observed failures
   are in `Partial`-heavy content, which such a cap would not touch.

The case for KEEP rather than KEEP-WITH-LIMITS: the rubric's limits mechanism
exists to cap a pathology, and measurement found no pathology worth capping. The
two failing gates are one mis-specified metric and one known-remedy design
question. Every gate that measures something the design actually has to survive
passes with at least 4× margin, most with 10× to 100×.

The case against, if you want it: scene (b) is a legitimate future state of a
popular server, and it fails two of ten gates today. Choosing KEEP-WITH-LIMITS
and defining a `Partial`-density cap would be defensible conservatism. It would
cost a degradation path nobody currently needs.

**Tasks 03–07 are sub-node-agnostic and may proceed regardless. Tasks 08 and 09
are blocked on the decision above.**
