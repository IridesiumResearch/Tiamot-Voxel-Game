# Revision log

## Revision 3 — 2026-07-29

Addresses eight open findings from the review of revision 2. Each entry states what changed
and the reasoning, so a reviewer can check the decision rather than just the diff.

### 1. Charter rules renumbered 1–17
Revision 2 numbered them 1–11, 15, 16, 17, 12, 13, 14 after new rules were appended. Tasks
cite rules by number and the charter loads every session, so the numbering is an interface.
Now sequential. Cross-references updated: rule 14 → 17 (licensing, in 01), rule 16 → 13
(identity, in 06/12), rule 17 → 14 (hostile input, in 06/08/12/13/14).

### 2. Determinism — decided, not hedged
Revision 2 still said "if any step can't be guaranteed, use fixed-point." That ambiguity was
the single most expensive unresolved question in the plan. Resolved in favour of **floats**,
with reasoning, because the constraint given was that speed is paramount:

Rust guarantees `+ - * / %`, `sqrt`, `abs`, `copysign` and comparisons match IEEE 754-2008
exactly; Rust has no fast-math mode, so the compiler may not contract `a*b + c` into an FMA
and LLVM may not reassociate float expressions. The same operation sequence is therefore
bit-identical across supported targets. Determinism comes from restricting *which* operations
are used — not from leaving the FPU.

Fixed-point would have been slower (widening 64-bit multiplies and shift normalisation per
sample, SIMD stranded) for no benefit: gradient noise needs no transcendental at all.

What this added:
- Charter rule 4 is now a full specification: allowed subset, banned list with per-item
  reasoning (libm transcendentals, `mul_add`, NaN production, unordered float reduction,
  i686/x87), supported targets, and the scope limit that presentation code is exempt.
- The **denormal hazard**, which revision 2 missed entirely: flush-to-zero is thread-local
  CPU state and audio backends and GPU drivers are known to set it process-wide. A game with
  `kira` in it is exposed. `detgen::assert_ieee_mode()` guards every simulation thread.
- Enforcement moved from intention to mechanism: a `clippy.toml` `disallowed-methods` list,
  with Task 04 required to demonstrate the lint firing on a deliberate violation.
- `docs/float-determinism.md` as the authoritative write-up, edited before any subset change.
- Task 04's stale acceptance criterion fixed — it benchmarked a 48³ fill while the design had
  already moved block-level 16³ to the primary path. Both are now benchmarked separately.

### 3. Best-practice pass, and 02b measuring the right things
- **Binary greedy meshing is now mandated** (02b, 08) rather than classic greedy meshing.
  Published implementations mesh a 64³-ish chunk in ~50–200 µs, roughly 7× faster than
  conventional Rust greedy meshers around 4.5 ms. 02b's kill thresholds were calibrated for
  the slow algorithm and would have measured the wrong thing — possibly killing a viable
  design. Recalibrated to < 1 ms (realistic scene), < 4 ms (worst case), < 2 ms remesh.
- **Discovered that 16³ chunks are load-bearing**: 48 sub-node cells + 2 padding bits = 50
  bits, one `u64` column. A 32³ block chunk would need 96 bits and lose the technique
  entirely. Previously an unexamined default; now documented as a constraint in charter
  rule 6 and the Sub-Node Contract.
- **02b gained the measurements it was missing**: a lighting probe (the 3×3 mask test sits in
  the light BFS inner loop, so sub-node cost leaks into Task 10 regardless), and a
  storage/bandwidth probe (compressed chunk size for chiselled scenes; delta bytes per minute
  of chiselling). Those two numbers constrain Tasks 03 and 06, which run in parallel with the
  spike. VRAM must now be measured, not projected.
- **Scripting VM is now a measured decision** (05). The engine's purpose is hosting heavy
  mods, so script throughput is a product property, and the choice is irreversible in
  practice because mod-visible semantics differ. Lua 5.4 / LuaJIT / Luau are all reachable
  through `mlua`; Luau in particular was built for untrusted UGC at scale and its interpreter
  is roughly LuaJIT-interpreter fast. Task 05 now benchmarks all three behind a `ScriptVm`
  trait and records a verdict, defaulting to Lua 5.4 if the spread is under ~2×.
- Decoders pinned by name: `image-png` for PNG (no unsafe, OSS-Fuzz'd, Chromium's PNG decoder
  since M139); **Symphonia** for ogg, with lewton explicitly ruled out as unmaintained.
- Rust **edition 2024** (was 2021), with supported targets stated.
- `postcard` enum variants are position-encoded — an append-only rule is now written into the
  protocol module docs and the version-bump checklist.
- Task 12's ECS choice gains `hecs` as a candidate and is told to weigh deterministic
  iteration order over raw query speed.
- SQLite gains `busy_timeout` and a stated WAL checkpoint policy (Task 16's `save-freeze`
  depends on it).

### 4. 02b's fallback clause reworked
It named prompts "08/09/11", but 11 had already been rewritten to block-resolution, so the
clause was stale on arrival — exactly the drift the spike exists to prevent. Now 08 and 09
only. Two further changes: the fallback must **rewrite** the Sub-Node Contract rather than
delete it, so later tasks still have one place to cite; and the verdict is now **three-way**,
adding KEEP-WITH-LIMITS (cap Mixed slots per chunk, degrade past the cap) — because the
likely failure mode is the pathological Mixed scene alone, and a binary keep/kill would
discard a working design over it.

### 5. Task 15 untangled into 15a / 15b / 15c
It bundled a persistence and replication generalisation, a rendering system, and a
space-travel demo into one session, so the engine mechanism could not be reviewed or shipped
without also building a solar system. Now:
- **15a domains** — engine only, no content.
- **15b chunk LOD** — engine only. Seam strategy settled on **skirts**, with a note that
  transvoxel and dual contouring are isosurface techniques that do not apply to cubic voxels
  (they dominate search results for "LOD cracks" and would send an implementer down a dead
  end). "Pick one and document why" removed.
- **15c core_space** — content, and marked optional. Its real job is to be the hardest test
  of 15a's API, so it now ships an explicit API-completeness finding.

Also: **Task 03 now reserves the `domain` column up front.** Deferring it meant migrating a
live chunk table plus every query touching it, purely to exercise a migration chain that
Task 03's synthetic v0→v1 test already proves. Schema readiness is free; the risk was not.

### 6. Scope discipline written into the charter
Per direction: the engine stays deliberately simple, and a default game is a later phase.
The charter now opens with a scope-discipline paragraph stating that `game/` holds reference
implementations and test fixtures, not a shipped game. Task 16 lost its "default game polish"
section (spawn experience, tuned day length, milk pool near spawn) — premature content design
— and gained a reference-mod tidy plus `game/README.md` making the distinction unmissable.

One piece of modder tooling moved earlier: `server --check-mods` is now in **Task 07**, not
16. Task 05 already builds the resolver, sandbox and registration window, so it is a thin
front-end; it validates `game/` in CI from that point on; and a mod API that can't be checked
without booting a world has a usability bug worth finding early. The template, book, and
profiler stay in 16.

### 7. Identity is now recoverable
Revision 2 said "loss = new identity, document this." On a platform where mods key state by
UUID, that means an unrecoverable character after a reinstall, with no admin remedy. Replaced
with the current best-practice stack (charter rule 13, implemented in Task 06):
- **BIP-39 24-word recovery phrase.** An Ed25519 secret key *is* 32 bytes of entropy, so the
  seed is the key — no derivation path. Checksummed, so a mistyped word fails rather than
  silently producing a stranger's identity.
- **Key sets, not single keys** (the passkey/WebAuthn model): many authorised keys, one
  identity; adding a device requires a signature from an existing key. UUID = BLAKE3(root
  pubkey) and never changes.
- **Pre-committed rotation** (KERI pre-rotation): each key commits to the hash of its
  successor, so a stolen current key cannot rotate the identity away from its owner. Roughly
  one extra hash on the join record.
- **RCON `rebind`** as the audit-logged manual escape hatch, plus allowlist mode.
- **An honest sybil note** in the charter, the code, and the docs: keypairs are free, so UUID
  bans are trivially evaded. Complements are IP/subnet bans and allowlist mode; an
  `AuthProvider` trait leaves room for a community identity service without a protocol break.
- Task 03 gained `player_keys` and `player_names`; Task 06 gained the full test suite
  (phrase round-trip, key-set addition, rotation-commitment rejection, rebind); Task 12
  proves mod state survives key rotation via the mimic imprint.

### 8. Housekeeping
- Moved the prompt set out of `.devcontainer/` (container config, pulled into build contexts)
  to `voxel-prompts/` at the repo root. `.devcontainer/` held nothing else and is gone.
- Deleted three byte-identical duplicate files (`00-CLAUDE.md`, `02b`, `11`) that sat at
  `.devcontainer/` root alongside the real copies — the two most-edited files among them.
- Tagged the remaining untagged acceptance criteria in 01–06 as [A].
- Added this changelog and a sizing note to the README: the file count is a dependency
  ordering, not a schedule.

## Open items not addressed
- Copyright holder is set to **Iridesium** (charter rule 17, Task 01). Still undecided:
  whether outside contributions are taken under DCO alone (authors keep copyright; a future
  relicense or exception amendment then needs their sign-off) or under a CLA assigning
  rights to Iridesium. DCO-only is the current written default. Decide before the first
  outside PR — it is very hard to change afterwards.
- No CI wall-clock budget is set anywhere. Three OS legs × fuzz smoke × screenshot tests ×
  benches will get slow; at some point the matrix needs splitting into
  per-PR and nightly tiers. Worth deciding around Task 07, when the shape is visible.
