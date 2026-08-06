<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# `bot` — scripted headless client

A real protocol client driven by a Lua script. It is the engine's keystone
testing artifact: integration tests, load tests, and benchmarks all run through
it, and it is the same tool a server admin can point at their own server.

Nothing here is a mock. A bot completes the real join flow, over real QUIC,
against a real server — which is why a bug in framing, identity, or streaming
fails a bot run rather than surviving to the first human connection.

## Modes

### `bot run <script.lua> --server <addr>`

Runs one scripted session. **The exit code is the assertions**: 0 if every
`bot.assert` held, 1 otherwise.

```console
$ bot run crates/bot/scripts/mine_3x3.lua --server 127.0.0.1:47811
connected to 127.0.0.1:47811, certificate fingerprint c3c71ae6...
PASS crates/bot/scripts/mine_3x3.lua (3 assertion(s))
```

A failure names the script, the line, and the message you wrote:

```console
$ bot run broken.lua --server 127.0.0.1:47811
FAIL broken.lua
  runtime error: broken.lua:4: expected 9 whole blocks, got 8
```

`--name <name>` sets the display name to join under. A script that calls
`bot.join` itself is left alone, so a scenario can test the join flow when it
wants to.

### `bot swarm <N> --server <addr> --duration <seconds>`

Runs N bots concurrently under a behaviour, then prints a latency report.

```console
$ bot swarm 20 --server 127.0.0.1:47811 --duration 60
swarm: 20 bots, 60s, behaviour `wander`, against 127.0.0.1:47811
  ran for 60.1s
  bots healthy: 20/20
  edits sent: 4820, confirmed: 2410
  edit round-trip, client-observed:
    mean 1204 us
    p50  1150 us
    p95  2310 us
    p99  4102 us
    max  9855 us
OK
```

Exits non-zero if any bot failed to finish, so it works as a CI gate rather
than only as a thing to read.

- `--behavior wander` (the only one so far) — move somewhere, place a block,
  dig it back out. Self-cleaning on purpose: a load test that only placed would
  grow the world without bound and end up measuring the disk.
- `--material <id>` — what to build with.
- `--seed <n>` — the movement sequence, so a failing run can be repeated.

Percentiles are **nearest-rank**, so a quoted p99 is a latency that actually
occurred rather than an interpolation between two that did.

### `bot replay <session.log> --server <addr>`

Replays a recorded session. Recordings are line-oriented text — one
`<tick> <verb> [args]` per line — because they diff, they grep, and a failing
benchmark can be bisected by deleting lines.

```
# a tiny recorded session
0 place 70 6 70 2
2 place 71 6 70 2
4 dig_block 70 6 70
6 dig_block 71 6 70
```

Verbs: `place x y z material`, `place_subnode x y z material`,
`dig_block x y z`, `dig_subnode x y z`, `move_to x y z`, `chat text`. Blank
lines and `#` comments are ignored.

Replay honours the **gaps** between ticks, not the absolute tick numbers. A
slower machine would otherwise fall behind and then rush to catch up, which
measures the replayer rather than the server.

## The `bot.*` script API

| Call | Effect |
|---|---|
| `bot.join(name)` | Complete the join flow |
| `bot.dig_block(x, y, z)` | Replace a whole block with air |
| `bot.dig_subnode(x, y, z)` | Replace one of 27 sub-nodes with air |
| `bot.place(x, y, z, material)` | Place a whole block of a material |
| `bot.place_subnode(x, y, z, material)` | Place one unit into the cell named |
| `bot.expect_block(x, y, z, material, timeout_ms)` | Block until the server confirms |
| `bot.move_to(x, y, z)` | Report movement intent |
| `bot.chat(text)` | Send a chat line |
| `bot.inventory()` | `{[material] = units}` — see below |
| `bot.expect_units(material, units, timeout_ms)` | Block until the inventory holds at least that many units |
| `bot.sleep_ticks(n)` | Wait roughly n server ticks |
| `bot.assert(cond, message)` | Assert, and count it |
| `bot.disconnect()` | Close cleanly |
| `bot.UNITS_PER_BLOCK` | 27 |
| `bot.AIR` | The air material id |

Calls **block until the server has confirmed**, so scripts read top to bottom
and a failing assertion points at a line rather than at a continuation.

### Inventories are in units

Charter rule 5: 1 block = 27 units. `bot.inventory()` returns units, so a
script does its own arithmetic:

```lua
local units = bot.inventory()[STONE] or 0
local blocks = units // bot.UNITS_PER_BLOCK
local spares = units % bot.UNITS_PER_BLOCK
```

`bot.UNITS_PER_BLOCK` is exposed so no script hard-codes 27.

### The sandbox

Scripts run with `io`, `os`, `package`, `dofile`, `loadfile`, `load`, `debug`
and `require` removed — the same list server mods lose.

Be precise about why: a bot script is a test file *you* wrote and chose to run,
so this is **not** a security boundary the way it is for a server mod. It is
defence in depth, and more usefully a guarantee that a scenario is portable —
a script reaching for the filesystem works on the machine that wrote it and
nowhere else.

`string`, `table`, and `math` are all present; a sandbox that removed those
would make scenarios unwritable, which is a different way of being useless.

### Movement is a placeholder shape, not a placeholder API

`bot.move_to` reports intent. There is no server-side physics until Task 09, so
nothing yet moves a player. The signature is the one real movement will use, so
scenarios written today keep working when the backend changes — which is the
whole point of specifying it now rather than later.

## Starter scripts

In `crates/bot/scripts/`:

| Script | What it proves |
|---|---|
| `smoke_join.lua` | Connect, join, chat, leave |
| `mine_3x3.lua` | Nine mined blocks are exactly 243 units — 9 blocks, 0 spares |
| `subnode_mining.lua` | Five chiselled cells are 5 units — 0 blocks, 5 spares |
| `churn.lua` | Edit/undo loops leave the world as they found it |

`mine_3x3.lua` and `subnode_mining.lua` are the canonical end-to-end proof of
the 27-unit design. Every other test of that arithmetic is a unit test against
a pure function; these go through a real client, a real protocol, and a real
world.

## Certificate trust

`bot` connects **trust-on-first-use in its weakest form**: the first
certificate is accepted and nothing is remembered. It prints the fingerprint it
saw so an operator can compare it against what the server logged.

That is fine for a tool pointed at a server you chose and wrong for anything
that needs to notice an interception — which is why the integration tests use
the library's pinning constructor with an expected fingerprint instead.
