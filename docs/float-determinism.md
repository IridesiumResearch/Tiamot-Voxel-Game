<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# The Deterministic Float Subset

**Authoritative. Charter rule 4.** This document defines which floating-point
operations simulation code may use, which it may not, and why.

**Edit this document before changing the subset — never after.** The
`disallowed-methods` list in `clippy.toml` is the enforcement; this is the
reasoning. A lint entry without an entry here is an unexplained rule, and an
unexplained rule gets silenced by the next person who hits it.

---

## The requirement

Same seed ⇒ **bit-identical** worlds on Linux, Windows, and macOS. Not
"visually identical", not "within epsilon" — the CI hash gate compares
`u64` fingerprints across three operating systems, and one differing bit fails
the build.

## The decision: restrict the operations, don't leave the FPU

The instinctive answer to cross-platform determinism is fixed-point arithmetic.
This project does not use it, and the reason is that **it was never the source
of the problem.**

Rust guarantees that `+ - * / %`, `sqrt`, `abs`, `copysign`, and comparisons on
IEEE floats produce results exactly matching IEEE 754-2008. Rust has **no
fast-math mode**: the compiler may not contract `a * b + c` into an FMA, and
LLVM may not reassociate float expressions without flags Rust never sets. The
same sequence of allowed operations therefore produces bit-identical results on
every supported target.

Non-determinism comes from a specific, enumerable set of operations — chiefly
platform libm — not from floats as such. Removing those is enough.

It is also the fast choice, which is why it is the choice
(`docs/performance-targets.md`):

- Fixed-point costs a widening 64-bit multiply and a shift-normalisation per
  sample, and strands the FPU and SIMD units entirely.
- The allowed subset is elementwise, so LLVM auto-vectorises bulk noise fills
  freely. Only *reductions* need reassociation, and the sample path has none.
- Nothing is lost in capability: gradient noise mathematically requires no
  transcendental at all.

---

## Allowed

| Operation | Note |
|---|---|
| `+` `-` `*` `/` `%` | IEEE-exact, guaranteed by Rust |
| `sqrt` | An IEEE 754 operation, correctly rounded, hardware instruction on both supported targets |
| `abs` `copysign` | Bit manipulation, no rounding |
| Unary negation | Sign-bit flip |
| Comparisons (`<` `<=` `==` …) | Exact |
| `to_bits` / `from_bits` | Pure reinterpretation |
| Float ↔ integer casts (`as`) | Rust's are **saturating and fully defined** — no UB, no platform variance |
| `f32` ↔ `f64` conversion | IEEE-exact in both directions |
| `min` / `max` | See the caveat below |

### `min` / `max`, with a caveat

`f32::min` and `f32::max` are deterministic and allowed. Their NaN handling
differs from a naive `if a < b` comparison, but since **producing NaN is itself
banned**, that difference is unreachable in conforming code.

### Constants may be computed however you like

A constant is evaluated once, by the compiler, and committed to the binary. It
does not matter that `(3.0f64.sqrt() - 1.0) / 2.0` involves a square root —
write the resulting literal, or a `const` expression, and the runtime path never
sees it. **The subset restricts runtime operations on simulation data, not the
provenance of your constants.**

---

## Banned in `crates/core` simulation paths

### 1. libm transcendentals

```text
sin cos tan asin acos atan atan2 exp exp2 ln log log2 log10
powf powi cbrt hypot exp_m1 ln_1p sin_cos
```

**Why:** these are not IEEE 754 operations. They are library functions with no
correctly-rounded requirement, and their implementations differ between
operating systems, libc versions, and CPU generations. Glibc, musl, MSVC's CRT,
and Apple's libm all give different last bits for the same input. **No Rust
guarantee covers them at all.**

`powi` is on the list despite looking integral: it lowers to repeated
multiplication whose *association order* LLVM may choose. Write the
multiplication out.

**Replacements:** gradient noise needs gradient-table lookups and dot products —
no transcendental. If simulation ever genuinely needs a trig value, use a
committed lookup table with linear interpolation: deterministic by construction,
and faster besides.

### 2. `mul_add`

**Why:** it uses a hardware FMA where one exists and a software fallback where
one does not, and **the two round differently** — FMA rounds once, `a * b + c`
rounds twice. A machine with FMA and a machine without produce different bits.

**Replacement:** write `a * b + c`. Rust forbids the compiler from contracting
it back into an FMA, so this is stable.

### 3. Producing `NaN` in simulation state

**Why:** NaN *payloads* are explicitly not specified. Two platforms can both
produce "a NaN" from the same operation with different bit patterns, and a hash
over that state then differs. NaN also propagates, so one appearance
contaminates everything downstream.

Simulation code must not generate NaN. Debug builds assert on it, and the
no-NaN property test in `detgen` covers the noise parameter space.

### 4. Float reduction over non-deterministic iteration order

**Why:** float addition is not associative. `(a + b) + c` and `a + (b + c)`
differ in the last bit. Summing over `HashMap` iteration order, a `rayon`
reduction, or an unordered `sum()` therefore gives a result that depends on
scheduling.

Rust's default hasher is **randomly seeded per process**, so `HashMap` order is
not stable even between two runs on the same machine. Use `BTreeMap`,
`IndexMap`, or a sorted `Vec`, and sum in a fixed order, always.

### 5. `floor` `ceil` `round` `trunc` — banned, and this one is subtle

**Why, and it is not the reason you would guess:** these *are* IEEE 754
roundToIntegral operations and *are* exactly specified. The problem is codegen.

`roundss` — the instruction that implements them in one step — is **SSE4.1**.
The supported x86_64 baseline is **SSE2**. On an SSE2-only target LLVM cannot
emit `roundss`, and `f32::floor` becomes **a call to platform libm** — putting
it straight back into category 1, silently, with no source-level hint that
anything changed.

**Replacement:** integer casts, which are exact and fully defined:

```rust
/// Floor, without libm. Truncation plus a correction for negatives.
fn floor_to_i32(x: f32) -> i32 {
    let truncated = x as i32;      // toward zero, saturating, defined
    truncated - i32::from(x < truncated as f32)
}
```

`detgen` uses this throughout. It is also faster than a libm call.

### 6. 32-bit x86 (i686)

**Why:** x87 computes intermediates at 80-bit extended precision and rounds when
spilling to memory, so results depend on register pressure — which depends on
the optimiser's mood. There is no way to write conforming code against it.

**Supported targets are x86_64 (SSE2 baseline) and aarch64. i686 is
unsupported and is not built in CI.**

---

## The denormal hazard

This is the one that gets missed, because nothing in your code is wrong.

Flush-to-zero (FTZ) and denormals-are-zero (DAZ) are **thread-local CPU state** —
`MXCSR` on x86, `FPCR.FZ` on ARM — not properties of the program. With them set,
subnormal results are silently replaced by zero. Identical code on identical
input then produces different bits depending on a mode register nobody in the
simulation set.

**Audio backends and GPU drivers set these process-wide.** It is a normal thing
for them to do: denormals are slow, and audio and graphics do not care about the
last bit. A game with `kira` and `wgpu` in it is exposed by construction.

### The guard

`detgen::assert_ieee_mode()` evaluates a fixed subnormal expression and compares
the result's bit pattern against a committed constant. Under FTZ or DAZ the
result is zero and the check fails loudly, naming the likely cause.

It runs:

- on **every simulation thread at spawn**, before any simulation work, and
- as a unit test, alongside a test that deliberately sets FTZ/DAZ and asserts
  the guard catches it.

The probe input is passed through `black_box` so the expression cannot be
constant-folded at compile time — a folded probe would report the compiler's
answer, not the CPU's, and would pass on a machine that was actually broken.

### The standing rule

**Simulation never runs on a thread owned by an audio or driver callback.** The
guard catches the mistake; the rule prevents it.

---

## Scope

Determinism is required for:

- worldgen
- the simulation tick — physics, fluid, light, entity stepping
- everything in the CI hash gate

It is explicitly **not** required for rendering, audio, UI layout, camera
smoothing, or client-side interpolation. Do not tax presentation code with these
rules; it has different constraints and no cross-machine agreement to maintain.

### Presentation code that lives in `crates/core` (Task 12)

The lint is scoped to the crate and the exemption is scoped to the *purpose*, so
the two disagree wherever presentation code sits in `core`. There is one such
place, and it is deliberate: `core::model` — the glTF reader, the built-in
humanoid rig and the animation sampler.

It is in `core` because it must be testable and **fuzzable without a GPU**
(charter rule 14 asks for a fuzz target in the same task as the parser), and
because the server has no business linking a renderer to validate a file. None
of it feeds the simulation: the server never calls it, no chunk hash depends on
it, and two clients disagreeing about an elbow by a float's last bit cannot make
two worlds disagree about anything.

So `core::model::humanoid` and `core::model::animate` carry a narrow
`#[allow(clippy::disallowed_methods)]` at the use site, naming this section.
`sin` and `cos` build quaternions there; a lookup table would cost precision in
the one place nobody can measure it and buy an agreement nobody needs.

**This is the only exemption inside `core`, and it stays that way.** A future
module that wants one is a module that should ask whether it belongs in `core`
at all.

### Audio (Task 13)

`crates/client::audio` uses `log10` to turn an amplitude into the decibels its
backend thinks in, and `sqrt` to measure how far away a sound is. Both are on
the banned list and both are fine here, for the reason the Scope section gives:
**audio is presentation.**

It is worth saying why this one cannot become a problem even by accident. The
sound API is one-way — a mod calls `game.play_sound` and gets back a count of
who was told, and there is no call that asks how loud anything was. So no
simulation state can depend on an audio float, whatever the client computes.
The lint fires because `clippy.toml` is workspace-wide rather than
crate-scoped, which is a property of clippy and not a claim about this code;
the exemption is at the use site and names this section.

---

## Enforcement

- `clippy.toml` `disallowed-methods` lists every banned function, with CI running
  `clippy -D warnings` (Task 01).
- Demonstrated firing on a deliberate violation in Task 04, then reverted.
- The golden-hash gate in `detgen` runs on all three OS legs of the CI matrix. A
  mismatch fails the build.

**Never silence the lint to make a hash match.** A cross-platform hash mismatch
is a real bug in the simulation; the lint is how you find it. Suppressing it
converts a build failure into a bug report from a player whose world generated
differently from their friend's.
