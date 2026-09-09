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

**Erosion and rivers need a pre-pass, not a density field.** A density field is
a function of one position. Where water goes depends on where the land is
everywhere else, so it cannot be one — it needs a whole field computed once, in
passes that see all of it:

```lua
local function field()
    return game.map{ name = "height", side = 256, scale = 16 }
end

game.register_on_world_init(function()          -- ONCE in a world's life
    local map = field()
    map:noise{ seed = 7, frequency = 0.01, octaves = 4, amplitude = 40.0 }
    local worn = game.map{ name = "worn", side = 256, scale = 16 }
    worn:noise{ seed = 7, frequency = 0.01, octaves = 4, amplitude = 40.0 }
    worn:blur(3)
    map:combine(worn, "min")                    -- valleys cut, peaks kept
end)

game.register_on_generate(function(buf, pos)
    buf:fill_below_heightmap(field():heightmap(pos), stone)
end)
```

The map is stored with the world, so the pre-pass runs once and every later run
reads it back. **There is no `erode` and there will not be one** — what erosion
looks like is an opinion about what a landscape is, and that is yours. Blur and
`combine("min")` are the primitives it is built from.

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

## Terrain that does not look like a texture

Two things, and they are separate mechanisms because they fix different halves
of "it looks artificial".

**Shape: sub-node terrain.** `fill_density` takes a resolution:

```lua
buf:fill_density(field, stone, { detail = "smooth" })   -- no block staircases
buf:fill_density(field, stone, { detail = "sampled" })  -- and fine detail
```

Do NOT reach for `set_subnode` to do this. It writes one cell, and a chunk is
110,592 of them — the whole point of the option above is that the 27x sample
cost happens inside the engine, on the blocks the surface actually crosses.
Measured per chunk on terrain with caves: 729 us at block resolution, 1.08 ms
smooth, 3.98 ms sampled. `set_subnode` is for the handful of cells you place
deliberately, not for terrain.

**What that costs the server, and what the engine does about it.** Your
generator runs on the simulation thread, and the tick is 50 ms shared by
everything (charter rule 18). The engine will spend at most **half of it**
serving chunks — generating, lighting and sending them — and whatever does not
fit waits for the next tick. So an expensive generator does not make the world
stutter; it makes the world arrive more slowly, which is the trade worth having
and the one you can see coming.

The arithmetic is yours to do: at 3.98 ms a chunk, sampled detail fills about
six chunks a tick, and `smooth` at 1.08 ms fills nearly four times as many. A
player at view distance 24 is asking for tens of thousands of chunks, so that
ratio is minutes of waiting rather than a detail. Pick `sampled` where the
surface is worth it and `smooth` where it is not — they can be different fills
in the same generator.

If a server does run over its budget it says so, and it names your share:

```
a tick ran over its budget — 74.5ms total: serving 45.6ms, save chunks 28.7ms
  — serving 1 chunks, 1 summaries, 10 deferred; gen 45.1ms, light 0.5ms
```

`gen` is your generator. `light` is the engine lighting what you generated.
`deferred` is what the budget held back, which is the number that tells you the
server is keeping up with the tick but not with the player.

**Colour: a tint field.** Declare it on the block and the client does the rest:

```lua
game.register_block{
    id = "grass",
    tint = { strength = 0.12, low = {0.85, 1.0, 0.8}, high = {1.0, 0.95, 0.85}, scale = 40 },
}
```

`strength` alone gives tone variation, which is most of what stops a surface
reading as tiling; `low`/`high` add a hue shift across the same field. One field
for every material, keyed on world position, so a hillside varies as a hillside
rather than each block type drifting on its own. It costs nothing for materials
that declare nothing.

---

## Interfaces: what a mod can and cannot do to the look

**Pictures.** `{ type = "image", hash = ... }` in a dialog tree, and
`style.nine_slice` on any widget. Both take a content hash — the same hash the
material table and the sound table use — and the client fetches, decodes and
draws them. A picture that has not arrived yet draws nothing and fills in when
it lands, so do not design around it being there on the first frame.

A **nine-slice's border is a third of the image**, both ways. Draw your frame so
its corners are the outer third and they will keep their size at any box size
while the edges stretch; that is the whole point of a nine-slice and it is why
there is no border argument to get wrong.

Pictures may be up to **2048 pixels on an edge**, and no more than 8 MiB
decoded — which at four bytes a pixel means about 1448² in practice. A frame at
1254² is fine.

**Scrolling.** `{ type = "scroll", children = { ... } }` gives its children
their full height and clips them, and the wheel moves them when the pointer is
inside. That is the answer to a dialog with more in it than fits: put the long
part in a scroll box rather than shrinking the controls, and the controls keep
the size you asked for.

**Slot counts scale with the slot**, so a bigger `item_slot` gets bigger
numbers. You do not set the font.

**Fonts.** Register one and name it in a style:

```lua
game.register_font{ id = "display", file = "fonts/display.ttf" }
-- ...then on any widget that has text:
{ type = "label", text = "Chapter One", font = "my_mod:display", text_size = 24 }
```

Up to **eight fonts per server** and **2 MiB each** — the count cap is about the
client's glyph atlas rather than the files, because what costs is coverage, and
a face with a full CJK range is orders of magnitude more atlas than a Latin one.

A font a client cannot load, or a `font` naming one nothing registered, draws in
the client's own face. A missing file is never a missing screen, so do not
design a dialog that only makes sense in your typeface.

**Ship a font you have the right to ship.** The engine carries no opinion about
your licence and no way to check one; a font in your mod directory is published
with your mod.

---

## Machines: containers a mod can fill

A chest is a container a player drags things into. A **furnace** is one your mod
fills itself, on a tick, whether or not anybody is looking — and that is the
same mechanism with two more calls:

```lua
local FURNACE = "my_mod:furnace:" .. x .. "," .. y .. "," .. z
game.make_container(FURNACE, 3)                 -- fuel, input, output

game.register_on_tick(function()
    local ore = game.container_take(FURNACE, { material = "my_mod:ore", count = 1, slot = 2 })
    if ore > 0 then
        game.container_give(FURNACE, { material = "my_mod:ingot", units = ore, slot = 3 })
    end
end)
```

- **Slots are one-based and worth naming.** `slot = 2` is the input; without it,
  `container_take` would happily consume the ingots sitting in the output.
  `game.container(name)` gives each stack its `slot` back, and leaves empty ones
  out — so `#` counts what is in there, not how big it is.
- **Both answer in UNITS, not true or false.** A container is a fixed size, so a
  partial fit is ordinary: what did not fit was never taken from you. 27 units
  to a block (charter rule 5).
- **They work while a player has it open.** An open container lives in that
  player's own inventory, and the engine writes into the slots they are looking
  at, so a machine does not stop while its owner watches it.
- **One callback per hook per mod.** Two `register_on_tick` calls is an error,
  not a merge — put your machines in one tick function.

---

## Building the same thing twice: plans

A village, a dungeon, a ship, somebody's saved house. Do NOT write this as a
loop over `game.get_block` and `game.set_block` holding the result in Lua
tables — the engine has it, in one place, bounded:

```lua
local made = game.plans.capture("hut", { x = 0, y = 8, z = 0 }, { x = 7, y = 12, z = 7 })
if made then                                    -- nil, reason if it could not
    game.plans.stamp("hut", { x = 40, y = 8, z = 40 })
end
```

Four things worth knowing before you design around it:

- **A plan is named, never held.** `capture` saves under a name and every other
  call takes that name back, so a plan survives a restart and a clipboard is
  just a name you reuse.
- **Stamping ADDS.** A plan records only blocks that hold something, so it puts
  a building onto a hillside rather than cutting a box out of it. Clear the
  space yourself if you want the box.
- **`stamp` returns "accepted", not "done".** Big plans are paced across ticks
  by the engine, so a large building appears over a second or two. That is the
  engine protecting the 50 ms tick, and taking the pacing into your own hands is
  not available — nor should you want it.
- **`materials` in the summary is in UNITS**, 27 to a block, which is what an
  inventory counts in. `blocks` is the block count. Check the first against what
  a player is carrying before you stamp, not after.

Limits: 64 blocks on a side, 65,536 filled blocks, and everything you capture
must be in loaded terrain — a capture reaching unloaded chunks is refused whole
rather than coming back with holes you cannot see.

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

**Glass is a flag, not a shader.** `register_block{ transparent = true }` and
the block's own texture alpha decides how see-through it is. That one flag
changes three things and leaves the rest alone (Sub-Node Contract §8.1):

```lua
game.register_block{
    id = "glass",
    transparent = true,                       -- see through it, and light does
    textures = { all = "textures/glass.png" }, -- the PNG's alpha IS the opacity
}
```

- A face draws where exactly ONE side of it is transparent, so a wall behind a
  window is not a hole and two panes touching do not double up.
- It is drawn in a blended pass after the opaque world.
- Light passes through a whole block of it, so a glass roof does not make a dark
  room.

**Collision does not change — glass is solid.** You cannot walk through a
window, and it holds fluid in.

Two limits worth knowing before you file them as bugs. Panes seen through one
another at an angle are not sorted against each other, which is a deliberate
trade: sorting per quad is per-frame work proportional to the geometry. And only
a WHOLE block of one transparent material passes light — a chiselled or mixed
block holding glass falls back to the ordinary cell rule.

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
