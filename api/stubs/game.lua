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
---@field hardness number? How long it takes to break.
---@field drops any? What breaking it yields.
---@field tags string[]? Arbitrary tags for other mods to match on.

---Fields accepted by `game.register_action`.
---@class Tiamot.ActionSpec
---@field id string Required. Namespaced with your mod id automatically.
---@field default_key string? Suggested default binding. The engine owns bindings; mods never read keys.

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

return game
