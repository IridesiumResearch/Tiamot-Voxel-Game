<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Entity store — Verdict

**Task 12.** The task names three candidates for `crates/core::ent` — `bevy_ecs`,
`hecs`, and a minimal hand-rolled store — and sets the weighting: *deterministic
iteration order and dependency weight in `core` count at least as heavily as raw
query speed, because entity counts here are small.*

> ## DECISION: **a hand-rolled generational arena**
>
> `crates/core::ent` owns its own store. No ECS library enters `crates/core`.
>
> Decided 2026-08-17 on the measurements below. **The mod-facing API is
> identical either way**, which is what keeps this reversible: it is an
> implementation choice behind `game.spawn_entity` and friends, not a contract.

| | |
|---|---|
| **Measured on** | this devcontainer, `--release`, single-threaded |
| **Versions** | `bevy_ecs` 0.19, `hecs` 0.11 |
| **Reproduce** | `cargo run --release -p ecs-spike` |

---

## 1. The measurements

Tick budget is 50 ms shared by all simulation for all players (charter rule 18).
`step` — integrate every entity's position — is the only column that runs 20
times a second forever, and it is reported as a share of that budget.

| store | entities | step | spawn all | spawn + despawn half | radius query | attach half | detach all |
|---|---|---|---|---|---|---|---|
| **hand-rolled** | 200 | **0.11 µs** (0.0002%) | **0.85 µs** | **0.94 µs** | **0.26 µs** | **0.04 µs** | **0.04 µs** |
| hecs | 200 | 0.12 µs (0.0002%) | 4.85 µs | 6.24 µs | 0.27 µs | 1.58 µs | 1.02 µs |
| bevy_ecs | 200 | 0.38 µs (0.0008%) | 10.82 µs | 14.64 µs | 0.42 µs | 1.76 µs | 1.69 µs |
| **hand-rolled** | 2000 | 1.32 µs (0.003%) | **5.74 µs** | **6.97 µs** | 2.67 µs | **0.74 µs** | **0.77 µs** |
| hecs | 2000 | **1.12 µs** (0.002%) | 41.98 µs | 57.14 µs | **2.49 µs** | 15.50 µs | 10.18 µs |
| bevy_ecs | 2000 | 1.66 µs (0.003%) | 123.56 µs | 159.99 µs | 2.32 µs | 18.77 µs | 17.76 µs |

Dependency weight, which the task asks to weigh as heavily:

| store | crates added to `core` | clean release build of the dependency |
|---|---|---|
| **hand-rolled** | **0** | **0 s** |
| hecs | 4 (`hecs`, `hashbrown`, `foldhash`, `spin`) | 1.45 s |
| bevy_ecs | 65 | 15.53 s |

## 2. Determinism does not separate them, and that is worth saying plainly

The gate was that two worlds built by an identical sequence of calls — spawn,
attach, despawn, respawn, detach — must iterate in an identical order. Rust's
`RandomState` is seeded per process and **advances per instance**, so a store
that iterates a std `HashMap` internally hands two worlds in ONE run different
orders. That is precisely the charter rule 4 failure, and it is invisible if you
only ever look at one world.

**All three passed.** No candidate was eliminated here.

That is a real finding rather than a formality: the expectation going in was
that an archetype ECS might fail it, since archetype discovery order is an
emergent property of the library's internals rather than anything the caller
states. It does not. `hecs` and `bevy_ecs` both key their internals on ordered
or fixed-hash structures.

So determinism is a **prerequisite all three meet**, and the decision has to be
made on the other two axes. Anyone re-opening this should not re-run the
determinism argument expecting it to decide anything.

## 3. Speed does not separate them either, where it matters

At the task's own perf gate — 200 scripted wandering entities — stepping every
one of them costs **0.11 µs, 0.12 µs, or 0.38 µs**. The worst of those is
**0.0008% of a tick**. Multiply by ten and it is still nothing.

Reporting these as a share of the budget rather than in isolation (charter rule
18) is what makes the point: the entity store is not where a tick goes, at any
scale this engine has. `bevy_ecs` is 3.5× the hand-rolled store on `step` at 200
entities and it does not matter, because 3.5× nothing is nothing.

Where the spread IS large — spawn, despawn, attach, detach, all 4–20× — it is
also not on the tick. Those run when a chunk thaws or a mod edits an entity.
They are worth having cheap; they are not worth choosing a dependency for.

## 4. What actually decides it

### The engine does not have the problem an ECS solves

The component set is **fixed and known at compile time**: Transform, Velocity,
collider, ModelRef, AnimState, Health, Nametag, Owner. An ECS earns its
archetypes when component combinations are discovered at runtime and queries
must find them efficiently. Here every combination is known when the engine is
compiled, and the query the mod API exposes — `entities_in_radius` — is a
distance test that no archetype layout helps with.

Mods do attach their own state, and that is the one place a general store would
help. It does not: **a mod attaches a Lua table**, which the engine holds as one
opaque handle. It is one component type shared by every mod, not a type per mod.
The archetype machinery would be carried and never used.

### 65 crates is not a detail in `crates/core`

`bevy_ecs` brings an async task runtime (`async-executor`, `async-task`,
`bevy_tasks`, `futures-lite`, `concurrent-queue`) and a reflection system
(`bevy_reflect`, `erased-serde`, `downcast-rs`) into the crate charter rule 3
exists to keep narrow — the crate that must stay free of anything that is not
voxel data, simulation, scripting, physics, persistence, and protocol. The
firewall script only checks for render, window and audio crates, so none of this
would trip it; that is an argument for judgement, not for the absence of a
tripwire.

It is also 15.5 s of clean build for the dependency alone, on every CI leg of
the matrix, three platforms.

`hecs` at 4 crates is genuinely cheap and this argument does not reach it. It is
ruled out by §4.1 and §4.3, not by weight.

### `bevy_ecs` needs `&mut World` to run a query

`World::query` caches state, so an uncached read is an exclusive borrow. That
collides directly with the shape of this engine's mod API: a mod's `on_step`
runs *inside* the engine's step, and `game.entities_in_radius` called from there
would be asking for the world mutably while the engine already holds it. It is
solvable — build every `QueryState` up front — but it is a constraint imported
from a library, applied to the API charter rule 1 says is the only API.

The spike's `Store` trait takes `&mut self` for its read-only methods **only**
because `bevy_ecs` left no choice; the other two are happy with `&self`.

### Owning the iteration order outright

Charter rule 4 requires that simulation results never depend on iteration order,
and charter rule 8 requires entity state to round-trip byte-for-byte through the
world database. Both are easier to hold when the store is a `Vec` we index in
order and a free list we sort. The measurements say the library candidates would
also hold them today; the hand-rolled store holds them *by construction*, and
cannot quietly stop holding them in a minor version bump.

## 5. What this costs, honestly

- **We own the bugs.** A generational arena is small and well-understood, but
  use-after-free across a despawn is now ours to prevent rather than a library's.
  Generations exist for exactly that and every id is checked.
- **No free parallelism.** `bevy_ecs`'s scheduler could run systems across
  threads. This engine cannot use that anyway: charter rule 4 forbids float
  reductions over non-deterministic order, and the tick is deliberately not
  async.
- **If the shape changes, revisit.** The argument in §4.1 is about *this*
  engine's component model. If entity counts reach six figures with genuinely
  heterogeneous runtime component sets, the trade turns over and `hecs` — not
  `bevy_ecs` — is where to look first. It is 4 crates and it was within noise of
  the hand-rolled store on the only column that runs every tick.

## 6. The spike

`spikes/ecs` is throwaway measurement code, excluded from `default-members` so a
normal build never produces it. **The two candidate libraries enter the lockfile
there and nowhere else**, so the decision above is also the reason `cargo tree`
for the shipped engine stays as it is.
