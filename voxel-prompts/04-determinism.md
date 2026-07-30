# TASK 04 — Deterministic worldgen primitives: noise, RNG streams, hash tests

Depends on: 02, 03. All code in `crates/core` (module `detgen`).

## Objective
Engine-owned, cross-platform bit-identical randomness and noise — **at full float speed**.
Worldgen POLICY is mods (later); these are the MECHANISMS. Do not add any terrain logic here
beyond test fixtures.

## The determinism decision (settled — implement, do not re-litigate)
Charter rule 4 is the specification. The short version, because this task is where it lands:

**Use `f32`. Do not use fixed-point.** Rust guarantees `+ - * / %`, `sqrt`, `abs`, `copysign`
and comparisons match IEEE 754-2008 exactly, and Rust has no fast-math mode — the compiler
may not contract `a*b + c` into an FMA, and LLVM may not reassociate float expressions
without flags Rust never sets. The same sequence of these operations is bit-identical on
every supported target. Determinism comes from *restricting which operations you use*, not
from abandoning the FPU.

This is also the fast choice, which is why it is the choice:
- Fixed-point would cost widening 64-bit multiplies and shift-normalisation per sample and
  strand the FPU/SIMD units entirely.
- The allowed float subset is elementwise, so LLVM auto-vectorises the bulk noise fills
  freely. (Only *reductions* need reassociation, and the sample path has none.)
- Nothing is given up in accuracy or capability: gradient noise mathematically requires no
  transcendental at all.

### Deliverable 1 — `docs/float-determinism.md` (write this BEFORE the noise code)
- The allowed subset: `+ - * / %`, `sqrt`, `abs`, `copysign`, negation, comparisons,
  `to_bits`/`from_bits`, float↔int casts (Rust's are saturating and fully defined).
- The banned list and *why each entry* breaks determinism: libm transcendentals
  (`sin cos tan asin acos atan atan2 exp exp2 ln log log2 log10 powf powi cbrt hypot
  exp_m1 ln_1p sin_cos`), `mul_add`, NaN production, float reduction over non-deterministic
  iteration order, i686/x87 excess precision.
- Supported targets: x86_64 (SSE2 baseline) and aarch64. i686 explicitly unsupported.
- The denormal/FTZ hazard and how `assert_ieee_mode()` catches it.
- The rule that this doc is edited before any change to the subset.

### Deliverable 2 — enforcement, not intention
- `clippy.toml` with a `disallowed-methods` entry for every banned function, scoped so
  `crates/core` fails the build on use. CI already runs `clippy -D warnings` (Task 01).
- **Demonstrate the lint fires:** add `let _ = x.sin();` to a core sim path, capture the CI
  failure output in the PR, revert.
- `detgen::assert_ieee_mode()` — evaluates a fixed subnormal expression and compares the
  result's bit pattern against a committed constant; panics with a diagnostic naming FTZ/DAZ
  if the CPU is in flush-to-zero mode. Called at simulation-thread spawn, covered by a unit
  test. Document that audio backends and GPU drivers are the known culprits for setting FTZ
  process-wide, and that simulation must never run on a thread owned by them.

## Design
- Noise (implement in-crate; do not pull a noise crate — determinism must be ours):
  - 2D/3D OpenSimplex-style gradient noise, seeded, plus fBm/ridged/billow combinators
    (octaves, lacunarity, gain — all explicit params).
  - Every step stays inside the allowed subset: integer hash → gradient-table index, dot
    products, and the polynomial fade curve `6t⁵ − 15t⁴ + 10t³` in Horner form with the
    multiply and add **written out separately** (never `mul_add`).
  - Bulk fill APIs: `fill_2d(buffer, region, params)`, `fill_3d(...)` — what Lua calls once
    per chunk; single-call whole-buffer fills, no per-sample FFI. Write the inner loops to
    auto-vectorise (flat slices, no branches, no accumulator reused across lanes). Confirm
    with a bench and record in the PR whether vectorisation actually happened (check the
    emitted asm or use `cargo-show-asm`); if it did not, say so rather than assuming.
- RNG: SplitMix64-seeded xoshiro256++ implemented in-crate — pure integer, trivially
  deterministic. `StreamRng::new(world_seed, chunk_pos, stream_name: &str)`; stream name
  hashed with a stable FNV-1a. Same inputs ⇒ same sequence, forever. Changing the stream
  name decorrelates.
- `ChunkBuffer` worker object: owns a scratch region for one chunk. Per the Sub-Node Contract
  (Task 02b): BLOCK-level ops are the primary, cheap path (backed by 16³ storage until a
  sub-node op is used — lazily expand to 48³ only then, so default worldgen never pays the
  27×). Ops worldgen will orchestrate: `fill_all(material)`,
  `fill_below_heightmap(heights, material)`, `set/get subnode`, `blit`,
  `to_chunk() -> Chunk` (canonicalization + palette build). This is the object handed to Lua
  generator callbacks in Task 05.
- Golden-hash harness: `detgen::fingerprint(world_seed, chunk_pos) -> u64` — fills a buffer
  with a fixed fBm recipe + RNG salt and hashes it (xxhash or FNV over bytes). Commit golden
  values for ~16 (seed, pos) pairs in a test.

## Tests
- [A] Golden hashes pass; the CI matrix makes this the cross-platform determinism gate — a
  mismatch on any OS fails the build.
- [A] RNG known-answer tests against reference xoshiro256++ vectors.
- [A] `assert_ieee_mode()` unit test, plus a test that deliberately enables FTZ (a small
  `#[cfg(test)]` MXCSR/FPCR poke) and asserts the guard catches it.
- [A] Noise sanity: value range bounds, seed sensitivity, continuity spot-checks.
- [A] No-NaN invariant: proptest over the parameter space asserts no fill ever yields NaN or
  infinity.
- [A] proptest: `fill_below_heightmap` then `to_chunk` equals naive reference construction.
- [A] Bench BOTH paths: block-level `fill_2d`/`fill_below_heightmap` over one 16³ chunk (the
  DEFAULT worldgen path) and `fill_3d` over a 48³ sub-node buffer (the opt-in path).

## Acceptance criteria
- [A] Golden hashes identical on ubuntu, windows, macos CI legs.
- [A] `docs/float-determinism.md` exists; the `disallowed-methods` lint is demonstrated
      firing on a deliberate violation (output in the PR), then reverted.
- [A] Default block-level chunk fill under 200 µs in release on CI hardware; opt-in 48³
      `fill_3d` under 2 ms. Record actuals — Task 05's Lua-overhead budget is measured
      against these numbers.
- [A] Zero terrain policy in the module (grep: no "grass", "dirt", biome logic — fixtures in
      tests only).
