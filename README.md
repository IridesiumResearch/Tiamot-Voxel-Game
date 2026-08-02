<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Tiamot

An experimental multiplayer voxel **engine** built around subdivided voxels, for
a more detailed voxel world than block-resolution allows.

Tiamot is an engine, not a game. Its entire purpose is to be a platform for
mods: the engine ships mechanisms, and all content is Lua. The mod API is the
only API — if something cannot be built through it, that is an engine bug to
fix rather than a reason to special-case the core. The headless server is the
game and the client is a viewer onto it; singleplayer is an embedded server over
loopback, so there is exactly one simulation code path and no singleplayer-only
behaviour that can drift out of sync with multiplayer.

Two properties shape most of the design. The first is **sub-block resolution**:
one block is one cubic yard, subdivided 3×3×3 into 27 units, everywhere, with no
special cases for partial blocks — quantities are stored in units and displayed
as blocks plus nodes. The second is **determinism**: the same seed produces
bit-identical worlds on Linux, Windows, and macOS, enforced by a cross-platform
hash gate in CI. That is achieved not by leaving the FPU for fixed-point
arithmetic, but by restricting simulation to a subset of `f32`/`f64` whose
results Rust guarantees to match IEEE 754-2008 exactly. Transcendentals,
`mul_add`, and unordered float reductions are banned from simulation paths, and
a clippy lint enforces the ban. Rendering, audio, and UI are exempt —
determinism is a simulation property, and taxing presentation code with it buys
nothing.

The engine is deliberately small, and it is being built as such. The mods in
`game/` are reference implementations and test fixtures that prove each
mechanism works through the public API; they are **not** a shipped game. A
default game is a later phase, built as mods on top of a finished engine. If a
feature looks like it belongs to the game rather than the engine, the engine's
job is only to make it expressible.

## Crate map

| Crate | Role | May depend on |
|---|---|---|
| [`crates/core`](crates/core) (`tiamot-core`) | Voxel data, simulation, Lua runtime, physics, persistence, protocol types. The whole simulation lives here. | **Never** wgpu, winit, kira, or egui |
| [`crates/server`](crates/server) | Thin headless binary over core. Runs with no display server. | **Never** wgpu, winit, kira, or egui |
| [`crates/client`](crates/client) | Viewer: rendering, windowing, audio, UI. | core + wgpu/winit/kira/egui |
| [`crates/bot`](crates/bot) | Scripted headless client for tests, benchmarks, and load. | core |

That table is not documentation of intent — it is enforced.
[`scripts/check-dep-firewall.sh`](scripts/check-dep-firewall.sh) fails CI if
`core` or `server` picks up a render, window, audio, or UI dependency on any
target platform.

Two more directories sit outside the workspace: [`game/`](game/) holds reference
mods and test fixtures, and [`api/`](api/) holds the MIT-licensed mod-facing
stubs and documentation.

## Supported targets

**x86_64** (SSE2 baseline) and **aarch64**, on Linux, Windows, and macOS.

**i686 is not supported and is not built in CI.** This is a determinism
requirement, not an oversight: 32-bit x86 computes through x87 registers at
80-bit excess precision, so intermediate results round differently than on any
SSE2 or NEON target and the cross-platform hash gate cannot hold. There is no
plan to support it.

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo run -p server -- --config server.example.toml
```

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml); rustup
picks it up automatically.

## Playing

```sh
cargo run -p client
```

With no config file at all that starts a server **in this process**, connects to
it over loopback, and opens a window. That is what singleplayer is here — there
is one simulation code path and it is the server's, so a bug in singleplayer is
a bug everyone else has too. Copy
[`client.example.toml`](client.example.toml) to `client.toml` to point it at
someone else's server, or to change the view distance, field of view, or
controls-adjacent settings; the file documents the controls as well.

The client needs Vulkan, Metal, DX12, or GL. The **server needs no GPU and no
display server at all**, which is checked in CI on every push.

## Licensing

The engine is **GPLv3-only**. The `api/` directory is **MIT**.

**Mods are not covered by the GPL.** Works that interact with the engine solely
through the Lua scripting API or the network protocol are independent works with
no copyleft obligation — you can license them however you like, including
commercially and closed-source. This is not a README promise: it is a formal
Additional Permission under GPLv3 §7, granted by Iridesium in
[`LICENSE.EXCEPTION`](LICENSE.EXCEPTION). See
[`MOD-LICENSING.md`](MOD-LICENSING.md) for what that means in practice.

Contributions are taken under the Developer Certificate of Origin with authors
retaining copyright — see [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Project charter

[`CLAUDE.md`](CLAUDE.md) is the binding architectural charter: the dependency
firewall, the unit system, the determinism rules, the identity model, and the
hostile-input policy for server-pushed assets. Read it before proposing anything
structural. The build plan lives in [`voxel-prompts/`](voxel-prompts/).
