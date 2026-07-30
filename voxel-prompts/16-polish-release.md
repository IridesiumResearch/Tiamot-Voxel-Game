# TASK 16 — Modder tooling, docs, hardening, release packaging

Depends on: all previous. This task turns the engine into a PLATFORM other people can build
on.

## Scope note (charter: scope discipline)
This is explicitly **not** "polish the default game." There is no default game yet, by
design. The mods in `game/` stay what they have been throughout: reference implementations
and test fixtures that demonstrate each mechanism through the public API. The shipped default
game is the next phase, written as mods against the engine this task finishes.

So the deliverable here is: a stranger can download a release, host a server, write a mod
from the template, and find answers in the docs — against an engine whose sharp edges have
been filed down. Not: a game that is fun.

## Deliverables
### Modder tooling
1. In-game dev console (client, permission-gated): script REPL against the server mod
   environment (op-only, config-gated), log viewer filtered by mod, `/reload <mod>` hot
   reload for server mods. Hot reload fights charter rule 9 (registries freeze) and cannot
   win: document precisely what it can and cannot change — callbacks and mod-local state can
   be replaced; block/item/domain registrations cannot, because the world DB's id table and
   every loaded chunk depend on them. Reloading a mod that changes its registrations must
   fail with that explanation, not half-apply.
2. Per-mod profiler: engine tracks script time, callback counts, and instruction usage per
   mod per tick; `/profile` renders a live table in-game and dumps JSON via RCON. The bot
   swarm harness gains a per-mod cost report.
3. Mod template `api/template/`: mod.toml, annotated init.lua touring the API (a block, a
   tool, a sound, an action, a dialog, an entity), LLS config wired to `api/stubs`, README
   walkthrough. `server --create-mod <id>` scaffolds it.
4. Extend `server --check-mods` (shipped in Task 07) with modder-facing polish: warning
   classes, a `--strict` mode, and machine-readable JSON output so modders can wire it into
   their own CI.

### Documentation (in-repo `docs/`, mdBook)
5. Book: Getting Started (player), Hosting a Server (systemd unit, Docker image + compose,
   backup guidance = copy the .db under RCON `save-freeze` — implement that command),
   Modding Guide (tutorial rebuilding a mini version of core_tools), full API reference
   GENERATED from the stub annotations (CI step keeps it in sync), Architecture overview
   (domains, determinism, trust tiers, the sub-node contract), MOD-LICENSING explainer,
   Identity & Recovery (the seed phrase, adding a device, what happens if you lose
   everything, and the honest note that keys are sybil-cheap so bans are weak).
6. Promote the standing design docs written along the way into the book:
   `docs/float-determinism.md`, `docs/subnode-contract.md`, `docs/subnode-verdict.md`,
   `docs/scripting-vm.md`. These are the "why" that a new contributor needs first.

### Reference-mod tidy (not game design)
7. A consistency pass over `game/`: every reference mod is readable as example code —
   consistent style, commented to explain the API being demonstrated rather than the
   behaviour being achieved, no dead experiments left behind. Add `game/README.md` stating
   plainly that these are reference implementations and test fixtures, that they are not the
   default game, and pointing to the modding guide.
8. First-run flow: engine-native main menu — singleplayer (world list/create with seed
   entry), multiplayer (server list by address + TOFU fingerprint confirmation UI), identity
   (first-run recovery-phrase display, restore-from-phrase, add-a-device), settings.

### Release engineering
9. Release workflow via `cargo dist`: tagged builds produce linux x86_64, windows x86_64 and
   macos universal archives (client + server each) containing `game/`, `api/`, LICENSE files
   and the book. Server Docker image pushed on tag. Everything versioned from one workspace
   version; protocol-version bump checklist in CONTRIBUTING. Keep the workflow readable
   enough that it could be replaced by a plain GitHub Actions matrix if the tool ever goes
   unmaintained — do not let release engineering become a single point of failure.
10. Final hardening sweep: every fuzz target built up since Task 06 (protocol decoder,
    texture, glTF, ogg, dialog schema) gets an extended run — hours, not CI-smoke minutes —
    with triage of findings and minimised corpora committed. `cargo deny` re-verified.
    `unsafe` inventory documented. Error-message pass: every user-facing error names the mod
    or subsystem at fault. Spot-audit that the SPDX/DCO/license-exception teeth from Task 01
    survived the whole project.

### Meta
11. Update CLAUDE.md: mark which charter rules are now enforced by tests (with the test
    name), list known debt, and add a "next phase" section — the default game as mods, plus
    deferred engine work (WASM script tier, skins, richer LOD, additional domains).

## Acceptance criteria
- [H] Fresh Ubuntu VM: download the release, run the server via the provided systemd unit,
      connect from a Windows client build, play — following only the docs. (Claude Code preps
      everything; only a human can run this gate.)
- [A] `--create-mod` template passes `--check-mods --strict`, loads on a server, and its
      block appears in-game without editing engine or `game/` code.
- [A] API reference regenerates in CI and matches the live API (sync check green).
- [A] Hot reload correctly refuses a registration-changing reload with the documented
      explanation, and correctly applies a callback-only reload.
- [A] All fuzz targets run clean for the extended duration; cargo-deny green.
- [A] The full bot suite + benchmarks + determinism gates green on the release tag across all
      three platforms.
- [A] A reader of `game/README.md` cannot mistake the reference mods for a shipped game.
