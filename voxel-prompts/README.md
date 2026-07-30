# Voxel Engine — Claude Code prompt sequence

A build plan for a multiplayer voxel **engine**: a platform for mods, not a game. See
`CHANGELOG.md` for what changed in the current revision and why.

## How to use
1. `git init` your repo. Copy `00-CLAUDE.md` into the repo root **as `CLAUDE.md`** — Claude
   Code auto-loads it every session, so the charter governs every task without repeating it.
2. Feed the numbered files to Claude Code **one per session, in order**. Paste the file
   contents as the prompt (or say "read and execute 03-persistence.md" if you keep them in a
   `prompts/` dir in the repo — committing them is recommended: they double as design docs).
3. At the end of each session, require the acceptance-criteria checklist verdict before
   moving on. Don't start N+1 with criteria failing in N — the sequence assumes it.
4. Acceptance criteria are tagged [A] (agent-verifiable — Claude Code must prove them) or
   [H] (human gates: feel, looks, real-hardware perf, fresh-machine installs — Claude Code
   preps and lists them; only you can pass them). Untagged = [A]. Budget your own time for
   the [H] items; they cluster in 02b, 08, 09, 10, 12, 13, 15b, 15c, 16.
5. Between sessions: `cargo test --workspace` yourself, skim the diff, play the build when
   there's something playable (08 onward). Course-correct in the file for the next task
   rather than mid-session when possible.

## Order and dependency shape
```
01 scaffold
02 voxel data
02b SUB-NODE SPIKE ......... hard gate: keep / keep-with-limits / fallback
03 persistence ............. reserves the domain column for 15a
04 determinism ............. the Deterministic Float Subset + its lint
05 Lua/mods ................ VM chosen by benchmark, then frozen
06 networking .............. identity, recovery, protocol fuzzing
07 bot harness ............. + `server --check-mods`
08 client render ........... binary greedy meshing
09 player/interaction
10 lighting
11 milk .................... block-resolution, MC-style
12 entities/mimic
13 audio/input
14 UI
15a domains ................ engine mechanism, no content
15b chunk LOD .............. engine mechanism, no content
15c core_space ............. OPTIONAL demo mod; proves 15a's API
16 tooling/docs/hardening/release
```

**02b is a hard gate.** Tasks 03–07 may proceed during or after it (they're sub-node-
agnostic), but do NOT start 08 until you've recorded the keep/limits/fallback decision.

Strict order for 01–09. After that 10–14 have some flexibility (13 and 14 can swap; 11 and 12
can swap) but the listed order keeps each task's tests runnable as written.

**15a/15b/15c are separable.** 15a and 15b are engine and belong before release. 15c is a
demonstration mod — valuable as the hardest test of 15a's API, but it can slip to the
default-mods phase without leaving the engine unfinished.

## Sizing expectation
Tasks 01–04 are single sessions. 05, 06, 08, 09, 11, 15a are large — expect to split a
session or run a follow-up "finish the remaining criteria" session; that's normal, keep the
criteria as the contract. If a task balloons, cut scope *within* the task's design section,
never the tests or determinism gates.

The 18-file count is a dependency ordering, not a time estimate. Several of these are weeks,
not evenings. Being at task 09 after far more sessions than there are numbers is the expected
shape of the work, not a sign of falling behind.

## Standing rules for every session (also in CLAUDE.md)
- The engine is mechanisms. `game/` holds **reference mods and test fixtures**, not the
  default game — that's a later phase. Don't design content here.
- Mod API is the only API; headless server is the game; the client is a viewer.
- 27 units per block, everywhere, no special cases.
- Floats in simulation stay inside the Deterministic Float Subset (charter rule 4). The
  clippy lint is the enforcement; never silence it to make a hash match.
- Determinism gates are sacred: never "fix" a cross-platform hash mismatch by loosening the
  test.
- Every parser or decoder that touches server-supplied bytes ships its fuzz target in the
  same task.
- Every task lands with its tests; CI stays green.
