# TASK 05 — Lua runtime, registries with freeze, mod loader & dependency resolver

Depends on: 02–04. Code in `crates/core` (modules `script`, `registry`, `modload`).
This is the single most important task in the project. Take it slowly.

## Objective
Mods are Lua. The engine loads a mod set, resolves dependencies, runs registration inside a
sandbox, freezes registries, and can run a mod-provided worldgen callback. Ship the first two
reference mods in `game/`.

## The VM decision — measure, then freeze (do this FIRST)
The engine's whole purpose is hosting heavy third-party mods, so script throughput is a
first-order product property, and the choice is **irreversible in practice** because
mod-visible language semantics differ between the candidates. Decide here, with numbers.

`mlua` binds all three candidates behind one Rust API:
- **Lua 5.4** — the reference implementation. Integers, goto, current semantics. Familiar to
  Luanti/Minetest modders. Slowest of the three.
- **LuaJIT** — much faster on numeric code when traces compile, but it is Lua 5.1 semantics
  (no integer type, different `require`/FFI norms), effectively frozen upstream, and needs
  interpreter-only mode on some Apple-silicon configurations. Its FFI must be disabled
  outright for untrusted mods — it is an arbitrary-memory-access primitive.
- **Luau** — Roblox's Lua 5.1 derivative, built specifically for running untrusted
  user-generated code at scale. Interpreter performance is roughly on par with LuaJIT's
  interpreter; with partial native compilation it lands within ~1.6× of LuaJIT's JIT.
  It ships sandboxing as a first-class feature (`mlua`'s built-in `sandbox` mode is
  Luau-only) and has optional type annotations.

Deliverables for the decision:
1. `ScriptVm` trait wrapping state creation, environment construction, function registration,
   call-with-budget, and memory limits. All `script` module code goes through it. Cargo
   features `vm-lua54` / `vm-luajit` / `vm-luau` select the backend; one is default.
2. A benchmark run on all three, reported in the PR: (a) the worldgen callback from this
   task, orchestrating Task 04's native fills; (b) a synthetic per-entity `on_step` loop at
   1000 calls/tick; (c) sandbox setup cost per mod; (d) call-boundary overhead for a trivial
   function. Use Task 04's recorded fill numbers as the denominator.
3. A written verdict naming the default backend and the reasoning, in `docs/scripting-vm.md`.
   State plainly which Lua dialect mods are written against — that string goes in the modding
   docs and cannot change later without breaking every mod.

Default to **Lua 5.4** if the benchmark spread is under ~2× on the realistic workloads;
familiarity and current semantics win when speed is close. Choose otherwise only on measured
evidence. Whatever wins, the trait stays — a future WASM tier plugs in the same way.

## Design
### Mod format
- A mod is a directory: `mod.toml` + `init.lua` (+ any `require`-able files inside the dir).
- `mod.toml`: `id` (lowercase snake, namespace rules), `name`, `version` (semver),
  `depends = ["other_mod >=1.0, <2"]`, `optional_depends`, `provides = ["alias"]`,
  `description`, `license`.
### Resolver (module `modload`)
- Scan a list of mod directories → parse manifests → build DAG.
- Rules: exactly one active version per id; `provides` aliases satisfy dependencies;
  semver range matching (`semver` crate); cycle detection with the full cycle in the error;
  unsatisfied deps error naming the requirer, the requirement, and what was found.
- Load order: topological, alphabetical tiebreak. Deterministic across machines. Emit the
  resolved set (id, version, dir hash) — this becomes the server's mod manifest later.
### Script runtime (module `script`, via the `ScriptVm` trait above)
- One VM state for the server-mod tier. Sandbox: fresh environment per mod with a shared
  read-only `game` API table; removed: `os`, `io`, `dofile`, `loadfile`, `package`, and
  (LuaJIT only) `ffi`; `require` replaced with a loader restricted to the mod's own
  directory; `load` restricted to text chunks. Instruction-count hook: budget per callback
  invocation; exceeding it errors that callback. Memory limit via the VM's allocator hook.
- Crash isolation: any script error is caught at the boundary, logged with a mod-attributed
  traceback, and marks that mod faulted (its future callbacks are skipped); the tick loop
  never unwinds.
- Registration API available ONLY during the registration window:
  - `game.register_block{ id="white", name="White", drops=..., hardness=..., ... }`
    (namespace auto-prefixed with mod id; explicit `core:` style ids allowed only for the
    `core` mod). Unknown fields error with the field name.
  - `game.register_on_generate(fn(chunkbuf, chunk_pos))` — worldgen callback.
  - `game.register_action{ id, default_key }` — stored for Task 13, inert for now.
- Frozen-phase API: `game.get_block_id(string_id)`, `game.noise_*` (bulk fills into a
  ChunkBuffer, wrapping Task 04), `game.rng_stream(chunk_pos, name)`, logging `game.log(...)`.
- ChunkBuffer exposed as userdata wrapping Task 04's object; all bulk ops native.
  Block-level fills are the documented default; sub-node ops are opt-in (Sub-Node Contract).
- **Script code must not perform simulation-relevant float math directly** — charter rule 4's
  subset cannot be enforced inside a script VM. Worldgen scripts orchestrate native fills;
  they do not compute per-sample values in Lua. Document this as an API rule and make the
  fast path the only ergonomic path.
### Lifecycle (engine-driven)
scan → resolve → for each mod in order: run init.lua in its env → close registration window →
freeze (registries become immutable; id table reconciled with the world DB via Task 03) →
worldgen callbacks may now be invoked per chunk on demand.
- Post-freeze `register_*` raises a script error "registration is closed".
### First reference mods (in `game/`)
- `game/core_blocks/`: registers `core:white`; verify `engine:unknown` exists engine-side and
  is NOT registerable by mods.
- `game/core_worldgen/`: the default generator — blocks with y < 0 filled `core:white`,
  y >= 0 air, via `fill_below_heightmap` with a constant heightmap. ~15 lines of Lua.
### API stubs
- `api/stubs/game.lua`: Lua Language Server annotations (`---@param` etc.) for every exposed
  function. CI grep check that every `game.` function registered in Rust appears in the stub.

## Tests
- [A] Resolver: table-driven cases — simple chain, diamond, cycle (error text asserted),
  version conflict, provides-alias satisfaction, deterministic order across shuffled scan
  input.
- [A] Sandbox: `os`/`io`/`ffi` absent; `require` escapes rejected; infinite loop in a callback
  errors via instruction budget without hanging the test; OOM script contained.
- [A] Lifecycle: register after freeze errors; faulted mod's callbacks skipped while others
  run.
- [A] End-to-end: load `game/`, freeze, generate chunk (0,-1,0) and (0,0,0) via the Lua
  callback, assert white/air respectively; fingerprint the generated chunk and add it to the
  golden determinism gate (script-driven worldgen must also be cross-platform identical).
- [A] The three-VM benchmark above, with results tabulated in the PR.
- [A] Bench: full generate-chunk via the script callback on the chosen backend — orchestration
  overhead < 20% over calling the native fills directly. Record numbers.

## Acceptance criteria
- [A] `game/` mods load and generate the half-white world deterministically on all CI
      platforms.
- [A] All sandbox and lifecycle tests pass.
- [A] `docs/scripting-vm.md` records the benchmarked verdict; the `ScriptVm` trait is the
      only path into the VM (grep: no direct backend types outside `script`).
- [A] Stub-coverage CI check passes.
- [A] A deliberately-crashing test mod is disabled at runtime while the world keeps working.
