<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: MIT -->

# `api/` — mod-facing stubs and documentation

**MIT licensed** ([`LICENSE`](LICENSE)), unlike the rest of the repository,
which is GPL-3.0-only. Everything under this directory carries the SPDX header
`MIT`, and CI enforces that.

This directory is empty for now. It will hold the things a mod author needs to
copy into their own project:

- Lua API stubs and type definitions, for editor completion and type checking
- Mod API reference documentation
- The mod template (Task 16)

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
