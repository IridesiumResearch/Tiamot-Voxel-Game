# TASK 07 — Bot harness, test infrastructure, benchmark & load framework

Depends on: 06. Code in `crates/bot` (promote from scaffolding to a real tool).

## Objective
The bot is the keystone testing artifact: scripted headless client used for integration
tests, load tests, benchmarks, and later by server admins.

## Design
- `bot` binary and library. A bot session = real protocol client + a Lua script driving it
  (reuse the core sandboxed runtime; bot-side API table `bot.*`):
  - `bot.connect(addr)`, `bot.wait_joined()`, `bot.move_to(x,y,z)` (teleport-style input for
    now; real movement arrives with physics in Task 09 — design the API so the backend swaps),
    `bot.dig_block(pos)`, `bot.dig_subnode(pos)`, `bot.place(pos, id)`,
    `bot.expect_block(pos, id, timeout)`, `bot.inventory()`, `bot.chat(msg)`,
    `bot.assert(cond, msg)`, `bot.sleep_ticks(n)`, `bot.disconnect()`.
- Runner modes:
  - `bot run script.lua --server addr` — single scripted session, exit code = assertions.
  - `bot swarm N --behavior wander --server addr --duration 60s` — N bots random-walking and
    periodically editing blocks; prints server-observed and client-observed latency stats.
  - `bot replay session.log` — deterministic replay of a recorded input session (record
    format: tick-stamped inputs; used by the macro benchmark).
- Test orchestration: `tests/integration/` at workspace root — helper that builds/starts a
  server subprocess (or embedded handle) with a temp world + specified mod set, runs bot
  scripts, tears down. Convert Task 06's integration tests to this harness.
- Ship starter scripts in `bot/scripts/`: `smoke_join.lua`, `mine_3x3.lua` (digs 3×3 blocks,
  asserts inventory = 9 blocks + 0 spares via unit arithmetic), `subnode_mining.lua` (digs
  single sub-nodes, asserts spares math), `churn.lua` (edit/undo loops for load).
- Macro benchmark harness: fixed seed world + recorded 4-bot session → replay → report tick
  time distribution: mean, p50, p95, p99, max; machine-readable JSON + human table. Store
  baseline JSON in repo; CI compares p99 against baseline with a generous threshold (fail on
  >2× regression) — tighten later.
- Wire `criterion` micro-benches (Tasks 02–06) plus this macro bench into a `just bench` /
  `cargo xtask bench` entry point and a nightly CI job that uploads the JSON as an artifact.
- `server --check-mods <dir>`: loads, resolves, and runs registration in a dry-run sandbox,
  reporting errors and warnings without touching a world. CI-friendly exit codes.
  Task 05 already built every piece of this — the resolver, the sandbox, and the registration
  window — so it is a thin binary front-end, and it belongs here rather than at the end
  because it is how every later task's `game/` mods get validated in CI, and because a mod
  API that cannot be checked without booting a world is an API with a usability bug. Task 16
  adds the modder-facing polish (warning classes, `--strict`, JSON output).

## Tests
- The harness testing itself: a failing bot assertion fails the test run; server crash is
  detected and reported (not a hang); temp worlds cleaned up.
- `mine_3x3.lua` and `subnode_mining.lua` pass against a live server — these are now the
  canonical end-to-end proof of the 27-unit design.
- Swarm 20 bots × 60s on CI: server survives, no tick > 5× budget, memory stable
  (assert RSS growth bounded after warmup).

## Acceptance criteria
- [A] `cargo test --workspace` runs unit + integration (bots against real server) green.
- [A] Swarm and replay modes work from the CLI as documented in `bot/README.md`.
- [A] `--check-mods` passes on `game/` and returns a non-zero exit code with a readable
      message on a deliberately broken mod (missing dependency, and post-freeze
      registration). It runs on `game/` in CI from this task onward.
- [A] Nightly bench CI job produces the JSON artifact and the regression gate functions
      (demonstrate by temporarily pessimizing a hot path, then revert).
