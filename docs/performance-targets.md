<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Performance targets

**Authoritative. Set by Iridesium, 2026-07-30.** Referenced by charter rule 18.

Every optimisation decision needs a target, or "fast" means nothing. These are
the numbers later tasks are held to, and the numbers a benchmark should be read
against. Changing them means editing this file first.

---

## Minimum spec

The machine the game must run **well** on, not merely launch on:

| | |
|---|---|
| CPU | ~6-core Intel i5 |
| RAM | 16 GiB |
| GPU | A modest **discrete** card. Integrated is best-effort, not a requirement. |

Anything better is headroom.

**Priority order, set explicitly by Iridesium on 2026-07-30, because these
conflict and later tasks need to know which way to resolve them:**

> **A detailed, smooth sub-node world beats reach onto low-end hardware.**
> *"If integrated graphics is not possible to achieve that is fine. Most people
> now have a card. I want the node/block system to work well and be smooth more
> than I want speed on low end devices."*

So when a rendering decision trades fidelity or frame pacing against running on
weaker hardware, **fidelity and smoothness win.** Do not spend design budget
degrading the sub-node system to fit an iGPU. Do not add a low-detail path
because integrated graphics might struggle — that is what Task 15b's LOD is
for, and LOD is a distance mechanism, not a hardware tier.

This is a priority, not permission to be wasteful. Speed still matters
everywhere it is free, and the server targets below are hard.

**What this changes for Task 08.** Frame time on integrated graphics becomes a
*nice-to-know*, not a gate. The gate is smooth frame pacing on a modest discrete
card at full sub-node detail. Fill rate and memory bandwidth are still the
binding client constraints — Task 02b measured geometry, and geometry is not
what makes a rasteriser struggle; overdraw and shading are, and neither existed
to measure at 02b.

## Server targets

| | |
|---|---|
| Players per server | **50** |
| Tick rate | 20 Hz — **50 ms per tick** |
| View distance | 12 chunks (the Task 02b gate baseline) |

The tick budget is the one to internalise: **50 ms, shared by physics, fluid,
light, entity stepping, mod scripts, and networking, for all 50 players at
once.** A subsystem that takes 5 ms/tick has consumed a tenth of the whole
simulation on its own.

### What Task 04 measured (worldgen)

Single-threaded, release, on the reference machine. Worldgen runs off the tick
on a worker pool, so these are throughput numbers rather than tick-budget ones —
but the ratio is what matters.

| Path | Target | Measured | |
|---|---|---|---|
| Block-resolution chunk, end to end | < 200 µs | **53.5 µs** | ✅ 3.7× |
| `fill_3d` over 48³, 1 octave | < 2 ms | **2.65 ms** | ❌ 1.3× over |
| `fill_3d` over 48³, 2 octaves | — | 5.04 ms | |
| `fill_3d` over 48³, 4 octaves | — | 11.24 ms | |
| Buffer expansion to 48³ | — | 88 µs | |
| Determinism fingerprint | — | 408 µs | |

**The default path is 50× cheaper than the opt-in one**, which is exactly what
Sub-Node Contract §5's lazy expansion exists to preserve. A generator that never
touches a sub-node pays 53 µs; one that fills the whole 48³ grid with 3D noise
pays milliseconds. That gap is the mechanism working.

**The 2 ms `fill_3d` target is not met, at any octave count.** Recorded as a
miss rather than explained away. Notes:

- The cost is linear in octaves, so the target is only ever within reach at one.
- A tried and rejected optimisation is documented in `detgen::noise`: a cheaper
  lattice hash gained 1% and broke seed sensitivity. **The hash is not the
  bottleneck.**
- The fills do **not** auto-vectorise — verified from the emitted assembly by
  `scripts/check-vectorisation.sh`, which found zero packed float instructions.
  The cause is inherent: data-dependent gradient lookups are gathers, and the
  simplex radial falloff branches per lane.
- Explicit SIMD is the remaining option and was not attempted. It needs its own
  determinism argument first: a vector path that disagrees with the scalar path
  by one bit breaks the cross-platform hash gate on any machine that dispatches
  differently.

Open question for whoever needs 3D worldgen: whether 2 ms was ever the right
number for 110,592 samples of 3D gradient noise, or whether the opt-in path
should simply be understood as costing milliseconds and be scheduled
accordingly.

### What Task 02b already spent

Measured on a Ryzen 7 7800X3D — faster than minimum spec, so treat these as
optimistic and the ratios as the useful part:

| Subsystem | Cost | Share of a 50 ms tick |
|---|---|---|
| Collision, 100 players, chiselled terrain | 0.014 ms | 0.03% |
| Meshing one chunk, realistic content | 0.108 ms | client-side, not the tick |
| Full-chunk relight, realistic content | 0.059 ms | 0.1% |

Sub-node resolution is not the expensive part of this engine. When something
later is slow, that is where to look — not here.

### What Task 10 measured (light)

Same machine, `cargo bench -p tiamot-core --bench light`, against the real
implementation rather than 02b's spike.

| Operation | Cost | Share of a 50 ms tick |
|---|---|---|
| Full-chunk relight, open sky | 0.233 ms | 0.5% |
| Full-chunk relight, solid rock | 0.078 ms | 0.2% |
| Full-chunk relight, every block chiselled | 0.233 ms | 0.5% |
| Digging one block into a lit wall | 0.00012 ms | negligible |
| Placing a lamp | 0.119 ms | 0.24% |
| Breaking a lamp | 0.271 ms | 0.54% |

**The chiselled case costs the same as open air.** That is the cached
permeability byte earning charter rule 19: Task 02b measured the uncached face
test at a ≈50% penalty on exactly this content, and the penalty is gone.

**Read these against how many happen per tick, not on their own:**

- Newly loaded chunks are capped at `RELIGHTS_PER_TICK` (32), so the worst a
  tick can spend filling in terrain is **32 × 0.233 ms ≈ 7.5 ms, 15% of the
  budget**. That cap exists because a teleport or a fresh start can make
  thousands of chunks resident at once; what a tick does not reach it takes on
  the next one.
- Ordinary digging is free at any player count — 50 players digging flat out is
  0.006 ms a tick.
- **Lamps are the expensive edit.** Fifty players breaking lamps as fast as
  they can would be 13.5 ms a tick, 27% of the budget. That is a load nobody
  will generate by playing, and it is the number to remember if lighting ever
  looks like the problem.

02b's spike measured 0.059 ms for the same shape of work. The real
implementation is four times that, and the difference is honest rather than a
regression: it carries four channels instead of one, it seeds and floods
sunlight as well as block light, and it tests both facing layers per crossing
rather than one. The absolute figure still leaves lighting well inside its
share.

## Bandwidth targets

| | |
|---|---|
| Chiselling delta stream | < 32 KiB/min/player (measured: 2.79) |
| Compressed chunk, heavily chiselled | < 8 KiB (measured: 1,797 B) |

At 50 players all chiselling continuously, the delta stream is roughly
140 KiB/min — under 20 Kbit/s. Not a constraint.

## How to use these

- **Benchmarks report against the tick budget**, not in isolation. "0.4 ms" says
  nothing; "0.4 ms, 0.8% of a tick" says something.
- **Client work is measured on a real GPU** at full sub-node detail — frame
  *pacing*, not just average frame rate. A smooth 60 beats a stuttering 90.
- **A regression gate needs a stable baseline.** CI runners do not provide one
  (see `crates/core/benches/`); real numbers come from dev hardware.
- **Determinism is not negotiable for speed** (charter rule 4). If an
  optimisation would take simulation outside the Deterministic Float Subset, it
  is not an optimisation available to this project.

## Open

- **Frame pacing at full sub-node detail** — Task 08 human gate, on a discrete
  card.
- **Whether view distance 12 is the shipping target.** Raising it to 32 scales
  geometry roughly 7×; 172 MiB becomes ~1.2 GiB. Comfortable on a discrete card,
  which is now the target. Decide before Task 15b's LOD design.
