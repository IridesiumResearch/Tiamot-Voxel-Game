// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The instruction budget, and the two mechanisms behind it.
//!
//! Shared by both VMs in this crate — the mod host in [`super::mlua_vm`] and
//! the client-side HUD runtime in [`super::hud_vm`]. They budget different
//! things (a mod gets a budget per callback, a HUD script gets one per frame)
//! but the machinery is identical, and a second copy of a backend-conditional
//! hook is a second place for a backend to be got wrong.

use mlua::Lua;

/// Marker text used to recognise a budget stop coming back through `mlua`'s
/// error type, which has no dedicated variant for it.
pub const MARKER: &str = "tiamot: instruction budget exceeded";

/// How often the budget hook fires. Checking every instruction would dominate
/// the runtime; a few thousand keeps the overhead negligible while still
/// bounding a runaway loop to a fraction of a millisecond.
const HOOK_STEP: u32 = 4_096;

/// Installs an instruction budget on a VM.
///
/// The mechanism differs by backend, which is exactly the kind of thing the
/// [`super::ScriptVm`] trait exists to hide from callers:
///
/// - **Lua 5.4 and `LuaJIT`** have a debug hook, called every *n* instructions.
/// - **Luau** has none, and an interrupt called at back-edges and calls
///   instead. Coarser than an instruction count, but bounded — which is what a
///   budget is for.
///
/// # Errors
///
/// Whatever `mlua` says when the hook cannot be installed. Luau's interrupt is
/// infallible; the signature stays uniform so callers do not branch on backend.
#[allow(
    clippy::unnecessary_wraps,
    reason = "Luau's interrupt API is infallible while the 5.4/LuaJIT hook is not; \
              the signature stays uniform so callers do not branch on backend"
)]
pub fn arm(lua: &Lua, instructions: u32) -> Result<(), mlua::Error> {
    #[cfg(any(feature = "vm-lua54", feature = "vm-luajit"))]
    {
        let limit = u64::from(instructions);
        let counter = std::cell::Cell::new(0u64);
        lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(HOOK_STEP),
            move |_lua, _debug| {
                let used = counter.get() + u64::from(HOOK_STEP);
                counter.set(used);
                if used > limit {
                    return Err(mlua::Error::external(MARKER));
                }
                Ok(mlua::VmState::Continue)
            },
        )?;
    }

    #[cfg(feature = "vm-luau")]
    {
        let limit = u64::from(instructions) / u64::from(HOOK_STEP);
        let counter = std::cell::Cell::new(0u64);
        lua.set_interrupt(move |_lua| {
            let used = counter.get() + 1;
            counter.set(used);
            if used > limit {
                return Err(mlua::Error::external(MARKER));
            }
            Ok(mlua::VmState::Continue)
        });
    }

    Ok(())
}

/// Removes the budget, so engine-side work is not counted against a script.
pub fn disarm(lua: &Lua) {
    #[cfg(any(feature = "vm-lua54", feature = "vm-luajit"))]
    lua.remove_hook();

    #[cfg(feature = "vm-luau")]
    lua.remove_interrupt();
}
