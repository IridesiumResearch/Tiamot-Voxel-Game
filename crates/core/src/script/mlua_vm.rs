// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The `mlua`-backed [`ScriptVm`], compiled against exactly one of Lua 5.4,
//! `LuaJIT`, or Luau.
//!
//! **`mlua` types may be named here, in [`super::hud_vm`], and in
//! [`super::budget`], and nowhere else in `crates/core`.**
//! `scripts/check-vm-containment.sh` enforces it. Until Task 14 this file said
//! it was the only one and that CI checked — CI did not, and the claim had
//! already stopped being true. See [`super`] for why the seam matters.
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

/// Registry key holding the mods that registered `on_player_join`.
const JOINERS: &str = "tiamot.joiners";
/// Registry key for the mods listening for a departure.
const LEAVERS: &str = "tiamot.leavers";
/// Mods with an `on_domain_exit` handler.
const DOMAIN_EXITERS: &str = "tiamot.domain_exiters";
/// Mods with an `on_domain_enter` handler.
const DOMAIN_ENTERERS: &str = "tiamot.domain_enterers";
/// Registry table of random-tick handlers, keyed by numeric material id.
///
/// **By material and not by mod**, unlike every other hook: the engine picks
/// cells and looks at what is in them, so the lookup has to go from a material
/// to a callback rather than from a mod to a list. One handler per material —
/// a second registration is an error, for the reason a second `on_player_join`
/// is.
const RANDOM_TICKS: &str = "tiamot.random_ticks";
/// The same, mapping a material id to the mod that owns its handler, for
/// attributing a fault (charter rule 10).
const RANDOM_TICK_OWNERS: &str = "tiamot.random_tick_owners";

/// Hook name used in registry keys and in fault messages.
const HOOK_JOIN: &str = "on_player_join";
/// The `on_player_leave` hook's name.
const HOOK_LEAVE: &str = "on_player_leave";
/// The hook asked before a body leaves a domain.
const HOOK_DOMAIN_EXIT: &str = "on_domain_exit";
/// The hook asked before a body enters one.
const HOOK_DOMAIN_ENTER: &str = "on_domain_enter";
/// The registry key holding every `on_action` callback.
const ACTORS: &str = "tiamot.actors";
const DIALOGISTS: &str = "tiamot.dialogists";
const CHATTERS: &str = "tiamot.chatters";
/// What an `on_action` hook is called in errors.
const HOOK_ACTION: &str = "on_action";
const HOOK_DIALOG: &str = "on_dialog_event";
const HOOK_CHAT: &str = "on_chat";

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
    /// Where `game.get_tool` and `game.set_tool` reach, once there are players.
    tools: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::dig::Tools>>>>,

    /// Where `game.inventory`, `game.give` and `game.take` reach.
    ///
    /// Empty until the server fills it, exactly like [`Self::tools`]: a mod
    /// crafting during worldgen has nobody to craft for.
    inventories:
        std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::inventory::Access>>>>,
    /// Where `game.play_sound` reaches, once there are players to hear it.
    sounds: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::sound::Access>>>>,
    /// The server's dialog API, installed once the world is running.
    dialogs: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::ui::host::Access>>>>,
    /// Where `game.set_block` sends its edits, once there is a world.
    edits: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::script::WorldEdit>>>>,
    /// Where the `game.*_entity` calls reach, once there is a world.
    entities: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::ent::Access>>>>,
    /// Where `game.storage` reaches, once there is a world.
    storage: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::storage::Access>>>>,
    /// Where `game.set_hud` sends a mod's own HUD values.
    hud: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::hud::Access>>>>,
    /// Where the container calls reach the world's chests.
    containers:
        std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::inventory::Containers>>>>,
    /// Where `game.line_of_sight` looks, while the tick has lent the world out.
    ///
    /// Unlike every other slot here, what is behind this one comes and goes
    /// *within* a tick — see `server::lease`. The handle is installed once and
    /// answers `Unavailable` whenever the engine is mid-edit, which is why the
    /// Lua function has a third return value and the others do not.
    sight: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::sight::Access>>>>,
    /// Where `game.find_path` searches, through the same lending window.
    paths: std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::path::Access>>>>,
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
        if text.contains(super::budget::MARKER) {
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
    /// Delegates to [`super::budget`], which both VMs in this crate share — the
    /// backend-conditional hook is one thing to get right, not two.
    fn arm_budget(&self, instructions: u32) -> Result<(), ScriptError> {
        super::budget::arm(&self.lua, instructions).map_err(|err| self.vm_error(&err))
    }

    /// Removes the budget, so engine-side work is not counted against a mod.
    fn disarm_budget(&self) {
        super::budget::disarm(&self.lua);
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

/// Converts a mod's one-based slot number to the zero-based one on the wire.
///
/// Absent means the first slot. A zero — which is not a valid one-based index,
/// but is what a mod used to writing C-shaped code will send — also means the
/// first slot rather than wrapping to the last.
fn one_based(value: Option<u16>) -> u16 {
    value.unwrap_or(1).saturating_sub(1)
}

/// The view a mod means when it does not say: the player's own bag.
const DEFAULT_VIEW: &str = "player:main";

/// Turns a Lua material — a string id or a numeric one — into a [`MaterialId`].
///
/// **Both spellings, because a mod has both to hand.** Charter rule 8 makes the
/// string id canonical and that is what a mod author writes; a numeric id is
/// what every hook event and inventory listing hands back, and asking them to
/// convert it before giving it straight back would be a papercut with no
/// purpose. A string that no mod registered is an error naming the id, not a
/// silent fall back to air.
fn material_of(lua: &mlua::Lua, value: &mlua::Value) -> mlua::Result<crate::material::MaterialId> {
    match value {
        mlua::Value::Integer(id) => u16::try_from(*id)
            .map(crate::material::MaterialId)
            .map_err(|_| mlua::Error::external(format!("material id {id} is out of range"))),
        mlua::Value::String(id) => {
            let id = id.to_str()?.to_owned();
            let registry: Table = lua.named_registry_value("tiamot.blocks")?;
            registry
                .get::<Option<u16>>(id.clone())?
                .map(crate::material::MaterialId)
                .ok_or_else(|| mlua::Error::external(format!("no block registered with id `{id}`")))
        }
        other => Err(mlua::Error::external(format!(
            "material must be a block id or a numeric material, got {}",
            other.type_name()
        ))),
    }
}

/// The player a mod named, as raw UUID bytes.
fn player_of(uuid: &str, what: &str) -> mlua::Result<[u8; 32]> {
    crate::identity::PlayerUuid::from_hex(uuid)
        .map(|player| *player.as_bytes())
        .map_err(|_| {
            mlua::Error::external(format!(
                "{what} takes a player UUID in hex, as a hook event reports one"
            ))
        })
}

/// A mod's own word for WHICH stack this is, out of a give/take table.
///
/// **Refused rather than truncated** when it is too long: a mod told its word
/// did not fit can send a shorter one, and a mod whose word was quietly cut
/// short has two items it believes are the same and the engine says are not.
fn detail_of(spec: &Table) -> mlua::Result<Option<String>> {
    let Some(detail) = spec.get::<Option<String>>("detail")? else {
        return Ok(None);
    };
    if detail.len() > crate::inventory::MAX_DETAIL {
        return Err(mlua::Error::external(format!(
            "a stack's `detail` is at most {} bytes, and this one is {}",
            crate::inventory::MAX_DETAIL,
            detail.len()
        )));
    }
    Ok(Some(detail))
}

/// The cut a spec asked for, and whether it was spelled at all.
///
/// A mask of every cell means a whole block, which is loose material and no
/// shape — see [`crate::inventory::Shape::new`], which refuses it for the same
/// reason. A mask a mod cannot mean at all (zero) is an error rather than a
/// silent whole block, because "give them nothing" is not what anyone wrote.
fn shape_of(spec: &Table) -> mlua::Result<Option<crate::inventory::Shape>> {
    let Some(mask) = spec.get::<Option<u32>>("shape")? else {
        return Ok(None);
    };
    if mask & crate::inventory::Shape::ALL == 0 {
        return Err(mlua::Error::external(
            "shape is an occupancy mask over a block's 27 sub-nodes and cannot be empty",
        ));
    }
    Ok(crate::inventory::Shape::new(mask))
}

/// How many units a `{units = }` or `{count = }` spec asks for.
///
/// `count` is items — blocks of loose material, or items of the cut — and is
/// what a crafting recipe is written in. `units` is charter rule 5's own
/// quantity and is what conservation is asserted on. Both, because a mod
/// writing "three stairs" and a mod writing "eighty-one units" are both right.
fn units_of(spec: &Table, shape: Option<crate::inventory::Shape>) -> mlua::Result<u32> {
    if let Some(units) = spec.get::<Option<u32>>("units")? {
        return Ok(units);
    }
    let count = spec.get::<Option<u32>>("count")?.unwrap_or(1);
    let per = shape.map_or(crate::UNITS_PER_BLOCK, crate::inventory::Shape::cells);
    count.checked_mul(per).ok_or_else(|| {
        mlua::Error::external(format!(
            "{count} items of {per} units each does not fit in a stack"
        ))
    })
}

/// One stack, as a mod writes it.
///
/// The same three fields `game.give` takes, so a mod that can hand a player a
/// stack can drop the same stack on the ground without learning a second
/// spelling.
fn stack_of(lua: &mlua::Lua, spec: &Table) -> mlua::Result<Option<crate::inventory::Stack>> {
    let material = material_of(lua, &spec.get::<mlua::Value>("material")?)?;
    let shape = shape_of(spec)?;
    let units = units_of(spec, shape)?;
    Ok(crate::inventory::Stack::new(material, units)
        .map(|stack| crate::inventory::Stack { shape, ..stack }))
}

/// One stack, as a mod reads it.
fn stack_table(lua: &mlua::Lua, stack: &crate::inventory::Stack) -> mlua::Result<Table> {
    let (blocks, nodes) = stack.display();
    let entry = lua.create_table()?;
    entry.set("material", stack.material.0)?;
    entry.set("units", stack.units)?;
    entry.set("blocks", blocks)?;
    entry.set("nodes", nodes)?;
    entry.set("count", stack.count())?;
    // Absent rather than zero for loose material, so `if slot.shape then` is
    // the test for "is this a cut" and a mask of zero never has to mean two
    // different things.
    entry.set("shape", stack.shape.map(crate::inventory::Shape::occupancy))?;
    // The mod's own word for which item this is, absent when it never said
    // one — so `if slot.detail then` is the test, and the empty string stays
    // available as something a mod could mean.
    entry.set("detail", stack.detail.clone())?;
    Ok(entry)
}

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
            tools: std::sync::Arc::new(std::sync::Mutex::new(None)),
            inventories: std::sync::Arc::new(std::sync::Mutex::new(None)),
            sounds: std::sync::Arc::new(std::sync::Mutex::new(None)),
            dialogs: std::sync::Arc::new(std::sync::Mutex::new(None)),
            entities: std::sync::Arc::new(std::sync::Mutex::new(None)),
            storage: std::sync::Arc::new(std::sync::Mutex::new(None)),
            hud: std::sync::Arc::new(std::sync::Mutex::new(None)),
            containers: std::sync::Arc::new(std::sync::Mutex::new(None)),
            sight: std::sync::Arc::new(std::sync::Mutex::new(None)),
            paths: std::sync::Arc::new(std::sync::Mutex::new(None)),
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

    fn set_tools_access(&mut self, access: std::sync::Arc<dyn crate::dig::Tools>) {
        if let Ok(mut slot) = self.tools.lock() {
            *slot = Some(access);
        }
    }

    fn set_inventory_access(&mut self, access: std::sync::Arc<dyn crate::inventory::Access>) {
        if let Ok(mut slot) = self.inventories.lock() {
            *slot = Some(access);
        }
    }

    fn set_sound_access(&mut self, access: std::sync::Arc<dyn crate::sound::Access>) {
        if let Ok(mut slot) = self.sounds.lock() {
            *slot = Some(access);
        }
    }

    fn set_dialog_access(&mut self, access: std::sync::Arc<dyn crate::ui::host::Access>) {
        if let Ok(mut slot) = self.dialogs.lock() {
            *slot = Some(access);
        }
    }

    fn registered_sounds(&self) -> Vec<crate::sound::Sound> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.sounds") else {
            return Vec::new();
        };
        // A sequence, so iterating it is load order — the order the settings
        // screen and the content pipeline both want.
        registry
            .sequence_values::<Table>()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                Some(crate::sound::Sound {
                    id: entry.get("id").ok()?,
                    mod_id: entry.get("mod_id").ok()?,
                    file: entry.get("file").ok()?,
                    gain: entry.get("gain").unwrap_or(1.0),
                    pitch_variance: entry.get("pitch_variance").unwrap_or(0.0),
                })
            })
            .collect()
    }

    fn registered_bindings(&self) -> Vec<crate::sound::Binding> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.bindings") else {
            return Vec::new();
        };
        registry
            .sequence_values::<Table>()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                Some(crate::sound::Binding {
                    cue: entry.get("cue").ok()?,
                    sound: entry.get("sound").ok()?,
                    mod_id: entry.get("mod_id").ok()?,
                })
            })
            .collect()
    }

    fn registered_hud_scripts(&self) -> Vec<crate::hud::ScriptFile> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.hud_scripts") else {
            return Vec::new();
        };
        // Load order, which is DRAW order on the client: a mod loaded later
        // draws on top of one loaded earlier.
        registry
            .sequence_values::<Table>()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                Some(crate::hud::ScriptFile {
                    mod_id: entry.get("mod_id").ok()?,
                    file: entry.get("file").ok()?,
                })
            })
            .collect()
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

    fn set_hud_access(&mut self, access: std::sync::Arc<dyn crate::hud::Access>) {
        if let Ok(mut slot) = self.hud.lock() {
            *slot = Some(access);
        }
    }

    fn set_container_access(&mut self, access: std::sync::Arc<dyn crate::inventory::Containers>) {
        if let Ok(mut slot) = self.containers.lock() {
            *slot = Some(access);
        }
    }

    fn set_sight_access(&mut self, access: std::sync::Arc<dyn crate::sight::Access>) {
        if let Ok(mut slot) = self.sight.lock() {
            *slot = Some(access);
        }
    }

    fn set_path_access(&mut self, access: std::sync::Arc<dyn crate::path::Access>) {
        if let Ok(mut slot) = self.paths.lock() {
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
            // The entity, not a UUID: everything in the world is an entity now,
            // including the other players. A mod that wants to know whether it
            // hit a person reads `owner`, which is the same field
            // `game.entity` reports and the same UUID `game.storage` should
            // key on (charter rule 13).
            table.set("target", event.target.0 as i64)?;
            if let Some(owner) = event.owner {
                table.set("owner", hex_uuid(owner))?;
            }
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_PUNCH, PUNCHERS, &table)
    }

    fn player_join(&mut self, event: &crate::script::JoinEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("name", event.name.as_str())?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_JOIN, JOINERS, &table)
    }

    fn player_leave(&mut self, event: &crate::script::LeaveEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("name", event.name.as_str())?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_LEAVE, LEAVERS, &table)
    }

    fn domain_exit(&mut self, event: &crate::script::DomainEvent) -> HookOutcome {
        self.run_domain_hook(HOOK_DOMAIN_EXIT, DOMAIN_EXITERS, event)
    }

    fn domain_enter(&mut self, event: &crate::script::DomainEvent) -> HookOutcome {
        self.run_domain_hook(HOOK_DOMAIN_ENTER, DOMAIN_ENTERERS, event)
    }

    fn random_tick(&mut self, event: &crate::script::RandomTickEvent) -> HookOutcome {
        let mut outcome = HookOutcome::allow();
        let Ok(handlers) = self.lua.named_registry_value::<Table>(RANDOM_TICKS) else {
            return outcome;
        };
        let Ok(callback) = handlers.get::<mlua::Function>(event.material.0) else {
            // Nothing registered for this material. The engine filters before
            // it gets here, so this is a material whose mod was disabled since
            // the set was taken — ordinary, and nothing to say about it.
            return outcome;
        };
        let owner = self
            .lua
            .named_registry_value::<Table>(RANDOM_TICK_OWNERS)
            .ok()
            .and_then(|owners| owners.get::<String>(event.material.0).ok())
            .unwrap_or_default();
        if self.faulted.contains(&owner) {
            return outcome;
        }

        let Ok(table) = self.lua.create_table() else {
            return outcome;
        };
        if table.set("x", event.pos.x).is_err()
            || table.set("y", event.pos.y).is_err()
            || table.set("z", event.pos.z).is_err()
            || table.set("material", event.material.0).is_err()
        {
            return outcome;
        }

        if self.arm_budget(self.limits.instructions_per_call).is_err() {
            return outcome;
        }
        let result = callback.call::<mlua::Value>(table);
        self.disarm_budget();

        if let Err(err) = result {
            let error = Self::classify(&err, &owner, "random_tick");
            self.faulted.insert(owner.clone());
            tracing::error!(
                mod_id = %owner,
                error = %error,
                "disabling mod after a random_tick failure"
            );
            outcome.faults.push((owner, error));
        }
        outcome
    }

    fn random_tick_materials(&self) -> Vec<MaterialId> {
        let Ok(handlers) = self.lua.named_registry_value::<Table>(RANDOM_TICKS) else {
            return Vec::new();
        };
        let mut ids: Vec<MaterialId> = handlers
            .pairs::<u16, mlua::Value>()
            .filter_map(Result::ok)
            .map(|(id, _)| MaterialId(id))
            .collect();
        // Sorted so the caller's set is built in one order whatever the table
        // iteration gave — charter rule 4's habit, even where nothing hashes.
        ids.sort_unstable();
        ids
    }

    fn action(&mut self, event: &crate::script::ActionEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("id", event.id.as_str())?;
            table.set("pressed", event.pressed)?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_ACTION, ACTORS, &table)
    }

    fn chat(&mut self, event: &crate::script::ChatEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("text", event.text.as_str())?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        self.run_hook(HOOK_CHAT, CHATTERS, &table)
    }

    fn dialog_event(&mut self, event: &crate::script::DialogEvent) -> HookOutcome {
        let Ok(table) = self.hook_event(event.player).and_then(|table| {
            table.set("form", event.form.as_str())?;
            dialog_event_fields(&self.lua, &table, &event.event)?;
            Ok(table)
        }) else {
            return HookOutcome::allow();
        };
        // **One mod, not the list.** Every other hook here runs everybody who
        // registered; this runs only the mod that owns the dialog, because its
        // events are private to it.
        self.run_owner_hook(HOOK_DIALOG, &event.mod_id, &table)
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
            table.set("volume", event.volume)?;
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
                // `absorbs = { rate = n, becomes = "damp_dirt" }`. Read here
                // rather than validated at registration for the same reason
                // `light_emit` is: the registry table holds what the mod said
                // and this turns it into what the simulation needs.
                let absorbs = entry
                    .as_ref()
                    .and_then(|entry| entry.get::<Option<Table>>("absorbs").ok().flatten())
                    .map_or((0, None), |absorbs| {
                        let rate = absorbs
                            .get::<Option<u32>>("rate")
                            .ok()
                            .flatten()
                            .unwrap_or(0)
                            .min(crate::fluid::MAX_VOLUME);
                        let becomes = absorbs.get::<Option<String>>("becomes").ok().flatten();
                        (rate, becomes)
                    });
                (
                    *id,
                    BlockRules {
                        block: block.clone(),
                        step_sound: entry.as_ref().and_then(|entry| {
                            entry.get::<Option<String>>("step_sound").ok().flatten()
                        }),
                        hardness,
                        dominance,
                        drops,
                        light_emit,
                        absorbs,
                    },
                )
            })
            .collect();

        all.sort_by_key(|(id, _)| id.0);
        all.into_iter().map(|(_, rules)| rules).collect()
    }

    fn registered_domains(&self) -> Vec<(String, crate::domain::Spec)> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.domains") else {
            return Vec::new();
        };
        let mut domains: Vec<(String, crate::domain::Spec)> = registry
            .pairs::<String, Table>()
            .filter_map(Result::ok)
            .filter_map(|(id, entry)| {
                let kind: String = entry.get("kind").ok()?;
                Some((
                    id,
                    crate::domain::Spec {
                        kind: if kind == "sparse" {
                            crate::domain::Kind::Sparse
                        } else {
                            crate::domain::Kind::Voxel
                        },
                        scale: entry.get("scale").ok()?,
                        instanced: entry.get("instanced").ok()?,
                    },
                ))
            })
            .collect();
        // Sorted, because a Lua table's pair order is not defined and which
        // domains a world has is observable — charter rule 4 does not care that
        // this one happens to be a registration rather than a simulation.
        domains.sort_by(|a, b| a.0.cmp(&b.0));
        domains
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

                    waterlogs_at: entry.get("waterlogs_at").ok()?,
                    evaporates: entry.get("evaporates").ok()?,
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

    fn registered_items(&self) -> Vec<String> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.items") else {
            return Vec::new();
        };
        let mut items: Vec<String> = registry
            .pairs::<String, bool>()
            .filter_map(Result::ok)
            .map(|(id, _)| id)
            .collect();
        items.sort();
        items
    }

    fn registered_views(&self) -> Vec<crate::inventory::ViewDef> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.views") else {
            return Vec::new();
        };
        let mut views: Vec<crate::inventory::ViewDef> = registry
            .pairs::<String, usize>()
            .filter_map(Result::ok)
            .map(|(id, slots)| crate::inventory::ViewDef { id, slots })
            .collect();
        // Lua table order is unspecified, and this list decides the ORDER views
        // are created in for every player on the server. Two servers disagreeing
        // about it would be two servers whose `ViewUpdate` streams differ.
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }

    fn registered_actions(&self) -> Vec<super::vm::Action> {
        let Ok(registry) = self.lua.named_registry_value::<Table>("tiamot.actions") else {
            return Vec::new();
        };
        // A sequence, not a map: `register_action` pushes, so iterating it in
        // order is load order — which is what the settings screen groups by and
        // the only order a player can predict. `registered_tools` sorts because
        // its list reaches the simulation; this one reaches a screen.
        registry
            .sequence_values::<Table>()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                Some(super::vm::Action {
                    id: entry.get("id").ok()?,
                    description: entry.get("description").unwrap_or_default(),
                    mod_id: entry.get("mod_id").unwrap_or_default(),
                    default_key: entry.get("default_key").unwrap_or_default(),
                })
            })
            .collect()
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
        game.set(
            "register_on_player_join",
            self.hook_registrar(mod_id, HOOK_JOIN, JOINERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_player_leave",
            self.hook_registrar(mod_id, HOOK_LEAVE, LEAVERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        // Refusable, unlike join and leave: those report a thing that has
        // already happened and these are asked before anything moves.
        game.set(
            "register_on_domain_exit",
            self.hook_registrar(mod_id, HOOK_DOMAIN_EXIT, DOMAIN_EXITERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_domain_enter",
            self.hook_registrar(mod_id, HOOK_DOMAIN_ENTER, DOMAIN_ENTERERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set("register_random_tick", self.random_tick_registrar(mod_id)?)
            .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_action",
            self.hook_registrar(mod_id, HOOK_ACTION, ACTORS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        // **No list for this one.** Every other hook appends the mod to a
        // registry list so `run_hook` can call everyone; a dialog event goes to
        // its owner alone, so the callback is stored under the mod's key and
        // there is nobody to enumerate.
        game.set(
            "register_on_chat",
            self.hook_registrar(mod_id, HOOK_CHAT, CHATTERS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        game.set(
            "register_on_dialog_event",
            self.hook_registrar(mod_id, HOOK_DIALOG, DIALOGISTS)?,
        )
        .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `register_on_*` function for one cancellable hook.
    ///
    /// One implementation for both: they differ only in which list they append
    /// to and which registry key they write, and the version with two copies
    /// had already drifted by the time the second one was written.
    /// Builds `game.register_random_tick` for one mod.
    ///
    /// **Keyed by material, not by mod.** The engine picks cells and looks at
    /// what is in them, so what it needs is a way from a material to a
    /// callback. One handler per material: a second registration is an error
    /// rather than a replacement, exactly as it is for the per-mod hooks, and
    /// for the same reason — the silent loss of the first is worse than a
    /// refusal a mod author reads at load.
    fn random_tick_registrar(&self, mod_id: &str) -> Result<mlua::Function, ScriptError> {
        let owner = mod_id.to_owned();
        self.lua
            .create_function(
                move |lua, (material, callback): (mlua::Value, mlua::Function)| {
                    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                    if frozen {
                        return Err(mlua::Error::external(format!(
                            "mod `{owner}`: registration is closed"
                        )));
                    }
                    let material = material_of(lua, &material)?;
                    let handlers: Table = lua.named_registry_value(RANDOM_TICKS)?;
                    if handlers.contains_key(material.0)? {
                        return Err(mlua::Error::external(format!(
                            "mod `{owner}`: a random tick is already registered for that \
                             material; one handler per material"
                        )));
                    }
                    handlers.set(material.0, callback)?;
                    let owners: Table = lua.named_registry_value(RANDOM_TICK_OWNERS)?;
                    owners.set(material.0, owner.as_str())?;
                    Ok(())
                },
            )
            .map_err(|err| self.vm_error(&err))
    }

    fn hook_registrar(
        &self,
        mod_id: &str,
        hook: &str,
        list: &'static str,
    ) -> Result<mlua::Function, ScriptError> {
        let owner = mod_id.to_owned();
        let hook = hook.to_owned();
        let key = Self::hook_key(&hook, mod_id);
        self.lua
            .create_function(move |lua, callback: mlua::Function| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                // **A second registration is an error, not a replacement.**
                //
                // It used to overwrite the callback and push the mod's id onto
                // the list again — so a mod that registered `on_player_join`
                // twice lost its first handler entirely and had its second one
                // called twice. Silently. Found while writing the cue tests, by
                // writing exactly the mod an author would write: two unrelated
                // things to do on join, one `register_` call each.
                //
                // Accumulating both would be the friendlier answer and is a
                // bigger change than it looks — `run_hook` walks one callback
                // per mod, and a veto hook would need a rule for what happens
                // when one of a mod's handlers refuses and another does not.
                // Refusing is the honest interim: it cannot lose anybody's code.
                if lua
                    .named_registry_value::<Value>(&key)
                    .is_ok_and(|held| !held.is_nil())
                {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}` already registered `{hook}`. One callback per hook per \
                         mod — combine them into one function."
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

    /// The `game.register_sound` and `game.play_sound` functions.
    ///
    /// Registration is bounded by the freeze like every other registry (charter
    /// rule 9). Playing is not: it happens during a tick, for ever.
    fn install_sound(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let owner = mod_id.to_owned();
        let register_sound = self
            .lua
            .create_function(move |lua, spec: Table| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                let id: String = spec.get("id")?;
                let entry = lua.create_table()?;
                entry.set(
                    "id",
                    qualify_id(&owner, &id).map_err(mlua::Error::external)?,
                )?;
                entry.set("mod_id", owner.clone())?;
                entry.set("file", spec.get::<String>("file")?)?;
                entry.set("gain", spec.get::<Option<f32>>("gain")?.unwrap_or(1.0))?;
                entry.set(
                    "pitch_variance",
                    spec.get::<Option<f32>>("pitch_variance")?.unwrap_or(0.0),
                )?;
                let sounds: Table = lua.named_registry_value("tiamot.sounds")?;
                sounds.push(entry)?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_sound", register_sound)
            .map_err(|err| self.vm_error(&err))?;

        // **One per mod, and the last one wins.** A mod with two HUD scripts is
        // a mod that should concatenate them: the client budgets per script per
        // frame, so two scripts from one mod would quietly buy it twice the
        // budget of a mod that shipped one.
        let owner = mod_id.to_owned();
        let register_hud = self
            .lua
            .create_function(move |lua, file: String| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                let scripts: Table = lua.named_registry_value("tiamot.hud_scripts")?;
                for existing in scripts.clone().sequence_values::<Table>().flatten() {
                    if existing.get::<String>("mod_id").ok().as_deref() == Some(owner.as_str()) {
                        existing.set("file", file)?;
                        return Ok(());
                    }
                }
                let entry = lua.create_table()?;
                entry.set("mod_id", owner.clone())?;
                entry.set("file", file)?;
                scripts.push(entry)?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_hud_script", register_hud)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.sounds);
        let play_sound = self
            .lua
            .create_function(move |_, spec: Table| {
                let sound: String = spec.get("sound")?;
                // Unqualified means the caller's own, the way every other id in
                // this API works — a mod naming its own sound should not have
                // to repeat its own name.
                let sound = if sound.contains(':') {
                    sound
                } else {
                    qualify_id(&owner, &sound).map_err(mlua::Error::external)?
                };
                let pos: Table = spec.get("pos")?;
                let request = crate::sound::sanitise(crate::sound::PlayRequest {
                    sound,
                    pos: [pos.get("x")?, pos.get("y")?, pos.get("z")?],
                    radius: spec.get::<Option<f32>>("radius")?.unwrap_or(16.0),
                    gain: spec.get::<Option<f32>>("gain")?.unwrap_or(1.0),
                    entity: spec.get::<Option<u64>>("entity")?,
                });
                // How many were told, which is the mod's only feedback — and
                // deliberately not a promise anybody HEARD it.
                let told = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.play(&request)));
                Ok(told.unwrap_or(0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("play_sound", play_sound)
            .map_err(|err| self.vm_error(&err))?;

        self.install_cues(mod_id, game)?;
        Ok(())
    }

    /// `game.bind_sound`, `game.cue`, `game.play_loop` and `game.stop_loop`.
    ///
    /// # The cue is the standardisation
    ///
    /// `register_sound` says a file exists. A BINDING says when it plays. Two
    /// steps rather than one is what turns a pile of `play_sound` calls into a
    /// system: the engine and every mod emit named events, and any mod binds
    /// any sound to any of them without either side knowing the other exists.
    ///
    /// A mod wanting a noise on jumping does not have to find the jump code —
    /// there is none it could reach — and the engine does not have to know
    /// anybody wanted one.
    fn install_cues(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let owner = mod_id.to_owned();
        let bind = self
            .lua
            .create_function(move |lua, (cue, sound): (String, String)| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                // **A cue may be anybody's; a SOUND is always the caller's own
                // unless it says otherwise.** Binding to another mod's cue is
                // the point of the mechanism — re-skinning somebody's door is
                // exactly charter rule 1 working — so the cue is taken as
                // written when it is qualified.
                let cue = if cue.contains(':') {
                    cue
                } else {
                    qualify_id(&owner, &cue).map_err(mlua::Error::external)?
                };
                let sound = if sound.contains(':') {
                    sound
                } else {
                    qualify_id(&owner, &sound).map_err(mlua::Error::external)?
                };
                let entry = lua.create_table()?;
                entry.set("cue", cue)?;
                entry.set("sound", sound)?;
                entry.set("mod_id", owner.clone())?;
                let bindings: Table = lua.named_registry_value("tiamot.bindings")?;
                bindings.push(entry)?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("bind_sound", bind)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.sounds);
        let time_of_day = self
            .lua
            .create_function(move |_, ()| {
                Ok(slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.time_of_day()))
                    .unwrap_or(0.0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("time_of_day", time_of_day)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.sounds);
        let cue = self
            .lua
            .create_function(move |lua, spec: Table| {
                let cue: String = spec.get("cue")?;
                let cue = if cue.contains(':') {
                    cue
                } else {
                    qualify_id(&owner, &cue).map_err(mlua::Error::external)?
                };
                // **A mod may not emit an engine cue.** These mean "the player
                // themselves just did this", and the client resolves them
                // without asking anybody — so a mod that could emit one could
                // make every client believe somebody had jumped.
                if crate::sound::is_engine_cue(&cue) {
                    return Err(mlua::Error::external(format!(
                        "`{cue}` is the engine's to emit; a mod may bind a sound to it but not \
                         raise it"
                    )));
                }
                let bindings: Table = lua.named_registry_value("tiamot.bindings")?;
                // Later wins, so the search runs backwards.
                let mut sound: Option<String> = None;
                for entry in bindings.sequence_values::<Table>().flatten() {
                    if entry.get::<String>("cue").ok().as_deref() == Some(cue.as_str()) {
                        sound = entry.get::<String>("sound").ok();
                    }
                }
                // **A cue nobody bound is silence, not an error.** That is the
                // whole point of separating the two: a mod raises its cues
                // whether or not anybody has given them a noise, and a
                // sound pack is something somebody adds later.
                let Some(sound) = sound else {
                    return Ok(0u32);
                };
                let pos: Table = spec.get("pos")?;
                let request = crate::sound::sanitise(crate::sound::PlayRequest {
                    sound,
                    pos: [pos.get("x")?, pos.get("y")?, pos.get("z")?],
                    radius: spec.get::<Option<f32>>("radius")?.unwrap_or(16.0),
                    gain: spec.get::<Option<f32>>("gain")?.unwrap_or(1.0),
                    entity: spec.get::<Option<u64>>("entity")?,
                });
                let told = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.play(&request)));
                Ok(told.unwrap_or(0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("cue", cue).map_err(|err| self.vm_error(&err))?;

        self.install_loops(mod_id, game)
    }

    /// `game.play_loop` and `game.stop_loop`.
    fn install_loops(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.sounds);
        let play_loop = self
            .lua
            .create_function(move |_, spec: Table| {
                let id: String = spec.get("id")?;
                let id = qualify_id(&owner, &id).map_err(mlua::Error::external)?;
                let sound: String = spec.get("sound")?;
                let sound = if sound.contains(':') {
                    sound
                } else {
                    qualify_id(&owner, &sound).map_err(mlua::Error::external)?
                };
                let everywhere = spec.get::<Option<bool>>("everywhere")?.unwrap_or(false);
                // Ambience has no position, so a mod writing one should not
                // have to invent coordinates for it.
                let pos = match spec.get::<Option<Table>>("pos")? {
                    Some(pos) => [pos.get("x")?, pos.get("y")?, pos.get("z")?],
                    None => [0.0, 0.0, 0.0],
                };
                let request = crate::sound::sanitise_loop(crate::sound::LoopRequest {
                    id,
                    sound,
                    pos,
                    radius: spec.get::<Option<f32>>("radius")?.unwrap_or(16.0),
                    gain: spec.get::<Option<f32>>("gain")?.unwrap_or(1.0),
                    everywhere,
                });
                let told = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.start_loop(&request)));
                Ok(told.unwrap_or(0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("play_loop", play_loop)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.sounds);
        let stop_loop = self
            .lua
            .create_function(move |_, id: String| {
                let id = qualify_id(&owner, &id).map_err(mlua::Error::external)?;
                let told = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.stop_loop(&id)));
                Ok(told.unwrap_or(0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("stop_loop", stop_loop)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `game.show_dialog`, `update_dialog` and `close_dialog` functions.
    ///
    /// # Ownership, and why the form name is qualified
    ///
    /// A dialog belongs to the mod that opened it, and the form name is
    /// namespaced with that mod's id like every other id (charter rule 8). Two
    /// mods can then both call a form `"inventory"` without colliding, and no
    /// mod can name a form into another's namespace to receive their events —
    /// which would mean watching what a player typed into somebody else's text
    /// field.
    fn install_dialogs(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        // **Registered by literal name, twice, rather than in a loop.**
        // `scripts/check-stubs.sh` proves every engine function is documented by
        // scanning for `game.set("...")`, and a loop over a name variable is
        // invisible to it — which it duly reported. A static check that can be
        // defeated by a loop is worth more than the four lines the loop saved.
        let show = self.dialog_shower(mod_id, false)?;
        game.set("show_dialog", show)
            .map_err(|err| self.vm_error(&err))?;
        let update = self.dialog_shower(mod_id, true)?;
        game.set("update_dialog", update)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.dialogs);
        let close = self
            .lua
            .create_function(move |_, spec: Table| {
                let player: String = spec.get("player")?;
                let form: String = spec.get("form")?;
                let form = qualify_id(&owner, &form).map_err(mlua::Error::external)?;
                let closed = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.close(&player, &form)));
                Ok(closed.unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("close_dialog", close)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The body behind `show_dialog` and `update_dialog`, which differ only in
    /// whether they replace a dialog already open.
    fn dialog_shower(&self, mod_id: &str, update: bool) -> Result<mlua::Function, ScriptError> {
        let owner = mod_id.to_owned();
        let slot = std::sync::Arc::clone(&self.dialogs);
        {
            self.lua
                .create_function(move |_, spec: Table| {
                    let player: String = spec.get("player")?;
                    let form: String = spec.get("form")?;
                    let form = qualify_id(&owner, &form).map_err(mlua::Error::external)?;
                    let tree: Table = spec.get("tree")?;
                    let tree = widget_tree(&tree)?;
                    // **Checked before it leaves the mod's call stack.** The
                    // client checks it again on arrival — it does not trust
                    // this server — but a mod that built a tree too deep or too
                    // wide should hear about it here, where the message can
                    // name what it did.
                    crate::ui::check(&tree, crate::ui::Limits::default())
                        .map_err(mlua::Error::external)?;
                    // **Absent means the sheet.** A mod that says nothing gets
                    // the same shape every other screen has; `compact = true`
                    // is a mod saying it built a prompt rather than a screen.
                    let compact: bool = spec.get::<Option<bool>>("compact")?.unwrap_or(false);
                    let request = crate::ui::host::ShowRequest {
                        player,
                        form,
                        tree,
                        update,
                        compact,
                    };
                    let shown = slot
                        .lock()
                        .ok()
                        .and_then(|slot| slot.as_ref().map(|access| access.show(&request)));
                    Ok(shown.unwrap_or(false))
                })
                .map_err(|err| self.vm_error(&err))
        }
    }

    /// `game.heading`, the deterministic arc tangent.
    ///
    /// **The inverse of `game.entity`'s `facing`, and the same reason.** Turning
    /// a direction back into a yaw is what anything that points at something
    /// does, and Lua's `math.atan` is the platform's libm — called inside the
    /// tick, producing a heading that is then persisted entity state. Two
    /// servers running the same mod diverge, quietly, in a saved world.
    ///
    /// A reference mod did exactly that for a day. It was recorded as a known
    /// hazard rather than left to be found, and this is what closes it.
    ///
    /// Arguments are `(dx, dz)` and not `(y, x)`: what a mod has is a direction
    /// across the ground, and asking it to know which of the two goes first in
    /// a maths library's argument list is asking it to get that wrong.
    fn install_heading(&self, game: &Table) -> Result<(), ScriptError> {
        let heading = self
            .lua
            .create_function(|_, (dx, dz): (f32, f32)| {
                // `(dx, dz)` in this engine's world axes, where `+z` is north:
                // the same convention `facing` reports, so the round trip
                // through the two is the identity.
                Ok(crate::detgen::trig::atan2(dx, dz))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("heading", heading)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `game.inventory`, `game.give` and `game.take` functions.
    ///
    /// **Crafting is a mod's job, and this is what makes that possible.**
    /// Charter rule 1: the engine holds mechanisms, and "twenty-seven units of
    /// stone becomes three stairs" is content. Until these existed nothing but
    /// digging could put anything into an inventory and nothing but placing
    /// could take it out, so a shaped stack — which only crafting produces —
    /// had no way to come into being at all.
    ///
    /// All three take a player UUID in hex, the way every hook event names a
    /// player (charter rule 13). Before the seam is installed — during worldgen
    /// — `inventory` answers empty, `give` is dropped and `take` takes nothing,
    /// because there is nobody carrying anything yet. Same rule as
    /// `game.set_tool`.
    fn install_inventory(&self, game: &Table) -> Result<(), ScriptError> {
        let slot = std::sync::Arc::clone(&self.inventories);
        let read = self
            .lua
            .create_function(move |lua, (uuid, view): (String, Option<String>)| {
                let player = player_of(&uuid, "inventory")?;
                let view = view.unwrap_or_else(|| DEFAULT_VIEW.to_owned());
                let stacks = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.contents(player, &view)))
                    .unwrap_or_default();
                let list = lua.create_table()?;
                for stack in stacks {
                    list.push(stack_table(lua, &stack)?)?;
                }
                Ok(list)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("inventory", read)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.inventories);
        let give = self
            .lua
            .create_function(move |lua, (uuid, spec): (String, Table)| {
                let player = player_of(&uuid, "give")?;
                let material = material_of(lua, &spec.get::<mlua::Value>("material")?)?;
                let shape = shape_of(&spec)?;
                let units = units_of(&spec, shape)?;
                let view = spec
                    .get::<Option<String>>("view")?
                    .unwrap_or_else(|| DEFAULT_VIEW.to_owned());
                // Nothing to give is not an error and not a change: a recipe
                // that yields zero of something is a recipe with a condition
                // in it, and the mod already knows what it asked for.
                let detail = detail_of(&spec)?;
                let Some(stack) = crate::inventory::Stack::new(material, units).map(|stack| {
                    crate::inventory::Stack {
                        shape,
                        detail,
                        ..stack
                    }
                }) else {
                    return Ok(false);
                };
                let accepted = slot.lock().ok().and_then(|slot| {
                    slot.as_ref()
                        .map(|access| access.give(player, &view, stack))
                });
                Ok(accepted.unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("give", give).map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.inventories);
        let held = self
            .lua
            .create_function(move |lua, uuid: String| {
                let player = player_of(&uuid, "held")?;
                let stack = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|access| access.held(player)))
                    .flatten();
                stack
                    .as_ref()
                    .map(|stack| stack_table(lua, stack))
                    .transpose()
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("held", held).map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.inventories);
        let take = self
            .lua
            .create_function(move |lua, (uuid, spec): (String, Table)| {
                let player = player_of(&uuid, "take")?;
                let material = material_of(lua, &spec.get::<mlua::Value>("material")?)?;
                let shape = shape_of(&spec)?;
                let units = units_of(&spec, shape)?;
                let view = spec
                    .get::<Option<String>>("view")?
                    .unwrap_or_else(|| DEFAULT_VIEW.to_owned());
                // **How many it actually got, not whether it got them all.**
                // A mod that asked for more than the player has can put back
                // what it took; one told only `false` would have to ask twice
                // to find out how much that was.
                let detail = detail_of(&spec)?;
                let took = slot.lock().ok().and_then(|slot| {
                    slot.as_ref().map(|access| {
                        access.take(player, &view, material, shape, detail.as_deref(), units)
                    })
                });
                Ok(took.unwrap_or(0))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("take", take).map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `game.get_tool` and `game.set_tool` functions.
    ///
    /// Both take a player UUID in hex, the way `game.entity` reports one and
    /// the way every hook event names a player (charter rule 13): mod state
    /// keys on the UUID, never the display name.
    ///
    /// Before the seam is installed — during worldgen — `get_tool` answers a
    /// bare hand and `set_tool` is dropped, because there is nobody holding
    /// anything yet. Same rule as `game.set_block` and `game.get_fluid`.
    fn install_tools(&self, game: &Table) -> Result<(), ScriptError> {
        let slot = std::sync::Arc::clone(&self.tools);
        let get_tool = self
            .lua
            .create_function(move |_, uuid: String| {
                let player = crate::identity::PlayerUuid::from_hex(&uuid).map_err(|_| {
                    mlua::Error::external(
                        "get_tool takes a player UUID in hex, as a hook event reports one",
                    )
                })?;
                let held = slot.lock().ok().and_then(|slot| {
                    slot.as_ref()
                        .and_then(|tools| tools.tool(*player.as_bytes()))
                });
                Ok(held)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("get_tool", get_tool)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.tools);
        let set_tool = self
            .lua
            .create_function(move |_, (uuid, tool): (String, Option<String>)| {
                let player = crate::identity::PlayerUuid::from_hex(&uuid).map_err(|_| {
                    mlua::Error::external(
                        "set_tool takes a player UUID in hex, as a hook event reports one",
                    )
                })?;
                let took = slot.lock().ok().and_then(|slot| {
                    slot.as_ref()
                        .map(|tools| tools.set_tool(*player.as_bytes(), tool.as_deref()))
                });
                // `false` for a tool nobody registered or a player who is not
                // connected, so a mod can tell "refused" from "done" rather
                // than discovering it when a dig never progresses.
                Ok(took.unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("set_tool", set_tool)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// The `game.register_action` function, built once per mod environment.
    ///
    /// Its own method for the same reason the hook registrars are: it captures
    /// the owning mod id, and `install_registration` is at clippy's line limit.
    /// The name is still set with a literal at the call site, because
    /// `scripts/check-stubs.sh` finds the API surface by grepping for
    /// `game.set("...")` and a name built in a loop is one it cannot see.
    fn action_registrar(&self, mod_id: &str) -> Result<mlua::Function, ScriptError> {
        let owner = mod_id.to_owned();
        let registrar = self
            .lua
            .create_function(move |lua, spec: Table| {
                let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
                if frozen {
                    return Err(mlua::Error::external(format!(
                        "mod `{owner}`: registration is closed"
                    )));
                }
                let id: String = spec.get("id")?;
                let entry = lua.create_table()?;
                entry.set(
                    "id",
                    qualify_id(&owner, &id).map_err(mlua::Error::external)?,
                )?;
                entry.set("mod_id", owner.clone())?;
                entry.set(
                    "description",
                    spec.get::<Option<String>>("description")?
                        .unwrap_or_default(),
                )?;
                // The engine does not check this against a key list: it cannot,
                // because `crates/core` must not know what a key is (charter
                // rule 3). The client parses it and warns about one it does not
                // recognise — see `client::input::parse_key`.
                entry.set(
                    "default_key",
                    spec.get::<Option<String>>("default_key")?
                        .unwrap_or_default(),
                )?;
                let actions: Table = lua.named_registry_value("tiamot.actions")?;
                actions.push(entry)?;
                Ok(())
            })
            .map_err(|err| self.vm_error(&err))?;
        Ok(registrar)
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
        let register_item = self
            .lua
            .create_function(move |lua, spec: Table| register_item(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_item", register_item)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let register_view = self
            .lua
            .create_function(move |lua, spec: Table| register_view(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_view", register_view)
            .map_err(|err| self.vm_error(&err))?;

        let owner = mod_id.to_owned();
        let register_fluid = self
            .lua
            .create_function(move |lua, spec: Table| register_fluid(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        let owner = mod_id.to_owned();
        let register_domain = self
            .lua
            .create_function(move |lua, spec: Table| register_domain(lua, &owner, &spec))
            .map_err(|err| self.vm_error(&err))?;
        game.set("register_domain", register_domain)
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
        self.install_tools(game)?;
        self.install_inventory(game)?;
        self.install_sound(mod_id, game)?;
        self.install_dialogs(mod_id, game)?;

        // The two cancellable hooks. Registered exactly like `on_tick` — one
        // callback per mod, ordered by load order — so a mod author has one
        // shape to learn rather than three.
        self.install_cancellable_hooks(mod_id, game)?;

        game.set("register_action", self.action_registrar(mod_id)?)
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

    /// The `game.get_block` function, built once per mod environment.
    ///
    /// # Why `nil` rather than air
    ///
    /// Air is an ANSWER — `{ material = game.AIR, occupancy = 0 }` — and `nil`
    /// means the engine could not say: the chunk is not loaded, or there is no
    /// world behind the VM at all. A mod that could not tell those apart would
    /// eventually build into terrain that was merely unloaded, and the mistake
    /// would show up as a hole in somebody's house rather than as an error
    /// anybody could catch.
    fn block_reader(&self) -> Result<mlua::Function, ScriptError> {
        let sight = std::sync::Arc::clone(&self.sight);
        self.lua
            .create_function(move |lua, position: Table| {
                let x: i32 = position.get("x")?;
                let y: i32 = position.get("y")?;
                let z: i32 = position.get("z")?;

                let reading = sight
                    .lock()
                    .map_err(|_| {
                        mlua::Error::external(
                            "the world lease is poisoned; the simulation thread panicked",
                        )
                    })?
                    .as_ref()
                    .map_or(crate::sight::Reading::Unavailable, |access| {
                        access.block_at(crate::BlockPos::new(x, y, z))
                    });

                match reading {
                    crate::sight::Reading::Absent | crate::sight::Reading::Unavailable => {
                        Ok(mlua::Value::Nil)
                    }
                    crate::sight::Reading::Single {
                        material,
                        occupancy,
                    } => {
                        let out = lua.create_table()?;
                        out.set("material", material.0)?;
                        out.set("occupancy", occupancy)?;
                        Ok(mlua::Value::Table(out))
                    }
                    crate::sight::Reading::Mixed(cells) => {
                        // **`cells` only when there are several**, because a
                        // twenty-seven entry table on every call would be a
                        // cost paid by every mod scanning a region to find the
                        // one block in a thousand that is mixed.
                        let out = lua.create_table()?;
                        let list = lua.create_table()?;
                        let mut occupancy = 0u32;
                        for (index, cell) in cells.iter().enumerate() {
                            list.set(index + 1, cell.0)?;
                            if !cell.is_air() {
                                occupancy |= 1 << index;
                            }
                        }
                        out.set("cells", list)?;
                        out.set("occupancy", occupancy)?;
                        Ok(mlua::Value::Table(out))
                    }
                }
            })
            .map_err(|err| self.vm_error(&err))
    }

    /// The `game.line_of_sight` function, built once per mod environment.
    ///
    /// # Three answers, not two
    ///
    /// `true` and `false` are about the world; `nil` means the engine could not
    /// look — the world was mid-edit, or there is no server behind the VM at
    /// all. Collapsing that into `false` would have been kinder to write and
    /// worse to debug: a mob that stopped following would look exactly like a
    /// mob that lost sight of you, and no amount of staring at the mod would
    /// say otherwise. Lua treats `nil` as false in a condition, so a mod that does
    /// not care gets the safe behaviour for free, and one that does can tell.
    ///
    /// The mod id is captured so the warning names the mod that asked. A mod
    /// reading the world from `on_generate` is making a mistake worth seeing in
    /// the log, and the log is the only place it can be seen.
    fn sight_reader(&self, mod_id: &str) -> Result<mlua::Function, ScriptError> {
        let sight = std::sync::Arc::clone(&self.sight);
        let owner = mod_id.to_owned();
        self.lua
            .create_function(move |_, (from, to): (Table, Table)| {
                let point = |table: &Table| -> mlua::Result<[f64; 3]> {
                    Ok([table.get("x")?, table.get("y")?, table.get("z")?])
                };
                let from = point(&from)?;
                let to = point(&to)?;

                let guard = sight.lock().map_err(|_| {
                    mlua::Error::external(
                        "the world lease is poisoned; the simulation thread panicked",
                    )
                })?;
                let answer = guard
                    .as_ref()
                    .map_or(crate::sight::Sighting::Unavailable, |access| {
                        access.line_of_sight(from, to)
                    });

                Ok(match answer {
                    crate::sight::Sighting::Clear => mlua::Value::Boolean(true),
                    crate::sight::Sighting::Blocked => mlua::Value::Boolean(false),
                    crate::sight::Sighting::Unavailable => {
                        tracing::debug!(
                            mod_id = %owner,
                            "game.line_of_sight was asked while there was no world to look \
                             through; it is available from on_tick and an entity's on_step, \
                             and not from on_generate"
                        );
                        mlua::Value::Nil
                    }
                })
            })
            .map_err(|err| self.vm_error(&err))
    }

    /// The `game.steer_entity` function.
    ///
    /// # One call, because the three parts have to agree
    ///
    /// Walking a mob toward something is: read where it is, work out the drive,
    /// write the drive back. A mod can do the first and the third, and cannot
    /// do the second — the middle needs a look at the block in front of the
    /// mob's feet to know whether to jump, which is terrain.
    ///
    /// Splitting it into `game.steer(from, to)` and `game.set_entity(id, ...)`
    /// would be two VM crossings per mob per tick and an invitation to pass the
    /// wrong position. So it is one call that takes an entity and a place.
    ///
    /// Returns `true` while it is still going and `false` once it has arrived,
    /// so `if not game.steer_entity(id, target) then ... end` is how a mod
    /// notices it got there. `nil` means the world was not available to look
    /// at — ask again next tick.
    fn steerer(&self) -> Result<mlua::Function, ScriptError> {
        let entities = std::sync::Arc::clone(&self.entities);
        let paths = std::sync::Arc::clone(&self.paths);
        self.lua
            .create_function(move |_, (id, target, gait): (u64, Table, Option<String>)| {
                let to: [f64; 3] = [target.get("x")?, target.get("y")?, target.get("z")?];
                // The same three names `set_entity`'s drive takes, so a mod
                // that knows one knows the other. An unknown one walks rather
                // than erroring: a typo in a gait should not stop a mob dead.
                let gait = match gait.as_deref() {
                    Some("sprint") => crate::phys::Gait::Sprint,
                    Some("sneak") => crate::phys::Gait::Sneak,
                    _ => crate::phys::Gait::Walk,
                };

                let Ok(entity_slot) = entities.lock() else {
                    return Ok(mlua::Value::Nil);
                };
                let Some(access) = entity_slot.as_ref() else {
                    return Ok(mlua::Value::Nil);
                };
                let Some(state) = access.get(crate::ent::EntityId(id)) else {
                    // No such entity. Not an error: a mod steering something a
                    // moment after it despawned is ordinary, and raising here
                    // would make every mob loop need a guard.
                    return Ok(mlua::Value::Nil);
                };
                let from = state.transform.to_world();
                // The body's own height, so a mob does not jump into a ceiling
                // it cannot fit through. A marker with no collider is one block
                // tall for this purpose — it has no physics to speak of.
                // `floor_to_i32` and add one rather than `ceil`: the ceiling
                // functions are on this crate's determinism deny-list, and one
                // spelling of a rounding per workspace is worth more than the
                // exemption this could claim for being presentation-adjacent.
                let height = state.collider.map_or(1, |shape| {
                    crate::detgen::floor_to_i32(shape.height).max(1) + 1
                });

                let Ok(path_slot) = paths.lock() else {
                    return Ok(mlua::Value::Nil);
                };
                let Some(paths) = path_slot.as_ref() else {
                    return Ok(mlua::Value::Nil);
                };
                let Some(steer) = paths.steer(from, to, height) else {
                    return Ok(mlua::Value::Nil);
                };

                access.patch(
                    crate::ent::EntityId(id),
                    &crate::ent::access::Patch {
                        drive: Some(crate::phys::Intent {
                            // A mod steers a body on the ground. Flight is a
                            // permission a SERVER grants a player, not a mode a
                            // mob asks for.
                            fly: false,
                            walk: steer.walk,
                            jump: steer.jump,
                            gait,
                        }),
                        ..Default::default()
                    },
                );
                Ok(mlua::Value::Boolean(!steer.arrived))
            })
            .map_err(|err| self.vm_error(&err))
    }

    /// The `game.find_path` function, built once per mod environment.
    ///
    /// # Two returns, because "no" has three meanings
    ///
    /// A route, or `nil` and a reason: `"unreachable"`, `"budget"`, or
    /// `"no world"`. A mob that treats a spent budget as "there is no way
    /// there" gives up on somewhere it could have reached with a nearer target,
    /// and one that treats it as "not yet" retries a search that will never
    /// succeed. The engine knows which happened; collapsing them would throw
    /// that away at the one boundary where it cannot be recovered.
    ///
    /// The route is world blocks, horizontally CENTRED. A mob steers at a
    /// point, and the corner of a block is not where anything walks.
    fn path_finder(&self, mod_id: &str) -> Result<mlua::Function, ScriptError> {
        let paths = std::sync::Arc::clone(&self.paths);
        let owner = mod_id.to_owned();
        self.lua
            .create_function(
                move |lua, (from, to, options): (Table, Table, Option<Table>)| {
                    let point = |table: &Table| -> mlua::Result<[f64; 3]> {
                        Ok([table.get("x")?, table.get("y")?, table.get("z")?])
                    };
                    let from = point(&from)?;
                    let to = point(&to)?;

                    // Every field optional, and a mod that passes nothing gets
                    // something humanoid — the shape almost every mob is.
                    let default = crate::path::Options::default();
                    let options = match options {
                        None => default,
                        Some(table) => crate::path::Options {
                            budget: table.get("budget").unwrap_or(default.budget),
                            height: table.get("height").unwrap_or(default.height),
                            step_up: table.get("step_up").unwrap_or(default.step_up),
                            max_drop: table.get("max_drop").unwrap_or(default.max_drop),
                        },
                    };

                    let guard = paths.lock().map_err(|_| {
                        mlua::Error::external(
                            "the world lease is poisoned; the simulation thread panicked",
                        )
                    })?;
                    let route = guard
                        .as_ref()
                        .map_or(crate::path::Route::Unavailable, |access| {
                            access.find_path(from, to, options)
                        });

                    let refusal = |reason: &'static str| -> mlua::Result<mlua::MultiValue> {
                        Ok(mlua::MultiValue::from_iter([
                            mlua::Value::Nil,
                            mlua::Value::String(lua.create_string(reason)?),
                        ]))
                    };

                    match route {
                        crate::path::Route::Found(blocks) => {
                            let out = lua.create_table()?;
                            let half = 1.0 / 2.0;
                            for block in blocks {
                                let step = lua.create_table()?;
                                step.set("x", f64::from(block.x) + half)?;
                                step.set("y", f64::from(block.y))?;
                                step.set("z", f64::from(block.z) + half)?;
                                out.push(step)?;
                            }
                            Ok(mlua::MultiValue::from_iter([mlua::Value::Table(out)]))
                        }
                        crate::path::Route::Unreachable => refusal("unreachable"),
                        crate::path::Route::Exhausted => refusal("budget"),
                        crate::path::Route::Unavailable => {
                            tracing::debug!(
                                mod_id = %owner,
                                "game.find_path was asked while there was no world to search"
                            );
                            refusal("no world")
                        }
                    }
                },
            )
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
                // `volume` rather than the packed word: a mod that had to know
                // the bit layout would be a mod that breaks when the layout
                // changes — as it just did.
                //
                // In CELLS of 27, the unit charter rule 5 uses for everything
                // else, so a mod comparing a puddle against a bucket compares
                // two numbers in the same units. There is no `source`: under a
                // conserved model nothing sustains itself (Sub-Node Contract
                // §4.2).
                out.set("volume", value.volume())?;
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

                let volume: u32 = spec.get("volume").unwrap_or(crate::fluid::MAX_VOLUME);

                // **Clearing needs no fluid named, and demanding one was a bug
                // with teeth.** The stub documents `set_fluid(pos, {volume = 0})`
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
                let value = if volume == 0 {
                    crate::fluid::Fluid::EMPTY
                } else {
                    let name: Option<String> = spec.get("fluid").ok();
                    let Some(name) = name else {
                        return Err(mlua::Error::external(
                            "set_fluid: missing required field `fluid`. Name the registered \
                             fluid to place, or pass `volume = 0` to clear.",
                        ));
                    };
                    let Some(id) = store.fluid_id(&name) else {
                        return Err(mlua::Error::external(format!(
                            "set_fluid: no fluid registered as `{name}`"
                        )));
                    };
                    crate::fluid::Fluid::new(id, volume)
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
            .create_function(move |lua, spec: Table| {
                let store = store!(store, None::<i64>);
                let position: Table = spec.get("pos")?;
                let transform = crate::ent::Transform::from_world(
                    position.get("x")?,
                    position.get("y")?,
                    position.get("z")?,
                );
                let mut entity = crate::ent::Entity::at(transform, source.clone());
                entity.model = spec.get::<Option<String>>("model")?;
                // **What it looks like, when what it is is a stack.** An item
                // on the ground is a mod's idea — what it is worth, how long it
                // lasts, who may pick it up — and the picture is the engine's,
                // because a mod cannot put geometry in the world at all.
                if let Some(item) = spec.get::<Option<Table>>("item")? {
                    entity.item = stack_of(lua, &item)?;
                }
                if let Some(points) = spec.get::<Option<u32>>("health")? {
                    entity.health = Some(crate::ent::Health::full(points));
                }
                if let Some(name) = spec.get::<Option<String>>("nametag")? {
                    entity.nametag = Some(crate::ent::Nametag::Text(name));
                }
                // A label that follows a player's CURRENT name rather than a
                // copy of what they were called when the entity was made.
                // Charter rule 13 in the one place a mod would otherwise get it
                // wrong: a stored name goes stale on a rebinding, and the stale
                // copy is then the thing a later lookup keys on.
                if let Some(uuid) = spec.get::<Option<String>>("nametag_player")? {
                    let uuid = crate::identity::PlayerUuid::from_hex(&uuid)
                        .map_err(|_| mlua::Error::external(
                            "nametag_player takes a player UUID in hex, as `game.entity` reports one",
                        ))?;
                    entity.nametag = Some(crate::ent::Nametag::Player(uuid));
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
                // **The direction, worked out by the engine.** A mod turning
                // the yaw into a vector itself would call `math.sin`, and that
                // is the platform's libm — two servers would put the same
                // thrown item in different places, and that difference becomes
                // persisted world state. Charter rule 4 names the answer and
                // `detgen::trig` is it, so the answer is the same everywhere
                // and a mod never has to know why.
                //
                // The same convention a figure's heading uses: facing is
                // `(sin yaw, cos yaw)` in the horizontal, so yaw zero is north.
                //
                // **Pitch is in it too**, because "which way is this pointing"
                // includes up and down: a mod throwing something along it
                // throws further when the head is raised and at its own feet
                // when it is lowered, which is what a player expects and what
                // was asked for. A unit vector, so a caller multiplying by a
                // speed gets that speed whichever way they are looking.
                let pitch = entity.transform.pitch;
                let flat = crate::detgen::trig::cos(pitch);
                let facing = lua.create_table()?;
                facing.set("x", crate::detgen::trig::sin(entity.transform.yaw) * flat)?;
                facing.set("y", crate::detgen::trig::sin(pitch))?;
                facing.set("z", crate::detgen::trig::cos(entity.transform.yaw) * flat)?;
                out.set("facing", facing)?;
                let velocity = lua.create_table()?;
                velocity.set("x", entity.velocity.0[0])?;
                velocity.set("y", entity.velocity.0[1])?;
                velocity.set("z", entity.velocity.0[2])?;
                out.set("velocity", velocity)?;
                out.set("on_ground", entity.on_ground)?;
                out.set("source", entity.source)?;
                out.set("model", entity.model)?;
                // The same shape `game.inventory` reports a stack in, so a mod
                // reading an item off the ground and giving it to a player
                // moves one table between two calls.
                out.set(
                    "item",
                    entity
                        .item
                        .as_ref()
                        .map(|stack| stack_table(lua, stack))
                        .transpose()?,
                )?;
                out.set("anim", entity.anim.0)?;
                // Charter rule 13: the UUID, as hex, and never the name. A mod
                // that stores "whose is this" must store something a rename
                // cannot invalidate, and giving it only the name would leave it
                // no way to do that — the reason `game.storage` has
                // `Value::uuid` for the same job.
                if let Some(owner) = entity.owner {
                    out.set("owner", owner.0.to_hex())?;
                }
                // And the label, resolved as far as `core` can: text is text,
                // and a player's tag is their UUID, because the current name is
                // a fact only the server's roster has. A mod wanting to print it
                // is asking a presentation question the engine answers at send
                // time — see `ent::replicate::Spawn::nametag`.
                match entity.nametag {
                    Some(crate::ent::Nametag::Text(text)) => out.set("nametag", text)?,
                    Some(crate::ent::Nametag::Player(uuid)) => {
                        out.set("nametag_player", uuid.to_hex())?;
                    }
                    None => {}
                }
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
        self.install_player_lookup(game)?;
        self.install_transfer(game)?;
        Ok(())
    }

    /// `game.transfer_entity(id, domain, position)`.
    ///
    /// **Asked for, not done.** The move runs `on_domain_exit` and
    /// `on_domain_enter`, and this call is already inside a mod's callback
    /// inside the tick — so performing it here would re-enter the VM while a
    /// script is running and let one mod's call run another's hook underneath
    /// it. The tick performs it once the mods have finished, which is also what
    /// makes asking mid-tick safe rather than merely usually safe.
    ///
    /// So `true` means accepted, not arrived. A mod finds out what became of it
    /// from `on_domain_enter`, which is the only place the answer exists.
    fn install_transfer(&self, game: &Table) -> Result<(), ScriptError> {
        let slot = std::sync::Arc::clone(&self.entities);
        let transfer = self
            .lua
            .create_function(move |_, (id, domain, position): (u64, String, Table)| {
                let to = [
                    position.get::<f64>("x")?,
                    position.get::<f64>("y")?,
                    position.get::<f64>("z")?,
                ];
                if to.iter().any(|axis| !axis.is_finite()) {
                    return Err(mlua::Error::external(
                        "game.transfer_entity was given a position that is not a number",
                    ));
                }
                if domain.is_empty() {
                    return Err(mlua::Error::external(
                        "game.transfer_entity was given no domain to move into",
                    ));
                }
                Ok(slot
                    .lock()
                    .ok()
                    .and_then(|slot| {
                        slot.as_ref()
                            .map(|store| store.transfer(crate::ent::EntityId(id), &domain, to))
                    })
                    .unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("transfer_entity", transfer)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// `game.player_entity(uuid)`.
    ///
    /// **Charter rule 13's other half.** Every hook hands a mod a UUID — who
    /// dug, who spoke, who pressed a key — and every entity call takes an id,
    /// so without this a mod that knows WHO cannot find out WHERE. The
    /// alternative is scanning the world for a body whose owner matches, which
    /// is a scan of everything to answer a question the server has a map for.
    fn install_player_lookup(&self, game: &Table) -> Result<(), ScriptError> {
        let slot = std::sync::Arc::clone(&self.entities);
        let lookup = self
            .lua
            .create_function(move |_, uuid: String| {
                let player = player_of(&uuid, "player_entity")?;
                let found = slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|store| store.player(player)))
                    .flatten();
                #[expect(
                    clippy::cast_possible_wrap,
                    reason = "an entity id is handed to Lua as an integer, as every other one is"
                )]
                Ok(found.map(|id| id.0 as i64))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("player_entity", lookup)
            .map_err(|err| self.vm_error(&err))?;

        // **Moving a player is not patching their entity.** A player is in the
        // store as a transient copy of a body the tick steps from their own
        // inputs; a position written to it is overwritten within the tick and
        // nothing says so. These two write the authoritative body instead.
        let slot = std::sync::Arc::clone(&self.entities);
        let send = self
            .lua
            .create_function(move |_, (uuid, position): (String, Table)| {
                let player = player_of(&uuid, "move_player")?;
                let to = [
                    position.get::<f64>("x")?,
                    position.get::<f64>("y")?,
                    position.get::<f64>("z")?,
                ];
                if to.iter().any(|axis| !axis.is_finite()) {
                    return Err(mlua::Error::external(
                        "game.move_player was given a position that is not a number",
                    ));
                }
                Ok(slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|store| store.move_player(player, to)))
                    .unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("move_player", send)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.entities);
        let shove = self
            .lua
            .create_function(move |_, (uuid, impulse): (String, Table)| {
                let player = player_of(&uuid, "push_player")?;
                let push = [
                    impulse.get::<f32>("x")?,
                    impulse.get::<f32>("y")?,
                    impulse.get::<f32>("z")?,
                ];
                if push.iter().any(|axis| !axis.is_finite()) {
                    return Err(mlua::Error::external(
                        "game.push_player was given an impulse that is not a number",
                    ));
                }
                Ok(slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|store| store.shove_player(player, push)))
                    .unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("push_player", shove)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// Puts the container calls on the `game` table.
    ///
    /// **Not namespaced by mod, unlike storage and HUD values.** A container is
    /// a place in the world and two mods may legitimately want at the same one
    /// — a hopper feeding a furnace is exactly that. The name is the mod's to
    /// choose and to keep unique, the way a block id is.
    fn install_container_api(&self, game: &Table) -> Result<(), ScriptError> {
        macro_rules! reach {
            ($slot:expr, $default:expr, $call:expr) => {
                $slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map($call))
                    .unwrap_or($default)
            };
        }

        let slot = std::sync::Arc::clone(&self.containers);
        let make = self
            .lua
            .create_function(move |_, (name, slots): (String, usize)| {
                if name.is_empty() {
                    return Err(mlua::Error::external(
                        "game.make_container needs a name; it is how the container is found again",
                    ));
                }
                if slots == 0 || slots > crate::inventory::MAX_VIEW_SLOTS {
                    return Err(mlua::Error::external(format!(
                        "a container has 1 to {} slots, and this one asked for {slots}",
                        crate::inventory::MAX_VIEW_SLOTS
                    )));
                }
                Ok(reach!(slot, false, |access| access.ensure(&name, slots)))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("make_container", make)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.containers);
        let open = self
            .lua
            .create_function(move |_, (name, uuid): (String, String)| {
                let player = player_of(&uuid, "open_container")?;
                Ok(reach!(slot, false, |access| access.open(&name, player)))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("open_container", open)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.containers);
        let close = self
            .lua
            .create_function(move |_, (name, uuid): (String, String)| {
                let player = player_of(&uuid, "close_container")?;
                Ok(reach!(slot, false, |access| access.close(&name, player)))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("close_container", close)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.containers);
        let read = self
            .lua
            .create_function(move |lua, name: String| {
                let stacks: Vec<crate::inventory::Stack> =
                    reach!(slot, Vec::new(), |access| access.contents(&name));
                let list = lua.create_table()?;
                for stack in &stacks {
                    list.push(stack_table(lua, stack)?)?;
                }
                Ok(list)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("container", read)
            .map_err(|err| self.vm_error(&err))?;

        let slot = std::sync::Arc::clone(&self.containers);
        let remove = self
            .lua
            .create_function(move |lua, name: String| {
                let stacks: Vec<crate::inventory::Stack> =
                    reach!(slot, Vec::new(), |access| access.remove(&name));
                let list = lua.create_table()?;
                for stack in &stacks {
                    list.push(stack_table(lua, stack)?)?;
                }
                Ok(list)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("break_container", remove)
            .map_err(|err| self.vm_error(&err))
    }

    /// Puts `game.set_hud` on the `game` table.
    ///
    /// **The mod id is captured, never passed**, exactly as `game.storage`'s
    /// is: a mod cannot set another's HUD values because there is nowhere in
    /// the surface to name one.
    fn install_hud_api(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let slot = std::sync::Arc::clone(&self.hud);
        let mine = mod_id.to_owned();
        let set = self
            .lua
            .create_function(move |_, (uuid, values): (String, Table)| {
                let player = player_of(&uuid, "set_hud")?;
                let mut out = crate::hud::Values::new();
                for pair in values.pairs::<String, mlua::Value>() {
                    let (key, value) = pair?;
                    if out.len() >= crate::hud::MAX_VALUES {
                        return Err(mlua::Error::external(format!(
                            "game.set_hud takes at most {} values",
                            crate::hud::MAX_VALUES
                        )));
                    }
                    if key.len() > crate::hud::MAX_KEY {
                        return Err(mlua::Error::external(format!(
                            "game.set_hud value names are at most {} bytes, and `{key}` is longer",
                            crate::hud::MAX_KEY
                        )));
                    }
                    // **Refused, not truncated or coerced.** A mod told that a
                    // value did not fit can decide what to send instead; one
                    // whose value was quietly changed has a HUD that disagrees
                    // with its own arithmetic.
                    let value = match value {
                        mlua::Value::Boolean(flag) => crate::hud::Value::Flag(flag),
                        mlua::Value::Integer(number) => {
                            #[expect(
                                clippy::cast_precision_loss,
                                reason = "a HUD number is drawn, not computed with"
                            )]
                            crate::hud::Value::Number(number as f64)
                        }
                        mlua::Value::Number(number) => {
                            if !number.is_finite() {
                                return Err(mlua::Error::external(format!(
                                    "game.set_hud was given `{key}` as a number that is not one"
                                )));
                            }
                            crate::hud::Value::Number(number)
                        }
                        mlua::Value::String(text) => {
                            let text = text.to_str()?.to_owned();
                            if text.len() > crate::hud::MAX_TEXT {
                                return Err(mlua::Error::external(format!(
                                    "game.set_hud text is at most {} bytes, and `{key}`'s is longer",
                                    crate::hud::MAX_TEXT
                                )));
                            }
                            crate::hud::Value::Text(text)
                        }
                        other => {
                            return Err(mlua::Error::external(format!(
                                "game.set_hud takes numbers, strings and booleans; `{key}` is a {}",
                                other.type_name()
                            )));
                        }
                    };
                    out.insert(key, value);
                }
                Ok(slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.as_ref().map(|hud| hud.set_hud(&mine, player, out)))
                    .unwrap_or(false))
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("set_hud", set).map_err(|err| self.vm_error(&err))
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

    /// What a mod may ask about the world it is standing in: sight, routes, and
    /// the drive that follows one.
    ///
    /// Split out of `install_frozen_api`, which was over clippy's line ceiling
    /// again — the seventh time across these sessions that appending to a
    /// well-named function was the wrong move.
    fn install_perception(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        let line_of_sight = self.sight_reader(mod_id)?;
        game.set("line_of_sight", line_of_sight)
            .map_err(|err| self.vm_error(&err))?;

        let find_path = self.path_finder(mod_id)?;
        game.set("find_path", find_path)
            .map_err(|err| self.vm_error(&err))?;

        let steer = self.steerer()?;
        game.set("steer_entity", steer)
            .map_err(|err| self.vm_error(&err))?;
        Ok(())
    }

    /// `game.get_block_id` and `game.block_of`: a material's two names.
    ///
    /// Charter rule 8 in one place — the string id is canonical and the numeric
    /// one is per-session, so every mod needs to get from either to the other.
    fn install_material_lookups(&self, game: &Table) -> Result<(), ScriptError> {
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

        // The other direction, which every listing needs: an inventory, a dig
        // event and a HUD state all name a material by NUMBER, and a mod that
        // wants to say what the player is holding has nothing to say it with.
        // Charter rule 8 makes the string canonical, so this is how a numeric
        // id gets back to something a person can read.
        let block_of = self
            .lua
            .create_function(|lua, material: u16| {
                let registry: Table = lua.named_registry_value("tiamot.blocks")?;
                for pair in registry.pairs::<String, u16>() {
                    let (id, numeric) = pair?;
                    if numeric == material {
                        return Ok(Some(id));
                    }
                }
                // **`nil` rather than an error.** A material a mod has never
                // heard of is the `engine:unknown` placeholder of charter rule
                // 8 arriving from a world written by a different mod set, and
                // listing it as unknown is what a mod should do about it.
                Ok(None)
            })
            .map_err(|err| self.vm_error(&err))?;
        game.set("block_of", block_of)
            .map_err(|err| self.vm_error(&err))?;

        Ok(())
    }

    /// Everything callable after freeze: lookups, bulk noise, streams, constants.
    fn install_frozen_api(&self, mod_id: &str, game: &Table) -> Result<(), ScriptError> {
        // -- frozen-phase API ---------------------------------------------
        self.install_material_lookups(game)?;

        let set_block = self.block_writer()?;
        game.set("set_block", set_block)
            .map_err(|err| self.vm_error(&err))?;

        self.install_entity_api(mod_id, game)?;
        self.install_storage_api(mod_id, game)?;
        self.install_hud_api(mod_id, game)?;
        self.install_container_api(game)?;
        self.install_heading(game)?;

        let (get_fluid, set_fluid) = self.fluid_functions()?;
        game.set("get_fluid", get_fluid)
            .map_err(|err| self.vm_error(&err))?;
        game.set("set_fluid", set_fluid)
            .map_err(|err| self.vm_error(&err))?;

        let get_light = self.light_reader()?;
        let get_block = self.block_reader()?;
        game.set("get_block", get_block)
            .map_err(|err| self.vm_error(&err))?;
        game.set("get_light", get_light)
            .map_err(|err| self.vm_error(&err))?;

        self.install_perception(mod_id, game)?;

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
        // How many of a thing a slot holds. A recipe that makes "as many as
        // will fit" needs the number, and a mod that guessed it would be wrong
        // the day the engine changed it.
        game.set("OCCUPANCY_FULL", crate::block::OCCUPANCY_FULL)
            .map_err(|err| self.vm_error(&err))?;
        game.set("ITEMS_PER_STACK", crate::inventory::ITEMS_PER_STACK)
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
        let sounds = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let hud_scripts = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        let bindings = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
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
        let items = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.items", items)
            .map_err(|err| self.vm_error(&err))?;
        let views = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.views", views)
            .map_err(|err| self.vm_error(&err))?;
        let tools = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.tools", tools)
            .map_err(|err| self.vm_error(&err))?;
        let domains = self.lua.create_table().map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.domains", domains)
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
        // **Every list a `register_on_*` appends to.** A hook whose list is
        // missing makes `hook_registrar` fail, which disables the mod at load
        // with no clue as to why — which is exactly what `DIALOGISTS` did when
        // it was added to the constants and forgotten here.
        for list in [
            DIGGERS,
            PLACERS,
            PUNCHERS,
            FLOWERS,
            JOINERS,
            LEAVERS,
            DOMAIN_EXITERS,
            DOMAIN_ENTERERS,
            ACTORS,
            DIALOGISTS,
            CHATTERS,
            RANDOM_TICKS,
            RANDOM_TICK_OWNERS,
        ] {
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
            .set_named_registry_value("tiamot.sounds", sounds)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.hud_scripts", hud_scripts)
            .map_err(|err| self.vm_error(&err))?;
        self.lua
            .set_named_registry_value("tiamot.bindings", bindings)
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
    /// Runs one mod's hook, rather than everyone who registered.
    ///
    /// The dialog case. Shares `run_hook`'s error handling by construction —
    /// a one-mod list — so a mod that throws inside a dialog event is disabled
    /// exactly like one that throws anywhere else, rather than through a second
    /// copy of that logic which would drift.
    fn run_owner_hook(&mut self, hook: &str, mod_id: &str, event: &Table) -> HookOutcome {
        if self.faulted.contains(mod_id) {
            return HookOutcome::allow();
        }
        let Ok(callback) = self
            .lua
            .named_registry_value::<mlua::Function>(&Self::hook_key(hook, mod_id))
        else {
            // The mod never registered one, which is ordinary: a dialog with
            // only labels in it needs no handler.
            return HookOutcome::allow();
        };
        let mut outcome = HookOutcome::allow();
        if self.arm_budget(self.limits.instructions_per_call).is_err() {
            return outcome;
        }
        let result = callback.call::<mlua::Value>(event.clone());
        self.disarm_budget();
        if let Err(err) = result {
            let error = Self::classify(&err, mod_id, hook);
            self.faulted.insert(mod_id.to_owned());
            tracing::error!(
                mod_id = %mod_id,
                error = %error,
                "disabling mod after a {hook} failure"
            );
            outcome.faults.push((mod_id.to_owned(), error));
        }
        outcome
    }

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

    /// Runs one of the two domain hooks over every mod that registered it.
    ///
    /// One implementation for both, because they differ only in which list
    /// they walk — and the version with two copies is the one that drifts.
    fn run_domain_hook(
        &mut self,
        hook: &str,
        registry: &str,
        event: &crate::script::DomainEvent,
    ) -> HookOutcome {
        let Ok(table) = (|| -> Result<Table, mlua::Error> {
            let table = self.lua.create_table()?;
            table.set("entity", event.entity)?;
            table.set("from", event.from.as_str())?;
            table.set("to", event.to.as_str())?;
            Ok(table)
        })() else {
            // A table that could not be built is not a refusal: a mod that
            // never ran cannot have objected, and refusing here would strand
            // whatever was moving on a VM problem it had nothing to do with.
            return HookOutcome::allow();
        };
        self.run_hook(hook, registry, &table)
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
/// Refuses a block spec with a field the engine does not know.
///
/// A typo in `hardness` should say so rather than silently taking the default.
/// Its own function because `register_block` is at clippy's line limit — and it
/// earns the separation: adding `sounds` to `BLOCK_FIELDS` was forgotten once,
/// and the mod failed to load with exactly the message this produces.
fn check_block_fields(id: &str, spec: &Table) -> mlua::Result<()> {
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
    Ok(())
}

/// Records the sound a block makes underfoot, if the mod named one.
///
/// Its own function because `register_block` is at clippy's line limit. Only
/// `step` for now: breaking and placing are already events a mod can hook, and
/// a hook can choose a noise with more context than a table could carry.
fn step_sound_of(owner: &str, spec: &Table, entry: &Table) -> mlua::Result<()> {
    if let Some(sounds) = spec.get::<Option<Table>>("sounds")?
        && let Some(step) = sounds.get::<Option<String>>("step")?
    {
        // Unqualified means the caller's own, like every other id here.
        let step = if step.contains(':') {
            step
        } else {
            qualify_id(owner, &step).map_err(mlua::Error::external)?
        };
        entry.set("step_sound", step)?;
    }
    Ok(())
}

/// `game.register_item{ id, name, texture }`.
///
/// # Why an item is a material
///
/// **Everything a player can carry is one id in one table.** A stack is a
/// material, a quantity in units and a cut (charter rule 5), and that is what
/// the inventory, the wire, the icons and the world's own string↔numeric table
/// are all built on. Giving items a second id space would mean a `kind` byte on
/// every stack and a `match` at every one of those places, for a distinction
/// that matters in exactly one of them.
///
/// So an item is registered into the same space and marked NOT PLACEABLE, which
/// is the only thing that actually differs: a sword is a thing you can hold and
/// not a thing you can put in the world. Everything else — the atlas tile, the
/// slot it sits in, the units it counts in, the id the world file remembers —
/// works because it already worked for blocks.
///
/// The consequence to keep in mind: a material id names something carryable,
/// and only some of those are blocks. `placeable` is what tells them apart, and
/// the world palette must never contain a `false` one.
fn register_item(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<u16> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_item: missing required field `id`"))?;
    for pair in spec.pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !ITEM_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_item(\"{id}\"): unknown field `{name}`. An item is not a block: it \
                     has no hardness, no drops and no light, because it is never in the world."
                )));
            }
        }
    }

    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;
    let texture = match spec.get::<Option<String>>("texture")? {
        Some(path) => {
            Some(validate_texture_path(&qualified, &path).map_err(mlua::Error::external)?)
        }
        None => None,
    };

    let registry: Table = lua.named_registry_value("tiamot.blocks")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "`{qualified}` is already registered"
        )));
    }
    let next: u16 = lua.named_registry_value("tiamot.next_material")?;
    registry.set(qualified.clone(), next)?;
    lua.set_named_registry_value("tiamot.next_material", next + 1)?;

    // The one thing that makes it an item rather than a block.
    let items: Table = lua.named_registry_value("tiamot.items")?;
    items.set(qualified.clone(), true)?;

    if let Some(path) = texture {
        let textures: Table = lua.named_registry_value("tiamot.block_textures")?;
        let entry = lua.create_table()?;
        entry.set("mod", owner)?;
        entry.set("path", path)?;
        textures.set(qualified.clone(), entry)?;
    }

    // A display name, on the same table blocks use, so an interface asking what
    // to call something asks one question.
    if let Some(name) = spec.get::<Option<String>>("name")? {
        let rules: Table = lua.named_registry_value("tiamot.block_rules")?;
        let entry = if let Some(entry) = rules.get::<Option<Table>>(qualified.clone())? {
            entry
        } else {
            let entry = lua.create_table()?;
            rules.set(qualified.clone(), entry.clone())?;
            entry
        };
        entry.set("name", name)?;
    }

    Ok(next)
}

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

    check_block_fields(&id, spec)?;

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

        step_sound_of(owner, spec, &entry)?;

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
        if let Some(absorbs) = spec.get::<Option<Table>>("absorbs")? {
            entry.set("absorbs", block_absorbs(lua, owner, &id, &absorbs)?)?;
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
            fly: false,
            walk: [x, z],
            jump: drive.get::<Option<bool>>("jump")?.unwrap_or(false),
            gait,
        });
    }
    Ok(patch)
}

/// `game.register_fluid{ id, material, waterlogs_at, tick_rate, evaporates }`.
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

/// `game.register_domain{ id, kind, scale, instanced }`.
///
/// Stored raw and turned into a `domain::Spec` by `registered_domains`, exactly
/// as fluids are: the VM's job is to collect what mods said, and the host's is
/// to decide what it means.
fn register_domain(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<()> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_domain: missing required field `id`"))?;

    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !DOMAIN_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "register_domain(\"{id}\"): unknown field `{name}`"
                )));
            }
        }
    }

    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;
    let kind: String = spec.get("kind").unwrap_or_else(|_| "voxel".to_owned());
    if kind != "voxel" && kind != "sparse" {
        return Err(mlua::Error::external(format!(
            "register_domain(\"{qualified}\"): `kind` is \"voxel\" or \"sparse\", not \"{kind}\""
        )));
    }
    let scale: f64 = spec.get("scale").unwrap_or(1.0);
    if !scale.is_finite() || scale <= 0.0 {
        return Err(mlua::Error::external(format!(
            "register_domain(\"{qualified}\"): `scale` must be a positive number"
        )));
    }
    let instanced: bool = spec.get("instanced").unwrap_or(false);

    let registry: Table = lua.named_registry_value("tiamot.domains")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "register_domain(\"{qualified}\"): already registered; a second registration \
             would silently replace the first"
        )));
    }
    let entry = lua.create_table()?;
    entry.set("kind", kind)?;
    entry.set("scale", scale)?;
    entry.set("instanced", instanced)?;
    registry.set(qualified, entry)?;
    Ok(())
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

    // A declared sink, and the one that can empty a world. `1` would evaporate
    // a cell from every exposed block every fluid tick, which is a pond that
    // vanishes while you look at it — legal, and almost certainly a typo for a
    // rate, so it is allowed and not silently corrected.
    let evaporates: u32 = spec
        .get("evaporates")
        .unwrap_or(FluidRules::DEFAULT_EVAPORATES);

    let color = fluid_colour(spec, &qualified)?;

    let registry: Table = lua.named_registry_value("tiamot.fluids")?;
    if registry.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "fluid `{qualified}` is already registered"
        )));
    }

    let entry = lua.create_table()?;
    entry.set("material", material)?;

    entry.set("waterlogs_at", waterlogs_at)?;
    entry.set("tick_rate", tick_rate)?;
    entry.set("evaporates", evaporates)?;
    entry.set("color_r", color[0])?;
    entry.set("color_g", color[1])?;
    entry.set("color_b", color[2])?;
    registry.set(qualified, entry)?;
    Ok(())
}

/// `game.register_view{ id, slots }`.
///
/// **A place a mod wants stacks to be able to sit**, given to every player.
/// What the slots MEAN — an armour rack, a tool belt, a bandolier — is the
/// mod's; the engine gives them a name and a size and moves stacks between
/// them. See [`crate::inventory::Slots::for_player_with`].
fn register_view(lua: &Lua, owner: &str, spec: &Table) -> mlua::Result<()> {
    let frozen: bool = lua.named_registry_value("tiamot.frozen").unwrap_or(false);
    if frozen {
        return Err(mlua::Error::external(format!(
            "mod `{owner}`: registration is closed"
        )));
    }

    let id: String = spec
        .get("id")
        .map_err(|_| mlua::Error::external("register_view: missing required field `id`"))?;
    let qualified = qualify_id(owner, &id).map_err(mlua::Error::external)?;

    let slots: usize = spec
        .get("slots")
        .map_err(|_| mlua::Error::external("register_view: missing required field `slots`"))?;
    if slots == 0 || slots > crate::inventory::MAX_VIEW_SLOTS {
        return Err(mlua::Error::external(format!(
            "register_view(\"{qualified}\"): {slots} slots, which is outside 1..={}. Every slot \
             of every view is sent to a client on any change.",
            crate::inventory::MAX_VIEW_SLOTS
        )));
    }

    let views: Table = lua.named_registry_value("tiamot.views")?;
    if views.contains_key(qualified.clone())? {
        return Err(mlua::Error::external(format!(
            "register_view(\"{qualified}\"): already registered"
        )));
    }
    views.set(qualified, slots)?;
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
/// The fields `game.register_domain` accepts.
///
/// Checked so a typo is an error rather than a silent default — the same rule
/// every other registration applies, and for the reason `register_fluid` gives:
/// a misspelled field is a mod that thinks it configured something.
const DOMAIN_FIELDS: [&str; 4] = ["id", "kind", "scale", "instanced"];

const FLUID_FIELDS: [&str; 6] = [
    "id",
    "material",
    "waterlogs_at",
    "tick_rate",
    "evaporates",
    "color",
];

/// Fields `register_block` accepts. Anything else is an error naming the field.
/// Keys `register_item` accepts.
///
/// **Deliberately short.** An item has no hardness, no drops, no dominance and
/// no light, because it is never in the world — and a field list that quietly
/// accepted them would be an API promising behaviour nothing implements.
const ITEM_FIELDS: [&str; 4] = ["id", "name", "texture", "description"];

const BLOCK_FIELDS: [&str; 11] = [
    "id",
    "name",
    "drops",
    "hardness",
    "dominance",
    "description",
    "tags",
    "textures",
    "light_emit",
    "sounds",
    "absorbs",
];

/// Keys the `textures` sub-table accepts.
///
/// Only `all` for now. Per-face textures (`top`, `sides`, …) are a natural
/// extension and deliberately not guessed at here: adding them later is
/// additive, while shipping a six-key schema nothing renders yet would freeze a
/// guess into the mod API.
const TEXTURE_FIELDS: [&str; 1] = ["all"];

/// Keys the `absorbs` sub-table accepts.
const ABSORBS_FIELDS: [&str; 2] = ["rate", "becomes"];

/// Reads and validates `absorbs = { rate = ..., becomes = ... }`.
///
/// Sub-Node Contract §4.3: the ENGINE knows a rate and a successor, and what
/// saturation means is the mod's — which is why the chain is materials rather
/// than state bits, and why the last link simply omits `becomes`.
fn block_absorbs(lua: &Lua, owner: &str, id: &str, absorbs: &Table) -> mlua::Result<Table> {
    for key in absorbs.clone().pairs::<String, Value>() {
        let (key, _) = key?;
        if !ABSORBS_FIELDS.contains(&key.as_str()) {
            return Err(mlua::Error::external(format!(
                "register_block(\"{id}\"): unknown absorbs field `{key}`"
            )));
        }
    }
    let rate: u32 = absorbs.get::<Option<u32>>("rate")?.unwrap_or(0);
    if rate == 0 || rate > crate::fluid::MAX_VOLUME {
        return Err(mlua::Error::external(format!(
            "register_block(\"{id}\"): absorbs.rate must be 1..={}, got {rate}. It is how many \
             of a block's 27 cells this drinks per fluid tick; zero is a block that does not \
             absorb, which is what leaving `absorbs` out says.",
            crate::fluid::MAX_VOLUME
        )));
    }
    let stored = lua.create_table()?;
    stored.set("rate", rate)?;
    if let Some(becomes) = absorbs.get::<Option<String>>("becomes")? {
        stored.set(
            "becomes",
            qualify_id(owner, &becomes).map_err(mlua::Error::external)?,
        )?;
    }
    Ok(stored)
}

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
/// Turns a mod's nested Lua table into a [`crate::ui::Tree`].
///
/// # Why this is strict
///
/// A mod is careless rather than hostile, and the two want opposite treatment.
/// A hostile tree is refused silently and the connection carries on; a mod's
/// mistake wants a message naming the widget and the field, because the
/// alternative is a dialog that renders as a blank box and a mod author with
/// nowhere to start.
///
/// So an unknown widget type, an unknown field, and a field of the wrong type
/// are all errors here — at `show_dialog`, in the mod's own call stack — rather
/// than something the client is asked to make sense of. That is the
/// forward-compatibility rule Task 14 asks for: **unknown widget tag = clean
/// error to the mod, not a client crash.**
///
/// Depth is bounded as it descends. A mod can write a table that nests as
/// deeply as Lua allows, and this recurses, so it stops at the same limit the
/// wire format enforces.
/// Puts a dialog event's own fields on the table a mod receives.
///
/// Named for what the player did — `kind = "pressed"`, `"clicked"` — rather
/// than for what should happen, which is the same choice `proto::Click` makes
/// and for the same reason: the gesture is the fact, the consequence is a
/// decision the mod may want to make differently.
fn dialog_event_fields(
    lua: &Lua,
    table: &Table,
    event: &crate::proto::DialogEvent,
) -> mlua::Result<()> {
    use crate::proto::{Click, DialogEvent};
    let _ = lua;
    match event {
        DialogEvent::Pressed { name } => {
            table.set("kind", "pressed")?;
            table.set("name", name.as_str())?;
        }
        DialogEvent::Submitted { name, text } => {
            table.set("kind", "submitted")?;
            table.set("name", name.as_str())?;
            table.set("text", text.as_str())?;
        }
        DialogEvent::Toggled { name, checked } => {
            table.set("kind", "toggled")?;
            table.set("name", name.as_str())?;
            table.set("checked", *checked)?;
        }
        DialogEvent::Slid { name, value } => {
            table.set("kind", "slid")?;
            table.set("name", name.as_str())?;
            table.set("value", *value)?;
        }
        DialogEvent::Chose { name, index } => {
            table.set("kind", "chose")?;
            table.set("name", name.as_str())?;
            // One-based, because this is a Lua index into the options table the
            // mod itself wrote. Off-by-one here would be silent and constant.
            table.set("index", u32::from(*index) + 1)?;
        }
        DialogEvent::Chiselled { name, shape } => {
            table.set("kind", "chiselled")?;
            table.set("name", name.as_str())?;
            // The raw mask, not a `Shape` — an editor can be chiselled to
            // nothing, and a mod deciding what to do about that is the whole
            // reason it hears about every change rather than only valid ones.
            table.set("shape", *shape)?;
        }
        DialogEvent::Clicked { view, index, click } => {
            table.set("kind", "clicked")?;
            table.set("view", view.as_str())?;
            table.set("index", u32::from(*index) + 1)?;
            table.set(
                "click",
                match click {
                    Click::Left => "left",
                    Click::Right => "right",
                    Click::ShiftLeft => "shift_left",
                },
            )?;
        }
        DialogEvent::Closed => table.set("kind", "closed")?,
    }
    Ok(())
}

fn widget_tree(spec: &Table) -> mlua::Result<crate::ui::Tree> {
    let build = widget_build(spec, 0)?;
    Ok(build.flatten())
}

/// One widget and its children, `depth` levels down.
fn widget_build(spec: &Table, depth: usize) -> mlua::Result<crate::ui::Build> {
    let limits = crate::ui::Limits::default();
    if depth >= limits.depth {
        return Err(mlua::Error::external(format!(
            "dialog nests deeper than {} widgets",
            limits.depth
        )));
    }
    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !WIDGET_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "dialog widget: unknown field `{name}`"
                )));
            }
        }
    }

    let kind: String = spec.get("type")?;
    let widget = widget_of(&kind, spec)?;
    let node = crate::ui::Node {
        widget,
        name: spec.get::<Option<String>>("name")?.unwrap_or_default(),
        style: widget_style(spec)?,
        grow: spec.get::<Option<u16>>("grow")?.unwrap_or(0),
        size: spec.get::<Option<u16>>("size")?,
        cross_size: spec.get::<Option<u16>>("cross_size")?,
        children: crate::ui::Children::default(),
    };

    let mut children = Vec::new();
    if let Some(list) = spec.get::<Option<Table>>("children")? {
        for child in list.sequence_values::<Table>() {
            children.push(widget_build(&child?, depth + 1)?);
        }
    }
    Ok(crate::ui::Build::of(node, children))
}

/// The widget itself, from its `type` and the fields that type uses.
fn widget_of(kind: &str, spec: &Table) -> mlua::Result<crate::ui::Widget> {
    use crate::ui::Widget;
    let text = |key: &str| -> mlua::Result<String> {
        Ok(spec.get::<Option<String>>(key)?.unwrap_or_default())
    };
    Ok(match kind {
        "container" => Widget::Container {
            direction: match spec
                .get::<Option<String>>("direction")?
                .unwrap_or_else(|| "column".to_owned())
                .as_str()
            {
                "row" => crate::ui::Direction::Row,
                "column" => crate::ui::Direction::Column,
                other => {
                    return Err(mlua::Error::external(format!(
                        "container direction must be `row` or `column`, not `{other}`"
                    )));
                }
            },
            gap: spec.get::<Option<u16>>("gap")?.unwrap_or(0),
            padding: spec.get::<Option<u16>>("padding")?.unwrap_or(0),
            align: match spec
                .get::<Option<String>>("align")?
                .unwrap_or_else(|| "start".to_owned())
                .as_str()
            {
                "start" => crate::ui::Align::Start,
                "center" => crate::ui::Align::Center,
                "end" => crate::ui::Align::End,
                "stretch" => crate::ui::Align::Stretch,
                other => {
                    return Err(mlua::Error::external(format!(
                        "container align must be start, center, end or stretch, not `{other}`"
                    )));
                }
            },
        },
        "label" => Widget::Label {
            text: text("text")?,
        },
        "button" => Widget::Button {
            text: text("text")?,
        },
        "image" => Widget::Image {
            hash: content_hash(spec, "hash")?,
        },
        "text_input" => Widget::TextInput {
            initial: text("initial")?,
            placeholder: text("placeholder")?,
        },
        "checkbox" => Widget::Checkbox {
            text: text("text")?,
            checked: spec.get::<Option<bool>>("checked")?.unwrap_or(false),
        },
        "slider" => Widget::Slider {
            min: spec.get::<Option<i32>>("min")?.unwrap_or(0),
            max: spec.get::<Option<i32>>("max")?.unwrap_or(100),
            value: spec.get::<Option<i32>>("value")?.unwrap_or(0),
        },
        // **One-based on the way in, because it is one-based on the way out.**
        // `DialogEvent::Chose` reports `index + 1` and the stubs document the
        // field as one-based, and this read it raw — so a mod that wrote back
        // the index it was just given selected the option after the one the
        // player picked, and a one-option dropdown set to `1` was refused
        // outright for selecting option 1 of 1. Exactly the asymmetry that bit
        // `item_slot` in Task 14, in the one place that escaped the fix.
        "dropdown" => Widget::Dropdown {
            options: spec
                .get::<Option<Vec<String>>>("options")?
                .unwrap_or_default(),
            selected: one_based(spec.get::<Option<u16>>("selected")?),
        },
        // **One-based on the way in, because they are one-based on the way
        // out.** A click event reports its slot as `index + 1`, and until Task
        // 14's play test these went through unconverted — so a mod wrote
        // `index = 3`, addressed the fourth slot's neighbour, and was told
        // `index = 4` when somebody clicked it. `core_ui`'s inventory screen
        // was written against the documented one-based rule and drew a grid
        // one slot past everything a player owned, which is what "items do not
        // seem to display in my inventory" looked like from the window.
        "item_slot" => Widget::ItemSlot {
            view: spec.get::<String>("view")?,
            index: one_based(spec.get::<Option<u16>>("index")?),
        },
        "item_grid" => Widget::ItemGrid {
            view: spec.get::<String>("view")?,
            columns: spec.get::<Option<u16>>("columns")?.unwrap_or(9),
            first: one_based(spec.get::<Option<u16>>("first")?),
            count: spec.get::<Option<u16>>("count")?.unwrap_or(0),
        },
        "scroll" => Widget::Scroll,
        "spacer" => Widget::Spacer,
        // **A full block by default**, because chiselling is subtraction: an
        // editor that opened empty would have nothing to take a cell off.
        "shape_editor" => Widget::ShapeEditor {
            shape: spec
                .get::<Option<u32>>("shape")?
                .unwrap_or(crate::inventory::Shape::ALL),
            material: spec.get::<Option<u16>>("material")?.unwrap_or(1),
        },
        "progress" => Widget::Progress {
            permille: spec.get::<Option<u16>>("permille")?.unwrap_or(0),
        },
        other => {
            // **The forward-compatibility rule, and where it belongs.** A
            // client cannot render a widget it has never heard of, and the
            // only party who can fix that is the mod author — so they are told
            // here, by name, in their own call stack, rather than a client
            // being handed something it must refuse.
            return Err(mlua::Error::external(format!(
                "dialog widget: unknown type `{other}`"
            )));
        }
    })
}

/// A style table, all of it optional.
fn widget_style(spec: &Table) -> mlua::Result<crate::ui::Style> {
    let Some(style) = spec.get::<Option<Table>>("style")? else {
        return Ok(crate::ui::Style::default());
    };
    for pair in style.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if let Value::String(name) = key {
            let name = name.to_string_lossy();
            if !STYLE_FIELDS.contains(&name.as_ref()) {
                return Err(mlua::Error::external(format!(
                    "dialog style: unknown field `{name}`"
                )));
            }
        }
    }
    Ok(crate::ui::Style {
        background: colour(&style, "background")?,
        border: colour(&style, "border")?,
        nine_slice: style
            .get::<Option<Table>>("nine_slice")?
            .map(|_| content_hash(&style, "nine_slice"))
            .transpose()?,
        text_colour: colour(&style, "text_colour")?,
        text_size: style.get::<Option<u16>>("text_size")?,
    })
}

/// `{ r, g, b, a }`, with alpha optional.
fn colour(table: &Table, key: &str) -> mlua::Result<Option<[u8; 4]>> {
    let Some(list) = table.get::<Option<Vec<u8>>>(key)? else {
        return Ok(None);
    };
    if list.len() < 3 || list.len() > 4 {
        return Err(mlua::Error::external(format!(
            "`{key}` wants three or four numbers, got {}",
            list.len()
        )));
    }
    Ok(Some([
        list[0],
        list[1],
        list[2],
        list.get(3).copied().unwrap_or(255),
    ]))
}

/// A 32-byte content hash, as a table of numbers.
fn content_hash(table: &Table, key: &str) -> mlua::Result<[u8; 32]> {
    let bytes: Vec<u8> = table.get(key)?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| mlua::Error::external(format!("`{key}` must be 32 bytes of content hash")))
}

/// Keys a widget table accepts. Same rule as `BLOCK_FIELDS`: a typo is an
/// error, because the alternative is a silently ignored field.
const WIDGET_FIELDS: [&str; 29] = [
    "type",
    "name",
    "style",
    "grow",
    "size",
    "cross_size",
    "children",
    "direction",
    "gap",
    "padding",
    "align",
    "permille",
    "text",
    "hash",
    "initial",
    "placeholder",
    "checked",
    "min",
    "max",
    "value",
    "options",
    "selected",
    "view",
    "index",
    "columns",
    "first",
    "count",
    // Protocol v25's shape editor. **Adding a widget means adding its fields
    // here too**, and forgetting is silent from the engine's side and total
    // from the mod's: the tree is refused wholesale with "unknown field", so
    // the dialog simply never opens. Caught by the bot test that opens one.
    "shape",
    "material",
];

/// Keys a style table accepts.
const STYLE_FIELDS: [&str; 5] = [
    "background",
    "border",
    "nine_slice",
    "text_colour",
    "text_size",
];

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
    #[test]
    fn a_slot_number_means_the_same_thing_going_out_as_coming_back() {
        let lua = Lua::new();
        let spec = lua.create_table().expect("table");
        spec.set("type", "item_grid").expect("set");
        spec.set("view", "player:main").expect("set");
        spec.set("first", 1).expect("set");
        spec.set("count", 27).expect("set");

        let widget = widget_of("item_grid", &spec).expect("build");
        let crate::ui::Widget::ItemGrid { first, .. } = widget else {
            panic!("expected a grid");
        };
        assert_eq!(first, 0, "a mod's first slot is the wire's slot zero");
    }

    #[test]
    fn a_dropdown_selection_is_one_based_the_way_the_event_reports_it() {
        // **The round trip a mod actually writes.** `DialogEvent::Chose` hands
        // back `index + 1`; storing that and putting it back as `selected` must
        // select the same option. Read raw, it selected the next one — and a
        // one-option list set to `1` was refused for selecting option 1 of 1,
        // which is how `core_ui`'s shape tab failed to open at all.
        let lua = Lua::new();
        let spec = lua.create_table().expect("table");
        spec.set("type", "dropdown").expect("set");
        spec.set("options", vec!["stone", "dirt"]).expect("set");
        spec.set("selected", 1).expect("set");

        let crate::ui::Widget::Dropdown { selected, options } =
            widget_of("dropdown", &spec).expect("build")
        else {
            panic!("expected a dropdown");
        };
        assert_eq!(selected, 0, "the mod's first option is the wire's zero");
        // And the tree it produces is one the limits accept, which the raw read
        // did not manage for a single-option list.
        let one = lua.create_table().expect("table");
        one.set("type", "dropdown").expect("set");
        one.set("options", vec!["stone"]).expect("set");
        one.set("selected", 1).expect("set");
        let tree = crate::ui::Build::leaf(widget_of("dropdown", &one).expect("build")).flatten();
        assert!(
            crate::ui::check(&tree, crate::ui::Limits::default()).is_ok(),
            "a mod selecting its only option was refused"
        );
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn a_slot_number_a_mod_left_out_is_the_first_one() {
        let lua = Lua::new();
        let spec = lua.create_table().expect("table");
        spec.set("type", "item_slot").expect("set");
        spec.set("view", "player:main").expect("set");

        let crate::ui::Widget::ItemSlot { index, .. } =
            widget_of("item_slot", &spec).expect("build")
        else {
            panic!("expected a slot");
        };
        assert_eq!(index, 0);

        // A zero is not a valid one-based index, but it is what somebody used
        // to C-shaped code will send. It means the first slot, not the last.
        spec.set("index", 0).expect("set");
        let crate::ui::Widget::ItemSlot { index, .. } =
            widget_of("item_slot", &spec).expect("build")
        else {
            panic!("expected a slot");
        };
        assert_eq!(index, 0, "a zero must not wrap to u16::MAX");
    }

    use super::*;

    fn vm() -> MluaVm {
        MluaVm::new(VmLimits::default()).expect("create vm")
    }

    /// Shows a dialog and captures what the server was asked to show.
    #[derive(Default)]
    struct Screen {
        shown: std::sync::Mutex<Vec<crate::ui::host::ShowRequest>>,
        closed: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl crate::ui::host::Access for Screen {
        fn show(&self, request: &crate::ui::host::ShowRequest) -> bool {
            if let Ok(mut shown) = self.shown.lock() {
                shown.push(request.clone());
            }
            true
        }

        fn close(&self, player: &str, form: &str) -> bool {
            if let Ok(mut closed) = self.closed.lock() {
                closed.push((player.to_owned(), form.to_owned()));
            }
            true
        }
    }

    /// Loads a mod that shows `tree`, and returns what reached the server.
    fn show(source: &str) -> Result<Vec<crate::ui::host::ShowRequest>, ScriptError> {
        let mut vm = vm();
        let screen = std::sync::Arc::new(Screen::default());
        vm.set_dialog_access(
            std::sync::Arc::clone(&screen) as std::sync::Arc<dyn crate::ui::host::Access>
        );
        load(&mut vm, "shop", source)?;
        let shown = screen.shown.lock().expect("lock").clone();
        Ok(shown)
    }

    #[test]
    fn every_register_on_hook_can_actually_be_registered() {
        // **The failure this catches has no useful symptom.** A `register_on_*`
        // whose registry list was never created fails inside
        // `hook_registrar`, so the MOD fails to load — and what a mod author
        // sees is "errored in init.lua" pointing at a line that is fine.
        //
        // `register_on_dialog_event` did exactly that: the list constant was
        // added, the table was not, and three bot tests hung waiting for a
        // dialog from a mod that had been disabled at load.
        //
        // Enumerated rather than derived, so adding a hook and forgetting its
        // list fails HERE, with a name.
        let hooks = [
            // The real names, taken from `game.set(...)` rather than from
            // memory: the first draft of this test invented `register_on_dig`,
            // which does not exist, and duly failed on a hook that was fine.
            "register_on_dig_complete",
            "register_on_place",
            "register_on_punch",
            "register_on_fluid_flow",
            "register_on_player_join",
            "register_on_action",
            "register_on_dialog_event",
        ];
        for hook in hooks {
            let mut vm = vm();
            let source = format!("game.{hook}(function(event) end)");
            if let Err(err) = load(&mut vm, "t", &source) {
                let (ScriptError::Load { detail, .. } | ScriptError::Runtime { detail, .. }) = &err
                else {
                    panic!("`{hook}`: {err:?}");
                };
                panic!("`{hook}` could not be registered: {detail}");
            }
        }
    }

    #[test]
    fn a_dialog_is_a_screen_unless_the_mod_says_it_is_a_prompt() {
        // **The default is the shape the rest of the interface has.** A mod
        // that says nothing gets the full sheet, because a player reads every
        // screen the game shows them as one interface — and the engine cannot
        // tell a two-button prompt from an inventory by looking at the tree.
        let tree = r#"tree = { type = "label", text = "hello" }"#;
        let silent = show(&format!(
            r#"game.show_dialog{{ player = "abc", form = "s", {tree} }}"#
        ))
        .expect("load");
        assert!(
            !silent[0].compact,
            "a mod that said nothing got a prompt rather than a screen"
        );

        let asked = show(&format!(
            r#"game.show_dialog{{ player = "abc", form = "s", compact = true, {tree} }}"#
        ))
        .expect("load");
        assert!(
            asked[0].compact,
            "`compact = true` did not reach the server"
        );

        // And on an update, because the flag travels with the tree: a redraw
        // that dropped it would change the window a player is reading.
        let updated = show(&format!(
            r#"game.update_dialog{{ player = "abc", form = "s", compact = true, {tree} }}"#
        ))
        .expect("load");
        assert!(updated[0].update && updated[0].compact);
    }

    #[test]
    fn a_mods_nested_table_becomes_a_flat_tree() {
        // Tier one's mod-facing contract: a nested table in, a flat tree out,
        // with the index-ordering invariant established by construction.
        let shown = show(
            r#"
            game.show_dialog{
              player = "abc",
              form = "shop",
              tree = {
                type = "container", direction = "column", gap = 4, padding = 8,
                children = {
                  { type = "label", text = "Buy" },
                  { type = "button", name = "ok", text = "OK" },
                },
              },
            }
            "#,
        )
        .expect("load");
        assert_eq!(shown.len(), 1, "the dialog never reached the server");
        let request = &shown[0];
        // Namespaced with the mod's own id, like every other id (rule 8).
        assert_eq!(request.form, "shop:shop");
        assert_eq!(request.player, "abc");
        assert!(!request.update, "show_dialog should not be an update");

        let tree = &request.tree;
        assert_eq!(tree.nodes.len(), 3, "root and two children");
        assert!(
            crate::ui::check(tree, crate::ui::Limits::default()).is_ok(),
            "a tree built from Lua did not pass the checker"
        );
        // The invariant, asserted rather than assumed.
        for (index, node) in tree.nodes.iter().enumerate() {
            if node.children.count > 0 {
                assert!(
                    node.children.first as usize > index,
                    "child {} is not after parent {index}",
                    node.children.first
                );
            }
        }
        assert_eq!(tree.nodes[2].name, "ok");
    }

    #[test]
    fn a_mods_mistake_in_a_dialog_names_what_is_wrong() {
        // **The forward-compatibility rule, where it belongs.** A client cannot
        // render a widget it has never heard of, and the only party who can fix
        // that is the mod author — so the error happens in their call stack,
        // naming the field, rather than a client being handed something it can
        // only refuse.
        let cases = [
            (
                r#"tree = { type = "buton", text = "hi" }"#,
                "unknown type `buton`",
            ),
            (
                r#"tree = { type = "label", txet = "hi" }"#,
                "unknown field `txet`",
            ),
            (
                r#"tree = { type = "container", direction = "diagonal" }"#,
                "direction must be",
            ),
            (
                r#"tree = { type = "container", align = "middle" }"#,
                "align must be",
            ),
            (
                r#"tree = { type = "label", style = { colour = {1,2,3} } }"#,
                "unknown field `colour`",
            ),
        ];
        for (tree, expected) in cases {
            let source = format!("game.show_dialog{{ player = \"a\", form = \"f\", {tree} }}");
            let err = show(&source).expect_err("a malformed dialog was accepted");
            // The DETAIL, not the Display string: a script error displays as
            // "mod `shop` errored in init.lua" and carries the backend's
            // message separately, so a mod author sees both.
            let (ScriptError::Load { detail, .. } | ScriptError::Runtime { detail, .. }) = &err
            else {
                panic!("expected a script error carrying detail, got {err:?}");
            };
            assert!(
                detail.contains(expected),
                "error for `{tree}` should mention `{expected}`, said: {detail}"
            );
        }
    }

    #[test]
    fn a_dialog_too_deep_is_refused_in_the_mods_own_call_stack() {
        // A mod can nest a Lua table as deep as it likes, and the conversion
        // recurses. It stops at the same limit the wire format enforces, and
        // says so where the author can see it.
        let depth = crate::ui::Limits::default().depth + 4;
        let mut tree = String::from(r#"{ type = "label", text = "deep" }"#);
        for _ in 0..depth {
            tree = format!(r#"{{ type = "container", children = {{ {tree} }} }}"#);
        }
        let source = format!("game.show_dialog{{ player = \"a\", form = \"f\", tree = {tree} }}");
        let err = show(&source).expect_err("an over-deep dialog was accepted");
        let (ScriptError::Load { detail, .. } | ScriptError::Runtime { detail, .. }) = &err else {
            panic!("expected a script error carrying detail, got {err:?}");
        };
        assert!(detail.contains("nests deeper"), "said: {detail}");
    }

    #[test]
    fn closing_a_dialog_names_the_same_form_showing_it_did() {
        // A mod closes what it opened, so both ends must qualify the name the
        // same way — an easy thing to get wrong in one place only.
        let mut vm = vm();
        let screen = std::sync::Arc::new(Screen::default());
        vm.set_dialog_access(
            std::sync::Arc::clone(&screen) as std::sync::Arc<dyn crate::ui::host::Access>
        );
        load(
            &mut vm,
            "shop",
            r#"
            game.show_dialog{ player = "abc", form = "till", tree = { type = "spacer" } }
            game.close_dialog{ player = "abc", form = "till" }
            "#,
        )
        .expect("load");
        let shown = screen.shown.lock().expect("lock");
        let closed = screen.closed.lock().expect("lock");
        assert_eq!(shown[0].form, "shop:till");
        assert_eq!(closed[0], ("abc".to_owned(), "shop:till".to_owned()));
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

    /// A world that answers whatever a test told it to, and remembers what it
    /// was asked. The engine's own answer is tested in `crate::sight`; what is
    /// under test here is the trip through Lua.
    #[derive(Default)]
    struct Eye {
        reply: std::sync::Mutex<Option<crate::sight::Sighting>>,
        asked: std::sync::Mutex<Vec<([f64; 3], [f64; 3])>>,
        /// What `block_at` should answer, and where it was asked.
        block: std::sync::Mutex<Option<crate::sight::Reading>>,
        blocks_asked: std::sync::Mutex<Vec<crate::BlockPos>>,
    }

    impl crate::sight::Access for Eye {
        fn line_of_sight(&self, from: [f64; 3], to: [f64; 3]) -> crate::sight::Sighting {
            if let Ok(mut asked) = self.asked.lock() {
                asked.push((from, to));
            }
            self.reply
                .lock()
                .ok()
                .and_then(|reply| *reply)
                .unwrap_or(crate::sight::Sighting::Unavailable)
        }

        fn block_at(&self, pos: crate::BlockPos) -> crate::sight::Reading {
            if let Ok(mut asked) = self.blocks_asked.lock() {
                asked.push(pos);
            }
            self.block
                .lock()
                .ok()
                .and_then(|reply| reply.clone())
                .unwrap_or(crate::sight::Reading::Unavailable)
        }
    }

    /// A pathfinder that answers whatever a test told it to.
    #[derive(Default)]
    struct Map {
        reply: std::sync::Mutex<Option<crate::path::Route>>,
        asked: std::sync::Mutex<Vec<crate::path::Options>>,
    }

    impl crate::path::Access for Map {
        fn steer(&self, from: [f64; 3], to: [f64; 3], _height: i32) -> Option<crate::path::Steer> {
            // No world here, so the direction is all this fixture can answer.
            // Jumping is what needs terrain, and the tests that care about it
            // drive a real server.
            let (dx, dz) = ((to[0] - from[0]) as f32, (to[2] - from[2]) as f32);
            let length = (dx * dx + dz * dz).sqrt();
            Some(crate::path::Steer {
                walk: if length > 0.0 {
                    [dx / length, dz / length]
                } else {
                    [0.0, 0.0]
                },
                jump: false,
                arrived: length <= 0.6,
            })
        }

        fn find_path(
            &self,
            _from: [f64; 3],
            _to: [f64; 3],
            options: crate::path::Options,
        ) -> crate::path::Route {
            if let Ok(mut asked) = self.asked.lock() {
                asked.push(options);
            }
            self.reply
                .lock()
                .ok()
                .and_then(|reply| reply.clone())
                .unwrap_or(crate::path::Route::Unavailable)
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
        // document `set_fluid(pos, {volume = 0})` as the way to scoop, and the
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
                 game.set_fluid({x=1,y=2,z=3}, {fluid='test:milk', volume=27})\n\
               else\n\
                 game.set_fluid({x=1,y=2,z=3}, {volume=0})\n\
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
            volume: 19,
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
             assert(seen.volume == 19, 'volume is ' .. tostring(seen.volume))\n\
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
            volume: 27,
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
    fn an_action_reaches_the_hook_with_both_edges_and_never_a_key() {
        // **Charter rule 11, asserted rather than promised**: the mod is told
        // WHAT was done and never which key did it. There is no key field here
        // and a mod must not be able to find one — a mod that could ask would
        // branch on it, and then rebinding would change behaviour rather than
        // just controls.
        let mut vm = vm();
        load(
            &mut vm,
            "referee",
            "seen = {}\ngame.register_on_action(function(e) seen[#seen + 1] = e end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        for pressed in [true, false] {
            let event = crate::script::ActionEvent {
                player: [0x33; 32],
                id: "referee:chisel_mode".to_owned(),
                pressed,
            };
            // An observation, not a veto: the key is already down.
            assert!(vm.action(&event).allowed, "an action was vetoed");
        }

        vm.eval_in(
            "referee",
            "assert(#seen == 2, 'both edges must arrive, got ' .. #seen)\n\
             assert(seen[1].player == string.rep('33', 32), 'player')\n\
             assert(seen[1].id == 'referee:chisel_mode', 'id')\n\
             assert(seen[1].pressed == true, 'the press')\n\
             assert(seen[2].pressed == false, 'the release')\n\
             for k in pairs(seen[1]) do\n\
               assert(k ~= 'key' and k ~= 'keycode' and k ~= 'default_key',\n\
                 'the engine told a mod which key was pressed: ' .. k)\n\
             end",
        )
        .expect("the hook should see both edges and no key");
    }

    #[test]
    fn a_registered_action_carries_its_mod_and_its_suggested_key() {
        // What reaches the client: the id qualified with the mod that asked,
        // so a settings screen can attribute every binding it offers, and the
        // suggested key as a NAME — `crates/core` must not know what a key is
        // (charter rule 3), so it never parses this.
        let mut vm = vm();
        load(
            &mut vm,
            "core_tools",
            "game.register_action{ id = 'chisel_mode', default_key = 'KeyC', \
             description = 'Hold to chisel' }",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let actions = vm.registered_actions();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].id, "core_tools:chisel_mode");
        assert_eq!(actions[0].mod_id, "core_tools");
        assert_eq!(actions[0].default_key, "KeyC");
        assert_eq!(actions[0].description, "Hold to chisel");
    }

    #[test]
    fn an_action_registered_after_freeze_is_refused() {
        // Charter rule 9: the registration window closes and stays closed.
        let mut vm = vm();
        load(&mut vm, "late", "").expect("load");
        vm.freeze().expect("freeze");
        let err = vm
            .eval_in("late", "game.register_action{ id = 'too_late' }")
            .expect_err("registration was accepted after freeze");
        // The reason is in the cause chain rather than the top line, which
        // reads "mod `late` errored in eval" — so look at the whole thing.
        assert!(
            format!("{err:?}").contains("registration is closed"),
            "the error did not say why: {err:?}"
        );
    }

    #[test]
    fn a_punch_reaches_the_hook_naming_the_entity_and_its_owner() {
        // The target is an entity id and not a UUID, which is the widening the
        // Task 09 version of `PunchEvent` said entities would need. `owner` is
        // carried beside it because the commonest question about a punch is
        // "did somebody hit a person", and the engine has the answer in its
        // hand — making every mod look it up would be work with no upside.
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
            target: crate::ent::EntityId::new(7, 1),
            owner: Some([0x22; 32]),
        };
        assert!(!vm.punch(&event).allowed, "the veto was ignored");
        vm.eval_in(
            "referee",
            "assert(seen.attacker == string.rep('11', 32), 'attacker')\n\
             assert(seen.target ~= nil and seen.target ~= 0, 'target')\n\
             assert(seen.owner == string.rep('22', 32), 'owner')\n\
             assert(seen.attacker ~= seen.owner, 'the two parties must be distinguishable')",
        )
        .expect("the hook should see both parties");
    }

    #[test]
    fn a_join_reaches_the_hook_with_the_uuid_and_the_current_name() {
        // Charter rule 13 lives or dies at this boundary: a mod handed only a
        // name has no way to remember "this player" across a rename, and a mod
        // handed only a UUID has nothing to print.
        let mut vm = vm();
        load(
            &mut vm,
            "greeter",
            "seen = nil\ngame.register_on_player_join(function(e) seen = e end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let outcome = vm.player_join(&crate::script::JoinEvent {
            player: [0x33; 32],
            name: "Ada".to_owned(),
        });
        assert!(outcome.faults.is_empty(), "the hook raised: {outcome:?}");
        vm.eval_in(
            "greeter",
            "assert(seen.player == string.rep('33', 32), 'uuid')\n\
             assert(seen.name == 'Ada', 'name')",
        )
        .expect("the hook should see both");
    }

    /// A body moving from the overworld into a ship.
    fn a_move() -> crate::script::DomainEvent {
        crate::script::DomainEvent {
            entity: 7,
            from: crate::domain::OVERWORLD.to_owned(),
            to: "mod:ship/17".to_owned(),
        }
    }

    #[test]
    fn a_mod_registers_domains_and_templates() {
        let mut vm = vm();
        load(
            &mut vm,
            "stars",
            "game.register_domain{ id = 'space', kind = 'sparse', scale = 1000.0 }\n\
             game.register_domain{ id = 'ship', instanced = true }",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let registered = vm.registered_domains();
        assert_eq!(
            registered.len(),
            2,
            "a mod registered two domains and the host was told about {}",
            registered.len()
        );
        // Namespaced with the mod's own id, like every other registration
        // (charter rule 8).
        let (space, spec) = &registered[1];
        assert_eq!(space, "stars:space");
        assert_eq!(spec.kind, crate::domain::Kind::Sparse);
        assert!((spec.scale - 1000.0).abs() < f64::EPSILON);
        assert!(!spec.instanced);

        let (ship, spec) = &registered[0];
        assert_eq!(ship, "stars:ship");
        assert_eq!(
            spec.kind,
            crate::domain::Kind::Voxel,
            "the default is voxel"
        );
        assert!(spec.instanced, "a template did not come back as one");
    }

    #[test]
    fn a_domain_registration_the_engine_cannot_read_is_refused_rather_than_defaulted() {
        // A misspelled field is a mod that thinks it configured something, and
        // a `kind` the engine does not know is a domain nobody can say what to
        // do with. Both are errors at load rather than surprises later.
        for bad in [
            "game.register_domain{ id = 'a', knid = 'sparse' }",
            "game.register_domain{ id = 'a', kind = 'liquid' }",
            "game.register_domain{ id = 'a', scale = 0 }",
            "game.register_domain{ id = 'a', scale = -1 }",
            "game.register_domain{ kind = 'voxel' }",
        ] {
            let mut vm = vm();
            assert!(load(&mut vm, "stars", bad).is_err(), "`{bad}` was accepted");
        }
    }

    #[test]
    fn registering_one_domain_twice_is_refused() {
        // The silent loss of the first is worse than a hard error, and it is
        // the rule every other registration follows.
        let mut vm = vm();
        assert!(
            load(
                &mut vm,
                "stars",
                "game.register_domain{ id = 'space' }\n\
                 game.register_domain{ id = 'space' }",
            )
            .is_err(),
            "a second registration replaced the first"
        );
    }

    #[test]
    fn a_domain_registered_after_freeze_is_refused() {
        // Charter rule 9.
        let mut vm = vm();
        vm.freeze().expect("freeze");
        assert!(
            load(&mut vm, "latecomer", "game.register_domain{ id = 'space' }").is_err(),
            "a domain was registered after the registries froze"
        );
    }

    #[test]
    fn either_domain_hook_can_refuse_a_move() {
        // Unlike join and leave, these are asked BEFORE anything moves, so
        // `false` has something to stop. A mod that owns a space is the only
        // thing that can know whether leaving or entering it is allowed — a
        // ship in flight, a room locked from the inside.
        for hook in ["on_domain_exit", "on_domain_enter"] {
            let mut vm = vm();
            load(
                &mut vm,
                "warden",
                &format!("game.register_{hook}(function() return false end)"),
            )
            .expect("load");
            vm.freeze().expect("freeze");

            let refused = if hook == "on_domain_exit" {
                vm.domain_exit(&a_move())
            } else {
                vm.domain_enter(&a_move())
            };
            assert!(!refused.allowed, "`{hook}` returned false and was ignored");
        }
    }

    #[test]
    fn a_domain_hook_that_says_nothing_lets_the_move_happen() {
        // The default has to be yes. A mod registering the hook to WATCH moves
        // — logging them, counting them — must not stop them by omission.
        let mut vm = vm();
        load(
            &mut vm,
            "watcher",
            "game.register_on_domain_exit(function() end)\n\
             game.register_on_domain_enter(function() return true end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(vm.domain_exit(&a_move()).allowed, "a bare return refused");
        assert!(
            vm.domain_enter(&a_move()).allowed,
            "a truthy return refused"
        );
    }

    #[test]
    fn a_domain_hook_is_told_both_ends_of_the_move() {
        // Both, so a mod can see where something went without keeping its own
        // record of where everything was — which it would have to rebuild
        // after every restart.
        let mut vm = vm();
        load(
            &mut vm,
            "warden",
            "game.register_on_domain_enter(function(event)\n\
               return event.from == 'overworld' and event.to == 'mod:ship/17' \
                 and event.entity == 7\n\
             end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        assert!(
            vm.domain_enter(&a_move()).allowed,
            "the event did not carry the entity and both domain ids"
        );
    }

    #[test]
    fn a_domain_hook_registered_after_freeze_is_refused() {
        // Charter rule 9, for the two newest hooks.
        let mut vm = vm();
        vm.freeze().expect("freeze");
        for hook in ["on_domain_exit", "on_domain_enter"] {
            assert!(
                load(
                    &mut vm,
                    "latecomer",
                    &format!("game.register_{hook}(function() end)"),
                )
                .is_err(),
                "`{hook}` was registered after the freeze"
            );
        }
    }

    #[test]
    fn a_join_hook_cannot_refuse_entry() {
        // There is nothing to refuse: the player is already in the world by the
        // time this runs, and who may connect at all is charter rule 13's
        // business, decided long before a body exists. A hook returning `false`
        // must not silently look like it did something.
        let mut vm = vm();
        load(
            &mut vm,
            "bouncer",
            "game.register_on_player_join(function() return false end)",
        )
        .expect("load");
        vm.freeze().expect("freeze");

        let outcome = vm.player_join(&crate::script::JoinEvent {
            player: [0x44; 32],
            name: "Nobody".to_owned(),
        });
        assert!(outcome.faults.is_empty());
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

    /// A mod that turns whatever `game.line_of_sight` returned into a block
    /// edit, so a test can see the exact Lua value rather than a proxy for it.
    const SIGHT_MOD: &str = "game.register_on_tick(function()\n\
           local seen = game.line_of_sight({x=1.5,y=2.5,z=3.5}, {x=4.5,y=5.5,z=6.5})\n\
           if seen == true then\n\
             game.set_block({x=1,y=0,z=0}, 'test:clear')\n\
           elseif seen == false then\n\
             game.set_block({x=2,y=0,z=0}, 'test:blocked')\n\
           else\n\
             game.set_block({x=3,y=0,z=0}, 'test:unknown')\n\
           end\n\
         end)";

    /// Reads one block and writes what it found, so a test can read it back
    /// through the world-edit slate.
    const READER_MOD: &str = "game.register_on_tick(function()\n\
           local at = game.get_block({x=1,y=2,z=3})\n\
           if at == nil then\n\
             game.set_block({x=0,y=0,z=0}, 'test:unknown')\n\
           elseif at.cells then\n\
             game.set_block({x=1,y=0,z=0}, 'test:mixed_' .. #at.cells .. '_' .. at.cells[2])\n\
           else\n\
             game.set_block({x=2,y=0,z=0}, 'test:one_' .. at.material .. '_' .. at.occupancy)\n\
           end\n\
         end)";

    #[test]
    fn a_mod_can_read_a_block_and_tells_absent_from_air() {
        // **Air is an answer and absence is not.** A mod that could not tell
        // "there is nothing here" from "I cannot say" would eventually build
        // into terrain that was merely unloaded — a hole in somebody's house
        // rather than an error anybody could catch.
        let mut vm = vm();
        let slate = std::sync::Arc::new(Slate::default());
        vm.set_world_edit(
            std::sync::Arc::clone(&slate) as std::sync::Arc<dyn crate::script::WorldEdit>
        );
        let eye = std::sync::Arc::new(Eye::default());

        load(&mut vm, "reader", READER_MOD).expect("load");
        let _ = vm.freeze();

        // No world behind the VM at all, which is what worldgen sees.
        assert!(vm.tick(1).expect("tick").is_empty());
        vm.set_sight_access(std::sync::Arc::clone(&eye) as std::sync::Arc<dyn crate::sight::Access>);

        // A chunk that is not loaded: the same `nil`, and deliberately — both
        // mean "the engine cannot say".
        *eye.block.lock().expect("block") = Some(crate::sight::Reading::Absent);
        assert!(vm.tick(1).expect("tick").is_empty());

        // Air, which is NOT nil.
        *eye.block.lock().expect("block") = Some(crate::sight::Reading::Single {
            material: crate::MaterialId::AIR,
            occupancy: 0,
        });
        assert!(vm.tick(1).expect("tick").is_empty());

        // A partly-filled block of one material.
        *eye.block.lock().expect("block") = Some(crate::sight::Reading::Single {
            material: crate::MaterialId(7),
            occupancy: 0b101,
        });
        assert!(vm.tick(1).expect("tick").is_empty());

        // And a mixed one, which is the only case that carries cells.
        let mut cells = [crate::MaterialId::AIR; crate::block::SUBNODES_PER_BLOCK];
        cells[1] = crate::MaterialId(9);
        *eye.block.lock().expect("block") = Some(crate::sight::Reading::Mixed(Box::new(cells)));
        assert!(vm.tick(1).expect("tick").is_empty());

        let written = slate.written.lock().expect("slate");
        let names: Vec<&str> = written.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "test:unknown",
                "test:unknown",
                "test:one_0_0",
                "test:one_7_5",
                "test:mixed_27_9",
            ],
            "a block reached Lua as the wrong value"
        );

        // And it asked about the block it was told to, rather than the chunk
        // or the cell — an off-by-a-frame here reads a neighbour for ever.
        assert_eq!(
            eye.blocks_asked.lock().expect("asked").as_slice(),
            &[crate::BlockPos::new(1, 2, 3); 4]
        );
    }

    #[test]
    fn line_of_sight_reports_clear_blocked_and_unknown_separately() {
        // The three answers are the whole design: `nil` is not a third flavour
        // of "no". A mod that cannot tell "I lost sight of you" from "I was
        // asked while the engine had no world" has no way to debug a mob that
        // stopped following, which is the failure this API was shaped around.
        let mut vm = vm();
        let slate = std::sync::Arc::new(Slate::default());
        vm.set_world_edit(
            std::sync::Arc::clone(&slate) as std::sync::Arc<dyn crate::script::WorldEdit>
        );
        let eye = std::sync::Arc::new(Eye::default());

        load(&mut vm, "watcher", SIGHT_MOD).expect("load");
        let _ = vm.freeze();

        // No world installed at all — a test with no server behind the VM, and
        // the same answer worldgen gets.
        assert!(vm.tick(1).expect("tick").is_empty());

        vm.set_sight_access(std::sync::Arc::clone(&eye) as std::sync::Arc<dyn crate::sight::Access>);

        *eye.reply.lock().expect("reply") = Some(crate::sight::Sighting::Clear);
        assert!(vm.tick(1).expect("tick").is_empty());

        *eye.reply.lock().expect("reply") = Some(crate::sight::Sighting::Blocked);
        assert!(vm.tick(1).expect("tick").is_empty());

        // Installed, but the world is not lent right now.
        *eye.reply.lock().expect("reply") = Some(crate::sight::Sighting::Unavailable);
        assert!(vm.tick(1).expect("tick").is_empty());

        let written = slate.written.lock().expect("slate");
        let names: Vec<&str> = written.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(
            names,
            ["test:unknown", "test:clear", "test:blocked", "test:unknown"],
            "a sighting reached Lua as the wrong value"
        );
    }

    #[test]
    fn line_of_sight_passes_world_blocks_through_unrounded() {
        // Charter rule 7 and the entity API's own rule: mods speak world blocks
        // as plain floats, and a sight test that rounded its ends to whole
        // blocks would answer about a line nobody asked about — which at
        // sub-node resolution is a different line by up to three cells.
        let mut vm = vm();
        let slate = std::sync::Arc::new(Slate::default());
        vm.set_world_edit(
            std::sync::Arc::clone(&slate) as std::sync::Arc<dyn crate::script::WorldEdit>
        );
        let eye = std::sync::Arc::new(Eye::default());
        *eye.reply.lock().expect("reply") = Some(crate::sight::Sighting::Clear);
        vm.set_sight_access(std::sync::Arc::clone(&eye) as std::sync::Arc<dyn crate::sight::Access>);

        load(&mut vm, "watcher", SIGHT_MOD).expect("load");
        let _ = vm.freeze();
        assert!(vm.tick(1).expect("tick").is_empty());

        let asked = eye.asked.lock().expect("asked");
        assert_eq!(
            asked.as_slice(),
            &[([1.5, 2.5, 3.5], [4.5, 5.5, 6.5])],
            "the points the mod named are not the points the engine was asked about"
        );
    }

    /// A mod that turns whatever `game.find_path` returned into a block edit:
    /// the first step of the route, or the reason there was not one.
    const PATH_MOD: &str = "game.register_on_tick(function()\n\
           local route, why = game.find_path(\n\
             {x=0,y=0,z=0}, {x=5,y=0,z=0}, {max_drop = 1})\n\
           if route then\n\
             game.set_block({x=#route,y=0,z=0}, 'test:route')\n\
           else\n\
             game.set_block({x=0,y=0,z=0}, 'test:' .. why)\n\
           end\n\
         end)";

    #[test]
    fn find_path_reports_its_three_refusals_apart() {
        // A mob that treats a spent budget as "there is no way there" gives up
        // on somewhere it could reach; one that treats it as "not yet" retries
        // for ever. The engine knows which happened, and this is the boundary
        // where that knowledge is either passed on or lost.
        let mut vm = vm();
        let slate = std::sync::Arc::new(Slate::default());
        vm.set_world_edit(
            std::sync::Arc::clone(&slate) as std::sync::Arc<dyn crate::script::WorldEdit>
        );
        let map = std::sync::Arc::new(Map::default());

        load(&mut vm, "walker", PATH_MOD).expect("load");
        let _ = vm.freeze();

        // Nothing installed at all.
        assert!(vm.tick(1).expect("tick").is_empty());

        vm.set_path_access(std::sync::Arc::clone(&map) as std::sync::Arc<dyn crate::path::Access>);

        *map.reply.lock().expect("reply") = Some(crate::path::Route::Unreachable);
        assert!(vm.tick(1).expect("tick").is_empty());

        *map.reply.lock().expect("reply") = Some(crate::path::Route::Exhausted);
        assert!(vm.tick(1).expect("tick").is_empty());

        *map.reply.lock().expect("reply") = Some(crate::path::Route::Found(vec![
            crate::BlockPos::new(0, 0, 0),
            crate::BlockPos::new(1, 0, 0),
            crate::BlockPos::new(2, 0, 0),
        ]));
        assert!(vm.tick(1).expect("tick").is_empty());

        let written = slate.written.lock().expect("slate");
        let names: Vec<&str> = written.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "test:no world",
                "test:unreachable",
                "test:budget",
                "test:route"
            ],
            "a refusal reached Lua as the wrong reason"
        );
        // And the route arrived as a Lua array of the right length, which the
        // mod encoded into the position it wrote.
        assert_eq!(
            written.last().map(|(pos, _)| pos.x),
            Some(3),
            "the route did not reach Lua as a three-element array"
        );
    }

    #[test]
    fn find_path_options_reach_the_engine_and_the_rest_default() {
        // A mod that names one option must not silently lose the others: a mob
        // that asked for `max_drop = 1` and got the default 3 walks off a
        // ledge, and the mod looks wrong.
        let mut vm = vm();
        let map = std::sync::Arc::new(Map::default());
        *map.reply.lock().expect("reply") = Some(crate::path::Route::Unreachable);
        vm.set_path_access(std::sync::Arc::clone(&map) as std::sync::Arc<dyn crate::path::Access>);
        vm.set_world_edit(std::sync::Arc::new(Slate::default()));

        load(&mut vm, "walker", PATH_MOD).expect("load");
        let _ = vm.freeze();
        assert!(vm.tick(1).expect("tick").is_empty());

        let asked = map.asked.lock().expect("asked");
        assert_eq!(
            asked.as_slice(),
            &[crate::path::Options {
                max_drop: 1,
                ..crate::path::Options::default()
            }]
        );
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
    fn an_item_is_a_material_that_cannot_be_placed() {
        // **Everything a player can carry is one id in one table.** A stack is
        // a material, a quantity in units and a cut (charter rule 5), and that
        // is what the inventory, the wire, the icons and the world's own
        // string↔numeric table are all built on. A sword gets an id from the
        // same counter a stone does; the only thing that differs is that it may
        // not go in the world.
        let mut vm = vm();
        load(
            &mut vm,
            "core_gear",
            r#"
            game.register_block{ id = "stone", hardness = 1.0 }
            game.register_item{ id = "sword", name = "Sword", texture = "sword.png" }
            "#,
        )
        .expect("load");

        let blocks = vm.registered_blocks();
        let ids: std::collections::BTreeMap<&str, MaterialId> = blocks
            .iter()
            .map(|(name, id)| (name.as_str(), *id))
            .collect();
        assert!(
            ids.contains_key("core_gear:sword"),
            "an item is in the material table: {ids:?}"
        );
        assert_ne!(
            ids["core_gear:sword"], ids["core_gear:stone"],
            "an item and a block share a counter and must not share an id"
        );

        // And it is marked, which is the whole of what makes it an item.
        assert_eq!(vm.registered_items(), vec!["core_gear:sword".to_owned()]);

        // Its texture reaches the atlas by the same road a block's does, which
        // is what makes a slot able to draw it with no new code at all.
        assert!(
            vm.registered_block_textures()
                .iter()
                .any(|texture| texture.block == "core_gear:sword" && texture.path == "sword.png"),
            "an item's texture did not reach the atlas"
        );
    }

    #[test]
    fn an_item_is_not_a_block_and_the_error_says_so() {
        let mut refuser = vm();
        // The fields a block has and an item does not. Refused rather than
        // ignored: an API that accepted `hardness` on a sword would be
        // promising behaviour nothing implements.
        for field in ["hardness = 1.0", "light_emit = { r = 5 }", "drops = {}"] {
            let source = format!("game.register_item{{ id = \"sword\", {field} }}");
            let err =
                load(&mut refuser, "core_gear", &source).expect_err(&format!("{field} accepted"));
            let (ScriptError::Load { detail, .. } | ScriptError::Runtime { detail, .. }) = &err
            else {
                panic!("{err:?}");
            };
            assert!(
                detail.contains("unknown field"),
                "the error for `{field}` should name it: {detail}"
            );
        }

        // And a name already taken is refused whichever way round it is.
        let mut fresh = vm();
        assert!(
            load(
                &mut fresh,
                "core_gear",
                r#"game.register_block{ id = "thing" }
                   game.register_item{ id = "thing" }"#,
            )
            .is_err(),
            "an item took a block's id"
        );
    }

    #[test]
    fn a_mod_registers_a_place_for_stacks_to_sit() {
        // **Armour, tool belts and bandoliers are a mod's** — what a slot MEANS
        // is content (charter rule 1) — but where a stack may sit is the
        // engine's, because the engine is what moves them. Before this a mod
        // could name any view it liked and find nothing there: `player:main`
        // was the only one that existed and it was written in Rust.
        let mut vm = vm();
        load(
            &mut vm,
            "core_armour",
            r#"
            game.register_view{ id = "worn", slots = 4 }
            game.register_view{ id = "belt", slots = 9 }
            "#,
        )
        .expect("load");

        let views = vm.registered_views();
        assert_eq!(views.len(), 2);
        // Namespaced with the mod's own id, like every other id (rule 8), and
        // sorted, because this list decides the order of a wire message.
        assert_eq!(views[0].id, "core_armour:belt");
        assert_eq!(views[0].slots, 9);
        assert_eq!(views[1].id, "core_armour:worn");

        // And a player actually gets them, at the size that was asked for.
        let slots = crate::inventory::Slots::for_player_with(&views);
        let worn = slots
            .view("core_armour:worn")
            .expect("the registered view is on the player");
        assert_eq!(worn.slots.len(), 4);
        assert!(
            slots.view(crate::inventory::PLAYER_MAIN).is_some(),
            "registering a view must not cost a player the one they had"
        );
    }

    #[test]
    fn a_view_may_not_be_nothing_or_everything_or_the_players_own() {
        let mut vm = vm();
        for (spec, why) in [
            (
                r#"game.register_view{ id = "empty", slots = 0 }"#,
                "0 slots",
            ),
            (
                r#"game.register_view{ id = "huge", slots = 100000 }"#,
                "a view bigger than the cap",
            ),
            (
                r#"game.register_view{ id = "dup", slots = 1 }
                   game.register_view{ id = "dup", slots = 2 }"#,
                "the same view twice",
            ),
        ] {
            assert!(load(&mut vm, "greedy", spec).is_err(), "{why} was accepted");
        }

        // **And `player:main` cannot be redefined by naming it.** A mod's ids
        // are namespaced, so the string it would have to produce is not one
        // `qualify_id` will make — but the view builder refuses it anyway,
        // because the view digging credits into is not something a mod resizes
        // by picking a string.
        let mine = crate::inventory::Slots::for_player_with(&[crate::inventory::ViewDef {
            id: crate::inventory::PLAYER_MAIN.to_owned(),
            slots: 1,
        }]);
        assert_eq!(
            mine.view(crate::inventory::PLAYER_MAIN)
                .expect("still there")
                .slots
                .len(),
            crate::inventory::PLAYER_MAIN_SLOTS,
            "a mod resized the player's own inventory"
        );
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
        /// Where players have been sent, and how hard they have been pushed.
        ///
        /// A log rather than a body, because the authoritative body lives on
        /// the server and what is under test here is the trip through Lua.
        moved: std::sync::Mutex<Vec<([u8; 32], [f64; 3])>>,
        shoved: std::sync::Mutex<Vec<([u8; 32], [f32; 3])>>,
        /// Whether there is a player of that name to move at all.
        connected: std::sync::Mutex<bool>,
        /// Transfers asked for, since asking is all a mod call does.
        transfers: std::sync::Mutex<Vec<(crate::ent::EntityId, String)>>,
    }

    impl crate::ent::Access for Menagerie {
        fn move_player(&self, uuid: [u8; 32], to: [f64; 3]) -> bool {
            if let Ok(mut moved) = self.moved.lock() {
                moved.push((uuid, to));
            }
            self.connected.lock().is_ok_and(|there| *there)
        }

        fn shove_player(&self, uuid: [u8; 32], impulse: [f32; 3]) -> bool {
            if let Ok(mut shoved) = self.shoved.lock() {
                shoved.push((uuid, impulse));
            }
            self.connected.lock().is_ok_and(|there| *there)
        }

        fn transfer(&self, id: crate::ent::EntityId, domain: &str, _to: [f64; 3]) -> bool {
            self.transfers
                .lock()
                .expect("transfers")
                .push((id, domain.to_owned()));
            true
        }

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

        fn player(&self, uuid: [u8; 32]) -> Option<crate::ent::EntityId> {
            // The test double has no roster, so it does what the real one does
            // by another road: find the body whose owner matches.
            let entities = self.entities.lock().ok()?;
            entities.iter().find_map(|(id, entity)| {
                (entity.owner.map(|owner| *owner.0.as_bytes()) == Some(uuid)).then_some(id)
            })
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
    fn moving_and_pushing_a_player_reach_the_body_and_report_back() {
        // **The distinction this API exists to make.** Patching a player's
        // entity writes a mirror the tick overwrites; these write the body. A
        // mod cannot tell the difference by looking, which is why the surface
        // has to.
        let (mut vm, store) = vm_with_entities();
        let uuid = "ab".repeat(32);
        *store.connected.lock().expect("connected") = true;

        load(
            &mut vm,
            "mover",
            &format!(
                "game.register_on_tick(function()\n\
                   local went = game.move_player('{uuid}', {{x=10.5,y=64.0,z=-3.25}})\n\
                   local shoved = game.push_player('{uuid}', {{x=0.0,y=0.9,z=0.0}})\n\
                   if went and shoved then game.log('both') end\n\
                 end)"
            ),
        )
        .expect("load");
        let _ = vm.freeze();
        assert!(vm.tick(1).expect("tick").is_empty());

        let moved = store.moved.lock().expect("moved");
        assert_eq!(moved.len(), 1, "the move did not reach the engine");
        // Bit-exact, and deliberately: these numbers make the trip through
        // Lua unchanged or the API is lying about where it puts people. A
        // tolerance here would hide a conversion that nearly works.
        assert_eq!(
            moved[0].1.map(f64::to_bits),
            [10.5f64, 64.0, -3.25].map(f64::to_bits),
            "it arrived somewhere else: {:?}",
            moved[0].1
        );
        let shoved = store.shoved.lock().expect("shoved");
        assert_eq!(shoved.len(), 1, "the push did not reach the engine");
        assert!(
            (shoved[0].1[1] - 0.9).abs() < 1e-6,
            "the impulse arrived as {:?}",
            shoved[0].1
        );
        assert_eq!(
            moved[0].0, shoved[0].0,
            "the two calls named different players"
        );
    }

    #[test]
    fn moving_a_player_who_is_not_here_answers_false_rather_than_erroring() {
        // A player disconnects between a mod deciding to move them and the
        // call landing. That is ordinary, and a mod written around an error
        // there would be a mod written around the server's timing.
        let (mut vm, store) = vm_with_entities();
        *store.connected.lock().expect("connected") = false;
        let uuid = "cd".repeat(32);

        load(
            &mut vm,
            "mover",
            &format!(
                "game.register_on_tick(function()\n\
                   if game.move_player('{uuid}', {{x=0,y=0,z=0}}) then\n\
                     error('a player who is not here was moved')\n\
                   end\n\
                 end)"
            ),
        )
        .expect("load");
        let _ = vm.freeze();
        assert!(
            vm.tick(1).expect("tick").is_empty(),
            "the mod was disabled, so it saw something other than false"
        );
    }

    #[test]
    fn a_position_that_is_not_a_number_is_refused_rather_than_placed() {
        // Charter rule 4: NaN must not reach simulation state. A mod computing
        // a position from a division it did not check is how one gets there,
        // and a body at NaN is a body no correction can bring back.
        let (mut vm, store) = vm_with_entities();
        *store.connected.lock().expect("connected") = true;
        let uuid = "ef".repeat(32);

        load(
            &mut vm,
            "mover",
            &format!(
                "game.register_on_tick(function()\n\
                   game.move_player('{uuid}', {{x=0/0,y=0,z=0}})\n\
                 end)"
            ),
        )
        .expect("load");
        let _ = vm.freeze();
        assert_eq!(
            vm.tick(1).expect("tick").len(),
            1,
            "a position of NaN was accepted"
        );
        assert!(
            store.moved.lock().expect("moved").is_empty(),
            "a position of NaN reached the engine"
        );
    }

    #[test]
    fn an_entity_can_be_a_stack_lying_on_the_ground() {
        // **What a dropped item is.** The engine has no opinion about dropping
        // — what an item is worth, how long it lasts and who may pick it up are
        // a mod's (charter rule 1) — but a mod cannot draw anything, so the
        // mechanism is that an entity says what stack it IS and the client
        // draws that. The round trip is the test: a mod puts a cut on the
        // ground and reads back the same cut, in the same shape `game.inventory`
        // hands it over in.
        const CARVED: u32 = 0b1010_0101;
        let (mut vm, store) = vm_with_entities();
        load(
            &mut vm,
            "dropper",
            &format!(
                "turn = 0\n\
                 seen = nil\n\
                 game.register_on_tick(function()\n\
                   turn = turn + 1\n\
                   if turn == 1 then\n\
                     id = game.spawn_entity{{ pos = {{x=1,y=2,z=3}},\n\
                       item = {{ material = 7, shape = {CARVED}, count = 2 }} }}\n\
                   else\n\
                     seen = game.entity(id)\n\
                   end\n\
                 end)"
            ),
        )
        .expect("load");
        let _ = vm.freeze();

        assert!(vm.tick(1).expect("tick").is_empty());
        {
            let entities = store.entities.lock().expect("lock");
            let (_, entity) = entities.iter().next().expect("one entity");
            let item = entity.item.as_ref().expect("the entity is an item");
            assert_eq!(item.material.0, 7);
            assert_eq!(
                item.shape.map(crate::inventory::Shape::occupancy),
                Some(CARVED),
                "the cut did not survive the spawn"
            );
            assert_eq!(
                item.units,
                2 * CARVED.count_ones(),
                "two of a cut is two lots of its cells"
            );
        }

        assert!(vm.tick(1).expect("tick").is_empty());
        vm.eval_in(
            "dropper",
            &format!(
                "assert(seen.item ~= nil, 'the item did not come back')\n\
                 assert(seen.item.shape == {CARVED}, 'shape came back as ' .. \
                 tostring(seen.item.shape))\n\
                 assert(seen.item.count == 2, 'count came back as ' .. tostring(seen.item.count))"
            ),
        )
        .expect("the mod read its own item back");
    }

    #[test]
    fn a_mod_turns_a_direction_into_a_heading_and_back_again() {
        // **The round trip that has to close.** A mod reads `facing` to point
        // at something and `game.heading` to work out which way it is pointing,
        // and both come from the same committed table — so a mob that looks at
        // you and then walks that way does not drift a little every time.
        //
        // The reason this is engine surface at all is charter rule 4: Lua's
        // `math.atan` is the platform's libm, and a yaw is persisted entity
        // state. A reference mod called it for a day.
        let (mut vm, store) = vm_with_entities();
        load(
            &mut vm,
            "surveyor",
            "id = nil\n\
             game.register_on_tick(function()\n\
               if id == nil then\n\
                 id = game.spawn_entity{ pos = {x=0,y=0,z=0} }\n\
               end\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();
        assert!(vm.tick(1).expect("tick").is_empty());
        assert_eq!(store.entities.lock().expect("lock").len(), 1);

        vm.eval_in(
            "surveyor",
            // Eight directions, checked against what the engine's own facing
            // would be: set the yaw, read the facing back, and the heading of
            // that facing must be the yaw again.
            "for step = 0, 7 do\n\
               local yaw = step * (math.pi / 4) - math.pi\n\
               game.set_entity(id, { yaw = yaw })\n\
               local me = game.entity(id)\n\
               local back = game.heading(me.facing.x, me.facing.z)\n\
               local apart = math.abs(back - yaw)\n\
               apart = math.min(apart, math.abs(apart - 2 * math.pi))\n\
               assert(apart < 0.01, 'yaw ' .. yaw .. ' came back as ' .. back)\n\
             end\n\
             assert(game.heading(0, 0) == 0, 'a direction of no length has no heading')\n\
             assert(math.abs(game.heading(1, 0) - math.pi / 2) < 0.01, 'east')\n\
             assert(math.abs(game.heading(0, 1)) < 0.01, 'north is zero')",
        )
        .expect("the round trip through facing and heading");
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
    fn a_runaway_entity_step_is_cut_off_rather_than_hanging_the_tick() {
        // Charter rule 10: a mod error disables that mod, never kills the tick.
        // An infinite loop inside a per-entity callback is the shape of that
        // error most likely to reach a real server, because a mob's behaviour
        // is a state machine and a state machine is where a `while` goes.
        let (mut vm, _store) = vm_with_entities();
        load(
            &mut vm,
            "runaway",
            "game.register_on_entity_step(function() while true do end end)",
        )
        .expect("load");
        let _ = vm.freeze();

        let fault = vm
            .entity_step("runaway", &[1], 1)
            .expect("the VM itself should survive")
            .expect("the runaway should have been stopped");
        assert_eq!(fault.0, "runaway");
        assert!(
            vm.faulted_mods().contains(&"runaway".to_owned()),
            "the mod was not disabled"
        );
    }

    #[test]
    fn every_entity_gets_its_own_instruction_budget() {
        // **The property the load test cannot show and this one can.** A budget
        // shared across the tick would let the first few mobs spend it and
        // starve the rest, and the symptom on a server would be "my mod works
        // with ten mobs and breaks at a hundred" — with nothing in the log to
        // say why.
        //
        // Each callback burns a large, BOUNDED number of instructions: enough
        // that a shared budget would be gone within a handful of entities, and
        // comfortably inside one call's allowance. Two hundred of them, which
        // is the task's own number.
        let (mut vm, _store) = vm_with_entities();
        load(
            &mut vm,
            "busy",
            "seen = 0\n\
             game.register_on_entity_step(function()\n\
               local total = 0\n\
               for i = 1, 20000 do total = total + i end\n\
               seen = seen + 1\n\
             end)",
        )
        .expect("load");
        let _ = vm.freeze();

        let ids: Vec<u64> = (1..=200).collect();
        let fault = vm
            .entity_step("busy", &ids, 1)
            .expect("the VM should survive");
        assert!(
            fault.is_none(),
            "an entity ran out of budget, so the budget is shared: {fault:?}"
        );
        vm.eval_in(
            "busy",
            "assert(seen == 200, 'only ' .. seen .. ' of 200 entities ran')",
        )
        .expect("every entity should have been stepped");
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
