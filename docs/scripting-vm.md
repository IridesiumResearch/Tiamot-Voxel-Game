<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Scripting VM — Verdict

**Task 05. This decision is irreversible in practice.** Mod-visible language
semantics differ between the candidates, so changing backend later breaks every
mod ever written against the old one.

> ## DECISION: **Lua 5.4**
>
> **Mods are written against Lua 5.4.** That string goes in the modding
> documentation and does not change.
>
> Decided 2026-07-30 on the measurements below.

| | |
|---|---|
| **Measured on** | AMD Ryzen 7 7800X3D, `--release`, single-threaded |
| **Binding** | `mlua` 0.11, vendored |
| **Reproduce** | `cargo run --release -p tiamot-core --example vm_bench --no-default-features --features vm-lua54` (and `vm-luajit`, `vm-luau`) |

---

## 1. The measurements

| Workload | Lua 5.4 | LuaJIT | Luau | Spread |
|---|---|---|---|---|
| **Worldgen callback**, per chunk | 57.4 µs | **52.6 µs** | 54.1 µs | 1.09× |
| — same work native, same binary | 51.0 µs | 50.2 µs | 49.8 µs | |
| — **orchestration overhead** | 12.7% | 4.8% | 8.6% | *gate: < 20%* ✅ |
| **`on_step` × 1000**, one tick | 316.8 µs | 356.2 µs | **306.9 µs** | 1.16× |
| — per call | 0.317 µs | 0.356 µs | **0.307 µs** | |
| **Sandbox setup**, fresh VM + 1 mod | **37.8 µs** | 56.3 µs | 65.3 µs | 1.73× |
| **Sandbox setup**, extra mod, VM up | 11.3 µs | **10.0 µs** | 19.1 µs | 1.91× |
| **Trivial call boundary**, per call | 0.252 µs | 0.338 µs | **0.241 µs** | 1.40× |

The native baseline is the *identical* recipe run in Rust in the same binary,
not Task 04's recorded figure — comparing against a similar-but-different
workload would have produced an "overhead" that was really a difference between
two jobs.

## 2. The decision rule, applied

The task's rule: **default to Lua 5.4 if the spread is under ~2× on the
realistic workloads.**

The realistic workloads are the worldgen callback and `on_step`. The spread is
**1.09×** and **1.16×**. Not close to the threshold — the three backends are
within noise of each other on everything that matters.

Lua 5.4 wins by the stated rule. The rest of this document is why the rule gives
the right answer here rather than merely a defensible one — and §5 records a
containment failure in LuaJIT that would have ruled it out regardless of any
measurement.

## 3. Why the JIT does not help

**LuaJIT is the *slowest* backend on `on_step`.** That is worth sitting with,
because it is the opposite of the reason anyone reaches for LuaJIT.

The explanation is architectural, not a benchmarking artefact. This engine
deliberately keeps heavy computation out of Lua:

- Charter rule 4's Deterministic Float Subset **cannot be enforced inside a
  script VM**. Lua has one number type and no lint reaches into it; `x^0.5` in a
  mod is a libm call on whatever platform the server runs.
- So the API is shaped so scripts *orchestrate* native work rather than compute
  it. A generator asks for a heightmap and hands it to a fill. There is no
  exposed way to loop over samples in Lua, and that is not an oversight.

The result is that script time is dominated by **crossings into Rust**, not by
numeric loops. A tracing JIT has almost nothing to trace: it cannot compile
through an FFI boundary, and the loops that remain are short and
call-terminated. LuaJIT pays its extra call-boundary cost (0.338 µs against
0.252 µs) and gets nothing back.

**The design decision that protects determinism is the same one that removes
LuaJIT's reason to exist.** Choosing LuaJIT would mean accepting Lua 5.1
semantics forever in exchange for a speedup that this architecture cannot
realise.

## 4. Why Lua 5.4 over Luau, when Luau is marginally faster

Luau is genuinely the fastest on the two call-heavy workloads, and it was built
for exactly this problem — untrusted user code at scale. It is a serious
candidate and the margin is real, if small (3% on `on_step`).

It loses on one thing this project cares about disproportionately: **integers.**

Charter rule 5 makes the entire quantity system integer arithmetic. Amounts are
`u32` units; display is `units / 27` blocks plus `units % 27` nodes. That is
integer division and integer modulo, and mod authors will write it constantly —
in inventory code, in crafting, in anything that counts.

Under Lua 5.1 semantics, which both Luau and LuaJIT use, **every number is a
double**. There is no integer subtype and no `//` operator. `units / 27` gives
`3.7037…` where the mod author meant `3`. That does not fail loudly; it produces
a fractional block count that propagates silently until something displays
wrong. Every mod would carry `math.floor` around its arithmetic forever.

Lua 5.4 has an integer subtype, `//`, and `%` that behaves as expected on
integers. For an engine whose defining feature is 27-unit quantity arithmetic,
that is not a nicety.

Secondary points, in descending order of weight:

- **Familiarity.** Luanti/Minetest modders — the closest existing population to
  this project's — write Lua 5.1-era code, but current-semantics Lua 5.4 is what
  the wider ecosystem and every current tutorial teach. Luau's divergences
  (type annotations, different standard library corners) are a second dialect to
  learn.
- **Sandbox setup is 1.7× cheaper** on Lua 5.4 for a fresh VM. Minor, paid once.
- **Luau's sandboxing is first-class**, which is a real point in its favour. But
  the sandbox this engine needs is built above `mlua` anyway, identically on all
  three, because the deny-list and the `require` confinement are engine policy.

## 5. LuaJIT cannot contain untrusted mods — measured, not assumed

This was found by running the sandbox test suite under each backend, and it
would have decided the question on its own.

### The instruction budget does not work on LuaJIT

`while true do end` in a mod callback **runs forever** under `vm-luajit`. The
budget is implemented as a debug hook, and **LuaJIT's debug hook does not fire
inside JIT-compiled traces** — a hot loop compiles, leaves the interpreter, and
never returns to be counted.

This is not a performance nuance. Charter rule 10 requires that a mod error
disables that mod and never kills the tick. Under LuaJIT, one bad loop in one
mod hangs the server permanently, with no error, no traceback, and nothing to
attribute it to.

Verified directly: `an_infinite_loop_is_stopped_by_the_budget` passes on Lua 5.4
and Luau, and **hangs** on LuaJIT. The test is compiled out there rather than
left to hang CI, and a second test asserts that LuaJIT is not the default so the
hole cannot be reintroduced quietly.

The only mitigation is `jit.off()` — which discards the JIT, and with it the
entire reason to have chosen LuaJIT.

### LuaJIT cannot enforce a memory limit either

Its allocator is tied to its own GC and `mlua` cannot install a ceiling. A mod
that allocates without bound takes the server with it.

**Two of the three containment primitives this engine needs are simply
unavailable on LuaJIT.** For an engine whose entire purpose is hosting
third-party code, that is disqualifying on its own, before any benchmark.

Also: `ffi` is an arbitrary-memory-access primitive and is removed
unconditionally; upstream is effectively frozen; and some Apple-silicon
configurations require interpreter-only mode, which — as above — discards the
JIT that was the entire reason to consider it.

## 6. What this decision does *not* settle

- **The `ScriptVm` trait stays.** All three backends remain selectable by cargo
  feature, and the benchmark stays runnable. A future WASM tier plugs in the same
  way. The trait is not speculative generality — it is what made this
  measurement possible, and re-running it on new hardware or a new `mlua` is a
  one-line change.
- **Client-side scripts** (charter rule 10's hard sandbox for scripts pushed by
  untrusted servers) are Task 14's problem and may reach a different conclusion.
  Luau's containment story is stronger, and the client tier has no integer-
  arithmetic requirement to trade against it.

## 7. Consequences to write down

1. **Mods are written against Lua 5.4.** Modding docs say so, `api/stubs/game.lua`
   is annotated for it, and it does not change.
2. `vm-lua54` is the default cargo feature. `vm-luajit` and `vm-luau` build and
   pass the sandbox tests, and exist for benchmarking and for the record.
3. The backend is written into the world's meta, because a world generated by
   mods on one dialect is not guaranteed to regenerate identically on another.
4. **Scripts must not perform simulation-relevant float maths.** Enforced by API
   shape rather than documentation: there is no per-sample entry point and no
   float accessor on a random stream. See `crates/core/src/script/`.
