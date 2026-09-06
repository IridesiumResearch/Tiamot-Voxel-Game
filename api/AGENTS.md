<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: MIT -->

# Writing a Tiamot mod — a brief for AI assistants

**Audience: an AI coding assistant that has been asked to write a mod, and the
person supervising it.** Copy this file to the root of the mod project as
`AGENTS.md` and most assistants will load it automatically. It is MIT, like
everything in `api/`, so it can be vendored anywhere.

Read [`stubs/game.lua`](stubs/game.lua) before writing a line. It is the whole
API — every function, its options table, every field, and why each behaves as it
does — and `scripts/check-stubs.sh` fails the engine's build if a `game.*`
function exists that it does not document. **It cannot fall behind the engine.**
If something is not in there, it does not exist; do not invent it.

---

## What a mod is

A directory with two files:

```
my_mod/
  mod.toml    -- the manifest
  init.lua    -- runs once, at load
```

Dropped into the server's mods directory (`game/` in this repository). Lua 5.4:
you have an integer subtype, `//`, and integer `%`, and you will use all three.

`mod.toml` — `id`, `name` and `version` are required, the rest optional:

```toml
id = "my_mod"           # letters, digits, underscore. Your namespace.
name = "My Mod"
version = "0.1.0"       # semver
depends = ["core >=0.1"]
description = "One line."
license = "MIT"
```

Validate without launching the game — this is the fast loop, and it catches
typos, namespace errors and load-order problems in seconds:

```console
cargo run -p server -- --check-mods <mods-dir>
```

It prints the mods that loaded, in dependency order, and every block that
registered. A mod that fails to load is disabled and named; it does not take the
server down.

---

## The six things an assistant gets wrong

These are unusual. An assistant carrying habits from other voxel engines will
violate all six without noticing, and most of them fail quietly.

### 1. Never compute simulation values in Lua

**The single most important rule.** The engine guarantees the same seed produces
bit-identical worlds on Linux, Windows and macOS. That rests on restricting
which floating-point operations run, and it cannot police what happens inside a
script — `x^0.5` in your mod is a platform library call and its last bits differ
between machines.

So ask the engine for whole buffers and hand them to whole-buffer operations:

```lua
-- RIGHT: one native call fills all 256 columns, another consumes it.
local heights = game.noise_heightmap(pos, { octaves = 5, frequency = 0.008, amplitude = 110.0 })
buf:fill_below_heightmap(heights, stone)
```

```lua
-- WRONG: there is no per-sample entry point, by design.
for x = 0, 15 do for z = 0, 15 do
    local h = math.floor(noise(x, z) * 24)   -- does not exist, and could not
end end
```

If you need a trig value, the engine has `game.heading(dx, dz)`. Do not reach
for `math.atan`, `math.sin` or `^`.

### 2. Quantities are integer units, 27 to a block

A block is 3x3x3 sub-nodes. Every inventory quantity, every drop, every cost is
in **units**, stored as integers. Display is `units // 27` blocks plus
`units % 27` nodes. There are no fractional blocks and no special case for
partial ones. `game.inventory` reports each stack as
`{ material, units, blocks, nodes, count, shape, detail }` — `units` and `count`
are different numbers and confusing them is the commonest arithmetic bug here,
and `blocks`/`nodes` are already worked out for you, so do not divide again.

### 3. String IDs are canonical; numbers are per-session

`"core:white"` is the identity. The numeric ids `game.get_block_id` hands back
are **per-session and mean nothing across runs** — never persist one, never
hard-code one, never compare one against a number from somewhere else.
`game.block_of` converts back. Ids you register are namespaced with your mod id
automatically: `game.register_block{ id = "brick" }` gives `my_mod:brick`.

### 4. Registration happens once, then the world freezes

`register_block`, `register_sky`, `register_tool`, `register_domain`,
`register_action`, `register_fluid`, `register_item` and friends work **only
while `init.lua` is running**. After the load phase the registries freeze and
calling one is a hard error. Anything conditional on the world, the player or
the time of day belongs in a hook, not in registration.

Hooks (`register_on_tick`, `register_on_chat`, `register_on_place`,
`register_on_dig_complete`, `register_on_generate`, …) are registered in the
window and called for ever after.

### 5. Worldgen: describe the field, never sample it

The rule above says no per-sample maths. That does not mean no 3D terrain — it
means you describe the expression and the engine evaluates it:

```lua
-- ONCE, in init.lua. Not per chunk.
local field = game.density{
    op = "min",
    a = {                                    -- terrain: noise falling off with height
        op = "sub",
        a = { op = "noise", stream = "terrain", frequency = 0.03, octaves = 3 },
        b = { op = "mul", a = { op = "y" }, b = { op = "const", value = 0.06 } },
    },
    b = {                                    -- caves: tunnels where the noise is near zero
        op = "sub",
        a = { op = "const", value = 0.35 },
        b = { op = "abs", a = { op = "noise", stream = "caves", frequency = 0.05 } },
    },
}

game.register_on_generate(function(buf, pos)
    buf:fill_density(field, stone)           -- solid wherever the field is > 0
end)
```

That is overhangs, arches and caves in one mechanism. Give each noise node a
different `stream` name or your caves will follow your hills exactly. The full
operation list is in the stubs, and it is short on purpose: every operation is
in the deterministic subset, so there is no `pow`, `sin` or `sqrt` and asking
for one is asking to break the cross-platform guarantee.

**Water is placed at generation or not at all.** The fluid solver conserves
volume — it moves what exists and creates nothing — so there are no sources and
nothing pours a sea into being later:

```lua
game.register_fluid{ id = "water", material = "my_mod:water_block" }
-- ...then, inside on_generate, after the terrain:
buf:fill_fluid_below(0, "my_mod:water")      -- sea level at y = 0
```

Fluid is a layer over the same blocks, not a material. How much goes into a
block is the room the terrain leaves it, out of 27 cells, so a shoreline falls
out of the terrain rather than having to be described.

**Heightmaps still exist and are still right** when a heightmap is what you
mean. `game.noise_heightmap` + `buf:fill_below_heightmap` is 52 us a chunk
against 719 us for terrain-with-caves — fourteen times cheaper, and worldgen
runs on the simulation tick.

### 6. Mods register named actions; the engine owns the keys

A mod never reads a key. It declares an action with `game.register_action` and
responds to `register_on_action`; the player binds it in the settings screen.
There is no key code anywhere in the mod API, and asking for one is asking for
the wrong thing.

---

## What belongs in your mod, not in the engine

The engine is deliberately small and holds no opinion about what a world is.
Three things assistants routinely ask the engine for that are yours:

**Biomes are a Lua table.** A registry of names to parameters needs nothing
from the engine. Pick the biome from a value you already have — a `game.density`
field sampled at block resolution, or a heightmap — and index your own table.

**Ore placement is already possible, and is NOT per-sample work.** This is the
distinction to get right: the forbidden thing is O(volume) arithmetic in Lua.
Scattering ore is O(ores) — a few dozen `set_block` calls a chunk, chosen from a
seeded stream — and that is ordinary mod code:

```lua
game.register_on_generate(function(buf, pos)
    local rng = game.rng_stream(pos, "ore")
    for _ = 1, 12 do
        local x, y, z = rng:below(16), rng:below(16), rng:below(16)
        buf:set_block(x, y, z, iron)
    end
end)
```

Reading terrain to decide where ore may go is the same shape: bounded, and
proportional to what you place rather than to the volume you place it in.

**Erosion, rivers and biome blending are compositions**, not engine features.
Build them from `game.density`'s arithmetic. If a shape genuinely cannot be
expressed with the operations that exist, that is a finding worth reporting —
the answer is a new operation with a determinism argument, not a loop in Lua.

---

## The sandbox

Server mods run sandboxed for crash isolation: an error disables that mod and is
logged, and the tick keeps going. `os`, `io`, `dofile`, `loadfile`, `package`
and `ffi` are removed, and `_G` does not hand them back.

Client HUD scripts — the ones a server pushes to a player, via
`game.register_hud_script` — are sandboxed much harder still: no `os`, no `io`,
no `require`, no `load`, no `coroutine`, plus instruction and memory caps. A HUD
script draws and nothing else.

---

## A complete mod, start to finish

```lua
-- SPDX-License-Identifier: MIT
local stone = game.get_block_id("core:white")

game.register_block{
    id = "brick",
    name = "Brick",
    hardness = 1.2,
    textures = { all = "textures/brick.png" },   -- `all` is required
}

game.register_domain{ id = "quarry", generator = function(buf, pos)
    local heights = game.noise_heightmap(pos, { octaves = 4, frequency = 0.01, amplitude = 60.0 })
    buf:fill_below_heightmap(heights, stone)
end }

game.register_on_chat(function(event)
    if event.text ~= "quarry" then
        return
    end
    local body = game.player_entity(event.player)
    if body ~= nil then
        game.transfer_entity(body, "my_mod:quarry", { x = 8, y = 96, z = 8 })
    end
    return false          -- swallow the message
end)

game.log("my_mod ready")
```

`docs/fixtures/relief/` in the engine repository is this shape as a real,
runnable file, and `game/` holds larger worked examples — every one of them
written through this API and nothing else, which the build enforces rather than
merely intends. Good ones to read, smallest first: `core_worldgen` (36 lines,
the whole generation path), `core_tools` (118, tools and actions), `core_gear`
(340, items that are not blocks, drops, worn slots).

---

## Before you say it works

- `--check-mods` passes and your mod is listed.
- No `math.sin`, `math.atan`, `^` or any other float maths on a value that
  reaches the world. A density table instead.
- A `game.density` field compiled ONCE at load, not rebuilt per chunk.
- No numeric block id stored, compared to a literal, or written anywhere.
- Nothing registered outside `init.lua`'s first run.
- Quantities in units, and `count` never confused with `units`.
- A hook that can refuse returns the right thing — check the stub, because
  `false`, `nil` and a table mean different things per hook.

## Licensing

The engine is GPL-3.0-only, but [`../LICENSE.EXCEPTION`](../LICENSE.EXCEPTION)
is a formal Additional Permission under GPLv3 §7: work interacting with the
engine solely through the Lua API or the network protocol is not a derivative
work and carries no copyleft obligation. Everything in `api/` is MIT so
vendoring the stubs is unambiguously fine. **Your mod is yours, under whatever
licence you choose.**

---

## Keeping this file honest

It describes behaviour that changes. When the engine changes something a mod
author relies on, this file changes in the same commit — the same rule
`scripts/check-stubs.sh` enforces mechanically for the stubs, applied by hand to
the prose. If it contradicts [`stubs/game.lua`](stubs/game.lua), the stubs are
right and this is stale: fix it.
