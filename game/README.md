<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# `game/` — reference mods and test fixtures

**This directory is not the game.** What lands here is reference
implementations and test fixtures, not shipped content.

| Mod | The mechanism it proves |
|---|---|
| `core_blocks` | Block registration and textures reach a client. |
| `core_worldgen` | A mod can generate terrain through the native heightmap fills. |
| `core_tools` | Digging rules live in Lua — delete it and nothing can be broken, because the engine has no bare hand (Task 09). |
| `core_sky` | Sky content lives in Lua — delete it and the world loses its day and keeps everything else (Task 10). |
| `core_gear` | Items that are not blocks, dropping and picking up, and a worn-slot view all live in Lua — the engine draws a dropped stack and decides nothing else about it. |
| `core_ui` | The HUD and the inventory screen live in Lua — delete it and a client keeps a crosshair, chat and settings, and loses everything else on the screen (Task 14). |

Each of those is checked by a test that removes the directory and asserts what
stops working, which is the only way a claim like "this lives in a mod" can be
anything more than an intention.

The distinction matters enough that the charter opens with it. Every mod in this
directory exists to prove that a specific engine mechanism works *through the
public mod API* — that is its whole job. If a mechanism can only be exercised by
reaching past the API, that is an engine bug (charter rule 1), and these mods are
how it gets caught.

A real default game is a later phase, built as mods on top of a finished engine.
It is not part of the 18-task build plan in [`../voxel-prompts/`](../voxel-prompts/).

## What that means in practice

**Do not** add content here, tune game feel, or design progression. If a task
tempts you toward "the game needs X", the answer is that a *future mod* needs X,
and the engine's job is to make X expressible.

A mod belongs here when it is the smallest thing that demonstrates a mechanism
and can be asserted against in a test. It does not belong here because it would
be fun.

## Licensing

Mods in this directory are part of the engine distribution and carry the
engine's licence, **GPL-3.0-only**.

That is not the situation for *your* mods. Anything interacting with the engine
solely through the Lua scripting API or the network protocol is an independent
work under the GPLv3 §7 Additional Permission in
[`../LICENSE.EXCEPTION`](../LICENSE.EXCEPTION) — license it however you like,
including commercially and closed-source. See
[`../MOD-LICENSING.md`](../MOD-LICENSING.md).
