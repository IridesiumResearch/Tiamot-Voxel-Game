# PROJECT CHARTER — read before every task
# Save this file as CLAUDE.md in the repo root so Claude Code always loads it.

## What this project is
A multiplayer voxel engine whose entire purpose is to be a platform for mods. The engine
contains MECHANISMS only. All content is Lua mods.

**Scope discipline — read this before proposing any feature.** The engine is deliberately
small. The mods in `game/` are *reference implementations and test fixtures* that prove each
mechanism works through the public API — they are NOT the shipped game. A real default game
is a later phase, built as mods on top of a finished engine. Do not add content, tune game
feel, or design progression in these 18 tasks. If a task tempts you toward "the game needs
X," the answer is that a future mod needs X, and the engine's job is to make X expressible.

## Non-negotiable architecture rules

1. **The mod API is the only API.** The default reference mods ship as Lua in `game/`.
   If a feature can't be built through the mod API, fix the API — never special-case core.

2. **The headless server is the game; the client is a viewer.** Singleplayer = embedded
   server over loopback. There is exactly one simulation code path.

3. **Crate hygiene is enforced by the compiler.** Cargo workspace:
   - `crates/core`   — voxel data, simulation, Lua runtime, physics, persistence, protocol types.
     MUST NOT depend on wgpu, winit, audio, or any windowing/render/input crate.
   - `crates/server` — thin binary over core. Headless. No GUI deps, no X11.
   - `crates/client` — core + wgpu/winit/kira/egui.
   - `crates/bot`    — scripted headless client for tests/benchmarks/load.

4. **Determinism — the Deterministic Float Subset.** Fixed timestep. Same seed ⇒ bit-identical
   worlds on Linux/Windows/macOS. The mechanism is NOT fixed-point arithmetic; it is a
   restricted subset of `f32`/`f64`, which is both deterministic and fast.
   - **Why plain floats work.** Rust guarantees that `+ - * / %`, `sqrt`, `abs`, `copysign`
     and comparisons on IEEE floats produce results that exactly match IEEE 754-2008, and
     Rust has no fast-math mode: the compiler is forbidden from contracting `a*b + c` into an
     FMA, and LLVM cannot reassociate float expressions without fast-math flags Rust never
     sets. Identical operation sequences therefore give bit-identical results on every
     supported target. (RFC 3514, "float semantics".)
   - **What breaks it, and is therefore BANNED in `crates/core` simulation paths:**
     - libm transcendentals: `sin cos tan asin acos atan atan2 exp exp2 ln log log2 log10
       powf powi cbrt hypot exp_m1 ln_1p sin_cos`. These call platform libm and differ
       between OSes, libc versions, and CPUs. **Not covered by any Rust guarantee.**
     - `mul_add` — uses a hardware FMA where available and a software fallback elsewhere;
       the two round differently. Write `a * b + c` explicitly.
     - Producing `NaN` in simulation state. NaN *payloads* are explicitly non-deterministic.
       Sim code must not generate NaN; debug builds assert on it.
     - Float accumulation over non-deterministic iteration order (`HashMap` iteration,
       `rayon` float reductions, unordered `sum()`). Sum in a fixed order, always.
     - 32-bit x86 (x87 excess precision). **Supported targets are x86_64 (SSE2, which is
       baseline) and aarch64 only.** i686 is unsupported and not built in CI.
   - **Replacements.** Gradient noise needs no transcendentals: gradient-table lookups, dot
     products, and a polynomial fade curve (`6t⁵ − 15t⁴ + 10t³`) are all in the allowed
     subset. If simulation ever genuinely needs a trig value, use a committed lookup table
     with linear interpolation — deterministic by construction.
   - **Denormal hazard.** Flush-to-zero / denormals-are-zero is *thread-local CPU state*
     (MXCSR on x86, FPCR.FZ on ARM), and audio backends and GPU drivers are known to set it
     process-wide. `detgen::assert_ieee_mode()` evaluates a known denormal expression and
     compares bit patterns; it runs on every simulation thread at spawn and as a unit test.
     Simulation never runs on a thread owned by an audio or driver callback.
   - **Enforcement.** The banned list is a `clippy.toml` `disallowed-methods` entry scoped to
     `crates/core`, `-D warnings` in CI. `docs/float-determinism.md` is the authoritative
     write-up; it lists the allowed subset, the banned list, and the reasoning, and any new
     entry requires editing that doc first.
   - **Scope.** Determinism is required for worldgen, the simulation tick (physics, fluid,
     light, entity stepping), and everything in the CI hash gate. It is explicitly NOT
     required for rendering, audio, UI layout, camera smoothing, or client-side
     interpolation — do not tax presentation code with these rules.
   - All worldgen/weather randomness comes from engine-provided seeded noise + per-chunk RNG
     streams (`world_seed + chunk_coords + stream_name`). No `HashMap` iteration order in
     simulation results; use `BTreeMap`, `IndexMap`, or sorted `Vec`. (Rust's default hasher
     is randomly seeded per process — `HashMap` order is not even stable run-to-run on one
     machine.)

5. **Units.** 1 block = 1 yard³ = 3×3×3 sub-nodes = 27 units. All inventory quantities are
   stored in units (u32). Display = `units / 27` blocks + `units % 27` nodes. No special
   cases for partial blocks anywhere.

6. **Chunks.** Cubic, 16×16×16 blocks (48³ sub-nodes). Palette-compressed. World is
   procedurally 120,000³ blocks; only modified/generated chunks persist.
   **16³ is load-bearing, not arbitrary:** a 48-cell sub-node column fits in a `u64` with
   room for the 2 padding bits that binary greedy meshing needs for neighbour-face culling
   (48 + 2 = 50 ≤ 64). A 32³ block chunk would be 96 sub-nodes per axis, breaking the
   single-word column and inflating remesh-after-one-edit cost. Do not change this number
   without redesigning the mesher.

7. **Coordinates.** Floating origin. Authoritative positions are `(i32 chunk coords, f32 local)`.
   Never accumulate world-space f32.

8. **IDs.** String IDs (`"core:white"`) are canonical. Numeric runtime IDs are per-session,
   never stable across runs. The world DB owns the string→numeric table. Unregistered IDs map
   to a preserved `engine:unknown` placeholder; data round-trips byte-for-byte.

9. **Registries freeze.** Lifecycle: manifest scan → dependency resolve → load (topo order) →
   registration window → FREEZE → world load → play. `register_*` after freeze is a hard error.

10. **Mods are Lua.** Server mods: sandboxed for crash isolation (a mod error disables that
    mod, never kills the tick). Client scripts pushed by servers: hard sandbox — no fs, no
    net, no `os`/`io`/binary `load`, instruction + memory caps. The VM is chosen by
    measurement in Task 05 and sits behind a trait so it can be swapped; mod-visible language
    semantics are frozen once that decision is made.

11. **Input:** mods register named actions; the engine owns key bindings. Mods never read keys.

12. **Sub-Node Contract.** [`docs/subnode-contract.md`](docs/subnode-contract.md) **exists as of
    Task 02b** and is the single authoritative definition of how every system treats
    Partial/Mixed blocks. Any PR touching collision, lighting, fluid, meshing, worldgen, or
    pathfinding cites the contract line it implements. New sub-node semantics require editing
    the contract first. Read it before touching any of those systems — in particular §1, the
    `u64`-column invariant, which is why chunks are 16³ and must not be resized.
    The spike's measurements and the keep/limits/fallback decision are in
    [`docs/subnode-verdict.md`](docs/subnode-verdict.md); **Tasks 08 and 09 do not start until
    that decision is recorded.**

13. **Player identity is cryptographic, and recoverable.** Identity is an Ed25519 key, but a
    key you can only lose once is a design defect, not security. The full model:
    - **Seed.** 256 bits of entropy generated client-side on first run. An Ed25519 secret key
      *is* 32 bytes of entropy, so the seed is the key — no derivation path needed.
    - **Recovery phrase.** That seed is presented once as a checksummed BIP-39 24-word
      mnemonic ("write this down"), and recoverable later via an explicit client command.
      `--restore-from-phrase` reconstructs the identity on any machine. This is the primary
      remedy for a lost or wiped device.
    - **Key sets, not single keys.** An identity is a *set* of authorized public keys (the
      passkey/WebAuthn model). Adding a device requires a signature from an existing key.
      Canonical player UUID = BLAKE3 of the ROOT public key and never changes, whatever
      happens to the set.
    - **Pre-committed rotation.** Each key registers the *hash* of its designated successor.
      Rotation publishes the successor and proves it matches the commitment, signed by the
      current key. A stolen current key therefore cannot hijack the rotation (KERI
      pre-rotation). Rotation records are persisted and replayable.
    - **Admin escape hatch.** RCON `rebind <uuid> <new-root-pubkey>` for the player with no
      phrase and no second device. Audit-logged, deliberately manual.
    - **Honest limit — keys are sybil-cheap.** Anyone can mint unlimited identities, so
      UUID bans are trivially evaded. Do not pretend otherwise in docs or code comments.
      Complements: IP/subnet bans, allowlist mode, and a pluggable `AuthProvider` trait so a
      community identity service can be added later without a protocol break.
    - Display names are a per-server claim bound to a UUID on first join. Names are display
      strings; UUIDs are identity. Engine and mods (inventory, ownership, bans, storage,
      imprints) MUST key on the UUID, never the name.

14. **All server-pushed assets are hostile input.** Clients decode PNG/OGG/glTF/scripts from
    servers they don't trust. From the first content-push code onward: pure-Rust decoders
    only (no C codec bindings in the client asset path), hard pre-decode caps (file size,
    dimensions, node/vertex/animation counts, sample length) checked before allocation,
    decoding on a worker with panic isolation (a poisoned asset disables that asset with a
    user-visible per-server warning, never crashes the client), and a `cargo fuzz` target
    added IN THE SAME TASK any parser/decoder path lands — not deferred to hardening.

15. **Testing:** every task ships unit tests; simulation invariants use `proptest`
    (conservation, round-trip identity); cross-platform determinism tests hash generated chunks;
    integration tests drive real bots against a real loopback server. Benchmarks use `criterion`.

16. **CI:** GitHub Actions matrix (ubuntu, windows, macos). `cargo fmt --check`, `clippy -D warnings`
    (including the determinism `disallowed-methods` lint), tests, determinism hash comparison,
    fuzz smoke targets. Keep it green.

17. **Licensing (with teeth):** Engine GPLv3-only. `api/` (Lua stubs, docs, mod template) MIT.
    The mod exception is not a README promise: it ships as a formal *Additional Permission
    under GPLv3 §7* in `LICENSE.EXCEPTION`, granted by **Iridesium**, stating that
    works interacting with the engine solely via the Lua scripting API or network protocol
    are not derivative works and carry no copyleft obligation. Enforcement hygiene: every
    source file carries an SPDX header (`GPL-3.0-only` or `MIT` in api/) checked by CI;
    `cargo deny` gates dependency licenses for GPLv3 compatibility in CI from Task 01;
    DCO sign-off required on every commit (provenance = standing to enforce). **The
    copyright holder is `Iridesium`** — that exact string in every LICENSE file, SPDX
    header copyright line, and the §7 exception grant. Only Iridesium can grant or amend the
    exception, so contributions are taken under DCO with copyright retained by their authors
    and licensed under GPLv3; if a future relicense or exception change is ever wanted, it
    needs either contributor sign-off or a CLA. Decide that before accepting outside PRs, not
    after.

18. **Performance targets are set, and speed is the stated priority.**
    [`docs/performance-targets.md`](docs/performance-targets.md) is authoritative. The
    headlines: **minimum spec is a ~6-core i5, 16 GiB RAM, and INTEGRATED GRAPHICS**;
    **50 players per server**; 20 Hz tick, so **a 50 ms budget shared by all simulation for
    all players**. Report benchmarks as a share of that budget, never in isolation — "0.4 ms"
    says nothing, "0.4 ms, 0.8% of a tick" says something.
    Integrated graphics is the binding constraint on the client, and it binds on **fill rate
    and memory bandwidth, not VRAM** — client work is measured on a real integrated GPU or it
    is not measured. Speed never buys its way out of charter rule 4: an optimisation that
    leaves the Deterministic Float Subset is not available to this project.

19. **Sub-node verdict: KEEP** (Iridesium, 2026-07-30). Full sub-node resolution for collision
    and meshing, no cap, no degradation path — see
    [`docs/subnode-verdict.md`](docs/subnode-verdict.md). Two conditions ride with it: Task 10
    **must** cache a per-block permeability byte (measured requirement, not a preference), and
    the geometry-inflation ratio gate is retired in favour of the absolute VRAM bound.

## Style
- Rust **edition 2024**, stable toolchain (pinned in `rust-toolchain.toml`).
  `thiserror` for errors. No `unwrap()` outside tests.
- Public items documented. Prefer small modules over god-files.
- Commit after each working increment with a conventional-commit message.

## Current task protocol
Each numbered prompt file is one work session. Read it fully, restate the acceptance
criteria as a checklist, implement, run tests, and finish by listing which criteria pass.
Do not implement features from future prompt files early unless a criterion requires a stub.

Acceptance criteria are tagged:
- **[A] agent-verifiable** — you (Claude Code) must demonstrate these pass via tests, CI
  output, or command output before ending the session. Untagged criteria are [A].
- **[H] human gate** — perceptual/experiential checks (feel, looks, real-hardware perf,
  fresh-machine installs). You CANNOT satisfy these. Do the enabling work, then list every
  [H] item at session end as "awaiting human verification" with exact reproduction steps.
  Never claim an [H] criterion as passed; never substitute a proxy test and call it done.
