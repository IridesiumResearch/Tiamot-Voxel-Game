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
use crate::script::vm::{
    Backend, BlockRules, BlockTexture, Brush, FluidRules, HookOutcome, ScriptError, ScriptVm, Sky,
    SkyGrade, SkyKeyframe, Tool, VmLimits,
};

/// Registry key holding the mods that registered `on_dig_complete`.
///
/// A list of mod ids in load order, exactly like `tiamot.tickers`; the callback
/// itself lives under [`MluaVm::hook_key`]. Two structures because the ORDER is
/// the contract and a Lua table keyed by mod id has no order at all.
const DIGGERS: &str = "tiamot.diggers";

/// Registry key holding the mods that registered `on_place`.
const PLACERS: &str = "tiamot.placers";

/// Hook name used in registry keys and in fault messages.
const HOOK_DIG: &str = "on_dig_complete";

/// Hook name used in registry keys and in fault messages.
const HOOK_PLACE: &str = "on_place";

/// Registry key holding the mods that registered `on_fluid_flow`.
const FLOWERS: &str = "tiamot.flowers";

/// Hook name used in registry keys and in fault messages.
const HOOK_FLOW: &str = "on_fluid_flow";

/// A player UUID as lowercase hex, for handing to Lua.
///
/// Hex rather than the raw 32 bytes because a mod keying per-player state needs
/// something it can use as a table key and print in the same breath, and raw
/// bytes in a Lua string are technically the former and emphatically not the
/// latter the moment anyone logs one.
fn hex_uuid(uuid: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in uuid {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Registry key holding the mods that registered `on_punch`.
const PUNCHERS: &str = "tiamot.punchers";

/// Hook name used in registry keys and in fault messages.
const HOOK_PUNCH: &str = "on_punch";

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
    /// Where `game.get_light` reads from, once the server has one.
    ///
    /// Behind a lock because the frozen API is installed before the world
    /// exists — the closure captures this and reads whatever is in it at call
    /// time, rather than needing the source to exist at freeze. Uncontended in
    /// practice: both the setter and every reader are the simulation thread.
    light: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::light::LightSource>>>>,
    /// Where `game.get_fluid` and `game.set_fluid` reach, once there is a world.
    ///
    /// Behind the same lock as `light` and for the same reason: the frozen API
    /// is installed before the world exists, so the closures capture this and
    /// read whatever is in it at call time.
    fluid: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::fluid::Access>>>>,
    /// Where `game.set_block` sends its edits, once there is a world.
    edits: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::script::WorldEdit>>>>,
    /// Where the `game.*_entity` calls reach, once there is a world.
    entities: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::ent::Access>>>>,
    /// Where `game.storage` reaches, once there is a world.
    storage: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::storage::Access>>>>,
}

/// Where a world's clock starts when the sky mod does not say.
///
/// Mid-morning: the sun is well up, so shadows have a direction and a length
/// and the world has colour in it. Midnight — which is where a counter starting
/// at zero lands — shows none of that.
const DEFAULT_START_TIME: f32 = 0.35;

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

        let mut vm = Self {
            lua,
            limits,
            frozen: false,
            faulted: BTreeSet::new(),
            environments: BTreeMap::new(),
            next_material: 2,
            light: std::sync::Arc::new(std::sync::Mutex::new(None)),
            fluid: std::sync::Arc::new(std::sync::Mutex::new(None)),
            entities: std::sync::Arc::new(std::sync::Mutex::new(None)),
            storage: std::sync::Arc::new(std::sync::Mutex::new(None)),
            edits: std::sync::Arc::new(std::sync::Mutex::new(None)),
        };
        // Installed here rather than in a second constructor the caller has to
        // remember. `ModHost` called `create` and got a VM with no registry
        // tables, so every registration failed — the exact failure a two-step
        // constructor invites.
        vm.install_registry()?;
        Ok(vm)
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

    fn set_light_source(&mut self, source: std::sync::Arc<dyn crate::light::LightSource>) {
        if let Ok(mut slot) = self.light.lock() {
            *slot = Some(source);
        }
    }

    fn set_fluid_access(&mut self, access: std::sync::Arc<dyn crate::fluid::Access>) {
        if let Ok(mut slot) = self.fluid.lock() {
            *slot = Some(access);
        }
    }

    fn set_entity_access(&mut self, access: std::sync::Arc<dyn crate::ent::Access>) {
        if let Ok(mut slot) = self.entities.lock() {
            *slot = Some(access);
        }
    }

    fn set_storage_access(&mut self, access: std::sync::Arc<dyn crate::storage::Access>) {
        if let Ok(mut slot) = self.storage.lock() {
            *slot = Some(access);
        }
    }

    fn set_world_edit(&mut self, edit: std::sync::Arc<dyn crate::script::WorldEdit>) {
        if let Ok(mut slot) = self.edits.lock() {
            *slot = Some(edit);
        }
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

    fn tick(&mut self, dt_ticks: u32) -> Result<Vec<(String, ScriptError)>, ScriptError> {
        let tickers: Vec<String> = self
            .lua
            .named_registry_value::<Table>("tiamot.tickers")
            .map_err(|err| self.vm_error(&err))?
            .sequence_values::<String>()
            .filter_map(Result::ok)
            .collect();

        let mut faults = Vec::new();
        for mod_id in tickers {
            if self.faulted.contains(&mod_id) {
                continue;
            }

            let callback: mlua::Function = self
                .lua
                .named_registry_value(&Self::tick_key(&mod_id))
                .map_err(|err| self.vm_error(&err))?;

            self.arm_budget(self.limits.instructions_per_call)?;
            let result = callback.call::<()>(dt_ticks);
            self.disarm_budget();

            if let Err(err) = result {
                // Charter rule 10: this mod is disabled, the tick continues.
                // Continue rather than return, or one bad mod would starve
                // every mod registered after it — and the symptom would be
                // "my mod stopped working" with nothing pointing at the cause.
                let error = Self::classify(&err, &mod_id, "on_tick");
                self.faulted.insert(mod_id.clone());
                tracing::error!(mod_id = %mod_id, error = %error, "disabling mod after tick failure");
                faults.push((mod_id, error));
            }
        }
        Ok(faults)
    }

    fn entity_step(
        &mut self,
        mod_id: &str,
        entities: &[u64],
        dt_ticks: u32,
    ) -> Result<Option<(String, ScriptError)>, ScriptError> {
        if entities.is_empty() || self.faulted.contains(mod_id) {
            return Ok(None);
        }
        let callback: mlua::Function = match self
            .lua
            .named_registry_value(&Self::hook_key("on_entity_step", mod_id))
        {
            Ok(callback) => callback,
            // The mod registered nothing, which is the normal case for most
            // mods and not worth an error.
            Err(_) => return Ok(None),
        };

        for id in entities {
            // **Armed per entity, not per tick.** A mod with two hundred mobs
            // must not get a two-hundredth of a budget each, and one runaway
            // mob must not starve the other hundred and ninety-nine.
            self.arm_budget(self.limits.instructions_per_call)?;
            let result = callback.call::<()>((*id as i64, dt_ticks));
            self.disarm_budget();

            if let Err(err) = result {
                // Charter rule 10: the mod is disabled and the tick continues.
                // Stopping at the first failure rather than running the rest is
                // deliberate — a mod whose callback throws will throw for every
                // entity, and reporting two hundred identical faults would bury
                // the one that matters.
                let error = Self::classify(&err, mod_id, "on_entity_step");
                self.faulted.insert(mod_id.to_owned());
                tracing::error!(
                    mod_id = %mod_id,
                    error = %error,
                    "disabling mod after an entity step failure"
                );
                return Ok(Some((mod_id.to_owned(), error)));
            }
        }
        Ok(None)
    }

    fn entity_steppers(&self) -> Vec<String> {
        self.lua
            .named_registry_value::<Table>("tiamot.entity_steppers")
            .map(|table| {
                table
                    .sequence_values::<String>()
                    .filter_map(Result::ok)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dig_complete(&mut self, event: &crate::script::DigEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("x", event.target.x)?;
            table.set("y", event.target.y)?;
            table.set("z", event.target.z)?;
            table.set("material", event.material.0)?;
            table.set(
                "brush",
                match event.brush {
                    crate::dig::Brush::Block => "block",
                    crate::dig::Brush::SubNode => "subnode",
                },
            )?;
            Ok(table)
        }) else {
            // The table could not be built, which is a VM problem rather than a
            // mod problem. Allowing is the safe answer: refusing would stop
            // every dig on the server because of an allocation failure.
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_DIG, DIGGERS, &table)
    }

    fn place(&mut self, event: &crate::script::PlaceEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("x", event.block.x)?;
            table.set("y", event.block.y)?;
            table.set("z", event.block.z)?;
            table.set("material", event.material.0)?;
            table.set("occupancy", event.occupancy)?;
            table.set("units", event.units)?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_PLACE, PLACERS, &table)
    }

    fn punch(&mut self, event: &crate::script::PunchEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.attacker).and_then(|table| {
            // `attacker` as well as `player`, because a punch has two parties
            // and a field called `player` would be ambiguous the moment anyone
            // read the other one. `player` stays for consistency with the other
            // two events, which have only one.
            table.set("attacker", table.get::<String>("player")?)?;
            table.set("target", hex_uuid(event.target))?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_PUNCH, PUNCHERS, &table)
    }

    fn fluid_flow(&mut self, event: &crate::script::FluidFlowEvent) -> HookOutcome {
        let Ok(table) = self.lua.create_table().and_then(|table| {
            // Block coordinates on both, and named so that nobody has to guess
            // which end is which. `get_light` takes blocks and a dig event's
            // x/y/z are CELLS, which has caught somebody once already — so
            // these are never bare x/y/z.
            let at = self.lua.create_table()?;
            at.set("x", event.from.x)?;
            at.set("y", event.from.y)?;
            at.set("z", event.from.z)?;
            table.set("from", at)?;

            let into = self.lua.create_table()?;
            into.set("x", event.into.x)?;
            into.set("y", event.into.y)?;
            into.set("z", event.into.z)?;
            table.set("into", into)?;

            table.set("fluid", event.fluid.as_str())?;
            table.set("level", event.level)?;
            table.set("occupancy", event.occupancy)?;
            table.set("units", event.occupancy.count_ones())?;
            // The blocking block by NAME. A runtime id would be a number that
            // means something different next run (charter rule 8), and this is
            // the one field a mod is certain to compare against.
            let name = self
                .registered_blocks()
                .into_iter()
                .find(|(_, id)| *id == event.blocked_by)
                .map(|(name, _)| name);
            table.set("block", name)?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_FLOW, FLOWERS, &table)
    }

    fn registered_blocks(&self) -> Vec<(String, MaterialId)> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.blocks") else {
            return Vec::new();
        };
        let mut blocks: Vec<(String, MaterialId)> = registry
            .pairs::<String, u16>()
            .filter_map(Result::ok)
            .map(|(name, id)| (name, MaterialId(id)))
            .collect();
        // By id, not by name. See the trait docs: the host replays these into a
        // registry that assigns sequentially, so any other order gives blocks
        // different ids than their mods were told.
        blocks.sort_by_key(|(_, id)| id.0);
        blocks
    }

    fn registered_block_textures(&self) -> Vec<BlockTexture> {
        let Ok(registry) = self
            .lua
            .named_registry_value::<Table>("tiamot.block_textures")
        else {
            return Vec::new();
        };
        let ids = self.block_ids();

        let mut textures: Vec<(MaterialId, BlockTexture)> = registry
            .pairs::<String, Table>()
            .filter_map(Result::ok)
            .filter_map(|(block, entry)| {
                // A block id with no material id cannot happen — the texture
                // is only written after the registration succeeds — but a
                // missing one here would be a silent mis-ordering, and this
                // whole list is ordered.
                let id = *ids.get(&block)?;
                Some((
                    id,
                    BlockTexture {
                        mod_id: entry.get("mod").ok()?,
                        path: entry.get("path").ok()?,
                        block,
                    },
                ))
            })
            .collect();

        // Lua table iteration order is unspecified. Sorting by material id
        // rather than leaving it to the table gives the server a stable list,
        // which matters because it goes on the wire.
        textures.sort_by_key(|(id, _)| id.0);
        textures.into_iter().map(|(_, texture)| texture).collect()
    }

    fn registered_block_rules(&self) -> Vec<BlockRules> {
        let ids = self.block_ids();
        let rules = self
            .lua
            .named_registry_value::<Table>("tiamot.block_rules")
            .ok();

        // Driven by the block list, not by the rules table: every registered
        // block gets an entry whether or not its mod said anything, so a caller
        // never has to tell "no rules" apart from "default rules".
        let mut all: Vec<(MaterialId, BlockRules)> = ids
            .iter()
            .map(|(block, id)| {
                let entry = rules
                    .as_ref()
                    .and_then(|table| table.get::<Option<Table>>(block.as_str()).ok().flatten());
                let hardness = entry
                    .as_ref()
                    .and_then(|entry| entry.get::<Option<f32>>("hardness").ok().flatten())
                    .unwrap_or(BlockRules::DEFAULT_HARDNESS);
                let dominance = entry
                    .as_ref()
                    .and_then(|entry| entry.get::<Option<f32>>("dominance").ok().flatten())
                    .unwrap_or(crate::dig::Resistance::DEFAULT_DOMINANCE);
                let drops = entry
                    .as_ref()
                    .and_then(|entry| entry.get::<Option<Table>>("drops").ok().flatten())
                    .map(|drops| {
                        drops
                            .pairs::<String, u32>()
                            .filter_map(Result::ok)
                            .collect::<Vec<_>>()
                    })
                    .map(|mut drops| {
                        // Contract §9: drop order is observable, so it must not
                        // depend on Lua's table iteration.
                        drops.sort();
                        drops
                    });
                let light_emit = entry
                    .as_ref()
                    .and_then(|entry| entry.get::<Option<Table>>("light_emit").ok().flatten())
                    .map_or((0, 0, 0), |emit| {
                        let channel = |key: &str| {
                            emit.get::<Option<u8>>(key)
                                .ok()
                                .flatten()
                                .unwrap_or(0)
                                .min(crate::light::MAX_LEVEL)
                        };
                        (channel("r"), channel("g"), channel("b"))
                    });
                (
                    *id,
                    BlockRules {
                        block: block.clone(),
                        hardness,
                        dominance,
                        drops,
                        light_emit,
                    },
                )
            })
            .collect();

        all.sort_by_key(|(id, _)| id.0);
        all.into_iter().map(|(_, rules)| rules).collect()
    }

    fn registered_fluids(&self) -> Vec<FluidRules> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.fluids") else {
            return Vec::new();
        };

        let mut fluids: Vec<FluidRules> = registry
            .pairs::<String, Table>()
            .filter_map(Result::ok)
            .filter_map(|(fluid, entry)| {
                Some(FluidRules {
                    fluid,
                    material: entry.get("material").ok()?,
                    flow_range: entry.get("flow_range").ok()?,
                    waterlogs_at: entry.get("waterlogs_at").ok()?,
                    renews_from: entry.get("renews_from").ok()?,
                    color: [
                        entry.get("color_r").ok()?,
                        entry.get("color_g").ok()?,
                        entry.get("color_b").ok()?,
                    ],
                    tick_rate: entry.get("tick_rate").ok()?,
                })
            })
            .collect();
        // Sorted, because a Lua table's pair order is not defined and the fluid
        // ids handed out downstream are positional. Two servers disagreeing
        // about which id is milk is charter rule 4 broken on the wire.
        fluids.sort_by(|a, b| a.fluid.cmp(&b.fluid));
        fluids
    }

    fn registered_tools(&self) -> Vec<Tool> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.tools") else {
            return Vec::new();
        };

        let mut tools: Vec<Tool> = registry
            .pairs::<String, Table>()
            .filter_map(Result::ok)
            .filter_map(|(id, entry)| {
                Some(Tool {
                    brush: Brush::parse(&entry.get::<String>("brush").ok()?)?,
                    speed_multiplier: entry.get("speed").ok()?,
                    default: entry.get("default").unwrap_or(false),
                    name: entry.get::<Option<String>>("name").ok().flatten(),
                    id,
                })
            })
            .collect();

        // Lua table order is unspecified and this list reaches the simulation.
        tools.sort_by(|a, b| a.id.cmp(&b.id));
        tools
    }

    fn registered_sky(&self) -> Option<Sky> {
        let registry = self
            .lua
            .named_registry_value::<Table>("tiamot.skies")
            .ok()?;

        // Lowest mod id wins where several register one, the same rule the
        // default tool uses: arbitrary but fixed beats depending on load order.
        let mut entries: Vec<(String, Table)> = registry
            .pairs::<String, Table>()
            .filter_map(Result::ok)
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let (mod_id, entry) = entries.into_iter().next()?;

        let frames: Table = entry.get("keyframes").ok()?;
        let mut keyframes: Vec<SkyKeyframe> = frames
            .sequence_values::<Table>()
            .filter_map(Result::ok)
            .filter_map(|frame| {
                let colour = |key: &str| -> Option<[f32; 3]> {
                    let table: Table = frame.get(key).ok()?;
                    Some([table.get(1).ok()?, table.get(2).ok()?, table.get(3).ok()?])
                };
                Some(SkyKeyframe {
                    time: frame.get("time").ok()?,
                    sky: colour("sky")?,
                    sun: colour("sun")?,
                    intensity: frame.get("intensity").ok()?,
                    // Validated in `register_sky`; absent means no grading, which
                    // is a keyframe saying nothing rather than an error.
                    grade: frame
                        .get::<Option<Table>>("grade")
                        .ok()
                        .flatten()
                        .as_ref()
                        .map_or(SkyGrade::NONE, read_grade),
                })
            })
            .collect();
        if keyframes.is_empty() {
            return None;
        }
        // Sorted here rather than trusted from the mod: the client walks these
        // in order to interpolate, and an out-of-order list would make the sky
        // jump backwards partway through the day.
        keyframes.sort_by(|a, b| a.time.total_cmp(&b.time));

        Some(Sky {
            mod_id,
            day_length_ticks: entry.get("day_length_ticks").ok()?,
            keyframes,
            // Morning unless the mod says otherwise. A counter left at zero
            // opens every new world at midnight, which is the one hour with
            // nothing in it to look at.
            start_time: entry
                .get::<Option<f32>>("start_time")
                .ok()
                .flatten()
                .unwrap_or(DEFAULT_START_TIME)
                .rem_euclid(1.0),
        })
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

    /// Installs `register_on_dig_complete` and `register_on_place`.
    ///
    /// The two `game.set` calls spell out literal names rather than looping,
    /// and that is load-bearing: `scripts/check-stubs.sh` finds the engine's
    /// API surface by grepping for `game.set("...")`, so a name that exists
    /// only as a loop variable is a name the drift check cannot see. The shared
    /// body lives in [`MluaVm::hook_registrar`], so this costs a line rather
    /// than a copy.
    fn install_cancellable_hooks(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        game.set(
            "register_on_dig_complete",
            self.hook_registrar(mod_id, HOOK_DIG, DIGGERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_place",
            self.hook_registrar(mod_id, HOOK_PLACE, PLACERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_punch",
            self.hook_registrar(mod_id, HOOK_PUNCH, PUNCHERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        // Registered through the same machinery even though it cannot veto:
        // what a hook DOES with its return value is the dispatcher's business,
        // and giving observation hooks a second registrar would be two paths
        // that have to keep agreeing about freezing and load order.
        game.set(
            "register_on_fluid_flow",
            self.hook_registrar(mod_id, HOOK_FLOW, FLOWERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `register_on_*` function for one cancellable hook.
    ///
    /// One implementation for both: they differ only in which list they append
    /// to and which registry key they write, and the version with two copies
    /// had already drifted by the time the second one was written.
    fn hook_registrar(
        &self,
        mod_id: &str,
        hook: &str,
        list: &'static str,
    ) -> Result<mlua::Function, ScriptError> {
        let owner = mod_id.to_owned();
        let key = Self::hook_key(hook, mod_id);
        self.lua
            .create_function(move |lua, callback: mlua::Function| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                lua.set_named_registry_value(&key, callback)?;
                // Load order, which the resolver already made deterministic —
                // so which mod gets to veto first is a property of the mod set
                // rather than of anything at runtime.
                let registered: Table = lua.named_registry_value(list)?;
                registered.push(owner.clone())?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))
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
            .create_function(move |lua, spec: Table| register_block(lua, &owner, &spec))
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
        let register_tool = self
            .lua
            .create_function(move |lua, spec: Table| register_tool(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_tool", register_tool)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let register_fluid = self
            .lua
            .create_function(move |lua, spec: Table| register_fluid(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_fluid", register_fluid)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let register_sky = self
            .lua
            .create_function(move |lua, spec: Table| register_sky(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_sky", register_sky)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let key = Self::tick_key(mod_id);
        let register_on_tick = self
            .lua
            .create_function(move |lua, callback: mlua::Function| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                lua.set_named_registry_value(&key, callback)?;
                // Registration order is the call order, and it is stable
                // because it is the order mods loaded in — which the resolver
                // already made deterministic.
                let tickers: Table = lua.named_registry_value("tiamot.tickers")?;
                tickers.push(owner.clone())?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_on_tick", register_on_tick)
            .map_err(|err| self.vm_error(&err))?;

        self.install_entity_step_hook(mod_id, game)?;

        // The two cancellable hooks. Registered exactly like `on_tick` — one
        // callback per mod, ordered by load order — so a mod author has one
        // shape to learn rather than three.
        self.install_cancellable_hooks(mod_id, game)?;

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

    /// The `game.get_light` function, built once per mod environment.
    ///
    /// Its own method because it is the only thing in the frozen API that
    /// depends on something outside the VM — see [`MluaVm::light`] for why it
    /// reads through a handle rather than capturing a store that does not exist
    /// at freeze.
    fn light_reader(&self) -> Result<mlua::Function, ScriptError> {
        // Levels are 0..15 per channel, the range the engine stores and the
        // same range `register_block{ light_emit }` takes. A mod that reads a
        // level and writes it straight back into an emitter gets what it asked
        // for.
        let light = std::sync::Arc::clone(&self.light);
        self.lua
            .create_function(move |lua, position: Table| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;

                let level = light
                    .lock()
                    .map_err(|_| {
                        mlua::Error::external(
                            "the light store is poisoned; the simulation thread panicked",
                        )
                    })?
                    .as_ref()
                    // No world yet — during worldgen, or in a test with no
                    // server behind the VM. Dark is the honest answer, and a
                    // mod that had to handle an error here would be a mod
                    // written around the engine's startup order.
                    .map_or(crate::light::Light::DARK, |source| {
                        source.light_at(crate::BlockPos::new(x, y, z))
                    });

                let out = lua.create_table()?;
                out.set("sun", level.sun())?;
                out.set("r", level.red())?;
                out.set("g", level.green())?;
                out.set("b", level.blue())?;
                Ok(out)
            })
            .map_err(|err| self.vm_error(&err))
    }

    /// The `game.set_block` function, built once per mod environment.
    ///
    /// Its own method for the reason `light_reader` is: it is part of the frozen
    /// API that depends on something outside the VM.
    fn block_writer(&self) -> Result<mlua::Function, ScriptError> {
        let edits = std::sync::Arc::clone(&self.edits);
        self.lua
            .create_function(move |_, (position, block): (Table, String)| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;
                let guard = edits.lock().map_err(|_| {
                    mlua::Error::external(
                        "the edit queue is poisoned; the simulation thread panicked",
                    )
                })?;
                // No world yet — during worldgen, or in a test with no server
                // behind the VM. Dropped rather than an error, the same way
                // `get_fluid` answers empty: a mod handling an error here would
                // be a mod written around the engine's startup order.
                let Some(edits) = guard.as_ref() else {
                    return Ok(false);
                };
                Ok(edits.set_block(crate::BlockPos::new(x, y, z), &block))
            })
            .map_err(|err| self.vm_error(&err))
    }

    /// The `game.get_fluid` and `game.set_fluid` pair, built once per mod
    /// environment.
    ///
    /// Its own method for the reason `light_reader` is: these are the part of
    /// the frozen API that depends on something outside the VM.
    fn fluid_functions(&self) -> Result<(mlua::Function, mlua::Function), ScriptError> {
        let reader = std::sync::Arc::clone(&self.fluid);
        let get = self
            .lua
            .create_function(move |lua, position: Table| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;

                let value = reader
                    .lock()
                    .map_err(|_| {
                        mlua::Error::external(
                            "the fluid store is poisoned; the simulation thread panicked",
                        )
                    })?
                    .as_ref()
                    // No world yet — during worldgen, or in a test with no
                    // server behind the VM. Nothing is the honest answer.
                    .map_or(crate::fluid::Fluid::EMPTY, |source| {
                        source.fluid_at(crate::BlockPos::new(x, y, z))
                    });

                let out = lua.create_table()?;
                // `level` and `source` rather than the packed byte: a mod that
                // had to know the bit layout would be a mod that breaks when
                // the layout changes.
                out.set("level", value.level())?;
                out.set("source", value.is_source())?;
                out.set("empty", value.is_empty())?;
                Ok(out)
            })
            .map_err(|err| self.vm_error(&err))?;

        let writer = std::sync::Arc::clone(&self.fluid);
        let set = self
            .lua
            .create_function(move |_, (position, spec): (Table, Table)| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;

                let guard = writer.lock().map_err(|_| {
                    mlua::Error::external(
                        "the fluid store is poisoned; the simulation thread panicked",
                    )
                })?;
                let Some(store) = guard.as_ref() else {
                    // Called before there is a world — during worldgen, say.
                    // Silently doing nothing beats an error a mod would have to
                    // write code around, and matches how `get_fluid` answers.
                    return Ok(false);
                };

                let level: u8 = spec.get("level").unwrap_or(crate::fluid::MAX_LEVEL);

                // **Clearing needs no fluid named, and demanding one was a bug
                // with teeth.** The stub documents `set_fluid(pos, {level = 0})`
                // to scoop; the implementation refused it as a missing field, so
                // the reference mod's scoop raised an error, and a hook that
                // errors disables its mod (charter rule 10). One attempt to pick
                // milk up therefore killed `core_milk` outright and every
                // placement after it silently did nothing.
                //
                // Reported from the window as two separate things — "no way to
                // destroy the source" and "after a certain amount of placements
                // it just stops working, like it gives up" — which were one bug
                // wearing both faces.
                let value = if level == 0 {
                    crate::fluid::Fluid::EMPTY
                } else {
                    let name: Option<String> = spec.get("fluid").ok();
                    let Some(name) = name else {
                        return Err(mlua::Error::external(
                            "set_fluid: missing required field `fluid`. Name the registered \
                             fluid to place, or pass `level = 0` to clear.",
                        ));
                    };
                    let Some(id) = store.fluid_id(&name) else {
                        return Err(mlua::Error::external(format!(
                            "set_fluid: no fluid registered as `{name}`"
                        )));
                    };
                    if spec.get("source").unwrap_or(false) {
                        crate::fluid::Fluid::source(id)
                    } else {
                        crate::fluid::Fluid::flowing(id, level)
                    }
                };
                Ok(store.set_fluid_at(crate::BlockPos::new(x, y, z), value))
            })
            .map_err(|err| self.vm_error(&err))?;

        Ok((get, set))
    }

    /// The `game.*_entity` family, built once per mod environment.
    ///
    /// Its own method for the reason `fluid_functions` is: these are the part of
    /// the frozen API that depends on something outside the VM.
    ///
    /// # What a mod says, and what it does not
    ///
    /// Positions are **world blocks as plain numbers** — one block is one yard
    /// (charter rule 5) — and never a chunk frame. Charter rule 7's
    /// `(chunk, local)` pairing exists so the engine never accumulates a
    /// world-space `f32`; a mod that had to know about it would be a mod that
    /// gets it wrong 60,000 blocks out, and every mod would have to get it right
    /// separately. See `ent::Transform::from_world`.
    ///
    /// Ids are opaque integers. Lua 5.4 has a real 64-bit integer subtype, so a
    /// mod holds one exactly rather than through an `f64` that starts rounding
    /// at 2^53 — one of the concrete reasons `docs/scripting-vm.md` picked it.
    fn entity_functions(&self, owner: &str) -> Result<EntityApi, ScriptError> {
        let (spawn, despawn) = self.entity_lifecycle(owner)?;
        let (get, set, within) = self.entity_queries()?;
        Ok(EntityApi {
            spawn,
            despawn,
            get,
            set,
            within,
        })
    }

    /// `game.spawn_entity` and `game.despawn_entity`.
    fn entity_lifecycle(
        &self,
        owner: &str,
    ) -> Result<(mlua::Function, mlua::Function), ScriptError> {
        /// A missing world is not an error a mod should have to write code
        /// around — it happens during worldgen and in a test with no server —
        /// so every call here answers "nothing happened" instead.
        macro_rules! store {
            ($slot:expr, $absent:expr) => {{
                let guard = $slot.lock().map_err(|_| {
                    mlua::Error::external(
                        "the entity store is poisoned; the simulation thread panicked",
                    )
                })?;
                match guard.as_ref() {
                    Some(store) => std::sync::Arc::clone(store),
                    None => return Ok($absent),
                }
            }};
        }

        let store = std::sync::Arc::clone(&self.entities);
        let source = owner.to_owned();
        let spawn = self
            .lua
            .create_function(move |_, spec: Table| {
                let store = store!(store, None::<i64>);
                let position: Table = spec.get("pos")?;
                let transform = crate::ent::Transform::from_world(
                    position.get("x")?,
                    position.get("y")?,
                    position.get("z")?,
                );
                let mut entity = crate::ent::Entity::at(transform, source.clone());
                entity.model = spec.get::<Option<String>>("model")?;
                if let Some(points) = spec.get::<Option<u32>>("health")? {
                    entity.health = Some(crate::ent::Health::full(points));
                }
                if let Some(name) = spec.get::<Option<String>>("nametag")? {
                    entity.nametag = Some(crate::ent::Nametag::Text(name));
                }
                // A box only if the mod asked for one: an entity with no
                // collider is a marker, and markers are useful.
                if let Some(box_spec) = spec.get::<Option<Table>>("collider")? {
                    entity.collider = Some(crate::ent::Collider {
                        width: box_spec.get("width")?,
                        height: box_spec.get("height")?,
                    });
                }
                Ok(store.spawn(entity).map(|id| id.0 as i64))
            })
            .map_err(|err| self.vm_error(&err))?;

        let store = std::sync::Arc::clone(&self.entities);
        let despawn = self
            .lua
            .create_function(move |_, id: i64| {
                let store = store!(store, false);
                Ok(store.despawn(crate::ent::EntityId(id as u64)))
            })
            .map_err(|err| self.vm_error(&err))?;

        Ok((spawn, despawn))
    }

    /// `game.entity`, `game.set_entity` and `game.entities_in_radius`.
    fn entity_queries(
        &self,
    ) -> Result<(mlua::Function, mlua::Function, mlua::Function), ScriptError> {
        /// As in `entity_lifecycle`: no world is not an error a mod writes code
        /// around.
        macro_rules! store {
            ($slot:expr, $absent:expr) => {{
                let guard = $slot.lock().map_err(|_| {
                    mlua::Error::external(
                        "the entity store is poisoned; the simulation thread panicked",
                    )
                })?;
                match guard.as_ref() {
                    Some(store) => std::sync::Arc::clone(store),
                    None => return Ok($absent),
                }
            }};
        }

        let store = std::sync::Arc::clone(&self.entities);
        let get = self
            .lua
            .create_function(move |lua, id: i64| {
                let store = store!(store, None::<Table>);
                let Some(entity) = store.get(crate::ent::EntityId(id as u64)) else {
                    return Ok(None);
                };
                let out = lua.create_table()?;
                let [x, y, z] = entity.transform.to_world();
                let position = lua.create_table()?;
                position.set("x", x)?;
                position.set("y", y)?;
                position.set("z", z)?;
                out.set("pos", position)?;
                out.set("yaw", entity.transform.yaw)?;
                out.set("pitch", entity.transform.pitch)?;
                let velocity = lua.create_table()?;
                velocity.set("x", entity.velocity.0[0])?;
                velocity.set("y", entity.velocity.0[1])?;
                velocity.set("z", entity.velocity.0[2])?;
                out.set("velocity", velocity)?;
                out.set("on_ground", entity.on_ground)?;
                out.set("source", entity.source)?;
                out.set("model", entity.model)?;
                out.set("anim", entity.anim.0)?;
                if let Some(health) = entity.health {
                    out.set("health", health.current)?;
                    out.set("max_health", health.max)?;
                }
                Ok(Some(out))
            })
            .map_err(|err| self.vm_error(&err))?;

        let store = std::sync::Arc::clone(&self.entities);
        let set = self
            .lua
            .create_function(move |_, (id, spec): (i64, Table)| {
                let store = store!(store, false);
                let patch = read_patch(&spec)?;
                Ok(store.patch(crate::ent::EntityId(id as u64), &patch))
            })
            .map_err(|err| self.vm_error(&err))?;

        let store = std::sync::Arc::clone(&self.entities);
        let within = self
            .lua
            .create_function(
                move |lua, (position, radius, filter): (Table, f64, Option<String>)| {
                    let store = store!(store, lua.create_table()?);
                    let centre = [position.get("x")?, position.get("y")?, position.get("z")?];
                    let found = store.within(centre, radius, filter.as_deref());
                    // A sequence, so `ipairs` works and the nearest is index 1.
                    let out = lua.create_table()?;
                    for (index, id) in found.into_iter().enumerate() {
                        out.set(index + 1, id.0 as i64)?;
                    }
                    Ok(out)
                },
            )
            .map_err(|err| self.vm_error(&err))?;

        Ok((get, set, within))
    }

    /// Puts the five entity functions on the `game` table.
    ///
    /// **Set one by one with literal names, rather than from a list.**
    /// `scripts/check-stubs.sh` finds the engine's API surface by grepping for
    /// `game.set("name"`, so a loop over a table of names would register five
    /// functions the stub checker cannot see — and that checker exists
    /// precisely to catch the API and its documentation drifting apart.
    fn install_entity_api(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let entities = self.entity_functions(mod_id)?;
        game.set("spawn_entity", entities.spawn)
            .map_err(|err| self.vm_error(&err))?;
        game.set("despawn_entity", entities.despawn)
            .map_err(|err| self.vm_error(&err))?;
        game.set("entity", entities.get)
            .map_err(|err| self.vm_error(&err))?;
        game.set("set_entity", entities.set)
            .map_err(|err| self.vm_error(&err))?;
        game.set("entities_in_radius", entities.within)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Puts `game.storage` on the `game` table.
    ///
    /// A table with three functions rather than three `game.*` entries, because
    /// this is one concept and a mod reads `game.storage.get` more easily than
    /// `game.storage_get`. `check-stubs.sh` sees it as one registration, which
    /// is why the stubs document it as an `@field`.
    ///
    /// **The mod id is captured, never passed.** A mod cannot name another's
    /// storage because there is nowhere in the API to put the name — the
    /// isolation is a property of the surface rather than of good behaviour.
    fn install_storage_api(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let storage = self.lua.create_table().map_err(|err| self.vm_error(&err))?;

        macro_rules! store {
            ($slot:expr, $absent:expr) => {{
                let guard = $slot.lock().map_err(|_| {
                    mlua::Error::external("the mod storage is poisoned; the simulation panicked")
                })?;
                match guard.as_ref() {
                    Some(store) => std::sync::Arc::clone(store),
                    None => return Ok($absent),
                }
            }};
        }

        let slot = std::sync::Arc::clone(&self.storage);
        let owner = mod_id.to_owned();
        let get = self
            .lua
            .create_function(move |lua, key: String| {
                let store = store!(slot, mlua::Value::Nil);
                Ok(match store.get(&owner, &key) {
                    None => mlua::Value::Nil,
                    Some(crate::storage::Value::Text(text)) => {
                        mlua::Value::String(lua.create_string(&text)?)
                    }
                    Some(crate::storage::Value::Number(number)) => mlua::Value::Number(number),
                    Some(crate::storage::Value::Flag(flag)) => mlua::Value::Boolean(flag),
                })
            })
            .map_err(|err| self.vm_error(&err))?;
        storage.set("get", get).map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.storage);
        let owner = mod_id.to_owned();
        let set = self
            .lua
            .create_function(move |_, (key, value): (String, mlua::Value)| {
                let store = store!(slot, ());
                let value = match value {
                    mlua::Value::Nil => None,
                    mlua::Value::String(text) => {
                        Some(crate::storage::Value::Text(text.to_str()?.to_owned()))
                    }
                    mlua::Value::Integer(number) => {
                        Some(crate::storage::Value::Number(number as f64))
                    }
                    mlua::Value::Number(number) => Some(crate::storage::Value::Number(number)),
                    mlua::Value::Boolean(flag) => Some(crate::storage::Value::Flag(flag)),
                    // A table would need a serialisation format that becomes
                    // part of the mod API for ever, and with it the engine's
                    // opinion on cycles, functions and userdata. Refused
                    // loudly, so a mod encodes its own structure into a string
                    // — which it can change without an engine release.
                    other => {
                        return Err(mlua::Error::external(format!(
                            "storage.set: a {} cannot be stored; use a string, a number or a \
                             boolean",
                            other.type_name()
                        )));
                    }
                };
                store.set(&owner, &key, value);
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        storage.set("set", set).map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.storage);
        let owner = mod_id.to_owned();
        let keys = self
            .lua
            .create_function(move |lua, ()| {
                let store = store!(slot, lua.create_table()?);
                let out = lua.create_table()?;
                for (index, key) in store.keys(&owner).into_iter().enumerate() {
                    out.set(index + 1, key)?;
                }
                Ok(out)
            })
            .map_err(|err| self.vm_error(&err))?;
        storage
            .set("keys", keys)
            .map_err(|err| self.vm_error(&err))?;

        game.set("storage", storage)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Puts `game.register_on_entity_step` on the `game` table.
    ///
    /// Its own method only because `install_registration` had outgrown the line
    /// limit; the shape is exactly `register_on_tick`'s, which is the point —
    /// a mod author has one shape to learn rather than several.
    fn install_entity_step_hook(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let owner = mod_id.to_owned();
        let key = Self::hook_key("on_entity_step", mod_id);
        let register_on_entity_step = self
            .lua
            .create_function(move |lua, callback: mlua::Function| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                lua.set_named_registry_value(&key, callback)?;
                // The same registration-order list `on_tick` keeps, for the
                // same reason: load order is the call order, and the resolver
                // already made load order deterministic.
                let steppers: Table = lua.named_registry_value("tiamot.entity_steppers")?;
                let already = steppers
                    .sequence_values::<String>()
                    .filter_map(Result::ok)
                    .any(|existing| existing == owner);
                if !already {
                    steppers.set(steppers.raw_len() + 1, owner.clone())?;
                }
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_on_entity_step", register_on_entity_step)
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

        let set_block = self.block_writer()?;
        game.set("set_block", set_block)
            .map_err(|err| self.vm_error(&err))?;

        self.install_entity_api(mod_id, game)?;
        self.install_storage_api(mod_id, game)?;

        let (get_fluid, set_fluid) = self.fluid_functions()?;
        game.set("get_fluid", get_fluid)
            .map_err(|err| self.vm_error(&err))?;
        game.set("set_fluid", set_fluid)
            .map_err(|err| self.vm_error(&err))?;

        let get_light = self.light_reader()?;
        game.set("get_light", get_light)
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
        let block_textures = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let generators = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let actions = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.blocks", blocks)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.block_textures", block_textures)
            .map_err(|err| self.vm_error(&err))?;
        let block_rules = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.block_rules", block_rules)
            .map_err(|err| self.vm_error(&err))?;
        let tools = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.tools", tools)
            .map_err(|err| self.vm_error(&err))?;
        let fluids = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.fluids", fluids)
            .map_err(|err| self.vm_error(&err))?;
        let skies = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.skies", skies)
            .map_err(|err| self.vm_error(&err))?;
        let tickers = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.tickers", tickers)
            .map_err(|err| self.vm_error(&err))?;
        let steppers = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.entity_steppers", steppers)
            .map_err(|err| self.vm_error(&err))?;
        for list in [DIGGERS, PLACERS, PUNCHERS, FLOWERS] {
            let table = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
            self.lua
                .set_named_registry_value(list, table)
                .map_err(|err| self.vm_error(&err))?;
        }
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

    /// Creates a VM ready for `load_mod`.
    ///
    /// An alias for [`ScriptVm::create`], kept so call sites read naturally.
    ///
    /// # Errors
    ///
    /// As [`ScriptVm::create`].
    pub fn new(limits: VmLimits) -> Result<Self, ScriptError> {
        <Self as ScriptVm>::create(limits)
    }

    /// Registry key holding a mod's `on_tick` callback.
    fn tick_key(mod_id: &str) -> String {
        format!("tiamot.on_tick.{mod_id}")
    }

    /// Where one mod's callback for a named hook is stashed.
    fn hook_key(hook: &str, mod_id: &str) -> String {
        format!("tiamot.{hook}.{mod_id}")
    }

    /// Runs one cancellable hook across every mod that registered it.
    ///
    /// The shared body of [`ScriptVm::dig_complete`] and [`ScriptVm::place`],
    /// which differ only in which list they walk and what table they hand over.
    /// Two copies of this drifted apart in about the time it took to write the
    /// second one.
    ///
    /// **Only an explicit `false` cancels.** A hook that returns nothing, or
    /// `nil`, or a table, is observing rather than voting — otherwise every mod
    /// author who forgot a `return` would silently make their block
    /// unbreakable, and the engine cannot tell that apart from a deliberate
    /// veto.
    fn run_hook(&mut self, hook: &str, list: &str, event: &Table) -> HookOutcome {
        let Ok(registered) = self.lua.named_registry_value::<Table>(list) else {
            // No list means nothing ever registered for this hook, which is the
            // ordinary case on a server with no mods that care.
            return HookOutcome::allow();
        };
        let mods: Vec<String> = registered
            .sequence_values::<String>()
            .filter_map(Result::ok)
            .collect();

        let mut outcome = HookOutcome::allow();
        for mod_id in mods {
            if self.faulted.contains(&mod_id) {
                continue;
            }
            let Ok(callback) = self
                .lua
                .named_registry_value::<mlua::Function>(&Self::hook_key(hook, &mod_id))
            else {
                continue;
            };

            if self.arm_budget(self.limits.instructions_per_call).is_err() {
                continue;
            }
            let result = callback.call::<mlua::Value>(event.clone());
            self.disarm_budget();

            match result {
                // Charter rule 10, and the reason it matters here more than
                // anywhere else: a mod that throws while deciding whether a dig
                // may happen is disabled and the dig PROCEEDS. Treating a crash
                // as a refusal would let one broken mod stop everybody on the
                // server from digging.
                Err(err) => {
                    let error = Self::classify(&err, &mod_id, hook);
                    self.faulted.insert(mod_id.clone());
                    tracing::error!(
                        mod_id = %mod_id,
                        error = %error,
                        "disabling mod after a {hook} failure; the action is allowed to proceed"
                    );
                    outcome.faults.push((mod_id, error));
                }
                Ok(mlua::Value::Boolean(false)) => {
                    outcome.allowed = false;
                    // Stop here: see the trait docs. A hook running after a
                    // veto would be invited to take side effects for an action
                    // that is not going to happen.
                    return outcome;
                }
                // A string cancels too, and says what to tell the player — or
                // says to tell them nothing, when it is empty. See
                // `HookOutcome::reason`: a mod that HANDLED the action was
                // being answered with a refusal every time it worked.
                Ok(mlua::Value::String(reason)) => {
                    let mut reason = reason.to_string_lossy();
                    // A mod is not hostile but it can be buggy, and this goes on
                    // the wire. Truncated on a character boundary, because
                    // slicing a UTF-8 string anywhere else panics.
                    if reason.len() > crate::script::MAX_REFUSAL_BYTES {
                        let end = (0..=crate::script::MAX_REFUSAL_BYTES)
                            .rev()
                            .find(|at| reason.is_char_boundary(*at))
                            .unwrap_or(0);
                        reason.truncate(end);
                    }
                    outcome.allowed = false;
                    outcome.reason = Some(reason);
                    return outcome;
                }
                Ok(_) => {}
            }
        }
        outcome
    }

    /// Builds the Lua table a hook receives.
    ///
    /// The UUID goes over as a lowercase hex string. Charter rule 13 keys
    /// everything on the UUID and never on the display name, and a mod that
    /// wants to store per-player state needs something it can use as a table
    /// key — which 16 raw bytes in a Lua string technically is and legibly is
    /// not, the moment anyone prints one.
    fn hook_event(&self, player: [u8; 32]) -> Result<Table, mlua::Error> {
        let event = self.lua.create_table()?;
        event.set("player", hex_uuid(player))?;
        Ok(event)
    }

    /// Blocks registered so far, keyed by string id.
    ///
    /// **Deliberately not called `registered_blocks`.** That name belongs to
    /// the [`ScriptVm`] trait method, which returns them ordered by numeric id
    /// because the host replays them into a registry that assigns
    /// sequentially. An inherent method of the same name silently won method
    /// resolution and handed callers alphabetical order instead — which would
    /// have given every mod block a different id than its mod was told, and the
    /// only symptom would have been blocks turning into the wrong material.
    ///
    /// This one exists for lookups, where order does not matter.
    #[must_use]
    pub fn block_ids(&self) -> BTreeMap<String, MaterialId> {
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

/// The body of `game.register_block`.
///
/// A free function rather than the closure it used to be, so the closure is one
/// line and this can be read without scrolling past the rest of the
/// registration API.
fn register_block(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<u16> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_block: missing required field `id`"))?;

    // Unknown fields are an error naming the field: a typo in `hardness`
    // should say so, not silently take the default.
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

    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;

    // Parsed before anything is registered, so a bad texture path leaves the
    // registry exactly as it was rather than half a block behind.
    let texture = match spec.get::<Option<Table>>("textures")? {
        Some(textures) => Some(block_texture_path(&qualified, &textures)?),
        None => None,
    };

    let registry: Table = lua.named_registry_value("tiamot.blocks")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "block `{qualified}` is already registered"
        )));
    }
    let next: u16 = lua.named_registry_value("tiamot.next_material")?;
    registry.set(qualified.clone(), next)?;
    lua.set_named_registry_value("tiamot.next_material", next + 1)?;

    // Breaking rules, recorded whether or not the mod set them: an absent
    // entry and a defaulted one must not be distinguishable downstream.
    {
        let hardness: Option<f32> = spec.get("hardness")?;
        if let Some(hardness) = hardness
            && (!hardness.is_finite() || hardness < 0.0)
        {
            return Err(mlua::Error::external(format!(
                "register_block(\"{id}\"): hardness must be a non-negative number of seconds, \
                 got {hardness}"
            )));
        }
        // Zero is refused as well as negative, unlike `hardness`. A hardness of
        // zero is meaningful — a block that comes apart on contact — but a
        // dominance of zero means "this material has no say in a mixture at
        // all", and a block made entirely of such materials would have nothing
        // left to average. Refusing it here is cheaper than the alternative,
        // which is a division nobody can predict the result of.
        let dominance: Option<f32> = spec.get("dominance")?;
        if let Some(dominance) = dominance
            && (!dominance.is_finite() || dominance <= 0.0)
        {
            return Err(mlua::Error::external(format!(
                "register_block(\"{id}\"): dominance must be a positive number, got {dominance}"
            )));
        }
        let drops: Option<Table> = spec.get("drops")?;
        let entry = lua.create_table()?;
        if let Some(hardness) = hardness {
            entry.set("hardness", hardness)?;
        }
        if let Some(dominance) = dominance {
            entry.set("dominance", dominance)?;
        }

        // `light_emit = { r = 0..15, g = ..., b = ... }`. Validated here rather
        // than clamped silently, because a mod asking for 30 has misunderstood
        // the range and should be told at registration — when the error names
        // the mod and the block — rather than shipping a lamp that is quietly
        // dimmer than its author intended.
        if let Some(emit) = spec.get::<Option<Table>>("light_emit")? {
            for key in ["r", "g", "b"] {
                let Some(level) = emit.get::<Option<i64>>(key)? else {
                    continue;
                };
                if !(0..=i64::from(crate::light::MAX_LEVEL)).contains(&level) {
                    return Err(mlua::Error::external(format!(
                        "register_block(\"{id}\"): light_emit.{key} must be 0..={}, got {level}",
                        crate::light::MAX_LEVEL
                    )));
                }
            }
            let stored = lua.create_table()?;
            for key in ["r", "g", "b"] {
                stored.set(key, emit.get::<Option<u8>>(key)?.unwrap_or(0))?;
            }
            entry.set("light_emit", stored)?;
        }
        if let Some(drops) = drops {
            let parsed = lua.create_table()?;
            for pair in drops.pairs::<String, u32>() {
                let (dropped, units) = pair?;
                parsed.set(
                    qualify_id(owner, &dropped).map_err(mlua::Error::external)?,
                    units,
                )?;
            }
            entry.set("drops", parsed)?;
        }
        let rules: Table = lua.named_registry_value("tiamot.block_rules")?;
        rules.set(qualified.clone(), entry)?;
    }

    if let Some(path) = texture {
        let entry = lua.create_table()?;
        // The owning mod travels with the path because the path is relative to
        // that mod's directory and nothing downstream can recover which one
        // from the block id alone — `qualify_id` allows a mod to register under
        // its own namespace, but the namespace is not guaranteed to keep
        // matching if that ever loosens.
        entry.set("mod", owner.to_owned())?;
        entry.set("path", path)?;
        let textures: Table = lua.named_registry_value("tiamot.block_textures")?;
        textures.set(qualified, entry)?;
    }
    Ok(next)
}

/// Registers a tool: what it removes, and how fast.
///
/// The `brush` is the whole reason this exists as an API rather than a hard-
/// coded rule. Sub-node resolution is only a real feature if a mod can reach
/// it, and `brush = "subnode"` is how — `core:chisel` in the reference mods is
/// nothing more than a mod using this.
/// The five entity functions, built together and installed by name.
struct EntityApi {
    spawn: mlua::Function,
    despawn: mlua::Function,
    get: mlua::Function,
    set: mlua::Function,
    within: mlua::Function,
}

/// Reads a `game.set_entity` table into a [`crate::ent::Patch`].
///
/// Absent means "leave it alone" throughout — see the type. Nothing here can
/// create or destroy an entity or change its size, so a mod cannot grow a mob a
/// collider halfway through a tick.
fn read_patch(spec: &Table) -> mlua::Result<crate::ent::Patch> {
    let mut patch = crate::ent::Patch::default();
    if let Some(position) = spec.get::<Option<Table>>("pos")? {
        patch.position = Some([position.get("x")?, position.get("y")?, position.get("z")?]);
    }
    if let Some(velocity) = spec.get::<Option<Table>>("velocity")? {
        patch.velocity = Some([velocity.get("x")?, velocity.get("y")?, velocity.get("z")?]);
    }
    patch.yaw = spec.get("yaw")?;
    patch.pitch = spec.get("pitch")?;
    patch.health = spec.get("health")?;
    if let Some(tag) = spec.get::<Option<u8>>("anim")? {
        patch.anim = Some(crate::ent::AnimTag(tag));
    }
    if let Some(drive) = spec.get::<Option<Table>>("drive")? {
        let walk: Option<Table> = drive.get("walk")?;
        let (x, z) = match walk {
            Some(walk) => (walk.get("x")?, walk.get("z")?),
            None => (0.0, 0.0),
        };
        // The gait names, and an unknown one is an error rather than a silent
        // walk: a mod that typed `"sprint"` should hear about it.
        let gait = match drive.get::<Option<String>>("gait")?.as_deref() {
            None | Some("walk") => crate::phys::Gait::Walk,
            Some("sprint") => crate::phys::Gait::Sprint,
            Some("sneak") => crate::phys::Gait::Sneak,
            Some(other) => {
                return Err(mlua::Error::external(format!(
                    "set_entity: unknown gait `{other}`; expected walk, sprint or sneak"
                )));
            }
        };
        patch.drive = Some(crate::phys::Intent {
            walk: [x, z],
            jump: drive.get::<Option<bool>>("jump")?.unwrap_or(false),
            gait,
        });
    }
    Ok(patch)
}

/// `game.register_fluid{ id, material, flow_range, tick_rate }`.
///
/// The whole of what the engine needs to simulate and draw a fluid. Everything
/// else a fluid might do — hurt you, make a sound, be drinkable — is the
/// registering mod's business and needs no engine support beyond the hooks that
/// already exist.
/// What a fluid looks like from inside, as three `0..=255` channels.
///
/// Split out of [`register_fluid`] only because that function had outgrown the
/// line limit; it is one field's parsing and belongs beside the rest.
fn fluid_colour(spec: &Table, qualified: &str) -> mlua::Result<[u8; 3]> {
    let Some(table) = spec.get::<Option<Table>>("color")? else {
        return Ok(FluidRules::DEFAULT_COLOR);
    };
    let mut channels = FluidRules::DEFAULT_COLOR;
    for (index, key) in ["r", "g", "b"].into_iter().enumerate() {
        // A channel a mod left out is full, so `{ r = 0 }` is cyan rather than
        // black — the same reading `light_emit` gives an omitted channel.
        let value: i64 = table.get::<Option<i64>>(key)?.unwrap_or(255);
        if !(0..=255).contains(&value) {
            return Err(mlua::Error::external(format!(
                "register_fluid(\"{qualified}\"): color.{key} must be 0..=255, got {value}"
            )));
        }
        channels[index] = value as u8;
    }
    Ok(channels)
}

fn register_fluid(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<()> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_fluid: missing required field `id`"))?;

    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !FLUID_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_fluid(\"{id}\"): unknown field `{name}`"
                )));
            }
        }
    }

    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;

    // **Required, and deliberately not defaulted.** A fluid with no material is
    // a fluid the mesher cannot draw, and inventing one here would mean the
    // engine picking what milk looks like — charter rule 1 says it does not get
    // to. Qualified against the same mod, so a fluid can name its own block.
    let material: String = spec.get("material").map_err(|_| {
        mlua::Error::external(format!(
            "register_fluid(\"{qualified}\"): missing required field `material`. A fluid is \
             drawn as a registered block; name the one it should look like."
        ))
    })?;
    let material = qualify_id(owner, &material).map_err(mlua::Error::external)?;

    let flow_range: u8 = spec
        .get("flow_range")
        .unwrap_or(FluidRules::DEFAULT_FLOW_RANGE);
    if flow_range == 0 || flow_range > crate::fluid::MAX_LEVEL {
        return Err(mlua::Error::external(format!(
            "register_fluid(\"{qualified}\"): flow_range must be 1..={}, got {flow_range}. The \
             level a block holds IS how far the fluid has travelled, and there are only that \
             many levels.",
            crate::fluid::MAX_LEVEL
        )));
    }

    // Contract §4's threshold, as the registering mod's decision rather than the
    // engine's. Zero would make every block floor and the fluid would never move
    // at all, which reads as the engine ignoring the mod.
    let waterlogs_at: u32 = spec
        .get("waterlogs_at")
        .unwrap_or(FluidRules::DEFAULT_WATERLOGS_AT);
    if waterlogs_at == 0 || waterlogs_at > crate::UNITS_PER_BLOCK {
        return Err(mlua::Error::external(format!(
            "register_fluid(\"{qualified}\"): waterlogs_at must be 1..={}, got {waterlogs_at}. \
             It is how many of a block's 27 cells must be filled before this fluid treats it as \
             floor.",
            crate::UNITS_PER_BLOCK
        )));
    }

    let tick_rate: u8 = spec
        .get("tick_rate")
        .unwrap_or(FluidRules::DEFAULT_TICK_RATE);
    if tick_rate == 0 {
        return Err(mlua::Error::external(format!(
            "register_fluid(\"{qualified}\"): tick_rate must be at least 1, got 0. A fluid that \
             updates every zeroth tick never moves, which reads as the engine ignoring the mod."
        )));
    }

    // How many neighbouring sources make a block a source of its own. Bounded
    // by the four lateral directions: asking for five is asking for a rule that
    // can never fire, which is a typo rather than an intention.
    let renews_from: u8 = spec
        .get("renews_from")
        .unwrap_or(FluidRules::DEFAULT_RENEWS_FROM);
    if renews_from > 4 {
        return Err(mlua::Error::external(format!(
            "register_fluid(\"{qualified}\"): renews_from must be 0..=4, got {renews_from}. It \
             counts the four lateral neighbours, so anything above four is a rule that never \
             fires."
        )));
    }

    let color = fluid_colour(spec, &qualified)?;

    let registry: Table = lua.named_registry_value("tiamot.fluids")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "fluid `{qualified}` is already registered"
        )));
    }

    let entry = lua.create_table()?;
    entry.set("material", material)?;
    entry.set("flow_range", flow_range)?;
    entry.set("waterlogs_at", waterlogs_at)?;
    entry.set("tick_rate", tick_rate)?;
    entry.set("renews_from", renews_from)?;
    entry.set("color_r", color[0])?;
    entry.set("color_g", color[1])?;
    entry.set("color_b", color[2])?;
    registry.set(qualified, entry)?;
    Ok(())
}

fn register_tool(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<()> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_tool: missing required field `id`"))?;

    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !TOOL_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_tool(\"{id}\"): unknown field `{name}`"
                )));
            }
        }
    }

    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;

    let brush: String = spec.get("brush").unwrap_or_else(|_| "block".to_owned());
    if Brush::parse(&brush).is_none() {
        return Err(mlua::Error::external(format!(
            "register_tool(\"{qualified}\"): unknown brush `{brush}`. The engine implements \
             `block` and `subnode`."
        )));
    }

    let default: bool = spec.get("default").unwrap_or(false);
    let speed: f32 = spec.get("speed_multiplier").unwrap_or(1.0);
    if !speed.is_finite() || speed <= 0.0 {
        return Err(mlua::Error::external(format!(
            "register_tool(\"{qualified}\"): speed_multiplier must be a positive number, got \
             {speed}. A tool that digs at zero speed never finishes, which reads as a hung \
             client rather than as a mistake in a mod."
        )));
    }

    let registry: Table = lua.named_registry_value("tiamot.tools")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "tool `{qualified}` is already registered"
        )));
    }

    let entry = lua.create_table()?;
    entry.set("brush", brush)?;
    entry.set("speed", speed)?;
    entry.set("default", default)?;
    // Accepted since tools existed and discarded until now, which meant a mod
    // could name its tool and the name went nowhere. The client shows it.
    if let Ok(Some(name)) = spec.get::<Option<String>>("name") {
        entry.set("name", name)?;
    }
    registry.set(qualified, entry)?;
    Ok(())
}

/// Registers the sky: how long a day is and what colour it goes.
///
/// **Charter rule 1.** The engine has no idea what a day is. It knows how to
/// advance a number and how to interpolate colours, and a mod supplies both the
/// number's period and the colours.
fn register_sky(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<()> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    for pair in spec.pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !SKY_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_sky: unknown field `{name}`"
                )));
            }
        }
    }

    let day_length_ticks: u32 = spec.get("day_length_ticks").map_err(|_| {
        mlua::Error::external("register_sky: missing required field `day_length_ticks`")
    })?;
    if day_length_ticks == 0 {
        return Err(mlua::Error::external(
            "register_sky: day_length_ticks must be at least 1; a day of no ticks never advances",
        ));
    }

    let keyframes: Table = spec
        .get("keyframes")
        .map_err(|_| mlua::Error::external("register_sky: missing required field `keyframes`"))?;
    let mut count = 0;
    for frame in keyframes.sequence_values::<Table>() {
        let frame = frame?;
        let time: f32 = frame
            .get("time")
            .map_err(|_| mlua::Error::external("register_sky: every keyframe needs a `time`"))?;
        if !(0.0..=1.0).contains(&time) {
            return Err(mlua::Error::external(format!(
                "register_sky: keyframe time must be 0..=1, got {time}"
            )));
        }
        let intensity: f32 = frame.get("intensity").map_err(|_| {
            mlua::Error::external("register_sky: every keyframe needs an `intensity`")
        })?;
        if !(0.0..=1.0).contains(&intensity) {
            return Err(mlua::Error::external(format!(
                "register_sky: keyframe intensity must be 0..=1, got {intensity}"
            )));
        }
        for key in ["sky", "sun"] {
            let colour: Table = frame.get(key).map_err(|_| {
                mlua::Error::external(format!(
                    "register_sky: every keyframe needs a `{key}` colour"
                ))
            })?;
            if colour.len()? != 3 {
                return Err(mlua::Error::external(format!(
                    "register_sky: `{key}` must be three numbers, {{r, g, b}}"
                )));
            }
        }
        if let Some(grade) = frame.get::<Option<Table>>("grade")? {
            validate_grade(&grade)?;
        }
        count += 1;
    }
    if count == 0 {
        return Err(mlua::Error::external(
            "register_sky: `keyframes` is empty; a sky with no colours has nothing to draw",
        ));
    }

    let registry: Table = lua.named_registry_value("tiamot.skies")?;
    let entry = lua.create_table()?;
    entry.set("day_length_ticks", day_length_ticks)?;
    entry.set("keyframes", keyframes)?;
    registry.set(owner.to_owned(), entry)?;
    Ok(())
}

/// Checks a keyframe's `grade` table, so a mistake in it is a registration
/// error rather than a colour nobody can explain.
///
/// Every field is optional and every bound is generous — this is a stylised
/// look, not a calibrated pipeline — but they are bounds all the same: a
/// `gamma` of zero is a divide by nothing, and a `tint` of 500 is a white
/// screen with a bug report attached.
fn validate_grade(grade: &Table) -> mlua::Result<()> {
    for pair in grade.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !GRADE_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_sky: unknown `grade` field `{name}`"
                )));
            }
        }
    }

    for (key, low, high) in [
        ("exposure", 0.0, GRADE_MAX),
        ("contrast", 0.0, GRADE_MAX),
        ("saturation", 0.0, GRADE_MAX),
        // Never zero: the bake raises each channel to this power, and a zero
        // exponent maps every colour in the frame to white.
        ("gamma", GRADE_MIN_GAMMA, GRADE_MAX),
    ] {
        if let Some(value) = grade.get::<Option<f32>>(key)?
            && (!value.is_finite() || value < low || value > high)
        {
            return Err(mlua::Error::external(format!(
                "register_sky: `grade.{key}` must be {low}..={high}, got {value}"
            )));
        }
    }

    for (key, low, high) in [("tint", 0.0, GRADE_MAX), ("offset", -1.0, 1.0)] {
        if let Some(colour) = grade.get::<Option<Table>>(key)? {
            if colour.len()? != 3 {
                return Err(mlua::Error::external(format!(
                    "register_sky: `grade.{key}` must be three numbers, {{r, g, b}}"
                )));
            }
            for channel in 1..=3 {
                let value: f32 = colour.get(channel)?;
                if !value.is_finite() || value < low || value > high {
                    return Err(mlua::Error::external(format!(
                        "register_sky: `grade.{key}` must be {low}..={high}, got {value}"
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Reads a `grade` table that [`validate_grade`] has already accepted.
///
/// Absent fields keep their identity, so a keyframe may set one knob without
/// restating the other five.
fn read_grade(grade: &Table) -> SkyGrade {
    let scalar = |key: &str, fallback: f32| -> f32 {
        grade
            .get::<Option<f32>>(key)
            .ok()
            .flatten()
            .unwrap_or(fallback)
    };
    let colour = |key: &str, fallback: [f32; 3]| -> [f32; 3] {
        grade
            .get::<Option<Table>>(key)
            .ok()
            .flatten()
            .and_then(|table| Some([table.get(1).ok()?, table.get(2).ok()?, table.get(3).ok()?]))
            .unwrap_or(fallback)
    };
    SkyGrade {
        exposure: scalar("exposure", SkyGrade::NONE.exposure),
        tint: colour("tint", SkyGrade::NONE.tint),
        offset: colour("offset", SkyGrade::NONE.offset),
        contrast: scalar("contrast", SkyGrade::NONE.contrast),
        saturation: scalar("saturation", SkyGrade::NONE.saturation),
        gamma: scalar("gamma", SkyGrade::NONE.gamma),
    }
}

/// Keys a keyframe's `grade` sub-table accepts.
const GRADE_FIELDS: [&str; 6] = [
    "exposure",
    "tint",
    "offset",
    "contrast",
    "saturation",
    "gamma",
];

/// The largest multiplier any grading knob may take.
///
/// Four. Enough for a strong stylised look and far short of the values that
/// only ever mean a mod meant to write a fraction.
const GRADE_MAX: f32 = 4.0;

/// The smallest `gamma`. Not zero — see [`validate_grade`].
const GRADE_MIN_GAMMA: f32 = 0.1;

/// Fields `register_sky` accepts.
/// Every field `register_sky` accepts.
///
/// Checked rather than ignored, so a typo in a mod is an error at registration
/// instead of a setting that silently does nothing. Adding a field to the Lua
/// side without adding it here rejects the whole registration — the world then
/// has no sky at all, which is exactly what happened when `start_time` was
/// added: no day, no sun, no shadows, and every graphics setting looking the
/// same because there was nothing lit to tell them apart.
const SKY_FIELDS: [&str; 3] = ["day_length_ticks", "keyframes", "start_time"];

/// Fields `register_tool` accepts.
const TOOL_FIELDS: [&str; 5] = ["id", "name", "brush", "speed_multiplier", "default"];

/// Fields `register_fluid` accepts. Anything else is a typo, and a typo that is
/// silently ignored is a mod whose author cannot tell why nothing happened.
const FLUID_FIELDS: [&str; 7] = [
    "id",
    "material",
    "flow_range",
    "waterlogs_at",
    "tick_rate",
    "renews_from",
    "color",
];

/// Fields `register_block` accepts. Anything else is an error naming the field.
const BLOCK_FIELDS: [&str; 9] = [
    "id",
    "name",
    "drops",
    "hardness",
    "dominance",
    "description",
    "tags",
    "textures",
    "light_emit",
];

/// Keys the `textures` sub-table accepts.
///
/// Only `all` for now. Per-face textures (`top`, `sides`, …) are a natural
/// extension and deliberately not guessed at here: adding them later is
/// additive, while shipping a six-key schema nothing renders yet would freeze a
/// guess into the mod API.
const TEXTURE_FIELDS: [&str; 1] = ["all"];

/// Reads and validates `textures = { all = "..." }`.
fn block_texture_path(block: &str, textures: &Table) -> mlua::Result<String> {
    for pair in textures.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !TEXTURE_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_block(\"{block}\"): unknown texture key `{name}`. Only `all` is \
                     supported; per-face textures are not implemented yet."
                )));
            }
        }
    }

    let path: String = textures.get("all").map_err(|_| {
        mlua::Error::external(format!(
            "register_block(\"{block}\"): `textures` must have an `all` key naming a file, e.g. \
             textures = {{ all = \"white.png\" }}"
        ))
    })?;

    validate_texture_path(block, &path).map_err(mlua::Error::external)
}

/// Checks a mod-supplied asset path and normalises its separators.
///
/// The path is never joined onto the filesystem — it is a key into the content
/// index, which was built by walking the mod's own directory, so a path
/// pointing outside simply fails to match. Refusing it here anyway turns a
/// silent missing texture into a startup error naming the mod, and means the
/// rule is stated somewhere rather than being an accident of how the index
/// happens to be keyed.
fn validate_texture_path(block: &str, path: &str) -> Result<String, String> {
    let normalised = path.replace('\\', "/");
    let refuse = |why: &str| {
        Err(format!(
            "register_block(\"{block}\"): texture path `{path}` {why}. Paths are relative to your \
             mod's own directory."
        ))
    };

    if normalised.trim().is_empty() {
        return refuse("is empty");
    }
    if normalised.starts_with('/') || normalised.contains(':') {
        return refuse("is absolute");
    }
    if normalised.split('/').any(|segment| segment == "..") {
        return refuse("escapes the mod directory");
    }
    Ok(normalised)
}

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

    /// A fluid store a test can watch, and the smallest one that can be.
    #[derive(Default)]
    struct Bucket {
        held: std::sync::Mutex<std::collections::BTreeMap<(i32, i32, i32), crate::fluid::Fluid>>,
    }

    impl crate::fluid::Access for Bucket {
        fn fluid_at(&self, pos: crate::BlockPos) -> crate::fluid::Fluid {
            self.held
                .lock()
                .ok()
                .and_then(|held| held.get(&(pos.x, pos.y, pos.z)).copied())
                .unwrap_or(crate::fluid::Fluid::EMPTY)
        }

        fn set_fluid_at(&self, pos: crate::BlockPos, value: crate::fluid::Fluid) -> bool {
            let Ok(mut held) = self.held.lock() else {
                return false;
            };
            if value.is_empty() {
                held.remove(&(pos.x, pos.y, pos.z)).is_some()
            } else {
                held.insert((pos.x, pos.y, pos.z), value) != Some(value)
            }
        }

        fn fluid_id(&self, name: &str) -> Option<crate::fluid::FluidId> {
            (name == "test:milk").then_some(crate::fluid::FluidId(1))
        }
    }

    /// An edit queue a test can look inside.
    #[derive(Default)]
    struct Slate {
        written: std::sync::Mutex<Vec<(crate::BlockPos, String)>>,
    }

    impl crate::script::WorldEdit for Slate {
        fn set_block(&self, pos: crate::BlockPos, block: &str) -> bool {
            // Refuses one name, so the test can see a rejection travel back to
            // Lua rather than assuming it does.
            if block == "nobody:registered" {
                return false;
            }
            self.written
                .lock()
                .map(|mut written| written.push((pos, block.to_owned())))
                .is_ok()
        }
    }

    #[test]
    fn a_mod_can_change_the_world_after_worldgen() {
        // **The gap this closed.** A mod could write terrain during worldgen and
        // never again, which made a block that reacts to anything — fluid
        // arriving, a crop growing, a fire spreading — inexpressible through the
        // only API the engine is supposed to have (charter rule 1).
        let mut vm = vm();
        let slate = std::sync::Arc::new(Slate::default());
        vm.set_world_edit(
            std::sync::Arc::clone(&slate) as std::sync::Arc<dyn crate::script::WorldEdit>
        );
        load(
            &mut vm,
            "mason",
            "ok = nil\n\
             refused = nil\n\
             game.register_on_tick(function()\n\
               ok = game.set_block({x=4,y=5,z=6}, 'core:white')\n\
               refused = game.set_block({x=0,y=0,z=0}, 'nobody:registered')\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();
        let faults = vm.tick(1).expect("tick");
        assert!(faults.is_empty(), "setting a block raised: {faults:?}");

        let written = slate.written.lock().expect("slate");
        assert_eq!(
            written.as_slice(),
            &[(crate::BlockPos::new(4, 5, 6), "core:white".to_owned())],
            "the edit did not reach the queue, or an unregistered name did"
        );
    }

    #[test]
    fn clearing_fluid_needs_no_fluid_named() {
        // **The bug this exists for cost a whole play session.** The stubs
        // document `set_fluid(pos, {level = 0})` as the way to scoop, and the
        // implementation refused it as a missing `fluid` field. A hook that
        // errors disables its mod (charter rule 10), so the reference mod's
        // scoop killed `core_milk` on its first use and every placement
        // afterwards silently did nothing — reported as "no way to destroy the
        // source" AND "after a while it just gives up", which were one bug.
        //
        // One mod doing both, because registration closes at freeze and a
        // second mod cannot be loaded after it.
        let mut vm = vm();
        let bucket = std::sync::Arc::new(Bucket::default());
        vm.set_fluid_access(
            std::sync::Arc::clone(&bucket) as std::sync::Arc<dyn crate::fluid::Access>
        );
        load(
            &mut vm,
            "pourer",
            "turn = 0\n\
             game.register_on_tick(function()\n\
               turn = turn + 1\n\
               if turn == 1 then\n\
                 game.set_fluid({x=1,y=2,z=3}, {fluid='test:milk', source=true})\n\
               else\n\
                 game.set_fluid({x=1,y=2,z=3}, {level=0})\n\
               end\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();

        let faults = vm.tick(1).expect("tick");
        assert!(faults.is_empty(), "pouring raised: {faults:?}");
        let at = crate::BlockPos::new(1, 2, 3);
        assert!(
            !crate::fluid::Access::fluid_at(&*bucket, at).is_empty(),
            "the pour did not land, so the clear below would prove nothing"
        );

        // And now the call the stubs promise works.
        let faults = vm.tick(1).expect("tick");
        assert!(
            crate::fluid::Access::fluid_at(&*bucket, at).is_empty(),
            "clearing without naming a fluid did nothing, which is the bug"
        );
        assert!(
            faults.is_empty(),
            "clearing raised an error, which is what disables a mod: {faults:?}"
        );
    }

    #[test]
    fn a_registered_on_tick_callback_runs_every_tick() {
        let mut vm = vm();
        load(
            &mut vm,
            "counter",
            "count = 0\ngame.register_on_tick(function(dt) count = count + dt end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(vm.tick(1).expect("tick").is_empty());
        assert!(vm.tick(1).expect("tick").is_empty());
        vm.eval_in("counter", "assert(count == 2, 'count is ' .. count)")
            .expect("the callback should have run twice");
    }

    #[test]
    fn the_tick_callback_receives_the_step_count_not_a_duration() {
        // Mods get a count of simulation steps, never wall-clock time: a
        // duration would let a mod scale behaviour by how fast the machine is,
        // and two servers would then produce different worlds.
        let mut vm = vm();
        load(
            &mut vm,
            "counter",
            "seen = nil\ngame.register_on_tick(function(dt) seen = dt end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        vm.tick(3).expect("tick");
        vm.eval_in("counter", "assert(seen == 3, 'got ' .. tostring(seen))")
            .expect("the callback should see the catch-up count");
        vm.eval_in("counter", "assert(math.type(seen) == 'integer')")
            .expect("a step count is an integer, not a float duration");
    }

    /// A dig event for a fixed player and cell.
    fn a_dig() -> crate::script::DigEvent {
        crate::script::DigEvent {
            player: [0xAB; 32],
            target: crate::coords::SubNodePos::new(3, 4, 5),
            material: MaterialId(7),
            brush: Brush::SubNode,
        }
    }

    /// A placement event for a fixed player and block.
    fn a_place() -> crate::script::PlaceEvent {
        crate::script::PlaceEvent {
            player: [0xCD; 32],
            block: crate::coords::BlockPos::new(1, 2, 3),
            material: MaterialId(7),
            occupancy: 0b111,
            units: 3,
        }
    }

    #[test]
    fn a_mod_can_refuse_a_dig_and_a_placement() {
        let mut vm = vm();
        load(
            &mut vm,
            "warden",
            "game.register_on_dig_complete(function() return false end)\n\
             game.register_on_place(function() return false end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(!vm.dig_complete(&a_dig()).allowed, "the veto was ignored");
        assert!(!vm.place(&a_place()).allowed, "the veto was ignored");
    }

    #[test]
    fn a_fluid_declares_what_it_looks_like_from_inside() {
        // The tint over a submerged camera, and a mod's decision rather than the
        // engine's: a texture is what the SURFACE looks like from outside, and
        // clear water has a vivid surface with a faint tint.
        let mut vm = vm();
        load(
            &mut vm,
            "dairy",
            "game.register_block{ id = 'milk' }\n\
             game.register_block{ id = 'oil' }\n\
             game.register_fluid{ id = 'milk', material = 'milk', \
               color = { r = 245, g = 243, b = 232 } }\n\
             game.register_fluid{ id = 'oil', material = 'oil' }",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let fluids = vm.registered_fluids();
        let milk = fluids
            .iter()
            .find(|rule| rule.fluid == "dairy:milk")
            .expect("milk");
        assert_eq!(milk.color, [245, 243, 232]);

        // And white when a mod says nothing, which is a tint it will notice
        // rather than a transparent one it will not.
        let oil = fluids
            .iter()
            .find(|rule| rule.fluid == "dairy:oil")
            .expect("oil");
        assert_eq!(oil.color, [255, 255, 255]);
    }

    #[test]
    fn a_colour_channel_outside_a_byte_is_refused_by_name() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "dairy",
            "game.register_block{ id = 'milk' }\n\
             game.register_fluid{ id = 'milk', material = 'milk', color = { g = 300 } }",
        )
        .expect_err("300 is not a channel");
        // `Display` names the mod and the file, which is what an operator reads
        // first; the detail the mod author needs is in the `Debug` form.
        let text = format!("{err:?}");
        assert!(
            text.contains("color.g") && text.contains("300"),
            "the error should name the field and the value: {text}"
        );
    }

    #[test]
    fn a_hook_that_returns_nothing_is_observing_rather_than_voting() {
        // If a bare `return` cancelled, every mod author who forgot one would
        // silently make the world unbreakable — and the engine could not tell
        // that apart from a deliberate veto.
        let mut vm = vm();
        load(
            &mut vm,
            "watcher",
            "seen = 0\n\
             game.register_on_dig_complete(function() seen = seen + 1 end)\n\
             game.register_on_place(function() seen = seen + 1 return true end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(vm.dig_complete(&a_dig()).allowed, "a bare return cancelled");
        assert!(vm.place(&a_place()).allowed, "a truthy return cancelled");
        vm.eval_in("watcher", "assert(seen == 2, 'seen is ' .. seen)")
            .expect("both hooks should have run");
    }

    #[test]
    fn what_a_hook_returns_says_whether_the_player_hears_about_it() {
        // **Cancelling has two meanings and the engine could not tell them
        // apart.** Refusing a player is one; HANDLING the action yourself is the
        // other — `core_milk` pours milk by intercepting a placement and
        // cancelling the block write, and was answered with "you cannot build
        // there" every single time it worked.
        let mut vm = vm();
        load(
            &mut vm,
            "refuser",
            "game.register_on_place(function(event)\n\
               if event.x == 1 then return false end\n\
               if event.x == 2 then return 'this land is claimed' end\n\
               return ''\n\
             end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let at = |x: i32| {
            let mut event = a_place();
            event.block = crate::coords::BlockPos::new(x, 0, 0);
            event
        };

        // `false` — refused, and the caller's own wording is what is shown.
        let outcome = vm.place(&at(1));
        assert!(!outcome.allowed);
        assert_eq!(
            outcome.notice("you cannot build there"),
            Some("you cannot build there")
        );

        // A string — refused, in the mod's words rather than the engine's.
        let outcome = vm.place(&at(2));
        assert!(!outcome.allowed);
        assert_eq!(
            outcome.notice("you cannot build there"),
            Some("this land is claimed")
        );

        // Empty — cancelled, and the player hears nothing. The milk poured.
        let outcome = vm.place(&at(3));
        assert!(!outcome.allowed, "an empty string did not cancel");
        assert_eq!(
            outcome.notice("you cannot build there"),
            None,
            "a mod that handled the placement itself was reported to the player as a refusal"
        );
    }

    #[test]
    fn a_refusal_a_mod_wrote_is_bounded_before_it_reaches_a_player() {
        // A mod is not hostile, but it can be buggy, and this ends up on the
        // wire. Truncated on a character boundary, because slicing UTF-8
        // anywhere else panics — and a mod is free to refuse in any language.
        let mut vm = vm();
        load(
            &mut vm,
            "shouty",
            "game.register_on_place(function() return string.rep('никак', 4000) end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let outcome = vm.place(&a_place());
        assert!(!outcome.allowed);
        let reason = outcome.reason.expect("a reason");
        assert!(
            reason.len() <= crate::script::MAX_REFUSAL_BYTES,
            "a mod put {} bytes in front of a player",
            reason.len()
        );
        assert!(!reason.is_empty(), "the whole message was truncated away");
    }

    #[test]
    fn a_hook_sees_the_event_it_is_deciding_about() {
        // A veto is only useful if the mod can tell WHAT it is vetoing. The
        // uuid is hex rather than raw bytes so it can be used as a table key
        // and printed in the same breath (charter rule 13 keys on the uuid).
        let mut vm = vm();
        load(
            &mut vm,
            "inspector",
            "dug = nil placed = nil\n\
             game.register_on_dig_complete(function(e) dug = e end)\n\
             game.register_on_place(function(e) placed = e end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        vm.dig_complete(&a_dig());
        vm.place(&a_place());
        vm.eval_in(
            "inspector",
            "assert(dug.x == 3 and dug.y == 4 and dug.z == 5, 'dig position')\n\
             assert(dug.material == 7, 'dig material')\n\
             assert(dug.brush == 'subnode', 'dig brush is ' .. tostring(dug.brush))\n\
             assert(#dug.player == 64, 'uuid should be 32 bytes of hex')\n\
             assert(placed.x == 1 and placed.y == 2 and placed.z == 3, 'place position')\n\
             assert(placed.units == 3, 'units')\n\
             assert(placed.occupancy == 7, 'occupancy')",
        )
        .expect("the hooks should have received the event fields");
    }

    #[test]
    fn a_blocked_flow_reaches_a_mod_with_what_is_in_the_way() {
        // What `on_fluid_flow` exists for: a mod cannot see a flow that did not
        // happen any other way, because a block milk cannot enter is a block
        // with no milk in it and looks exactly like one milk never reached.
        let mut vm = vm();
        load(
            &mut vm,
            "sponge",
            "seen = nil\n\
             game.register_block{ id = \"rock\" }\n\
             game.register_on_fluid_flow(function(e) seen = e end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let rock = vm
            .registered_blocks()
            .into_iter()
            .find(|(name, _)| name == "sponge:rock")
            .map(|(_, id)| id)
            .expect("rock should be registered");

        let outcome = vm.fluid_flow(&crate::script::FluidFlowEvent {
            from: crate::coords::BlockPos::new(1, 2, 3),
            into: crate::coords::BlockPos::new(2, 2, 3),
            fluid: "core_milk:milk".to_owned(),
            level: 5,
            blocked_by: rock,
            // Three cells filled, which is what `units` must come to.
            occupancy: 0b111,
        });
        assert!(outcome.faults.is_empty(), "{:?}", outcome.faults);

        vm.eval_in(
            "sponge",
            "assert(seen.from.x == 1 and seen.from.y == 2 and seen.from.z == 3, 'from')\n\
             assert(seen.into.x == 2 and seen.into.y == 2 and seen.into.z == 3, 'into')\n\
             assert(seen.fluid == 'core_milk:milk', 'fluid is ' .. tostring(seen.fluid))\n\
             assert(seen.level == 5, 'level')\n\
             assert(seen.occupancy == 7, 'occupancy')\n\
             assert(seen.units == 3, 'units is ' .. tostring(seen.units))\n\
             assert(seen.block == 'sponge:rock', 'block is ' .. tostring(seen.block))",
        )
        .expect("the hook should have received the event fields");
    }

    #[test]
    fn a_mod_that_throws_in_on_fluid_flow_is_disabled_rather_than_killing_the_tick() {
        // Charter rule 10. Nothing is being vetoed here — the flow already
        // failed — so the only thing an error can do is take the mod down with
        // it, which is exactly what it must do and no more.
        let mut vm = vm();
        load(
            &mut vm,
            "broken",
            "game.register_on_fluid_flow(function() error('boom') end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let outcome = vm.fluid_flow(&crate::script::FluidFlowEvent {
            from: crate::coords::BlockPos::new(0, 0, 0),
            into: crate::coords::BlockPos::new(1, 0, 0),
            fluid: "core_milk:milk".to_owned(),
            level: 7,
            blocked_by: MaterialId::UNKNOWN,
            occupancy: crate::block::OCCUPANCY_FULL,
        });
        assert_eq!(outcome.faults.len(), 1, "the mod should have been faulted");
        assert_eq!(outcome.faults[0].0, "broken");
    }

    #[test]
    fn a_mod_that_throws_while_vetoing_is_disabled_and_the_action_proceeds() {
        // Charter rule 10's sharpest edge. If a crash counted as a refusal, one
        // broken mod would stop everybody on the server from digging — a much
        // worse outcome than whatever it was trying to prevent.
        let mut vm = vm();
        load(
            &mut vm,
            "broken",
            "game.register_on_dig_complete(function() error('boom') end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let outcome = vm.dig_complete(&a_dig());
        assert!(
            outcome.allowed,
            "a mod's crash was treated as a refusal, which lets one bad mod stop the server"
        );
        assert_eq!(outcome.faults.len(), 1, "the fault was not reported");
        assert_eq!(outcome.faults[0].0, "broken");

        // And it is disabled from then on, so it cannot fault every tick.
        let again = vm.dig_complete(&a_dig());
        assert!(again.allowed);
        assert!(
            again.faults.is_empty(),
            "a disabled mod ran again and faulted again"
        );
    }

    #[test]
    fn a_veto_stops_the_hooks_after_it() {
        // Documented behaviour, and the reason for it: once the dig is not
        // happening, a later hook running would be invited to take side effects
        // for an action that will not occur.
        let mut vm = vm();
        load(
            &mut vm,
            "first",
            "game.register_on_dig_complete(function() return false end)",
        )
        .expect("load");
        load(
            &mut vm,
            "second",
            "ran = false\ngame.register_on_dig_complete(function() ran = true end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(!vm.dig_complete(&a_dig()).allowed);
        vm.eval_in("second", "assert(ran == false, 'a hook ran after a veto')")
            .expect("the second hook must not have run");
    }

    #[test]
    fn every_hook_runs_when_nobody_objects() {
        // The counter-example to the test above: short-circuiting must happen
        // ONLY on a refusal, or the second mod would never run at all and that
        // test would pass for the wrong reason.
        let mut vm = vm();
        load(
            &mut vm,
            "first",
            "game.register_on_dig_complete(function() end)",
        )
        .expect("load");
        load(
            &mut vm,
            "second",
            "ran = false\ngame.register_on_dig_complete(function() ran = true end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(vm.dig_complete(&a_dig()).allowed);
        vm.eval_in("second", "assert(ran == true, 'the second hook never ran')")
            .expect("both hooks should have run");
    }

    #[test]
    fn the_punch_hook_works_even_though_nothing_calls_it_yet() {
        // Entities are Task 12, so there is nothing to punch and no caller.
        // Registration and dispatch exist and are tested now so that task adds
        // a CALLER rather than an API — and so this does not arrive untested
        // and half-remembered when it is finally needed.
        let mut vm = vm();
        load(
            &mut vm,
            "referee",
            "seen = nil\ngame.register_on_punch(function(e) seen = e return false end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let event = crate::script::PunchEvent {
            attacker: [0x11; 32],
            target: [0x22; 32],
        };
        assert!(!vm.punch(&event).allowed, "the veto was ignored");
        vm.eval_in(
            "referee",
            "assert(seen.attacker == string.rep('11', 32), 'attacker')\n\
             assert(seen.target == string.rep('22', 32), 'target')\n\
             assert(seen.attacker ~= seen.target, 'the two parties must be distinguishable')",
        )
        .expect("the hook should see both parties");
    }

    #[test]
    fn registering_a_hook_after_freeze_is_refused() {
        // Charter rule 9: the registration window closes and `register_*`
        // becomes a hard error. A hook registered after freeze would be a
        // callback the engine never learned about, or worse, one it did.
        let mut vm = vm();
        load(&mut vm, "late", "").expect("load");
        vm.freeze().expect("freeze");

        assert!(
            vm.eval_in("late", "game.register_on_place(function() end)")
                .is_err(),
            "a hook was registered after the registries froze"
        );
    }

    #[test]
    fn a_server_with_no_hooks_allows_everything() {
        // The ordinary case, and the one that must not cost anything: a world
        // whose mods have no opinion about digging is a world you can dig in.
        let mut vm = vm();
        load(&mut vm, "quiet", "").expect("load");
        vm.freeze().expect("freeze");

        assert!(vm.dig_complete(&a_dig()).allowed);
        assert!(vm.place(&a_place()).allowed);
    }

    #[test]
    fn a_failing_mod_is_disabled_without_stopping_the_others() {
        // Charter rule 10, and the precise shape of it: the mods registered
        // AFTER the failing one must still run. Returning at the first error
        // would starve them, and the symptom would be "my mod stopped working"
        // with nothing pointing at the real cause.
        let mut vm = vm();
        load(
            &mut vm,
            "first",
            "ran = 0\ngame.register_on_tick(function() ran = ran + 1 end)",
        )
        .expect("load");
        load(
            &mut vm,
            "bad",
            "game.register_on_tick(function() error('boom') end)",
        )
        .expect("load");
        load(
            &mut vm,
            "last",
            "ran = 0\ngame.register_on_tick(function() ran = ran + 1 end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let faults = vm.tick(1).expect("the tick itself must not fail");
        assert_eq!(faults.len(), 1, "exactly one mod should have faulted");
        assert_eq!(faults[0].0, "bad");

        vm.eval_in("first", "assert(ran == 1)").expect("first ran");
        vm.eval_in(
            "last",
            "assert(ran == 1, 'the mod after the failing one was starved')",
        )
        .expect("last ran");

        // And the faulted mod stays disabled rather than erroring every tick.
        let faults = vm.tick(1).expect("tick");
        assert!(faults.is_empty(), "a disabled mod must not re-report");
        assert!(vm.faulted_mods().contains(&"bad".to_owned()));
        vm.eval_in("last", "assert(ran == 2)")
            .expect("still ticking");
    }

    #[test]
    fn registering_a_tick_callback_after_freeze_is_refused() {
        // Charter rule 9: the registration window closes.
        let mut vm = vm();
        load(&mut vm, "late", "").expect("load");
        vm.freeze().expect("freeze");

        assert!(
            vm.eval_in("late", "game.register_on_tick(function() end)")
                .is_err(),
            "registration after freeze must be a hard error"
        );
    }

    #[test]
    fn a_tick_with_no_registered_callbacks_is_harmless() {
        let mut vm = vm();
        load(&mut vm, "quiet", "").expect("load");
        vm.freeze().expect("freeze");
        assert!(vm.tick(1).expect("tick").is_empty());
    }

    #[test]
    fn registered_blocks_come_back_ordered_by_id_not_by_name() {
        // The order IS the contract. The host replays these into a registry
        // that assigns ids sequentially, so alphabetical order would give every
        // block a different id than its mod was handed — and every block that
        // mod placed would silently be the wrong material.
        let mut vm = vm();
        load(
            &mut vm,
            "zeta",
            "game.register_block{ id = 'zzz' }\ngame.register_block{ id = 'aaa' }",
        )
        .expect("load");
        load(&mut vm, "alpha", "game.register_block{ id = 'mmm' }").expect("load");

        let blocks = vm.registered_blocks();
        let names: Vec<&str> = blocks.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec!["zeta:zzz", "zeta:aaa", "alpha:mmm"],
            "blocks must come back in registration order, not alphabetical"
        );

        // The ids must be contiguous from the first mod id, so replaying them
        // into a fresh Registry reproduces exactly these numbers.
        let ids: Vec<u16> = blocks.iter().map(|(_, id)| id.0).collect();
        assert_eq!(ids, vec![2, 3, 4]);

        let mut registry = crate::material::Registry::new();
        for (name, expected) in &blocks {
            assert_eq!(
                registry.register(name).expect("register"),
                *expected,
                "replaying `{name}` gave it a different id than the VM did"
            );
        }
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
    fn a_block_texture_travels_with_the_mod_that_registered_it() {
        // The path is relative to the mod's own directory, so the mod id has to
        // come with it — nothing downstream can recover which directory to look
        // in from the block id alone.
        let mut vm = vm();
        load(
            &mut vm,
            "paint",
            "game.register_block{ id = 'red', textures = { all = 'textures/red.png' } }",
        )
        .expect("load");

        let textures = vm.registered_block_textures();
        assert_eq!(textures.len(), 1);
        assert_eq!(textures[0].block, "paint:red");
        assert_eq!(textures[0].mod_id, "paint");
        assert_eq!(textures[0].path, "textures/red.png");
    }

    #[test]
    fn a_block_with_no_texture_has_no_entry() {
        // Not an empty string, not a placeholder path. The engine has no
        // opinion about what an untextured block looks like; that is the
        // client's business, and encoding a guess here would make it the
        // engine's.
        let mut vm = vm();
        load(&mut vm, "t", "game.register_block{ id = 'plain' }").expect("load");
        assert!(vm.registered_block_textures().is_empty());
    }

    #[test]
    fn block_textures_come_back_in_material_id_order() {
        // Lua table iteration order is unspecified, and this list goes on the
        // wire. An order that depended on the table would make two identical
        // servers look different to a client.
        let mut vm = vm();
        load(
            &mut vm,
            "t",
            "for _, id in ipairs{'zulu', 'alpha', 'mike'} do
                 game.register_block{ id = id, textures = { all = id .. '.png' } }
             end",
        )
        .expect("load");

        let ids = vm.block_ids();
        let textures = vm.registered_block_textures();
        let numbered: Vec<u16> = textures
            .iter()
            .map(|texture| ids[&texture.block].0)
            .collect();
        let mut sorted = numbered.clone();
        sorted.sort_unstable();
        assert_eq!(numbered, sorted, "textures must come back ordered by id");
    }

    #[test]
    fn a_texture_path_may_not_escape_the_mod_directory() {
        // The index this path is looked up in is keyed by mod-relative paths,
        // so an escaping path would simply fail to match — but refusing it here
        // turns a silently missing texture into a startup error naming the mod.
        for bad in ["../../etc/passwd", "/etc/passwd", "C:/windows/win.ini", ""] {
            let mut vm = vm();
            let err = load(
                &mut vm,
                "t",
                &format!("game.register_block{{ id = 'a', textures = {{ all = '{bad}' }} }}"),
            )
            .expect_err("`{bad}` should be refused");
            assert!(
                err.to_string().contains('t'),
                "the error should attribute the mod: {err}"
            );
        }
    }

    #[test]
    fn a_bad_texture_spec_leaves_the_registry_untouched() {
        // Parsed before anything is registered. Registering the block and then
        // failing on the texture would leave a material id assigned to a block
        // the mod does not believe exists.
        let mut vm = vm();
        let _ = load(
            &mut vm,
            "t",
            "game.register_block{ id = 'a', textures = { all = '../escape.png' } }",
        );
        assert!(
            vm.block_ids().is_empty(),
            "a rejected registration must not have half-happened: {:?}",
            vm.block_ids()
        );
    }

    #[test]
    fn an_unknown_texture_key_says_which_keys_exist() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "t",
            "game.register_block{ id = 'a', textures = { top = 'x.png' } }",
        )
        .expect_err("only `all` is supported");
        // The detail rather than the Display string: a script error displays as
        // "mod `t` errored in init.lua" and carries the backend's message
        // separately, so a mod author sees both.
        let (ScriptError::Load { detail, .. } | ScriptError::Runtime { detail, .. }) = &err else {
            panic!("expected a script error carrying detail, got {err:?}");
        };
        assert!(
            detail.contains("top") && detail.contains("all"),
            "the error should name the offending key and the supported one: {detail}"
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
        let blocks = vm.block_ids();
        assert!(blocks.contains_key("mymod:white"), "{blocks:?}");
        assert!(blocks["mymod:white"].get() >= 2, "reserved ids are 0 and 1");
    }
}

#[cfg(test)]
mod dig_rules_tests {
    use std::path::Path;

    use super::*;

    fn vm() -> MluaVm {
        MluaVm::new(VmLimits::default()).expect("create vm")
    }

    fn load(vm: &mut MluaVm, id: &str, source: &str) -> Result<(), ScriptError> {
        vm.load_mod(id, source, Path::new("."))
    }

    /// The backend's message, which is where a rejection explains itself.
    ///
    /// `ScriptError`'s `Display` deliberately says only which mod failed and
    /// where — that line goes in a server log next to fifty others. The reason
    /// a mod author needs is in `detail`, so that is what these assert on.
    fn detail_of(err: &ScriptError) -> String {
        match err {
            ScriptError::Vm { detail, .. }
            | ScriptError::Load { detail, .. }
            | ScriptError::Runtime { detail, .. } => detail.clone(),
            other => format!("{other}"),
        }
    }

    #[test]
    fn a_block_that_says_nothing_still_has_a_hardness() {
        // The absent case is the common one, and it must not be special. A mod
        // that forgot `hardness` should get a breakable block, not bedrock.
        let mut vm = vm();
        load(&mut vm, "core", r#"game.register_block{ id = "plain" }"#).expect("load");

        let rules = vm.registered_block_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].block, "core:plain");
        assert!(
            (rules[0].hardness - BlockRules::DEFAULT_HARDNESS).abs() < 1e-6,
            "got {}",
            rules[0].hardness
        );
        assert!(rules[0].drops.is_none(), "no override means the usual rule");
    }

    #[test]
    fn hardness_and_drops_reach_the_engine() {
        let mut vm = vm();
        load(
            &mut vm,
            "core",
            r#"
            game.register_block{ id = "stone", hardness = 2.5 }
            game.register_block{ id = "ore", hardness = 4.0, drops = { gem = 3 } }
            "#,
        )
        .expect("load");

        let rules = vm.registered_block_rules();
        assert_eq!(rules.len(), 2);
        assert!((rules[0].hardness - 2.5).abs() < 1e-6);
        assert_eq!(rules[0].drops, None);
        assert!((rules[1].hardness - 4.0).abs() < 1e-6);
        assert_eq!(
            rules[1].drops.as_deref(),
            Some(&[("core:gem".to_owned(), 3)][..]),
            "a bare drop id is qualified with the registering mod's namespace"
        );
    }

    #[test]
    fn dominance_reaches_the_engine_and_defaults_to_neutral() {
        let mut vm = vm();
        load(
            &mut vm,
            "core",
            r#"
            game.register_block{ id = "plain", hardness = 1.5 }
            game.register_block{ id = "dirt", hardness = 0.5, dominance = 3.0 }
            "#,
        )
        .expect("load");

        let rules = vm.registered_block_rules();
        assert!(
            (rules[0].dominance - crate::dig::Resistance::DEFAULT_DOMINANCE).abs() < 1e-6,
            "a block that says nothing should pull its own weight and no more, got {}",
            rules[0].dominance
        );
        assert!((rules[1].dominance - 3.0).abs() < 1e-6);

        // And the two travel together into the blend, which is the whole point
        // of the field existing on `BlockRules` rather than beside it.
        assert!((rules[1].resistance().hardness - 0.5).abs() < 1e-6);
        assert!((rules[1].resistance().dominance - 3.0).abs() < 1e-6);
    }

    #[test]
    fn a_dominance_of_zero_is_refused_rather_than_clamped() {
        // Unlike hardness, where zero is meaningful. A material with no say at
        // all in a mixture leaves a block made entirely of such materials with
        // nothing to average — so this is refused at registration, where the
        // error can name the mod and the block.
        let mut zero = vm();
        let err = load(
            &mut zero,
            "core",
            r#"game.register_block{ id = "ghost", dominance = 0 }"#,
        )
        .expect_err("a dominance of zero should be refused");
        assert!(
            detail_of(&err).contains("dominance"),
            "the error should name the field: {}",
            detail_of(&err)
        );

        let mut negative = vm();
        assert!(
            load(
                &mut negative,
                "core",
                r#"game.register_block{ id = "odd", dominance = -2 }"#,
            )
            .is_err(),
            "a negative dominance should be refused too"
        );
    }

    #[test]
    fn the_rules_come_back_in_material_id_order() {
        // The same contract `registered_blocks` carries: this list reaches the
        // simulation, and Lua's table iteration order is unspecified.
        let mut vm = vm();
        load(
            &mut vm,
            "core",
            r#"
            game.register_block{ id = "zeta" }
            game.register_block{ id = "alpha" }
            game.register_block{ id = "mu" }
            "#,
        )
        .expect("load");

        let rules = vm.registered_block_rules();
        let order: Vec<&str> = rules.iter().map(|entry| entry.block.as_str()).collect();
        assert_eq!(
            order,
            vec!["core:zeta", "core:alpha", "core:mu"],
            "registration order is id order, not alphabetical"
        );
    }

    #[test]
    fn a_block_emits_nothing_unless_its_mod_says_so() {
        // Charter rule 1 for light sources: the engine has no lamps of its own,
        // so a world whose mods register none is lit only by the sky.
        let mut vm = vm();
        load(&mut vm, "core", r#"game.register_block{ id = "plain" }"#).expect("load");

        let rules = vm.registered_block_rules();
        assert_eq!(rules[0].light_emit, (0, 0, 0));
        assert!(rules[0].emission().is_dark());
    }

    #[test]
    fn a_mod_can_register_a_coloured_lamp() {
        let mut vm = vm();
        load(
            &mut vm,
            "core",
            r#"
            game.register_block{ id = "torch", light_emit = { r = 15, g = 9, b = 2 } }
            game.register_block{ id = "moonstone", light_emit = { b = 12 } }
            "#,
        )
        .expect("load");

        let rules = vm.registered_block_rules();
        assert_eq!(rules[0].light_emit, (15, 9, 2));
        // An omitted channel is zero, so this is a blue lamp and not a white
        // one — the difference between "unset" and "full" for a mod that wrote
        // only the channel it cared about.
        assert_eq!(rules[1].light_emit, (0, 0, 12));

        // And the packed form the simulation actually uses keeps the sun
        // channel clear: a block emits colour, daylight comes from the sky.
        let emission = rules[0].emission();
        assert_eq!(emission.red(), 15);
        assert_eq!(emission.green(), 9);
        assert_eq!(emission.blue(), 2);
        assert_eq!(emission.sun(), 0, "a block emitted sunlight");
    }

    #[test]
    fn a_light_level_out_of_range_is_refused_rather_than_clamped() {
        // Same reasoning as hardness: clamping leaves a mod believing it set
        // something. A lamp quietly dimmer than its author asked for is a bug
        // they would chase in the wrong place.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core",
            r#"game.register_block{ id = "toobright", light_emit = { r = 30 } }"#,
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("light_emit.r"),
            "the error should name the channel: {err:?}"
        );
    }

    #[test]
    fn a_negative_hardness_is_refused_rather_than_clamped() {
        // Clamping would leave a mod believing it had set something.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core",
            r#"game.register_block{ id = "odd", hardness = -1 }"#,
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("hardness"),
            "the error should name the field: {err:?}"
        );
    }

    #[test]
    fn a_world_with_no_sky_mod_has_no_sky() {
        // Charter rule 1: the engine has no day. A world whose mods register
        // no sky is legitimate — it simply never changes — rather than being
        // given a default the engine invented.
        let mut vm = vm();
        load(&mut vm, "core", r#"game.register_block{ id = "plain" }"#).expect("load");
        assert!(vm.registered_sky().is_none());
    }

    #[test]
    fn a_mod_registers_the_length_of_a_day_and_its_colours() {
        let mut vm = vm();
        load(
            &mut vm,
            "core_sky",
            r"
            game.register_sky{
                day_length_ticks = 24000,
                keyframes = {
                    { time = 0.5, sky = {0.5, 0.7, 1.0}, sun = {1, 1, 1}, intensity = 1.0 },
                    { time = 0.0, sky = {0.0, 0.0, 0.05}, sun = {0.2, 0.2, 0.4}, intensity = 0.05 },
                },
            }
            ",
        )
        .expect("load");

        let sky = vm.registered_sky().expect("a sky was registered");
        assert_eq!(sky.mod_id, "core_sky");
        assert_eq!(sky.day_length_ticks, 24_000);
        // **Sorted by the engine, not trusted from the mod.** The client walks
        // these in order to interpolate, and an out-of-order list makes the sky
        // jump backwards partway through the day.
        assert_eq!(sky.keyframes.len(), 2);
        assert!((sky.keyframes[0].time - 0.0).abs() < 1e-6);
        assert!((sky.keyframes[1].time - 0.5).abs() < 1e-6);
        assert!((sky.keyframes[1].intensity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_day_of_no_ticks_is_refused() {
        // It would never advance, and a sky frozen at midnight is a bug report
        // rather than a design.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 0, keyframes = {
                { time = 0, sky = {0,0,0}, sun = {0,0,0}, intensity = 0 },
            } }",
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("day_length_ticks"),
            "the error should name the field: {err:?}"
        );
    }

    #[test]
    fn a_sky_with_no_keyframes_is_refused() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {} }",
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("keyframes"),
            "the error should name the field: {err:?}"
        );
    }

    #[test]
    fn a_keyframe_outside_the_day_is_refused_rather_than_clamped() {
        // Same reasoning as hardness and light_emit: clamping leaves a mod
        // believing it set something.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {
                { time = 1.5, sky = {0,0,0}, sun = {0,0,0}, intensity = 0 },
            } }",
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("time"),
            "the error should name the field: {err:?}"
        );
    }

    #[test]
    fn a_keyframe_that_says_nothing_about_grading_is_graded_not_at_all() {
        // The whole reason grading is additive: every world that predates it, and
        // every sky that does not care, must come out exactly as before.
        let mut vm = vm();
        load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {
                { time = 0.5, sky = {0.5, 0.7, 1.0}, sun = {1, 1, 1}, intensity = 1.0 },
            } }",
        )
        .expect("load");

        let sky = vm.registered_sky().expect("a sky");
        assert_eq!(sky.keyframes[0].grade, SkyGrade::NONE);
        assert!(sky.keyframes[0].grade.is_none());
    }

    #[test]
    fn a_keyframe_can_grade_one_knob_without_restating_the_others() {
        let mut vm = vm();
        load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {
                { time = 0.0, sky = {0,0,0}, sun = {0,0,0}, intensity = 0,
                  grade = { saturation = 0.5, tint = {0.9, 1.0, 1.2} } },
            } }",
        )
        .expect("load");

        let grade = vm.registered_sky().expect("a sky").keyframes[0].grade;
        assert!((grade.saturation - 0.5).abs() < 1e-6);
        assert!((grade.tint[2] - 1.2).abs() < 1e-6);
        // The five it did not mention keep their identities rather than zeroing.
        assert!((grade.exposure - 1.0).abs() < 1e-6);
        assert!((grade.contrast - 1.0).abs() < 1e-6);
        assert!((grade.gamma - 1.0).abs() < 1e-6);
        assert!(grade.offset.iter().all(|channel| channel.abs() < 1e-6));
        assert!(
            !grade.is_none(),
            "a grade that sets something is not the identity"
        );
    }

    #[test]
    fn a_gamma_of_zero_is_refused() {
        // It is a divide by nothing in the bake, and the frame comes out one
        // flat colour with nothing on screen to say why.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {
                { time = 0, sky = {0,0,0}, sun = {0,0,0}, intensity = 0,
                  grade = { gamma = 0 } },
            } }",
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("gamma"),
            "the error should name the field: {err:?}"
        );
    }

    #[test]
    fn a_misspelt_grade_field_is_refused_rather_than_ignored() {
        // The lesson `start_time` taught, one level down: a field the engine
        // silently ignores is a mod author staring at a setting that does
        // nothing.
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core_sky",
            r"game.register_sky{ day_length_ticks = 100, keyframes = {
                { time = 0, sky = {0,0,0}, sun = {0,0,0}, intensity = 0,
                  grade = { saturaton = 0.5 } },
            } }",
        )
        .expect_err("should refuse");
        assert!(
            detail_of(&err).contains("saturaton"),
            "the error should quote the typo: {err:?}"
        );
    }

    #[test]
    fn a_grade_outside_its_bounds_is_refused_rather_than_clamped() {
        // Same reasoning as hardness, light_emit and keyframe time: clamping
        // leaves a mod believing it set something.
        let mut vm = vm();
        for (field, spec) in [
            ("contrast", "grade = { contrast = 99 }"),
            ("tint", "grade = { tint = {0, 0, 99} }"),
            ("offset", "grade = { offset = {0, 0, -9} }"),
            ("tint", "grade = { tint = {1, 1} }"),
        ] {
            let err = load(
                &mut vm,
                "core_sky",
                &format!(
                    r"game.register_sky{{ day_length_ticks = 100, keyframes = {{
                        {{ time = 0, sky = {{0,0,0}}, sun = {{0,0,0}}, intensity = 0, {spec} }},
                    }} }}"
                ),
            )
            .expect_err("should refuse");
            assert!(
                detail_of(&err).contains(field),
                "the error for `{spec}` should name `{field}`: {err:?}"
            );
        }
    }

    #[test]
    fn a_tool_can_ask_for_a_subnode_brush() {
        // The whole point of the tool API. Sub-node resolution is only a real
        // feature if a mod can reach it without engine changes.
        let mut vm = vm();
        load(
            &mut vm,
            "core",
            r#"
            game.register_tool{ id = "hand" }
            game.register_tool{ id = "chisel", brush = "subnode", speed_multiplier = 0.5 }
            "#,
        )
        .expect("load");

        let tools = vm.registered_tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].id, "core:chisel");
        assert_eq!(tools[0].brush, Brush::SubNode);
        assert!((tools[0].speed_multiplier - 0.5).abs() < 1e-6);
        assert_eq!(tools[1].id, "core:hand");
        assert_eq!(
            tools[1].brush,
            Brush::Block,
            "a tool that says nothing removes a whole block"
        );
    }

    #[test]
    fn an_unknown_brush_is_refused_and_says_what_exists() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core",
            r#"game.register_tool{ id = "drill", brush = "sphere" }"#,
        )
        .expect_err("should refuse");
        let text = detail_of(&err);
        assert!(text.contains("sphere"), "should name the bad brush: {text}");
        assert!(
            text.contains("subnode"),
            "should say which brushes exist: {text}"
        );
    }

    #[test]
    fn a_zero_speed_tool_is_refused_because_it_would_never_finish() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core",
            r#"game.register_tool{ id = "useless", speed_multiplier = 0 }"#,
        )
        .expect_err("should refuse");
        assert!(detail_of(&err).contains("speed_multiplier"), "{err:?}");
    }

    #[test]
    fn a_typo_in_a_tool_field_names_the_field() {
        let mut vm = vm();
        let err = load(
            &mut vm,
            "core",
            r#"game.register_tool{ id = "chisel", brsh = "subnode" }"#,
        )
        .expect_err("should refuse");
        assert!(detail_of(&err).contains("brsh"), "{err:?}");
    }
}

#[cfg(test)]
mod entity_tests {
    use super::*;
    use crate::script::vm::{ScriptVm, VmLimits};
    use std::path::Path;

    fn vm() -> MluaVm {
        MluaVm::new(VmLimits::default()).expect("create vm")
    }

    fn load(vm: &mut MluaVm, id: &str, source: &str) -> Result<(), ScriptError> {
        vm.load_mod(id, source, Path::new("."))
    }

    /// A live entity store a test can watch.
    ///
    /// The real `Entities` behind a lock, not a stand-in — the point of these
    /// tests is that the Lua surface drives the actual store, and a fake would
    /// let the two agree about something neither does.
    #[derive(Default)]
    struct Menagerie {
        entities: std::sync::Mutex<crate::ent::Entities>,
    }

    impl crate::ent::Access for Menagerie {
        fn spawn(&self, entity: crate::ent::Entity) -> Option<crate::ent::EntityId> {
            self.entities.lock().ok().map(|mut e| e.spawn(entity))
        }

        fn despawn(&self, id: crate::ent::EntityId) -> bool {
            self.entities
                .lock()
                .is_ok_and(|mut e| e.despawn(id).is_some())
        }

        fn get(&self, id: crate::ent::EntityId) -> Option<crate::ent::Entity> {
            self.entities.lock().ok().and_then(|e| e.get(id).cloned())
        }

        fn patch(&self, id: crate::ent::EntityId, patch: &crate::ent::Patch) -> bool {
            self.entities
                .lock()
                .ok()
                .and_then(|mut e| e.get_mut(id).map(|entity| patch.apply(entity)))
                .unwrap_or(false)
        }

        fn within(
            &self,
            centre: [f64; 3],
            radius: f64,
            source: Option<&str>,
        ) -> Vec<crate::ent::EntityId> {
            let Ok(entities) = self.entities.lock() else {
                return Vec::new();
            };
            let centre = crate::ent::Transform::from_world(centre[0], centre[1], centre[2]);
            let cells = radius * f64::from(crate::SUBNODES_PER_AXIS);
            entities
                .within(&centre, cells as f32)
                .into_iter()
                .filter(|(id, _)| {
                    source
                        .is_none_or(|wanted| entities.get(*id).is_some_and(|e| e.source == wanted))
                })
                .map(|(id, _)| id)
                .collect()
        }
    }

    /// A VM with an entity store behind it, and the store to inspect.
    fn vm_with_entities() -> (MluaVm, std::sync::Arc<Menagerie>) {
        let mut vm = vm();
        let store = std::sync::Arc::new(Menagerie::default());
        vm.set_entity_access(
            std::sync::Arc::clone(&store) as std::sync::Arc<dyn crate::ent::Access>
        );
        (vm, store)
    }

    #[test]
    fn a_mod_spawns_reads_and_despawns_an_entity() {
        let (mut vm, store) = vm_with_entities();
        load(
            &mut vm,
            "keeper",
            "turn = 0\n\
             id = nil\n\
             seen = nil\n\
             game.register_on_tick(function()\n\
               turn = turn + 1\n\
               if turn == 1 then\n\
                 id = game.spawn_entity{ pos = {x=10.5,y=64,z=-3}, model='engine:humanoid', health=20 }\n\
               elseif turn == 2 then\n\
                 seen = game.entity(id)\n\
               else\n\
                 game.despawn_entity(id)\n\
               end\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();

        assert!(vm.tick(1).expect("tick").is_empty());
        assert_eq!(store.entities.lock().expect("lock").len(), 1);

        assert!(vm.tick(1).expect("tick").is_empty());
        // The mod read it back; check what it saw rather than trusting the call
        // returned something.
        vm.eval_in(
            "keeper",
            "assert(seen ~= nil, 'game.entity returned nil')\n\
             assert(math.abs(seen.pos.x - 10.5) < 0.05, 'x came back as ' .. seen.pos.x)\n\
             assert(math.abs(seen.pos.z + 3) < 0.05, 'z came back as ' .. seen.pos.z)\n\
             assert(seen.health == 20, 'health came back as ' .. tostring(seen.health))\n\
             assert(seen.source == 'keeper', 'source came back as ' .. seen.source)",
        )
        .expect("what the mod saw");

        assert!(vm.tick(1).expect("tick").is_empty());
        assert_eq!(store.entities.lock().expect("lock").len(), 0);
    }

    #[test]
    fn a_stale_id_reads_as_nothing_rather_than_as_its_successor() {
        // The generation check, from Lua. A mod holding the id of a mob that
        // died must get nil — not whatever moved into its slot, which is the
        // same slot, because the free list hands the lowest one back.
        let (mut vm, _store) = vm_with_entities();
        load(&mut vm, "keeper", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "keeper",
            "local first = game.spawn_entity{ pos = {x=0,y=0,z=0} }\n\
             game.despawn_entity(first)\n\
             local second = game.spawn_entity{ pos = {x=0,y=0,z=0} }\n\
             assert(game.entity(first) == nil, 'a stale id resolved')\n\
             assert(game.entity(second) ~= nil, 'the new entity did not resolve')\n\
             assert(game.despawn_entity(first) == false, 'a stale id despawned its successor')\n\
             assert(game.entity(second) ~= nil, 'and took it out of the world')",
        )
        .expect("stale id");
    }

    #[test]
    fn a_mod_steers_an_entity_rather_than_teleporting_it() {
        // `drive` is the mod-facing half of charter rule 1: the engine moves
        // bodies and something else says where they are trying to go. Setting
        // it must reach the entity the physics will read next tick.
        let (mut vm, store) = vm_with_entities();
        load(&mut vm, "herder", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "herder",
            "id = game.spawn_entity{ pos = {x=0,y=64,z=0} }\n\
             assert(game.set_entity(id, {\n\
               drive = { walk = { x = 1, z = 0 }, gait = 'sprint', jump = true },\n\
               yaw = 1.5, anim = 2,\n\
             }), 'set_entity changed nothing')",
        )
        .expect("steer");

        let entities = store.entities.lock().expect("lock");
        let (_, entity) = entities.iter().next().expect("one entity");
        assert_eq!(entity.drive.gait, crate::phys::Gait::Sprint);
        assert!(entity.drive.jump);
        assert!((entity.drive.walk[0] - 1.0).abs() < f32::EPSILON);
        assert!((entity.transform.yaw - 1.5).abs() < f32::EPSILON);
        assert_eq!(entity.anim, crate::ent::AnimTag::RUN);
    }

    #[test]
    fn a_mistyped_gait_is_an_error_rather_than_a_silent_walk() {
        // A mod that typed `run` when the engine says `sprint` should hear
        // about it. Silently walking would be a mob that is subtly wrong and a
        // bug nobody can see.
        let (mut vm, _store) = vm_with_entities();
        load(&mut vm, "herder", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        let err = vm
            .eval_in(
                "herder",
                "local id = game.spawn_entity{ pos = {x=0,y=0,z=0} }\n\
                 game.set_entity(id, { drive = { gait = 'run' } })",
            )
            .expect_err("should refuse");
        // `Display` names the mod, which is what an operator reads first; the
        // detail a mod author needs is in the `Debug` form.
        let text = format!("{err:?}");
        assert!(
            text.contains("unknown gait") && text.contains("run"),
            "the error should name the field and the value: {text}"
        );
    }

    #[test]
    fn a_radius_query_answers_in_blocks_nearest_first_and_filters_by_mod() {
        let (mut vm, _store) = vm_with_entities();
        load(&mut vm, "watcher", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        // Radius is in BLOCKS, and the store works in cells — three to a block
        // (charter rule 5). A query that forgot the conversion would reach a
        // third as far, which is the sort of thing that looks like a tuning
        // problem for a week.
        vm.eval_in(
            "watcher",
            "local near = game.spawn_entity{ pos = {x=2,y=0,z=0} }\n\
             local far = game.spawn_entity{ pos = {x=20,y=0,z=0} }\n\
             local mid = game.spawn_entity{ pos = {x=5,y=0,z=0} }\n\
             local found = game.entities_in_radius({x=0,y=0,z=0}, 10)\n\
             assert(#found == 2, 'expected 2 within 10 blocks, got ' .. #found)\n\
             assert(found[1] == near, 'the nearest is not first')\n\
             assert(found[2] == mid, 'the order is wrong')\n\
             assert(#game.entities_in_radius({x=0,y=0,z=0}, 10, 'watcher') == 2)\n\
             assert(#game.entities_in_radius({x=0,y=0,z=0}, 10, 'someone_else') == 0)\n\
             assert(#game.entities_in_radius({x=0,y=0,z=0}, 100) == 3, 'the far one is reachable')\n\
             local _ = far",
        )
        .expect("radius query");
    }

    #[test]
    fn the_entity_api_does_nothing_rather_than_failing_when_there_is_no_world() {
        // Called during worldgen, or in a test with no server behind the VM.
        // An error here would be one every mod has to write code around, and
        // the fluid API already answers this way.
        let mut vm = vm();
        load(&mut vm, "early", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "early",
            "assert(game.spawn_entity{ pos = {x=0,y=0,z=0} } == nil)\n\
             assert(game.despawn_entity(1) == false)\n\
             assert(game.entity(1) == nil)\n\
             assert(game.set_entity(1, { yaw = 1 }) == false)\n\
             assert(#game.entities_in_radius({x=0,y=0,z=0}, 10) == 0)",
        )
        .expect("no world");
    }
    /// A storage store a test can watch: the server's semantics, in memory.
    #[derive(Default)]
    struct Shelf {
        held: std::sync::Mutex<std::collections::BTreeMap<(String, String), crate::storage::Value>>,
    }

    impl crate::storage::Access for Shelf {
        fn get(&self, mod_id: &str, key: &str) -> Option<crate::storage::Value> {
            self.held
                .lock()
                .ok()?
                .get(&(mod_id.to_owned(), key.to_owned()))
                .cloned()
        }

        fn set(&self, mod_id: &str, key: &str, value: Option<crate::storage::Value>) {
            if let Ok(mut held) = self.held.lock() {
                let at = (mod_id.to_owned(), key.to_owned());
                match value {
                    Some(value) => held.insert(at, value),
                    None => held.remove(&at),
                };
            }
        }

        fn keys(&self, mod_id: &str) -> Vec<String> {
            self.held.lock().map_or_else(
                |_| Vec::new(),
                |held| {
                    held.keys()
                        .filter(|(owner, _)| owner == mod_id)
                        .map(|(_, key)| key.clone())
                        .collect()
                },
            )
        }
    }

    fn vm_with_storage() -> (MluaVm, std::sync::Arc<Shelf>) {
        let mut vm = vm();
        let shelf = std::sync::Arc::new(Shelf::default());
        vm.set_storage_access(
            std::sync::Arc::clone(&shelf) as std::sync::Arc<dyn crate::storage::Access>
        );
        (vm, shelf)
    }

    #[test]
    fn a_mod_stores_and_reads_back_its_own_facts() {
        let (mut vm, shelf) = vm_with_storage();
        load(&mut vm, "keeper", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "keeper",
            "game.storage.set('imprint', 'abc123')\n\
             game.storage.set('count', 7)\n\
             game.storage.set('greeted', true)\n\
             assert(game.storage.get('imprint') == 'abc123')\n\
             assert(game.storage.get('count') == 7)\n\
             assert(game.storage.get('greeted') == true)\n\
             assert(game.storage.get('nothing') == nil)\n\
             local keys = game.storage.keys()\n\
             assert(#keys == 3, 'expected 3 keys, got ' .. #keys)\n\
             assert(keys[1] == 'count', 'keys are not in order: ' .. keys[1])\n\
             game.storage.set('count', nil)\n\
             assert(game.storage.get('count') == nil)",
        )
        .expect("storage round trip");

        assert_eq!(
            crate::storage::Access::keys(&*shelf, "keeper"),
            vec!["greeted".to_owned(), "imprint".to_owned()]
        );
    }

    #[test]
    fn a_mods_storage_is_keyed_to_that_mod() {
        // The isolation is a property of the API surface: the mod id is
        // captured when the function is built and there is nowhere to pass a
        // different one. This is the test that the capture is per environment
        // rather than shared.
        let (mut vm, shelf) = vm_with_storage();
        load(&mut vm, "first", "game.register_on_tick(function() end)").expect("load first");
        load(&mut vm, "second", "game.register_on_tick(function() end)").expect("load second");
        let _ = vm.freeze();

        vm.eval_in("first", "game.storage.set('who', 'first')")
            .expect("first");
        vm.eval_in("second", "game.storage.set('who', 'second')")
            .expect("second");
        vm.eval_in(
            "first",
            "assert(game.storage.get('who') == 'first', 'a mod read the other one\\'s value')",
        )
        .expect("isolation");

        assert_eq!(
            crate::storage::Access::get(&*shelf, "first", "who"),
            Some(crate::storage::Value::Text("first".into()))
        );
        assert_eq!(
            crate::storage::Access::get(&*shelf, "second", "who"),
            Some(crate::storage::Value::Text("second".into()))
        );
    }

    #[test]
    fn storing_a_table_is_refused_by_name_rather_than_silently_dropped() {
        let (mut vm, _shelf) = vm_with_storage();
        load(&mut vm, "keeper", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        let err = vm
            .eval_in("keeper", "game.storage.set('bad', { 1, 2, 3 })")
            .expect_err("a table cannot be stored");
        let text = format!("{err:?}");
        assert!(
            text.contains("table") && text.contains("string"),
            "the error should say what was passed and what is allowed: {text}"
        );
    }

    #[test]
    fn storage_does_nothing_rather_than_failing_when_there_is_no_world() {
        let mut vm = vm();
        load(&mut vm, "early", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "early",
            "game.storage.set('anything', 1)\n\
             assert(game.storage.get('anything') == nil)\n\
             assert(#game.storage.keys() == 0)",
        )
        .expect("no world");
    }
    #[test]
    fn a_mods_entity_callback_runs_once_per_entity_it_owns() {
        let (mut vm, store) = vm_with_entities();
        load(
            &mut vm,
            "herder",
            "seen = {}\n\
             game.register_on_entity_step(function(id, dt)\n\
               seen[#seen + 1] = id\n\
               game.set_entity(id, { drive = { walk = { x = 1, z = 0 } } })\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();

        vm.eval_in(
            "herder",
            "a = game.spawn_entity{ pos = {x=0,y=0,z=0} }\n\
             b = game.spawn_entity{ pos = {x=1,y=0,z=0} }",
        )
        .expect("spawn");
        let ids: Vec<u64> = store
            .entities
            .lock()
            .expect("lock")
            .ids()
            .into_iter()
            .map(|id| id.0)
            .collect();

        assert!(vm.entity_step("herder", &ids, 1).expect("step").is_none());
        vm.eval_in(
            "herder",
            "assert(#seen == 2, 'callback ran ' .. #seen .. ' times for two entities')\n\
             assert(seen[1] == a and seen[2] == b, 'the callback saw them out of order')",
        )
        .expect("what the mod saw");

        // And the drive it set reached the entity the physics will read.
        let entities = store.entities.lock().expect("lock");
        for (_, entity) in entities.iter() {
            assert!(
                (entity.drive.walk[0] - 1.0).abs() < f32::EPSILON,
                "the callback's drive did not reach the entity"
            );
        }
    }

    #[test]
    fn a_callback_that_throws_disables_its_mod_and_stops_reporting() {
        // Charter rule 10: the mod is disabled and the tick continues. Stopping
        // at the first failure is deliberate — a callback that throws will
        // throw for every entity, and two hundred identical faults would bury
        // the one that matters.
        let (mut vm, _store) = vm_with_entities();
        load(
            &mut vm,
            "broken",
            "runs = 0\n\
             game.register_on_entity_step(function(id)\n\
               runs = runs + 1\n\
               error('no')\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();

        let fault = vm
            .entity_step("broken", &[1, 2, 3, 4], 1)
            .expect("vm ok")
            .expect("should fault");
        assert_eq!(fault.0, "broken");
        assert!(
            vm.faulted_mods().contains(&"broken".to_owned()),
            "the mod was not disabled"
        );

        // A disabled mod is not called again.
        assert!(
            vm.entity_step("broken", &[1], 1).expect("vm ok").is_none(),
            "a faulted mod was called again"
        );
    }

    #[test]
    fn a_mod_that_registered_no_entity_callback_is_not_a_stepper() {
        let (mut vm, _store) = vm_with_entities();
        load(&mut vm, "quiet", "game.register_on_tick(function() end)").expect("load");
        let _ = vm.freeze();

        assert!(vm.entity_steppers().is_empty());
        assert!(vm.entity_step("quiet", &[1], 1).expect("vm ok").is_none());
    }
}
