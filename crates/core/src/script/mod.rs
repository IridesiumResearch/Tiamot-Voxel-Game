// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The scripting runtime, behind one trait.
//!
//! # Why a trait at all, when there is one backend
//!
//! [`ScriptVm`] is the only path into a script VM. Nothing outside this module
//! names a backend type, and CI greps to keep it that way.
//!
//! That is not architecture for its own sake. The backend is chosen by
//! measurement (see `docs/scripting-vm.md`) and the choice is **irreversible in
//! practice** — mod-visible language semantics differ between the candidates, so
//! switching later breaks every mod ever written. The trait cannot undo that.
//! What it does buy is:
//!
//! - the *benchmark* that makes the decision, which needs all three behind one
//!   API to compare them at all;
//! - a future WASM tier, which would be an addition rather than a rewrite;
//! - a boundary where budgets, memory limits, and crash isolation are enforced
//!   once instead of at every call site.
//!
//! # The rule scripts cannot break by accident
//!
//! **Script code must not do simulation-relevant float maths.** Charter rule 4's
//! Deterministic Float Subset cannot be enforced inside a script VM — Lua has
//! one number type and no lint reaches into it, and `x^0.5` in a mod is a libm
//! call on whatever platform the server happens to run.
//!
//! So worldgen scripts **orchestrate** native fills; they never compute
//! per-sample values. The API is shaped to make that the only ergonomic path: a
//! generator asks for a heightmap and hands it to a fill, and there is no
//! exposed way to loop over samples in Lua. Making the wrong thing awkward is
//! more reliable than documenting that it is wrong.

mod host;
mod vm;

pub use host::{HostError, ModHost, Phase, read_manifest};
pub use vm::{
    ActionEvent, Backend, BlockRules, BlockTexture, Brush, ChatEvent, DialogEvent, DigEvent,
    FluidFlowEvent, FluidRules, HookOutcome, JoinEvent, LeaveEvent, MAX_REFUSAL_BYTES, PlaceEvent,
    PunchEvent, ScriptError, ScriptVm, Tool, VmLimits, WorldEdit,
};

#[cfg(feature = "script")]
mod budget;

#[cfg(feature = "script")]
mod hud_vm;

#[cfg(feature = "script")]
mod mlua_vm;

#[cfg(feature = "script")]
pub use hud_vm::{Fault, HudLimits, HudVm};

#[cfg(feature = "script")]
pub use mlua_vm::MluaVm;

/// The VM the engine uses.
///
/// One alias so callers name a concrete type without naming a backend.
#[cfg(feature = "script")]
pub type EngineVm = MluaVm;

/// The engine's mod host, over the chosen VM.
#[cfg(feature = "script")]
pub type EngineHost = ModHost<MluaVm>;
