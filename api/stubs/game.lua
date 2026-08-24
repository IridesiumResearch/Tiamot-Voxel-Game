-- SPDX-FileCopyrightText: Iridesium
-- SPDX-License-Identifier: MIT
--
-- Type stubs for the Tiamot mod API, annotated for the Lua Language Server.
--
-- MIT licensed, unlike the engine — copy this file into your mod, closed-source
-- or otherwise. See ../README.md.
--
-- =========================================================================
-- MODS ARE WRITTEN AGAINST LUA 5.4.
-- =========================================================================
--
-- That is settled and does not change; see docs/scripting-vm.md for the
-- measurements behind it. Lua 5.4 means you have an integer subtype, `//`, and
-- `%` that behaves on integers — which matters, because quantities in this
-- engine are integer units (27 per block) and you will divide them constantly.
--
-- =========================================================================
-- THE ONE RULE THAT IS NOT A STYLE PREFERENCE
-- =========================================================================
--
-- **Do not compute simulation values in Lua.**
--
-- The engine guarantees that the same seed produces bit-identical worlds on
-- Linux, Windows and macOS. That guarantee rests on restricting which
-- floating-point operations run — and the engine cannot police what happens
-- inside a script. `x^0.5` in your mod is a platform library call, and it will
-- give different last bits on different machines.
--
-- So: ask the engine for whole buffers and hand them to whole-buffer
-- operations. `game.noise_heightmap` gives you a heightmap;
-- `buf:fill_below_heightmap` consumes it. Neither lets you see the individual
-- numbers, and that is deliberate rather than an omission.
--
-- This file describes what exists. `scripts/check-stubs.sh` fails CI if the
-- engine registers something this file does not document.

---@meta

---@class Tiamot.ChunkPos
---@field x integer Chunk x, in chunks.
---@field y integer Chunk y, in chunks.
---@field z integer Chunk z, in chunks.
---@field seed integer The world seed.

---A per-column height field. Produced and consumed natively; you cannot read
---the individual heights, by design.
---@class Tiamot.Heightmap
local Heightmap = {}

---Number of columns. 256 for a chunk.
---@return integer
function Heightmap:len() end

---A chunk being generated.
---
---Every operation is whole-buffer or whole-block. Block-level calls are the
---cheap default; sub-node calls expand the buffer 27x and are opt-in (see the
---Sub-Node Contract, §5).
---@class Tiamot.ChunkBuffer
local ChunkBuffer = {}

---Fills every block with one material.
---@param material integer A numeric block id from `game.register_block` or `game.get_block_id`.
function ChunkBuffer:fill_all(material) end

---Fills every block below the given surface. The workhorse terrain operation.
---@param heightmap Tiamot.Heightmap
---@param material integer
function ChunkBuffer:fill_below_heightmap(heightmap, material) end

---Sets one whole block. Coordinates are chunk-local, 0..15.
---@param x integer
---@param y integer
---@param z integer
---@param material integer
function ChunkBuffer:set_block(x, y, z, material) end

---Sets one sub-node cell. **Expands the buffer to sub-node resolution**, which
---costs 27x the memory and fill time. Opt-in for a reason.
---@param bx integer Block x, 0..15.
---@param by integer Block y, 0..15.
---@param bz integer Block z, 0..15.
---@param sx integer Sub-node x within the block, 0..2.
---@param sy integer Sub-node y within the block, 0..2.
---@param sz integer Sub-node z within the block, 0..2.
---@param material integer
function ChunkBuffer:set_subnode(bx, by, bz, sx, sy, sz, material) end

---Whether this buffer has expanded to sub-node resolution.
---@return boolean
function ChunkBuffer:is_expanded() end

---A named random stream. Reproducible for the same world seed, chunk and name;
---uncorrelated with streams under other names.
---@class Tiamot.Stream
local Stream = {}

---A uniformly distributed integer in `0..bound-1`.
---@param bound integer
---@return integer
function Stream:below(bound) end

---A boolean with even odds.
---@return boolean
function Stream:next_bool() end

---Options for `game.noise_heightmap`.
---@class Tiamot.NoiseOptions
---@field octaves integer? Detail levels. Each doubles the cost. Default 4, capped at 16.
---@field frequency number? Inverse feature size. Default 0.02.
---@field lacunarity number? Frequency multiplier per octave. Default 2.0.
---@field gain number? Amplitude multiplier per octave. Default 0.5.
---@field amplitude number? Vertical scale, in blocks. Default 6.0.
---@field base integer? Height the noise varies around, in world blocks. Default 0.

---Fields accepted by `game.register_block`. Anything else is an error naming
---the field — a typo should stop you, not silently take a default.
---@class Tiamot.BlockSpec
---@field id string Required. Namespaced with your mod id automatically.
---@field name string? Display name.
---@field description string? One-line description.
---@field hardness number? Seconds to break with a bare hand. Default 0.75. Must not be negative. One SUB-NODE of it costs a thirteen-and-a-half-th of this, so chiselling a block out cell by cell takes twice as long as smashing it whole.
---@field dominance number? How strongly this material imposes its hardness on a block it is only part of. Default 1.0. Must be positive. See below.
---@field drops table<string, integer>? Overrides what breaking it yields: block id to UNITS (27 to a block). Omit for the ordinary rule — the block drops itself, 27 units whole or one per occupied sub-node. Bare ids are namespaced with your mod id.
---@field tags string[]? Arbitrary tags for other mods to match on.
---@field textures Tiamot.BlockTextures? Which images clients draw this block with.
---@field sounds { step: string }? What this block sounds like underfoot. The client plays its own footsteps from its own movement, so this is the only way it can know. Unqualified ids mean your own mod's.
---@field light_emit Tiamot.LightEmit? Light this block gives off. Omit for anything that is not a lamp.

---How `dominance` decides a mixed block's hardness.
---
---A block is 27 sub-node cells and each may be a different material, so the
---engine has to blend. It averages mining RATES, weighted by `dominance`:
---
---    rate = sum(dominance / hardness) / sum(dominance)     time = 1 / rate
---
---Averaging rates rather than times means the SOFT part of a block carries the
---rest away for free — dirt mixed into stone breaks at nearly dirt's speed with
---every dominance left at 1. What that alone cannot express is a material that
---is *sticky*: hard to cut in a way that makes everything packed around it hard
---to cut. `dominance` is that knob, and because the average is over rates it
---works in both directions at once:
---
---    dirt:   hardness = 0.5,  dominance = 3   -- weakens anything it is in
---    rubber: hardness = 10.0, dominance = 6   -- toughens anything it is in
---
---With those, half a block of dirt and stone breaks in 0.59 s (stone alone is
---1.5 s) and half a block of rubber and stone takes 5.68 s. A block with no
---`dominance` set anywhere still blends sensibly; the field is for materials
---that should punch above their share.
---
---The blend never leaves the range its materials define: a block of stone and
---dirt can never be harder than stone.

---Light a block gives off, per colour channel, 0 to 15.
---
---There is no sunlight channel: daylight comes from the sky, not from a block.
---A channel you omit is zero, so `{ r = 15 }` is a red lamp and not a white one.
---
---**The engine registers no light sources of its own.** A world whose mods
---define no emissive block is lit only by the sky — that is a mod set's
---decision, the same way a world with no tools is one nobody can dig in.
---@class Tiamot.LightEmit
---@field r integer? Red, 0..15. Default 0.
---@field g integer? Green, 0..15. Default 0.
---@field b integer? Blue, 0..15. Default 0.

---Textures for a block. Paths are relative to your mod's own directory, and the
---files are pushed to clients through the content pipeline — an absolute path,
---or one containing `..`, is refused.
---
---Only `all` exists today. Per-face keys are a natural extension and are
---deliberately not reserved in advance: adding them later is additive, whereas
---shipping a six-key schema nothing renders yet would freeze a guess into the
---mod API.
---@class Tiamot.BlockTextures
---@field all string Required. The image every face uses, e.g. `"textures/white.png"`.

---Fields accepted by `game.register_tool`.
---@class Tiamot.ToolSpec
---@field id string Required. Namespaced with your mod id automatically.
---@field name string? Display name.
---@field brush string? What shape it removes: `"block"` (default) or `"subnode"`.
---@field speed_multiplier number? How much faster than a bare hand. Default 1.0, must be positive.
---@field default boolean? Whether this is what a player digs with holding nothing. The engine has no bare hand of its own, so a world whose mods register no default is one nobody can dig in. Lowest id wins if several mods mark one.

---Fields accepted by `game.register_sky`.
---
---**Registration window only.** The engine has no sky of its own: a world whose
---mods register none has no day and holds its colours fixed, which is a
---legitimate world rather than a missing feature.
---@class Tiamot.SkySpec
---@field day_length_ticks integer Required. Ticks in a full day, at 20 ticks a second. Must be at least 1.
---@field keyframes Tiamot.SkyKeyframe[]
---@field start_time number? Where a fresh world's clock starts, 0..1. Defaults to mid-morning: a counter left at zero opens every world at midnight, which is the one hour with no sun in it. Required, and not empty. Need not be sorted — the engine sorts them, because an out-of-order list would make the sky walk backwards partway through the day.

---One moment in your day.
---
---The client interpolates between keyframes, so a handful describes a whole
---day. Make the last keyframe restate the first's colours, or the sky cuts hard
---at the moment the clock wraps.
---@class Tiamot.SkyKeyframe
---@field time number Required. When in the day, 0 to 1, where 0 is midnight and 0.5 is noon.
---@field sky number[] Required. `{r, g, b}` for the sky itself. Distance fog fades towards this, so it is also the horizon.
---@field sun number[] Required. `{r, g, b}` tinting the sunlight stored in the world.
---@field intensity number Required, 0 to 1. Scales stored sunlight at DRAW time — which is why a day/night cycle costs nothing: the world's sunlight is always full daylight and never needs relighting.
---@field grade Tiamot.SkyGrade? Optional. How the finished picture is graded at this moment. Omit it and nothing is graded.

---How a moment's finished picture is graded.
---
---**Lighting mode 3 only.** Grading happens in the post chain, and modes 1 and
---2 have no post chain to put it in — so a grade is polish on the highest
---setting rather than something the other modes will look wrong without.
---
---Every field is optional and defaults to doing nothing, so a keyframe can set
---one knob without restating the rest. The client interpolates these between
---keyframes like the colours, bakes the result into a lookup table once, and
---applies it with a single texture read per pixel — nothing here costs per-pixel
---maths, however many knobs you set.
---
---They apply in a fixed order: `exposure` multiplies the scene BEFORE the
---highlight roll-off (which is what makes it decide how much of the picture
---rolls off at all), then, on the finished image, `contrast` about mid grey,
---`saturation`, `tint` then `offset`, and `gamma` last.
---@class Tiamot.SkyGrade
---@field exposure number? 0 to 4, default 1. Multiplies the scene before the tonemap.
---@field tint number[]? `{r, g, b}`, each 0 to 4, default `{1, 1, 1}`. Multiplies the graded image.
---@field offset number[]? `{r, g, b}`, each -1 to 1, default `{0, 0, 0}`. Added after `tint`.
---@field contrast number? 0 to 4, default 1. Pushes each channel away from mid grey.
---@field saturation number? 0 to 4, default 1. Zero is greyscale of the same brightness; above 1 exaggerates.
---@field gamma number? 0.1 to 4, default 1. Applied last, per channel. Never 0 — a zero exponent maps the whole frame to white.

---Fields accepted by `game.register_action`.
---@class Tiamot.ActionSpec
---@field id string Required. Namespaced with your mod id automatically.
---@field default_key string? Suggested default binding, as a key name: "KeyF", "Space", "BracketLeft". The engine owns bindings; mods never read keys, and there is deliberately no way to ask which key a player chose.
---@field description string? One line for the settings screen, shown beside your mod's name.

---The mod API.
---
---Available inside your `init.lua` and every callback you register.
---@class Tiamot.Game
---@field CHUNK_BLOCKS integer Blocks along each axis of a chunk. 16.
---@field UNITS_PER_BLOCK integer Sub-node units in a block. 27.
---@field AIR integer The numeric id of air. Always 0.
---@field mod_id string Your mod's id, and your registration namespace.
game = {}

---Writes a line to the server log, attributed to your mod.
---@param message string
function game.log(message) end

---Registers a block.
---
---**Registration window only.** Calling this after the engine freezes the
---registries is an error — that is what makes numeric ids safe to persist.
---@param spec Tiamot.BlockSpec
---@return integer id The numeric id, for use with the fill operations.
function game.register_block(spec) end

---Registers the sky: how long a day is, and what colour it goes.
---
---**Registration window only.**
---
---Everything a player would call "the sky" is yours. The engine advances a
---number from 0 to 1 over the period you set and interpolates between the
---colours you list; it has no idea what dawn is.
---@param spec Tiamot.SkySpec
function game.register_sky(spec) end

---Registers a tool.
---
---**Registration window only**, like `game.register_block`.
---
---`brush` is what makes sub-node resolution reachable from a mod. `"block"`
---removes the whole block containing the targeted cell; `"subnode"` removes
---only the cell under the crosshair, which is how a chisel works. An unknown
---brush is an error naming it rather than a silent fallback.
---@param spec Tiamot.ToolSpec
function game.register_tool(spec) end

---Registers a world generation callback.
---
---**Registration window only.**
---@param callback fun(buf: Tiamot.ChunkBuffer, pos: Tiamot.ChunkPos)
function game.register_on_generate(callback) end

---Registers a per-tick callback.
---
---**Registration window only.**
---
---Runs once per simulation tick, at 20 Hz. `dt_ticks` is how many simulation
---steps this call covers — normally 1, but more when the server has fallen
---behind and is catching up.
---
---It is a **count of steps, not a duration**, and deliberately so: scaling
---behaviour by wall-clock time would make your mod produce different results on
---a fast machine than a slow one, and two servers running the same world would
---drift apart.
---
---If your callback raises an error, your mod is disabled for the rest of the
---session and every other mod carries on. Nothing you do here can stop the
---server's tick.
---@param callback fun(dt_ticks: integer)
function game.register_on_tick(callback) end

---The light at a block, right now.
---
---**Frozen phase only** — there is no world during registration, and asking
---then answers darkness rather than failing.
---
---Levels are 0..15 per channel, the same range `register_block{ light_emit }`
---takes, so a level read here can be written straight back into an emitter.
---`sun` is separate from the colour channels because it behaves differently:
---the engine stores full daylight and scales it by time of day when it draws,
---so a value of 15 means "open sky", not "bright right now". A mod deciding
---whether it is dark enough for something to spawn wants `sun` and the colour
---channels both.
---
---Coordinates are BLOCKS. A dig event gives you cells — divide by three.
---
---Somewhere unloaded answers darkness, which is the honest answer for a place
---nobody is: an error would push that judgement onto every caller.
---
---```lua
---local here = game.get_light{ x = 10, y = 64, z = -3 }
---if here.sun == 0 and here.r + here.g + here.b < 4 then
---    -- dark enough for something unpleasant
---end
---```
---@param position { x: integer, y: integer, z: integer }
---@return { sun: integer, r: integer, g: integer, b: integer }
function game.get_light(position) end

---Whether one point in the world can see another.
---
---**Positions are POINTS, not bodies.** This is a single line between two
---coordinates. If you mean "can this mob see that one", raise both ends to eye
---height yourself — a line drawn between two sets of feet clips the floor they
---are both standing on, and the engine has no idea where your mob keeps its
---eyes.
---
---Coordinates are world BLOCKS, as plain floats, the same as
---`game.spawn_entity` and `game.entity` speak. Fractions are kept: the test
---runs at sub-node resolution, so half a block matters and is not rounded away.
---
---Three answers, and the third is not a kind of `false`:
---
---* `true` — nothing solid stands between them.
---* `false` — something does; or they are more than 64 blocks apart; or the
---  line crosses terrain the server has not loaded. All three are "no" to what
---  you were really asking, and unloaded terrain reads as solid everywhere else
---  in the engine too.
---* `nil` — the engine had no world to look through at that moment. This
---  happens in `register_on_generate` (you are *making* the chunk you would be
---  asking about) and in a test with no server. **It is not "I could not see
---  it"** — a mob that stops following because it got `nil` looks exactly like
---  a mob that lost sight of you, and the two have completely different fixes.
---  Lua treats `nil` as false in a condition, so ignoring the difference is
---  safe; the distinction is there for when you are debugging.
---
---Available from `register_on_tick` and from an entity's step callback, which
---is where perception belongs. The cost is cells walked, so it is cheap over
---short distances and capped at 64 blocks.
---
---```lua
---local me = game.entity(id)
---local eye = { x = me.pos.x, y = me.pos.y + 1.5, z = me.pos.z }
---if game.line_of_sight(eye, { x = target.x, y = target.y + 1.5, z = target.z }) then
---    -- it is in view
---end
---```
---@param from { x: number, y: number, z: number }
---@param to { x: number, y: number, z: number }
---@return boolean|nil visible `true` clear, `false` blocked, `nil` no world to look through
function game.line_of_sight(from, to) end

---Options for `game.find_path`. Every field is optional; the defaults describe
---something humanoid.
---@class Tiamot.PathOptions
---@field budget? integer Blocks the search may expand before giving up. Default 2000, capped at 10000, and 0 means the default. A search is unbounded work wearing the shape of a function call, and this is what bounds it. **There is also a pool of 8000 expansions shared by every search in one tick**, so what you ask for is capped by what the tick has left — measured, 2000 expansions is about 0.5 ms, or 1% of a tick, and the whole pool is about 4%. A mob that cannot find a way in two thousand blocks should do something else, not stall the server.
---@field height? integer Blocks of clear space the body needs above its feet. Default 2. Not read off the entity's collider on purpose: you may want a mob to route only where it would also fit crouching, or to reserve headroom it does not strictly need.
---@field step_up? integer How far it climbs in one move, in blocks. Default 1. Zero for something that cannot climb at all.
---@field max_drop? integer How far it will fall in one move, in blocks. Default 3. Nothing here knows about fall damage — that is your rule, not the engine's — so this is only how far the search is willing to route.

---Called when a player arrives in the world.
---
---**The engine does not tell you whether this is their first time**, and that
---is deliberate. "First-ever join" is a rule you invent — a mob that imprints on
---one, a tutorial that greets one, a shop that gives one a starting stack — and
---every one of them wants a different definition of first. Remember it yourself
---with `game.storage`, keyed on the UUID.
---
---`player` is the UUID and is the identity. `name` is what they are currently
---called on this server, which is a claim bound to that UUID and can be rebound
---(charter rule 13) — store the UUID, print the name.
---
---Runs on the simulation thread inside the tick, before that player's first
---tick, so `game.spawn_entity`, `game.line_of_sight` and the rest all work here.
---
---```lua
---game.register_on_player_join(function(event)
---    if not game.storage.get("imprint") then
---        game.storage.set("imprint", event.player)
---    end
---end)
---```
---@param callback fun(event: { player: string, name: string })
function game.register_on_player_join(callback) end

---Called when a player presses or releases one of YOUR registered actions.
---
---Charter rule 11: you are told WHAT was done, never which key did it. There is
---no key in the event and there is not going to be — a mod that branched on the
---key would make rebinding change behaviour rather than just controls.
---
---Both edges arrive, so a "hold to..." control is written by watching
---`pressed`. Only actions YOUR server registered arrive; the engine's own
---controls (moving, digging, placing) are not actions and never appear here.
---
---Runs on the simulation thread inside the tick, before the entity step, so a
---mod that flips a mode here sees it while stepping its own mobs.
---
---```lua
---game.register_on_action(function(event)
---    if event.id == "my_mod:shout" and event.pressed then
---        game.log(event.player .. " shouted")
---    end
---end)
---```
---@param callback fun(event: { player: string, id: string, pressed: boolean })
function game.register_on_action(callback) end

---The tool a player is holding, or nil for a bare hand.
---
---Takes a player UUID in hex, the way a hook event reports one (charter rule
---13): key on the UUID, never the display name.
---
---Answers nil during worldgen, when nobody is holding anything yet.
---@param player string
---@return string? tool The qualified tool id, e.g. "core_tools:chisel".
function game.get_tool(player) end

---Puts a tool in a player's hand. `nil` is a bare hand.
---
---A tool decides what a dig REMOVES and what a placement WRITES, so this is how
---a mod builds a control that changes how digging behaves. If you swap somebody
---to another tool for the duration of a key, read `game.get_tool` first and put
---back what was there — assuming a bare hand takes the tool off anyone who had
---chosen one.
---
---Returns false for a tool nobody registered, or a player who is not connected;
---an unresolvable tool would be a dig that silently never progresses.
---@param player string
---@param tool string? The qualified tool id, or nil for a bare hand.
---@return boolean took
function game.set_tool(player, tool) end

---What a player is carrying in one of their views.
---
---Consolidated: one entry per material AND cut, so ten stairs in three slots
---are one entry of thirty items. The default view is `"player:main"`, the
---player's own bag; `"player:hotbar"` is the other one every player has.
---
---Each entry is `{ material = <numeric id>, units = , blocks = , nodes = ,
---count = , shape = }`. `units` is charter rule 5's own quantity, `blocks` and
---`nodes` are that split for display, `count` is how many ITEMS — whole blocks
---of loose material, or items of the cut. `shape` is the 27-bit occupancy mask
---of a cut and is nil for loose material, so `if entry.shape then` is the test
---for "is this a shaped stack".
---
---Answers an empty list during worldgen, when nobody is carrying anything yet.
---@param player string A player UUID in hex, as a hook event reports one.
---@param view string? Which view. Defaults to "player:main".
---@return table[] stacks
function game.inventory(player, view) end

---Puts material into a player's inventory.
---
---**This and `game.take` are what make crafting expressible.** The engine holds
---no recipes: turning twenty-seven units of stone into three stairs is a mod
---taking the stone and giving back the stairs, and the engine's only job is to
---make both halves possible and to conserve units across them (charter rule 5).
---
---The spec is `{ material = , units = | count = , shape = , view = }`.
---`material` is a block id like `"core:stone"` or a numeric material id.
---Quantity is either `units` — raw units — or `count`, which is items and is
---multiplied by what one costs: 27 units for loose material, or the number of
---cells in `shape` for a cut. `shape` is a 27-bit occupancy mask over the
---block's sub-nodes, indexed `x + 3*y + 9*z`; leave it out for loose material.
---
---Two stacks stack only if they are the same material AND the same shape, so
---giving somebody a cut never merges it into the rubble they were carrying.
---
---Returns false for a player who is not connected, or for a quantity of zero.
---An inventory never refuses for lack of room — it grows.
---@param player string A player UUID in hex.
---@param spec table
---@return boolean gave
function game.give(player, spec) end

---Takes material out of a player's inventory.
---
---The same spec as `game.give`, and the same defaults. **Returns how many
---UNITS it actually got**, which may be fewer than asked for — a mod that
---cannot complete a recipe can give back exactly what it took rather than
---having to ask twice how much that was.
---
---The cut is part of what is being spent: taking loose stone will not empty the
---stairs the player crafted out of it, and taking stairs will not drain their
---rubble.
---@param player string A player UUID in hex.
---@param spec table
---@return integer units How many units were removed.
function game.take(player, spec) end

---Called when a player says something in chat.
---
---**A veto.** Returning `false` stops the line reaching anybody, and returning
---a string stops it and tells the speaker why. That is what makes a chat filter
---expressible. The first refusal wins, so a later mod is not invited to act on
---a message that is not going to be sent.
---
---Chat itself is ENGINE-native and works with no mods loaded, because
---moderation and RCON depend on it: an operator must be able to read and stop
---what is said without every server having installed the same mod. What may be
---said is policy, and policy is yours (charter rule 1).
---
---`player` is the UUID and is the identity — store the UUID, print the name
---(charter rule 13).
---
---```lua
---game.register_on_chat(function(event)
---    if event.text:find("spoiler") then
---        return "not in this world, thank you"
---    end
---end)
---```
---@param callback fun(event: { player: string, text: string }): boolean|string|nil
function game.register_on_chat(callback) end

---Fields accepted by `game.register_sound`.
---@class Tiamot.SoundSpec
---@field id string Required. Namespaced with your mod id automatically.
---@field file string Required. Path inside your mod directory, e.g. "sounds/break.ogg". Ogg Vorbis or WAV — see the limits below.
---@field gain number? Loudness multiplier on the file's own level. Default 1.
---@field pitch_variance number? How much to vary the pitch each play, as a fraction. Default 0, which makes a repeated sound machine-like.

---One widget in a dialog tree.
---
---`type` decides which other fields are read; an unknown type or an unknown
---field is an error naming it, rather than something a client is asked to make
---sense of. Nesting is through `children`, and is limited to 32 deep.
---
---Types: `container`, `label`, `button`, `image`, `text_input`, `checkbox`,
---`slider`, `dropdown`, `item_slot`, `item_grid`, `scroll`, `spacer`,
---`progress`, `shape_editor`.
---@class Tiamot.Widget
---@field type string Required. One of the types above.
---@field name string? What events from this widget carry, so you can tell two buttons apart.
---@field children Tiamot.Widget[]? Only for `container` and `scroll`.
---@field style Tiamot.WidgetStyle?
---@field grow integer? Share of the parent's leftover space. 0 takes only what it needs.
---@field size integer? Fixed size along the parent's direction, in virtual pixels.
---@field cross_size integer? Fixed size across it.
---@field direction string? `container`: "row" or "column". Default "column".
---@field gap integer? `container`: space between children.
---@field padding integer? `container`: space inside its own edges.
---@field align string? `container`: "start", "center", "end" or "stretch".
---@field text string? `label`, `button`, `checkbox`.
---@field hash integer[]? `image`: 32 bytes of content hash.
---@field initial string? `text_input`: what is in it to begin with.
---@field placeholder string? `text_input`: shown when empty.
---@field checked boolean? `checkbox`.
---@field min integer? `slider`.
---@field max integer? `slider`.
---@field value integer? `slider`.
---@field options string[]? `dropdown`.
---@field selected integer? `dropdown`: which option, one-based.
---@field view string? `item_slot`, `item_grid`: which inventory view.
---@field index integer? `item_slot`: which slot, one-based.
---@field columns integer? `item_grid`: slots per row.
---@field first integer? `item_grid`: the first slot shown, one-based.
---@field count integer? `item_grid`: how many slots.
---@field permille integer? `progress`: how full, 0 to 1000.
---@field shape integer? `shape_editor`: the 27-bit occupancy mask, indexed `x + 3*y + 9*z`. Defaults to a whole block.
---@field material integer? `shape_editor`: which material the cells are drawn as.

---What a widget may say about how it looks. Deliberately small.
---@class Tiamot.WidgetStyle
---@field background integer[]? `{r, g, b}` or `{r, g, b, a}`.
---@field border integer[]? Same shape. The width is the client's.
---@field nine_slice integer[]? 32 bytes of content hash, stretched around the widget.
---@field text_colour integer[]? Same shape as `background`.
---@field text_size integer? In virtual pixels; the client keeps it legible.

---Fields accepted by `game.show_dialog` and `game.update_dialog`.
---@class Tiamot.DialogSpec
---@field player string Required. The player's UUID — never their display name (charter rule 13).
---@field form string Required. Your name for this dialog, namespaced with your mod id automatically.
---@field tree Tiamot.Widget Required. The root widget.

---Shows a dialog on a player's screen.
---
---**No code crosses the wire.** The tree is data; the client renders it and
---executes nothing. That is why the widget set is fixed and why an unknown type
---is an error here rather than something the client has to refuse.
---
---The dialog belongs to you: only your mod is told about its events, so no
---other mod can watch what a player types into your text field or act on your
---buttons. That is also why `form` is namespaced — two mods may both use
---"inventory" without colliding.
---
---Returns whether the player was there to show it to, which is NOT a promise it
---rendered.
---@param spec Tiamot.DialogSpec
---@return boolean shown
function game.show_dialog(spec) end

---Replaces the contents of a dialog already open.
---
---A whole tree, not a patch: a dialog is small, and a patch stream that ever
---dropped a message would leave a player looking at something you do not
---believe is there.
---@param spec Tiamot.DialogSpec
---@return boolean shown
function game.update_dialog(spec) end

---Closes a dialog you opened.
---@param spec { player: string, form: string }
---@return boolean closed
function game.close_dialog(spec) end

---# The shape editor
---
---`{ type = "shape_editor", shape = <mask>, material = <id> }` draws a block as
---twenty-seven cells and lets the player chisel it. Left-click takes off the
---nearest cell, right-click puts one back against the face that was clicked —
---the same gesture as digging and placing, because a player already knows it.
---
---Every change reports `"chiselled"` with the WHOLE mask. The client draws its
---own copy so a click lands immediately, and adopts yours again whenever you
---send a tree with a different `shape` — so a "reset" button is just
---`game.update_dialog` with the mask you want.
---
---The mask may be empty or full, which a `game.give` shape may not be: a whole
---block is where chiselling starts, an empty one is where it can end up, and
---deciding what either means is yours. `shape` counts its own cells, so an item
---of that cut costs that many units.
---
---Called when a player does something in one of YOUR dialogs.
---
---Only your own: a dialog's events are private to the mod that opened it.
---
---`event.kind` says what the player did, and which other fields are set:
---
---  - `"pressed"` — `name`
---  - `"submitted"` — `name`, `text`
---  - `"toggled"` — `name`, `checked`
---  - `"slid"` — `name`, `value`
---  - `"chose"` — `name`, `index` (one-based)
---  - `"clicked"` — `view`, `index` (one-based), `click` ("left", "right", "shift_left")
---  - `"chiselled"` — `name`, `shape` (the whole 27-bit mask)
---  - `"closed"` — nothing else
---
---**Every one is a REQUEST, never a result.** A slot click says what the player
---did with the mouse; whether any item moves is the server's decision, taken
---against its own inventory. A client saying "I moved this" does not make it so.
---@param callback fun(event: { player: string, form: string, kind: string, name: string?, text: string?, checked: boolean?, value: integer?, index: integer?, view: string?, click: string?, shape: integer? })
function game.register_on_dialog_event(callback) end

---Registers a sound. Registration window only.
---
---The file travels to clients by hash, through the same content pipeline a
---block texture uses — you ship the file in your mod directory and the engine
---does the rest. Two mods shipping byte-identical files send those bytes once.
---
---**Formats: Ogg Vorbis or WAV, and prefer Ogg.** A client decodes files from
---servers it has no reason to trust, so the WAV path refuses anything whose
---structure has more than one reading: exactly one `fmt ` chunk, a RIFF size
---that matches the file, and chunks that account for it exactly. A WAV straight
---out of an encoder passes; one carrying trailing editor metadata may not.
---
---**Limits, all checked before anything is allocated:** 4 MiB per file, at most
---8 channels, at most 192 kHz, and at most a minute at 48 kHz. Anything longer
---is music, which wants streaming rather than a decoded buffer, and streaming
---does not exist yet.
---
---A file that is missing, oversized or malformed disables that ONE sound, with
---a warning naming the server. It never refuses the join and never stops the
---client.
---@param spec Tiamot.SoundSpec
function game.register_sound(spec) end

---Binds a sound to a named event. Registration window only.
---
---**This is the standard way to give anything a noise.** `register_sound` says
---a file exists; this says *when it plays*. Keeping them apart is what makes
---sound a system rather than a habit: the engine and every mod raise named
---**cues**, and any mod binds any sound to any cue without either side knowing
---the other exists.
---
---A cue you write unqualified becomes your own. A qualified one is taken as
---written, which is deliberate — binding to another mod's cue is how you
---re-skin its doors without touching its code.
---
---```lua
----- Your own events.
---game.bind_sound("door_open", "creak")
---
----- The engine's. These are the player's OWN actions, and the client plays
----- them without waiting for the server, because a sound of something you did
----- arriving 80 ms late reads as a worse sound rather than as latency.
---game.bind_sound("engine:jump", "grunt")
---game.bind_sound("engine:land", "thud")
---game.bind_sound("engine:ui_click", "click")
---game.bind_sound("engine:ui_close", "click")
---```
---
---A cue nobody bound is silence, never an error. Raise your cues whether or not
---anybody has given them a noise; a sound pack is something somebody adds later.
---@param cue string The event. Unqualified means your own.
---@param sound string The sound id. Unqualified means your own.
function game.bind_sound(cue, sound) end

---Raises a cue, playing whatever is bound to it.
---
---The same delivery `game.play_sound` has — a radius, and only the players
---inside it are told — with the sound chosen by the binding table instead of by
---you. Returns how many players were told, which is 0 when nothing is bound.
---
---You may **not** raise an `engine:` cue. Those mean "this player just did
---this", and the client trusts them without asking anybody.
---
---```lua
---game.cue{ cue = "door_open", pos = pos, radius = 12 }
---```
---@param spec { cue: string, pos: { x: number, y: number, z: number }, radius?: number, gain?: number, entity?: integer }
---@return integer told
function game.cue(spec) end

---Starts a looping sound. Returns how many players were told.
---
---**Ambience is a loop, not a repeated clip.** Day, night, weather, the inside
---of a cave, a river ten blocks away — none of these are events, and playing a
---clip over and over means guessing its length and hearing every seam.
---
---`everywhere = true` is what makes ambience expressible: no position, no
---panning, full gain wherever the player stands. Without it the loop sits at
---`pos` and attenuates over `radius` like anything else.
---
---**Starting a loop that is already running replaces it**, so the natural thing
---to write — making sure the night loop is on, every tick — does not end up
---with a tick's worth of overlapping copies.
---
---```lua
---game.register_on_tick(function()
---    if game.time_of_day() > 0.75 then
---        game.play_loop{ id = "night", sound = "crickets", everywhere = true, gain = 0.6 }
---    else
---        game.stop_loop("night")
---    end
---end)
---```
---@param spec { id: string, sound: string, pos?: { x: number, y: number, z: number }, radius?: number, gain?: number, everywhere?: boolean }
---@return integer told
function game.play_loop(spec) end

---Walks an entity one tick toward a place, jumping over what is in the way.
---
---**The missing half of pathfinding.** `game.find_path` says which blocks to
---visit; this turns "the next waypoint is over there" into the drive a body
---actually takes — including whether to jump, which needs a look at the block in
---front of the mob's feet and is therefore the engine's job rather than yours.
---
---A mob that walks into a one-block step climbs it. Call it every tick with
---wherever you want the mob to go — the next waypoint of a route, or the player
---it is following.
---
---```lua
---game.register_on_tick(function()
---    for _, id in ipairs(game.entities_in_radius(home, 64, "mymod")) do
---        local target = whoever_it_is_chasing(id)
---        if target and not game.steer_entity(id, target.pos) then
---            -- Arrived. Do whatever arriving means.
---        end
---    end
---end)
---```
---
---Returns `true` while it is still going, `false` once it has arrived, and `nil`
---if the entity is gone or the world could not be looked at this tick — so a
---plain `if not game.steer_entity(...)` treats "arrived" and "could not ask" the
---same way, which is usually what you want.
---
---Sets the entity's `drive`, so anything you set yourself in the same tick is
---overwritten. Steer or drive; not both.
---`gait` takes the same three names `set_entity`'s drive does — `"walk"`,
---`"sprint"`, `"sneak"` — and an unrecognised one walks, because a typo in a
---gait should not stop a mob dead.
---@param id integer
---@param target { x: number, y: number, z: number }
---@param gait "walk"|"sprint"|"sneak"|nil
---@return boolean|nil going
function game.steer_entity(id, target, gait) end

---Where the day stands: 0 at midnight, 0.5 at noon, wrapping at 1.
---
---The same number the sky is drawn from, so a mod crossfading night ambience
---into day is working from what the player can see. `0.0` in a world whose mods
---registered no sky, which is a world with no day rather than an error.
---@return number time
function game.time_of_day() end

---Stops a looping sound by the id you gave it. Returns how many were told.
---
---Stopping one that is not running is not an error, so tidying up on shutdown
---does not mean remembering what you started.
---@param id string
---@return integer told
function game.stop_loop(id) end

---Registers a HUD script this mod wants clients to run. Registration window only.
---
---**This is the only thing your mod can send that RUNS on a player's machine**,
---and the rules are correspondingly tight. Everything else — dialogs, sounds,
---textures — is data a client interprets. A HUD is not expressible as data: a
---health bar's length is a *function* of the health, and putting "a function of"
---on the wire would mean inventing a worse language than Lua.
---
---The file travels by hash through the same content pipeline a texture uses.
---Ship `hud.lua` in your mod directory and name it here. **One per mod, last
---call wins** — the client budgets per script per frame, so two scripts from one
---mod would quietly buy you twice a one-script mod's budget. Concatenate
---instead.
---
---Inside the script you have `hud` and a small standard library, and **nothing
---else**: no `os`, no `io`, no `require`, no `load`, no `coroutine`, no
---filesystem, no network. This is an allow-list, not a deny-list — a future Lua
---version cannot add a capability into it by existing.
---
---```lua
---game.register_hud_script("hud.lua")
---```
---
---and in `hud.lua`:
---
---```lua
---hud.on_draw(function(state)
---    hud.hide_builtin("crosshair")
---    for index, slot in ipairs(state.carried) do
---        hud.icon{ anchor = "bottom", x = (index - 1) * 56 - 224, y = 72, size = 48,
---                  material = slot.material }
---        hud.text{ anchor = "bottom", x = (index - 1) * 56 - 224, y = 26,
---                  text = slot.blocks .. "+" .. slot.nodes, size = 18 }
---    end
---end)
---```
---
---**Your callback runs once a frame with a budget of about 200,000
---instructions.** Go over it and that frame's drawing is discarded whole — a
---half-drawn hotbar is worse than none — the player is shown a warning naming
---your mod, and the script sits out twelve frames. Five strikes and it is
---switched off for the session. Draw; do not compute.
---
---`state` is read-only and small on purpose: your own position, facing, time of
---day, what you carry, what you are looking at, the dig in progress, the tool in
---hand. It is not a way to read terrain the player cannot see.
---
---Chat and the settings screen are not in `hud.hide_builtin` and never will be —
---moderation and rebinding have to work whatever a server pushes.
---@param file string Path to the Lua file inside your mod directory.
function game.register_hud_script(file) end

---Fields accepted by `game.play_sound`.
---@class Tiamot.PlaySpec
---@field sound string Required. A sound id; unqualified means your own.
---@field pos { x: number, y: number, z: number } Required. Where it happens, in world blocks. Ignored when `entity` is set.
---@field radius number? How far it carries, in blocks. Default 16, capped at 512. Players outside are not sent it at all.
---@field gain number? Loudness multiplier on the sound's registered gain. Default 1.
---@field entity integer? An entity to follow, if the sound should move with one.

---Plays a sound for everyone close enough to hear it.
---
---Returns how many players were told — which is NOT a promise anybody heard
---it: a client may have it muted, may still be fetching the file, or may have
---refused it as a poisoned asset.
---
---A careless number is clamped rather than refused: `0/0` is a quiet NaN in Lua
---and would otherwise reach a mixer.
---@param spec Tiamot.PlaySpec
---@return integer told
function game.play_sound(spec) end

---A walkable route between two points, or why there is not one.
---
---Navigation is **block resolution** and deliberately simple (Sub-Node Contract
---§6): a block is walked through only if its bottom sub-node layer — the nine
---cells at the floor — is empty. So a mob may fail to path through a gap you
---could squeeze into. That is an accepted limitation, not a bug: sub-node
---navigation would multiply the search by twenty-seven for very little.
---
---Returns an array of `{ x, y, z }` in world blocks, start first and goal last,
---**horizontally centred** — a mob steers at a point, and the corner of a block
---is not where anything walks. `y` is the block's floor, which is where feet go.
---
---On failure returns `nil` and a reason, and the reason matters:
---
---* `"unreachable"` — everything reachable was searched and the goal was not in
---  it; or the goal is not somewhere this body could stand; or the ends are more
---  than 192 blocks apart. Asking again will not help.
---* `"budget"` — the search ran out first, either its own or the tick's shared
---  pool. **Not the same thing as unreachable.** A nearer target, a bigger
---  budget, or simply the next tick might succeed. A mob that treats this as
---  "there is no way" gives up on somewhere it could have reached. Searching
---  every tick is how you exhaust the pool and make it everyone's problem:
---  repath when the target has moved, not because a tick went by.
---* `"no world"` — asked when the engine had no world to search, which is
---  `register_on_generate` and a test with no server. See `game.line_of_sight`.
---
---The goal must be somewhere the body could stand: floor underneath, headroom
---above. A route into a wall or into mid-air is not a route.
---
---```lua
---local route, why = game.find_path(here, there, { max_drop = 1 })
---if route then
---    steer_towards(route[2])          -- [1] is where you already are
---elseif why == "budget" then
---    aim_somewhere_closer()
---end
---```
---@param from { x: number, y: number, z: number }
---@param to { x: number, y: number, z: number }
---@param options? Tiamot.PathOptions
---@return { x: number, y: number, z: number }[]|nil route
---@return string|nil reason `"unreachable"`, `"budget"` or `"no world"` when there is no route
function game.find_path(from, to, options) end

---Fields accepted by `game.register_fluid`.
---@class Tiamot.FluidSpec
---@field id string Unqualified id. `"milk"` from mod `core_milk` becomes `"core_milk:milk"`.
---@field material string The registered block a full block of it is drawn as. REQUIRED — a fluid with no material cannot be drawn, and the engine does not get to decide what your fluid looks like (charter rule 1). Qualified against your own mod, so a fluid can name its own block.
---@field flow_range? integer How far a source spreads sideways on flat ground, in blocks. 1..=7, default 7. The level a block holds IS how far the fluid has travelled, which is why seven is the ceiling — a shorter range is a fluid that thins out faster.
---@field tick_rate? integer Simulation ticks between updates. Default 1, which is every fluid tick (10 Hz). Larger is slower and more viscous, and costs proportionally less to simulate.
---@field color? { r: integer, g: integer, b: integer } What the world looks like from INSIDE the fluid — the tint and fog a submerged camera sees. Channels are 0..=255 and default to white. Deliberately not derived from `material`: a texture is what the surface looks like from outside, and clear water has a vivid surface with a faint tint. The engine has no opinion about either.
---@field renews_from? integer How many of the four lateral neighbours must be sources before a block becomes a source itself. 0..=4, default 0, which never renews. Set it to 3 for water that behaves like an ocean: without renewal a source exists exactly once, so scooping one out of the middle of a lake leaves flow that drains away, and a body of water collapses as people fill buckets from it. Three rather than Minecraft's two on purpose — at two, any 2x2 pool is an infinite well. **This creates matter out of nothing, which is why the engine defaults it off and leaves the decision to you.**

---Registers a fluid.
---
---Registration only, during the loading window — see charter rule 9. The engine
---keeps what it must simulate and draw with; everything else a fluid might do,
---like hurting you or making a sound, is yours and needs no engine support
---beyond the hooks that already exist.
---
---Fluid is BLOCK resolution, not sub-node. A block holds fluid only if it is
---entirely empty — Sub-Node Contract §4 — so there is no such thing as a
---partially flooded chiselled block, and you do not have to think about one.
---
---```lua
---game.register_block{ id = "milk", texture = "milk.png" }
---game.register_fluid{ id = "milk", material = "milk", flow_range = 7 }
---```
---@param spec Tiamot.FluidSpec
function game.register_fluid(spec) end

---What a block holds.
---
---`level` is 0..7, where 7 is a source or a block directly fed by one and each
---block of lateral travel costs one. `source` says the block sustains itself
---rather than draining — the two differ, and that difference is what makes a
---channel empty when you take its spring away.
---
---Coordinates are BLOCKS. A dig event gives you cells — divide by three.
---
---Somewhere unloaded answers empty, which is the honest answer for a place
---nobody is.
---
---```lua
---local here = game.get_fluid{ x = 10, y = 64, z = -3 }
---if not here.empty and here.level > 4 then
---    -- deep enough to swim in
---end
---```
---@param position { x: integer, y: integer, z: integer }
---@return { level: integer, source: boolean, empty: boolean }
function game.get_fluid(position) end

---Puts fluid in a block, or takes it away.
---
---Returns whether anything changed. Writing to a block that cannot accept fluid
---is not refused: the next fluid tick clears it, which is the same answer the
---engine gives when a player builds in a pond. One rule rather than two.
---
---`level = 0` clears the block whatever `fluid` names. `source = true` places a
---spring, which sustains itself until something removes it; anything else
---places flowing fluid that drains once nothing is feeding it.
---
---```lua
---game.set_fluid({ x = 10, y = 64, z = -3 }, { fluid = "core_milk:milk", source = true })
---game.set_fluid({ x = 10, y = 64, z = -3 }, { level = 0 })  -- scoop it up
---```
---@param position { x: integer, y: integer, z: integer }
---@param spec { fluid: string, level?: integer, source?: boolean }
---@return boolean changed
function game.set_fluid(position, spec) end

---Replaces a whole block, at runtime.
---
---**The way a mod changes the world after worldgen.** `register_on_generate`
---writes terrain while a chunk is being made; this writes it while people are
---standing on it — a block that changes when fluid reaches it, a crop that
---grows, a fire that spreads.
---
---Blocks are NAMED, not numbered, and that is not a convenience. Numeric ids
---come in two flavours — the runtime ids registration hands out and the world
---ids the database keeps — and they are different numbers. A mod given the
---wrong one gets a comparison that works whenever the two happen to coincide.
---Names have one meaning.
---
---The edit is QUEUED and lands on the next tick, so this returns whether it was
---accepted rather than whether it landed — look next tick to find that out.
---`false` means the queue is full or nothing is registered under that name.
---
---Pass `"engine:air"` to clear a block.
---
---```lua
---game.set_block({ x = 10, y = 64, z = -3 }, "core_milk:waterlogged")
---```
---@param position { x: integer, y: integer, z: integer }
---@param block string A registered block id, qualified.
---@return boolean queued
function game.set_block(position, block) end

---A dig about to happen.
---@class Tiamot.DigEvent
---@field player string Who is digging, as 64 hex characters. This is the canonical player UUID — key any per-player state on it, never on the display name, which a player can change and which is not unique across servers.
---@field x integer Sub-node cell being dug. These are CELL coordinates, three per block on each axis, so the block is `x // 3`.
---@field y integer
---@field z integer
---@field material integer Numeric id of what is there. Compare against `game.get_block_id("yourmod:something")`.
---@field brush string `"block"` for the whole block, `"subnode"` for the single cell.

---A placement about to happen.
---@class Tiamot.PlaceEvent
---@field player string Who is placing, as 64 hex characters.
---@field x integer The BLOCK being written — block coordinates, not cells.
---@field y integer
---@field z integer
---@field material integer What it would be made of.
---@field occupancy integer Bitmask of which of the block's 27 cells would be filled.
---@field units integer How many units it would cost, which is the number of set bits in `occupancy`.

---Registers a veto on completed digs.
---
---**Registration window only.**
---
---Called when a dig has finished counting down and is about to remove
---geometry — before anything is removed, so refusing costs nothing and leaves
---no trace. Return `false` to cancel it.
---
---Anything else allows it, including returning nothing at all. That is
---deliberate: a hook that only wants to watch should not have to remember to
---return something, and forgetting a `return` should not silently make the
---world unbreakable.
---
---**The first mod to refuse wins, and the hooks after it do not run.** Once the
---dig is not happening, running them would invite them to take side effects for
---an action that will not occur. Order is mod load order.
---
---If your callback raises an error, your mod is disabled for the rest of the
---session **and the dig goes ahead**. A crash is not a veto — otherwise one
---broken mod could stop everybody on the server from digging.
---Return `false` to refuse with the engine's wording, a string to refuse with
---your own, or `""` to cancel silently — the same ladder
---`game.register_on_place` uses.
---@param callback fun(event: Tiamot.DigEvent): boolean|string|nil
function game.register_on_dig_complete(callback) end

---Registers a veto on placements.
---
---**Registration window only.**
---
---Called after the engine's own rules have passed — the player is carrying the
---material, the target block is empty, and nobody is standing in it — and
---before anything is written or charged. Cancelling leaves the player holding
---their material.
---
---**Cancelling has two meanings and what you return says which.** Refusing the
---player is one; HANDLING the placement yourself and not wanting a block
---written is the other — `core_milk` places milk by intercepting a placement,
---pouring a fluid, and cancelling the write.
---
---| return | meaning |
---| --- | --- |
---| nothing, or anything truthy | the placement goes ahead |
---| `false` | refused, and the player is told "you cannot build there" |
---| `"reason"` | refused, and the player is told exactly that |
---| `""` | cancelled, and the player is told nothing — you handled it |
---
---A returned reason is truncated to 512 bytes.
---
---The same rules as `game.register_on_dig_complete` otherwise: the first
---cancellation stops the rest, and an error disables your mod while letting the
---placement through.
---@param callback fun(event: Tiamot.PlaceEvent): boolean|string|nil
function game.register_on_place(callback) end

---Somebody hitting something.
---@class Tiamot.PunchEvent
---@field attacker string Who threw the punch, as 64 hex characters.
---@field target integer The entity that took it, as `game.entity` names one. Everything in the world is an entity, including the other players.
---@field owner string|nil The player that entity belongs to, if it belongs to one — so "did somebody hit a person" is one field rather than a lookup.
---@field player string The same value as `attacker`. Present so every hook event has a `player` field; prefer `attacker` here, because a punch has two parties and `player` does not say which.

---Registers a veto on punches.
---
---**Registration window only.**
---
---The engine has no damage model and no opinion about what a hit does — that is
---your rule, not its (charter rule 1). It tells you who hit what, having already
---checked that the attacker could reach it, and stops there.
---
---The rules are the other hooks': return `false` to cancel, anything else
---allows, the first refusal stops the rest, and an error disables your mod while
---letting the punch land.
---
---```lua
---game.register_on_punch(function(event)
---    if event.owner == game.storage.get("imprint") then
---        return false  -- this one is protected
---    end
---end)
---```
---@param callback fun(event: Tiamot.PunchEvent): boolean?
function game.register_on_punch(callback) end

---Fluid pressing against something it cannot get into.
---
---Coordinates are BLOCKS on both ends, and they are named rather than being bare
---`x`/`y`/`z` — a dig event's `x`/`y`/`z` are CELLS, and the two have been
---confused before.
---@class Tiamot.FluidFlowEvent
---@field from { x: integer, y: integer, z: integer } The block the fluid is in.
---@field into { x: integer, y: integer, z: integer } The block it could not enter.
---@field fluid string The fluid's registered id, e.g. `"core:milk"`.
---@field level integer What level it is pressing at, 1 to 7.
---@field block string? The blocking block's id, or nil if nothing registered it.
---@field occupancy integer 27-bit mask of which of the blocking block's sub-nodes are filled.
---@field units integer How many of the 27 are filled — `occupancy`'s popcount.

---Registers a listener for flows that could not happen.
---
---**Registration window only.**
---
---# Why the hook is about the flow that DIDN'T happen
---
---Where a fluid went is already in the world and `game.get_fluid` will tell you.
---Where it *tried* to go is recorded nowhere: a block milk cannot enter is a
---block with no milk in it, and that looks exactly like a block milk never
---reached. This is the only way to learn the difference.
---
---That is the fact waterlogging needs — see `game/core_milk` for the reference
---implementation, which swaps a block for a wet one when milk presses on it.
---
---# It cannot veto
---
---The flow has already failed; there is nothing left to allow or refuse, so a
---return value is ignored. Act on the world instead, with `game.set_block` or
---`game.set_fluid`. An error still disables your mod, as everywhere else.
---
---# It is budgeted, and it is not exhaustive
---
---At most 64 blocked flows are reported per fluid tick, and the surplus is
---DROPPED rather than queued — so a shoreline a thousand blocks long is sampled
---across several ticks rather than delivered at once. Write the callback so that
---missing one is harmless: the same shoreline is still there next tick.
---
---Nothing fires for a settled world at all. The solver only examines blocks an
---edit woke or a flow is moving through, so a pond nobody has touched costs
---nothing here either.
---@param callback fun(event: Tiamot.FluidFlowEvent)
function game.register_on_fluid_flow(callback) end

---Registers a named input action.
---
---Mods register actions; the engine owns key bindings and mods never read keys.
---Stored now, inert until Task 13.
---
---**Registration window only.**
---@param spec Tiamot.ActionSpec
function game.register_action(spec) end

---Looks up a registered block's numeric id by its string id.
---
---String ids like `"core:white"` are stable forever. Numeric ids are per
---session and must never be persisted or hard-coded.
---@param id string
---@return integer
function game.get_block_id(id) end

---Generates a heightmap for a chunk from fractal noise.
---
---One call fills all 256 columns natively. There is no per-sample entry point,
---and that is the point — see the rule at the top of this file.
---@param pos Tiamot.ChunkPos
---@param options Tiamot.NoiseOptions
---@return Tiamot.Heightmap
function game.noise_heightmap(pos, options) end

---A heightmap with the same height in every column.
---@param height integer World block height.
---@return Tiamot.Heightmap
function game.flat_heightmap(height) end

---Opens a named random stream for a chunk.
---
---Use a distinct name per purpose. Streams under different names are
---uncorrelated, so drawing more numbers for one cannot shift another — which
---means you can change one generator without moving everything else in the
---world.
---@param pos Tiamot.ChunkPos
---@param name string
---@return Tiamot.Stream
function game.rng_stream(pos, name) end

--- ENTITIES ----------------------------------------------------------------
---
--- Positions here are WORLD BLOCKS, as plain numbers, and may be fractional —
--- one block is one yard. There is no chunk frame in this API and there is not
--- meant to be: the engine anchors positions to a chunk internally so it never
--- accumulates a world-space float, and a mod that had to do that itself would
--- be a mod that gets it wrong sixty thousand blocks out.
---
--- Ids are opaque integers. Hold one as long as you like: an id whose entity
--- has gone stops resolving, it does not start pointing at whatever took its
--- place.

---Puts an entity in the world.
---
---Returns its id, or nil if there is no world yet — during worldgen, say.
---
---`collider` is optional, and leaving it out is meaningful: an entity with no
---box is a marker. It has a position and nothing else, it does not fall, and
---nothing collides with it.
---
---The mod that calls this is recorded as the entity's source, which is what
---`entities_in_radius` filters on and what tells the engine whose entity a
---leftover is when a mod is removed from a world.
---
---```lua
---local id = game.spawn_entity{
---    pos = { x = 10.5, y = 64, z = -3 },
---    model = "engine:humanoid",
---    health = 20,
---    nametag = "Something",
---    collider = { width = 1.8, height = 5.4 },  -- cells
---}
---```
---@param spec { pos: { x: number, y: number, z: number }, model?: string, health?: integer, nametag?: string, collider?: { width: number, height: number } }
---@return integer|nil id
function game.spawn_entity(spec) end

---Removes an entity. Returns whether it was there to remove.
---@param id integer
---@return boolean removed
function game.despawn_entity(id) end

---Everything the engine knows about an entity, or nil if the id is stale.
---
---A copy, not a live view. Read it, decide, and write back with `set_entity`.
---
---**Players are entities too.** Every connected player is mirrored into the
---entity store each tick with `source = "engine:player"`, so `entities_in_radius`
---finds them like anything else and `owner` is their UUID. Their bodies are moved
---by their own inputs, so writing to one with `set_entity` is overwritten on the
---next tick — read them, do not drive them.
---
---`owner` is a UUID in hex and **never a name** (charter rule 13): names are a
---per-server claim that can be rebound, UUIDs are identity. Store the UUID if you
---mean "this player"; `game.storage` takes strings for exactly that.
---
---`nametag` is a literal label somebody set. `nametag_player` is a UUID whose
---CURRENT display name the engine resolves when it draws the tag — a player's
---own body has the second, never the first.
---@param id integer
---@return { pos: { x: number, y: number, z: number }, yaw: number, pitch: number, velocity: { x: number, y: number, z: number }, on_ground: boolean, source: string, model: string|nil, anim: integer, health: integer|nil, max_health: integer|nil, owner: string|nil, nametag: string|nil, nametag_player: string|nil }|nil
function game.entity(id) end

---Changes an entity. Returns whether anything changed.
---
---Every field is optional and anything you leave out is left alone. You cannot
---create or destroy an entity here, change its size, or change its source:
---those are decided when it spawns.
---
---**`drive` is how you move something, and `pos` is not.** `drive` is what the
---entity is TRYING to do, and the engine's physics does the rest — it walks,
---collides, steps up a lip and swims exactly as a player does. Setting `pos`
---teleports, without sweeping, so it will happily put a mob inside a wall and
---the engine will leave it there.
---
---```lua
---game.set_entity(id, {
---    drive = { walk = { x = 1, z = 0 }, gait = "walk" },
---    yaw = 1.57,
---    anim = 1,  -- WALK
---})
---```
---@param id integer
---@param spec { pos?: { x: number, y: number, z: number }, velocity?: { x: number, y: number, z: number }, yaw?: number, pitch?: number, health?: integer, anim?: integer, drive?: { walk?: { x: number, z: number }, jump?: boolean, gait?: "walk"|"sprint"|"sneak" } }
---@return boolean changed
function game.set_entity(id, spec) end

---Every entity within `radius` blocks of a position, nearest first.
---
---`source` filters by which mod spawned them. That is the only label the
---engine has an opinion about — it has no idea what a "hostile" is — so a
---finer filter belongs in your own state.
---
---```lua
---for _, id in ipairs(game.entities_in_radius({ x = 0, y = 64, z = 0 }, 32)) do
---    ...
---end
---```
---@param position { x: number, y: number, z: number }
---@param radius number Blocks.
---@param source? string
---@return integer[]
function game.entities_in_radius(position, radius, source) end

--- MOD STORAGE -------------------------------------------------------------

---Your mod's own persistent key/value store, saved with the world.
---
---Somewhere to keep a fact that is not attached to a block, a chunk or an
---entity — which world this is, whether something has happened yet, who a
---thing belongs to. Without it, such a fact has to be smuggled into a block
---somewhere, where a player can dig it up.
---
---**It is yours alone.** There is nowhere in this API to name another mod's
---storage, so two mods may both use the key `"seen"` and mean different things.
---
---Values may be a string, a number or a boolean. Not a table: that would need
---a serialisation format baked into the mod API for ever, along with the
---engine's opinion on cycles and functions. Encode your own structure into a
---string and you can change it whenever you like.
---
---**If you are storing "which player", store the UUID and never the name.**
---Names are a per-server claim and can be rebound to someone else; the UUID is
---the identity, and every engine system keys on it.
---
---```lua
---game.storage.set("imprint", uuid)     -- a UUID string, not a display name
---game.storage.set("greeted", true)
---local who = game.storage.get("imprint")
---for _, key in ipairs(game.storage.keys()) do ... end
---```
---@field storage { get: fun(key: string): string|number|boolean|nil, set: fun(key: string, value: string|number|boolean|nil), keys: fun(): string[] }

---Runs once per tick for every entity your mod spawned.
---
---**This is where a mob's behaviour lives.** The engine moves bodies and this
---decides where they are trying to go — set `drive` and the physics that runs
---immediately afterwards acts on it, in the same tick.
---
---Called with the entity's id, so use `game.entity(id)` to read it and
---`game.set_entity(id, ...)` to change it. You only ever see your own
---entities; the engine does the grouping.
---
---**Each entity gets its own instruction budget**, not a share of one. Two
---hundred mobs are two hundred budgets, and one runaway mob cannot starve the
---rest. An error disables your whole mod, as every hook does — and it stops at
---the first failure rather than reporting the same error two hundred times.
---
---```lua
---game.register_on_entity_step(function(id, dt)
---    local me = game.entity(id)
---    if not me then return end
---    game.set_entity(id, { drive = { walk = { x = 1, z = 0 }, gait = "walk" } })
---end)
---```
---@param callback fun(id: integer, dt: integer)
function game.register_on_entity_step(callback) end

return game
