<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: MIT -->

# `api/` — mod-facing stubs and documentation

**MIT licensed** ([`LICENSE`](LICENSE)), unlike the rest of the repository,
which is GPL-3.0-only. Everything under this directory carries the SPDX header
`MIT`, and CI enforces that.

What a mod author needs to copy into their own project.

## What is here now

**[`stubs/game.lua`](stubs/game.lua)** — the whole mod API as LuaLS `---@meta`
annotations: every function, every options table, every field, with the reason
each behaves the way it does. Point your editor at it and you get completion,
signatures and type checking against the real API.

**It is complete, and CI keeps it that way.** `scripts/check-stubs.sh` fails the
build if the engine registers a `game.*` function this file does not document,
so it cannot quietly fall behind the engine the way hand-written API docs do.
That makes it the one file worth reading end to end before writing a mod — and
the one worth handing to a tool that is going to write one with you.

## What is not here yet

- The mod template (Task 16).
- Prose documentation. Until it exists, the stubs carry the reference material
  in their doc comments, and [`../game/`](../game/) holds worked examples —
  every mod in it is written through this API and nothing else, which is a rule
  the build enforces rather than an intention.

## Why MIT and not GPL

Because you have to be able to copy these files into a closed-source mod without
consequence. A GPL type-stub file that a mod author vendors into their project
would drag the copyleft along with it, which would defeat the entire point of
the §7 exception. Making this directory MIT removes the question.

The exception in [`../LICENSE.EXCEPTION`](../LICENSE.EXCEPTION) already says that
using the scripting API does not make your mod a derivative work. This is the
belt to that pair of braces: even *vendoring* the stubs is unambiguously fine.

## SPDX headers

Source files here use:

```
// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: MIT
```

with the comment syntax appropriate to the file type (`--` for Lua). The
`GPL-3.0-only` identifier used everywhere else in the repository is **wrong**
here, and `scripts/check-spdx.sh` fails the build if it appears.

See [`../MOD-LICENSING.md`](../MOD-LICENSING.md) for the full picture.
