# TASK 01 — Workspace scaffold, CI, licensing

Context: CLAUDE.md is in the repo root and governs everything. This is the first task in an
empty repo.

## Objective
Create the Cargo workspace, license structure, and CI so every later task lands on rails.

## Deliverables
1. Workspace with crates: `crates/core`, `crates/server`, `crates/client`, `crates/bot`.
   Each compiles with a placeholder `lib.rs`/`main.rs`. Workspace-level dependency table for
   shared versions.
2. Dependency firewall test: a CI step (small script or `cargo tree` check) that FAILS if
   `core` or `server` transitively depends on `wgpu`, `winit`, `kira`, or `egui`.
3. `game/` directory (empty except README explaining it will hold the default Lua mods) and
   `api/` directory (README: MIT-licensed mod-facing stubs/docs live here).
4. Licensing (charter rule 17 — with teeth):
   - `LICENSE` (GPLv3 full text), `api/LICENSE` (MIT).
   - `LICENSE.EXCEPTION`: an Additional Permission under GPLv3 §7 stating that works
     interacting with the engine solely through the Lua scripting API or the network
     protocol are independent works with no copyleft obligation. Reference it from LICENSE
     and README. `MOD-LICENSING.md` explains it in plain language for modders.
   - Copyright holder is **`Iridesium`** — use that exact string, no placeholder, in the
     `LICENSE` copyright line, `api/LICENSE`, the `LICENSE.EXCEPTION` grant, and every SPDX
     header's copyright line (`// SPDX-FileCopyrightText: Iridesium`). Add a line to
     `CONTRIBUTING.md` stating that contributions are accepted under DCO with authors
     retaining copyright, and that only Iridesium can amend the §7 exception.
   - SPDX header (`// SPDX-License-Identifier: GPL-3.0-only`, MIT under `api/`) in every
     source file, enforced by a CI script that fails on missing/incorrect headers.
   - `deny.toml` + `cargo deny check` in CI: dependency licenses must be GPLv3-compatible;
     advisories checked.
   - `CONTRIBUTING.md`: DCO sign-off required (add a CI DCO check on commit messages).
5. CI: GitHub Actions workflow, matrix {ubuntu-latest, windows-latest, macos-latest}:
   fmt check, clippy with `-D warnings`, `cargo test --workspace`, the firewall check,
   SPDX check, cargo-deny, DCO check. Cache cargo registry/target.
6. `rust-toolchain.toml` pinned to current stable; every crate on **edition 2024**.
   `clippy.toml` created now with an empty `disallowed-methods` list and a comment pointing at
   Task 04, which populates it with the determinism ban list (charter rule 4) — creating the
   file here means CI wiring is already in place when the rules arrive.
   `.gitignore`. Top-level `README.md` with a 3-paragraph project description and the crate
   map. State the supported targets (x86_64 + aarch64; i686 unsupported, per charter rule 4).
7. `server` main: parses `--config <path>` (TOML: `bind_addr`, `world_path`, `max_players`),
   logs startup with `tracing`, installs a SIGTERM/ctrl-c handler that logs "saving and
   shutting down" and exits 0. No X11/GUI deps — verify it builds with no display server.

## Acceptance criteria
- [A] `cargo build --workspace` and `cargo test --workspace` succeed on a clean checkout.
- [A] Firewall check fails if you temporarily add `wgpu` to core (demonstrate, then revert).
- [A] `cargo run -p server -- --config server.example.toml` starts, logs, and exits cleanly on ctrl-c.
- [A] CI workflow file is valid YAML and covers all matrix legs.
- [A] All license/policy files present with correct texts, including the §7 exception.
- [A] SPDX and cargo-deny checks pass, and each fails when violated (demonstrate both with a
      temporary violation, then revert).
