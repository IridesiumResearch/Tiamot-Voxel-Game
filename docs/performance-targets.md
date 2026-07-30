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
| GPU | **Integrated graphics** |

Anything better is headroom. Anything worse is unsupported.

**The GPU line is the demanding one.** Integrated graphics share system memory
and have a small fraction of a discrete card's fill rate and memory bandwidth.
Two consequences that shape Task 08 and everything after it:

- **Fill rate and bandwidth bind before VRAM does.** Task 02b measured that a
  fully chiselled view-distance-12 world is 172 MiB of geometry — comfortable.
  That is not the constraint. Overdraw, shading cost, and the bandwidth to feed
  them are, and none of them existed to measure at 02b.
- **Frame time must be measured on a real integrated GPU.** Not projected from a
  discrete card, not extrapolated from geometry counts. This is a human gate on
  Task 08.

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
- **Client work is measured on integrated graphics** or it is not measured.
- **A regression gate needs a stable baseline.** CI runners do not provide one
  (see `crates/core/benches/`); real numbers come from dev hardware.
- **Determinism is not negotiable for speed** (charter rule 4). If an
  optimisation would take simulation outside the Deterministic Float Subset, it
  is not an optimisation available to this project.

## Open

- **Frame time on integrated graphics** — Task 08 human gate.
- **Whether view distance 12 is the shipping target.** Raising it to 32 scales
  geometry roughly 7×; 172 MiB would become ~1.2 GiB, which is where an iGPU
  sharing system RAM starts to matter. Decide before Task 15b's LOD design.
