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
