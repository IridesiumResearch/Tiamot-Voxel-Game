<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Licensing for mod authors

**Short version: your mod is yours. License it however you like, including
commercially and including closed-source.**

This page explains the licensing in plain language. It is a summary and has no
legal effect — [`LICENSE`](LICENSE) and [`LICENSE.EXCEPTION`](LICENSE.EXCEPTION)
are the terms that actually govern.

## Why this needs saying at all

The engine is licensed **GPLv3-only**. Left alone, the GPL is deliberately
sticky: a plausible reading is that anything running inside the engine's process
is a derivative work and must itself be GPL. For a game engine whose entire
purpose is hosting third-party mods, that reading would make the project useless
— nobody can build a commercial mod on a platform where the licence might eat it.

So we don't leave it alone, and we don't settle it with a friendly note in the
README. A README promise is not a licence and cannot be relied on. Instead the
permission ships as a formal **Additional Permission under GPLv3 section 7** in
[`LICENSE.EXCEPTION`](LICENSE.EXCEPTION), granted by Iridesium, the copyright
holder. That is a real grant with legal effect, and it travels with every copy
of the engine.

## What you can do

If your work talks to the engine **only** through the Lua scripting API or the
network protocol, then it is an independent work:

- License it under anything you want — MIT, proprietary, all rights reserved.
- Sell it. Keep the source closed. Ship it on any storefront.
- Distribute it alongside the engine.
- Everything it generates — worlds, assets, save data — is yours too.

This holds even though your Lua runs inside the engine's process. The exception
says so explicitly, precisely because that is the case people worry about.

## What is still covered by the GPL

The exception is about *the boundary*, not about the engine. You are back under
the GPL, in full, if you:

- **Modify the engine.** Patches to anything in `crates/` are GPLv3, and must be
  released as such if you distribute them. This is true even when the change
  exists only to make your mod work.
- **Link against engine code directly** — as a Rust crate, a native shared
  library, or anything else that is not the scripting API or the network
  protocol.
- **Reach past the public surface** into private or unpublished interfaces.

The dividing line is the API, not the file boundary. Go through the front door
and you are independent; go around it and you are not.

## Hit a wall in the API?

Charter rule 1 is that the mod API is the only API — if a mod can't do something
through it, that is an engine bug, not an invitation to fork. **Open an issue.**
Extending the API keeps you on the clean side of the line and everyone else
benefits. Patching the engine to get around it puts your work under the GPL and
leaves you maintaining a fork.

## The `api/` directory is MIT

Everything under [`api/`](api/) — the Lua stubs, type definitions, mod template,
and API documentation — is **MIT** licensed, not GPL. You can copy those files
into your own project freely, including a closed-source one. See
[`api/LICENSE`](api/LICENSE).

## Contributing back

Contributions to the engine are taken under the Developer Certificate of Origin;
you keep your copyright and license your contribution under GPLv3. See
[`CONTRIBUTING.md`](CONTRIBUTING.md). Note that only Iridesium can grant or amend
the section 7 exception.
