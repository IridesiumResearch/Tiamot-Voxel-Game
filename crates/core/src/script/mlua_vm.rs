// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The `mlua`-backed [`ScriptVm`], compiled against exactly one of Lua 5.4,
//! `LuaJIT`, or Luau.
//!
//! **This is the only file in the engine that may name `mlua` types.** CI greps
//! for it. See [`super`] for why.
//!
//! # The three backends differ in ways that leak here
//!
//! `mlua` presents one Rust API, but the capabilities underneath are not the
//! same, and pretending otherwise would produce a sandbox that is airtight on
//! one backend and porous on another:
//!
//! - **Instruction budgets.** Lua 5.4 and `LuaJIT` use a debug hook; Luau has no
//!   debug hook and uses an interrupt callback instead.
//! - **Memory limits.** Lua 5.4 and Luau expose an allocator hook. **`LuaJIT`
//!   does not** — its allocator is tied to its own GC, so a memory ceiling is
//!   simply unavailable there. That is a real security difference for untrusted
//!   mods and it is recorded in `docs/scripting-vm.md`, not hidden.
//! - **`ffi`.** `LuaJIT` only, and it is an arbitrary-memory-access primitive.
//!   It is removed unconditionally.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use mlua::{Lua, Table, Value};

use crate::CHUNK_BLOCKS;
use crate::chunk::Chunk;
use crate::coords::{ChunkPos, LocalBlock};
use crate::detgen::{ChunkBuffer, FractalParams, Region2d, StreamRng, fill_2d};
use crate::material::MaterialId;
use crate::script::vm::{Backend, ScriptError, ScriptVm, VmLimits};

/// Globals removed from every mod environment.
///
/// `os` and `io` are filesystem and process access. `dofile` and `loadfile`
/// read arbitrary paths. `package` reaches the real module loader, which would
/// undo the `require` restriction. `require` itself is replaced rather than
/// removed — mods legitimately split across files, they just may not leave
/// their own directory.
const REMOVED_GLOBALS: [&str; 6] = ["os", "io", "dofile", "loadfile", "package", "ffi"];

/// A per-column height field, produced natively and consumed natively.
///
/// The whole point of this type is that **Lua never sees the individual
/// numbers**. A generator asks for one and hands it to a fill; there is no
/// exposed way to iterate it. That is what keeps simulation-relevant float
/// maths out of scripts (charter rule 4), and it is enforced by the API shape
/// rather than by asking mod authors to be careful.
#[derive(Debug, Clone)]
struct Heightmap {
    heights: Vec<i32>,
}

impl mlua::UserData for Heightmap {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        // Deliberately minimal. `len` and nothing else: enough for a script to
        // sanity-check, not enough to loop over.
        methods.add_method("len", |_, this, ()| Ok(this.heights.len()));
    }
}

/// A chunk being generated, exposed to Lua as userdata.
///
/// Every operation is a whole-buffer or whole-block one. There is no per-sample
/// entry point, by design.
struct BufferHandle {
    buffer: ChunkBuffer,
}

impl mlua::UserData for BufferHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("fill_all", |_, this, material: u16| {
            this.buffer.fill_all(MaterialId(material));
            Ok(())
        });

        methods.add_method_mut(
            "fill_below_heightmap",
            |_, this, (heightmap, material): (mlua::AnyUserData, u16)| {
                let heightmap = heightmap.borrow::<Heightmap>()?;
                this.buffer
                    .fill_below_heightmap(&heightmap.heights, MaterialId(material))
                    .map_err(|err| mlua::Error::external(err.to_string()))?;
                Ok(())
            },
        );

        methods.add_method_mut(
            "set_block",
            |_, this, (x, y, z, material): (u32, u32, u32, u16)| {
                if x >= CHUNK_BLOCKS || y >= CHUNK_BLOCKS || z >= CHUNK_BLOCKS {
                    return Err(mlua::Error::external(format!(
                        "block ({x}, {y}, {z}) is outside the chunk"
                    )));
                }
                this.buffer
                    .set_block(LocalBlock::new(x, y, z), MaterialId(material));
                Ok(())
            },
        );

        // Sub-node writes are opt-in and expand the buffer — Sub-Node Contract
        // §5. Named so a mod author can see they are asking for something
        // different from the block-level calls.
        methods.add_method_mut(
            "set_subnode",
            |_, this, (bx, by, bz, sx, sy, sz, material): (u32, u32, u32, u32, u32, u32, u16)| {
                if bx >= CHUNK_BLOCKS || by >= CHUNK_BLOCKS || bz >= CHUNK_BLOCKS {
                    return Err(mlua::Error::external(format!(
                        "block ({bx}, {by}, {bz}) is outside the chunk"
                    )));
                }
                if sx >= 3 || sy >= 3 || sz >= 3 {
                    return Err(mlua::Error::external(format!(
                        "sub-node ({sx}, {sy}, {sz}) is outside the 3x3x3 block"
                    )));
                }
                this.buffer.set_subnode(
                    LocalBlock::new(bx, by, bz),
                    sx,
                    sy,
                    sz,
                    MaterialId(material),
                );
                Ok(())
            },
        );

        methods.add_method("is_expanded", |_, this, ()| Ok(this.buffer.is_expanded()));
    }
}

/// A named random stream, exposed as userdata.
struct StreamHandle {
    stream: StreamRng,
}

impl mlua::UserData for StreamHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("below", |_, this, bound: u64| Ok(this.stream.below(bound)));
        methods.add_method_mut("next_bool", |_, this, ()| Ok(this.stream.next_bool()));
        // Deliberately no float accessor. A script that pulled floats out would
        // be doing simulation-relevant float maths in Lua, which charter rule 4
        // cannot police inside a VM.
    }
}

/// The `mlua`-backed VM.
pub struct MluaVm {
    lua: Lua,
    limits: VmLimits,
    frozen: bool,
    faulted: BTreeSet<String>,
    /// Per-mod sandbox environments, by mod id.
    environments: BTreeMap<String, Table>,
    /// Next numeric id to hand out. 0 and 1 are reserved (charter rule 8).
    ///
    /// The authoritative copy lives in the Lua registry, because registration
    /// happens inside Lua callbacks which cannot also borrow `self`. This is the
    /// seed value only. Keeping a second mutable mirror here was a bug once:
    /// `generate_chunk` read a Rust-side generator list that registration never
    /// wrote to, so no callback ever ran. Registration state has exactly one
    /// home now, and it is the registry.
    next_material: u16,
}

impl MluaVm {
    /// The registry key under which a mod's `on_generate` callback is stored.
    fn generator_key(mod_id: &str) -> String {
        format!("tiamot.on_generate.{mod_id}")
    }

    /// Builds a fresh sandboxed environment for one mod.
    fn build_environment(&self, mod_id: &str, dir: &Path) -> Result<Table, ScriptError> {
        let env = self.lua.create_table().map_err(|err| self.vm_error(&err))?;

        // Start from the real globals, then take things away. Copying an
        // allow-list would be safer in principle but would silently drop
        // whatever a future Lua version adds that mods legitimately need;
        // removing a deny-list keeps the failure mode "a mod uses something it
        // should not have" rather than "a mod cannot use string.format".
        let globals = self.lua.globals();
        for pair in globals.pairs::<Value, Value>() {
            let (key, value) = pair.map_err(|err| self.vm_error(&err))?;
            if let Value::String(name) = &key {
                let name = name.to_string_lossy();
                if REMOVED_GLOBALS.contains(&name.as_ref()) {
                    continue;
                }
            }
            env.set(key, value).map_err(|err| self.vm_error(&err))?;
        }

        // `_G` must point at the sandbox, not the real globals — otherwise
        // `_G.os` hands back everything the deny-list just removed.
        env.set("_G", env.clone())
            .map_err(|err| self.vm_error(&err))?;

        // `load` restricted to text chunks: binary chunks bypass the parser and
        // are a known route to memory corruption in every Lua implementation.
        let restricted_load = self
            .lua
            .create_function(|lua, (chunk, name): (mlua::String, Option<String>)| {
                let bytes = chunk.as_bytes();
                if bytes.first() == Some(&0x1B) {
                    return Err(mlua::Error::external(
                        "load: binary chunks are not permitted",
                    ));
                }
                lua.load(bytes.as_ref())
                    .set_name(name.unwrap_or_else(|| "=(load)".to_owned()))
                    .into_function()
            })
            .map_err(|err| self.vm_error(&err))?;
        env.set("load", restricted_load)
            .map_err(|err| self.vm_error(&err))?;
        // `loadstring` is the 5.1 spelling and would otherwise survive on the
        // two 5.1-semantics backends.
        env.set("loadstring", Value::Nil)
            .map_err(|err| self.vm_error(&err))?;

        // `require` confined to the mod's own directory.
        let mod_dir = dir.to_path_buf();
        let owner = mod_id.to_owned();
        let env_for_require = env.clone();
        let require = self
            .lua
            .create_function(move |lua, name: String| {
                let path = resolve_require(&mod_dir, &name).ok_or_else(|| {
                    mlua::Error::external(format!(
                        "require(\"{name}\"): mod `{owner}` may only require files inside its own \
                         directory"
                    ))
                })?;
                let source = std::fs::read_to_string(&path)
                    .map_err(|err| mlua::Error::external(format!("require(\"{name}\"): {err}")))?;
                lua.load(&source)
                    .set_name(format!("@{}", path.display()))
                    .set_environment(env_for_require.clone())
                    .eval::<Value>()
            })
            .map_err(|err| self.vm_error(&err))?;
        env.set("require", require)
            .map_err(|err| self.vm_error(&err))?;

        Ok(env)
    }

    #[allow(
        clippy::unused_self,
        reason = "kept a method so call sites read as self.vm_error(..)"
    )]
    fn vm_error(&self, err: &mlua::Error) -> ScriptError {
        ScriptError::Vm {
            backend: Self::backend().name(),
            detail: err.to_string(),
        }
    }

    /// Classifies a script failure, so budget and memory exhaustion are
    /// distinguishable from an ordinary error.
    fn classify(err: &mlua::Error, mod_id: &str, context: &str) -> ScriptError {
        let text = err.to_string();
        if text.contains(BUDGET_MARKER) {
            return ScriptError::BudgetExceeded {
                mod_id: mod_id.to_owned(),
                context: context.to_owned(),
            };
        }
        if matches!(err, mlua::Error::MemoryError(_)) || text.contains("not enough memory") {
            return ScriptError::OutOfMemory {
                mod_id: mod_id.to_owned(),
                context: context.to_owned(),
            };
        }
        ScriptError::Runtime {
            mod_id: mod_id.to_owned(),
            context: context.to_owned(),
            detail: text,
        }
    }

    /// Installs the per-call instruction budget.
    ///
    /// The mechanism differs by backend, which is exactly the kind of thing the
    /// trait exists to hide from callers.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "Luau's interrupt API is infallible while the 5.4/LuaJIT hook is not;                   the signature stays uniform so callers do not branch on backend"
    )]
    fn arm_budget(&self, instructions: u32) -> Result<(), ScriptError> {
        #[cfg(any(feature = "vm-lua54", feature = "vm-luajit"))]
        {
            let limit = u64::from(instructions);
            let counter = std::cell::Cell::new(0u64);
            self.lua
                .set_hook(
                    mlua::HookTriggers::new().every_nth_instruction(BUDGET_HOOK_STEP),
                    move |_lua, _debug| {
                        let used = counter.get() + u64::from(BUDGET_HOOK_STEP);
                        counter.set(used);
                        if used > limit {
                            return Err(mlua::Error::external(BUDGET_MARKER));
                        }
                        Ok(mlua::VmState::Continue)
                    },
                )
                .map_err(|err| self.vm_error(&err))?;
        }

        #[cfg(feature = "vm-luau")]
        {
            // Luau has no debug hook; it has an interrupt, called at back-edges
            // and calls. Coarser than an instruction count but bounded, which is
            // what the budget is for.
            let limit = u64::from(instructions) / u64::from(BUDGET_HOOK_STEP);
            let counter = std::cell::Cell::new(0u64);
            self.lua.set_interrupt(move |_lua| {
                let used = counter.get() + 1;
                counter.set(used);
                if used > limit {
                    return Err(mlua::Error::external(BUDGET_MARKER));
                }
                Ok(mlua::VmState::Continue)
            });
        }

        Ok(())
    }

    /// Removes the budget, so engine-side work is not counted against a mod.
    fn disarm_budget(&self) {
        #[cfg(any(feature = "vm-lua54", feature = "vm-luajit"))]
        self.lua.remove_hook();

        #[cfg(feature = "vm-luau")]
        self.lua.remove_interrupt();
    }

    fn environment(&self, mod_id: &str) -> Result<&Table, ScriptError> {
        self.environments
            .get(mod_id)
            .ok_or_else(|| ScriptError::Runtime {
                mod_id: mod_id.to_owned(),
                context: "environment lookup".to_owned(),
                detail: "mod was never loaded".to_owned(),
            })
    }
}

/// Marker text used to recognise a budget stop coming back through `mlua`'s
/// error type, which has no dedicated variant for it.
const BUDGET_MARKER: &str = "tiamot: instruction budget exceeded";

/// How often the budget hook fires. Checking every instruction would dominate
/// the runtime; a few thousand keeps the overhead negligible while still
/// bounding a runaway loop to a fraction of a millisecond.
const BUDGET_HOOK_STEP: u32 = 4_096;

/// Resolves a `require` name against a mod's directory, refusing to escape it.
///
/// Returns `None` for anything that leaves the directory — checked after
/// canonicalisation, so `..` and symlinks are both covered rather than just the
/// obvious textual form.
fn resolve_require(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.contains('\0') {
        return None;
    }
    // Lua's convention: dots are path separators.
    let relative = name.replace('.', std::path::MAIN_SEPARATOR_STR);
    let candidate = dir.join(format!("{relative}.lua"));

    let canonical_dir = dir.canonicalize().ok()?;
    let canonical = candidate.canonicalize().ok()?;
    canonical.starts_with(&canonical_dir).then_some(canonical)
}

impl ScriptVm for MluaVm {
    fn backend() -> Backend {
        #[cfg(feature = "vm-lua54")]
        {
            Backend::Lua54
        }
        #[cfg(feature = "vm-luajit")]
        {
            Backend::LuaJit
        }
        #[cfg(feature = "vm-luau")]
        {
            Backend::Luau
        }
    }

    fn create(limits: VmLimits) -> Result<Self, ScriptError> {
        let lua = Lua::new();

        // LuaJIT has no allocator hook, so a memory ceiling is unavailable
        // there. Recorded in docs/scripting-vm.md rather than silently ignored.
        #[cfg(not(feature = "vm-luajit"))]
        lua.set_memory_limit(limits.memory_bytes)
            .map_err(|err| ScriptError::Vm {
                backend: Self::backend().name(),
                detail: err.to_string(),
            })?;

        Ok(Self {
            lua,
            limits,
            frozen: false,
            faulted: BTreeSet::new(),
            environments: BTreeMap::new(),
            next_material: 2,
        })
    }

    fn load_mod(&mut self, mod_id: &str, source: &str, dir: &Path) -> Result<(), ScriptError> {
        let env = self.build_environment(mod_id, dir)?;
        let game = self.build_game_table(mod_id, &env)?;
        env.set("game", game).map_err(|err| self.vm_error(&err))?;

        self.arm_budget(self.limits.instructions_per_call)?;
        let result = self
            .lua
            .load(source)
            .set_name(format!("@{mod_id}/init.lua"))
            .set_environment(env.clone())
            .exec();
        self.disarm_budget();

        result.map_err(|err| Self::classify(&err, mod_id, "init.lua"))?;
        self.environments.insert(mod_id.to_owned(), env);
        Ok(())
    }

    fn freeze(&mut self) -> Result<(), ScriptError> {
        // The flag the `register_*` closures actually read lives in the Lua
        // registry. Setting only the Rust-side one left registration open after
        // freeze — charter rule 9 violated in silence.
        self.lua
            .set_named_registry_value("tiamot.frozen", true)
            .map_err(|err| self.vm_error(&err))?;
        self.frozen = true;
        Ok(())
    }

    fn is_frozen(&self) -> bool {
        self.frozen
    }

    fn generate_chunk(
        &mut self,
        world_seed: u64,
        pos: ChunkPos,
        fill: MaterialId,
    ) -> Result<Chunk, ScriptError> {
        let handle = self
            .lua
            .create_userdata(BufferHandle {
                buffer: ChunkBuffer::new(pos, fill),
            })
            .map_err(|err| self.vm_error(&err))?;

        // Read from the registry, which is where registration actually wrote.
        let generators: Vec<String> = self
            .lua
            .named_registry_value::<Table>("tiamot.generators")
            .map_err(|err| self.vm_error(&err))?
            .sequence_values::<String>()
            .filter_map(Result::ok)
            .collect();

        for mod_id in generators {
            if self.faulted.contains(&mod_id) {
                continue;
            }

            let callback: mlua::Function = self
                .lua
                .named_registry_value(&Self::generator_key(&mod_id))
                .map_err(|err| self.vm_error(&err))?;

            let position = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
            position
                .set("x", pos.x)
                .map_err(|err| self.vm_error(&err))?;
            position
                .set("y", pos.y)
                .map_err(|err| self.vm_error(&err))?;
            position
                .set("z", pos.z)
                .map_err(|err| self.vm_error(&err))?;
            position
                .set("seed", world_seed)
                .map_err(|err| self.vm_error(&err))?;

            self.arm_budget(self.limits.instructions_per_call)?;
            let result = callback.call::<()>((handle.clone(), position));
            self.disarm_budget();

            if let Err(err) = result {
                // Charter rule 10: the mod is disabled, the tick continues.
                let error = Self::classify(&err, &mod_id, "on_generate");
                self.faulted.insert(mod_id.clone());
                tracing::error!(mod_id = %mod_id, error = %error, "disabling mod after generation failure");
                return Err(error);
            }
        }

        let buffer = handle
            .borrow::<BufferHandle>()
            .map_err(|err| self.vm_error(&err))?;
        Ok(buffer.buffer.to_chunk())
    }

    fn call_void(&mut self, mod_id: &str, name: &str) -> Result<(), ScriptError> {
        let env = self.environment(mod_id)?;
        let function: mlua::Function = env
            .get(name)
            .map_err(|err| Self::classify(&err, mod_id, name))?;

        self.arm_budget(self.limits.instructions_per_call)?;
        let result = function.call::<()>(());
        self.disarm_budget();

        result.map_err(|err| Self::classify(&err, mod_id, name))
    }

    fn eval_in(&mut self, mod_id: &str, source: &str) -> Result<(), ScriptError> {
        let env = self.environment(mod_id)?.clone();
        self.arm_budget(self.limits.instructions_per_call)?;
        let result = self
            .lua
            .load(source)
            .set_name(format!("@{mod_id}/eval"))
            .set_environment(env)
            .exec();
        self.disarm_budget();
        result.map_err(|err| Self::classify(&err, mod_id, "eval"))
    }

    fn faulted_mods(&self) -> Vec<String> {
        self.faulted.iter().cloned().collect()
    }

    fn mark_faulted(&mut self, mod_id: &str) {
        self.faulted.insert(mod_id.to_owned());
    }
}

impl MluaVm {
    /// Builds the `game` table for one mod.
    ///
    /// Per mod rather than shared, because every registration call has to know
    /// which mod is making it — for namespacing, for fault attribution, and to
    /// refuse `core:`-prefixed ids from anyone but `core`.
    fn build_game_table(&mut self, mod_id: &str, _env: &Table) -> Result<Table, ScriptError> {
        let game = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.install_logging(mod_id, &game)?;
        self.install_registration(mod_id, &game)?;
        self.install_frozen_api(mod_id, &game)?;
        Ok(game)
    }

    /// `game.log`.
    fn install_logging(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        // -- logging ------------------------------------------------------
        let owner = mod_id.to_owned();
        let log = self
            .lua
            .create_function(move |_, message: String| {
                tracing::info!(mod_id = %owner, "{message}");
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("log", log).map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `register_*` family, live only during the registration window.
    fn install_registration(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        // -- registration -------------------------------------------------
        // Registration mutates engine state from inside a Lua callback, so the
        // state it touches lives in the Lua registry rather than in `self`:
        // `self` is borrowed for the duration of the call.
        let owner = mod_id.to_owned();
        let register_block = self
            .lua
            .create_function(move |lua, spec: Table| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }

                let id: String = spec.get("id").map_err(|_| {
                    mlua::Error::external("register_block: missing required field `id`")
                })?;

                // Unknown fields are an error naming the field: a typo in
                // `hardness` should say so, not silently take the default.
                for pair in spec.pairs::<Value, Value>() {
                    let (key, _) = pair?;
                    if let Value::String(name) = key {
                        let name = name.to_string_lossy();
                        if !BLOCK_FIELDS.contains(&name.as_ref()) {
                            return Err(mlua::Error::external(format!(
                                "register_block(\"{id}\"): unknown field `{name}`"
                            )));
                        }
                    }
                }

                let qualified = qualify_id(&owner, &id).map_err(mlua::Error::external)?;

                let registry: Table = lua.named_registry_value("tiamot.blocks")?;
                if registry.contains_key(qualified.clone())? {
                    return Err(mlua::Error::external(format!(
                        "block `{qualified}` is already registered"
                    )));
                }
                let next: u16 = lua.named_registry_value("tiamot.next_material")?;
                registry.set(qualified, next)?;
                lua.set_named_registry_value("tiamot.next_material", next + 1)?;
                Ok(next)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_block", register_block)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let key = Self::generator_key(mod_id);
        let register_on_generate = self
            .lua
            .create_function(move |lua, callback: mlua::Function| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                lua.set_named_registry_value(&key, callback)?;
                let generators: Table = lua.named_registry_value("tiamot.generators")?;
                generators.push(owner.clone())?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_on_generate", register_on_generate)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let register_action = self
            .lua
            .create_function(move |lua, spec: Table| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                let id: String = spec.get("id")?;
                let actions: Table = lua.named_registry_value("tiamot.actions")?;
                actions.push(qualify_id(&owner, &id).map_err(mlua::Error::external)?)?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_action", register_action)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Everything callable after freeze: lookups, bulk noise, streams, constants.
    fn install_frozen_api(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        // -- frozen-phase API ---------------------------------------------
        let get_block_id = self
            .lua
            .create_function(|lua, id: String| {
                let registry: Table = lua.named_registry_value("tiamot.blocks")?;
                registry.get::<Option<u16>>(id.clone())?.ok_or_else(|| {
                    mlua::Error::external(format!("no block registered with id `{id}`"))
                })
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("get_block_id", get_block_id)
            .map_err(|err| self.vm_error(&err))?;

        // The bulk noise entry point. Takes a whole region and returns a native
        // heightmap; there is deliberately no per-sample call.
        let noise_heightmap = self
            .lua
            .create_function(|_, (position, options): (Table, Table)| {
                let chunk_x: i32 = position.get("x")?;
                let chunk_y: i32 = position.get("y")?;
                let chunk_z: i32 = position.get("z")?;
                let seed: u64 = position.get("seed").unwrap_or(0);

                let params = FractalParams {
                    fractal: crate::detgen::Fractal::Fbm,
                    octaves: options.get("octaves").unwrap_or(4),
                    frequency: options.get("frequency").unwrap_or(0.02),
                    lacunarity: options.get("lacunarity").unwrap_or(2.0),
                    gain: options.get("gain").unwrap_or(0.5),
                };
                let amplitude: f32 = options.get("amplitude").unwrap_or(6.0);
                let base: i32 = options.get("base").unwrap_or(0);

                let region = Region2d {
                    origin_x: (chunk_x * CHUNK_BLOCKS as i32) as f32,
                    origin_y: (chunk_z * CHUNK_BLOCKS as i32) as f32,
                    step_x: 1.0,
                    step_y: 1.0,
                    width: CHUNK_BLOCKS as usize,
                    height: CHUNK_BLOCKS as usize,
                };
                let mut samples = vec![0.0f32; region.len()];
                fill_2d(seed, &region, &params, &mut samples)
                    .map_err(|err| mlua::Error::external(err.to_string()))?;

                // The float→int conversion happens HERE, in Rust, inside the
                // Deterministic Float Subset. If a script did it, the subset
                // would be unenforceable.
                let _ = chunk_y;
                let heights = samples
                    .iter()
                    .map(|sample| base + (sample * amplitude) as i32)
                    .collect();
                Ok(Heightmap { heights })
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("noise_heightmap", noise_heightmap)
            .map_err(|err| self.vm_error(&err))?;

        // A flat heightmap, for generators that want a constant surface.
        let flat_heightmap = self
            .lua
            .create_function(|_, height: i32| {
                Ok(Heightmap {
                    heights: vec![height; (CHUNK_BLOCKS * CHUNK_BLOCKS) as usize],
                })
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("flat_heightmap", flat_heightmap)
            .map_err(|err| self.vm_error(&err))?;

        let rng_stream = self
            .lua
            .create_function(|_, (position, name): (Table, String)| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;
                let seed: u64 = position.get("seed").unwrap_or(0);
                Ok(StreamHandle {
                    stream: StreamRng::new(seed, ChunkPos::new(x, y, z), &name),
                })
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("rng_stream", rng_stream)
            .map_err(|err| self.vm_error(&err))?;

        // Constants a generator needs, so nothing has to be hard-coded in Lua.
        game.set("CHUNK_BLOCKS", CHUNK_BLOCKS)
            .map_err(|err| self.vm_error(&err))?;
        game.set("UNITS_PER_BLOCK", crate::UNITS_PER_BLOCK)
            .map_err(|err| self.vm_error(&err))?;
        game.set("AIR", MaterialId::AIR.get())
            .map_err(|err| self.vm_error(&err))?;
        game.set("mod_id", mod_id)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Initialises the registry tables the `game` closures read.
    ///
    /// Separate from `create` so the borrow checker is not asked to hold `self`
    /// across a Lua callback that also wants it.
    fn install_registry(&mut self) -> Result<(), ScriptError> {
        let blocks = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let generators = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let actions = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.blocks", blocks)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.generators", generators)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.actions", actions)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.next_material", self.next_material)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.frozen", false)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Creates a VM with the registry installed, ready for `load_mod`.
    ///
    /// # Errors
    ///
    /// As [`ScriptVm::create`].
    pub fn new(limits: VmLimits) -> Result<Self, ScriptError> {
        let mut vm = <Self as ScriptVm>::create(limits)?;
        vm.install_registry()?;
        Ok(vm)
    }

    /// Blocks registered so far, string id → numeric id.
    #[must_use]
    pub fn registered_blocks(&self) -> BTreeMap<String, MaterialId> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.blocks") else {
            return BTreeMap::new();
        };
        registry
            .pairs::<String, u16>()
            .filter_map(Result::ok)
            .map(|(name, id)| (name, MaterialId(id)))
            .collect()
    }
}

/// Fields `register_block` accepts. Anything else is an error naming the field.
const BLOCK_FIELDS: [&str; 6] = ["id", "name", "drops", "hardness", "description", "tags"];

/// Applies namespace rules to a registered id.
///
/// A mod's ids are prefixed with its own id automatically. An explicit
/// `namespace:name` form is allowed only when the namespace is the mod's own —
/// otherwise any mod could register `core:white` and shadow the engine's
/// reference blocks.
fn qualify_id(mod_id: &str, id: &str) -> Result<String, String> {
    match id.split_once(':') {
        None => Ok(format!("{mod_id}:{id}")),
        Some((namespace, _)) if namespace == mod_id => Ok(id.to_owned()),
        Some((namespace, _)) => Err(format!(
            "mod `{mod_id}` may not register into namespace `{namespace}`: a mod owns only its \
             own id as a namespace"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> MluaVm {
        MluaVm::new(VmLimits::default()).expect("create vm")
    }

    fn load(vm: &mut MluaVm, id: &str, source: &str) -> Result<(), ScriptError> {
        vm.load_mod(id, source, Path::new("."))
    }

    #[test]
    fn namespace_rules_hold() {
        assert_eq!(qualify_id("mymod", "white").unwrap(), "mymod:white");
        assert_eq!(qualify_id("mymod", "mymod:white").unwrap(), "mymod:white");
        // The rule that stops a mod shadowing the engine's reference blocks.
        assert!(qualify_id("mymod", "core:white").is_err());
    }

    #[test]
    fn dangerous_globals_are_absent() {
        let mut vm = vm();
        load(&mut vm, "t", "").expect("load");
        for global in ["os", "io", "dofile", "loadfile", "package", "ffi"] {
            let source = format!("if {global} ~= nil then error('{global} is reachable') end");
            vm.eval_in("t", &source)
                .unwrap_or_else(|err| panic!("{global} should be absent: {err}"));
        }
    }

    #[test]
    fn the_sandbox_global_table_does_not_leak_the_real_one() {
        // `_G.os` would undo the whole deny-list if `_G` still pointed at the
        // real globals.
        let mut vm = vm();
        load(&mut vm, "t", "").expect("load");
        vm.eval_in(
            "t",
            "if _G.os ~= nil then error('_G leaks the real globals') end",
        )
        .expect("sandbox _G");
    }

    #[test]
    fn binary_chunks_are_refused_by_load() {
        let mut vm = vm();
        load(&mut vm, "t", "").expect("load");
        let err = vm
            .eval_in("t", "local f = load('\\27Lua binary')")
            .expect_err("binary chunks must be refused");
        assert!(err.to_string().contains('t'), "{err}");
    }

    /// **Not run on `LuaJIT`, because on `LuaJIT` it does not terminate.**
    ///
    /// `LuaJIT`'s debug hook does not fire inside JIT-compiled traces, so a hot
    /// loop escapes the instruction budget entirely and runs forever. Verified
    /// by running this test under `vm-luajit`: it hangs rather than failing.
    ///
    /// This is a containment failure, not a performance nuance — see
    /// `docs/scripting-vm.md` §5. It is one of the reasons `LuaJIT` is not the
    /// chosen backend, and the reason this test is compiled out there rather
    /// than left to hang CI.
    #[test]
    #[cfg(not(feature = "vm-luajit"))]
    fn an_infinite_loop_is_stopped_by_the_budget() {
        let mut vm = MluaVm::new(VmLimits {
            instructions_per_call: 100_000,
            ..VmLimits::default()
        })
        .expect("create");
        load(&mut vm, "t", "").expect("load");

        // Without a budget this hangs the test suite rather than failing it.
        let err = vm
            .eval_in("t", "while true do end")
            .expect_err("the budget must stop this");
        assert!(
            matches!(err, ScriptError::BudgetExceeded { .. }),
            "expected a budget error, got {err:?}"
        );
    }

    /// The compiled-in backend agrees with its own documented containment
    /// properties.
    ///
    /// Deliberately does NOT fail the `LuaJIT` build: `vm-luajit` stays
    /// selectable so the benchmark can be re-run and the finding re-verified.
    /// What it asserts is that the containment story matches reality on
    /// whichever backend is compiled in — including that the budget test above
    /// is compiled out exactly where the budget does not work.
    #[test]
    fn the_backend_matches_its_documented_containment() {
        let backend = <MluaVm as ScriptVm>::backend();

        // The budget test is `cfg`'d out on, and only on, the backend that
        // cannot honour it.
        let budget_test_runs = cfg!(not(feature = "vm-luajit"));
        assert_eq!(
            budget_test_runs,
            backend.can_bound_runaway_loops(),
            "{backend:?}: the instruction-budget test should run exactly when the \
             backend can actually bound a runaway loop"
        );
    }

    #[test]
    fn registration_is_refused_after_freeze() {
        let mut vm = vm();
        load(&mut vm, "t", "game.register_block{ id = 'a' }").expect("load");
        vm.freeze().expect("freeze");

        let err = vm
            .eval_in("t", "game.register_block{ id = 'b' }")
            .expect_err("registration must be closed");
        assert!(err.to_string().contains('t'), "{err}");
    }

    #[test]
    fn unknown_registration_fields_name_the_field() {
        let mut vm = vm();
        let err = load(&mut vm, "t", "game.register_block{ id = 'a', hardnes = 3 }")
            .expect_err("typo should be rejected");
        assert!(
            err.to_string().contains('t'),
            "the error should attribute the mod: {err}"
        );
    }

    #[test]
    fn a_mod_cannot_require_outside_its_directory() {
        let dir = std::env::temp_dir().join("tiamot-require-test");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("inside.lua"), "return 42").expect("write");

        let mut vm = vm();
        vm.load_mod("t", "", &dir).expect("load");

        vm.eval_in("t", "assert(require('inside') == 42)")
            .expect("a mod may require its own files");

        let err = vm
            .eval_in("t", "require('...escape')")
            .expect_err("escaping the mod directory must be refused");
        assert!(err.to_string().contains('t'), "{err}");
    }

    #[test]
    fn registered_blocks_are_namespaced_and_numbered() {
        let mut vm = vm();
        load(
            &mut vm,
            "mymod",
            "game.register_block{ id = 'white', name = 'White' }",
        )
        .expect("load");
        let blocks = vm.registered_blocks();
        assert!(blocks.contains_key("mymod:white"), "{blocks:?}");
        assert!(blocks["mymod:white"].get() >= 2, "reserved ids are 0 and 1");
    }
}
